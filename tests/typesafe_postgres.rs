#![allow(dead_code)]
#[path = "support/finance.rs"]
mod finance;
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
use std::sync::Arc;

/// TypeSafe 프로필. 입력 100만 토큰당 0.042달러, 출력은 무료다.
fn configuration() -> config::ValidatedConfig {
    let yaml = include_str!("support/config.yaml")
        .replace("provider: anthropic", "provider: typesafe")
        .replace("https://api.anthropic.com", "https://api.typesafe.ai")
        .replace("model: synthetic-text", "model: jev-latest")
        .replace("api: messages", "api: systemone")
        .replace(
            "          - synthetic-text",
            "          - jev-latest\n          - jev-1.14.0",
        )
        .replace("        max_output_tokens: 1024\n", "")
        .replace("input: '1.000000000000'", "input: '0.042000000000'")
        .replace("output: '2.000000000000'", "output: '0.000000000000'");
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
    provider: Arc<provider::Provider>,
    _provider_server: provider::Server,
    _jwks: sso::Server,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    monitor: tokio::task::JoinHandle<()>,
    base: String,
    token: String,
    runtime: Arc<m_gated::runtime::Runtime>,
}
impl Harness {
    async fn new() -> Self {
        use m_gated::{
            auth::{SsoAuthenticator, SsoVerifier, jwks::JwksCache},
            bootstrap::{self, Dependencies},
            config::ProviderId,
            gateway::InferenceGateway,
            providers::transport::NativeClient,
            runtime::Dependency,
        };
        let cfg = configuration();
        let db = postgres::TestDb::new().await;
        let database = db.store.clone();
        let fake = sso::FakeJwks::new(vec![sso::Signer::shared().jwk.clone()]);
        let jwks = fake.serve().await;
        let verifier = SsoVerifier::new(
            cfg.auth(),
            Arc::new(JwksCache::loopback_for_test(jwks.uri.clone()).unwrap()),
        );
        let token = sso::Signer::shared().sign(&sso::claims(), "key1");
        let identity = verifier
            .verify(&sso::credential(token), 1900000000)
            .await
            .unwrap();
        let current = database
            .current_human(&identity, &AccessSettings::from_config(&cfg))
            .await
            .unwrap();
        sqlx::query("UPDATE gw_policies SET allow_rules=$2 WHERE id=$1")
            .bind(current.policy_id)
            .bind(serde_json::json!([{"provider":"typesafe","model":"jev-latest","capabilities":["systemone"]}]))
            .execute(database.pool())
            .await
            .unwrap();
        let key = finance::virtual_key(&db.store, &cfg, current.id).await;
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
        let provider_server = provider.serve().await;
        let client = NativeClient::provider_loopback_for_test(
            provider_server.base.clone(),
            ProviderId::Typesafe,
            "synthetic-typesafe-company-key".into(),
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
            token: key,
            runtime,
        }
    }
    fn request(&self, body: impl Into<reqwest::Body>) -> reqwest::RequestBuilder {
        reqwest::Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .no_zstd()
            .build()
            .unwrap()
            .post(format!("{}{PATH}", self.base))
            .bearer_auth(&self.token)
            .header("content-type", "application/json")
            .body(body)
    }
    async fn set(&self, status: StatusCode, body: &str) {
        self.provider
            .set(
                status,
                HeaderMap::from_iter([
                    (
                        "content-type".parse().unwrap(),
                        "application/json".parse().unwrap(),
                    ),
                    (
                        "x-typesafe-request-id".parse().unwrap(),
                        "ts-synthetic-request".parse().unwrap(),
                    ),
                    ("retry-after-ms".parse().unwrap(), "1500".parse().unwrap()),
                ]),
                Bytes::copy_from_slice(body.as_bytes()),
            )
            .await;
    }
    /// (요청 수, unknown 수, 비용 합계)
    async fn state(&self) -> (i64, i64, String) {
        self.runtime.wait_requests().await;
        sqlx::query_as("SELECT count(*)::bigint,count(*) FILTER(WHERE accounting_state='unknown')::bigint,COALESCE(sum(cost_usd),0)::text FROM gw_requests")
            .fetch_one(self.db.store.pool())
            .await
            .unwrap()
    }
    async fn close(self) {
        self.monitor.abort();
        let _ = self.monitor.await;
        self.task.abort();
        let _ = self.task.await;
        self.db.store.pool().close().await;
        self.db.close().await;
    }
}
const PATH: &str = "/typesafe/v1/systemone";
const BODY: &str = r#"{"model":"jev-latest","state":{"message":"I was charged twice for my subscription this month. Please refund the extra charge."},"questions":{"billing":{"type":"noul","instructions":"Is this message about billing?"},"tone":{"type":"choice","criteria":{"angry":"An upset message","calm":"A polite message"}}}}"#;
const SUCCESS: &str = r#"{"model":"jev-1.14.0","answers":{"billing":{"type":"noul","noul":0.98},"tone":{"type":"choice","choice":"calm","confidence":0.9,"probabilities":{"angry":0.1,"calm":0.9}}},"usage":{"input_tokens":120,"output_tokens":12}}"#;
fn zstd(data: &[u8]) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest)
}
async fn detail(response: reqwest::Response) -> (u16, String, String) {
    let status = response.status().as_u16();
    let code = response.headers()["x-gateway-error-code"]
        .to_str()
        .unwrap()
        .to_owned();
    let body: serde_json::Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    (status, code, body["detail"].as_str().unwrap().to_owned())
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn typesafe_settles_input_tokens_and_relays_the_native_answer() {
    let h = Harness::new().await;
    h.set(StatusCode::OK, SUCCESS).await;
    let r = h.request(BODY).send().await.unwrap();
    assert_eq!(
        r.status(),
        200,
        "{:?}",
        r.headers().get("x-gateway-error-code")
    );
    assert_eq!(r.headers()["x-typesafe-request-id"], "ts-synthetic-request");
    assert_eq!(r.headers()["retry-after-ms"], "1500");
    assert_eq!(r.bytes().await.unwrap(), SUCCESS.as_bytes());
    // 입력 120토큰 × 0.042달러/100만 토큰. 출력 12토큰은 무료다.
    assert_eq!(h.state().await, (1, 0, "0.000005040000".into()));
    let (provider_request_id, input, output): (String, String, String) = sqlx::query_as("SELECT r.provider_request_id,e.normalized_usage->>'input_tokens',e.normalized_usage->>'output_tokens' FROM gw_requests r JOIN gw_usage_events e ON e.request_id=r.id AND e.kind='final'")
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    assert_eq!(
        (
            provider_request_id.as_str(),
            input.as_str(),
            output.as_str()
        ),
        ("ts-synthetic-request", "120", "12")
    );
    let calls = h.provider.calls.lock().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(
        (calls[0].method.as_str(), calls[0].path.as_str()),
        ("POST", "/v1/systemone")
    );
    assert_eq!(
        calls[0].headers["authorization"],
        "Bearer synthetic-typesafe-company-key"
    );
    assert_eq!(calls[0].body, BODY.as_bytes());
    drop(calls);
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn typesafe_zstd_request_is_checked_and_sent_decompressed() {
    let h = Harness::new().await;
    h.set(StatusCode::OK, SUCCESS).await;
    let r = h
        .request(zstd(BODY.as_bytes()))
        .header("content-encoding", "zstd")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    {
        let calls = h.provider.calls.lock().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].body, BODY.as_bytes());
        assert!(!calls[0].headers.contains_key("content-encoding"));
        assert_eq!(calls[0].headers["content-type"], "application/json");
    }
    // 압축을 풀어 검사하므로, 압축한 본문 안의 이미지도 거절한다.
    let images = BODY.replace(
        r#""questions":"#,
        r#""assets":{"photo":{"type":"image","format":"png","data_base64":"AA=="}},"questions":"#,
    );
    let r = h
        .request(zstd(images.as_bytes()))
        .header("content-encoding", "zstd")
        .send()
        .await
        .unwrap();
    let (status, code, text) = detail(r).await;
    assert_eq!((status, code.as_str()), (400, "unsupported_option"));
    assert!(text.ends_with("gateway_unsupported_option"), "{text}");
    // 깨진 zstd는 400, 다른 압축은 415다. 둘 다 Provider에 보내지 않는다.
    let r = h
        .request(b"not zstd".to_vec())
        .header("content-encoding", "zstd")
        .send()
        .await
        .unwrap();
    assert_eq!(detail(r).await.1, "invalid_request");
    let r = h
        .request(BODY)
        .header("content-encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(detail(r).await.0, 415, "gzip은 받지 않는다");
    assert_eq!(h.provider.calls.lock().await.len(), 1);
    assert_eq!(h.state().await, (1, 0, "0.000005040000".into()));
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn typesafe_images_and_danger_zone_headers_are_rejected_before_dispatch() {
    let h = Harness::new().await;
    let box_question = BODY.replace(r#""type":"noul""#, r#""type":"box""#);
    for request in [
        h.request(box_question),
        h.request(BODY)
            .header("TypeSafe-Danger-Zone-Images", "true"),
        h.request(BODY)
            .header("TypeSafe-Danger-Zone-Long-Context", "true"),
    ] {
        let r = request.send().await.unwrap();
        assert_eq!(r.headers()["x-gateway-dispatch-state"], "not_dispatched");
        assert_eq!(detail(r).await.1, "unsupported_option");
    }
    assert!(h.provider.calls.lock().await.is_empty());
    assert_eq!(h.state().await.0, 0);
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn typesafe_http_error_is_free_and_does_not_block() {
    let h = Harness::new().await;
    h.set(
        StatusCode::TOO_MANY_REQUESTS,
        r#"{"detail":"Rate limit exceeded"}"#,
    )
    .await;
    let r = h.request(BODY).send().await.unwrap();
    assert_eq!(r.status(), 429);
    assert_eq!(r.headers()["retry-after-ms"], "1500");
    assert_eq!(
        r.bytes().await.unwrap(),
        br#"{"detail":"Rate limit exceeded"}"#.as_slice()
    );
    assert_eq!(
        finance::last_settlement(h.db.store.pool()).await,
        finance::no_response()
    );
    h.set(StatusCode::OK, SUCCESS).await;
    let r = h.request(BODY).send().await.unwrap();
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    assert_eq!(h.state().await, (2, 0, "0.000005040000".into()));
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn typesafe_unknown_usage_field_stays_unknown_and_blocks() {
    let h = Harness::new().await;
    h.set(
        StatusCode::OK,
        &SUCCESS.replace(
            r#""output_tokens":12"#,
            r#""output_tokens":12,"image_tokens":300"#,
        ),
    )
    .await;
    let r = h.request(BODY).send().await.unwrap();
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    assert_eq!(h.state().await, (1, 1, "0".into()));
    let r = h.request(BODY).send().await.unwrap();
    let (status, code, _) = detail(r).await;
    assert_eq!((status, code.as_str()), (403, "unsettled_usage_block"));
    assert_eq!(h.provider.calls.lock().await.len(), 1);
    h.close().await;
}
