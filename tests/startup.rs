mod support;
use m_gated::{
    bootstrap::{Dependencies, build},
    config::{self, ConfigInputs, StartupError},
    runtime::Lifecycle,
};
use std::{path::PathBuf, time::Duration};
use tokio::{process::Command, time::timeout};

struct Fixture {
    directory: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("m-gated-startup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        Self { directory }
    }
    fn config(&self, yaml: &str) -> PathBuf {
        let path = self.directory.join("synthetic-nonsecret.yaml");
        std::fs::write(&path, yaml).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.directory).unwrap();
    }
}
async fn run(
    fixture: &Fixture,
    yaml: Option<&str>,
    env: &std::collections::BTreeMap<String, String>,
) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_m-gated"));
    command
        .env_clear()
        .envs(env)
        .current_dir(&fixture.directory)
        .kill_on_drop(true);
    if let Some(yaml) = yaml {
        command.arg("--config").arg(fixture.config(yaml));
    }
    let output = timeout(Duration::from_secs(3), command.output())
        .await
        .expect("기동 실패는 HTTP 대기 없이 종료해야 한다")
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let error = String::from_utf8(output.stderr).unwrap();
    for secret in env.values() {
        assert!(!error.contains(secret));
    }
    assert!(!error.contains("secret-sentinel"));
    assert!(!error.contains(&fixture.directory.to_string_lossy().to_string()));
    error
}

#[tokio::test]
async fn absent_config_exits_before_bind_without_inheriting_environment() {
    assert_eq!(
        run(&Fixture::new(), None, &std::collections::BTreeMap::new()).await,
        "config_input_invalid: 기동 검증에 실패했습니다.\n"
    );
}

#[tokio::test]
async fn actual_binary_rejects_each_config_failure_and_missing_bundle_safely() {
    let fixture = Fixture::new();
    for (yaml, expected) in [
        (
            support::YAML.replace("api: messages", "api: responses"),
            "config_profile_invalid",
        ),
        (
            support::YAML.replace(
                "issuer: https://auth.example.test",
                "issuer: https://auth.other.test",
            ),
            "config_auth_invalid",
        ),
        (
            support::YAML.replace(
                "https://api.anthropic.com",
                "https://secret-sentinel@api.anthropic.com",
            ),
            "config_url_invalid",
        ),
        (
            format!("{}limits:\n  inference: 0\n", support::YAML),
            "config_limits_invalid",
        ),
        (
            format!("{}retention:\n  ledger: null\n", support::YAML),
            "config_syntax_invalid",
        ),
        (support::YAML.to_owned(), "ui_bundle_unavailable"),
    ] {
        assert_eq!(
            run(&fixture, Some(&yaml), &support::environment()).await,
            format!("{expected}: 기동 검증에 실패했습니다.\n")
        );
    }
    let mut env = support::environment();
    env.remove("CREDENTIAL_ENCRYPTION_KEY");
    assert_eq!(
        run(&fixture, Some(support::YAML), &env).await,
        "config_secret_missing: 기동 검증에 실패했습니다.\n"
    );
    let mut removed = support::environment();
    removed.insert(
        "ANTHROPIC_API_KEY".into(),
        "synthetic-provider-secret".into(),
    );
    assert_eq!(
        run(&fixture, Some(support::YAML), &removed).await,
        "config_provider_key_env_removed: 기동 검증에 실패했습니다.\n"
    );
    env = support::environment();
    env.insert("CSRF_SIGNING_KEY".into(), "short-secret-sentinel".into());
    assert_eq!(
        run(&fixture, Some(support::YAML), &env).await,
        "config_secret_invalid: 기동 검증에 실패했습니다.\n"
    );
}

#[tokio::test]
async fn same_assembly_refuses_missing_components_before_binding_an_occupied_socket() {
    let config = config::load(ConfigInputs {
        yaml: support::YAML,
        file_environment: None,
        environment: &support::environment(),
    })
    .unwrap();
    let app = build(config, Dependencies::unassembled()).unwrap();
    let runtime = app.runtime().clone();
    assert_eq!(runtime.lifecycle(), Lifecycle::Starting);
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    assert_eq!(
        app.run(occupied.local_addr().unwrap(), std::future::pending())
            .await,
        Err(StartupError::UiUnavailable)
    );
    assert_eq!(runtime.lifecycle(), Lifecycle::Stopped);
}

#[tokio::test]
async fn maintenance_rejects_invalid_bounded_stdin_before_database_or_bind() {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    let fixture = Fixture::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_m-gated"))
        .args(["--maintenance", "--config"])
        .arg(fixture.config(support::YAML))
        .env_clear()
        .envs(support::environment())
        .current_dir(&fixture.directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"access_token\":\"synthetic-input-sentinel\",\"command_id\":\"bad\"}")
        .await
        .unwrap();
    let output = timeout(Duration::from_secs(3), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "maintenance_failed: 유지보수 검증에 실패했습니다.\n"
    );
}

#[tokio::test]
async fn maintenance_stdin_limits_are_finite_and_do_not_connect_to_database() {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    let fixture = Fixture::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let yaml = support::YAML.replace(
        "port: 64603",
        &format!("port: {}", listener.local_addr().unwrap().port()),
    );
    for payload in [
        Some(vec![b'x'; 65537]),
        Some(br#"{"access_token":"never","access_\u0074oken":"duplicate"}"#.to_vec()),
        None,
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_m-gated"))
            .args(["--maintenance", "--config"])
            .arg(fixture.config(&yaml))
            .env_clear()
            .envs(support::environment())
            .current_dir(&fixture.directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut input = Some(child.stdin.take().unwrap());
        let stalled = payload.is_none();
        if let Some(bytes) = payload {
            let _ = input.as_mut().unwrap().write_all(&bytes).await;
            drop(input.take());
        }
        let begin = tokio::time::Instant::now();
        let output = timeout(Duration::from_secs(7), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        drop(input);
        if stalled {
            assert!(begin.elapsed() >= Duration::from_secs(5));
        }
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "maintenance_failed: 유지보수 검증에 실패했습니다.\n"
        );
    }
    assert!(
        timeout(Duration::from_millis(10), listener.accept())
            .await
            .is_err(),
        "구문/수신 실패는 DB 연결을 시작하지 않는다"
    );
}
