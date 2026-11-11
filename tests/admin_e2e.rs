#[path = "support/browser_result.rs"]
mod browser_result;
#[path = "support/finance.rs"]
#[allow(dead_code)]
mod finance;
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
    bootstrap::{self, Dependencies, RunningApp},
    config::{self, ConfigInputs},
    ledger::{
        PgLedger,
        finance::{FinalObservation, FinalOutcome, FinalUsage, RequestSpec},
    },
    runtime::Dependency,
};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use uuid::Uuid;
fn config(demoted: bool) -> config::ValidatedConfig {
    let mut env = support::environment();
    env.insert(
        "ALLOWED_EMAILS".into(),
        "alice@example.test,bob@example.test,carol@example.test".into(),
    );
    env.insert(
        "ADMIN_EMAILS".into(),
        if demoted {
            "bob@example.test"
        } else {
            "alice@example.test,bob@example.test"
        }
        .into(),
    );
    config::load(ConfigInputs {
        yaml: support::YAML,
        file_environment: None,
        environment: &env,
    })
    .unwrap()
}
async fn app(
    db: &postgres::TestDb,
    uri: &url::Url,
    upstream: &url::Url,
    demoted: bool,
) -> Arc<RunningApp> {
    let cfg = config(demoted);
    let auth = Arc::new(SsoAuthenticator::new(
        &cfg,
        SsoVerifier::new(
            cfg.auth(),
            Arc::new(JwksCache::loopback_for_test(uri.clone()).unwrap()),
        ),
        db.store.clone(),
    ));
    let api = Arc::new(m_gated::admin::DashboardApi::new(
        &cfg,
        auth.clone(),
        db.store.clone(),
    ));
    let ledger = Arc::new(PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&cfg),
        m_gated::gateway::auth_config_digest(&cfg),
    ));
    let client = m_gated::providers::anthropic::AnthropicClient::loopback_for_test(
        upstream.clone(),
        "synthetic-provider-secret".into(),
    )
    .unwrap();
    let gateway = Arc::new(m_gated::gateway::InferenceGateway::assemble(
        &cfg.providers()[0],
        auth.clone(),
        ledger,
        client,
    ));
    let bundle = Arc::new(
        m_gated::dashboard::UiBundle::load(
            std::path::Path::new("web/dist"),
            "https://auth.example.test",
        )
        .unwrap(),
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
    assert!(auth.refresh_health(app.runtime()).await);
    let (database, schema) = db.store.health().await;
    for (dependency, value) in [
        (Dependency::Database, database),
        (Dependency::Schema, schema),
        (Dependency::Ui, true),
    ] {
        app.runtime()
            .record_health(dependency, value, tokio::time::Instant::now());
    }
    app.runtime().start_http().unwrap();
    app
}
async fn serve(
    app: Arc<RunningApp>,
) -> (
    String,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        let health_app = app.clone();
        let monitor = tokio::spawn(app.runtime().clone().monitor(move || {
            let app = health_app.clone();
            async move { app.refresh_health().await }
        }));
        m_gated::server::serve(listener, app.router(), app.runtime().clone(), async {
            let _ = stopped.await;
        })
        .await
        .unwrap();
        monitor.await.unwrap().unwrap();
    });
    (url, stop, handle)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL, pinned Node22 and Playwright"]
async fn administrator_browser_commands_queries_and_current_authority() {
    let db = postgres::TestDb::new().await;
    let cfg = config(false);
    let fake = sso::FakeJwks::new(vec![sso::Signer::shared().jwk.clone()]);
    let jwks = fake.serve().await;
    let verifier = SsoVerifier::new(
        cfg.auth(),
        Arc::new(JwksCache::loopback_for_test(jwks.uri.clone()).unwrap()),
    );
    let mut tokens = Vec::new();
    let mut humans = Vec::new();
    for (name, email) in [
        ("alice", "alice@example.test"),
        ("bob", "bob@example.test"),
        ("carol", "carol@example.test"),
    ] {
        let mut claims = sso::claims();
        claims["sub"] = json!(name);
        claims["email"] = json!(email);
        claims["name"] = json!(name);
        let token = sso::Signer::shared().sign(&claims, "key1");
        let identity = verifier
            .verify(&sso::credential(token.clone()), 1900000000)
            .await
            .unwrap();
        humans.push(
            db.store
                .current_human(&identity, &AccessSettings::from_config(&cfg))
                .await
                .unwrap(),
        );
        tokens.push(token);
    }
    let service = Uuid::new_v4();
    let policy = Uuid::new_v4();
    sqlx::query("INSERT INTO gw_principals(id,kind,display_name,owner_principal_id) VALUES($1,'service','CAS 서비스',$2)").bind(service).bind(humans[0].id).execute(db.store.pool()).await.unwrap();
    sqlx::query("INSERT INTO gw_policies(id,principal_id,allow_rules,budget_mode,budget_limit_usd,period_kind,period_timezone,updated_by) VALUES($1,$2,$3,'actual',100,'calendar_month','UTC',$4)").bind(policy).bind(service).bind(json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}])).bind(humans[0].id).execute(db.store.pool()).await.unwrap();
    let current = finance::current(db.store.pool(), service).await;
    let ledger = PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&cfg),
        m_gated::gateway::auth_config_digest(&cfg),
    );
    let mut requests = Vec::new();
    for _ in 0..6 {
        let spec = RequestSpec::from_profile(
            config::ProviderId::Anthropic,
            &cfg.providers()[0].profiles[0],
            Some(16),
        );
        ledger.admit_request(&current, &spec).await.unwrap();
        let _permit = ledger.authorize_dispatch(spec.id).await.unwrap();
        requests.push(spec.id);
    }
    ledger
        .finalize_request(
            requests[0],
            &FinalObservation::completed(FinalUsage {
                actual_model: "synthetic-text".into(),
                input_tokens: 100,
                output_tokens: 10,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: None,
            }),
        )
        .await
        .unwrap();
    for id in &requests[1..] {
        ledger
            .finalize_request(
                *id,
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
    }
    ledger.record_execution_stopped(requests[1]).await.unwrap();
    sqlx::query("UPDATE gw_principals SET version=7,inference_safety_version=29,inference_paused_at=clock_timestamp(),inference_pause_reason='cost_bound_violation',inference_pause_request_id=$2 WHERE id=$1").bind(service).bind(requests[1]).execute(db.store.pool()).await.unwrap();
    sqlx::query("UPDATE gw_policies SET version=13 WHERE id=$1")
        .bind(policy)
        .execute(db.store.pool())
        .await
        .unwrap();
    let provider = provider::Provider::new();
    let upstream = provider.serve().await;
    let app = app(&db, &jwks.uri, &upstream.base, false).await;
    let peer = self::app(&db, &jwks.uri, &upstream.base, false).await;
    let demoted = self::app(&db, &jwks.uri, &upstream.base, true).await;
    // 실제 runtime request registry가 알려진 sender 실행 중임을 관리 handler에 전달한다.
    let (active, registration) = app
        .runtime()
        .register_request(requests[4], tokio::time::Instant::now());
    active.set_phase(m_gated::runtime::request::RequestPhase::Relaying);
    let (base, stop, server) = serve(app).await;
    let (peer_base, stop_peer, peer_server) = serve(peer).await;
    let (demoted_base, stop_demoted, demoted_server) = serve(demoted).await;
    let stale_store = db.store.clone();
    let retention_database = db.clock_store(None).await;
    let retention_store = retention_database.clone();
    let retention_actor = humans[0].id;
    let retention_request = requests[0];
    let late_ledger = Arc::new(ledger);
    let late_request = requests[5];
    let control = axum::Router::new()
        .route("/new-safety-cause", axum::routing::post(move || { let ledger=late_ledger.clone(); async move { ledger.finalize_request(late_request, &FinalObservation::completed(FinalUsage {actual_model:"synthetic-text".into(),input_tokens:5000,output_tokens:0,cache_read_tokens:0,cache_write_tokens:0,reasoning_tokens:None})).await.unwrap(); axum::http::StatusCode::NO_CONTENT } }))
        .route("/stale-policy", axum::routing::post(move || { let store=stale_store.clone(); async move { sqlx::query("UPDATE gw_policies SET version=version+1 WHERE id=$1").bind(policy).execute(store.pool()).await.unwrap(); axum::http::StatusCode::NO_CONTENT } }))
        .route("/retention", axum::routing::post(move || { let store=retention_store.clone(); async move {
            use m_gated::{config::{DurationPreset,Retention},ledger::{commands::CommandContext,retention::RetentionDelete}};
            sqlx::query("UPDATE gw_fixture_clock SET observed=(SELECT GREATEST(r.finished_at,(SELECT max(e.observed_at) FROM gw_usage_events e WHERE e.request_id=r.id))+interval '3 months' FROM gw_requests r WHERE r.id=$1)").bind(retention_request).execute(store.pool()).await.unwrap();
            let cfg=config(false);let settings=AccessSettings::from_config(&cfg);let grants=json!([]);
            let ledger=PgLedger::new(store.clone(),settings.clone(),m_gated::gateway::auth_config_digest(&cfg));
            let ctx=CommandContext{actor:retention_actor,command_id:Uuid::new_v4(),method:"MAINTENANCE",path:"retention",settings:&settings,grants:&grants,digest:[1;32],ceiling:cfg.execution_ceiling(),execution_running:false};
            let result=ledger.retention_delete(&ctx,&Retention{ledger:Some(DurationPreset::ThreeMonths),audit:None,commands:None},&RetentionDelete{after:None,limit:50,deletion_approved:true,approval_ref:"synthetic-browser-local-D7".into(),reason:"실제 삭제 후 UI 캐시 제거".into()}).await.unwrap();
            let deleted=result["result"]["deleted_requests"].as_u64().unwrap();
            assert_eq!(deleted,1,"실제 선택 가족 삭제 건수");
            axum::http::StatusCode::NO_CONTENT
        } }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let control_url = format!("http://{}", listener.local_addr().unwrap());
    let control_task = tokio::spawn(async move {
        axum::serve(listener, control).await.unwrap();
    });
    let home = std::env::temp_dir().join(format!("mgated-u2-e2e-{}", Uuid::new_v4()));
    std::fs::create_dir(&home).unwrap();
    let mut command =
        tokio::process::Command::new(std::env::var("TEST_FNM_BINARY").expect("explicit fnm"));
    command
        .env_clear()
        .env("PATH", "/opt/homebrew/bin:/usr/bin:/bin")
        .env("HOME", &home)
        .env(
            "FNM_DIR",
            std::env::var("TEST_FNM_DIR").expect("explicit Node22 cache"),
        )
        .env(
            "PLAYWRIGHT_BROWSERS_PATH",
            std::env::var("TEST_PLAYWRIGHT_BROWSERS_PATH").expect("explicit browsers"),
        )
        .env("DO_NOT_TRACK", "1")
        .env("E2E_BASE_URL", base)
        .env("E2E_DEMOTED_URL", demoted_base)
        .env("E2E_PEER_URL", peer_base)
        .env("E2E_CONTROL_URL", control_url)
        .env("E2E_TOKEN", &tokens[0])
        .env("E2E_BOB_TOKEN", &tokens[1])
        .env("E2E_USER_TOKEN", &tokens[2])
        .env("E2E_ADMIN_ID", humans[0].id.to_string())
        .env("E2E_BOB_ID", humans[1].id.to_string())
        .env("E2E_USER_ID", humans[2].id.to_string())
        .env("E2E_SERVICE_ID", service.to_string())
        .env("E2E_RUNTIME_REQUEST", requests[1].to_string())
        .env("E2E_CONFIRM_REQUEST", requests[2].to_string())
        .env("E2E_RACE_REQUEST", requests[3].to_string())
        .env("E2E_ACTIVE_REQUEST", requests[4].to_string())
        .args([
            "exec",
            "--using",
            "22.22.2",
            "node",
            "web/node_modules/@playwright/test/cli.js",
            "test",
            "--config=web/playwright.config.ts",
            "admin.spec.ts",
        ])
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(180), command.output()).await;
    drop(registration);
    drop(active);
    let _ = stop.send(());
    let _ = stop_demoted.send(());
    let _ = stop_peer.send(());
    server.await.unwrap();
    demoted_server.await.unwrap();
    peer_server.await.unwrap();
    control_task.abort();
    std::fs::remove_dir_all(home).unwrap();
    let manual: (i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM gw_usage_events WHERE kind='settlement'),(SELECT count(*) FROM gw_commands WHERE operation='settle_unknown'),(SELECT count(*) FROM gw_audit_events WHERE operation='settle_unknown')").fetch_one(db.store.pool()).await.unwrap();
    let unknown: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gw_requests WHERE accounting_state='unknown'")
            .fetch_one(db.store.pool())
            .await
            .unwrap();
    let token_null: Option<bool>=sqlx::query_scalar("SELECT bool_and(normalized_usage->>'input_tokens' IS NULL AND normalized_usage->>'source'='manual_entry') FROM gw_usage_events WHERE kind='settlement'").fetch_one(db.store.pool()).await.unwrap();
    let accounting_consistent: bool=sqlx::query_scalar("SELECT bool_and(t.cost_usd = t.carried_cost_usd + (SELECT coalesce(sum(b.applied_cost_usd),0) FROM gw_request_budgets b WHERE b.scope_id=t.scope_id AND b.period_start=t.period_start) AND t.unknown_count = (SELECT count(*) FROM gw_request_budgets b WHERE b.scope_id=t.scope_id AND b.period_start=t.period_start AND b.state='unknown')) FROM gw_usage_totals t WHERE scope_id=$1").bind(policy).fetch_one(db.store.pool()).await.unwrap();
    let attribution_consistent: bool=sqlx::query_scalar("SELECT bool_and(b.period_start <= r.admitted_at AND r.admitted_at < b.period_end AND b.period_start=t.period_start) FROM gw_request_budgets b JOIN gw_requests r ON r.id=b.request_id JOIN gw_usage_totals t ON t.scope_id=b.scope_id AND t.period_start=b.period_start WHERE b.scope_id=$1").bind(policy).fetch_one(db.store.pool()).await.unwrap();
    let admins: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gw_principals WHERE id=ANY($1) AND disabled_at IS NULL",
    )
    .bind(vec![humans[0].id, humans[1].id])
    .fetch_one(db.store.pool())
    .await
    .unwrap();
    retention_database.pool().close().await;
    db.close().await;
    let output = output.expect("browser 180초").unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        tokens
            .iter()
            .all(|t| !stdout.contains(t) && !stderr.contains(t))
    );
    browser_result::forward_sanitized(&stdout);
    browser_result::assert_cases(&stdout, &(16..=25).collect::<Vec<_>>());
    assert!(
        output.status.success(),
        "Playwright exit={:?}",
        output.status.code()
    );
    assert_eq!(provider.calls.lock().await.len(), 0);
    assert_eq!(manual, (3, 3, 3));
    assert_eq!(unknown, 2);
    assert_eq!(token_null, Some(true));
    assert!(accounting_consistent && attribution_consistent);
    assert_eq!(admins, 1);
}
