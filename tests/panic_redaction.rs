use std::process::Stdio;
use tokio::{io::AsyncReadExt, process::Command};

#[tokio::test]
async fn panic_hook_child() {
    let Ok(mode) = std::env::var("SYNTHETIC_PANIC_CHILD") else {
        return;
    };
    assert!(["cold", "full"].contains(&mode.as_str()));
    m_gated::runtime::panic::install_redacting_hook();
    if mode == "full" {
        use std::io::Write;
        let bytes = [b'X'; 4096];
        let mut full = false;
        for _ in 0..1024 {
            match std::io::stderr().write(&bytes) {
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    full = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert!(full, "비대기 sink를 실제로 포화시켜야 한다");
    }
    let caught =
        std::panic::catch_unwind(|| panic!("synthetic-JWT-secret-conversation-tool-sentinel"));
    assert!(caught.is_err());
    let (count, dropped) = m_gated::runtime::panic::counts();
    println!("PANIC_COUNTS count={count} dropped={dropped}");
    assert_eq!((count, dropped), (1, u64::from(mode == "full")));
}

#[tokio::test]
async fn panic_payload_is_never_reported_by_cold_or_saturated_process_hook() {
    for mode in ["cold", "full"] {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .env_clear()
            .env("SYNTHETIC_PANIC_CHILD", mode)
            .args(["--exact", "panic_hook_child", "--nocapture"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        // 의도적으로 stderr를 읽지 않아 포화를 유지한다. child는 nonblocking 단일 write만 사용한다.
        let status = tokio::time::timeout(std::time::Duration::from_secs(3), child.wait())
            .await
            .unwrap()
            .unwrap();
        let mut out = String::new();
        stdout.read_to_string(&mut out).await.unwrap();
        let mut err = Vec::new();
        stderr.read_to_end(&mut err).await.unwrap();
        assert_eq!(status.code(), Some(0));
        assert!(err.len() <= 4 * 1024 * 1024);
        let err = String::from_utf8(err).unwrap();
        for output in [&out, &err] {
            assert!(!output.contains("synthetic-JWT-secret-conversation-tool-sentinel"));
        }
        assert!(out.contains(&format!(
            "PANIC_COUNTS count=1 dropped={}",
            u8::from(mode == "full")
        )));
        if mode == "cold" {
            assert_eq!(err, "{\"alarm\":\"runtime_panic\"}\n");
        } else {
            assert!(err.bytes().all(|b| b == b'X'));
        }
    }
}
