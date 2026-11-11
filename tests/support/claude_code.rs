use std::{os::unix::fs::DirBuilderExt, time::Duration};

// CLI는 구현 위임이 아니라 고정 비도구 client 접속 시험이다. 개인 설정/keychain과 외부 network는 OS가 거부한다.
pub struct CliResult {
    pub helpers: usize,
    pub exit: i32,
    pub permission: bool,
    pub status400: bool,
    pub config_open_denied: bool,
}
pub async fn read_only_initialization(base: &str, token: &str) -> CliResult {
    let url = url::Url::parse(base).unwrap();
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    let fnm = std::env::var_os("TEST_FNM_DIR").expect("전용 Node22 필요");
    let home = std::env::temp_dir().join(format!("m-gated-s9-cli-{}", uuid::Uuid::new_v4()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&home)
        .unwrap();
    let home = home.canonicalize().unwrap();
    let counter = home.join("helper-count");
    std::fs::write(&counter, b"").unwrap();
    std::fs::create_dir(home.join("config")).unwrap();
    let helper = home.join("synthetic-helper.sh");
    std::fs::write(
        &helper,
        "#!/bin/sh\nprintf x >> \"$HOME/helper-count\"\nprintf '%s' \"$MGATED_GATEWAY_TOKEN\"\n",
    )
    .unwrap();
    let settings = serde_json::json!({"apiKeyHelper":format!("/bin/sh {}",helper.display()),"autoUpdatesChannel":"stable","alwaysThinkingEnabled":false});
    let sandbox = format!(
        r#"(version 1)(allow default)(deny network*)(allow network-outbound (remote tcp "localhost:{}"))(deny file-write* (require-all (require-not (literal "{}")) (require-not (literal "/dev/null"))))(deny file-read* file-write* (subpath "/Users/snark/.claude") (literal "/Users/snark/.claude.json") (subpath "/Users/snark/.config") (subpath "/Users/snark/Library/Keychains") (subpath "/Users/snark/Library/Application Support/Claude"))(deny process-exec (literal "/usr/bin/security"))"#,
        url.port().unwrap(),
        counter.display()
    );
    let guard=tokio::process::Command::new("/usr/bin/sandbox-exec").env_clear().env("HOME",&home)
        .args(["-p",&sandbox,"/bin/sh","-c",r#"if printf x > "$HOME/forbidden" 2>/dev/null; then exit 1; fi; printf x >> "$HOME/helper-count""#])
        .output().await.unwrap();
    assert!(
        guard.status.success(),
        "무해 sandbox 사전probe 실패; CLI 실행하지 않음"
    );
    assert!(!home.join("forbidden").exists());
    assert_eq!(std::fs::read(&counter).unwrap(), b"x");
    std::fs::write(&counter, b"").unwrap();
    let node = std::path::Path::new(&fnm).join("node-versions/v22.22.2/installation/bin/node");
    assert!(
        node.is_file(),
        "부모가 고정 fnm exec로 검증한 정확 Node22 binary 필요"
    );
    let control=tokio::process::Command::new("/usr/bin/sandbox-exec").env_clear()
        .args(["-p", &sandbox]).arg(&node).args(["-e", "if(process.versions.node!=='22.22.2')process.exit(1);process.stdout.write('NODE_CONTROL_OK');"])
        .output().await.unwrap();
    assert!(control.status.success());
    assert_eq!(control.stdout, b"NODE_CONTROL_OK");
    assert!(control.stderr.is_empty());
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/compat/node_modules/@anthropic-ai/claude-code");
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.join("package.json")).unwrap()).unwrap();
    assert_eq!(manifest["version"], "2.1.63");
    let cli = directory.join("cli.js");
    let mut cmd = tokio::process::Command::new("/usr/bin/sandbox-exec");
    cmd.env_clear()
        .current_dir(&home)
        .env("PATH", "/opt/homebrew/bin:/usr/bin:/bin")
        .env("HOME", &home)
        .env("CLAUDE_CONFIG_DIR", home.join("config"))
        .env("FNM_DIR", fnm)
        .env("MGATED_GATEWAY_TOKEN", token)
        .env("ANTHROPIC_BASE_URL", format!("{base}/anthropic"))
        .env("DO_NOT_TRACK", "1")
        .env("DISABLE_TELEMETRY", "1")
        .env("DISABLE_ERROR_REPORTING", "1")
        .env("DISABLE_AUTOUPDATER", "1")
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", "1")
        .env("CLAUDE_CODE_SKIP_PROMPT_HISTORY", "1")
        .env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1")
        .env("CLAUDE_CODE_DISABLE_FILE_CHECKPOINTING", "1")
        .env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "100")
        .env("CLAUDE_CODE_MAX_RETRIES", "0")
        .env("API_TIMEOUT_MS", "10000")
        .env("OTEL_LOG_USER_PROMPTS", "0")
        .args(["-p", &sandbox])
        .arg(node)
        .arg(cli)
        .args([
            "--print",
            "--output-format",
            "json",
            "--model",
            "synthetic-text",
            "--tools",
            "",
            "--system-prompt",
            "synthetic system fixture",
            "--setting-sources",
            "",
            "--settings",
            &settings.to_string(),
            "--strict-mcp-config",
            "--mcp-config",
            r#"{"mcpServers":{}}"#,
            "--disable-slash-commands",
            "--no-session-persistence",
            "synthetic-cli-prompt-sentinel",
        ])
        .kill_on_drop(true);
    let result = tokio::time::timeout(Duration::from_secs(25), cmd.output()).await;
    let helpers = std::fs::read(home.join("helper-count"))
        .unwrap_or_default()
        .len();
    fn inspect(path: &std::path::Path, token: &str) -> bool {
        std::fs::read_dir(path).unwrap().all(|entry| {
            let p = entry.unwrap().path();
            if p.is_dir() {
                inspect(&p, token)
            } else {
                let bytes = std::fs::read(p).unwrap();
                ![
                    token.as_bytes(),
                    b"synthetic-cli-prompt-sentinel",
                    b"synthetic system fixture",
                ]
                .iter()
                .any(|needle| bytes.windows(needle.len()).any(|v| v == *needle))
            }
        })
    }
    let clean = inspect(&home, token);
    std::fs::remove_dir_all(home).unwrap();
    assert!(
        clean,
        "CLI가 인증/대화 sentinel을 파일에 남겼음 (원문 삭제)"
    );
    let output = result.expect("CLI 유한마감").expect("OS sandbox CLI 실행");
    // stdout/stderr는 메모리에서 고정 분류만 확인하고 원문/본문을 출력하거나 저장하지 않는다.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let failure = stderr
        .lines()
        .find(|line| line.contains("EPERM") || line.contains("EACCES"))
        .unwrap_or("");
    let target = failure.split('\'').nth(1).unwrap_or("");
    let target_kind = if target.starts_with("/dev/fd/")
        || ["/dev/stdout", "/dev/stderr", "/dev/stdin"].contains(&target)
    {
        "STDIO_PIPE"
    } else if target == "/dev/tty" {
        "TTY"
    } else if target.contains("debug") {
        "CLI_DEBUG"
    } else if target.contains("config") || target.ends_with(".json") {
        "CLI_CONFIG"
    } else if target.contains("session") || target.contains("projects") {
        "SESSION"
    } else if target.contains("cache") {
        "CACHE"
    } else if target.starts_with("/tmp/") || target.starts_with("/private/tmp/") {
        "TEMP"
    } else {
        "OTHER"
    };
    let syscall = if failure.contains("open") {
        "open"
    } else if failure.contains("mkdir") {
        "mkdir"
    } else if failure.contains("write") {
        "write"
    } else {
        "other"
    };
    eprintln!(
        "CLI_FAILURE syscall={syscall} target={target_kind} path_present={}",
        !target.is_empty()
    );
    CliResult {
        helpers,
        exit: output.status.code().unwrap_or(-1),
        permission: stderr.contains("EPERM") || stderr.contains("EACCES"),
        status400: stdout.contains("400") || stderr.contains("400"),
        config_open_denied: syscall == "open" && target_kind == "CLI_CONFIG",
    }
}
