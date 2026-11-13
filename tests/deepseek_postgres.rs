#![allow(dead_code)]
#[path = "support/postgres.rs"]
mod postgres;
#[path = "support/provider.rs"]
mod provider;
#[path = "support/sso.rs"]
mod sso;
mod support;
use axum::{
    body::Bytes,
    http::{HeaderMap, StatusCode},
};
use m_gated::{
    auth::AccessSettings,
    config::{self, ConfigInputs},
    ledger::PgLedger,
};
use sqlx::Row;
use std::sync::Arc;
fn configuration() -> config::ValidatedConfig {
    configuration_options(false, "fixed_admission_v1", "")
}
fn configuration_options(
    verified: bool,
    time_rule: &str,
    options: &str,
) -> config::ValidatedConfig {
    configuration_features(
        verified,
        time_rule,
        options,
        "client_tools, thinking, cache, json_schema",
    )
}
fn configuration_features(
    verified: bool,
    time_rule: &str,
    options: &str,
    features: &str,
) -> config::ValidatedConfig {
    let mut yaml=include_str!("support/config.yaml").replace("provider: anthropic","provider: deepseek").replace("https://api.anthropic.com","https://api.deepseek.com").replace("api: messages",&format!("api: deepseek_chat\n        billing_time_rule: {time_rule}\n        features: [{features}]")).replace("          input: '1.000000000000'","          input: '1.000000000000'\n          cache_read: '0.500000000000'");
    if verified {
        yaml=yaml.replace("        max_input_tokens: 4096","        input_bound_contract:\n          kind: deepseek_chat_context_v1\n          context_tokens: 4096\n          input_categories: [messages]\n        max_input_tokens: 4096");
    }
    yaml = yaml.replace(
        "          - synthetic-text",
        "          - synthetic-text\n          - synthetic-deepseek-version",
    );
    yaml.push_str(options);
    let mut env = support::environment();
    env.insert("ALLOWED_EMAILS".into(), "alice@example.test".into());
    env.insert("ADMIN_EMAILS".into(), "alice@example.test".into());
    config::load(ConfigInputs {
        yaml: &yaml,
        file_environment: None,
        environment: &env,
    })
    .unwrap()
}
struct Harness {
    db: postgres::TestDb,
    database: Arc<m_gated::ledger::PgIdentityStore>,
    provider: Arc<provider::Provider>,
    _provider_server: provider::Server,
    _jwks: sso::Server,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    monitor: tokio::task::JoinHandle<()>,
    base: String,
    token: String,
    principal: uuid::Uuid,
    runtime: Arc<m_gated::runtime::Runtime>,
}

impl Harness {
    async fn new() -> Self {
        Self::with_config(configuration()).await
    }
    async fn with_config(cfg: config::ValidatedConfig) -> Self {
        Self::with_reset(cfg, false).await
    }
    async fn with_reset(cfg: config::ValidatedConfig, reset: bool) -> Self {
        Self::with_database_clock(cfg, reset, false).await
    }
    async fn with_clock(cfg: config::ValidatedConfig) -> Self {
        Self::with_database_clock(cfg, false, true).await
    }
    async fn with_database_clock(
        cfg: config::ValidatedConfig,
        reset: bool,
        controlled_clock: bool,
    ) -> Self {
        use m_gated::{
            auth::{SsoAuthenticator, SsoVerifier, jwks::JwksCache},
            bootstrap::{self, Dependencies},
            gateway::InferenceGateway,
            providers::anthropic::AnthropicClient,
            runtime::Dependency,
        };
        let db = postgres::TestDb::new().await;
        let database = if controlled_clock {
            db.clock_store(None).await
        } else {
            db.store.clone()
        };
        let fake = sso::FakeJwks::new(vec![sso::Signer::shared().jwk.clone()]);
        let jwks = fake.serve().await;
        let verifier = SsoVerifier::new(
            cfg.auth(),
            Arc::new(JwksCache::loopback_for_test(jwks.uri.clone()).unwrap()),
        );
        let token = sso::Signer::shared().sign(&sso::claims(), "key1");
        let identity = verifier
            .verify(&sso::credential(token.clone()), 1900000000)
            .await
            .unwrap();
        let current = database
            .current_human(&identity, &AccessSettings::from_config(&cfg))
            .await
            .unwrap();
        let principal = current.id;
        sqlx::query("UPDATE gw_policies SET allow_rules=$2 WHERE id=$1").bind(current.policy_id).bind(serde_json::json!([{"provider":"deepseek","model":"synthetic-text","capabilities":["chat_completions"]}])).execute(database.pool()).await.unwrap();
        let auth = Arc::new(SsoAuthenticator::new(&cfg, verifier, database.clone()));
        let ledger = Arc::new(
            PgLedger::new(
                database.clone(),
                AccessSettings::from_config(&cfg),
                m_gated::gateway::auth_config_digest(&cfg),
            )
            .with_ceiling(cfg.execution_ceiling().clone()),
        );
        let provider = provider::Provider::new();
        let provider_server = if reset {
            provider.serve_reset().await
        } else {
            provider.serve().await
        };
        let client = AnthropicClient::deepseek_loopback_for_test(
            provider_server.base.clone(),
            "synthetic-deepseek-company-key".into(),
        )
        .unwrap();
        let gateway = Arc::new(InferenceGateway::assemble(
            &cfg.providers()[0],
            auth.clone(),
            ledger,
            client,
        ));
        let dashboard = Arc::new(m_gated::admin::DashboardApi::new(
            &cfg,
            auth.clone(),
            database.clone(),
        ));
        let app = bootstrap::build(
            cfg,
            Dependencies::unassembled()
                .with_ui(Arc::new(
                    m_gated::dashboard::UiBundle::load(
                        std::path::Path::new("web/dist"),
                        "https://auth.example.test",
                    )
                    .expect("검증된 실제 Vite bundle 필요; scripts/test-s8.sh 선행 build"),
                ))
                .with_authentication(auth)
                .with_dashboard(dashboard)
                .with_inference(gateway),
        )
        .unwrap();
        // 실제 migration/검증bundle/JWKS의 관측으로 시작하며 제품과 같은 bounded monitor를 유지한다.
        let app = Arc::new(app);
        app.validate_bootstrap().unwrap();
        let runtime = app.runtime().clone();
        let report = app.refresh_health().await;
        assert!(report.database && report.schema && report.jwks && report.ui);
        for (d, healthy) in [
            (Dependency::Database, report.database),
            (Dependency::Schema, report.schema),
            (Dependency::Jwks, report.jwks),
            (Dependency::Ui, report.ui),
        ] {
            runtime.record_health(d, healthy, tokio::time::Instant::now());
        }
        runtime.start_http().unwrap();
        let monitor_app = app.clone();
        let monitor_runtime = runtime.clone();
        let monitor = tokio::spawn(async move {
            monitor_runtime
                .monitor(|| monitor_app.refresh_health())
                .await
                .unwrap();
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(m_gated::server::serve(
            listener,
            app.router(),
            runtime.clone(),
            std::future::pending(),
        ));
        Self {
            db,
            database,
            provider,
            _provider_server: provider_server,
            _jwks: jwks,
            task,
            monitor,
            base: format!("http://{address}"),
            token,
            principal,
            runtime,
        }
    }
    fn request(&self, path: &str, body: impl Into<reqwest::Body>) -> reqwest::RequestBuilder {
        self.request_with_token(path, body, &self.token)
    }
    fn request_with_token(
        &self,
        path: &str,
        body: impl Into<reqwest::Body>,
        token: &str,
    ) -> reqwest::RequestBuilder {
        reqwest::Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
            .post(format!("{}{path}", self.base))
            .bearer_auth(token)
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .body(body)
    }
    async fn close(self) {
        self.monitor.abort();
        if let Err(error) = self.monitor.await {
            assert!(error.is_cancelled(), "health monitor panic");
        }
        self.task.abort();
        let _ = self.task.await;
        self.database.pool().close().await;
        self.db.close().await;
    }
}
async fn set(h: &Harness, native: &str, stream: bool) {
    h.provider
        .set(
            StatusCode::OK,
            HeaderMap::from_iter([(
                "content-type".parse().unwrap(),
                if stream {
                    "text/event-stream"
                } else {
                    "application/json"
                }
                .parse()
                .unwrap(),
            )]),
            Bytes::copy_from_slice(native.as_bytes()),
        )
        .await;
}
async fn state(h: &Harness) -> (i64, i64, String, i64, i64) {
    h.runtime.wait_requests().await;
    sqlx::query_as("SELECT count(*)::bigint,count(*) FILTER(WHERE accounting_state='unknown')::bigint,COALESCE(sum(cost_usd),0)::text,COALESCE((SELECT sum(input_tokens)::bigint FROM gw_usage_totals),0),COALESCE((SELECT sum(output_tokens)::bigint FROM gw_usage_totals),0) FROM gw_requests").fetch_one(h.db.store.pool()).await.unwrap()
}
async fn sdk(h: &Harness, kind: &str, status: u16, native_status: &str, token: &str) {
    use std::os::unix::fs::DirBuilderExt;
    let fnm_dir = std::env::var_os("TEST_FNM_DIR").expect(
        "explicit TEST_FNM_DIR required for installed Node22.22.2; no personal CLI config fallback",
    );
    assert!(
        std::path::Path::new(&fnm_dir)
            .join("node-versions/v22.22.2/installation/bin/node")
            .is_file(),
        "fixed Node22.22.2 installation required"
    );
    let home = std::path::PathBuf::from(format!("/tmp/m-gated-s8-sdk-{}", uuid::Uuid::new_v4()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&home)
        .unwrap();
    let user = home.join("empty-user-npmrc");
    let global = home.join("empty-global-npmrc");
    std::fs::write(&user, "").unwrap();
    std::fs::write(&global, "").unwrap();
    let mut command = tokio::process::Command::new("/opt/homebrew/bin/fnm");
    command
        .env_clear()
        .env("PATH", "/opt/homebrew/bin:/usr/bin:/bin")
        .env("HOME", &home)
        .env("FNM_DIR", fnm_dir)
        .env("NPM_CONFIG_USERCONFIG", &user)
        .env("NPM_CONFIG_GLOBALCONFIG", &global)
        .env("NPM_CONFIG_UPDATE_NOTIFIER", "false")
        .env("DO_NOT_TRACK", "1")
        .env("ANTHROPIC_TELEMETRY", "0")
        .env("MGATED_GATEWAY_BASE", format!("{}/deepseek/v1", h.base))
        .env("MGATED_GATEWAY_TOKEN", token)
        .env("MGATED_SDK_KIND", kind)
        .env("MGATED_EXPECTED_STATUS", status.to_string())
        .env("MGATED_NATIVE_STATUS", native_status)
        .args([
            "exec",
            "--using",
            "22.22.2",
            "npm",
            "--prefix",
            "tests/compat",
            "run",
            "test:deepseek",
            "--silent",
        ])
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(15), command.output())
        .await
        .unwrap()
        .unwrap();
    let passed = output.status.success();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    std::fs::remove_dir_all(home).unwrap();
    let attempts = if kind.starts_with("tool_") { 2 } else { 1 };
    assert!(
        passed
            && stdout.contains(&format!(
                "SDK_S8_OK status={status} attempts={attempts} maxRetries=0"
            ))
            && stderr.is_empty(),
        "합성 DeepSeek SDK 검증 실패(kind={kind},status={status}); 원문오류/본문 출력없음"
    );
}
const PATH: &str = "/deepseek/v1/chat/completions";
const BODY: &str = r#" {"model":"synthetic-text","messages":[{"role":"user","content":"synthetic request"}],"max_tokens":100} "#;
fn raw(stream: bool, include: Option<bool>) -> String {
    let mut v: serde_json::Value = serde_json::from_str(BODY).unwrap();
    if stream {
        v["stream"] = true.into();
        if let Some(b) = include {
            v["stream_options"] = serde_json::json!({"include_usage":b});
        }
    }
    v.to_string()
}
fn usage() -> serde_json::Value {
    serde_json::json!({"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_cache_hit_tokens":40,"prompt_cache_miss_tokens":60,"completion_tokens_details":{"reasoning_tokens":10}})
}
fn native(stream: bool) -> String {
    let base = serde_json::json!({"id":"ds1","object":if stream{"chat.completion.chunk"}else{"chat.completion"},"model":"synthetic-text","choices":[{"index":0,"finish_reason":"stop",if stream{"delta"}else{"message"}:{"role":"assistant","content":"synthetic","reasoning_content":"synthetic thought"}}],"usage":usage()});
    if stream {
        format!("data: {base}\r\n\r\ndata: [DONE]\n\n")
    } else {
        base.to_string()
    }
}
async fn strict(h: &Harness) {
    sqlx::query("UPDATE gw_policies SET budget_mode='reserved',budget_limit_usd=1,tpm_limit=5000")
        .execute(h.db.store.pool())
        .await
        .unwrap();
}
async fn unknown(h: &Harness) {
    let s = state(h).await;
    assert_eq!((s.0, s.1), (1, 1));
    let row=sqlx::query("SELECT cost_usd IS NULL AS null_cost, execution_stopped_at IS NOT NULL AS stopped FROM gw_requests").fetch_one(h.db.store.pool()).await.unwrap();
    assert!(row.get::<bool, _>("null_cost"));
    assert!(row.get::<bool, _>("stopped"));
    let n = h.provider.calls.lock().await.len();
    let r = h.request(PATH, BODY).send().await.unwrap();
    assert_eq!(r.status(), 403);
    assert_eq!(r.headers()["x-gateway-error-code"], "unsettled_usage_block");
    assert_eq!(r.headers()["x-gateway-dispatch-state"], "not_dispatched");
    r.bytes().await.unwrap();
    h.runtime.wait_requests().await;
    assert_eq!(h.provider.calls.lock().await.len(), n);
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn deepseek_last_chunk_is_counted_once() {
    for (stream, include) in [
        (false, None),
        (true, None),
        (true, Some(false)),
        (true, Some(true)),
    ] {
        let h = Harness::new().await;
        let wire = native(stream);
        set(&h, &wire, stream).await;
        let body = raw(stream, include);
        let r = h
            .request(PATH, body.clone())
            .header("cookie", "synthetic-cookie")
            .header("openai-project", "synthetic-project")
            .header("idempotency-key", "synthetic-idempotency")
            .header("x-gateway-request-id", uuid::Uuid::nil().to_string())
            .send()
            .await
            .unwrap();
        let status = r.status();
        let id = r.headers()["x-gateway-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let received = r.bytes().await;
        if status != 200 {
            h.close().await;
            assert_eq!(status, 200, "DeepSeek native route");
            return;
        }
        assert_eq!(received.unwrap(), wire);
        assert_ne!(id, uuid::Uuid::nil().to_string());
        assert_eq!(state(&h).await, (1, 0, "0.000120000000".into(), 100, 20));
        let event: serde_json::Value =
            sqlx::query_scalar("SELECT provider_usage FROM gw_usage_events WHERE kind='final'")
                .fetch_one(h.db.store.pool())
                .await
                .unwrap();
        assert_eq!(event["prompt_cache_hit_tokens"], 40);
        assert_eq!(event["prompt_cache_miss_tokens"], 60);
        assert_eq!(event["completion_tokens_details"]["reasoning_tokens"], 10);
        let calls = h.provider.calls.lock().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].path, "/chat/completions");
        assert_eq!(calls[0].body, body);
        assert_eq!(
            calls[0].headers["authorization"],
            "Bearer synthetic-deepseek-company-key"
        );
        for name in [
            "cookie",
            "x-api-key",
            "openai-project",
            "idempotency-key",
            "anthropic-version",
            "x-gateway-request-id",
        ] {
            assert!(!calls[0].headers.contains_key(name));
        }
        drop(calls);
        h.close().await;
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn deepseek_strict_context_reserves_and_settles_native_thinking() {
    for stream in [false, true] {
        let h = Harness::with_config(configuration_options(true, "fixed_admission_v1", "")).await;
        strict(&h).await;
        set(&h, &native(stream), stream).await;
        let (started, release) = h.provider.hold_response().await;
        let body = raw(stream, None).replacen('{', "{\"thinking\":{\"type\":\"enabled\"},", 1);
        let request = h.request(PATH, body).send();
        tokio::pin!(request);
        tokio::select! {r=&mut request=>panic!("unexpected early response {}",r.is_ok()),_=started=>{}}
        let snapshot:(String,i64)=sqlx::query_as("SELECT reserved_usd::text,(SELECT reserved_tokens FROM gw_rate_windows) FROM gw_usage_totals").fetch_one(h.db.store.pool()).await.unwrap();
        assert_eq!(snapshot, ("0.004296000000".into(), 4196));
        release.send(()).unwrap();
        let r = request.await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.bytes().await.unwrap(), native(stream));
        assert_eq!(state(&h).await, (1, 0, "0.000120000000".into(), 100, 20));
        let left:(String,i64)=sqlx::query_as("SELECT reserved_usd::text,(SELECT settled_tokens FROM gw_rate_windows) FROM gw_usage_totals").fetch_one(h.db.store.pool()).await.unwrap();
        assert_eq!(left, ("0.000000000000".into(), 120));
        h.close().await;
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn deepseek_unresolved_time_is_unknown_not_fixed_or_zero() {
    for stream in [false, true] {
        let h = Harness::with_config(configuration_options(
            true,
            "deepseek_time_unresolved_v1",
            "",
        ))
        .await;
        set(&h, &native(stream), stream).await;
        let r = h.request(PATH, raw(stream, None)).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.bytes().await.unwrap(), native(stream));
        unknown(&h).await;
        let row=sqlx::query("SELECT pricing_snapshot,execution_snapshot FROM gw_requests WHERE accounting_state='unknown'").fetch_one(h.db.store.pool()).await.unwrap();
        let p: serde_json::Value = row.get("pricing_snapshot");
        let e: serde_json::Value = row.get("execution_snapshot");
        assert_eq!(p["billing_time_rule"], "deepseek_time_unresolved_v1");
        assert!(e["cost_bound_usd"].is_null());
        h.close().await;
    }
    for (verified, time) in [
        (false, "fixed_admission_v1"),
        (true, "deepseek_time_unresolved_v1"),
    ] {
        let h = Harness::with_config(configuration_options(verified, time, "")).await;
        strict(&h).await;
        let r = h.request(PATH, BODY).send().await.unwrap();
        assert_eq!(r.status(), 400);
        assert_eq!(
            r.headers()["x-gateway-error-code"],
            "unsupported_strict_policy"
        );
        r.bytes().await.unwrap();
        assert_eq!(state(&h).await.0, 0);
        assert!(h.provider.calls.lock().await.is_empty());
        h.close().await;
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL and fixed SDK"]
async fn deepseek_fixed_sdk_last_normal_chunk_usage_and_one_send() {
    for kind in ["plain", "stream", "stream_false", "stream_true"] {
        let h = Harness::new().await;
        let stream = kind.starts_with("stream");
        set(&h, &native(stream), stream).await;
        sdk(&h, kind, 200, "completed", &h.token).await;
        assert_eq!(state(&h).await, (1, 0, "0.000120000000".into(), 100, 20));
        assert_eq!(h.provider.calls.lock().await.len(), 1);
        h.close().await;
    }
}

async fn api(
    h: &Harness,
    method: reqwest::Method,
    path: &str,
    input: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    let client = reqwest::Client::builder()
        .no_proxy()
        .retry(reqwest::retry::never())
        .build()
        .unwrap();
    let mut r = client
        .request(method, format!("{}{path}", h.base))
        .bearer_auth(&h.token)
        .header("x-gateway-command-id", uuid::Uuid::new_v4().to_string());
    if let Some(input) = input {
        r = r
            .header("content-type", "application/json")
            .body(input.to_string());
    }
    let r = r.send().await.unwrap();
    let status = r.status().as_u16();
    let v = if status == 204 {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&r.bytes().await.unwrap()).unwrap()
    };
    (status, v)
}
fn grants() -> serde_json::Value {
    serde_json::json!([{"provider":"deepseek","model":"synthetic-text","capabilities":["chat_completions"]}])
}
fn limits() -> serde_json::Value {
    serde_json::json!({"budget":null,"rpm":null,"tpm":null,"count_rpm":null})
}
async fn key(h: &Harness, service: Option<uuid::Uuid>) -> (String, uuid::Uuid) {
    let mut input = serde_json::json!({"name":"합성 DeepSeek key","validity":"3_months","grants":grants(),"limits":{"budget":{"mode":"actual","limit_usd":"1.000000000000","period":{"kind":"calendar_month","timezone":"UTC"}},"rpm":null,"tpm":null,"count_rpm":null}});
    if let Some(service) = service {
        input["service_principal_id"] = service.to_string().into();
    }
    let (status, v) = api(h, reqwest::Method::POST, "/gateway/keys", Some(input)).await;
    assert_eq!(status, 201);
    (
        v["key"].as_str().unwrap().to_owned(),
        v["metadata"]["id"].as_str().unwrap().parse().unwrap(),
    )
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn deepseek_personal_and_service_keys_apply_both_scopes_owner_and_revoke() {
    for (path, body) in [(PATH, BODY)] {
        let h = Harness::new().await;
        h.provider
            .set(
                StatusCode::OK,
                HeaderMap::from_iter([(
                    "content-type".parse().unwrap(),
                    "application/json".parse().unwrap(),
                )]),
                Bytes::from(native(false)),
            )
            .await;
        let (personal, personal_id) = key(&h, None).await;
        let r = h
            .request_with_token(path, body, &personal)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        r.bytes().await.unwrap();
        h.runtime.wait_requests().await;
        let b = sqlx::query(
        "SELECT count(*) AS n,min(remaining_reservation_usd)::text AS held FROM gw_request_budgets",
    )
    .fetch_one(h.db.store.pool())
    .await
    .unwrap();
        assert_eq!(b.get::<i64, _>("n"), 2);
        assert_eq!(b.get::<String, _>("held"), "0.000000000000");
        let(status,v)=api(&h,reqwest::Method::POST,"/gateway/admin/principals",Some(serde_json::json!({"display_name":"합성 DeepSeek service","management_owner_id":h.principal}))).await;
        assert_eq!(status, 201);
        let service: uuid::Uuid = v["target"]["id"].as_str().unwrap().parse().unwrap();
        let service_path = format!("/gateway/admin/principals/{service}");
        let(status,_)=api(&h,reqwest::Method::PUT,&format!("{service_path}/policy"),Some(serde_json::json!({"expected_version":"1","disabled":false,"grants":grants(),"limits":limits(),"reason":"합성 허용"}))).await;
        assert_eq!(status, 200);
        let (service_key, _) = key(&h, Some(service)).await;
        let r = h
            .request_with_token(path, body, &service_key)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        r.bytes().await.unwrap();
        h.runtime.wait_requests().await;
        sqlx::query("UPDATE gw_principals SET disabled_at=clock_timestamp(),disabled_reason='synthetic_owner_stop' WHERE id=$1").bind(h.principal).execute(h.db.store.pool()).await.unwrap();
        let r = h
            .request_with_token(path, body, &service_key)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
        assert_eq!(r.headers()["x-gateway-error-code"], "owner_inactive");
        drop(r);
        h.runtime.wait_requests().await;
        assert_eq!(h.provider.calls.lock().await.len(), 2);
        sqlx::query("UPDATE gw_principals SET disabled_at=NULL,disabled_reason=NULL WHERE id=$1")
            .bind(h.principal)
            .execute(h.db.store.pool())
            .await
            .unwrap();
        let (status, _) = api(
            &h,
            reqwest::Method::DELETE,
            &format!("/gateway/keys/{personal_id}?expected_version=1"),
            None,
        )
        .await;
        assert_eq!(status, 204);
        let r = h
            .request_with_token(path, body, &personal)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
        drop(r);
        h.runtime.wait_requests().await;
        assert_eq!(h.provider.calls.lock().await.len(), 2);
        h.close().await;
    }
}

async fn reservation(h: &Harness) {
    let r=sqlx::query("SELECT r.phase,b.reservation_usd::text AS amount,b.remaining_reservation_usd::text AS held,b.reserved_tokens FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id ORDER BY admitted_at DESC LIMIT 1").fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(r.get::<String, _>("phase"), "dispatching");
    assert_eq!(r.get::<String, _>("amount"), "0.004296000000");
    assert_eq!(r.get::<String, _>("held"), "0.004296000000");
    assert_eq!(r.get::<i64, _>("reserved_tokens"), 4196);
}

fn chat_start() -> String {
    "data: {\"object\":\"chat.completion.chunk\",\"id\":\"c1\",\"model\":\"synthetic-text\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"synthetic\"},\"finish_reason\":null}]}\n\n".into()
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn deepseek_full_queue_cancel_has_no_synthetic_terminal_and_holds_unknown_reservation() {
    use std::time::Duration;
    let h = Harness::with_config(configuration_options(true, "fixed_admission_v1", "")).await;
    strict(&h).await;
    let (tx, stopped) = h.provider.stream().await;
    let socket = tokio::net::TcpStream::connect(h.base.trim_start_matches("http://"))
        .await
        .unwrap();
    let mut builder = h2::client::Builder::new();
    builder.initial_window_size(0);
    let (mut sender, connection) = builder.handshake::<_, Bytes>(socket).await.unwrap();
    let connection = tokio::spawn(connection);
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("{}{PATH}", h.base))
        .header("authorization", format!("Bearer {}", h.token))
        .header("content-type", "application/json")
        .body(())
        .unwrap();
    let (r, mut send) = sender.send_request(req, false).unwrap();
    send.send_data(
        Bytes::from(BODY.replacen(
            '{',
            "{\"stream\":true,\"stream_options\":{\"include_usage\":true},",
            1,
        )),
        true,
    )
    .unwrap();
    tx.send(Bytes::from(chat_start())).await.unwrap();
    let response = r.await.unwrap();
    assert_eq!(response.status(), 200);
    reservation(&h).await;
    let producer = tokio::spawn(async move {
        for _ in 1..=256 {
            let wire = format!(
                "data: {{\"object\":\"chat.completion.chunk\",\"id\":\"c1\",\"model\":\"synthetic-text\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{}\"}},\"finish_reason\":null}}],\"usage\":null}}\n\n",
                "x".repeat(16 * 1024)
            );
            if tx.send(Bytes::from(wire)).await.is_err() {
                break;
            }
        }
        std::future::pending::<()>().await;
    });
    let mut changed = h.runtime.resource_changes();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if h.runtime.request_resources().queue_waiters > 0 {
                break;
            }
            changed.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(h.runtime.active_requests(), 1);
    assert_eq!(h.runtime.available_inference(), 15);
    send.send_reset(h2::Reason::CANCEL);
    drop(response);
    tokio::time::timeout(Duration::from_secs(2), stopped)
        .await
        .unwrap()
        .unwrap();
    h.runtime.wait_requests().await;
    unknown(&h).await;
    let resources = h.runtime.request_resources();
    assert_eq!(
        (
            resources.queue_bytes,
            resources.queue_waiters,
            resources.body_bytes
        ),
        (0, 0, 0)
    );
    assert!(resources.queue_peak > 48 * 1024 && resources.queue_peak <= 64 * 1024);
    assert!(resources.parser_peak <= 256 * 1024);
    producer.abort();
    let _ = producer.await;
    drop(sender);
    connection.abort();
    let _ = connection.await;
    h.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn deepseek_absolute_transport_deadline_preserves_five_second_finalization_gap() {
    let cfg = configuration_options(
        true,
        "fixed_admission_v1",
        "\nlimits:\n  transport_timeout_ms: 1500\n  request_timeout_ms: 6500\n  body_timeout_ms: 1500\n  body_idle_timeout_ms: 1500\n",
    );
    let h = Harness::with_config(cfg).await;
    strict(&h).await;
    let (tx, stopped) = h.provider.stream().await;
    tx.send(Bytes::from(chat_start())).await.unwrap();
    let r = h
        .request(
            PATH,
            BODY.replacen(
                '{',
                "{\"stream\":true,\"stream_options\":{\"include_usage\":true},",
                1,
            ),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    reservation(&h).await;
    assert!(r.bytes().await.is_err());
    tokio::time::timeout(std::time::Duration::from_secs(1), stopped)
        .await
        .unwrap()
        .unwrap();
    h.runtime.wait_requests().await;
    unknown(&h).await;
    let row=sqlx::query("SELECT execution_stopped_at IS NOT NULL AS stopped,extract(epoch FROM execution_deadline-transport_deadline)::bigint AS gap FROM gw_requests WHERE accounting_state='unknown'").fetch_one(h.db.store.pool()).await.unwrap();
    assert!(row.get::<bool, _>("stopped"));
    assert_eq!(row.get::<i64, _>("gap"), 5);
    drop(tx);
    h.close().await;
}

#[path = "support/finance.rs"]
mod finance;
#[path = "support/pg_proxy.rs"]
mod pg_proxy;
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn deepseek_uncertain_dispatch_original_id_expires_unknown_without_reissuing_permit() {
    use m_gated::{
        config::{ApiKind, ProviderId},
        ledger::finance::{Finalized, LedgerError, RequestSpec},
        providers::transport::{NativeClient, SendOptions},
    };
    use std::sync::atomic::Ordering;
    for api in [ApiKind::DeepseekChat] {
        let db = postgres::TestDb::new().await;
        let principal = finance::human(db.store.pool()).await;
        sqlx::query("UPDATE gw_policies SET allow_rules=$2,budget_mode='actual',budget_limit_usd=1,tpm_limit=NULL WHERE principal_id=$1").bind(principal).bind(grants()).execute(db.store.pool()).await.unwrap();
        let current = finance::current(db.store.pool(), principal).await;
        let proxy = pg_proxy::CommitProxy::new().await;
        let store = db.clock_store(Some(proxy.port)).await;
        let cfg = configuration_options(true, "fixed_admission_v1", "");
        let profile = cfg.providers()[0]
            .profiles
            .iter()
            .find(|p| p.api == api)
            .unwrap();
        let ledger = PgLedger::new(store.clone(), AccessSettings::from_config(&cfg), [1; 32]);
        let spec = RequestSpec::from_profile(ProviderId::Deepseek, profile, Some(100));
        ledger.admit_request(&current, &spec).await.unwrap();
        let provider = provider::Provider::new();
        let server = provider.serve().await;
        let client = NativeClient::deepseek_loopback_for_test(
            server.base.clone(),
            "synthetic-deepseek-company-key".into(),
        )
        .unwrap();
        let runtime = m_gated::runtime::Runtime::new(config::Limits::default()).unwrap();
        let headers = HeaderMap::from_iter([(
            "content-type".parse().unwrap(),
            "application/json".parse().unwrap(),
        )]);
        proxy.armed.store(true, Ordering::SeqCst);
        match ledger.authorize_dispatch(spec.id).await {
            Err(e) => assert_eq!(e, LedgerError::DispatchUncertain),
            Ok(permit) => {
                let _ = client
                    .send(
                        permit,
                        Bytes::from_static(BODY.as_bytes()),
                        SendOptions {
                            api,
                            headers: &headers,
                            profile,
                            deadline: tokio::time::Instant::now()
                                + std::time::Duration::from_secs(1),
                            runtime: &runtime,
                        },
                    )
                    .await;
                panic!("불명COMMIT이송신권한을발급했다");
            }
        }
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            proxy.committed.notified(),
        )
        .await
        .unwrap();
        let healthy = db.clock_store(None).await;
        let second = PgLedger::new(healthy.clone(), AccessSettings::from_config(&cfg), [1; 32]);
        assert!(matches!(
            second.authorize_dispatch(spec.id).await,
            Err(LedgerError::Conflict)
        ));
        assert_eq!(second.recover_expired(spec.id).await.unwrap(), None);
        sqlx::query("UPDATE gw_fixture_clock SET observed=(SELECT lease_expires_at FROM gw_requests WHERE id=$1)").bind(spec.id).execute(healthy.pool()).await.unwrap();
        assert_eq!(
            second.recover_expired(spec.id).await.unwrap(),
            Some(Finalized::Unknown)
        );
        assert_eq!(second.recover_expired(spec.id).await.unwrap(), None);
        let row=sqlx::query("SELECT r.accounting_state,b.remaining_reservation_usd::text AS held,b.period_start::text AS period,t.unknown_count FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id JOIN gw_usage_totals t ON (t.scope_id,t.period_start)=(b.scope_id,b.period_start) WHERE r.id=$1").bind(spec.id).fetch_one(healthy.pool()).await.unwrap();
        assert_eq!(row.get::<String, _>("accounting_state"), "unknown");
        assert_eq!(row.get::<String, _>("held"), "0.000000000000");
        assert!(row.get::<String, _>("period").starts_with("2025-03-01"));
        assert_eq!(row.get::<i64, _>("unknown_count"), 1);
        assert!(provider.calls.lock().await.is_empty());
        store.pool().close().await;
        healthy.pool().close().await;
        drop(proxy);
        db.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn deepseek_usage_and_terminal_negative_matrix_keeps_unknown_reservation() {
    for kind in [
        "missing_usage",
        "hit_miss",
        "reasoning",
        "total",
        "alias",
        "tier",
        "usage_only",
        "no_done",
        "duplicate",
        "malformed",
        "early_tier",
    ] {
        let h = Harness::with_config(configuration_options(true, "fixed_admission_v1", "")).await;
        strict(&h).await;
        let mut value: serde_json::Value = serde_json::from_str(&native(false)).unwrap();
        let stream = matches!(
            kind,
            "usage_only" | "no_done" | "duplicate" | "malformed" | "early_tier"
        );
        match kind {
            "missing_usage" => {
                value.as_object_mut().unwrap().remove("usage");
            }
            "hit_miss" => value["usage"]["prompt_cache_miss_tokens"] = 59.into(),
            "reasoning" => {
                value["usage"]["completion_tokens_details"]["reasoning_tokens"] = 21.into()
            }
            "total" => value["usage"]["total_tokens"] = 121.into(),
            "alias" => value["model"] = "unpriced-alias".into(),
            "tier" => value["service_tier"] = "discount".into(),
            _ => {}
        }
        let wire=match kind{"usage_only"=>native(true).replace("\"choices\":[{\"delta\":{\"content\":\"synthetic\",\"reasoning_content\":\"synthetic thought\",\"role\":\"assistant\"},\"finish_reason\":\"stop\",\"index\":0}]","\"choices\":[]"),"no_done"=>native(true).replace("data: [DONE]\n\n",""),"duplicate"=>native(true).replace("data: [DONE]\n\n",&native(true)),"malformed"=>native(true)+"data: {bad}\n\n","early_tier"=>chat_start().replace("\"object\":","\"service_tier\":\"discount\",\"object\":").replace("\"id\":\"c1\"","\"id\":\"ds1\"")+&native(true),_=>value.to_string()};
        set(&h, &wire, stream).await;
        let r = h.request(PATH, raw(stream, None)).send().await.unwrap();
        assert!([200, 502].contains(&r.status().as_u16()));
        let _ = r.bytes().await;
        unknown(&h).await;
        let row:(String,i64)=sqlx::query_as("SELECT remaining_reservation_usd::text,remaining_reserved_tokens FROM gw_request_budgets WHERE state='unknown'").fetch_one(h.db.store.pool()).await.unwrap();
        assert_eq!(
            row,
            ("0.004296000000".into(), 4196),
            "unknown 예약 보존: {kind}"
        );
        h.close().await;
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn deepseek_native_features_preserve_wire_and_reject_unsupported_without_rows() {
    let h = Harness::new().await;
    for extra in [
        serde_json::json!({"thinking":{"type":"enabled"}}),
        serde_json::json!({"thinking":{"type":"disabled"},"tool_choice":"required"}),
        serde_json::json!({"response_format":{"type":"json_object"}}),
        serde_json::json!({"tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object","properties":{"model":{"type":"string"}}}}}],"tool_choice":"auto"}),
        serde_json::json!({"messages":[{"role":"assistant","content":null,"reasoning_content":"synthetic","tool_calls":[{"id":"c1","type":"function","function":{"name":"f","arguments":"{not parsed JSON}"}}]},{"role":"tool","content":"synthetic result","tool_call_id":"c1"}]}),
    ] {
        let mut b: serde_json::Value = serde_json::from_str(BODY).unwrap();
        b.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let wire = if b.get("response_format").is_some() {
            feature_native(false, false, true)
        } else if b["thinking"]["type"] == "disabled" {
            nonthinking_native(false)
        } else {
            native(false)
        };
        set(&h, &wire, false).await;
        let body = b.to_string();
        let r = h.request(PATH, body.clone()).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.bytes().await.unwrap(), wire);
        h.runtime.wait_requests().await;
        assert_eq!(h.provider.calls.lock().await.last().unwrap().body, body);
    }
    for extra in [
        serde_json::json!({"max_completion_tokens":100}),
        serde_json::json!({"store":false}),
        serde_json::json!({"reasoning_effort":"high"}),
        serde_json::json!({"n":1}),
        serde_json::json!({"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://invalid.invalid/x"}}]}]}),
        serde_json::json!({"messages":[{"role":"assistant","content":"x","prefix":true}]}),
        serde_json::json!({"tools":[{"type":"web_search"}]}),
        serde_json::json!({"tools":[{"type":"fetch"}]}),
        serde_json::json!({"response_format":{"type":"json_schema","json_schema":{"name":"x","schema":{}}}}),
    ] {
        let mut b: serde_json::Value = serde_json::from_str(BODY).unwrap();
        b.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let r = h.request(PATH, b.to_string()).send().await.unwrap();
        assert_eq!(r.status(), 400);
        assert_eq!(r.headers()["x-gateway-dispatch-state"], "not_dispatched");
        r.bytes().await.unwrap();
    }
    for path in [
        "/deepseek/v1/responses",
        "/deepseek/v1/embeddings",
        "/deepseek/beta/completions",
        "/deepseek/anthropic/v1/messages",
        "/deepseek/v1/search",
        "/deepseek/v1/fetch",
    ] {
        let r = h.request(path, BODY).send().await.unwrap();
        assert_eq!(r.status(), 404);
        r.bytes().await.unwrap();
    }
    assert_eq!(state(&h).await.0, 5);
    assert_eq!(h.provider.calls.lock().await.len(), 5);
    h.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL and fixed SDK"]
async fn deepseek_company_auth_redirect_encoding_and_native_errors_never_retry() {
    for kind in [
        "auth401",
        "auth403",
        "redirect",
        "encoding",
        "reset",
        "native_error",
    ] {
        let h = Harness::with_reset(configuration(), kind == "reset").await;
        let target = provider::Provider::new();
        let target_server = target.serve().await;
        let (status,headers,body)=match kind{
 "auth401"|"auth403"=>(if kind=="auth401"{StatusCode::UNAUTHORIZED}else{StatusCode::FORBIDDEN},HeaderMap::new(),Bytes::from_static(b"synthetic-deepseek-company-secret")),
 "redirect"=>(StatusCode::TEMPORARY_REDIRECT,HeaderMap::from_iter([("location".parse().unwrap(),target_server.base.as_str().parse().unwrap())]),Bytes::new()),
 "encoding"=>(StatusCode::OK,HeaderMap::from_iter([("content-type".parse().unwrap(),"application/json".parse().unwrap()),("content-encoding".parse().unwrap(),"gzip".parse().unwrap())]),Bytes::from(native(false))),
 "native_error"=>(StatusCode::TOO_MANY_REQUESTS,HeaderMap::from_iter([("content-type".parse().unwrap(),"application/json".parse().unwrap()),("set-cookie".parse().unwrap(),"synthetic-forbidden".parse().unwrap())]),Bytes::from_static(br#"{"error":{"message":"synthetic rate","type":"rate_limit_error","param":null,"code":"rate_limit"}}"#)),_ => (StatusCode::OK,HeaderMap::new(),Bytes::new())};
        h.provider.set(status, headers, body.clone()).await;
        if kind.starts_with("auth") {
            sdk(&h, "plain", 502, "failed", &h.token).await;
        } else {
            let r = h.request(PATH, BODY).send().await.unwrap();
            assert_eq!(r.status(), if kind == "native_error" { 429 } else { 502 });
            assert_eq!(r.headers()["x-should-retry"], "false");
            for name in [
                "x-gateway-dispatch-state",
                "location",
                "set-cookie",
                "www-authenticate",
            ] {
                assert!(!r.headers().contains_key(name));
            }
            let bytes = r.bytes().await.unwrap();
            if kind == "native_error" {
                assert_eq!(bytes, body);
            } else {
                let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(v["error"]["type"], "server_error");
                assert!(!String::from_utf8_lossy(&bytes).contains("synthetic-"));
            }
        }
        unknown(&h).await;
        assert_eq!(h.provider.calls.lock().await.len(), 1);
        assert!(target.calls.lock().await.is_empty());
        h.close().await;
    }
}

fn error_wire(named: bool, size: usize, ending: &str) -> String {
    format!(
        "{}data: {{\"error\":{{\"message\":\"{}\",\"type\":\"server_error\",\"param\":\"synthetic_param\",\"code\":\"synthetic_stream_error\"}}}}{ending}{ending}",
        if named {
            format!("event: error{ending}")
        } else {
            String::new()
        },
        "x".repeat(size)
    )
}
async fn failed_unknown(h: &Harness) {
    unknown(h).await;
    let outcome: String =
        sqlx::query_scalar("SELECT outcome FROM gw_requests WHERE accounting_state='unknown'")
            .fetch_one(h.db.store.pool())
            .await
            .unwrap();
    assert_eq!(outcome, "failed");
    assert_eq!(h.runtime.active_requests(), 0);
    assert_eq!(h.runtime.available_inference(), 16);
    let r = h.runtime.request_resources();
    assert_eq!((r.body_bytes, r.queue_bytes, r.queue_waiters), (0, 0, 0));
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL and fixed SDK"]
async fn deepseek_native_error_sdk_abort_preserves_failed_and_no_final_usage() {
    for named in [false, true] {
        let h = Harness::with_config(configuration_options(true, "fixed_admission_v1", "")).await;
        strict(&h).await;
        let (tx, stopped) = h.provider.stream().await;
        tx.send(Bytes::from(error_wire(named, 50000, "\r\n")))
            .await
            .unwrap();
        sdk(&h, "native_error", 200, "failed", &h.token).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), stopped)
            .await
            .unwrap()
            .unwrap();
        drop(tx);
        failed_unknown(&h).await;
        let usage: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT e.normalized_usage FROM gw_usage_events e JOIN gw_requests r ON r.id=e.request_id WHERE e.kind='final' AND r.dispatch_authorized_at IS NOT NULL")
                .fetch_optional(h.db.store.pool())
                .await
                .unwrap();
        assert!(usage.is_none());
        h.close().await;
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn deepseek_native_error_wire_and_size_shape_boundaries() {
    for (named, ending) in [(false, "\n"), (true, "\r\n"), (true, "\r")] {
        for size in [65535, 65536, 65537] {
            let h =
                Harness::with_config(configuration_options(true, "fixed_admission_v1", "")).await;
            strict(&h).await;
            let empty = error_wire(named, 0, ending);
            let wire = error_wire(named, size - empty.len(), ending);
            assert_eq!(wire.len(), size);
            set(&h, &wire, true).await;
            let r = h.request(PATH, raw(true, None)).send().await.unwrap();
            let status = r.status();
            if status != 200 {
                let code = match r
                    .headers()
                    .get("x-gateway-error-code")
                    .and_then(|v| v.to_str().ok())
                {
                    Some("not_ready") => 1,
                    Some("db_unavailable") => 2,
                    Some("auth_dependency_unavailable") => 3,
                    Some("finalization_unavailable") => 4,
                    Some("stream_limit") => 5,
                    _ => 0,
                };
                eprintln!(
                    "S9_BOUNDARY_DIAGNOSTIC named={} ending={} size={} status={} code={} ready={} tasks={} permits={}",
                    u8::from(named),
                    ending.len(),
                    size,
                    status.as_u16(),
                    code,
                    u8::from(h.runtime.lifecycle() == m_gated::runtime::Lifecycle::Ready),
                    h.runtime.active_requests(),
                    h.runtime.available_inference()
                );
            }
            let received = r.bytes().await;
            if size <= 65536 {
                assert_eq!(status, 200);
                assert_eq!(received.unwrap(), wire);
            } else {
                assert!(status == 502 || received.is_err());
            }
            failed_unknown(&h).await;
            h.close().await;
        }
    }
    for stream in [false, true] {
        for error in [
            serde_json::json!({}),
            serde_json::json!({"message":"synthetic","type":3}),
            serde_json::json!(false),
        ] {
            let h = Harness::new().await;
            let mut v: serde_json::Value = serde_json::from_str(&native(false)).unwrap();
            v["error"] = error;
            let wire = if stream {
                format!("data: {v}\n\n")
            } else {
                v.to_string()
            };
            set(&h, &wire, stream).await;
            let r = h.request(PATH, raw(stream, None)).send().await.unwrap();
            assert_eq!(r.status(), 502);
            assert_eq!(
                r.headers()["x-gateway-error-code"],
                "upstream_protocol_error"
            );
            r.bytes().await.unwrap();
            failed_unknown(&h).await;
            h.close().await;
        }
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn deepseek_nonstream_and_stream_large_native_bytes_are_bounded() {
    for size in [16 * 1024 * 1024 - 1, 16 * 1024 * 1024, 16 * 1024 * 1024 + 1] {
        let h = Harness::new().await;
        let base = native(false);
        let wire = base.replace(
            "\"content\":\"synthetic\"",
            &format!(
                "\"content\":\"{}\"",
                "x".repeat(size - base.len() + "synthetic".len())
            ),
        );
        assert_eq!(wire.len(), size);
        h.provider
            .set_chunked(
                StatusCode::OK,
                HeaderMap::from_iter([(
                    "content-type".parse().unwrap(),
                    "application/json".parse().unwrap(),
                )]),
                Bytes::from(wire.clone()),
            )
            .await;
        let r = h.request(PATH, BODY).send().await.unwrap();
        if size <= 16 * 1024 * 1024 {
            assert_eq!(r.status(), 200);
            assert_eq!(r.bytes().await.unwrap(), wire);
            assert_eq!(state(&h).await, (1, 0, "0.000120000000".into(), 100, 20));
        } else {
            assert_eq!(r.status(), 502);
            r.bytes().await.unwrap();
            unknown(&h).await;
        }
        assert_eq!(h.provider.calls.lock().await.len(), 1);
        h.close().await;
    }
    let h = Harness::new().await;
    let mut first = chat_start().replace("\"c1\"", "\"ds1\"");
    first = first.replace("synthetic", &"x".repeat(512 * 1024)); // model은 아래에서 복원한다.
    first = first.replace(
        &format!("\"model\":\"{}-text\"", "x".repeat(512 * 1024)),
        "\"model\":\"synthetic-text\"",
    );
    let wire = first + &native(true);
    set(&h, &wire, true).await;
    let r = h.request(PATH, raw(true, None)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.bytes().await.unwrap(), wire);
    assert_eq!(state(&h).await.1, 0);
    assert!(h.runtime.request_resources().parser_peak <= 256 * 1024);
    h.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn deepseek_price_snapshot_survives_mutation_and_alias_uncertainty() {
    use m_gated::{
        config::ProviderId,
        ledger::finance::{FinalObservation, FinalOutcome, FinalUsage, Finalized, RequestSpec},
    };
    let db = postgres::TestDb::new().await;
    let id = finance::human(db.store.pool()).await;
    sqlx::query("UPDATE gw_policies SET allow_rules=$2 WHERE principal_id=$1")
        .bind(id)
        .bind(grants())
        .execute(db.store.pool())
        .await
        .unwrap();
    let current = finance::current(db.store.pool(), id).await;
    let cfg = configuration();
    let mut profile = cfg.providers()[0].profiles[0].clone();
    let ledger = PgLedger::new(db.store.clone(), AccessSettings::from_config(&cfg), [1; 32]);
    let spec = RequestSpec::from_profile(ProviderId::Deepseek, &profile, Some(100));
    ledger.admit_request(&current, &spec).await.unwrap();
    let _permit = ledger.authorize_dispatch(spec.id).await.unwrap();
    profile
        .rates
        .insert("input".into(), "999.000000000000".into());
    let observation = FinalObservation {
        outcome: FinalOutcome::Completed,
        usage: Some(FinalUsage {
            actual_model: "synthetic-text".into(),
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 40,
            cache_write_tokens: 0,
            reasoning_tokens: Some(10),
        }),
        counted_input_tokens: None,
        reason: "completed",
        http_status: Some(200),
        provider_request_id: None,
    };
    assert_eq!(
        ledger
            .finalize_request(spec.id, &observation)
            .await
            .unwrap(),
        Finalized::Settled
    );
    assert_eq!(
        ledger
            .finalize_request(spec.id, &observation)
            .await
            .unwrap(),
        Finalized::Noop
    );
    let cost: String = sqlx::query_scalar("SELECT cost_usd::text FROM gw_requests")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    assert_eq!(cost, "0.000120000000");
    db.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn deepseek_unknown_spans_next_month_other_providers() {
    use m_gated::{
        config::{ApiKind, ProviderId},
        ledger::finance::{FinalObservation, FinalOutcome, Finalized, LedgerError, RequestSpec},
    };
    let db = postgres::TestDb::new().await;
    let store = db.clock_store(None).await;
    let id = finance::human(store.pool()).await;
    let rules = serde_json::json!([{"provider":"deepseek","model":"synthetic-text","capabilities":["chat_completions"]},{"provider":"openai","model":"synthetic-text","capabilities":["chat_completions"]},{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}]);
    sqlx::query("UPDATE gw_policies SET allow_rules=$2,budget_limit_usd=1 WHERE principal_id=$1")
        .bind(id)
        .bind(rules)
        .execute(store.pool())
        .await
        .unwrap();
    let cfg = configuration_options(true, "fixed_admission_v1", "");
    let current = finance::current(store.pool(), id).await;
    let ledger = PgLedger::new(store.clone(), AccessSettings::from_config(&cfg), [1; 32]);
    let profile = cfg.providers()[0].profiles[0].clone();
    let root = RequestSpec::from_profile(ProviderId::Deepseek, &profile, Some(100));
    ledger.admit_request(&current, &root).await.unwrap();
    let _permit = ledger.authorize_dispatch(root.id).await.unwrap();
    assert_eq!(
        ledger
            .finalize_request(
                root.id,
                &FinalObservation {
                    outcome: FinalOutcome::Completed,
                    usage: None,
                    counted_input_tokens: None,
                    reason: "completed",
                    http_status: Some(200),
                    provider_request_id: None
                }
            )
            .await
            .unwrap(),
        Finalized::Unknown
    );
    sqlx::query("UPDATE gw_fixture_clock SET observed='2025-04-01T00:00:00Z'")
        .execute(store.pool())
        .await
        .unwrap();
    for provider in [
        ProviderId::Deepseek,
        ProviderId::Openai,
        ProviderId::Anthropic,
    ] {
        let mut p = profile.clone();
        let mut provider_spec = RequestSpec::from_profile(provider, &p, Some(100));
        if provider == ProviderId::Openai {
            p.api = ApiKind::OpenaiChat;
            p.billing_time_rule = None;
            provider_spec = RequestSpec::from_profile(provider, &p, Some(100));
        }
        if provider == ProviderId::Anthropic {
            p.api = ApiKind::Messages;
            p.billing_time_rule = None;
            provider_spec = RequestSpec::from_profile(provider, &p, Some(100));
        }
        ledger
            .admit_request(&current, &provider_spec)
            .await
            .unwrap();
        assert!(matches!(
            ledger.authorize_dispatch(provider_spec.id).await,
            Err(LedgerError::Unsettled)
        ));
    }
    let original:(String,String)=sqlx::query_as("SELECT r.accounting_state,b.period_start::text FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id WHERE r.id=$1").bind(root.id).fetch_one(store.pool()).await.unwrap();
    assert_eq!(original.0, "unknown");
    assert!(original.1.starts_with("2025-03-01"));
    store.pool().close().await;
    db.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL and fixed SDK"]
async fn deepseek_cookie_bad_credentials_and_other_methods_never_dispatch() {
    let h = Harness::new().await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .retry(reqwest::retry::never())
        .build()
        .unwrap();
    let cookie = client
        .post(format!("{}{PATH}", h.base))
        .header("cookie", format!("mgated_access={}", h.token))
        .header("content-type", "application/json")
        .body(BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(cookie.status(), 401);
    assert_eq!(
        cookie.headers()["x-gateway-dispatch-state"],
        "not_dispatched"
    );
    let v: serde_json::Value = serde_json::from_slice(&cookie.bytes().await.unwrap()).unwrap();
    assert_eq!(v["error"]["type"], "authentication_error");
    sdk(&h, "plain", 401, "failed", "synthetic-invalid-token").await;
    for method in [
        reqwest::Method::GET,
        reqwest::Method::HEAD,
        reqwest::Method::OPTIONS,
        reqwest::Method::PUT,
        reqwest::Method::DELETE,
    ] {
        let r = client
            .request(method.clone(), format!("{}{PATH}", h.base))
            .bearer_auth(&h.token)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 405);
        assert_eq!(r.headers()["allow"], "POST");
        if method != reqwest::Method::HEAD {
            let v: serde_json::Value = serde_json::from_slice(&r.bytes().await.unwrap()).unwrap();
            assert_eq!(v["error"]["type"], "server_error");
        }
    }
    assert!(h.provider.calls.lock().await.is_empty());
    assert_eq!(state(&h).await.0, 0);
    h.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn deepseek_verified_alias_and_output_bound_violation_remain_independent() {
    let h = Harness::with_config(configuration_options(true, "fixed_admission_v1", "")).await;
    strict(&h).await;
    let wire = native(false).replace("synthetic-text", "synthetic-deepseek-version");
    set(&h, &wire, false).await;
    let r = h.request(PATH, BODY).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.bytes().await.unwrap(), wire);
    assert_eq!(state(&h).await, (1, 0, "0.000120000000".into(), 100, 20));
    let model: String =
        sqlx::query_scalar("SELECT actual_model FROM gw_usage_events WHERE kind='final'")
            .fetch_one(h.db.store.pool())
            .await
            .unwrap();
    assert_eq!(model, "synthetic-deepseek-version");
    let mut v: serde_json::Value = serde_json::from_str(&native(false)).unwrap();
    v["usage"]["completion_tokens"] = 101.into();
    v["usage"]["total_tokens"] = 201.into();
    set(&h, &v.to_string(), false).await;
    let r = h.request(PATH, BODY).send().await.unwrap();
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    h.runtime.wait_requests().await;
    let paused: (String, i64) = sqlx::query_as(
        "SELECT inference_pause_reason,inference_safety_version FROM gw_principals WHERE id=$1",
    )
    .bind(h.principal)
    .fetch_one(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(paused, ("cost_bound_violation".into(), 2));
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gw_audit_events WHERE actor_kind='system'")
            .fetch_one(h.db.store.pool())
            .await
            .unwrap();
    assert_eq!(count, 1);
    let outcome: (String, String) = sqlx::query_as(
        "SELECT accounting_state,cost_usd::text FROM gw_requests ORDER BY admitted_at DESC LIMIT 1",
    )
    .fetch_one(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(outcome, ("settled".into(), "0.000282000000".into()));
    let r = h.request(PATH, BODY).send().await.unwrap();
    assert_eq!(r.status(), 403);
    assert_eq!(r.headers()["x-gateway-error-code"], "inference_paused");
    r.bytes().await.unwrap();
    assert_eq!(h.provider.calls.lock().await.len(), 2);
    h.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn deepseek_optional_reasoning_does_not_invent_provider_numeric_fields() {
    let h = Harness::new().await;
    let mut v: serde_json::Value = serde_json::from_str(&native(false)).unwrap();
    v["usage"]
        .as_object_mut()
        .unwrap()
        .remove("completion_tokens_details");
    set(&h, &v.to_string(), false).await;
    let r = h.request(PATH, BODY).send().await.unwrap();
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    assert_eq!(state(&h).await, (1, 0, "0.000120000000".into(), 100, 20));
    let row = sqlx::query(
        "SELECT provider_usage,normalized_usage FROM gw_usage_events WHERE kind='final'",
    )
    .fetch_one(h.db.store.pool())
    .await
    .unwrap();
    let p: serde_json::Value = row.get("provider_usage");
    let n: serde_json::Value = row.get("normalized_usage");
    assert!(
        p.get("completion_tokens_details").is_none(),
        "없는Provider numeric detail을합성하지않음"
    );
    assert!(n["reasoning_tokens"].is_null());
    h.close().await;
}

fn nonthinking_native(stream: bool) -> String {
    let mut v: serde_json::Value = serde_json::from_str(&native(false)).unwrap();
    v["choices"][0]["message"]
        .as_object_mut()
        .unwrap()
        .remove("reasoning_content");
    v["usage"]
        .as_object_mut()
        .unwrap()
        .remove("completion_tokens_details");
    if stream {
        v["object"] = "chat.completion.chunk".into();
        let message = v["choices"][0]
            .as_object_mut()
            .unwrap()
            .remove("message")
            .unwrap();
        v["choices"][0]["delta"] = message;
        format!("data: {v}\r\n\r\ndata: [DONE]\n\n")
    } else {
        v.to_string()
    }
}
async fn no_dispatch(h: &Harness, body: String) -> (u16, Option<String>, bool, bool) {
    let before = state(h).await;
    let calls = h.provider.calls.lock().await.len();
    let r = h.request(PATH, body).send().await.unwrap();
    let status = r.status().as_u16();
    let dispatch = r
        .headers()
        .get("x-gateway-dispatch-state")
        .map(|v| v.to_str().unwrap().to_owned());
    r.bytes().await.unwrap();
    let after = state(h).await;
    (
        status,
        dispatch,
        after == before,
        h.provider.calls.lock().await.len() == calls,
    )
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn deepseek_review_r1_effective_thinking_http_pg() {
    for proof in [false, true] {
        let features = if proof {
            "client_tools, thinking, cache, json_schema"
        } else {
            "client_tools, cache, json_schema"
        };
        let h = Harness::with_config(configuration_features(
            true,
            "fixed_admission_v1",
            "",
            features,
        ))
        .await;
        strict(&h).await;
        sqlx::query("UPDATE gw_policies SET tpm_limit=100000,rpm_limit=100")
            .execute(h.db.store.pool())
            .await
            .unwrap();
        let mut count = 0;
        for mode in [None, Some("enabled"), Some("disabled")] {
            for choice in [
                None,
                Some(r#""auto""#),
                Some(r#""none""#),
                Some(r#""required""#),
                Some(r#"{"type":"function","function":{"name":"f"}}"#),
            ] {
                for reverse in [false, true] {
                    let thinking = mode
                        .map(|m| format!(r#""thinking":{{"type":"{m}"}},"#))
                        .unwrap_or_default();
                    let tools = choice
                        .map(|c| format!(r#""tool_choice":{c},"#))
                        .unwrap_or_default();
                    let fields = if reverse {
                        format!("{thinking}{tools}")
                    } else {
                        format!("{tools}{thinking}")
                    };
                    let body = format!(
                        r#"{{{fields}"model":"synthetic-text","messages":[{{"role":"user","content":"synthetic"}}],"max_tokens":100}}"#
                    );
                    let forced = choice.is_some_and(|c| c != r#""auto""# && c != r#""none""#);
                    let allowed = (proof || mode == Some("disabled"))
                        && (!forced || mode == Some("disabled"));
                    if !allowed {
                        let observed = no_dispatch(&h, body).await;
                        let expected = (400, Some("not_dispatched".into()), true, true);
                        if observed != expected {
                            h.close().await;
                            assert_eq!(
                                observed, expected,
                                "R1 HTTP상태/dispatch/시작행불변/송신불변"
                            );
                            return;
                        }
                        continue;
                    }
                    let wire = if mode == Some("disabled") {
                        nonthinking_native(false)
                    } else {
                        native(false)
                    };
                    set(&h, &wire, false).await;
                    let r = h.request(PATH, body.clone()).send().await.unwrap();
                    assert_eq!(r.status(), 200);
                    assert_eq!(r.bytes().await.unwrap(), wire);
                    count += 1;
                    assert_eq!(state(&h).await.0, count);
                    assert_eq!(state(&h).await.1, 0);
                    assert_eq!(h.provider.calls.lock().await.last().unwrap().body, body);
                }
            }
        }
        if !proof {
            let history = r#"{"model":"synthetic-text","max_tokens":100,"thinking":{"type":"disabled"},"messages":[{"role":"assistant","content":"synthetic","reasoning_content":"synthetic thought"}]}"#;
            let observed = no_dispatch(&h, history.into()).await;
            let expected = (400, Some("not_dispatched".into()), true, true);
            if observed != expected {
                h.close().await;
                assert_eq!(
                    observed, expected,
                    "history Thinking proof는 현재 disabled와 독립"
                );
                return;
            }
        }
        assert_eq!(h.provider.calls.lock().await.len(), count as usize);
        assert_eq!(
            state(&h).await,
            (
                count,
                0,
                format!("0.{:012}", count * 120_000_000),
                count * 100,
                count * 20
            )
        );
        let left:(String,i64,i64)=sqlx::query_as("SELECT reserved_usd::text,(SELECT reserved_tokens FROM gw_rate_windows),(SELECT settled_tokens FROM gw_rate_windows) FROM gw_usage_totals").fetch_one(h.db.store.pool()).await.unwrap();
        assert_eq!(left, ("0.000000000000".into(), 0, count * 120));
        h.close().await;
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn deepseek_review_r2_top_p_http_pg() {
    let h = Harness::with_config(configuration_options(true, "fixed_admission_v1", "")).await;
    strict(&h).await;
    let wire = nonthinking_native(false);
    set(&h, &wire, false).await;
    let body = |value: &str| {
        format!(
            r#" {{"model":"synthetic-text","max_tokens":100,"thinking":{{"type":"disabled"}},"messages":[{{"role":"user","content":"synthetic"}}],"top_p":{value}}} "#
        )
    };
    for value in [
        "-0", "0", "-0.0", "-0.1", "1.00001", r#""0.5""#, "true", "null", "{}", "[]",
    ] {
        let observed = no_dispatch(&h, body(value)).await;
        let expected = (400, Some("not_dispatched".into()), true, true);
        if observed != expected {
            h.close().await;
            assert_eq!(
                observed, expected,
                "R2 HTTP상태/dispatch/시작행불변/송신불변"
            );
            return;
        }
    }
    assert_eq!(state(&h).await.0, 0);
    assert!(h.provider.calls.lock().await.is_empty());
    for (i, value) in ["0.000001", "1", "1.0", "1e-3"].iter().enumerate() {
        let b = body(value);
        let r = h.request(PATH, b.clone()).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.bytes().await.unwrap(), wire);
        assert_eq!(state(&h).await.0, (i + 1) as i64);
        assert_eq!(h.provider.calls.lock().await.last().unwrap().body, b);
    }
    assert_eq!(state(&h).await, (4, 0, "0.000480000000".into(), 400, 80));
    let left:(String,i64,i64)=sqlx::query_as("SELECT reserved_usd::text,(SELECT reserved_tokens FROM gw_rate_windows),(SELECT settled_tokens FROM gw_rate_windows) FROM gw_usage_totals").fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(left, ("0.000000000000".into(), 0, 480));
    h.close().await;
}

fn tool_call() -> serde_json::Value {
    serde_json::json!({"id":"call_ds1","type":"function","function":{"name":"lookup","arguments":"{\"city\":\"Seoul\"}"}})
}
fn feature_body(stream: bool, tool: bool, thinking: bool) -> String {
    let mut v: serde_json::Value = serde_json::from_str(BODY).unwrap();
    v["stream"] = stream.into();
    v["thinking"] = serde_json::json!({"type":if thinking {"enabled"} else {"disabled"}});
    if tool {
        v["tools"] = serde_json::json!([{"type":"function","function":{"name":"lookup","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}]);
        v["tool_choice"] = if thinking {
            serde_json::json!("auto")
        } else {
            serde_json::json!({"type":"function","function":{"name":"lookup"}})
        };
    } else {
        v["messages"][0]["content"] = "Return a JSON object".into();
        v["response_format"] = serde_json::json!({"type":"json_object"});
    }
    format!(" {v}\n")
}
fn feature_native(stream: bool, tool: bool, thinking: bool) -> String {
    let mut u = usage();
    if !thinking {
        u.as_object_mut()
            .unwrap()
            .remove("completion_tokens_details");
    }
    let finish = if tool { "tool_calls" } else { "stop" };
    let mut message = if tool {
        serde_json::json!({"role":"assistant","content":null,"tool_calls":[tool_call()]})
    } else {
        serde_json::json!({"role":"assistant","content":"{\"ok\":true}"})
    };
    if thinking {
        message["reasoning_content"] = "synthetic thought".into();
    }
    if !stream {
        return serde_json::json!({"id":"ds_feature","object":"chat.completion","created":1900000000,"model":"synthetic-text","choices":[{"index":0,"finish_reason":finish,"message":message}],"usage":u}).to_string();
    }
    let (mut first, second) = if tool {
        (
            serde_json::json!({"role":"assistant","content":null,"tool_calls":[{"index":0,"id":"call_ds1","type":"function","function":{"name":"look","arguments":"{\"city\":"}}]}),
            serde_json::json!({"tool_calls":[{"index":0,"function":{"name":"up","arguments":"\"Seoul\"}"}}]}),
        )
    } else {
        (
            serde_json::json!({"role":"assistant","content":"{\"ok\":"}),
            serde_json::json!({"content":"true}"}),
        )
    };
    if thinking {
        first["reasoning_content"] = "synthetic thought".into();
    }
    let mut wire = String::new();
    for (delta, reason, last) in [
        (first, serde_json::Value::Null, false),
        (second, serde_json::Value::Null, false),
        (serde_json::json!({}), serde_json::json!(finish), true),
    ] {
        let mut chunk = serde_json::json!({"id":"ds_feature","object":"chat.completion.chunk","created":1900000000,"model":"synthetic-text","choices":[{"index":0,"finish_reason":reason,"delta":delta}]});
        if last {
            chunk["usage"] = u.clone();
        } else {
            chunk["usage"] = serde_json::Value::Null;
        }
        wire.push_str(&format!("data: {chunk}\r\n\r\n"));
    }
    wire.push_str("data: [DONE]\n\n");
    wire
}
async fn assert_held_feature(h: &Harness) {
    let held:(String,i64)=sqlx::query_as("SELECT reserved_usd::text,(SELECT reserved_tokens FROM gw_rate_windows) FROM gw_usage_totals").fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(held, ("0.004296000000".into(), 4196));
}
async fn assert_settled_features(h: &Harness, count: i64) {
    assert_eq!(
        state(h).await,
        (
            count,
            0,
            format!("0.{:012}", count * 120_000_000),
            count * 100,
            count * 20
        )
    );
    // 한 scope/lane도 정상 호출 사이에 분창이 바뀔 수 있다. 원래 budget 창 집합을 중복 없이 검사한다.
    let windows: Vec<(i64, i64, i64)> = sqlx::query_as("WITH expected AS (SELECT scope_id,rate_lane,rate_start,sum(applied_input_tokens+applied_output_tokens)::bigint AS tokens FROM gw_request_budgets GROUP BY scope_id,rate_lane,rate_start) SELECT e.tokens,w.settled_tokens,w.reserved_tokens FROM expected e JOIN gw_rate_windows w ON w.scope_id=e.scope_id AND w.lane=e.rate_lane AND w.window_start=e.rate_start").fetch_all(h.db.store.pool()).await.unwrap();
    let counts:(i64,i64)=sqlx::query_as("SELECT count(*)::bigint,(SELECT count(*) FROM (SELECT DISTINCT scope_id,rate_lane,rate_start FROM gw_request_budgets) b)::bigint FROM gw_rate_windows").fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(counts, (windows.len() as i64, windows.len() as i64));
    assert!(!windows.is_empty());
    let mut settled = 0;
    for (expected, actual, reserved) in windows {
        assert_eq!(actual, expected);
        assert_eq!(reserved, 0);
        settled += actual;
    }
    assert_eq!(settled, count * 120);
    let budget:(i64,i64,i64)=sqlx::query_as("SELECT count(*)::bigint,count(*) FILTER(WHERE reservation_usd=0.004296 AND reserved_tokens=4196 AND remaining_reservation_usd=0 AND remaining_reserved_tokens=0 AND applied_cost_usd=0.00012 AND state='settled')::bigint,(SELECT count(*) FROM gw_usage_events WHERE kind='final') FROM gw_request_budgets").fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(budget, (count, count, count));
    let reserved: String = sqlx::query_scalar("SELECT reserved_usd::text FROM gw_usage_totals")
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    assert_eq!(reserved, "0.000000000000");
    let calls = h.provider.calls.lock().await;
    assert_eq!(calls.len(), count as usize);
    for call in calls.iter() {
        assert_eq!(call.path, "/chat/completions");
        assert_eq!(
            call.headers["authorization"],
            "Bearer synthetic-deepseek-company-key"
        );
        for name in ["cookie", "x-api-key", "openai-project", "anthropic-version"] {
            assert!(!call.headers.contains_key(name));
        }
    }
    // 전체 JSON 열/장부 projection에 생성된 인수·대화 내용이 들어가지 않는다.
    let stored:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM gw_usage_events WHERE provider_usage::text LIKE '%Seoul%' OR normalized_usage::text LIKE '%Seoul%' OR pricing_resolution::text LIKE '%Seoul%')").fetch_one(h.db.store.pool()).await.unwrap();
    assert!(!stored);
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn deepseek_review_r3_tool_json_outputs_wire_pg_and_followup() {
    for stream in [false, true] {
        for (tool, thinking) in [(true, false), (true, true), (false, false), (false, true)] {
            let h =
                Harness::with_config(configuration_options(true, "fixed_admission_v1", "")).await;
            strict(&h).await;
            let wire = feature_native(stream, tool, thinking);
            set(&h, &wire, stream).await;
            let (started, release) = h.provider.hold_response().await;
            let body = feature_body(stream, tool, thinking);
            let pending = h.request(PATH, body.clone()).send();
            tokio::pin!(pending);
            tokio::select! {r=&mut pending=>panic!("feature dispatch 전 응답 {}",r.is_ok()),_=started=>{}}
            assert_held_feature(&h).await;
            release.send(()).unwrap();
            let r = pending.await.unwrap();
            assert_eq!(r.status(), 200);
            assert_eq!(r.bytes().await.unwrap(), wire);
            assert_settled_features(&h, 1).await;
            assert_eq!(h.provider.calls.lock().await[0].body, body);
            if !stream {
                let v: serde_json::Value = serde_json::from_str(&wire).unwrap();
                if tool {
                    assert_eq!(v["choices"][0]["message"]["tool_calls"][0], tool_call());
                    assert_eq!(v["choices"][0]["finish_reason"], "tool_calls");
                } else {
                    assert_eq!(
                        serde_json::from_str::<serde_json::Value>(
                            v["choices"][0]["message"]["content"].as_str().unwrap()
                        )
                        .unwrap(),
                        serde_json::json!({"ok":true})
                    );
                }
            }
            let mut follow: serde_json::Value = serde_json::from_str(BODY).unwrap();
            follow["thinking"] =
                serde_json::json!({"type":if thinking {"enabled"} else {"disabled"}});
            if tool {
                let mut assistant = serde_json::json!({"role":"assistant","content":null,"tool_calls":[tool_call()]});
                if thinking {
                    assistant["reasoning_content"] = "synthetic thought".into();
                }
                follow["messages"].as_array_mut().unwrap().extend([assistant,serde_json::json!({"role":"tool","content":"synthetic result","tool_call_id":"call_ds1"})]);
            }
            let next = if thinking {
                native(false)
            } else {
                nonthinking_native(false)
            };
            set(&h, &next, false).await;
            let raw = format!(" {follow}\n");
            let r = h.request(PATH, raw.clone()).send().await.unwrap();
            assert_eq!(r.status(), 200);
            assert_eq!(r.bytes().await.unwrap(), next);
            assert_settled_features(&h, 2).await;
            assert_eq!(h.provider.calls.lock().await[1].body, raw);
            h.close().await;
        }
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL and fixed SDK"]
async fn deepseek_review_r3_fixed_sdk_tool_arguments_json_and_tool_result_followup() {
    for (kind, stream, tool, thinking) in [
        ("tool_plain_disabled", false, true, false),
        ("tool_stream_disabled", true, true, false),
        ("tool_plain_enabled", false, true, true),
        ("tool_stream_enabled", true, true, true),
        ("json_plain", false, false, false),
        ("json_stream", true, false, false),
    ] {
        let h = Harness::with_config(configuration_options(true, "fixed_admission_v1", "")).await;
        strict(&h).await;
        // 첫 응답만 channel로 보내고 두 번째 tool-result 회차는 정상 후속 응답을 쓴다.
        let follow = if thinking {
            native(false)
        } else {
            nonthinking_native(false)
        };
        set(&h, &follow, false).await;
        let (tx, stopped) = if stream {
            h.provider.stream().await
        } else {
            h.provider.stream_json().await
        };
        let (started, release) = h.provider.hold_response().await;
        {
            let token = h.token.clone();
            let pending = sdk(&h, kind, 200, "completed", &token);
            tokio::pin!(pending);
            tokio::select! { _=&mut pending=>panic!("SDK feature dispatch 전 종료"),_=started=>{} }
            assert_held_feature(&h).await;
            release.send(()).unwrap();
            let wire = feature_native(stream, tool, thinking);
            let send = async {
                for part in wire.as_bytes().chunks(37) {
                    tx.send(Bytes::copy_from_slice(part)).await.unwrap();
                }
                drop(tx);
            };
            tokio::join!(pending, send);
            stopped.await.unwrap();
        }
        assert_settled_features(&h, if tool { 2 } else { 1 }).await;
        let calls = h.provider.calls.lock().await;
        let first: serde_json::Value = serde_json::from_slice(&calls[0].body).unwrap();
        assert_eq!(
            first["thinking"]["type"],
            if thinking { "enabled" } else { "disabled" }
        );
        if tool {
            assert_eq!(first["tools"][0]["function"]["name"], "lookup");
            assert_eq!(
                first["tool_choice"],
                if thinking {
                    serde_json::json!("auto")
                } else {
                    serde_json::json!({"type":"function","function":{"name":"lookup"}})
                }
            );
            let next: serde_json::Value = serde_json::from_slice(&calls[1].body).unwrap();
            assert_eq!(next["messages"][1]["tool_calls"][0], tool_call());
            assert_eq!(
                next["messages"][2],
                serde_json::json!({"role":"tool","content":"{\"weather\":\"clear\"}","tool_call_id":"call_ds1"})
            );
        } else {
            assert_eq!(first["response_format"]["type"], "json_object");
        }
        drop(calls);
        h.close().await;
    }
}

#[path = "support/compat.rs"]
mod error_compat;
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL and fixed SDK"]
async fn s9_sdk_error_matrix_measures_company_auth_and_zero_refresh_retry() {
    for (input, status, code, dispatched) in [
        ("invalid", 401, "invalid_credentials", false),
        ("denied", 403, "access_denied", false),
        ("auth401", 502, "upstream_auth_error", true),
        ("auth403", 502, "upstream_auth_error", true),
        ("rate", 429, "", true),
        ("unavailable", 503, "", true),
    ] {
        let h = Harness::new().await;
        if input == "denied" {
            sqlx::query("UPDATE gw_principals SET disabled_at=clock_timestamp(),disabled_reason='synthetic-denial' WHERE id=$1").bind(h.principal).execute(h.db.store.pool()).await.unwrap();
        }
        let upstream_status = match input {
            "auth401" => 401,
            "auth403" => 403,
            "rate" => 429,
            _ => 503,
        };
        h.provider.set(StatusCode::from_u16(upstream_status).unwrap(),HeaderMap::from_iter([("content-type".parse().unwrap(),"application/json".parse().unwrap()),("retry-after".parse().unwrap(),"1".parse().unwrap())]),Bytes::from_static(br#"{"error":{"type":"server_error","message":"synthetic error","param":null,"code":"synthetic_error"}}"#)).await;
        error_compat::error_sdk(
            &h.base,
            if input == "invalid" {
                "synthetic-invalid"
            } else {
                &h.token
            },
            "deepseek",
            status,
            code,
            dispatched,
        )
        .await;
        h.runtime.wait_requests().await;
        assert_eq!(h.provider.calls.lock().await.len(), usize::from(dispatched));
        error_compat::assert_error_ledger(h.db.store.pool(), dispatched).await;
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL, fixed SDK and actual 120 second upstream idle"]
async fn s9_upstream_idle_timeout_is_504_before_headers_and_incomplete_after_headers() {
    let cfg = || {
        configuration_options(
            true,
            "fixed_admission_v1",
            "\nlimits:\n  transport_timeout_ms: 130000\n  request_timeout_ms: 135000\n",
        )
    };
    let h0 = Harness::with_config(cfg()).await;
    let h1 = Harness::with_config(cfg()).await;
    strict(&h0).await;
    strict(&h1).await;
    let (started, release) = h0.provider.hold_response().await;
    let (stream, stopped) = h1.provider.stream().await;
    let first = chat_start();
    stream.send(Bytes::from(first.clone())).await.unwrap();
    let began = tokio::time::Instant::now();
    let before = async {
        let sdk = error_compat::error_sdk(
            &h0.base,
            &h0.token,
            "deepseek",
            504,
            "upstream_read_timeout",
            true,
        );
        let observe = async {
            started.await.unwrap();
        };
        tokio::join!(sdk, observe);
        let _ = release.send(());
        h0.runtime.wait_requests().await;
        let row: (String, String, bool) = sqlx::query_as(
            "SELECT reason_code,outcome,execution_stopped_at IS NOT NULL FROM gw_requests",
        )
        .fetch_one(h0.db.store.pool())
        .await
        .unwrap();
        assert_eq!(row, ("upstream_read_timeout".into(), "failed".into(), true));
    };
    let after = async {
        let r = h1.request(PATH, raw(true, None)).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["x-should-retry"], "false");
        assert!(!r.headers().contains_key("x-gateway-dispatch-state"));
        assert!(!r.headers().contains_key("x-gateway-error-code"));
        let mut bytes = Vec::new();
        let mut r = r;
        loop {
            match r.chunk().await {
                Ok(Some(chunk)) => bytes.extend_from_slice(&chunk),
                Ok(None) => panic!("실제 I/O timeout H1이 성공 EOF가 됐다"),
                Err(_) => break,
            }
        }
        assert_eq!(
            bytes,
            first.as_bytes(),
            "H1 status/순서/성공terminal을 합성하면 안 됨"
        );
        stopped.await.unwrap();
        drop(stream);
        h1.runtime.wait_requests().await;
        let row: (String, String, bool) = sqlx::query_as(
            "SELECT reason_code,outcome,execution_stopped_at IS NOT NULL FROM gw_requests",
        )
        .fetch_one(h1.db.store.pool())
        .await
        .unwrap();
        assert_eq!(row, ("upstream_read_timeout".into(), "failed".into(), true));
    };
    tokio::time::timeout(std::time::Duration::from_secs(128), async {
        tokio::join!(before, after);
    })
    .await
    .unwrap();
    assert!(began.elapsed() >= std::time::Duration::from_secs(119));
    for h in [&h0, &h1] {
        assert_eq!(h.provider.calls.lock().await.len(), 1);
        let held:(String,String,String,i64,i64)=sqlx::query_as("SELECT r.phase,b.reservation_usd::text,b.remaining_reservation_usd::text,b.reserved_tokens,b.remaining_reserved_tokens FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id").fetch_one(h.db.store.pool()).await.unwrap();
        assert_eq!(
            held,
            (
                "finished".into(),
                "0.004296000000".into(),
                "0.004296000000".into(),
                4196,
                4196
            )
        );
        let state:(i64,i64)=sqlx::query_as("SELECT count(*)::bigint,count(*) FILTER(WHERE accounting_state='unknown' AND cost_usd IS NULL)::bigint FROM gw_requests").fetch_one(h.db.store.pool()).await.unwrap();
        assert_eq!(state, (1, 1));
        let resources = h.runtime.request_resources();
        assert_eq!(
            (
                resources.body_bytes,
                resources.queue_bytes,
                resources.queue_waiters
            ),
            (0, 0, 0)
        );
        assert_eq!(h.runtime.available_inference(), 16);
    }
    eprintln!(
        "S9_UPSTREAM_IDLE_OK elapsed_ms={} h0_status=504 h1_status=200 provider_calls=2 unknown=2",
        began.elapsed().as_millis()
    );
    h0.close().await;
    h1.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL and fixed SDK"]
async fn s9_stream_syntax_and_limit_have_distinct_h0_h1_and_sdk_causes() {
    for (bad, code) in [
        ("data: {bad}\n\n".to_string(), "stream_parse_error"),
        (format!("event: {}\n", "x".repeat(129)), "stream_limit"),
    ] {
        for mode in ["h0", "h1", "sdk"] {
            let h =
                Harness::with_config(configuration_options(true, "fixed_admission_v1", "")).await;
            strict(&h).await;
            let body = raw(true, None);
            let prefix = chat_start();
            let (tx, stopped) = h.provider.stream().await;
            if mode == "h1" {
                tx.send(Bytes::from(prefix.clone())).await.unwrap();
            } else {
                tx.send(Bytes::from(bad.clone())).await.unwrap();
            }
            if mode == "sdk" {
                error_compat::error_stream_sdk(&h.base, &h.token, "deepseek", 502, code, true)
                    .await;
            } else {
                let mut r = h.request(PATH, body).send().await.unwrap();
                assert_eq!(r.headers()["x-should-retry"], "false");
                assert!(!r.headers().contains_key("x-gateway-dispatch-state"));
                if mode == "h0" {
                    assert_eq!(r.status(), 502);
                    assert_eq!(r.headers()["x-gateway-error-code"], code);
                    let v: serde_json::Value =
                        serde_json::from_slice(&r.bytes().await.unwrap()).unwrap();
                    assert_eq!(v["error"]["type"], "server_error");
                } else {
                    assert_eq!(r.status(), 200);
                    assert!(!r.headers().contains_key("x-gateway-error-code"));
                    let received = r.chunk().await.unwrap().unwrap();
                    assert_eq!(received, prefix.as_bytes());
                    tx.send(Bytes::from(bad.clone())).await.unwrap();
                    let mut tail = Vec::new();
                    loop {
                        match r.chunk().await {
                            Ok(Some(chunk)) => {
                                tail.extend_from_slice(&chunk);
                            }
                            Ok(None) => panic!("H1 성공 EOF는 불완전 전송 종료가 아니다"),
                            Err(_) => break,
                        }
                    }
                    assert!(tail.is_empty(), "성공/error terminal 합성없음");
                }
            }
            stopped.await.unwrap();
            drop(tx);
            h.runtime.wait_requests().await;
            let row: (String, String, String, bool) = sqlx::query_as(
                "SELECT phase,reason_code,accounting_state,cost_usd IS NULL FROM gw_requests",
            )
            .fetch_one(h.db.store.pool())
            .await
            .unwrap();
            assert_eq!(
                row,
                ("finished".into(), code.into(), "unknown".into(), true)
            );
            let held:(String,String,i64,i64)=sqlx::query_as("SELECT reservation_usd::text,remaining_reservation_usd::text,reserved_tokens,remaining_reserved_tokens FROM gw_request_budgets").fetch_one(h.db.store.pool()).await.unwrap();
            assert_eq!(
                held,
                ("0.004296000000".into(), "0.004296000000".into(), 4196, 4196)
            );
            assert_eq!(h.provider.calls.lock().await.len(), 1);
            assert_eq!(h.runtime.available_inference(), 16);
            assert_eq!(h.runtime.request_resources().body_bytes, 0);
            h.close().await;
        }
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s9_verified_terminal_before_stream_limit_settles_once_and_releases_reservation() {
    let h = Harness::with_config(configuration_options(true, "fixed_admission_v1", "")).await;
    strict(&h).await;

    let body = raw(true, None);
    let native = native(true);
    let (tx, stopped) = h.provider.stream().await;
    tx.send(Bytes::from(native.clone())).await.unwrap();
    let mut r = h.request(PATH, body.clone()).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let id = r.headers()["x-gateway-request-id"]
        .to_str()
        .unwrap()
        .parse::<uuid::Uuid>()
        .unwrap();
    let mut received = Vec::new();
    while received.len() < native.len() {
        received.extend_from_slice(&r.chunk().await.unwrap().unwrap());
    }
    assert_eq!(received, native.as_bytes());
    tx.send(Bytes::from(format!("event: {}\n", "x".repeat(129))))
        .await
        .unwrap();
    let mut tail = Vec::new();
    loop {
        match r.chunk().await {
            Ok(Some(chunk)) => {
                tail.extend_from_slice(&chunk);
            }
            Ok(None) => panic!("H1 성공 EOF는 불완전 전송 종료가 아니다"),
            Err(_) => break,
        }
    }
    assert!(tail.is_empty());
    stopped.await.unwrap();
    drop(tx);
    h.runtime.wait_requests().await;
    let row: (String, String, String, Option<String>) = sqlx::query_as(
        "SELECT phase,reason_code,accounting_state,cost_usd::text FROM gw_requests WHERE id=$1",
    )
    .bind(id)
    .fetch_one(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            "finished".into(),
            "stream_limit".into(),
            "settled".into(),
            Some("0.000120000000".into())
        )
    );
    let budget:(String,i64,i64,String,i64)=sqlx::query_as("SELECT b.remaining_reservation_usd::text,b.remaining_reserved_tokens,t.unknown_count,t.cost_usd::text,(SELECT count(*) FROM gw_usage_events WHERE request_id=$1 AND kind='final') FROM gw_request_budgets b JOIN gw_usage_totals t ON t.scope_id=b.scope_id AND t.period_start=b.period_start WHERE b.request_id=$1").bind(id).fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(
        budget,
        ("0.000000000000".into(), 0, 0, "0.000120000000".into(), 1)
    );
    let final_state:(String,String,i64)=sqlx::query_as("SELECT r.outcome,e.normalized_usage->>'source',w.settled_tokens FROM gw_requests r JOIN gw_usage_events e ON e.request_id=r.id AND e.kind='final' JOIN gw_request_budgets b ON b.request_id=r.id JOIN gw_rate_windows w ON w.scope_id=b.scope_id AND w.lane=b.rate_lane AND w.window_start=b.rate_start WHERE r.id=$1").bind(id).fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(final_state, ("failed".into(), "provider".into(), 120));
    h.provider
        .set(
            StatusCode::OK,
            HeaderMap::from_iter([(
                "content-type".parse().unwrap(),
                "text/event-stream".parse().unwrap(),
            )]),
            Bytes::from(native.clone()),
        )
        .await;
    let next = h.request(PATH, body).send().await.unwrap();
    assert_eq!(next.status(), 200);
    assert_eq!(next.bytes().await.unwrap(), native.as_bytes());
    h.runtime.wait_requests().await;
    assert_eq!(h.provider.calls.lock().await.len(), 2);
    assert_eq!(h.runtime.available_inference(), 16);
    assert_eq!(h.runtime.request_resources().body_bytes, 0);
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s9_stream_limit_never_promotes_unpriced_complete_usage() {
    let h = Harness::with_config(configuration_options(
        true,
        "deepseek_time_unresolved_v1",
        "",
    ))
    .await;
    sqlx::query("UPDATE gw_policies SET tpm_limit=100000")
        .execute(h.db.store.pool())
        .await
        .unwrap();

    let body = raw(true, None);
    let native = native(true);
    let (tx, stopped) = h.provider.stream().await;
    tx.send(Bytes::from(native.clone())).await.unwrap();
    let mut r = h.request(PATH, body.clone()).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let mut received = Vec::new();
    while received.len() < native.len() {
        received.extend_from_slice(&r.chunk().await.unwrap().unwrap());
    }
    assert_eq!(received, native.as_bytes());
    tx.send(Bytes::from(format!("event: {}\n", "x".repeat(129))))
        .await
        .unwrap();
    loop {
        match r.chunk().await {
            Ok(Some(chunk)) => {
                assert!(chunk.is_empty());
            }
            Ok(None) => panic!("H1 성공 EOF는 불완전 전송 종료가 아니다"),
            Err(_) => break,
        }
    }
    stopped.await.unwrap();
    drop(tx);
    h.runtime.wait_requests().await;
    let row:(String,String,bool,String,i64,i64)=sqlx::query_as("SELECT r.reason_code,r.accounting_state,r.cost_usd IS NULL,b.remaining_reservation_usd::text,b.remaining_reserved_tokens,t.unknown_count FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id JOIN gw_usage_totals t ON t.scope_id=b.scope_id AND t.period_start=b.period_start").fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(
        row,
        (
            "stream_limit".into(),
            "unknown".into(),
            true,
            "0.000000000000".into(),
            4196,
            1
        )
    );
    let blocked = h.request(PATH, body).send().await.unwrap();
    assert_eq!(blocked.status(), 403);
    assert_eq!(
        blocked.headers()["x-gateway-error-code"],
        "unsettled_usage_block"
    );
    drop(blocked);
    h.runtime.wait_requests().await;
    assert_eq!(h.provider.calls.lock().await.len(), 1);
    assert_eq!(h.runtime.available_inference(), 16);
    h.close().await;
}

#[path = "support/s9_finality.rs"]
mod s9_finality;
async fn s9_finality_fixture(
    _incomplete: bool,
) -> (Harness, &'static str, String, String, String, &'static str) {
    let h = Harness::with_config(configuration_options(true, "fixed_admission_v1", "")).await;
    strict(&h).await;
    (
        h,
        PATH,
        raw(true, None),
        native(true),
        native(true).split("\r\n\r\n").next().unwrap().to_owned() + "\r\n\r\n",
        "0.000120000000",
    )
}

fn s9_native_error_tail(length: usize) -> String {
    error_wire(true, length, "\n")
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s9_deepseek_feature_accounting_checks_each_original_rate_window() {
    let h = Harness::with_clock(configuration_options(true, "fixed_admission_v1", "")).await;
    strict(&h).await;
    sqlx::query("UPDATE gw_fixture_clock SET observed='2025-03-15T12:00:00Z'")
        .execute(h.database.pool())
        .await
        .unwrap();
    for step in [0, 1] {
        if step == 1 {
            sqlx::query("UPDATE gw_fixture_clock SET observed=observed+interval '60 seconds'")
                .execute(h.database.pool())
                .await
                .unwrap();
        }
        set(&h, &native(false), false).await;
        let r = h.request(PATH, raw(false, None)).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let _ = r.bytes().await.unwrap();
        h.runtime.wait_requests().await;
    }
    let windows:(i64,i64,i64,i64)=sqlx::query_as("SELECT count(*)::bigint,count(DISTINCT scope_id)::bigint,count(DISTINCT lane)::bigint,count(DISTINCT window_start)::bigint FROM gw_rate_windows").fetch_one(h.database.pool()).await.unwrap();
    assert_eq!(windows, (2, 1, 1, 2));
    // 원래 helper의 단일행 가정을 두 정상 분창으로 직접 반증한다.
    use futures_util::FutureExt;
    let result = std::panic::AssertUnwindSafe(assert_settled_features(&h, 2))
        .catch_unwind()
        .await;
    h.close().await;
    assert!(
        result.is_ok(),
        "합법적인 두 원래 rate 창의 feature 정산 검사가 실패했다"
    );
}
