use std::{os::unix::fs::DirBuilderExt, time::Duration};

pub async fn error_sdk(
    base: &str,
    token: &str,
    api: &str,
    status: u16,
    code: &str,
    dispatched: bool,
) {
    error_sdk_options(base, token, api, status, code, dispatched, false).await;
}
pub async fn error_stream_sdk(
    base: &str,
    token: &str,
    api: &str,
    status: u16,
    code: &str,
    dispatched: bool,
) {
    error_sdk_options(base, token, api, status, code, dispatched, true).await;
}
async fn error_sdk_options(
    base: &str,
    token: &str,
    api: &str,
    status: u16,
    code: &str,
    dispatched: bool,
    stream: bool,
) {
    let fnm = std::env::var_os("TEST_FNM_DIR").expect("전용 고정 Node22 설치 필요");
    let home = std::env::temp_dir().join(format!("m-gated-s9-sdk-{}", uuid::Uuid::new_v4()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&home)
        .unwrap();
    let mut command = tokio::process::Command::new("/opt/homebrew/bin/fnm");
    command
        .env_clear()
        .env("PATH", "/opt/homebrew/bin:/usr/bin:/bin")
        .env("HOME", &home)
        .env("FNM_DIR", fnm)
        .env("DO_NOT_TRACK", "1")
        .env("ANTHROPIC_TELEMETRY", "0")
        .env("MGATED_GATEWAY_BASE", base)
        .env("MGATED_GATEWAY_TOKEN", token)
        .env("MGATED_ERROR_STREAM", if stream { "1" } else { "0" })
        .env("MGATED_ERROR_API", api)
        .env("MGATED_EXPECTED_STATUS", status.to_string())
        .env("MGATED_EXPECT_ERROR_CODE", code)
        .env("MGATED_EXPECT_DISPATCH", if dispatched { "D" } else { "N" })
        .args([
            "exec",
            "--using",
            "22.22.2",
            "node",
            "tests/compat/errors.mjs",
        ])
        .kill_on_drop(true);
    let result = tokio::time::timeout(Duration::from_secs(140), command.output()).await;
    std::fs::remove_dir_all(home).unwrap();
    let output = result.expect("SDK 유한마감").expect("고정 Node 실행");
    assert!(output.status.success(), "SDK 오류 행렬 실패 (원문 비저장)");
    assert!(
        output.stderr.is_empty(),
        "SDK stderr 비어야 함 (원문 비저장)"
    );
    assert_eq!(
        output.stdout,
        format!(
            "SDK_ERROR_MATRIX_OK api={api} status={status} attempts=1 refresh=0 maxRetries=0\n"
        )
        .as_bytes()
    );
}

pub async fn assert_error_ledger(pool: &sqlx::PgPool, dispatched: bool) {
    let state:(i64,i64,i64,bool,bool)=sqlx::query_as("SELECT count(*)::bigint,count(*) FILTER(WHERE dispatch_authorized_at IS NOT NULL)::bigint,count(*) FILTER(WHERE accounting_state='unknown')::bigint,COALESCE(bool_and(cost_usd IS NULL),true),COALESCE((SELECT bool_and(remaining_reservation_usd=0 AND remaining_reserved_tokens=0) FROM gw_request_budgets),true) FROM gw_requests").fetch_one(pool).await.unwrap();
    assert_eq!(
        state,
        (
            i64::from(dispatched),
            i64::from(dispatched),
            i64::from(dispatched),
            true,
            true
        )
    );
    let finals: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_usage_events WHERE kind='final'")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(finals, 0);
}
