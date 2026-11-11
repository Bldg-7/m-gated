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
    configuration_with(false, "max_completion_tokens")
}
fn configuration_with(verified: bool, output: &str) -> config::ValidatedConfig {
    configuration_options(verified, output, "")
}
fn configuration_options(verified: bool, output: &str, options: &str) -> config::ValidatedConfig {
    let base = include_str!("support/config.yaml")
        .replace("provider: anthropic", "provider: openai")
        .replace("https://api.anthropic.com", "https://api.openai.com");
    let mut yaml=base.replace("api: messages", &format!("api: openai_chat\n        chat_output_limit_field: {output}\n        features: [client_tools, thinking, cache, json_schema, inline_png]"))
        .replace("          input: '1.000000000000'", "          input: '1.000000000000'\n          cache_read: '0.500000000000'");
    if output == "max_tokens" {
        yaml = yaml.replace(
            "features: [client_tools, thinking, cache, json_schema, inline_png]",
            "features: [client_tools, cache, json_schema, inline_png]",
        );
    }
    if verified {
        yaml=yaml.replace("        max_input_tokens: 4096", "        input_bound_contract:\n          kind: openai_chat_context_v1\n          context_tokens: 4096\n          input_categories: [messages]\n        max_input_tokens: 4096");
    }
    let profile=base[base.find("      - model:").unwrap()..].replace("api: messages", "api: embeddings\n        embedding_dimensions: {default: 3, max: 4096}\n        embedding_encodings: [float, base64]").replace("        max_output_tokens: 1024\n", "").replace("          output: '2.000000000000'\n", "");
    yaml.push_str(&profile);
    yaml.push_str(options);
    let mut env = support::environment();
    env.insert("OPENAI_API_KEY".into(), "synthetic-company-key".into());
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
        use m_gated::{
            auth::{SsoAuthenticator, SsoVerifier, jwks::JwksCache},
            bootstrap::{self, Dependencies},
            gateway::InferenceGateway,
            providers::anthropic::AnthropicClient,
            runtime::Dependency,
        };
        let db = postgres::TestDb::new().await;
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
        let current = db
            .store
            .current_human(&identity, &AccessSettings::from_config(&cfg))
            .await
            .unwrap();
        let principal = current.id;
        sqlx::query("UPDATE gw_policies SET allow_rules=$2 WHERE id=$1").bind(current.policy_id).bind(serde_json::json!([{"provider":"openai","model":"synthetic-text","capabilities":["chat_completions","embeddings"]}])).execute(db.store.pool()).await.unwrap();
        let auth = Arc::new(SsoAuthenticator::new(&cfg, verifier, db.store.clone()));
        let ledger = Arc::new(
            PgLedger::new(
                db.store.clone(),
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
        let client = AnthropicClient::openai_loopback_for_test(
            provider_server.base.clone(),
            "synthetic-company-key".into(),
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
            db.store.clone(),
        ));
        let app = bootstrap::build(
            cfg,
            Dependencies::unassembled()
                .with_ui(Arc::new(
                    m_gated::dashboard::UiBundle::load(
                        std::path::Path::new("web/dist"),
                        "https://auth.example.test",
                    )
                    .expect("검증된 실제 Vite bundle 필요; scripts/test-s7.sh 선행 build"),
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
        self.db.close().await;
    }
}
const CHAT: &str = r#" {"model":"synthetic-text","messages":[{"role":"user","content":"synthetic request"}],"max_completion_tokens":100} "#;
const EMBEDDINGS: &str = r#" {"model":"synthetic-text","input":["synthetic request"],"dimensions":3,"encoding_format":"float"} "#;
fn chat_native(stream: bool) -> String {
    let usage = r#"{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_tokens_details":{"cached_tokens":40,"audio_tokens":0},"completion_tokens_details":{"reasoning_tokens":10,"audio_tokens":0,"accepted_prediction_tokens":0,"rejected_prediction_tokens":0}}"#;
    if stream {
        format!(
            "data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"synthetic-text\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"synthetic\"}},\"finish_reason\":null}}],\"usage\":null}}\r\n\r\ndata: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"synthetic-text\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":null}}\n\ndata: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"synthetic-text\",\"choices\":[],\"usage\":{usage}}}\n\ndata: [DONE]\n\n"
        )
    } else {
        format!(
            r#"{{"id":"c1","object":"chat.completion","model":"synthetic-text","choices":[{{"index":0,"message":{{"role":"assistant","content":"synthetic"}},"finish_reason":"stop"}}],"usage":{usage}}}"#
        )
    }
}
fn embeddings_native(count: usize, dim: usize, encoding: &str) -> String {
    use base64::Engine;
    let vector = if encoding == "float" {
        serde_json::json!(vec![0.5; dim])
    } else {
        serde_json::json!(
            base64::engine::general_purpose::STANDARD.encode(
                vec![0.5_f32; dim]
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect::<Vec<_>>()
            )
        )
    };
    serde_json::json!({"object":"list","model":"synthetic-text","data":(0..count).map(|i|serde_json::json!({"object":"embedding","index":i,"embedding":vector})).collect::<Vec<_>>(),"usage":{"prompt_tokens":100,"total_tokens":100}}).to_string()
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
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_native_stream_nonstream_usage_only_done_and_price_once() {
    for stream in [false, true] {
        let h = Harness::new().await;
        let native = chat_native(stream);
        set(&h, &native, stream).await;
        let body = if stream {
            CHAT.replace(
                "100}",
                "100,\"stream\":true,\"stream_options\":{\"include_usage\":true}} ",
            )
        } else {
            CHAT.into()
        };
        let r = h
            .request("/openai/v1/chat/completions", body.clone())
            .header("openai-project", "client-project")
            .header("cookie", "synthetic-cookie")
            .header("idempotency-key", "synthetic-idempotency")
            .send()
            .await
            .unwrap();
        let status = r.status();
        let bytes = r.bytes().await;
        if status != StatusCode::OK {
            h.close().await;
            assert_eq!(status, StatusCode::OK, "전용 Chat route");
            return;
        }
        assert_eq!(bytes.unwrap(), native);
        assert_eq!(state(&h).await, (1, 0, "0.000120000000".into(), 100, 20));
        let row = sqlx::query(
            "SELECT provider_usage,normalized_usage FROM gw_usage_events WHERE kind='final'",
        )
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
        let u: serde_json::Value = row.get("provider_usage");
        assert_eq!(u["prompt_tokens"], 100);
        assert_eq!(u["completion_tokens_details"]["reasoning_tokens"], 10);
        let calls = h.provider.calls.lock().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].path, "/v1/chat/completions");
        assert_eq!(calls[0].body, body);
        assert_eq!(
            calls[0].headers["authorization"],
            "Bearer synthetic-company-key"
        );
        for k in [
            "x-api-key",
            "cookie",
            "openai-project",
            "idempotency-key",
            "anthropic-version",
        ] {
            assert!(!calls[0].headers.contains_key(k));
        }
        drop(calls);
        h.close().await;
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn embeddings_native_strings_vectors_dimensions_and_price_once() {
    let h = Harness::new().await;
    for (count, dim, encoding) in [
        (1, 3, "float"),
        (256, 3, "float"),
        (1, 4096, "float"),
        (256, 4096, "base64"),
    ] {
        let native = embeddings_native(count, dim, encoding);
        set(&h, &native, false).await;
        let raw=serde_json::json!({"model":"synthetic-text","input":vec!["synthetic";count],"dimensions":dim,"encoding_format":encoding}).to_string();
        let r = h
            .request("/openai/v1/embeddings", raw.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.bytes().await.unwrap(), native);
        let calls = h.provider.calls.lock().await;
        let call = calls.last().unwrap();
        assert_eq!(call.path, "/v1/embeddings");
        assert_eq!(call.body, raw);
        assert_eq!(
            call.headers["authorization"],
            "Bearer synthetic-company-key"
        );
        drop(calls);
    }
    assert_eq!(state(&h).await, (4, 0, "0.000400000000".into(), 400, 0));
    h.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_and_embeddings_invalid_inputs_never_dispatch_or_record() {
    let h = Harness::new().await;
    let mut bad = Vec::new();
    for extra in [
        r#", "n":2"#,
        r#", "store":true"#,
        r#", "modalities":["text"]"#,
        r#", "audio":{}"#,
        r#", "max_tokens":100"#,
        r#", "stream":true"#,
        r#", "stream":true,"stream_options":{"include_usage":false}"#,
        r#", "stream_options":{"include_usage":true}"#,
        r#", "service_tier":"auto"#,
        r#", "prompt_cache_key":"x"#,
    ] {
        bad.push((
            "/openai/v1/chat/completions",
            CHAT.trim().trim_end_matches('}').to_owned() + extra + "}",
        ));
    }
    for input in [
        serde_json::json!(""),
        serde_json::json!([]),
        serde_json::json!([""]),
        serde_json::json!(["x", 1]),
        serde_json::json!([1, 2]),
        serde_json::json!([[1, 2]]),
        serde_json::json!(vec!["x"; 257]),
    ] {
        bad.push((
            "/openai/v1/embeddings",
            serde_json::json!({"model":"synthetic-text","input":input}).to_string(),
        ));
    }
    for extra in [
        r#", "stream":false"#,
        r#", "dimensions":0"#,
        r#", "dimensions":4097"#,
        r#", "encoding_format":"int8"#,
    ] {
        bad.push((
            "/openai/v1/embeddings",
            r#"{"model":"synthetic-text","input":"x""#.to_owned() + extra + "}",
        ));
    }
    for (path, body) in bad {
        let r = h.request(path, body).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert_eq!(r.headers()["x-gateway-dispatch-state"], "not_dispatched");
        let v: serde_json::Value = serde_json::from_slice(&r.bytes().await.unwrap()).unwrap();
        assert_eq!(v["error"]["type"], "invalid_request_error");
    }
    assert_eq!(state(&h).await, (0, 0, "0".into(), 0, 0));
    assert!(h.provider.calls.lock().await.is_empty());
    h.close().await;
}

async fn unknown(h: &Harness) {
    let s = state(h).await;
    assert_eq!(s.0, 1);
    assert_eq!(s.1, 1);
    let r = h
        .request("/openai/v1/chat/completions", CHAT)
        .send()
        .await
        .unwrap();
    if r.status() != 403 {
        let code = match r
            .headers()
            .get("x-gateway-error-code")
            .and_then(|v| v.to_str().ok())
        {
            Some("not_ready") => "not_ready",
            Some("db_unavailable") => "db_unavailable",
            Some("auth_dependency_unavailable") => "auth_dependency_unavailable",
            Some("concurrency_limited") => "concurrency_limited",
            _ => "other",
        };
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .unwrap();
        let live = client
            .get(format!("{}/livez", h.base))
            .send()
            .await
            .unwrap()
            .status();
        let ready = client
            .get(format!("{}/readyz", h.base))
            .send()
            .await
            .unwrap()
            .status();
        let (database, schema) = h.db.store.health().await;
        eprintln!(
            "S7_SAFE_DIAGNOSTIC status={} code={code} live={} ready={} database={database} schema={schema} active={} permits={}",
            r.status().as_u16(),
            live.as_u16(),
            ready.as_u16(),
            h.runtime.active_requests(),
            h.runtime.available_inference()
        );
    }
    assert_eq!(r.status(), 403);
    assert_eq!(r.headers()["x-gateway-error-code"], "unsettled_usage_block");
    assert_eq!(h.provider.calls.lock().await.len(), 1);
}
async fn strict(h: &Harness) {
    sqlx::query("UPDATE gw_policies SET budget_mode='reserved',budget_limit_usd=1,tpm_limit=5000 WHERE principal_id=$1").bind(h.principal).execute(h.db.store.pool()).await.unwrap();
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_missing_conflicting_usage_done_or_model_never_settles() {
    for stream in [false, true] {
        let base = chat_native(stream);
        let mut variants=vec![base.replace("\"prompt_tokens\":100","\"prompt_tokens\":101"),base.replace("\"cached_tokens\":40","\"cached_tokens\":101"),base.replace("\"reasoning_tokens\":10","\"reasoning_tokens\":21"),base.replace("\"synthetic-text\"","\"unpriced-alias\""),base.replace("\"prompt_tokens_details\":{\"cached_tokens\":40,\"audio_tokens\":0},",""),base.replace("\"completion_tokens_details\":{\"reasoning_tokens\":10,\"audio_tokens\":0,\"accepted_prediction_tokens\":0,\"rejected_prediction_tokens\":0}","\"completion_tokens_details\":{}"),base.replace("\"completion_tokens\":20","\"completion_tokens\":20,\"unknown_billable_tokens\":5")];
        if stream {
            variants.extend([
                base.replace("data: [DONE]\n\n", ""),
                base.clone() + "data: [DONE]\n\n",
                base.replace(
                    "\"choices\":[],\"usage\":",
                    "\"choices\":[{\"index\":0}],\"usage\":",
                ),
                base.replace(
                    "\"choices\":[],\"usage\":",
                    "\"service_tier\":\"flex\",\"choices\":[],\"usage\":",
                ),
                base.replace("data: [DONE]\n\n", "data: {\n\ndata: [DONE]\n\n"),
            ]);
        } else {
            variants.extend([
                base.clone() + "{}",
                base.replace(
                    "\"object\":\"chat.completion\"",
                    "\"service_tier\":\"flex\",\"object\":\"chat.completion\"",
                ),
            ]);
        }
        for native in variants {
            let h = Harness::with_config(configuration_with(true, "max_completion_tokens")).await;
            strict(&h).await;
            set(&h, &native, stream).await;
            let body = if stream {
                CHAT.replacen(
                    '{',
                    "{\"stream\":true,\"stream_options\":{\"include_usage\":true},",
                    1,
                )
            } else {
                CHAT.into()
            };
            let r = h
                .request("/openai/v1/chat/completions", body)
                .send()
                .await
                .unwrap();
            let _ = r.bytes().await;
            unknown(&h).await;
            let held: String = sqlx::query_scalar("SELECT reserved_usd::text FROM gw_usage_totals")
                .fetch_one(h.db.store.pool())
                .await
                .unwrap();
            assert_eq!(held, "0.004296000000");
            h.close().await;
        }
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn embeddings_invalid_vectors_usage_or_model_are_unknown_and_block_other_api() {
    let base = embeddings_native(1, 3, "float");
    let mut variants = vec![
        base.replace("0.5,0.5,0.5", "0.5,0.5"),
        base.replace("0.5,0.5,0.5", "0.5,0.5,0.5,0.5"),
        base.replace("\"index\":0", "\"index\":1"),
        base.replace("\"index\":0,", ""),
        base.replace("\"total_tokens\":100", "\"total_tokens\":101"),
        base.replace("\"prompt_tokens\":100,", ""),
        base.replace("synthetic-text", "unknown-alias"),
        base.replace("0.5,0.5,0.5", "1e999,0.5,0.5"),
        base.clone() + "{}",
    ];
    let mut v: serde_json::Value = serde_json::from_str(&base).unwrap();
    v["data"] = serde_json::json!([v["data"][0].clone(), v["data"][0].clone()]);
    variants.push(v.to_string());
    for native in variants {
        let h = Harness::new().await;
        set(&h, &native, false).await;
        let r = h
            .request("/openai/v1/embeddings", EMBEDDINGS)
            .send()
            .await
            .unwrap();
        let _ = r.bytes().await;
        unknown(&h).await;
        let r = h
            .request("/openai/v1/chat/completions", CHAT)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
        h.close().await;
    }
    // base64의 정확 decoded float32 길이/유한성/padding도 검증한다.
    for encoded in [
        "AAAA",
        "AAAAAAAAAAAAAAAAAAAAAA==",
        "AACAfwAAAD8AAAA/",
        "AAAAAAAAAAAAAAAA=",
        "!!!!!!!!!!!!",
    ] {
        let h = Harness::new().await;
        let mut v: serde_json::Value = serde_json::from_str(&base).unwrap();
        v["data"][0]["embedding"] = encoded.into();
        set(&h, &v.to_string(), false).await;
        let r = h
            .request(
                "/openai/v1/embeddings",
                EMBEDDINGS.replace("float", "base64"),
            )
            .send()
            .await
            .unwrap();
        let _ = r.bytes().await;
        unknown(&h).await;
        h.close().await;
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_verified_strict_reservation_releases_and_unverified_apis_reject() {
    for stream in [false, true] {
        let h = Harness::with_config(configuration_with(true, "max_completion_tokens")).await;
        strict(&h).await;
        h.provider.hardcap(4096).await;
        let native = chat_native(stream).replace("\"stop\"", "\"length\"");
        set(&h, &native, stream).await;
        let (started, release) = h.provider.hold_response().await;
        let body = if stream {
            CHAT.replacen(
                '{',
                "{\"stream\":true,\"stream_options\":{\"include_usage\":true},",
                1,
            )
        } else {
            CHAT.into()
        };
        let send = h.request("/openai/v1/chat/completions", body).send();
        let check = async {
            started.await.unwrap();
            let row = sqlx::query(
                "SELECT reservation_usd::text AS r,reserved_tokens FROM gw_request_budgets",
            )
            .fetch_one(h.db.store.pool())
            .await
            .unwrap();
            assert_eq!(row.get::<String, _>("r"), "0.004296000000");
            assert_eq!(row.get::<i64, _>("reserved_tokens"), 4196);
            release.send(()).unwrap();
        };
        let (r, ()) = tokio::join!(send, check);
        let r = r.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.bytes().await.unwrap(), native);
        assert_eq!(state(&h).await, (1, 0, "0.000120000000".into(), 100, 20));
        let row=sqlx::query("SELECT t.reserved_usd::text AS r,w.reserved_tokens,w.settled_tokens FROM gw_usage_totals t JOIN gw_rate_windows w USING(scope_id)").fetch_one(h.db.store.pool()).await.unwrap();
        assert_eq!(row.get::<String, _>("r"), "0.000000000000");
        assert_eq!(row.get::<i64, _>("reserved_tokens"), 0);
        assert_eq!(row.get::<i64, _>("settled_tokens"), 120);
        h.close().await;
    }
    for path in ["/openai/v1/chat/completions", "/openai/v1/embeddings"] {
        let h = Harness::new().await;
        strict(&h).await;
        let r = h
            .request(
                path,
                if path.ends_with("embeddings") {
                    EMBEDDINGS
                } else {
                    CHAT
                },
            )
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        assert_eq!(
            r.headers()["x-gateway-error-code"],
            "unsupported_strict_policy"
        );
        assert!(h.provider.calls.lock().await.is_empty());
        h.close().await;
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_output_field_is_profile_specific_and_optional_features_preserve_native() {
    for output in ["max_tokens", "max_completion_tokens"] {
        let h = Harness::with_config(configuration_with(true, output)).await;
        let native = if output == "max_tokens" {
            chat_native(false).replace("\"reasoning_tokens\":10", "\"reasoning_tokens\":0")
        } else {
            chat_native(false)
        };
        set(&h, &native, false).await;
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScLbtAAAAABJRU5ErkJggg==";
        let features = vec![
            serde_json::json!({"messages":[{"role":"system","content":"synthetic"},{"role":"user","content":[{"type":"text","text":"synthetic"},{"type":"image_url","image_url":{"url":format!("data:image/png;base64,{png}"),"detail":"auto"}}]}]}),
            serde_json::json!({"messages":[{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"f","arguments":"{opaque not JSON}"}}]},{"role":"tool","tool_call_id":"call_1","content":"synthetic result"}],"tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object","properties":{"model":{"type":"string"}}}}}],"tool_choice":{"type":"function","function":{"name":"f"}},"parallel_tool_calls":false}),
            serde_json::json!({"reasoning_effort":"low","response_format":{"type":"json_schema","json_schema":{"name":"answer","schema":{"type":"object","properties":{"x":{"type":"string"}}},"strict":true}}}),
        ];
        for feature in features {
            let mut body: serde_json::Value = serde_json::from_str(CHAT).unwrap();
            body.as_object_mut()
                .unwrap()
                .remove("max_completion_tokens");
            body[output] = 100.into();
            for (k, v) in feature.as_object().unwrap() {
                if output == "max_tokens" && k == "reasoning_effort" {
                    continue;
                }
                body[k] = v.clone();
            }
            let raw = format!(" {} ", body);
            let r = h
                .request("/openai/v1/chat/completions", raw.clone())
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            assert_eq!(r.bytes().await.unwrap(), native);
            assert_eq!(h.provider.calls.lock().await.last().unwrap().body, raw);
        }
        assert_eq!(state(&h).await, (3, 0, "0.000360000000".into(), 300, 60));
        h.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_embeddings_transport_errors_strip_company_secrets_and_never_redirect_or_retry() {
    for (path, request) in [
        ("/openai/v1/chat/completions", CHAT),
        ("/openai/v1/embeddings", EMBEDDINGS),
    ] {
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
   "auth401"|"auth403"=>(if kind=="auth401"{StatusCode::UNAUTHORIZED}else{StatusCode::FORBIDDEN},HeaderMap::from_iter([("content-type".parse().unwrap(),"application/json".parse().unwrap()),("www-authenticate".parse().unwrap(),"Bearer synthetic-secret".parse().unwrap())]),Bytes::from_static(b"synthetic-company-error-content")),
   "redirect"=>(StatusCode::TEMPORARY_REDIRECT,HeaderMap::from_iter([("location".parse().unwrap(),target_server.base.as_str().parse().unwrap())]),Bytes::new()),
   "encoding"=>(StatusCode::OK,HeaderMap::from_iter([("content-type".parse().unwrap(),"application/json".parse().unwrap()),("content-encoding".parse().unwrap(),"gzip".parse().unwrap())]),Bytes::from(chat_native(false))),
   "native_error"=>(StatusCode::TOO_MANY_REQUESTS,HeaderMap::from_iter([("content-type".parse().unwrap(),"application/json".parse().unwrap()),("retry-after".parse().unwrap(),"2".parse().unwrap()),("set-cookie".parse().unwrap(),"synthetic-forbidden".parse().unwrap())]),Bytes::from_static(br#"{"error":{"message":"Synthetic provider rate","type":"rate_limit_error","param":null,"code":"rate_limit_exceeded"}}"#)),
   _=>(StatusCode::OK,HeaderMap::new(),Bytes::new())
  };
            h.provider.set(status, headers, body.clone()).await;
            let r = h
                .request(path, request)
                .header("openai-project", "client-project")
                .header("idempotency-key", "client-dedup")
                .header("cookie", "synthetic-client-cookie")
                .send()
                .await
                .unwrap();
            assert!(!r.headers().contains_key("x-gateway-dispatch-state"));
            assert_eq!(r.headers()["x-should-retry"], "false");
            assert!(!r.headers().contains_key("location"));
            assert!(!r.headers().contains_key("www-authenticate"));
            assert!(!r.headers().contains_key("set-cookie"));
            if kind == "native_error" {
                assert_eq!(r.status(), 429);
                assert_eq!(r.headers()["retry-after"], "2");
                assert_eq!(r.bytes().await.unwrap(), body);
            } else {
                assert_eq!(r.status(), 502);
                let bytes = r.bytes().await.unwrap();
                let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(v["error"]["type"], "server_error");
                assert!(!std::str::from_utf8(&bytes).unwrap().contains("synthetic-"));
            }
            h.runtime.wait_requests().await;
            unknown(&h).await;
            assert!(target.calls.lock().await.is_empty());
            let calls = h.provider.calls.lock().await;
            assert_eq!(calls.len(), 1);
            for name in [
                "openai-project",
                "idempotency-key",
                "cookie",
                "anthropic-version",
            ] {
                assert!(!calls[0].headers.contains_key(name));
            }
            drop(calls);
            h.close().await;
        }
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
    serde_json::json!([{"provider":"openai","model":"synthetic-text","capabilities":["chat_completions","embeddings"]}])
}
fn limits() -> serde_json::Value {
    serde_json::json!({"budget":null,"rpm":null,"tpm":null,"count_rpm":null})
}
async fn key(h: &Harness, service: Option<uuid::Uuid>) -> (String, uuid::Uuid) {
    let mut input = serde_json::json!({"name":"합성 Responses key","validity":"3_months","grants":grants(),"limits":{"budget":{"mode":"actual","limit_usd":"1.000000000000","period":{"kind":"calendar_month","timezone":"UTC"}},"rpm":null,"tpm":null,"count_rpm":null}});
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
async fn chat_embeddings_personal_and_service_keys_apply_both_scopes_owner_and_revoke() {
    for (path, body) in [
        ("/openai/v1/chat/completions", CHAT),
        ("/openai/v1/embeddings", EMBEDDINGS),
    ] {
        let h = Harness::new().await;
        h.provider
            .set(
                StatusCode::OK,
                HeaderMap::from_iter([(
                    "content-type".parse().unwrap(),
                    "application/json".parse().unwrap(),
                )]),
                Bytes::from(if path.ends_with("embeddings") {
                    embeddings_native(1, 3, "float")
                } else {
                    chat_native(false)
                }),
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
        let(status,v)=api(&h,reqwest::Method::POST,"/gateway/admin/principals",Some(serde_json::json!({"display_name":"합성 Responses service","management_owner_id":h.principal}))).await;
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
    let home = std::path::PathBuf::from(format!("/tmp/m-gated-s7-sdk-{}", uuid::Uuid::new_v4()));
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
        .env("MGATED_GATEWAY_BASE", format!("{}/openai/v1", h.base))
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
            "test:chat-embeddings",
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
    assert!(
        passed
            && stdout.contains(&format!(
                "SDK_S7_OK status={status} attempts=1 maxRetries=0"
            ))
            && stderr.is_empty(),
        "합성 Responses SDK 검증 실패(kind={kind},status={status}); 원문오류/본문 출력없음"
    );
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL and fixed TEST_FNM_DIR/SDK"]
async fn chat_embeddings_fixed_sdk_observes_native_usage_encoding_and_one_send() {
    for kind in [
        "chat",
        "chat_stream",
        "embeddings_float",
        "embeddings_base64",
        "embeddings_default",
    ] {
        let h = Harness::new().await;
        let native = if kind.starts_with("embeddings") {
            embeddings_native(
                1,
                3,
                if kind == "embeddings_float" {
                    "float"
                } else {
                    "base64"
                },
            )
        } else {
            chat_native(kind == "chat_stream")
        };
        set(&h, &native, kind == "chat_stream").await;
        sdk(&h, kind, 200, "completed", &h.token).await;
        assert_eq!(h.provider.calls.lock().await.len(), 1);
        assert_eq!(state(&h).await.1, 0);
        h.close().await;
    }
    for (kind, status) in [
        ("chat_bad_usage", 400),
        ("chat", 401),
        ("embeddings_float", 401),
        ("chat", 502),
        ("embeddings_float", 502),
    ] {
        let h = Harness::new().await;
        h.provider
            .set(
                StatusCode::UNAUTHORIZED,
                HeaderMap::new(),
                Bytes::from_static(b"synthetic-company-secret"),
            )
            .await;
        sdk(
            &h,
            kind,
            status,
            "completed",
            if status == 401 {
                "synthetic-invalid"
            } else {
                &h.token
            },
        )
        .await;
        assert_eq!(
            h.provider.calls.lock().await.len(),
            usize::from(status == 502)
        );
        if status == 502 {
            unknown(&h).await;
        }
        h.close().await;
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_legacy_output_cannot_verify_unexpected_hidden_reasoning() {
    for stream in [false, true] {
        let h = Harness::with_config(configuration_with(true, "max_tokens")).await;
        strict(&h).await;
        set(&h, &chat_native(stream), stream).await;
        let mut body = CHAT.replace("max_completion_tokens", "max_tokens");
        if stream {
            body = body.replacen(
                '{',
                "{\"stream\":true,\"stream_options\":{\"include_usage\":true},",
                1,
            );
        }
        let r = h
            .request("/openai/v1/chat/completions", body)
            .send()
            .await
            .unwrap();
        let _ = r.bytes().await;
        let s = state(&h).await;
        h.close().await;
        assert_eq!(
            s.1, 1,
            "legacy max_tokens 계약에 포함되지 않은 reasoning은 확정 불가"
        );
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_embeddings_exact_sixteen_mib_cl_chunked_and_stream_retained_bounds() {
    use sha2::{Digest, Sha256};
    for kind in ["chat", "chat_stream", "embeddings"] {
        for chunked in [false, true] {
            for size in [16 * 1024 * 1024, 16 * 1024 * 1024 + 1] {
                let h = Harness::new().await;
                let stream = kind == "chat_stream";
                let base = if kind == "embeddings" {
                    embeddings_native(1, 3, "float")
                } else {
                    chat_native(stream)
                };
                let native = if stream {
                    let first_end = base.find("\r\n\r\n").unwrap() + 4;
                    let first = &base[..first_end];
                    first.replacen(
                        "synthetic\"",
                        &("x".repeat(size - first_end + "synthetic".len()) + "\""),
                        1,
                    ) + &base[first_end..]
                } else if kind == "chat" {
                    base.replacen(
                        "synthetic\"",
                        &("x".repeat(size - base.len() + "synthetic".len()) + "\""),
                        1,
                    )
                } else {
                    base.clone() + &" ".repeat(size - base.len())
                };
                let headers = HeaderMap::from_iter([(
                    "content-type".parse().unwrap(),
                    if stream {
                        "text/event-stream"
                    } else {
                        "application/json"
                    }
                    .parse()
                    .unwrap(),
                )]);
                if chunked {
                    h.provider
                        .set_chunked(StatusCode::OK, headers, Bytes::from(native.clone()))
                        .await;
                } else {
                    h.provider
                        .set(StatusCode::OK, headers, Bytes::from(native.clone()))
                        .await;
                }
                let path = if kind == "embeddings" {
                    "/openai/v1/embeddings"
                } else {
                    "/openai/v1/chat/completions"
                };
                let body = if stream {
                    CHAT.replacen(
                        '{',
                        "{\"stream\":true,\"stream_options\":{\"include_usage\":true},",
                        1,
                    )
                } else if kind == "embeddings" {
                    EMBEDDINGS.into()
                } else {
                    CHAT.into()
                };
                let r = h.request(path, body).send().await.unwrap();
                let status = r.status();
                let bytes = r.bytes().await;
                if size == 16 * 1024 * 1024 {
                    assert_eq!(status, 200);
                    let bytes = bytes.unwrap();
                    assert_eq!(bytes.len(), native.len());
                    assert_eq!(Sha256::digest(&bytes), Sha256::digest(native.as_bytes()));
                    assert_eq!(state(&h).await.1, 0);
                } else {
                    if !stream {
                        assert_eq!(status, 502);
                    }
                    eprintln!("S7_BOUNDARY_CASE kind={kind} chunked={chunked} size={size}");
                    unknown(&h).await;
                }
                let resources = h.runtime.request_resources();
                assert!(resources.parser_peak <= 256 * 1024);
                assert!(resources.queue_peak <= 64 * 1024);
                assert_eq!((resources.body_bytes, resources.queue_bytes), (0, 0));
                assert_eq!(h.runtime.available_inference(), 16);
                assert_eq!(h.provider.calls.lock().await.len(), 1);
                h.close().await;
            }
        }
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn embeddings_parser_float_combination_limit_refuses_before_admission() {
    let h = Harness::new().await;
    for (count, dim) in [(29, 3445), (256, 4096)] {
        let body=serde_json::json!({"model":"synthetic-text","input":vec!["x";count],"dimensions":dim,"encoding_format":"float"}).to_string();
        let r = h
            .request("/openai/v1/embeddings", body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        assert_eq!(r.headers()["x-gateway-dispatch-state"], "not_dispatched");
    }
    assert_eq!(state(&h).await.0, 0);
    assert!(h.provider.calls.lock().await.is_empty());
    let native = embeddings_native(29, 3444, "float");
    set(&h, &native, false).await;
    let body=serde_json::json!({"model":"synthetic-text","input":vec!["x";29],"dimensions":3444,"encoding_format":"float"}).to_string();
    let r = h
        .request("/openai/v1/embeddings", body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.bytes().await.unwrap(), native);
    assert_eq!(state(&h).await, (1, 0, "0.000100000000".into(), 100, 0));
    h.close().await;
}

async fn reservation(h: &Harness) {
    let r=sqlx::query("SELECT r.phase,b.reservation_usd::text AS amount,b.remaining_reservation_usd::text AS held,b.reserved_tokens FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id ORDER BY admitted_at DESC LIMIT 1").fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(r.get::<String, _>("phase"), "dispatching");
    assert_eq!(r.get::<String, _>("amount"), "0.004296000000");
    assert_eq!(r.get::<String, _>("held"), "0.004296000000");
    assert_eq!(r.get::<i64, _>("reserved_tokens"), 4196);
}

fn chat_start() -> String {
    chat_native(true)
        .split("\r\n\r\n")
        .next()
        .unwrap()
        .to_owned()
        + "\r\n\r\n"
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_full_queue_cancel_has_no_synthetic_terminal_and_holds_unknown_reservation() {
    use std::time::Duration;
    let h = Harness::with_config(configuration_with(true, "max_completion_tokens")).await;
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
        .uri(format!("{}/openai/v1/chat/completions", h.base))
        .header("authorization", format!("Bearer {}", h.token))
        .header("content-type", "application/json")
        .body(())
        .unwrap();
    let (r, mut send) = sender.send_request(req, false).unwrap();
    send.send_data(
        Bytes::from(CHAT.replacen(
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
async fn chat_absolute_transport_deadline_preserves_five_second_finalization_gap() {
    let cfg = configuration_options(
        true,
        "max_completion_tokens",
        "\nlimits:\n  transport_timeout_ms: 1500\n  request_timeout_ms: 6500\n  body_timeout_ms: 1500\n  body_idle_timeout_ms: 1500\n",
    );
    let h = Harness::with_config(cfg).await;
    strict(&h).await;
    let (tx, stopped) = h.provider.stream().await;
    tx.send(Bytes::from(chat_start())).await.unwrap();
    let r = h
        .request(
            "/openai/v1/chat/completions",
            CHAT.replacen(
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
async fn chat_embeddings_uncertain_dispatch_original_id_expires_unknown_without_reissuing_permit() {
    use m_gated::{
        config::{ApiKind, ProviderId},
        ledger::finance::{Finalized, LedgerError, RequestSpec},
        providers::transport::{NativeClient, SendOptions},
    };
    use std::sync::atomic::Ordering;
    for api in [ApiKind::OpenaiChat, ApiKind::Embeddings] {
        let db = postgres::TestDb::new().await;
        let principal = finance::human(db.store.pool()).await;
        sqlx::query("UPDATE gw_policies SET allow_rules=$2,budget_mode='actual',budget_limit_usd=1,tpm_limit=NULL WHERE principal_id=$1").bind(principal).bind(grants()).execute(db.store.pool()).await.unwrap();
        let current = finance::current(db.store.pool(), principal).await;
        let proxy = pg_proxy::CommitProxy::new().await;
        let store = db.clock_store(Some(proxy.port)).await;
        let cfg = configuration_with(true, "max_completion_tokens");
        let profile = cfg.providers()[0]
            .profiles
            .iter()
            .find(|p| p.api == api)
            .unwrap();
        let ledger = PgLedger::new(store.clone(), AccessSettings::from_config(&cfg), [1; 32]);
        let spec = RequestSpec::from_profile(
            ProviderId::Openai,
            profile,
            if api == ApiKind::Embeddings {
                None
            } else {
                Some(100)
            },
        );
        ledger.admit_request(&current, &spec).await.unwrap();
        let provider = provider::Provider::new();
        let server = provider.serve().await;
        let client = NativeClient::openai_loopback_for_test(
            server.base.clone(),
            "synthetic-company-key".into(),
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
                        Bytes::from_static(if api == ApiKind::Embeddings {
                            EMBEDDINGS.as_bytes()
                        } else {
                            CHAT.as_bytes()
                        }),
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
async fn chat_embeddings_route_auth_method_cookie_and_feature_negatives() {
    let h = Harness::new().await;
    for path in ["/openai/v1/chat/completions", "/openai/v1/embeddings"] {
        let body = if path.ends_with("embeddings") {
            EMBEDDINGS
        } else {
            CHAT
        };
        for token in ["synthetic-invalid", ""] {
            let r = h
                .request_with_token(path, body, token)
                .header("cookie", "mgated_access=synthetic")
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 401);
            assert_eq!(r.headers()["x-gateway-dispatch-state"], "not_dispatched");
        }
        let r = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .unwrap()
            .get(format!("{}{path}", h.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 405);
        assert_eq!(r.headers()["allow"], "POST");
        let extras = if path.ends_with("embeddings") {
            vec![
                r#", "tools":[]"#,
                r#", "reasoning":{"effort":"low"}"#,
                r#", "response_format":{"type":"json_object"}"#,
                r#", "input":{"url":"https://example.invalid/image.png"}"#,
            ]
        } else {
            vec![
                r#", "tools":[{"type":"web_search"}]"#,
                r#", "response_format":{"type":"json_schema","json_schema":{"name":"x","schema":[]}}"#,
                r#", "messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.invalid/image.png"}}]}]"#,
                r#", "messages":[{"role":"system","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}]}]"#,
                r#", "tools":[{"type":"function","function":{"name":"f","parameters":[]}}]"#,
            ]
        };
        for extra in extras {
            let raw = body.trim().trim_end_matches('}').to_owned() + extra + "}";
            let r = h.request(path, raw).send().await.unwrap();
            assert_eq!(r.status(), 400);
            assert_eq!(r.headers()["x-gateway-dispatch-state"], "not_dispatched");
        }
    }
    // 중복 field 반례와 구분하여 문법상 정상인 원격 이미지/문맥 위반도 실제 route에서 거부한다.
    for message in [
        serde_json::json!({"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.invalid/image.png"}}]}),
        serde_json::json!({"role":"system","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}]}),
        serde_json::json!({"role":"user","content":"synthetic","tool_calls":[{"id":"c","type":"function","function":{"name":"f","arguments":"x"}}]}),
    ] {
        let body=serde_json::json!({"model":"synthetic-text","messages":[message],"max_completion_tokens":100}).to_string();
        let r = h
            .request("/openai/v1/chat/completions", body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        assert_eq!(r.headers()["x-gateway-dispatch-state"], "not_dispatched");
    }
    assert_eq!(state(&h).await.0, 0);
    assert!(h.provider.calls.lock().await.is_empty());
    h.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn embeddings_raw_string_defaults_order_and_canonical_encoding() {
    let h = Harness::new().await;
    let native = embeddings_native(1, 3, "float");
    set(&h, &native, false).await;
    let raw = r#" { "model":"synthetic-text", "input":"synthetic" } "#;
    let r = h
        .request("/openai/v1/embeddings", raw)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.bytes().await.unwrap(), native);
    assert_eq!(h.provider.calls.lock().await[0].body, raw);
    assert_eq!(state(&h).await, (1, 0, "0.000100000000".into(), 100, 0));
    h.close().await;
    let base = embeddings_native(2, 3, "float");
    let mut v: serde_json::Value = serde_json::from_str(&base).unwrap();
    v["data"].as_array_mut().unwrap().reverse();
    let h = Harness::new().await;
    set(&h, &v.to_string(), false).await;
    let raw =
        serde_json::json!({"model":"synthetic-text","input":["a","b"],"dimensions":3}).to_string();
    let r = h
        .request("/openai/v1/embeddings", raw)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 502);
    r.bytes().await.unwrap();
    unknown(&h).await;
    h.close().await;
    // float32 3개는12bytes라 padding이 없으며 정확 차원이 아닌 임의 padding은 거부한다.
    for encoded in [
        "AAAAAAAAAAAAAAAA=",
        "AAAA=AAAAAAAAAAA",
        "AACAfwAAAD8AAAA/",
        "AAAAAAAAAAAAAAAB==",
    ] {
        let h = Harness::new().await;
        let mut v: serde_json::Value = serde_json::from_str(&native).unwrap();
        v["data"][0]["embedding"] = encoded.into();
        set(&h, &v.to_string(), false).await;
        let r = h
            .request(
                "/openai/v1/embeddings",
                EMBEDDINGS.replace("float", "base64"),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 502);
        r.bytes().await.unwrap();
        unknown(&h).await;
        h.close().await;
    }
    // 1차원(4bytes)의 canonical padding/잔여bit 정상과 음성.
    for (encoded, valid) in [("AAAAPw==", true), ("AAAAPx==", false), ("AAAAPw=", false)] {
        let h = Harness::new().await;
        let mut v: serde_json::Value = serde_json::from_str(&native).unwrap();
        v["data"][0]["embedding"] = encoded.into();
        set(&h, &v.to_string(), false).await;
        let r = h
            .request(
                "/openai/v1/embeddings",
                EMBEDDINGS
                    .replace("float", "base64")
                    .replace("\"dimensions\":3", "\"dimensions\":1"),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), if valid { 200 } else { 502 });
        r.bytes().await.unwrap();
        if valid {
            assert_eq!(state(&h).await.1, 0)
        } else {
            unknown(&h).await;
        }
        h.close().await;
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn embeddings_slow_nonstream_upstream_uses_original_deadline_and_unknown() {
    let h=Harness::with_config(configuration_options(false,"max_completion_tokens","\nlimits:\n  transport_timeout_ms: 1500\n  request_timeout_ms: 6500\n  body_timeout_ms: 1500\n  body_idle_timeout_ms: 1500\n")).await;
    let (tx, stopped) = h.provider.stream_json().await;
    tx.send(Bytes::from_static(br#"{"object":"list","data":["#))
        .await
        .unwrap();
    let (started, release) = h.provider.hold_response().await;
    let clock = tokio::time::Instant::now();
    let request = h.request("/openai/v1/embeddings", EMBEDDINGS).send();
    let dispatch = async {
        started.await.unwrap();
        let id: uuid::Uuid = sqlx::query_scalar(
            "SELECT id FROM gw_requests WHERE phase='dispatching' AND api='embeddings'",
        )
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
        release.send(()).unwrap();
        id
    };
    let (result, request_id) = tokio::join!(request, dispatch);
    let elapsed = clock.elapsed();
    assert!(
        (std::time::Duration::from_millis(1400)..std::time::Duration::from_millis(2500))
            .contains(&elapsed),
        "원래 1.5초 transport 기한"
    );
    let received_response = result.is_ok();
    match result {
        Ok(r) => {
            assert_eq!(r.status(), 504);
            assert_eq!(r.headers()["x-gateway-error-code"], "upstream_read_timeout");
            assert_eq!(r.headers()["x-should-retry"], "false");
            assert!(!r.headers().contains_key("x-gateway-dispatch-state"));
            let v: serde_json::Value = serde_json::from_slice(&r.bytes().await.unwrap()).unwrap();
            assert_eq!(v["error"]["code"], "gateway_upstream_read_timeout");
        }
        Err(error) => {
            assert!(
                error.is_request()
                    && !error.is_timeout()
                    && !error.is_connect()
                    && !error.is_body()
                    && error.status().is_none(),
                "실수신/dispatch 뒤 원래 hard transport 기한의 연결종료만 허용"
            );
        }
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), stopped)
        .await
        .unwrap()
        .unwrap();
    unknown(&h).await;
    let row=sqlx::query("SELECT id,cost_usd IS NULL AS cost_null,execution_stopped_at IS NOT NULL AS stopped,reason_code,outcome,finished_at>=transport_deadline AS finalized_after_transport,execution_stopped_at>=transport_deadline AS stopped_after_transport,extract(epoch FROM execution_deadline-transport_deadline)::bigint AS gap,lease_expires_at=execution_deadline AND dispatch_authorized_at<transport_deadline AND transport_deadline-dispatch_authorized_at<=interval '1.5 seconds' AND execution_deadline-admitted_at<=interval '6.5 seconds' AND execution_deadline-admitted_at>interval '5 seconds' AS original FROM gw_requests WHERE id=$1").bind(request_id).fetch_one(h.db.store.pool()).await.unwrap();
    let reason: String = row.get("reason_code");
    let safe_reason = match reason.as_str() {
        "upstream_read_timeout" => "upstream_read_timeout",
        "client_cancelled" => "client_cancelled",
        "shutdown_cancelled" => "shutdown_cancelled",
        _ => "other",
    };
    let outcome: String = row.get("outcome");
    let safe_outcome = match outcome.as_str() {
        "failed" => "failed",
        "cancelled" => "cancelled",
        _ => "other",
    };
    eprintln!(
        "EMBEDDINGS_DEADLINE_DIAGNOSTIC reason={safe_reason} outcome={safe_outcome} response={received_response} elapsed_ms={} same_id={} original={} finalized_after_transport={} stopped_after_transport={}",
        elapsed.as_millis(),
        row.get::<uuid::Uuid, _>("id") == request_id,
        row.get::<bool, _>("original"),
        row.get::<bool, _>("finalized_after_transport"),
        row.get::<bool, _>("stopped_after_transport")
    );
    assert_eq!(row.get::<uuid::Uuid, _>("id"), request_id);
    assert!(
        row.get::<bool, _>("cost_null")
            && row.get::<bool, _>("stopped")
            && row.get::<bool, _>("original")
    );
    assert_eq!(row.get::<String, _>("reason_code"), "upstream_read_timeout");
    assert_eq!(row.get::<String, _>("outcome"), "failed");
    assert_eq!(row.get::<i64, _>("gap"), 5);
    assert_eq!(h.provider.calls.lock().await.len(), 1);
    assert_eq!(
        (
            h.runtime.available_inference(),
            h.runtime.available_nonstream(),
            h.runtime.active_requests()
        ),
        (16, 4, 0)
    );
    let resources = h.runtime.request_resources();
    assert_eq!((resources.body_bytes, resources.queue_bytes), (0, 0));
    drop(tx);
    h.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn embeddings_h2_slow_body_holds_permit_and_reset_releases_without_changing_final_usage() {
    let h = Harness::new().await;
    let mut native = embeddings_native(1, 3, "float");
    native.push_str(&" ".repeat(1024 * 1024 - native.len()));
    set(&h, &native, false).await;
    let socket = tokio::net::TcpStream::connect(h.base.trim_start_matches("http://"))
        .await
        .unwrap();
    let mut builder = h2::client::Builder::new();
    builder.initial_window_size(0);
    let (mut sender, connection) = builder.handshake::<_, Bytes>(socket).await.unwrap();
    let connection = tokio::spawn(connection);
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("{}/openai/v1/embeddings", h.base))
        .header("authorization", format!("Bearer {}", h.token))
        .header("content-type", "application/json")
        .body(())
        .unwrap();
    let (r, mut send) = sender.send_request(req, false).unwrap();
    send.send_data(Bytes::from_static(EMBEDDINGS.as_bytes()), true)
        .unwrap();
    let response = r.await.unwrap();
    assert_eq!(response.status(), 200);
    let mut changes = h.runtime.resource_changes();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while h.runtime.request_resources().body_bytes < 1024 * 1024 {
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(h.runtime.available_inference(), 15);
    assert_eq!(h.runtime.available_nonstream(), 3);
    send.send_reset(h2::Reason::CANCEL);
    drop(response);
    assert_eq!(state(&h).await, (1, 0, "0.000100000000".into(), 100, 0));
    assert_eq!(h.runtime.available_inference(), 16);
    assert_eq!(h.runtime.available_nonstream(), 4);
    assert_eq!(h.runtime.request_resources().body_bytes, 0);
    drop(sender);
    connection.abort();
    let _ = connection.await;
    h.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn embeddings_input_only_price_settles_float_base64_and_permits_next_request() {
    let h = Harness::new().await;
    assert_eq!(configuration().providers()[0].profiles[1].rates.len(), 1);
    for encoding in ["float", "base64"] {
        let native = embeddings_native(1, 3, encoding);
        set(&h, &native, false).await;
        let r = h
            .request(
                "/openai/v1/embeddings",
                EMBEDDINGS.replace("float", encoding),
            )
            .send()
            .await
            .unwrap();
        let status = r.status();
        let id = r.headers()["x-gateway-request-id"]
            .to_str()
            .unwrap()
            .parse::<uuid::Uuid>()
            .unwrap();
        r.bytes().await.unwrap();
        h.runtime.wait_requests().await;
        let row = sqlx::query(
            "SELECT accounting_state,cost_usd::text AS cost FROM gw_requests WHERE id=$1",
        )
        .bind(id)
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
        let result = (
            status.as_u16(),
            row.get::<String, _>("accounting_state"),
            row.get::<Option<String>, _>("cost"),
        );
        if result != (200, "settled".into(), Some("0.000100000000".into())) {
            h.close().await;
            assert_eq!(
                result,
                (200, "settled".into(), Some("0.000100000000".into())),
                "최소 input-only 가격으로 완전 usage 정산"
            );
            return;
        }
    }
    assert_eq!(state(&h).await, (2, 0, "0.000200000000".into(), 200, 0));
    assert_eq!(h.provider.calls.lock().await.len(), 2);
    h.close().await;
}

// 검토 반례는 실제 dispatch 예약을 barrier로 먼저 관측하고 모든 assertion 전에 정리한다.
async fn chat_review_observation(native: &str, stream: bool) -> (u16, bool, bool, bool) {
    let h = Harness::with_config(configuration_with(true, "max_completion_tokens")).await;
    strict(&h).await;
    set(&h, native, stream).await;
    let (started, release) = h.provider.hold_response().await;
    let body = if stream {
        CHAT.replacen(
            '{',
            "{\"stream\":true,\"stream_options\":{\"include_usage\":true},",
            1,
        )
    } else {
        CHAT.into()
    };
    let send = h
        .request("/openai/v1/chat/completions", body.clone())
        .send();
    let before = async {
        started.await.unwrap();
        let reserved: bool = sqlx::query_scalar("SELECT phase='dispatching' AND accounting_state='pending' AND b.remaining_reservation_usd=0.004296 AND b.remaining_reserved_tokens=4196 FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id").fetch_one(h.db.store.pool()).await.unwrap();
        release.send(()).unwrap();
        reserved
    };
    let (response, reserved) = tokio::join!(send, before);
    let response = response.unwrap();
    let status = response.status().as_u16();
    let _ = response.bytes().await;
    h.runtime.wait_requests().await;
    let unknown: bool = sqlx::query_scalar("SELECT r.accounting_state='unknown' AND r.cost_usd IS NULL AND t.unknown_count=1 AND t.reserved_usd=0.004296 AND b.remaining_reservation_usd=0.004296 AND b.remaining_reserved_tokens=4196 AND w.reserved_tokens=4196 FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id JOIN gw_usage_totals t USING(scope_id) JOIN gw_rate_windows w USING(scope_id)").fetch_one(h.db.store.pool()).await.unwrap();
    let follow = h
        .request("/openai/v1/chat/completions", CHAT)
        .send()
        .await
        .unwrap();
    let blocked = follow.status() == 403
        && follow
            .headers()
            .get("x-gateway-error-code")
            .is_some_and(|v| v == "unsettled_usage_block");
    let calls = h.provider.calls.lock().await;
    let single_wire = calls.len() == 1
        && calls[0].body == body
        && calls[0].path == "/v1/chat/completions"
        && calls[0]
            .headers
            .get("authorization")
            .is_some_and(|v| v == "Bearer synthetic-company-key");
    drop(calls);
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gw_requests WHERE dispatch_authorized_at IS NOT NULL",
    )
    .fetch_one(h.db.store.pool())
    .await
    .unwrap();
    h.close().await;
    (
        status,
        reserved,
        unknown,
        blocked && single_wire && rows == 1,
    )
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_review_literal_length_usage_only_is_unknown() {
    let native = chat_native(true).replace("\"choices\":[]", "\"choices\":{\"$length\":0}");
    let actual = chat_review_observation(&native, true).await;
    assert!(
        actual.1 && actual.2 && actual.3,
        "R1 usage-only: status/reserved/unknown/blocked_wire={actual:?}"
    );
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_review_missing_message_is_unknown() {
    let native = chat_native(false).replace(
        "\"message\":{\"role\":\"assistant\",\"content\":\"synthetic\"},",
        "",
    );
    let actual = chat_review_observation(&native, false).await;
    assert!(
        actual.1 && actual.2 && actual.3,
        "R1 message: status/reserved/unknown/blocked_wire={actual:?}"
    );
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_review_early_unverified_tier_is_unknown() {
    let native = chat_native(true).replacen("{", "{\"service_tier\":\"flex\",", 1);
    let actual = chat_review_observation(&native, true).await;
    assert!(
        actual.1 && actual.2 && actual.3,
        "R2 tier: status/reserved/unknown/blocked_wire={actual:?}"
    );
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_review_native_shape_negative_matrix_is_unknown() {
    let mut variants = Vec::new();
    let base: serde_json::Value = serde_json::from_str(&chat_native(false)).unwrap();
    for message in [
        serde_json::json!(null),
        serde_json::json!([]),
        serde_json::json!({}),
        serde_json::json!({"role":"user","content":"x"}),
        serde_json::json!({"role":null,"content":"x"}),
        serde_json::json!({"role":"assistant"}),
        serde_json::json!({"role":"assistant","content":1}),
        serde_json::json!({"role":"assistant","content":[]}),
        serde_json::json!({"role":"assistant","content":null,"refusal":{}}),
        serde_json::json!({"role":"assistant","content":null,"tool_calls":{}}),
    ] {
        let mut v = base.clone();
        v["choices"][0]["message"] = message;
        variants.push((false, v.to_string()));
    }
    for delta in [
        "null",
        "[]",
        "0",
        "{\"role\":\"user\"}",
        "{\"role\":null}",
        "{\"content\":1}",
        "{\"content\":[]}",
        "{\"refusal\":true}",
        "{\"tool_calls\":{}}",
    ] {
        let native = chat_native(true).replacen(
            "\"delta\":{\"role\":\"assistant\",\"content\":\"synthetic\"}",
            &format!("\"delta\":{delta}"),
            1,
        );
        variants.push((true, native));
    }
    // 각 Choice 자체가 Object여야 하며 literal metadata가 배열 증명을 대체하지 못한다.
    for choices in [
        serde_json::json!({"$length":1,"*":{"index":0}}),
        serde_json::json!([null]),
        serde_json::json!([[]]),
    ] {
        let mut v = base.clone();
        v["choices"] = choices;
        variants.push((false, v.to_string()));
    }
    let mut failed = Vec::new();
    for (i, (stream, native)) in variants.iter().enumerate() {
        let actual = chat_review_observation(native, *stream).await;
        if !(actual.1 && actual.2 && actual.3) {
            failed.push((i, actual));
        }
    }
    assert!(failed.is_empty(), "R1 shape 위반: {failed:?}");
}

fn chat_review_chunks() -> Vec<serde_json::Value> {
    let chunks: Vec<serde_json::Value> = chat_native(true)
        .replace("\r\n\r\n", "\n\n")
        .split("\n\n")
        .filter_map(|s| s.strip_prefix("data: "))
        .filter(|s| *s != "[DONE]")
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0]["choices"].as_array().unwrap().len(), 1);
    assert_eq!(chunks[1]["choices"].as_array().unwrap().len(), 1);
    assert!(chunks[2]["choices"].as_array().unwrap().is_empty());
    chunks
}
fn chat_review_wire(chunks: &[serde_json::Value]) -> String {
    chunks
        .iter()
        .map(|v| format!("data: {v}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n"
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_review_first_middle_tier_matrix_cannot_recover_default_price() {
    let base = chat_review_chunks();
    let mut failed = Vec::new();
    for position in [0, 1] {
        for (tier_id, tier) in ["\"flex\"", "\"priority\"", "0", "null", "true", "{}"]
            .iter()
            .enumerate()
        {
            for last_default in [false, true] {
                let mut chunks = base.clone();
                let tier: serde_json::Value = serde_json::from_str(tier).unwrap();
                chunks[position]["service_tier"] = tier.clone();
                if last_default {
                    chunks[2]["service_tier"] = "default".into();
                }
                assert_eq!(chunks[position]["service_tier"], tier);
                assert_eq!(chunks[2].get("service_tier").is_some(), last_default);
                let native = chat_review_wire(&chunks);
                let actual = chat_review_observation(&native, true).await;
                if actual != (200, true, true, true) {
                    failed.push((position, tier_id, last_default, actual));
                }
            }
        }
    }
    assert!(
        failed.is_empty(),
        "R2 원본200/unknown/예약/차단/송신1 위반: {failed:?}"
    );
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_review_text_refusal_tools_structured_and_default_tier_preserve_native() {
    let h = Harness::with_config(configuration_with(true, "max_completion_tokens")).await;
    strict(&h).await;
    // 4196 최악 예약 + 이 정상 대조 전체의 이전120token 누계도 수용하는 명시 시험 정책이다.
    sqlx::query("UPDATE gw_policies SET tpm_limit=10000 WHERE principal_id=$1")
        .bind(h.principal)
        .execute(h.db.store.pool())
        .await
        .unwrap();
    let mut variants = Vec::new();
    let base: serde_json::Value = serde_json::from_str(&chat_native(false)).unwrap();
    for (message, reason) in [
        (
            serde_json::json!({"role":"assistant","content":"synthetic"}),
            "stop",
        ),
        (
            serde_json::json!({"role":"assistant","content":null,"refusal":"synthetic refusal"}),
            "stop",
        ),
        (
            serde_json::json!({"role":"assistant","content":null,"tool_calls":[{"id":"a","type":"function","function":{"name":"f","arguments":"{opaque"}},{"id":"b","type":"function","function":{"name":"g","arguments":"not JSON"}}]}),
            "tool_calls",
        ),
        (
            serde_json::json!({"role":"assistant","content":"{\"answer\":1}"}),
            "stop",
        ),
    ] {
        let mut v = base.clone();
        v["choices"][0]["message"] = message;
        v["choices"][0]["finish_reason"] = reason.into();
        variants.push((false, v.to_string()));
    }
    for mask in 0..8 {
        let mut chunks = chat_review_chunks();
        for (i, chunk) in chunks.iter_mut().enumerate() {
            if mask & (1 << i) != 0 {
                chunk["service_tier"] = "default".into();
            }
            assert_eq!(chunk.get("service_tier").is_some(), mask & (1 << i) != 0);
        }
        variants.push((true, chat_review_wire(&chunks)));
    }
    for delta in [
        serde_json::json!({"role":"assistant","content":null,"refusal":"synthetic refusal"}),
        serde_json::json!({"role":"assistant","content":"{\"answer\":1}"}),
    ] {
        let mut chunks = chat_review_chunks();
        chunks[0]["choices"][0]["delta"] = delta;
        variants.push((true, chat_review_wire(&chunks)));
    }
    // 정상 tool delta는 role/null/부분 인수를 허용하며 빈 finish delta와 사용량 전용 배열로 끝난다.
    let tool=chat_native(true).replacen("\"role\":\"assistant\",\"content\":\"synthetic\"", "\"role\":\"assistant\",\"content\":null,\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"{fragment\"}}]",1).replace("\"finish_reason\":\"stop\"","\"finish_reason\":\"tool_calls\"");
    variants.push((true, tool));
    let mut failed = Vec::new();
    for (i, (stream, native)) in variants.iter().enumerate() {
        set(&h, native, *stream).await;
        let body = if *stream {
            CHAT.replacen(
                '{',
                "{\"stream\":true,\"stream_options\":{\"include_usage\":true},",
                1,
            )
        } else {
            CHAT.into()
        };
        let r = h
            .request("/openai/v1/chat/completions", body)
            .send()
            .await
            .unwrap();
        let status = r.status();
        let bytes = r.bytes().await;
        if status != 200
            || !bytes
                .as_ref()
                .is_ok_and(|b| b.as_ref() == native.as_bytes())
        {
            failed.push(i);
        }
        h.runtime.wait_requests().await;
    }
    let s = state(&h).await;
    let calls = h.provider.calls.lock().await.len();
    let remaining: bool =
        sqlx::query_scalar("SELECT reserved_usd=0 AND unknown_count=0 FROM gw_usage_totals")
            .fetch_one(h.db.store.pool())
            .await
            .unwrap();
    h.close().await;
    assert!(failed.is_empty(), "정상 shape/native 대조 실패: {failed:?}");
    assert_eq!((s.0, s.1, s.3, s.4, calls), (15, 0, 1500, 300, 15));
    assert_eq!(s.2, "0.001800000000");
    assert!(remaining);
}

fn chat_error_event(named: bool, message: usize, ending: &str) -> String {
    let name = if named {
        format!("event: error{ending}")
    } else {
        String::new()
    };
    format!(
        "{name}data: {{\"error\":{{\"message\":\"{}\",\"type\":\"server_error\",\"param\":\"synthetic_param\",\"code\":\"synthetic_stream_error\"}}}}{ending}{ending}",
        "x".repeat(message)
    )
}
fn chat_stream_request() -> String {
    CHAT.replacen(
        '{',
        "{\"stream\":true,\"stream_options\":{\"include_usage\":true},",
        1,
    )
}
async fn native_error_accounting(h: &Harness) -> bool {
    h.runtime.wait_requests().await;
    let accounting: bool = sqlx::query_scalar("SELECT r.outcome='failed' AND r.accounting_state='unknown' AND r.cost_usd IS NULL AND r.execution_stopped_at IS NOT NULL AND t.unknown_count=1 AND t.reserved_usd=0.004296 AND b.remaining_reservation_usd=0.004296 AND b.remaining_reserved_tokens=4196 AND w.reserved_tokens=4196 FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id JOIN gw_usage_totals t USING(scope_id) JOIN gw_rate_windows w USING(scope_id)").fetch_one(h.db.store.pool()).await.unwrap();
    let resources = h.runtime.request_resources();
    let follow = h
        .request("/openai/v1/chat/completions", CHAT)
        .send()
        .await
        .unwrap();
    let blocked = follow.status() == 403
        && follow
            .headers()
            .get("x-gateway-error-code")
            .is_some_and(|v| v == "unsettled_usage_block");
    drop(follow);
    accounting
        && blocked
        && h.provider.calls.lock().await.len() == 1
        && h.runtime.active_requests() == 0
        && h.runtime.available_inference() == 16
        && resources.body_bytes == 0
        && resources.queue_bytes == 0
        && resources.queue_waiters == 0
        && resources.parser_peak <= 256 * 1024
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_native_sse_error_preserves_wire_and_failed_unknown_accounting() {
    let mut violations = Vec::new();
    for named in [false, true] {
        for prefix in [false, true] {
            for done in [false, true] {
                let h =
                    Harness::with_config(configuration_with(true, "max_completion_tokens")).await;
                strict(&h).await;
                let native = if prefix { chat_start() } else { String::new() }
                    + &chat_error_event(named, 50000, "\n")
                    + if done { "data: [DONE]\n\n" } else { "" };
                set(&h, &native, true).await;
                let (started, release) = h.provider.hold_response().await;
                let request = h
                    .request("/openai/v1/chat/completions", chat_stream_request())
                    .send();
                let barrier = async {
                    started.await.unwrap();
                    reservation(&h).await;
                    release.send(()).unwrap();
                };
                let (r, ()) = tokio::join!(request, barrier);
                let r = r.unwrap();
                let status = r.status();
                let bytes = r.bytes().await;
                let passed = status == 200
                    && bytes.is_ok_and(|b| b == native)
                    && native_error_accounting(&h).await;
                h.close().await;
                if !passed {
                    violations.push((named, prefix, done));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "native 오류 wire/failed unknown 위반 (named,prefix,done): {violations:?}"
    );
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL and fixed TEST_FNM_DIR/SDK"]
async fn chat_native_sse_error_fixed_sdk_consumes_typed_api_error_not_eof() {
    use futures_util::FutureExt;
    let mut violations = Vec::new();
    for named in [false, true] {
        let h = Harness::with_config(configuration_with(true, "max_completion_tokens")).await;
        strict(&h).await;
        set(
            &h,
            &(chat_start() + &chat_error_event(named, 50000, "\r\n")),
            true,
        )
        .await;
        let result = std::panic::AssertUnwindSafe(sdk(
            &h,
            if named {
                "chat_native_error_named"
            } else {
                "chat_native_error"
            },
            200,
            "failed",
            &h.token,
        ))
        .catch_unwind()
        .await;
        let accounting = native_error_accounting(&h).await;
        h.close().await;
        if result.is_err() || !accounting {
            violations.push(named);
        }
    }
    assert!(
        violations.is_empty(),
        "native APIError 전용 소비/회계 위반 named={violations:?}"
    );
}

async fn chat_native_error_fragment_case(native: &str, fragment: usize, valid: bool) -> bool {
    let h = Harness::with_config(configuration_with(true, "max_completion_tokens")).await;
    strict(&h).await;
    let (tx, stopped) = h.provider.stream().await;
    let prefix = chat_start();
    tx.send(Bytes::from(prefix.clone())).await.unwrap();
    let response = h
        .request("/openai/v1/chat/completions", chat_stream_request())
        .send()
        .await
        .unwrap();
    let status = response.status();
    reservation(&h).await;
    let producer = async {
        for part in native.as_bytes().chunks(fragment) {
            if tx.send(Bytes::copy_from_slice(part)).await.is_err() {
                break;
            }
        }
        drop(tx);
    };
    let (bytes, ()) = tokio::join!(response.bytes(), producer);
    let upstream_closed = tokio::time::timeout(std::time::Duration::from_secs(2), stopped)
        .await
        .is_ok_and(|r| r.is_ok());
    let accounting = native_error_accounting(&h).await;
    let wire = status == 200
        && if valid {
            bytes.is_ok_and(|b| b == prefix + native)
        } else {
            bytes.is_err()
        };
    h.close().await;
    wire && accounting && upstream_closed
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_native_sse_error_exact_wire_limit_fragmented_crlf_and_cr_only() {
    let mut violations = Vec::new();
    for named in [false, true] {
        for ending in ["\n", "\r\n", "\r"] {
            for size in [65535, 65536, 65537] {
                let base = chat_error_event(named, 0, ending);
                let raw = chat_error_event(named, size - base.len(), ending);
                assert_eq!(raw.len(), size);
                // 선택 LF를 마지막 별도 frame으로도 보내 오류 전체 wire bytes를 검사한다.
                for fragment in [16 * 1024, raw.len() - 1] {
                    if !chat_native_error_fragment_case(&raw, fragment, size <= 65536).await {
                        violations.push((named, ending.len(), size, fragment));
                    }
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "64KiB wire/분할CRLF 경계 위반(named,endingbytes,size,fragment): {violations:?}"
    );
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_native_sse_error_one_byte_fragment_and_invalid_shape_never_settle() {
    let mut violations = Vec::new();
    for named in [false, true] {
        let raw = chat_error_event(named, 32, "\r\n") + "data: [DONE]\r\n\r\n";
        if !chat_native_error_fragment_case(&raw, 1, true).await {
            violations.push(usize::from(named));
        }
    }
    let error = chat_error_event(false, 32, "\n");
    let mut invalid = vec![
        "data: {\"error\":null}\n\n".to_string(),
        "event: error\ndata: {\"error\":{\"message\":0,\"type\":\"server_error\"}}\n\n".to_string(),
        "data: {\"error\":{\"message\":\"x\",\"type\":[],\"code\":null}}\n\n".to_string(),
        "data: {\"error\":{\"message\":\"x\",\"message\":\"y\",\"type\":\"server_error\"}}\n\n"
            .to_string(),
        "data: {\"error\":{\"message\":\"x\",\"type\":\"server_error\",\"param\":{}}}\n\n"
            .to_string(),
        "event: response.failed\n".to_string() + &error,
        error.clone() + &error,
        error.clone() + &chat_native(true),
        error.clone() + "data: [DONE]\n\ndata: [DONE]\n\n",
        error[..error.len() - 2].to_string(),
    ];
    // 성공usage를 먼저 관측해도 native 오류 뒤에는 비용/성공 terminal을 복원하지 않는다.
    let prior_usage = chat_native(true).replace("data: [DONE]\n\n", "") + &error;
    if !chat_native_error_fragment_case(&prior_usage, 17, true).await {
        violations.push(2);
    }
    for (i, raw) in invalid.drain(..).enumerate() {
        if !chat_native_error_fragment_case(&raw, 7, false).await {
            violations.push(i + 3);
        }
    }
    assert!(
        violations.is_empty(),
        "native 오류 shape/sequence/unknown 위반 번호: {violations:?}"
    );
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_native_sse_error_invalid_first_event_is_sanitized_h0_502() {
    let h = Harness::with_config(configuration_with(true, "max_completion_tokens")).await;
    strict(&h).await;
    set(
        &h,
        "event: error\ndata: {\"error\":{\"message\":17,\"type\":\"server_error\"}}\n\n",
        true,
    )
    .await;
    let r = h
        .request("/openai/v1/chat/completions", chat_stream_request())
        .send()
        .await
        .unwrap();
    let status = r.status();
    let code = r
        .headers()
        .get("x-gateway-error-code")
        .is_some_and(|v| v == "upstream_protocol_error");
    let body = r.bytes().await.unwrap();
    let envelope: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let sanitized = envelope["error"]["code"] == "gateway_upstream_protocol_error"
        && envelope["error"]["message"].is_string()
        && envelope["error"]["message"] != "17";
    let accounting = native_error_accounting(&h).await;
    h.close().await;
    assert!(
        status == 502 && code && sanitized && accounting,
        "기형 첫오류 H0/unknown 분류"
    );
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL and fixed TEST_FNM_DIR/SDK"]
async fn chat_native_sse_error_sdk_abort_of_stalled_upstream_preserves_failed_terminal() {
    use futures_util::FutureExt;
    let mut violations = Vec::new();
    for named in [false, true] {
        for prefix in [false, true] {
            let h = Harness::with_config(configuration_with(true, "max_completion_tokens")).await;
            strict(&h).await;
            let (tx, stopped) = h.provider.stream().await;
            let native = if prefix { chat_start() } else { String::new() }
                + &chat_error_event(named, 50000, "\r\n");
            tx.send(Bytes::from(native)).await.unwrap();
            // EOF를 보내지 않는다. SDK가 정확 native APIError를 읽은 뒤 abort해야 한다.
            let result = std::panic::AssertUnwindSafe(sdk(
                &h,
                if named {
                    "chat_native_error_named"
                } else {
                    "chat_native_error"
                },
                200,
                "failed",
                &h.token,
            ))
            .catch_unwind()
            .await;
            let closed = tokio::time::timeout(std::time::Duration::from_secs(2), stopped)
                .await
                .is_ok_and(|r| r.is_ok());
            drop(tx);
            let accounting = native_error_accounting(&h).await;
            h.close().await;
            if result.is_err() || !closed || !accounting {
                violations.push((named, prefix));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "SDK native 오류 뒤 취소가 failed terminal을 덮음(named,prefix): {violations:?}"
    );
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_native_sse_error_stalled_tail_cannot_extend_original_transport_deadline() {
    let cfg = configuration_options(
        true,
        "max_completion_tokens",
        "\nlimits:\n  transport_timeout_ms: 1500\n  request_timeout_ms: 6500\n  body_timeout_ms: 1500\n  body_idle_timeout_ms: 1500\n",
    );
    let h = Harness::with_config(cfg).await;
    strict(&h).await;
    let (tx, stopped) = h.provider.stream().await;
    let error = chat_error_event(true, 50, "\r\n");
    tx.send(Bytes::from(error.clone())).await.unwrap();
    let response = h
        .request("/openai/v1/chat/completions", chat_stream_request())
        .send()
        .await
        .unwrap();
    let status = response.status();
    reservation(&h).await;
    // 유효 error 뒤 EOF/DONE를 영원히 기다리는 Provider도 원래 절대기한으로 종료한다.
    let received = tokio::time::timeout(std::time::Duration::from_secs(3), response.bytes()).await;
    let ended = matches!(received, Ok(Err(_)));
    let closed = tokio::time::timeout(std::time::Duration::from_secs(1), stopped)
        .await
        .is_ok_and(|r| r.is_ok());
    let accounting = native_error_accounting(&h).await;
    let gap:i64=sqlx::query_scalar("SELECT extract(epoch FROM execution_deadline-transport_deadline)::bigint FROM gw_requests WHERE dispatch_authorized_at IS NOT NULL").fetch_one(h.db.store.pool()).await.unwrap();
    drop(tx);
    h.close().await;
    assert!(
        status == 200 && ended && closed && accounting && gap == 5,
        "오류 후 고정기한/failed/마감5초/자원반환"
    );
}

async fn chat_nonstream_error_metadata_observation(
    native: &str,
    status: u16,
    reason: &str,
) -> bool {
    let h = Harness::with_config(configuration_with(true, "max_completion_tokens")).await;
    strict(&h).await;
    set(&h, native, false).await;
    let (started, release) = h.provider.hold_response().await;
    let request = h.request("/openai/v1/chat/completions", CHAT).send();
    let barrier = async {
        started.await.unwrap();
        let row = sqlx::query("SELECT r.id,r.phase='dispatching' AND r.accounting_state='pending' AND b.remaining_reservation_usd=0.004296 AND b.remaining_reserved_tokens=4196 AS reserved FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id").fetch_one(h.db.store.pool()).await.unwrap();
        let id = row.get::<uuid::Uuid, _>("id");
        let reserved = row.get::<bool, _>("reserved");
        release.send(()).unwrap();
        (id, reserved)
    };
    let (response, (id, reserved)) = tokio::join!(request, barrier);
    let response = response.unwrap();
    let same_id = response.headers()["x-gateway-request-id"].to_str().unwrap() == id.to_string();
    let wire_status = response.status().as_u16();
    let retry = response
        .headers()
        .get("x-should-retry")
        .is_some_and(|v| v == "false");
    let no_dispatch_claim = !response.headers().contains_key("x-gateway-dispatch-state");
    let gateway_error = response
        .headers()
        .get("x-gateway-error-code")
        .is_some_and(|v| v == reason);
    let bytes = response.bytes().await.unwrap();
    let wire = if status == 200 {
        bytes == native
    } else {
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["error"]["code"]
            == format!("gateway_{reason}")
            && gateway_error
    };
    h.runtime.wait_requests().await;
    let observed_reason: String =
        sqlx::query_scalar("SELECT reason_code FROM gw_requests WHERE id=$1")
            .bind(id)
            .fetch_one(h.db.store.pool())
            .await
            .unwrap();
    let accounting = native_error_accounting(&h).await;
    let calls = h.provider.calls.lock().await;
    let single_wire = calls.len() == 1
        && calls[0].body == CHAT
        && calls[0].path == "/v1/chat/completions"
        && calls[0].headers["authorization"] == "Bearer synthetic-company-key";
    drop(calls);
    let nonstream_released = h.runtime.available_nonstream() == 4;
    h.close().await;
    reserved
        && same_id
        && wire_status == status
        && retry
        && no_dispatch_claim
        && wire
        && observed_reason == reason
        && accounting
        && single_wire
        && nonstream_released
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_nonstream_error_metadata_conflict_is_protocol_unknown() {
    let mut violations = Vec::new();
    for (i, error) in [
        r#"{"message":"synthetic failure","type":"server_error","param":null,"code":"server_error"}"#,
        "{}", "[]", "true", "0", r#""synthetic""#,
        r#"{"message":0,"type":"server_error"}"#,
        r#"{"message":"synthetic failure","type":null}"#,
    ].iter().enumerate() {
        for first in [true, false] {
            let base = chat_native(false);
            let native = if first {
                format!("{{\"error\":{error},{}", &base[1..])
            } else {
                format!("{},\"error\":{error}}}", &base[..base.len()-1])
            };
            if !chat_nonstream_error_metadata_observation(&native, 502, "upstream_protocol_error").await {
                violations.push((i, first));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "R4 모순 error+완전usage의 protocol/unknown/예약/차단/원본송신 위반: {violations:?}"
    );
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_nonstream_error_metadata_native_failure_shape_and_size_matrix() {
    let mut violations = Vec::new();
    for (i, native) in [
        r#"{"error":{"message":"synthetic failure","type":"server_error"}}"#.to_owned(),
        r#"{"error":{"code":null,"param":null,"type":"server_error","message":"synthetic failure"}}"#.to_owned(),
        serde_json::json!({"error":{"message":"x".repeat(18000),"type":"x".repeat(18000),"param":"x".repeat(18000),"code":"x".repeat(1000)}}).to_string(),
    ].iter().enumerate() {
        if !chat_nonstream_error_metadata_observation(native, 200, "provider_error").await {
            violations.push(i);
        }
    }
    let base = r#"{"error":{"message":"synthetic failure","type":"server_error","param":null,"code":null}}"#;
    for size in [65535, 65536, 65537] {
        let native = base.to_owned() + &" ".repeat(size - base.len());
        let (status, reason) = if size <= 65536 {
            (200, "provider_error")
        } else {
            (502, "upstream_response_too_large")
        };
        if !chat_nonstream_error_metadata_observation(&native, status, reason).await {
            violations.push(size);
        }
    }
    for (i, error) in [
        "{}",
        "[]",
        "true",
        "0",
        r#""synthetic""#,
        r#"{"message":0,"type":"server_error"}"#,
        r#"{"message":"synthetic failure","type":null}"#,
        r#"{"message":"synthetic failure","type":"server_error","param":{}}"#,
        r#"{"message":"synthetic failure","type":"server_error","code":[]}"#,
        r#"{"message":"synthetic failure","type":"server_error","unknown":0}"#,
    ]
    .iter()
    .enumerate()
    {
        let native = format!("{{\"error\":{error}}}");
        if !chat_nonstream_error_metadata_observation(&native, 502, "upstream_protocol_error").await
        {
            violations.push(100 + i);
        }
    }
    assert!(
        violations.is_empty(),
        "R4 native 실패/기형/64KiB 경계 위반(case): {violations:?}"
    );
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn chat_nonstream_error_metadata_missing_or_null_settles_and_releases_reservations() {
    let base = chat_native(false);
    let mut violations = Vec::new();
    for (i, native) in [
        base.clone(),
        format!("{{\"error\":null,{}", &base[1..]),
        format!("{},\"error\":null}}", &base[..base.len() - 1]),
    ]
    .iter()
    .enumerate()
    {
        let h = Harness::with_config(configuration_with(true, "max_completion_tokens")).await;
        strict(&h).await;
        set(&h, native, false).await;
        let (started, release) = h.provider.hold_response().await;
        let request = h.request("/openai/v1/chat/completions", CHAT).send();
        let barrier = async {
            started.await.unwrap();
            reservation(&h).await;
            release.send(()).unwrap();
        };
        let (response, ()) = tokio::join!(request, barrier);
        let response = response.unwrap();
        let status = response.status();
        let id: uuid::Uuid = response.headers()["x-gateway-request-id"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let wire = response.bytes().await.unwrap();
        h.runtime.wait_requests().await;
        let settled: bool = sqlx::query_scalar("SELECT r.outcome='completed' AND r.accounting_state='settled' AND r.cost_usd=0.000120 AND r.reason_code='completed' AND t.unknown_count=0 AND t.reserved_usd=0 AND b.remaining_reservation_usd=0 AND b.remaining_reserved_tokens=0 AND w.reserved_tokens=0 AND w.settled_tokens=120 FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id JOIN gw_usage_totals t USING(scope_id) JOIN gw_rate_windows w USING(scope_id) WHERE r.id=$1").bind(id).fetch_one(h.db.store.pool()).await.unwrap();
        set(&h, &base, false).await;
        let follow = h
            .request("/openai/v1/chat/completions", CHAT)
            .send()
            .await
            .unwrap();
        let follow_status = follow.status();
        let follow_wire = follow.bytes().await.unwrap();
        let s = state(&h).await;
        let calls = h.provider.calls.lock().await.len();
        h.close().await;
        if status != 200
            || wire != native
            || !settled
            || follow_status != 200
            || follow_wire != base
            || s != (2, 0, "0.000240000000".into(), 200, 40)
            || calls != 2
        {
            violations.push(i);
        }
    }
    assert!(
        violations.is_empty(),
        "R4 정상 누락/null 양성/다음dispatch 위반: {violations:?}"
    );
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
            "chat",
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
            "embeddings",
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

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL and fixed SDK"]
async fn s9_stream_syntax_and_limit_have_distinct_h0_h1_and_sdk_causes() {
    for (bad, code) in [
        ("data: {bad}\n\n".to_string(), "stream_parse_error"),
        (format!("event: {}\n", "x".repeat(129)), "stream_limit"),
    ] {
        for mode in ["h0", "h1", "sdk"] {
            let h = Harness::with_config(configuration_with(true, "max_completion_tokens")).await;
            strict(&h).await;
            let body = CHAT.replacen(
                '{',
                "{\"stream\":true,\"stream_options\":{\"include_usage\":true},",
                1,
            );
            let prefix = chat_start();
            let (tx, stopped) = h.provider.stream().await;
            if mode == "h1" {
                tx.send(Bytes::from(prefix.clone())).await.unwrap();
            } else {
                tx.send(Bytes::from(bad.clone())).await.unwrap();
            }
            if mode == "sdk" {
                error_compat::error_stream_sdk(&h.base, &h.token, "chat", 502, code, true).await;
            } else {
                let mut r = h
                    .request("/openai/v1/chat/completions", body)
                    .send()
                    .await
                    .unwrap();
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
    let h = Harness::with_config(configuration_with(true, "max_completion_tokens")).await;
    strict(&h).await;

    let body = CHAT.replacen(
        '{',
        "{\"stream\":true,\"stream_options\":{\"include_usage\":true},",
        1,
    );
    let native = chat_native(true);
    let (tx, stopped) = h.provider.stream().await;
    tx.send(Bytes::from(native.clone())).await.unwrap();
    let mut r = h
        .request("/openai/v1/chat/completions", body.clone())
        .send()
        .await
        .unwrap();
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
    let next = h
        .request("/openai/v1/chat/completions", body)
        .send()
        .await
        .unwrap();
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
    let h = Harness::with_config(configuration_with(true, "max_completion_tokens")).await;
    strict(&h).await;

    let body = CHAT.replacen(
        '{',
        "{\"stream\":true,\"stream_options\":{\"include_usage\":true},",
        1,
    );
    let native = chat_native(true).replacen(
        "\"object\":\"chat.completion.chunk\"",
        "\"service_tier\":\"unverified\",\"object\":\"chat.completion.chunk\"",
        1,
    );
    let (tx, stopped) = h.provider.stream().await;
    tx.send(Bytes::from(native.clone())).await.unwrap();
    let mut r = h
        .request("/openai/v1/chat/completions", body.clone())
        .send()
        .await
        .unwrap();
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
            "0.004296000000".into(),
            4196,
            1
        )
    );
    let blocked = h
        .request("/openai/v1/chat/completions", body)
        .send()
        .await
        .unwrap();
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
    let h = Harness::with_config(configuration_with(true, "max_completion_tokens")).await;
    strict(&h).await;
    (
        h,
        "/openai/v1/chat/completions",
        CHAT.replacen(
            '{',
            "{\"stream\":true,\"stream_options\":{\"include_usage\":true},",
            1,
        ),
        chat_native(true),
        chat_start(),
        "0.000120000000",
    )
}

fn s9_native_error_tail(length: usize) -> String {
    chat_error_event(true, length, "\n")
}
