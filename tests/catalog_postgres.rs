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
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use m_gated::{
    auth::{Authenticator, SsoVerifier, jwks::JwksCache},
    catalog::CatalogService,
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
const ACCESS: &[(&str, &str)] = &[("bob@example.test", "user")];
fn config() -> config::ValidatedConfig {
    let start = support::YAML.find("    profiles:").unwrap();
    let yaml = format!("{}{}", &support::YAML[..start], CATALOG_YAML);
    let mut env = support::environment();
    // alice가 고정 관리자다. bob은 일반 사용자로 접근 목록에 넣는다(`ACCESS`).
    env.insert("ADMIN_EMAIL".into(), "alice@example.test".into());
    config::load(ConfigInputs {
        yaml: &yaml,
        file_environment: None,
        environment: &env,
    })
    .unwrap()
}

/// 가격 출처(models.dev)를 흉내 내는 합성 서버. 내려주는 JSON은 시험 중에 바꿀 수 있다.
#[derive(Default)]
struct Sources {
    models_dev: Mutex<Option<Value>>,
}
async fn serve(sources: Arc<Sources>) -> (url::Url, tokio::task::JoinHandle<()>) {
    use axum::{extract::State, response::IntoResponse, routing::get};
    let app = axum::Router::new()
        .route(
            "/models-dev",
            get(|State(s): State<Arc<Sources>>| async move {
                let v = s.models_dev.lock().unwrap().clone();
                match v {
                    Some(v) => axum::Json(v).into_response(),
                    None => axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
                }
            }),
        )
        .with_state(sources);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("{base}/models-dev").parse().unwrap(), task)
}
fn models_dev(input: f64) -> Value {
    json!({"anthropic":{"models":{
        "synthetic-text":{"cost":{"input":input,"output":2,"cache_read":0.1,"cache_write":1.25},"limit":{"context":4096,"output":1024}},
        "synthetic-tiered":{"cost":{"input":1,"output":2,"cache_read":0.1,"cache_write":1.25,"tiers":[{"tier":{"type":"context","size":2048}}]},"limit":{"context":4096,"output":1024}},
        "synthetic-extra":{"cost":{"input":3,"output":4,"cache_read":0.3,"cache_write":3.75},"limit":{"context":4096,"output":1024}},
        "other-model":{"cost":{"input":1,"output":2,"cache_read":0.1,"cache_write":1.25},"limit":{"context":4096,"output":1024}}}}})
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
        postgres::grant_access(&db.store, &cfg, ACCESS).await;
        let fake = sso::FakeJwks::new(vec![sso::Signer::shared().jwk.clone()]);
        let jwks = fake.serve().await;
        let verifier = SsoVerifier::new(
            cfg.sso().unwrap(),
            Arc::new(JwksCache::loopback_for_test(jwks.uri.clone()).unwrap()),
        );
        let alice = sso::Signer::shared().sign(&sso::claims(), "key1");
        let mut bob_claims = sso::claims();
        bob_claims["sub"] = json!("human-2");
        bob_claims["email"] = json!("bob@example.test");
        let bob = sso::Signer::shared().sign(&bob_claims, "key1");
        let auth = Arc::new(Authenticator::new(&cfg, verifier, db.store.clone()));
        let sources = Arc::new(Sources::default());
        *sources.models_dev.lock().unwrap() = Some(models_dev(1.0));
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
            "/api/admin/model-catalog/sync",
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
            "/api/admin/model-catalog/approve",
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
        .call("GET", "/api/admin/model-catalog", &h.alice, None, None)
        .await;
    assert_eq!(status, 200);
    assert_eq!(view["version"], "0");
    assert_eq!(
        view["providers"][0]["model_prefixes"],
        json!(["synthetic-"])
    );
    let body = json!({"model":"synthetic-text","max_tokens":100,"messages":[{"role":"user","content":"x"}]});
    let alice: Uuid =
        sqlx::query_scalar("SELECT id FROM gw_principals WHERE email='alice@example.test'")
            .fetch_one(h.db.store.pool())
            .await
            .unwrap();
    finance::allow(h.db.store.pool(), alice).await;
    let issue = finance::with_credentials(
        json!({"name":"k","validity":"1_year","grants":[{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}],"limits":{"budget":null,"rpm":null,"tpm":null,"count_rpm":null}}),
    );
    let (status, _) = h
        .call(
            "POST",
            "/api/keys",
            &h.alice,
            Some(Uuid::new_v4()),
            Some(issue.clone()),
        )
        .await;
    assert_eq!(
        status, 403,
        "승인 전에는 그 모델로 가상 키를 발급할 수 없다"
    );

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
    assert_eq!(proposal["source"]["source"], "models_dev");
    assert_eq!(proposal["source"]["models"], 4);
    let text = item(proposal, "synthetic-text");
    assert_eq!(text["status"], "new");
    assert_eq!(text["proposed"]["rates"]["input"], "1.000000000000");
    assert_eq!(text["proposed"]["source"], "models_dev");
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
        .call("GET", "/api/admin/model-catalog", &h.alice, None, None)
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
    let (status, issued) = h
        .call(
            "POST",
            "/api/keys",
            &h.alice,
            Some(Uuid::new_v4()),
            Some(issue),
        )
        .await;
    assert_eq!(status, 201, "{issued}");
    let key = issued["key"].as_str().unwrap().to_owned();
    let (status, answer) = h
        .call("POST", "/v1/messages", &key, None, Some(body.clone()))
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

    // 가격이 바뀌면 이전 값과 새 값을 보여 준다.
    *h.sources.models_dev.lock().unwrap() = Some(models_dev(1.5));
    let (_, synced) = h.sync(Uuid::new_v4()).await;
    let changed = item(&synced["proposal"], "synthetic-text");
    assert_eq!(changed["status"], "changed");
    assert_eq!(changed["current"]["rates"]["input"], "1.000000000000");
    assert_eq!(changed["proposed"]["rates"]["input"], "1.500000000000");
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
        .call("POST", "/v1/messages", &key, None, Some(body.clone()))
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

    // 가격 출처를 받지 못하면 변경안을 만들지 않는다.
    *h.sources.models_dev.lock().unwrap() = None;
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
        .call("GET", "/api/admin/model-catalog", &h.alice, None, None)
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
            "/api/admin/model-catalog/sync",
            &self.bob,
            Some(Uuid::new_v4()),
            None,
        )
        .await
    }
}

impl Harness {
    async fn rates(&self, command: Uuid, body: Value) -> (u16, Value) {
        self.call(
            "PUT",
            "/api/admin/model-catalog/rates",
            &self.alice,
            Some(command),
            Some(body),
        )
        .await
    }
}

/// 결정 0012: 관리자가 칸별 단가를 고치고, 1시간 캐시 요청은 그 단가가 있어야 나간다.
/// 동기화는 models.dev에 없는 칸(`cache_write_1h`)을 관리자가 정한 값으로 둔다.
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn admin_sets_category_rates_and_one_hour_cache_needs_its_rate() {
    let h = Harness::new().await;
    let (_, synced) = h.sync(Uuid::new_v4()).await;
    let proposal_id = synced["proposal_id"].as_str().unwrap().to_owned();
    let (status, _) = h.approve(&proposal_id, "0", &["synthetic-text"]).await;
    assert_eq!(status, 200);
    let (_, view) = h
        .call("GET", "/api/admin/model-catalog", &h.alice, None, None)
        .await;
    assert_eq!(
        view["providers"][0]["usage_categories"],
        json!([
            "input",
            "output",
            "cache_read",
            "cache_write_5m",
            "cache_write_1h"
        ])
    );
    assert_eq!(
        view["entries"][0]["rates"],
        json!({"input":"1.000000000000","output":"2.000000000000","cache_read":"0.100000000000","cache_write_5m":"1.250000000000"})
    );

    // alice의 가상 키.
    let alice: Uuid =
        sqlx::query_scalar("SELECT id FROM gw_principals WHERE email='alice@example.test'")
            .fetch_one(h.db.store.pool())
            .await
            .unwrap();
    finance::allow(h.db.store.pool(), alice).await;
    sqlx::query("UPDATE gw_policies SET allow_rules=$1 WHERE principal_id=$2 AND key_id IS NULL")
        .bind(
            json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}]),
        )
        .bind(alice)
        .execute(h.db.store.pool())
        .await
        .unwrap();
    let issue = finance::with_credentials(
        json!({"name":"k","validity":"1_year","grants":[{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}],"limits":{"budget":null,"rpm":null,"tpm":null,"count_rpm":null}}),
    );
    let (status, issued) = h
        .call(
            "POST",
            "/api/keys",
            &h.alice,
            Some(Uuid::new_v4()),
            Some(issue),
        )
        .await;
    assert_eq!(status, 201, "{issued}");
    let key = issued["key"].as_str().unwrap().to_owned();
    let cached = |ttl: Option<&str>| {
        let mut control = json!({"type":"ephemeral"});
        if let Some(ttl) = ttl {
            control["ttl"] = json!(ttl);
        }
        json!({"model":"synthetic-text","max_tokens":100,"messages":[{"role":"user","content":[{"type":"text","text":"x","cache_control":control}]}]})
    };

    // 1시간 캐시 단가가 없으면 1시간 캐시 요청을 보내지 않는다.
    let (status, refused) = h
        .call("POST", "/v1/messages", &key, None, Some(cached(Some("1h"))))
        .await;
    assert_eq!(status, 400, "{refused}");
    assert_eq!(refused["error"]["type"], "invalid_request_error");
    assert_eq!(refused["gateway_error_code"], "unsupported_option");
    assert!(h.provider.calls.lock().await.is_empty());

    // 단가 고치기: 버전이 다르면 거절하고, 관리자만 고칠 수 있다.
    let edit = |version: &str, rates: Value| json!({"provider":"anthropic","model":"synthetic-text","rates":rates,"expected_version":version});
    let (status, conflict) = h
        .rates(Uuid::new_v4(), edit("0", json!({"cache_write_1h":"2"})))
        .await;
    assert_eq!(
        (status, conflict["error"]["code"].clone()),
        (409, json!("version_conflict"))
    );
    let (status, _) = h
        .call(
            "PUT",
            "/api/admin/model-catalog/rates",
            &h.bob,
            Some(Uuid::new_v4()),
            Some(edit("1", json!({"cache_write_1h":"2"}))),
        )
        .await;
    assert_eq!(status, 403);
    for (body, expected) in [
        // 이 API가 알리지 않은 칸.
        (edit("1", json!({"cache_write":"2"})), 400),
        (edit("1", json!({"reasoning":"2"})), 400),
        // 단가 형식.
        (edit("1", json!({"cache_write_1h":"2.0000000000001"})), 400),
        (edit("1", json!({"cache_write_1h":"-1"})), 400),
        (edit("1", json!({"cache_write_1h":2})), 400),
        (edit("1", json!({})), 400),
        // 캐시를 쓰는 모델에서 5분 캐시 쓰기 단가는 지울 수 없다.
        (edit("1", json!({"cache_write_5m":null})), 400),
        // 카탈로그에 없는 모델.
        (
            json!({"provider":"anthropic","model":"synthetic-extra","rates":{"input":"1"},"expected_version":"1"}),
            404,
        ),
    ] {
        let (status, answer) = h.rates(Uuid::new_v4(), body.clone()).await;
        assert_eq!(status, expected, "{body} {answer}");
    }
    let command = Uuid::new_v4();
    let set = edit("1", json!({"cache_write_1h":"2"}));
    let (status, ack) = h.rates(command, set.clone()).await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(
        (
            ack["operation"].clone(),
            ack["result_version"].clone(),
            ack["effect"].clone(),
            ack["replayed"].clone()
        ),
        (
            json!("edit_model_catalog_rates"),
            json!("2"),
            json!("changed"),
            json!(false)
        )
    );
    // 같은 명령을 다시 보내면 같은 결과를 돌려주고 새 버전을 만들지 않는다.
    let (status, replay) = h.rates(command, set).await;
    assert_eq!(
        (
            status,
            replay["result_version"].clone(),
            replay["replayed"].clone()
        ),
        (200, json!("2"), json!(true))
    );
    let (status, reused) = h
        .rates(command, edit("2", json!({"cache_write_1h":"3"})))
        .await;
    assert_eq!(
        (status, reused["error"]["code"].clone()),
        (409, json!("command_conflict"))
    );
    // 같은 값은 바뀐 것이 없다.
    let (status, noop) = h
        .rates(
            Uuid::new_v4(),
            edit("2", json!({"cache_write_1h":"2.000000000000"})),
        )
        .await;
    assert_eq!(
        (
            status,
            noop["result_version"].clone(),
            noop["effect"].clone()
        ),
        (200, json!("2"), json!("noop"))
    );
    assert_eq!(h.service.cache().snapshot().version, 2);
    let (_, view) = h
        .call("GET", "/api/admin/model-catalog", &h.alice, None, None)
        .await;
    assert_eq!(view["version"], "2");
    assert_eq!(
        view["entries"][0]["rates"]["cache_write_1h"],
        "2.000000000000"
    );
    let (operation, before, after): (String, Value, Value) = sqlx::query_as(
        "SELECT operation,before_snapshot,after_snapshot FROM gw_audit_events WHERE target_kind='catalog' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(operation, "edit_model_catalog_rates");
    assert_eq!(before["version"], 1);
    assert!(before["rates"].get("cache_write_1h").is_none());
    assert_eq!(after["version"], 2);
    assert_eq!(after["rates"]["cache_write_1h"], "2.000000000000");
    let commands: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gw_commands WHERE operation='edit_model_catalog_rates'",
    )
    .fetch_one(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(commands, 2, "바뀐 것과 바뀐 것이 없는 명령");

    // 1시간 캐시 요청이 나가고, 5분·1시간 캐시 쓰기를 칸마다 그 단가로 계산한다.
    let headers = axum::http::HeaderMap::from_iter([(
        "content-type".parse().unwrap(),
        "application/json".parse().unwrap(),
    )]);
    let usage = |cache: &str| {
        axum::body::Bytes::from(format!(
            r#"{{"id":"msg_fixture","type":"message","role":"assistant","model":"synthetic-text","content":[{{"type":"text","text":"x"}}],"stop_reason":"end_turn","usage":{{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":7{cache}}}}}"#
        ))
    };
    h.provider
        .set(
            axum::http::StatusCode::OK,
            headers.clone(),
            usage(r#","cache_creation_input_tokens":30,"cache_creation":{"ephemeral_5m_input_tokens":10,"ephemeral_1h_input_tokens":20}"#),
        )
        .await;
    let (status, answer) = h
        .call("POST", "/v1/messages", &key, None, Some(cached(Some("1h"))))
        .await;
    assert_eq!(status, 200, "{answer}");
    let latest = |pool: &sqlx::PgPool| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (String, Option<String>, Value, Value)>(
                "SELECT r.accounting_state,r.cost_usd::text,e.normalized_usage,r.pricing_snapshot FROM gw_requests r JOIN gw_usage_events e ON e.request_id=r.id AND e.kind IN ('partial','final') ORDER BY r.admitted_at DESC LIMIT 1",
            )
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let (state, cost, usage_record, pricing) = latest(h.db.store.pool()).await;
    assert_eq!(state, "settled");
    // 입력 100×1 + 출력 20×2 + 캐시 읽기 7×0.1 + 5분 캐시 쓰기 10×1.25 + 1시간 캐시 쓰기 20×2.
    assert_eq!(cost.as_deref(), Some("0.000193200000"));
    assert_eq!(
        (
            usage_record["cache_write_5m_tokens"].clone(),
            usage_record["cache_write_1h_tokens"].clone(),
            usage_record["cache_write_tokens"].clone()
        ),
        (json!("10"), json!("20"), json!("0"))
    );
    assert_eq!(pricing["cache_tier_rules"], "explicit_categories_v1");

    // 동기화는 models.dev가 주는 칸만 바꾸고 1시간 캐시 쓰기 단가는 둔다.
    *h.sources.models_dev.lock().unwrap() = Some(models_dev(1.5));
    let (_, synced) = h.sync(Uuid::new_v4()).await;
    let changed = item(&synced["proposal"], "synthetic-text");
    assert_eq!(changed["status"], "changed");
    assert_eq!(changed["proposed"]["rates"]["input"], "1.500000000000");
    assert_eq!(
        changed["proposed"]["rates"]["cache_write_1h"],
        "2.000000000000"
    );
    let (status, _) = h
        .approve(
            synced["proposal_id"].as_str().unwrap(),
            "2",
            &["synthetic-text"],
        )
        .await;
    assert_eq!(status, 200);
    let rates = &h.service.cache().snapshot().profiles[0].1.rates;
    assert_eq!(
        (rates["input"].as_str(), rates["cache_write_1h"].as_str()),
        ("1.500000000000", "2.000000000000")
    );

    // 캐시 쓰기를 5분·1시간으로 나누지 않은 응답은 금액을 정하지 않는다(unknown).
    h.provider
        .set(
            axum::http::StatusCode::OK,
            headers,
            usage(r#","cache_creation_input_tokens":30"#),
        )
        .await;
    let (status, _) = h
        .call("POST", "/v1/messages", &key, None, Some(cached(None)))
        .await;
    assert_eq!(status, 200);
    let (state, cost, usage_record, _) = latest(h.db.store.pool()).await;
    assert_eq!((state.as_str(), cost), ("unknown", None));
    assert!(usage_record["cache_write_5m_tokens"].is_null());
    h.db.close().await;
}
