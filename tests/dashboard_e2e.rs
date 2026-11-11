#[path = "support/browser_result.rs"]
mod browser_result;
#[path = "support/postgres.rs"]
mod postgres;
#[path = "support/provider.rs"]
#[allow(dead_code)]
mod provider;
#[path = "support/sso.rs"]
#[allow(dead_code)]
mod sso;
mod support;
use m_gated::{
    auth::{AccessSettings, SsoAuthenticator, SsoVerifier, jwks::JwksCache},
    bootstrap::{self, Dependencies},
    config::{self, ConfigInputs},
    ledger::{
        PgLedger,
        finance::{FinalObservation, FinalOutcome, FinalUsage, RequestSpec},
    },
    runtime::Dependency,
};
use serde_json::json;
use std::{sync::Arc, time::Duration};
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL, Node22 and pinned Playwright Chromium"]
async fn rust_bundle_postgres_and_signed_sso_browser_contract() {
    let db = postgres::TestDb::new().await;
    let mut env = support::environment();
    env.insert("ALLOWED_EMAILS".into(), "alice@example.test".into());
    let yaml = support::YAML.replace("'1.000000000000'", "'0.000001000000'");
    let cfg = config::load(ConfigInputs {
        yaml: &yaml,
        file_environment: None,
        environment: &env,
    })
    .unwrap();
    let fake = sso::FakeJwks::new(vec![sso::Signer::shared().jwk.clone()]);
    let jwks = fake.serve().await;
    let verifier = SsoVerifier::new(
        cfg.auth(),
        Arc::new(JwksCache::loopback_for_test(jwks.uri.clone()).unwrap()),
    );
    let mut claims = sso::claims();
    claims["name"] = json!("<img src=x onerror=window.__xss=1>");
    let token = sso::Signer::shared().sign(&claims, "key1");
    let mut second_claims = claims.clone();
    second_claims["sub"] = json!("human-2");
    second_claims["name"] = json!("Synthetic Second");
    let second_token = sso::Signer::shared().sign(&second_claims, "key1");
    let identity = verifier
        .verify(&sso::credential(token.clone()), 1900000000)
        .await
        .unwrap();
    let human = db
        .store
        .current_human(&identity, &AccessSettings::from_config(&cfg))
        .await
        .unwrap();
    sqlx::query(
        "UPDATE gw_policies SET allow_rules=$2,budget_limit_usd=gw_amount(100) WHERE id=$1",
    )
    .bind(human.policy_id)
    .bind(json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}]))
    .execute(db.store.pool())
    .await
    .unwrap();
    let ledger = Arc::new(PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&cfg),
        m_gated::gateway::auth_config_digest(&cfg),
    ));
    let spec = RequestSpec::from_profile(
        config::ProviderId::Anthropic,
        &cfg.providers()[0].profiles[0],
        Some(16),
    );
    ledger.admit_request(&human, &spec).await.unwrap();
    let _send = ledger.authorize_dispatch(spec.id).await.unwrap();
    ledger
        .finalize_request(
            spec.id,
            &FinalObservation::completed(FinalUsage {
                actual_model: "synthetic-text".into(),
                input_tokens: 1,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: None,
            }),
        )
        .await
        .unwrap();
    let unknown = RequestSpec::from_profile(
        config::ProviderId::Anthropic,
        &cfg.providers()[0].profiles[0],
        Some(16),
    );
    ledger.admit_request(&human, &unknown).await.unwrap();
    let _send = ledger.authorize_dispatch(unknown.id).await.unwrap();
    ledger
        .finalize_request(
            unknown.id,
            &FinalObservation {
                outcome: FinalOutcome::Completed,
                usage: None,
                counted_input_tokens: None,
                reason: "usage_unresolved",
                http_status: Some(200),
                provider_request_id: None,
            },
        )
        .await
        .unwrap();
    let auth = Arc::new(SsoAuthenticator::new(&cfg, verifier, db.store.clone()));
    let provider = provider::Provider::new();
    let upstream = provider.serve().await;
    let client = m_gated::providers::anthropic::AnthropicClient::loopback_for_test(
        upstream.base.clone(),
        "synthetic-provider-secret".into(),
    )
    .unwrap();
    let gateway = Arc::new(m_gated::gateway::InferenceGateway::assemble(
        &cfg.providers()[0],
        auth.clone(),
        ledger,
        client,
    ));
    let api = Arc::new(m_gated::admin::DashboardApi::new(
        &cfg,
        auth.clone(),
        db.store.clone(),
    ));
    let bundle = Arc::new(
        m_gated::dashboard::UiBundle::load(
            std::path::Path::new("web/dist"),
            "https://auth.example.test",
        )
        .expect("실제 Vite build가 필수다"),
    );
    let app = Arc::new(
        bootstrap::build(
            cfg,
            Dependencies::unassembled()
                .with_authentication(auth.clone())
                .with_inference(gateway)
                .with_dashboard(api)
                .with_ui(bundle),
        )
        .unwrap(),
    );
    let (database, schema) = db.store.health().await;
    assert!(database && schema);
    assert!(auth.refresh_health(app.runtime()).await);
    let runtime = app.runtime().clone();
    runtime.record_health(Dependency::Database, database, tokio::time::Instant::now());
    runtime.record_health(Dependency::Schema, schema, tokio::time::Instant::now());
    runtime.record_health(Dependency::Ui, true, tokio::time::Instant::now());
    runtime.start_http().unwrap();
    let health_app = app.clone();
    let monitor = tokio::spawn(runtime.clone().monitor(move || {
        let app = health_app.clone();
        async move { app.refresh_health().await }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(m_gated::server::serve(
        listener,
        app.router(),
        runtime.clone(),
        async {
            let _ = stopped.await;
        },
    ));
    // test-only sidecar는 제품 route와 분리되어 자기 fixture의 정지/복구만 제어한다.
    let store = db.store.clone();
    let enable_store = store.clone();
    let id = human.id;
    let mut expired_claims = claims.clone();
    expired_claims["exp"] = json!(time::OffsetDateTime::now_utc().unix_timestamp() - 31);
    let expired_token = sso::Signer::shared().sign(&expired_claims, "key1");
    let mut refreshed_claims = claims.clone();
    refreshed_claims["exp"] = json!(time::OffsetDateTime::now_utc().unix_timestamp() + 600);
    let refreshed_token = sso::Signer::shared().sign(&refreshed_claims, "key1");
    let refresh_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls = refresh_calls.clone();
    let refresh_cookie = format!("mgated_access={refreshed_token}; Path=/; HttpOnly; SameSite=Lax");
    let control=axum::Router::new()
        .route("/disable",axum::routing::post(move||{let store=store.clone();async move{sqlx::query("UPDATE gw_principals SET disabled_at=clock_timestamp(),disabled_reason='e2e_stop' WHERE id=$1").bind(id).execute(store.pool()).await.unwrap();axum::http::StatusCode::NO_CONTENT}}))
        .route("/enable",axum::routing::post(move||{let store=enable_store.clone();async move{sqlx::query("UPDATE gw_principals SET disabled_at=NULL,disabled_reason=NULL WHERE id=$1").bind(id).execute(store.pool()).await.unwrap();axum::http::StatusCode::NO_CONTENT}}))
        .route("/refresh",axum::routing::post(move||{let cookie=refresh_cookie.clone();let calls=calls.clone();async move{calls.fetch_add(1,std::sync::atomic::Ordering::SeqCst);(axum::http::StatusCode::NO_CONTENT,[("set-cookie",cookie)])}}));
    let control_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let control_address = control_listener.local_addr().unwrap();
    let control_task = tokio::spawn(async move {
        axum::serve(control_listener, control).await.unwrap();
    });
    let home = std::env::temp_dir().join(format!("mgated-u0-e2e-home-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&home).unwrap();
    let fnm = std::env::var("TEST_FNM_BINARY").expect("명시 TEST_FNM_BINARY가 필요하다");
    let fnm_dir = std::env::var("TEST_FNM_DIR").expect("명시 TEST_FNM_DIR가 필요하다");
    let browsers =
        std::env::var("TEST_PLAYWRIGHT_BROWSERS_PATH").expect("명시 browser cache가 필요하다");
    let scope = std::env::var("TEST_E2E_SCOPE").unwrap_or_default();
    let keys_only = scope == "keys" || scope == "keys-one";
    let mut command = tokio::process::Command::new(fnm);
    command
        .env_clear()
        .env("PATH", "/opt/homebrew/bin:/usr/bin:/bin")
        .env("HOME", &home)
        .env("FNM_DIR", fnm_dir)
        .env("PLAYWRIGHT_BROWSERS_PATH", browsers)
        .env("DO_NOT_TRACK", "1")
        .env("E2E_BASE_URL", format!("http://{address}"))
        .env("E2E_CONTROL_URL", format!("http://{control_address}"))
        .env("E2E_TOKEN", &token)
        .env("E2E_SECOND_TOKEN", &second_token)
        .env("E2E_EXPIRED_TOKEN", &expired_token)
        .env("E2E_REFRESHED_TOKEN", &refreshed_token)
        .env("E2E_COOKIE", "mgated_access")
        .args([
            "exec",
            "--using",
            "22.22.2",
            "node",
            "web/node_modules/@playwright/test/cli.js",
            "test",
            "--config=web/playwright.config.ts",
        ])
        .kill_on_drop(true);
    if keys_only {
        command.arg("keys.spec.ts");
    } else {
        command.args(["dashboard.spec.ts", "keys.spec.ts"]);
    }
    if scope == "keys-one" {
        command.args(["--grep", "personal_keys_five_explicit"]);
    }
    let output = tokio::time::timeout(Duration::from_secs(120), command.output()).await;
    let _ = stop.send(());
    server.await.unwrap().unwrap();
    monitor.await.unwrap().unwrap();
    control_task.abort();
    std::fs::remove_dir_all(home).unwrap();
    let observed_refreshes = refresh_calls.load(std::sync::atomic::Ordering::SeqCst);
    let observed_dispatches = provider.calls.lock().await.len();
    // U1 browser가 만든 키/명령/감사를 실제 SQL로 확인한다. 원문은 어떤 행에도 없다.
    let issued: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_virtual_keys")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    let commands: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gw_commands WHERE operation='issue_key'")
            .fetch_one(db.store.pool())
            .await
            .unwrap();
    let audits: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gw_audit_events WHERE operation='issue_key'")
            .fetch_one(db.store.pool())
            .await
            .unwrap();
    let validity: i64 =
        sqlx::query_scalar("SELECT count(DISTINCT validity_preset) FROM gw_virtual_keys")
            .fetch_one(db.store.pool())
            .await
            .unwrap();
    let raw_stored: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM gw_commands WHERE result_projection::text LIKE '%mgw_v1_%') OR EXISTS(SELECT 1 FROM gw_audit_events WHERE before_snapshot::text LIKE '%mgw_v1_%' OR after_snapshot::text LIKE '%mgw_v1_%')").fetch_one(db.store.pool()).await.unwrap();
    let lost_delete: Option<(bool, i64, i64, i64)> = sqlx::query_as(
        "SELECT revoked_at IS NOT NULL,version,
         (SELECT count(*) FROM gw_commands c WHERE c.operation='revoke_key' AND c.target_id=k.id),
         (SELECT count(*) FROM gw_audit_events a WHERE a.operation='revoke_key' AND a.target_id=k.id)
         FROM gw_virtual_keys k WHERE name='lost-delete-response'",
    )
    .fetch_optional(db.store.pool())
    .await
    .unwrap();
    let explicit_replacement: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gw_virtual_keys WHERE name='explicit-after-lost-delete' AND revoked_at IS NULL",
    )
    .fetch_one(db.store.pool())
    .await
    .unwrap();
    db.close().await;
    let output = output.expect("E2E 전체120초").unwrap();
    // JWT를 포함할 가능성이 있는 raw browser 오류를 저장/출력하지 않는다.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected: Vec<usize> = if scope == "keys-one" {
        vec![8]
    } else if keys_only {
        (8..=15).chain([27]).collect()
    } else {
        (1..=15).chain([26, 27]).collect()
    };
    browser_result::assert_cases(&stdout, &expected);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        [&token, &second_token, &expired_token, &refreshed_token]
            .iter()
            .all(|t| !stdout.contains(t.as_str()) && !stderr.contains(t.as_str()))
    );
    browser_result::forward_sanitized(&stdout);
    assert!(
        output.status.success(),
        "Playwright exit={:?}",
        output.status.code()
    );
    assert_eq!(
        observed_refreshes,
        if keys_only { 0 } else { 2 },
        "정상 복구와 새 me403 사례에서 실제 합성 SSO refresh는 각각 한 번"
    );
    assert_eq!(
        observed_dispatches, 0,
        "cookie 추론은 합성 Provider에도 미송신"
    );
    assert!(issued >= 5, "다섯 기간의 실제 발급 행");
    assert_eq!((commands, audits), (issued, issued));
    assert_eq!(validity, 5);
    assert!(!raw_stored);
    if scope != "keys-one" {
        assert_eq!(
            lost_delete,
            Some((true, 2, 1, 1)),
            "유실 DELETE도 실제 회수/버전/명령/감사는 한 번"
        );
        assert_eq!(
            explicit_replacement, 1,
            "사용자 새 확인으로만 대체 키 한 개"
        );
    }
}
