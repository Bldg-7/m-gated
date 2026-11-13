#[path = "support/postgres.rs"]
mod postgres;
#[path = "support/provider.rs"]
#[allow(dead_code)]
mod provider;
#[path = "support/sso.rs"]
#[allow(dead_code)]
mod sso;
mod support;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use m_gated::{
    auth::{AccessSettings, SsoAuthenticator, SsoVerifier, jwks::JwksCache},
    catalog::{CatalogService, SourceUrls},
    config::{self, ConfigInputs},
    http::{self, AppState},
    ledger::PgLedger,
    runtime::{Dependency, Runtime},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use uuid::Uuid;

const CATALOG_YAML: &str = "    catalog:
      api: messages
      fixture_version: text-v1
      features: [cache]
      max_body_bytes: 8388608
      max_response_bytes: 16777216
      model_prefixes: [synthetic-]
";
fn config() -> config::ValidatedConfig {
    let start = support::YAML.find("    profiles:").unwrap();
    let yaml = format!("{}{}", &support::YAML[..start], CATALOG_YAML);
    let mut env = support::environment();
    env.insert(
        "ALLOWED_EMAILS".into(),
        "alice@example.test,bob@example.test".into(),
    );
    env.insert("ADMIN_EMAILS".into(), "alice@example.test".into());
    config::load(ConfigInputs {
        yaml: &yaml,
        file_environment: None,
        environment: &env,
    })
    .unwrap()
}

/// 공개 가격 목록 세 곳을 흉내 내는 합성 서버. 내려주는 JSON은 시험 중에 바꿀 수 있다.
#[derive(Default)]
struct Sources {
    models_dev: Mutex<Option<Value>>,
    litellm: Mutex<Option<Value>>,
    openrouter: Mutex<Option<Value>>,
}
async fn serve(sources: Arc<Sources>) -> (SourceUrls, tokio::task::JoinHandle<()>) {
    use axum::{extract::State, routing::get};
    async fn answer(v: Option<Value>) -> axum::response::Response {
        use axum::response::IntoResponse;
        match v {
            Some(v) => axum::Json(v).into_response(),
            None => axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
        }
    }
    let app = axum::Router::new()
        .route(
            "/models-dev",
            get(|State(s): State<Arc<Sources>>| async move {
                let v = s.models_dev.lock().unwrap().clone();
                answer(v).await
            }),
        )
        .route(
            "/litellm",
            get(|State(s): State<Arc<Sources>>| async move {
                let v = s.litellm.lock().unwrap().clone();
                answer(v).await
            }),
        )
        .route(
            "/openrouter",
            get(|State(s): State<Arc<Sources>>| async move {
                let v = s.openrouter.lock().unwrap().clone();
                answer(v).await
            }),
        )
        .with_state(sources);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (
        SourceUrls {
            models_dev: format!("{base}/models-dev").parse().unwrap(),
            litellm: format!("{base}/litellm").parse().unwrap(),
            openrouter: format!("{base}/openrouter").parse().unwrap(),
        },
        task,
    )
}
fn models_dev(input: f64) -> Value {
    json!({"anthropic":{"models":{
        "synthetic-text":{"cost":{"input":input,"output":2,"cache_read":0.1,"cache_write":1.25},"limit":{"context":4096,"output":1024}},
        "synthetic-tiered":{"cost":{"input":1,"output":2,"cache_read":0.1,"cache_write":1.25,"tiers":[{"tier":{"type":"context","size":2048}}]},"limit":{"context":4096,"output":1024}},
        "other-model":{"cost":{"input":1,"output":2,"cache_read":0.1,"cache_write":1.25},"limit":{"context":4096,"output":1024}}}}})
}
fn litellm() -> Value {
    json!({"synthetic-text":{"litellm_provider":"anthropic","mode":"chat","input_cost_per_token":0.000001,"output_cost_per_token":0.000002,"cache_read_input_token_cost":1e-7,"cache_creation_input_token_cost":0.00000125,"max_input_tokens":4096,"max_output_tokens":1024},
           "synthetic-extra":{"litellm_provider":"anthropic","mode":"chat","input_cost_per_token":0.000003,"output_cost_per_token":0.000004,"cache_read_input_token_cost":3e-7,"cache_creation_input_token_cost":0.00000375,"max_input_tokens":4096,"max_output_tokens":1024}})
}
fn openrouter() -> Value {
    json!({"data":[{"id":"anthropic/synthetic-text","context_length":4096,"top_provider":{"max_completion_tokens":1024},"pricing":{"prompt":"0.000001","completion":"0.000002","input_cache_read":"0.0000001","input_cache_write":"0.00000125"}}]})
}

struct Harness {
    db: postgres::TestDb,
    router: axum::Router,
    service: Arc<CatalogService>,
    sources: Arc<Sources>,
    alice: String,
    bob: String,
    provider: Arc<provider::Provider>,
    _servers: (provider::Server, sso::Server, tokio::task::JoinHandle<()>),
}
impl Harness {
    async fn new() -> Self {
        let cfg = config();
        let db = postgres::TestDb::new().await;
        let fake = sso::FakeJwks::new(vec![sso::Signer::shared().jwk.clone()]);
        let jwks = fake.serve().await;
        let verifier = SsoVerifier::new(
            cfg.auth(),
            Arc::new(JwksCache::loopback_for_test(jwks.uri.clone()).unwrap()),
        );
        let alice = sso::Signer::shared().sign(&sso::claims(), "key1");
        let mut bob_claims = sso::claims();
        bob_claims["sub"] = json!("human-2");
        bob_claims["email"] = json!("bob@example.test");
        let bob = sso::Signer::shared().sign(&bob_claims, "key1");
        let auth = Arc::new(SsoAuthenticator::new(&cfg, verifier, db.store.clone()));
        let sources = Arc::new(Sources::default());
        *sources.models_dev.lock().unwrap() = Some(models_dev(1.0));
        *sources.litellm.lock().unwrap() = Some(litellm());
        *sources.openrouter.lock().unwrap() = Some(openrouter());
        let (urls, task) = serve(sources.clone()).await;
        let service = CatalogService::for_test(&cfg, db.store.clone(), urls);
        service.refresh().await.unwrap();
        let api = Arc::new(
            m_gated::admin::DashboardApi::new(&cfg, auth.clone(), db.store.clone())
                .with_catalog(service.clone()),
        );
        let runtime = Runtime::new(cfg.limits().clone()).unwrap();
        runtime.start_http().unwrap();
        for d in [
            Dependency::Database,
            Dependency::Jwks,
            Dependency::Schema,
            Dependency::Ui,
        ] {
            runtime.record_health(d, true, tokio::time::Instant::now());
        }
        let p = provider::Provider::new();
        let provider_server = p.serve().await;
        let client = m_gated::providers::anthropic::AnthropicClient::loopback_for_test(
            provider_server.base.clone(),
            "synthetic-company-key".into(),
        )
        .unwrap();
        let ledger = Arc::new(PgLedger::new(
            db.store.clone(),
            AccessSettings::from_config(&cfg),
            m_gated::gateway::auth_config_digest(&cfg),
        ));
        let gateway = Arc::new(
            m_gated::gateway::InferenceGateway::assemble(&cfg.providers()[0], auth, ledger, client)
                .with_catalog(service.cache()),
        );
        let router = http::router_with_components(
            AppState {
                runtime: runtime.clone(),
            },
            Some(gateway),
            api.routes(runtime),
        );
        Self {
            db,
            router,
            service,
            sources,
            alice,
            bob,
            provider: p,
            _servers: (provider_server, jwks, task),
        }
    }
    async fn call(
        &self,
        method: &str,
        path: &str,
        token: &str,
        command: Option<Uuid>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {token}"))
            .header("anthropic-version", "2023-06-01");
        if let Some(c) = command {
            request = request.header("x-gateway-command-id", c.to_string());
        }
        if body.is_some() {
            request = request.header("content-type", "application/json");
        }
        let request = request
            .body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let code = response
            .headers()
            .get("x-gateway-error-code")
            .map(|v| v.to_str().unwrap().to_owned());
        let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        let mut value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if let (Some(code), Value::Object(map)) = (code, &mut value) {
            map.insert("gateway_error_code".into(), json!(code));
        }
        (status, value)
    }
    async fn sync(&self, command: Uuid) -> (u16, Value) {
        self.call(
            "POST",
            "/gateway/admin/model-catalog/sync",
            &self.alice,
            Some(command),
            None,
        )
        .await
    }
    async fn approve(&self, proposal: &str, expected: &str, models: &[&str]) -> (u16, Value) {
        let items: Vec<Value> = models
            .iter()
            .map(|m| json!({"provider":"anthropic","model":m}))
            .collect();
        self.call(
            "POST",
            "/gateway/admin/model-catalog/approve",
            &self.alice,
            Some(Uuid::new_v4()),
            Some(json!({"proposal_id":proposal,"expected_version":expected,"items":items})),
        )
        .await
    }
}
fn item<'a>(proposal: &'a Value, model: &str) -> &'a Value {
    proposal["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["model"] == model)
        .unwrap_or_else(|| panic!("{model}"))
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn admin_sync_shows_diff_and_only_approved_items_become_billable_models() {
    let h = Harness::new().await;
    // 승인 전: 카탈로그 모델로는 추론할 수 없다.
    let (status, view) = h
        .call("GET", "/gateway/admin/model-catalog", &h.alice, None, None)
        .await;
    assert_eq!(status, 200);
    assert_eq!(view["version"], "0");
    assert_eq!(
        view["providers"][0]["model_prefixes"],
        json!(["synthetic-"])
    );
    let body = json!({"model":"synthetic-text","max_tokens":100,"messages":[{"role":"user","content":"x"}]});
    let (status, _) = h
        .call("POST", "/v1/messages", &h.alice, None, Some(body.clone()))
        .await;
    assert_eq!(status, 400, "승인 전에는 profile 이 없다");

    // 관리자만 동기화할 수 있다.
    let (status, _) = h.sync_as_bob().await;
    assert_eq!(status, 403);

    // 동기화는 변경안만 만든다. 같은 명령 재전송은 같은 변경안을 돌려준다.
    let command = Uuid::new_v4();
    let (status, synced) = h.sync(command).await;
    assert_eq!(status, 201, "{synced}");
    let proposal = &synced["proposal"];
    let proposal_id = synced["proposal_id"].as_str().unwrap().to_owned();
    assert_eq!(proposal["base_version"], 0);
    assert!(
        proposal["sources"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["ok"] == true)
    );
    let text = item(proposal, "synthetic-text");
    assert_eq!(text["status"], "new");
    assert_eq!(text["agree"], true);
    assert_eq!(text["proposed"]["rates"]["input"], "1.000000000000");
    assert_eq!(text["sources"].as_object().unwrap().len(), 3);
    assert_eq!(
        item(proposal, "synthetic-tiered")["proposed"]["max_input_tokens"],
        2048
    );
    assert_eq!(item(proposal, "synthetic-extra")["status"], "new");
    assert!(
        proposal["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["model"] != "other-model")
    );
    let (status, replay) = h.sync(command).await;
    assert_eq!(status, 201);
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["proposal_id"], synced["proposal_id"]);
    assert_eq!(replay["proposal"], synced["proposal"]);
    let (status, view) = h
        .call("GET", "/gateway/admin/model-catalog", &h.alice, None, None)
        .await;
    assert_eq!(
        (status, view["version"].clone()),
        (200, json!("0")),
        "동기화만으로는 적용되지 않는다"
    );

    // 고른 항목만 적용된다. 버전이 다르면 거절한다.
    let (status, conflict) = h.approve(&proposal_id, "1", &["synthetic-text"]).await;
    assert_eq!(
        (status, conflict["error"]["code"].clone()),
        (409, json!("version_conflict"))
    );
    let (status, bad) = h.approve(&proposal_id, "0", &["other-model"]).await;
    assert_eq!(
        (status, bad["error"]["code"].clone()),
        (400, json!("invalid_request"))
    );
    let (status, approved) = h
        .approve(&proposal_id, "0", &["synthetic-text", "synthetic-tiered"])
        .await;
    assert_eq!(status, 200, "{approved}");
    assert_eq!(approved["result_version"], "1");
    let snapshot = h.service.cache().snapshot();
    assert_eq!(snapshot.version, 1);
    let models: Vec<_> = snapshot
        .profiles
        .iter()
        .map(|(_, p)| p.model.clone())
        .collect();
    assert_eq!(models, ["synthetic-text", "synthetic-tiered"]);
    assert_eq!(snapshot.profiles[0].1.pricing_version, "catalog-v1");

    // 승인된 모델로 실제 추론이 나가고, 그 시점 가격으로 장부에 남는다.
    sqlx::query("UPDATE gw_policies SET allow_rules=$1 WHERE principal_id=(SELECT id FROM gw_principals WHERE email='alice@example.test') AND key_id IS NULL")
        .bind(json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}]))
        .execute(h.db.store.pool()).await.unwrap();
    let (status, answer) = h
        .call("POST", "/v1/messages", &h.alice, None, Some(body.clone()))
        .await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(h.provider.calls.lock().await.len(), 1);
    let (cost, pricing): (String, Value) = sqlx::query_as(
        "SELECT cost_usd::text,pricing_snapshot FROM gw_requests ORDER BY admitted_at DESC LIMIT 1",
    )
    .fetch_one(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(cost, "0.000140000000", "입력 100×1 + 출력 20×2 (USD/1M)");
    assert!(pricing.to_string().contains("catalog-v1"));

    // 이전 버전을 기준으로 만든 변경안은 쓸 수 없다.
    let (status, stale) = h.approve(&proposal_id, "1", &["synthetic-extra"]).await;
    assert_eq!(
        (status, stale["error"]["code"].clone()),
        (409, json!("catalog_proposal_stale"))
    );

    // 가격이 바뀌면 이전 값과 새 값을 보여 준다. 출처 사이 불일치도 표시한다.
    *h.sources.models_dev.lock().unwrap() = Some(models_dev(1.5));
    let (_, synced) = h.sync(Uuid::new_v4()).await;
    let changed = item(&synced["proposal"], "synthetic-text");
    assert_eq!(changed["status"], "changed");
    assert_eq!(changed["current"]["rates"]["input"], "1.000000000000");
    assert_eq!(changed["proposed"]["rates"]["input"], "1.500000000000");
    assert_eq!(changed["agree"], false);
    assert_eq!(
        item(&synced["proposal"], "synthetic-tiered")["status"],
        "unchanged"
    );
    let (status, _) = h
        .approve(
            synced["proposal_id"].as_str().unwrap(),
            "1",
            &["synthetic-tiered"],
        )
        .await;
    assert_eq!(status, 400, "바뀌지 않은 항목은 고를 수 없다");
    let (status, _) = h
        .approve(
            synced["proposal_id"].as_str().unwrap(),
            "1",
            &["synthetic-text"],
        )
        .await;
    assert_eq!(status, 200);
    let (status, second) = h
        .call("POST", "/v1/messages", &h.alice, None, Some(body.clone()))
        .await;
    assert_eq!(status, 200, "{second}");
    let costs: Vec<String> =
        sqlx::query_scalar("SELECT cost_usd::text FROM gw_requests ORDER BY admitted_at")
            .fetch_all(h.db.store.pool())
            .await
            .unwrap();
    assert_eq!(
        costs,
        ["0.000140000000", "0.000190000000"],
        "앞 요청은 이전 가격 그대로"
    );

    // 출처에서 사라진 모델은 승인해야 제거된다.
    let mut gone = models_dev(1.5);
    gone["anthropic"]["models"]
        .as_object_mut()
        .unwrap()
        .remove("synthetic-tiered");
    *h.sources.models_dev.lock().unwrap() = Some(gone);
    let (_, synced) = h.sync(Uuid::new_v4()).await;
    assert_eq!(
        item(&synced["proposal"], "synthetic-tiered")["status"],
        "missing"
    );
    let (status, _) = h
        .approve(
            synced["proposal_id"].as_str().unwrap(),
            "2",
            &["synthetic-tiered"],
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(h.service.cache().snapshot().profiles.len(), 1);

    // 출처를 하나도 받지 못하면 변경안을 만들지 않는다.
    *h.sources.models_dev.lock().unwrap() = None;
    *h.sources.litellm.lock().unwrap() = None;
    *h.sources.openrouter.lock().unwrap() = None;
    let (status, failed) = h.sync(Uuid::new_v4()).await;
    assert_eq!(
        (status, failed["error"]["code"].clone()),
        (502, json!("catalog_sources_unavailable"))
    );

    let audits: Vec<String> = sqlx::query_scalar(
        "SELECT operation FROM gw_audit_events WHERE target_kind='catalog' ORDER BY id",
    )
    .fetch_all(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(audits, ["approve_model_catalog"; 3]);
    let (status, view) = h
        .call("GET", "/gateway/admin/model-catalog", &h.alice, None, None)
        .await;
    assert_eq!(
        (
            status,
            view["version"].clone(),
            view["approved_by_email"].clone()
        ),
        (200, json!("3"), json!("alice@example.test"))
    );
    h.db.close().await;
}
impl Harness {
    async fn sync_as_bob(&self) -> (u16, Value) {
        self.call(
            "POST",
            "/gateway/admin/model-catalog/sync",
            &self.bob,
            Some(Uuid::new_v4()),
            None,
        )
        .await
    }
}
