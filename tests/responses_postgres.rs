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
    let yaml = include_str!("support/config.yaml")
        .replace("provider: anthropic", "provider: openai")
        .replace("https://api.anthropic.com", "https://api.openai.com")
        .replace("api: messages", "api: responses");
    configuration_yaml(&yaml)
}
fn configuration_yaml(yaml: &str) -> config::ValidatedConfig {
    let mut env = support::environment();
    env.insert("ALLOWED_EMAILS".into(), "alice@example.test".into());
    env.insert("ADMIN_EMAILS".into(), "alice@example.test".into());
    config::load(ConfigInputs {
        yaml,
        file_environment: None,
        environment: &env,
    })
    .unwrap()
}
fn verified_yaml() -> String {
    include_str!("support/config.yaml").replace("provider: anthropic","provider: openai").replace("https://api.anthropic.com","https://api.openai.com").replace("api: messages","api: responses").replace("        max_input_tokens: 4096","        features: [client_tools, thinking, cache, json_schema, inline_png]\n        input_bound_contract:\n          kind: openai_responses_context_v1\n          context_tokens: 4096\n          input_categories: [input]\n        max_input_tokens: 4096").replace("          input: '1.000000000000'","          input: '1.000000000000'\n          cache_read: '0.500000000000'")
}
fn verified_configuration() -> config::ValidatedConfig {
    configuration_yaml(&verified_yaml())
}
struct Harness {
    db: postgres::TestDb,
    provider: Arc<provider::Provider>,
    _provider_server: provider::Server,
    _jwks: sso::Server,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
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
        sqlx::query("UPDATE gw_policies SET allow_rules=$2 WHERE id=$1").bind(current.policy_id).bind(serde_json::json!([{"provider":"openai","model":"synthetic-text","capabilities":["responses"]}])).execute(db.store.pool()).await.unwrap();
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
                .with_authentication(auth)
                .with_dashboard(dashboard)
                .with_inference(gateway),
        )
        .unwrap();
        // U0 전 제품 main 부팅으로 세지 않는 실제 backend 조립. health는 테스트가 관찰한 의존성만 기록한다.
        let runtime = app.runtime().clone();
        runtime.start_http().unwrap();
        for d in [
            Dependency::Database,
            Dependency::Schema,
            Dependency::Jwks,
            Dependency::Ui,
        ] {
            runtime.record_health(d, true, tokio::time::Instant::now());
        }
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
        self.task.abort();
        let _ = self.task.await;
        self.db.close().await;
    }
}
const BODY: &str = r#" { "model":"synthetic-text", "input":"synthetic request","store":false,"max_output_tokens":100 } "#;
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn responses_independent_request_contract() {
    let h = Harness::new().await;
    let native = r#"{"object":"response","id":"resp_synthetic","status":"completed","model":"synthetic-text","output":[],"usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}}"#;
    h.provider
        .set(
            StatusCode::OK,
            HeaderMap::from_iter([(
                "content-type".parse().unwrap(),
                "application/json".parse().unwrap(),
            )]),
            Bytes::copy_from_slice(native.as_bytes()),
        )
        .await;
    let r = h
        .request("/openai/v1/responses", BODY)
        .send()
        .await
        .unwrap();
    let status = r.status();
    let bytes = r.bytes().await.unwrap();
    // Red도 자기 schema를 정리한 후 assertion한다.
    if status != StatusCode::OK {
        h.close().await;
        assert_eq!(status, StatusCode::OK, "독립 Responses 생성 route");
        return;
    }
    assert_eq!(bytes, native);
    h.runtime.wait_requests().await;
    let row = sqlx::query("SELECT accounting_state,cost_usd::text AS cost FROM gw_requests")
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("accounting_state"), "settled");
    assert_eq!(row.get::<String, _>("cost"), "0.000140000000");
    let calls = h.provider.calls.lock().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].path, "/v1/responses");
    assert_eq!(calls[0].body, BODY);
    assert_eq!(
        calls[0].headers["authorization"],
        "Bearer synthetic-company-key"
    );
    assert!(!calls[0].headers.contains_key("x-api-key"));
    drop(calls);
    h.close().await;
}

fn native(status: &str) -> String {
    serde_json::json!({"id":"resp_fixture","object":"response","status":status,"model":"synthetic-text","incomplete_details":if status=="incomplete"{Some(serde_json::json!({"reason":"max_output_tokens"}))}else{None},"output":[],"usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cached_tokens":40},"output_tokens_details":{"reasoning_tokens":10}}}).to_string()
}
fn start() -> String {
    format!(
        "event: response.created\r\ndata: {}\r\n\r\n",
        serde_json::json!({"type":"response.created","sequence_number":0,"response":{"id":"resp_fixture","object":"response","model":"synthetic-text","status":"in_progress","usage":null,"output":[]}})
    )
}
fn terminal(status: &str) -> String {
    format!(
        "event: response.{status}\ndata: {{\"type\":\"response.{status}\",\"sequence_number\":1,\"response\":{}}}\n\n",
        native(status)
    )
}
async fn idle(h: &Harness) {
    tokio::time::timeout(std::time::Duration::from_secs(6), h.runtime.wait_requests())
        .await
        .expect("유한 task/body 수명");
    assert_eq!(h.runtime.available_inference(), 16);
    assert_eq!(h.runtime.active_principals(), 0);
}
async fn strict(h: &Harness) {
    sqlx::query("UPDATE gw_policies SET budget_mode='reserved',budget_limit_usd=1,tpm_limit=100000 WHERE principal_id=$1").bind(h.principal).execute(h.db.store.pool()).await.unwrap();
    h.provider.responses_contract(4096, 1024).await;
}
async fn reservation(h: &Harness) {
    let r=sqlx::query("SELECT r.phase,b.reservation_usd::text AS amount,b.remaining_reservation_usd::text AS held,b.reserved_tokens FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id ORDER BY admitted_at DESC LIMIT 1").fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(r.get::<String, _>("phase"), "dispatching");
    assert_eq!(r.get::<String, _>("amount"), "0.004296000000");
    assert_eq!(r.get::<String, _>("held"), "0.004296000000");
    assert_eq!(r.get::<i64, _>("reserved_tokens"), 4196);
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn responses_completed_and_output_limit_incomplete_release_nonzero_reservation_and_tpm() {
    for stream in [false, true] {
        for status in ["completed", "incomplete"] {
            let h = Harness::with_config(verified_configuration()).await;
            strict(&h).await;
            let wire = if stream {
                {
                    let progress = |sequence| {
                        format!(
                            "data: {}\n\n",
                            serde_json::json!({"type":"response.in_progress","sequence_number":sequence,"response":{"id":"resp_fixture","object":"response","model":"synthetic-text","status":"in_progress","error":null,"incomplete_details":null,"usage":null}})
                        )
                    };
                    format!(
                        "{}{}{}data: {{\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"delta\":\"synthetic\"}}\n\n{}",
                        start(),
                        progress(1),
                        progress(2),
                        terminal(status).replace("\"sequence_number\":1", "\"sequence_number\":4")
                    )
                }
            } else {
                native(status)
            };
            h.provider
                .set_chunked(
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
                    Bytes::from(wire.clone()),
                )
                .await;
            let (started, release) = h.provider.hold_response().await;
            let input = if status == "completed" {
                r#"[{"role":"assistant","content":"synthetic request"}]"#
            } else {
                r#"[{"content":[{"type":"input_text","text":"synthetic request"}],"role":"assistant"}]"#
            };
            let body = BODY.replace("\"synthetic request\"", input);
            let body = if stream {
                body.replacen('{', "{\"stream\":true,", 1)
            } else {
                body
            };
            let send = h.request("/openai/v1/responses", body.clone()).send();
            let check = async {
                started.await.unwrap();
                reservation(&h).await;
                release.send(()).unwrap();
            };
            let (r, ()) = tokio::join!(send, check);
            let r = r.unwrap();
            assert_eq!(r.status(), 200);
            let id: uuid::Uuid = r.headers()["x-gateway-request-id"]
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(r.bytes().await.unwrap(), wire);
            idle(&h).await;
            let r=sqlx::query("SELECT r.outcome,r.accounting_state,r.cost_usd::text AS cost,b.remaining_reservation_usd::text AS held,b.remaining_reserved_tokens,w.reserved_tokens,w.settled_tokens,t.unknown_count,t.input_tokens,t.output_tokens,e.normalized_usage AS usage,e.provider_usage AS native,e.pricing_resolution FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id JOIN gw_usage_totals t ON (t.scope_id,t.period_start)=(b.scope_id,b.period_start) JOIN gw_rate_windows w ON (w.scope_id,w.lane,w.window_start)=(b.scope_id,b.rate_lane,b.rate_start) JOIN gw_usage_events e ON e.request_id=r.id AND e.kind='final' WHERE r.id=$1").bind(id).fetch_one(h.db.store.pool()).await.unwrap();
            assert_eq!(r.get::<String, _>("outcome"), "completed");
            assert_eq!(r.get::<String, _>("accounting_state"), "settled");
            assert_eq!(r.get::<String, _>("cost"), "0.000120000000");
            assert_eq!(r.get::<String, _>("held"), "0.000000000000");
            assert_eq!(r.get::<i64, _>("remaining_reserved_tokens"), 0);
            assert_eq!(r.get::<i64, _>("reserved_tokens"), 0);
            assert_eq!(r.get::<i64, _>("settled_tokens"), 120);
            assert_eq!(r.get::<i64, _>("unknown_count"), 0);
            assert_eq!(r.get::<i64, _>("input_tokens"), 100);
            assert_eq!(r.get::<i64, _>("output_tokens"), 20);
            let resolution: serde_json::Value = r.get("pricing_resolution");
            let categories = resolution["categories"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| {
                    (
                        c["category"].as_str().unwrap(),
                        c["tokens"].as_str().unwrap(),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>();
            assert_eq!(
                categories,
                std::collections::BTreeMap::from([
                    ("input", "60"),
                    ("cache_read", "40"),
                    ("output", "20")
                ])
            );
            let u: serde_json::Value = r.get("usage");
            assert_eq!(u["input_tokens"], "100");
            assert_eq!(u["cache_read_tokens"], "40");
            assert_eq!(u["reasoning_tokens"], "10");
            let native: serde_json::Value = r.get("native");
            assert_eq!(native["input_tokens"], 100);
            assert_eq!(native["input_tokens_details"]["cached_tokens"], 40);
            let next = h
                .request("/openai/v1/responses", body.clone())
                .send()
                .await
                .unwrap();
            assert_eq!(next.status(), 200);
            assert_eq!(next.bytes().await.unwrap(), wire);
            idle(&h).await;
            let calls = h.provider.calls.lock().await;
            assert_eq!(calls.len(), 2);
            assert!(
                calls
                    .iter()
                    .all(|call| call.body.as_ref() == body.as_bytes())
            );
            drop(calls);
            assert_eq!(
                sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM gw_usage_events WHERE kind='final'"
                )
                .fetch_one(h.db.store.pool())
                .await
                .unwrap(),
                2
            );
            let totals=sqlx::query("SELECT cost_usd::text AS cost, reserved_usd::text AS reserved,settled_count,unknown_count,input_tokens,output_tokens FROM gw_usage_totals").fetch_one(h.db.store.pool()).await.unwrap();
            assert_eq!(totals.get::<String, _>("cost"), "0.000240000000");
            assert_eq!(totals.get::<String, _>("reserved"), "0.000000000000");
            assert_eq!(totals.get::<i64, _>("settled_count"), 2);
            assert_eq!(totals.get::<i64, _>("unknown_count"), 0);
            assert_eq!(totals.get::<i64, _>("input_tokens"), 200);
            assert_eq!(totals.get::<i64, _>("output_tokens"), 40);
            h.close().await;
        }
    }
}

async fn unknown(h: &Harness) {
    let row=sqlx::query("SELECT r.accounting_state,r.cost_usd::text AS cost,b.remaining_reservation_usd::text AS held,b.remaining_reserved_tokens,t.unknown_count,t.reserved_usd::text AS reserved FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id JOIN gw_usage_totals t ON (t.scope_id,t.period_start)=(b.scope_id,b.period_start) ORDER BY r.admitted_at LIMIT 1").fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(row.get::<String, _>("accounting_state"), "unknown");
    assert!(row.get::<Option<String>, _>("cost").is_none());
    assert_eq!(row.get::<String, _>("held"), "0.004296000000");
    assert_eq!(row.get::<String, _>("reserved"), "0.004296000000");
    assert_eq!(row.get::<i64, _>("remaining_reserved_tokens"), 4196);
    assert_eq!(row.get::<i64, _>("unknown_count"), 1);
    let next = h
        .request("/openai/v1/responses", BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(next.status(), 403);
    assert_eq!(
        next.headers()["x-gateway-error-code"],
        "unsettled_usage_block"
    );
    let body: serde_json::Value = serde_json::from_slice(&next.bytes().await.unwrap()).unwrap();
    assert_eq!(body["error"]["code"], "gateway_unsettled_usage_block");
    assert!(body.get("type").is_none());
    idle(h).await;
    assert_eq!(h.provider.calls.lock().await.len(), 1);
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn responses_missing_conflicting_truncated_or_unpriced_finality_holds_reservation_and_blocks()
{
    for stream in [false, true] {
        for case in [
            "missing_usage",
            "missing_input",
            "conflicting_total",
            "cached_over_input",
            "reasoning_over_output",
            "unknown_alias",
            "missing_price",
            "content_filter",
            "failed",
            "duplicate_usage",
            "truncated",
            "malformed_after_usage",
            "duplicate_terminal",
            "terminal_sequence",
            "missing_details",
            "tier_priority",
            "terminal_error",
            "completed_incomplete_details",
        ] {
            let cfg = if case == "missing_price" {
                configuration_yaml(
                    &verified_yaml()
                        .replace("          cache_read: '0.500000000000'\n", "")
                        .replace(", cache,", ","),
                )
            } else {
                verified_configuration()
            };
            let h = Harness::with_config(cfg).await;
            strict(&h).await;
            let mut value: serde_json::Value = serde_json::from_str(&native("incomplete")).unwrap();
            match case {
                "missing_details" => {
                    value["usage"]
                        .as_object_mut()
                        .unwrap()
                        .remove("input_tokens_details");
                }
                "tier_priority" => value["service_tier"] = "priority".into(),
                "terminal_error" => value["error"] = serde_json::json!({"code":"server_error"}),
                "completed_incomplete_details" => value["status"] = "completed".into(),
                "missing_usage" => {
                    value.as_object_mut().unwrap().remove("usage");
                }
                "missing_input" => {
                    value["usage"]
                        .as_object_mut()
                        .unwrap()
                        .remove("input_tokens");
                }
                "conflicting_total" => value["usage"]["total_tokens"] = 121.into(),
                "cached_over_input" => {
                    value["usage"]["input_tokens_details"]["cached_tokens"] = 101.into()
                }
                "reasoning_over_output" => {
                    value["usage"]["output_tokens_details"]["reasoning_tokens"] = 21.into()
                }
                "unknown_alias" => value["model"] = "synthetic-unknown".into(),
                "content_filter" => value["incomplete_details"]["reason"] = "content_filter".into(),
                "failed" => value["status"] = "failed".into(),
                _ => {}
            }
            let mut json = value.to_string();
            if case == "duplicate_usage" {
                json.insert_str(json.len() - 1, ",\"usage\":{}");
            }
            if case == "truncated" {
                json.pop();
            }
            if case == "malformed_after_usage" {
                json.insert_str(json.len() - 1, ",\"skip\":[1,]");
            }
            let kind = value["status"].as_str().unwrap();
            let tail = format!(
                "event: response.{kind}\ndata: {{\"type\":\"response.{kind}\",\"sequence_number\":{},\"response\":{json}}}\n\n",
                if case == "terminal_sequence" { 0 } else { 1 }
            );
            if stream {
                let (tx, stopped) = h.provider.stream().await;
                let body = BODY.replacen('{', "{\"stream\":true,", 1);
                let send = h.request("/openai/v1/responses", body).send();
                let produce = async {
                    tx.send(Bytes::from(start())).await.unwrap();
                };
                let (r, ()) = tokio::join!(send, produce);
                let r = r.unwrap();
                assert_eq!(r.status(), 200);
                reservation(&h).await;
                tx.send(Bytes::from(if case == "duplicate_terminal" {
                    format!("{tail}{tail}")
                } else {
                    tail
                }))
                .await
                .unwrap();
                drop(tx);
                let _ = r.bytes().await;
                tokio::time::timeout(std::time::Duration::from_secs(2), stopped)
                    .await
                    .unwrap()
                    .unwrap();
            } else {
                // stream 전용 sequence/중복 terminal 사례는 nonstream의 중복/추가 JSON으로 대응한다.
                if case == "duplicate_terminal" || case == "terminal_sequence" {
                    json.push_str("{}");
                }
                h.provider
                    .set_chunked(
                        StatusCode::OK,
                        HeaderMap::from_iter([(
                            "content-type".parse().unwrap(),
                            "application/json".parse().unwrap(),
                        )]),
                        Bytes::from(json),
                    )
                    .await;
                let r = h
                    .request("/openai/v1/responses", BODY)
                    .send()
                    .await
                    .unwrap();
                assert!([200, 502].contains(&r.status().as_u16()));
                let _ = r.bytes().await;
            }
            idle(&h).await;
            unknown(&h).await;
            h.close().await;
        }
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn responses_state_resource_and_unverified_strict_never_dispatch() {
    let h = Harness::new().await;
    for body in [
        BODY.replace("\"store\":false,", ""),
        BODY.replace("\"store\":false", "\"store\":true"),
        BODY.replace(
            "\"store\":false",
            "\"store\":false,\"previous_response_id\":\"resp_old\"",
        ),
        BODY.replace("\"store\":false", "\"store\":false,\"conversation\":\"c\""),
        BODY.replace("\"store\":false", "\"store\":false,\"background\":true"),
        BODY.replace(
            "\"input\":\"synthetic request\"",
            "\"input\":[{\"type\":\"item_reference\",\"id\":\"item_1\"}]",
        ),
        BODY.replace(
            "\"store\":false",
            "\"store\":false,\"tools\":[{\"type\":\"web_search\"}]",
        ),
    ] {
        let r = h
            .request("/openai/v1/responses", body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        assert_eq!(r.headers()["x-gateway-dispatch-state"], "not_dispatched");
        let v: serde_json::Value = serde_json::from_slice(&r.bytes().await.unwrap()).unwrap();
        assert_eq!(v["error"]["type"], "invalid_request_error");
        idle(&h).await;
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM gw_requests")
            .fetch_one(h.db.store.pool())
            .await
            .unwrap(),
        0
    );
    strict(&h).await;
    let r = h
        .request("/openai/v1/responses", BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(
        r.headers()["x-gateway-error-code"],
        "unsupported_strict_policy"
    );
    drop(r);
    idle(&h).await;
    assert!(h.provider.calls.lock().await.is_empty());
    h.close().await;
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
    let home = std::path::PathBuf::from(format!("/tmp/m-gated-s6-sdk-{}", uuid::Uuid::new_v4()));
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
            "test:responses",
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
                "SDK_RESPONSES_OK status={status} attempts=1 maxRetries=0"
            ))
            && stderr.is_empty(),
        "합성 Responses SDK 검증 실패(kind={kind},status={status}); 원문오류/본문 출력없음"
    );
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL and TEST_FNM_DIR, installed fixed SDK"]
async fn responses_fixed_sdk_observes_native_completed_incomplete_and_one_send_per_call() {
    for stream in [false, true] {
        for status in ["completed", "incomplete"] {
            let h = Harness::with_config(verified_configuration()).await;
            strict(&h).await;
            let wire = if stream {
                format!("{}{}", start(), terminal(status))
            } else {
                native(status)
            };
            h.provider
                .set_chunked(
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
                    Bytes::from(wire),
                )
                .await;
            sdk(
                &h,
                if stream {
                    "responses_stream"
                } else {
                    "responses"
                },
                200,
                status,
                &h.token,
            )
            .await;
            idle(&h).await;
            assert_eq!(h.provider.calls.lock().await.len(), 1);
            let row =
                sqlx::query("SELECT accounting_state,cost_usd::text AS cost FROM gw_requests")
                    .fetch_one(h.db.store.pool())
                    .await
                    .unwrap();
            assert_eq!(row.get::<String, _>("accounting_state"), "settled");
            assert_eq!(row.get::<String, _>("cost"), "0.000120000000");
            h.close().await;
        }
    }
    for status in [400, 401, 403, 429, 502, 503] {
        let h = Harness::with_config(verified_configuration()).await;
        if status == 403 {
            sqlx::query("UPDATE gw_policies SET allow_rules='[]'")
                .execute(h.db.store.pool())
                .await
                .unwrap();
        }
        if status == 429 {
            sqlx::query("UPDATE gw_policies SET budget_limit_usd=0")
                .execute(h.db.store.pool())
                .await
                .unwrap();
        }
        if status == 502 {
            h.provider
                .set(
                    StatusCode::UNAUTHORIZED,
                    HeaderMap::from_iter([
                        (
                            "content-type".parse().unwrap(),
                            "application/json".parse().unwrap(),
                        ),
                        (
                            "www-authenticate".parse().unwrap(),
                            "Bearer synthetic-company-secret".parse().unwrap(),
                        ),
                    ]),
                    Bytes::from_static(b"synthetic-company-key-error"),
                )
                .await;
        }
        if status == 503 {
            h.runtime.begin_draining();
        }
        sdk(
            &h,
            if status == 400 {
                "responses_bad_store"
            } else {
                "responses"
            },
            status,
            "completed",
            if status == 401 {
                "invalid-synthetic-token"
            } else {
                &h.token
            },
        )
        .await;
        idle(&h).await;
        assert_eq!(
            h.provider.calls.lock().await.len(),
            usize::from(status == 502)
        );
        if status == 502 {
            assert_eq!(
                sqlx::query_scalar::<_, String>("SELECT accounting_state FROM gw_requests")
                    .fetch_one(h.db.store.pool())
                    .await
                    .unwrap(),
                "unknown"
            );
        }
        h.close().await;
    }
}

fn padded_native(status: &str, size: usize) -> String {
    let base = native(status).replace("\"output\":[],", "");
    let suffix = &base[1..];
    let prefix = "{\"instructions\":\"";
    let middle =
        "\",\"tools\":[{\"type\":\"function\",\"name\":\"f\",\"parameters\":{\"description\":\"";
    let output = "\"}}],\"output\":[{\"id\":\"msg_synthetic\",\"type\":\"message\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"annotations\":[],\"text\":\"";
    let end = "합성\\uD83D\\uDE00\"}]}],";
    let room = size - prefix.len() - middle.len() - output.len() - end.len() - suffix.len();
    let third = room / 3;
    format!(
        "{prefix}{}{middle}{}{output}{}{end}{suffix}",
        "i".repeat(third),
        "t".repeat(third),
        "o".repeat(room - third * 2)
    )
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn responses_large_terminal_and_nonstream_exact_limits_preserve_native_and_bounded_retained()
{
    use sha2::{Digest, Sha256};
    for stream in [false, true] {
        for size in [256 * 1024 + 1, 16 * 1024 * 1024, 16 * 1024 * 1024 + 1] {
            let h = Harness::with_config(verified_configuration()).await;
            strict(&h).await;
            // SSE 상한은 JSON bytes가 아니라 event 전체 wire bytes다.
            let prefix = "event: response.incomplete\r\ndata: {\"type\":\"response.incomplete\",\"sequence_number\":1,\"response\":";
            let suffix = "}\r\n\r\n";
            let json = padded_native(
                "incomplete",
                if stream {
                    size - prefix.len() - suffix.len()
                } else {
                    size
                },
            );
            let wire = if stream {
                format!("{}{prefix}{json}{suffix}", start())
            } else {
                json
            };
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
                    Bytes::from(wire.clone()),
                )
                .await;
            let body = if stream {
                BODY.replacen('{', "{\"stream\":true,", 1)
            } else {
                BODY.to_string()
            };
            let r = h
                .request("/openai/v1/responses", body)
                .send()
                .await
                .unwrap();
            if size <= 16 * 1024 * 1024 {
                assert_eq!(r.status(), 200);
                let bytes = r.bytes().await.unwrap();
                assert_eq!(bytes.len(), wire.len());
                assert_eq!(Sha256::digest(&bytes), Sha256::digest(wire.as_bytes()));
            } else if stream {
                let _ = r.bytes().await;
            } else {
                assert_eq!(r.status(), 502);
                drop(r);
            }
            idle(&h).await;
            if size <= 16 * 1024 * 1024 {
                let r =
                    sqlx::query("SELECT accounting_state,cost_usd::text AS cost FROM gw_requests")
                        .fetch_one(h.db.store.pool())
                        .await
                        .unwrap();
                assert_eq!(r.get::<String, _>("accounting_state"), "settled");
                assert_eq!(r.get::<String, _>("cost"), "0.000120000000");
            } else {
                let (phase, state, reason): (String, String, String) =
                    sqlx::query_as("SELECT phase,accounting_state,reason_code FROM gw_requests")
                        .fetch_one(h.db.store.pool())
                        .await
                        .unwrap();
                eprintln!(
                    "S9_TERMINAL_BOUNDARY stream={stream} bytes={size} phase={phase} state={state} reason={reason}"
                );
                unknown(&h).await;
            }
            let resources = h.runtime.request_resources();
            assert!(resources.parser_peak <= 256 * 1024);
            assert!(resources.queue_peak <= 64 * 1024);
            assert_eq!(resources.body_bytes, 0);
            assert_eq!(resources.queue_bytes, 0);
            if stream && size <= 16 * 1024 * 1024 {
                assert!(resources.parser_peak > 0);
                println!(
                    "S6_WIRE event_bytes={size} parser_retained_peak={} queue_peak={}",
                    resources.parser_peak, resources.queue_peak
                );
            }
            h.close().await;
        }
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn responses_full_queue_cancel_has_no_synthetic_terminal_and_holds_unknown_reservation() {
    use std::time::Duration;
    let h = Harness::with_config(verified_configuration()).await;
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
        .uri(format!("{}/openai/v1/responses", h.base))
        .header("authorization", format!("Bearer {}", h.token))
        .header("content-type", "application/json")
        .body(())
        .unwrap();
    let (r, mut send) = sender.send_request(req, false).unwrap();
    send.send_data(
        Bytes::from(BODY.replacen('{', "{\"stream\":true,", 1)),
        true,
    )
    .unwrap();
    tx.send(Bytes::from(start())).await.unwrap();
    let response = r.await.unwrap();
    assert_eq!(response.status(), 200);
    reservation(&h).await;
    let producer = tokio::spawn(async move {
        for seq in 1..=256 {
            let wire = format!(
                "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"sequence_number\":{seq},\"delta\":\"{}\"}}\n\n",
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
    idle(&h).await;
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
async fn responses_absolute_transport_deadline_preserves_five_second_finalization_gap() {
    let cfg = configuration_yaml(&format!(
        "{}\nlimits:\n  transport_timeout_ms: 1500\n  request_timeout_ms: 6500\n  body_timeout_ms: 1500\n  body_idle_timeout_ms: 1500\n",
        verified_yaml()
    ));
    let h = Harness::with_config(cfg).await;
    strict(&h).await;
    let (tx, stopped) = h.provider.stream().await;
    tx.send(Bytes::from(start())).await.unwrap();
    let r = h
        .request(
            "/openai/v1/responses",
            BODY.replacen('{', "{\"stream\":true,", 1),
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
    idle(&h).await;
    unknown(&h).await;
    let row=sqlx::query("SELECT execution_stopped_at IS NOT NULL AS stopped,extract(epoch FROM execution_deadline-transport_deadline)::bigint AS gap FROM gw_requests WHERE accounting_state='unknown'").fetch_one(h.db.store.pool()).await.unwrap();
    assert!(row.get::<bool, _>("stopped"));
    assert_eq!(row.get::<i64, _>("gap"), 5);
    drop(tx);
    h.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn responses_transport_errors_strip_company_secrets_and_never_redirect_or_retry() {
    for kind in [
        "auth401",
        "auth403",
        "redirect",
        "encoding",
        "reset",
        "native_error",
    ] {
        let h = Harness::with_reset(verified_configuration(), kind == "reset").await;
        strict(&h).await;
        let target = provider::Provider::new();
        let target_server = target.serve().await;
        let (status,headers,body)=match kind{
   "auth401"|"auth403"=>(if kind=="auth401"{StatusCode::UNAUTHORIZED}else{StatusCode::FORBIDDEN},HeaderMap::from_iter([("content-type".parse().unwrap(),"application/json".parse().unwrap()),("www-authenticate".parse().unwrap(),"Bearer synthetic-secret".parse().unwrap())]),Bytes::from_static(b"synthetic-company-error-content")),
   "redirect"=>(StatusCode::TEMPORARY_REDIRECT,HeaderMap::from_iter([("location".parse().unwrap(),target_server.base.as_str().parse().unwrap())]),Bytes::new()),
   "encoding"=>(StatusCode::OK,HeaderMap::from_iter([("content-type".parse().unwrap(),"application/json".parse().unwrap()),("content-encoding".parse().unwrap(),"gzip".parse().unwrap())]),Bytes::from(native("completed"))),
   "native_error"=>(StatusCode::TOO_MANY_REQUESTS,HeaderMap::from_iter([("content-type".parse().unwrap(),"application/json".parse().unwrap()),("retry-after".parse().unwrap(),"2".parse().unwrap()),("set-cookie".parse().unwrap(),"synthetic-forbidden".parse().unwrap())]),Bytes::from_static(br#"{"error":{"message":"Synthetic provider rate","type":"rate_limit_error","param":null,"code":"rate_limit_exceeded"}}"#)),
   _=>(StatusCode::OK,HeaderMap::new(),Bytes::new())
  };
        h.provider.set(status, headers, body.clone()).await;
        let r = h
            .request("/openai/v1/responses", BODY)
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
        idle(&h).await;
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
    serde_json::json!([{"provider":"openai","model":"synthetic-text","capabilities":["responses"]}])
}
fn limits() -> serde_json::Value {
    serde_json::json!({"budget":null,"rpm":null,"tpm":null,"count_rpm":null})
}
async fn key(h: &Harness, service: Option<uuid::Uuid>) -> (String, uuid::Uuid) {
    let mut input = serde_json::json!({"name":"합성 Responses key","validity":"3_months","grants":grants(),"limits":{"budget":{"mode":"reserved","limit_usd":"1.000000000000","period":{"kind":"calendar_month","timezone":"UTC"}},"rpm":null,"tpm":100000,"count_rpm":null}});
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
async fn responses_personal_and_service_keys_apply_both_scopes_owner_and_revoke() {
    let h = Harness::with_config(verified_configuration()).await;
    strict(&h).await;
    h.provider
        .set(
            StatusCode::OK,
            HeaderMap::from_iter([(
                "content-type".parse().unwrap(),
                "application/json".parse().unwrap(),
            )]),
            Bytes::from(native("incomplete")),
        )
        .await;
    let (personal, personal_id) = key(&h, None).await;
    let r = h
        .request_with_token("/openai/v1/responses", BODY, &personal)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    idle(&h).await;
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
        .request_with_token("/openai/v1/responses", BODY, &service_key)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    idle(&h).await;
    sqlx::query("UPDATE gw_principals SET disabled_at=clock_timestamp(),disabled_reason='synthetic_owner_stop' WHERE id=$1").bind(h.principal).execute(h.db.store.pool()).await.unwrap();
    let r = h
        .request_with_token("/openai/v1/responses", BODY, &service_key)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    assert_eq!(r.headers()["x-gateway-error-code"], "owner_inactive");
    drop(r);
    idle(&h).await;
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
        .request_with_token("/openai/v1/responses", BODY, &personal)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    drop(r);
    idle(&h).await;
    assert_eq!(h.provider.calls.lock().await.len(), 2);
    h.close().await;
}
fn features_request() -> String {
    serde_json::json!({"model":"synthetic-text","store":false,"max_output_tokens":100,"input":[{"role":"user","content":[{"type":"input_text","text":"합성 text"},{"type":"input_image","detail":"auto","image_url":"data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScLbtAAAAABJRU5ErkJggg=="}]},{"type":"function_call","name":"f","call_id":"call_1","arguments":"opaque tool arguments"},{"type":"function_call_output","call_id":"call_1","output":"synthetic result"}],"instructions":"합성 instruction","tools":[{"type":"function","name":"f","description":"합성 client tool","parameters":{"type":"object","properties":{"previous_response_id":{"type":"string"}}},"strict":false}],"tool_choice":{"type":"function","name":"f"},"parallel_tool_calls":false,"reasoning":{"effort":"low","summary":"auto"},"text":{"format":{"type":"json_schema","name":"answer","schema":{"type":"object","properties":{"store":{"type":"boolean"}}},"strict":true}}}).to_string()
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn responses_native_image_function_reasoning_schema_and_alias_preserve_exact_wire() {
    for stream in [false, true] {
        let yaml = verified_yaml().replace(
            "actual_models:\n          - synthetic-text",
            "actual_models: [synthetic-text, synthetic-alias]",
        );
        let h = Harness::with_config(configuration_yaml(&yaml)).await;
        strict(&h).await;
        let mut request: serde_json::Value = serde_json::from_str(&features_request()).unwrap();
        request["stream"] = stream.into();
        let request = request.to_string();
        let mut response: serde_json::Value = serde_json::from_str(&native("completed")).unwrap();
        response["model"] = "synthetic-alias".into();
        response["output"] = serde_json::json!([{"type":"reasoning","id":"rs_synthetic","summary":[{"type":"summary_text","text":"합성 summary"}]},{"type":"function_call","id":"fc_synthetic","call_id":"call_2","name":"f","arguments":"{\"value\":1}","status":"completed"}]);
        let response = response.to_string();
        let wire = if stream {
            format!(
                "{}event: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"sequence_number\":1,\"output_index\":0,\"item\":{{\"type\":\"reasoning\",\"id\":\"rs_synthetic\",\"summary\":[{{\"type\":\"summary_text\",\"text\":\"합성 summary\"}}]}}}}\n\nevent: response.completed\ndata: {{\"type\":\"response.completed\",\"sequence_number\":2,\"response\":{response}}}\n\n",
                start().replace("synthetic-text", "synthetic-alias")
            )
        } else {
            response
        };
        h.provider
            .set_chunked(
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
                Bytes::from(wire.clone()),
            )
            .await;
        let r = h
            .request("/openai/v1/responses", request.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.bytes().await.unwrap(), wire);
        idle(&h).await;
        let calls = h.provider.calls.lock().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].body, request);
        drop(calls);
        let row=sqlx::query("SELECT r.accounting_state,r.cost_usd::text AS cost,r.pricing_snapshot,e.actual_model,e.normalized_usage FROM gw_requests r JOIN gw_usage_events e ON e.request_id=r.id AND e.kind='final'").fetch_one(h.db.store.pool()).await.unwrap();
        assert_eq!(row.get::<String, _>("accounting_state"), "settled");
        assert_eq!(row.get::<String, _>("cost"), "0.000120000000");
        assert_eq!(row.get::<String, _>("actual_model"), "synthetic-alias");
        let snapshot: serde_json::Value = row.get("pricing_snapshot");
        assert_eq!(
            snapshot["included_token_rules"],
            "openai_responses_input_includes_cache_output_includes_reasoning_v1"
        );
        assert!(!snapshot.to_string().contains("합성"));
        h.close().await;
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL and TEST_FNM_DIR, installed fixed SDK"]
async fn responses_sdk_consumes_large_completed_and_incomplete_terminal_once() {
    for status in ["completed", "incomplete"] {
        let h = Harness::with_config(verified_configuration()).await;
        strict(&h).await;
        let prefix = format!(
            "event: response.{status}\ndata: {{\"type\":\"response.{status}\",\"sequence_number\":1,\"response\":"
        );
        let suffix = "}\n\n";
        let json = padded_native(status, 16 * 1024 * 1024 - prefix.len() - suffix.len());
        let wire = format!("{}{prefix}{json}{suffix}", start());
        h.provider
            .set(
                StatusCode::OK,
                HeaderMap::from_iter([(
                    "content-type".parse().unwrap(),
                    "text/event-stream".parse().unwrap(),
                )]),
                Bytes::from(wire),
            )
            .await;
        sdk(&h, "responses_stream", 200, status, &h.token).await;
        idle(&h).await;
        assert_eq!(h.provider.calls.lock().await.len(), 1);
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT accounting_state FROM gw_requests")
                .fetch_one(h.db.store.pool())
                .await
                .unwrap(),
            "settled"
        );
        assert!(h.runtime.request_resources().parser_peak <= 256 * 1024);
        h.close().await;
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn responses_token_only_bound_violation_and_amount_overflow_pause_atomically() {
    for case in ["input_bound", "output_bound", "amount_overflow"] {
        let yaml = if case == "amount_overflow" {
            verified_yaml().replace(
                "input: '1.000000000000'",
                "input: '1000000000000.000000000000'",
            )
        } else {
            verified_yaml()
        };
        let h = Harness::with_config(configuration_yaml(&yaml)).await;
        let mut value: serde_json::Value = serde_json::from_str(&native("completed")).unwrap();
        let (input, output, cached) = match case {
            "input_bound" => (4097u64, 20u64, 4097u64),
            "output_bound" => (100, 101, 40),
            _ => (1_000_000_000_000_000_000, 20, 40),
        };
        value["usage"]["input_tokens"] = input.into();
        value["usage"]["output_tokens"] = output.into();
        value["usage"]["total_tokens"] = (input + output).into();
        value["usage"]["input_tokens_details"]["cached_tokens"] = cached.into();
        h.provider
            .set(
                StatusCode::OK,
                HeaderMap::from_iter([(
                    "content-type".parse().unwrap(),
                    "application/json".parse().unwrap(),
                )]),
                Bytes::from(value.to_string()),
            )
            .await;
        let r = h
            .request("/openai/v1/responses", BODY)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        r.bytes().await.unwrap();
        idle(&h).await;
        let row=sqlx::query("SELECT r.id,r.accounting_state,r.cost_usd::text AS cost,p.disabled_at IS NULL AS active,p.inference_pause_reason,p.inference_safety_version,r.safety_violation_recorded FROM gw_requests r JOIN gw_principals p ON p.id=r.principal_id").fetch_one(h.db.store.pool()).await.unwrap();
        assert_eq!(
            row.get::<String, _>("accounting_state"),
            if case == "amount_overflow" {
                "unknown"
            } else {
                "settled"
            }
        );
        assert_eq!(
            row.get::<String, _>("inference_pause_reason"),
            if case == "amount_overflow" {
                "amount_overflow"
            } else {
                "cost_bound_violation"
            }
        );
        assert_eq!(row.get::<i64, _>("inference_safety_version"), 2);
        assert!(row.get::<bool, _>("active"));
        assert!(row.get::<bool, _>("safety_violation_recorded"));
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM gw_audit_events WHERE actor_kind='system' AND operation='inference_auto_pause'").fetch_one(h.db.store.pool()).await.unwrap(),1);
        let r = h
            .request("/openai/v1/responses", BODY)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
        assert_eq!(r.headers()["x-gateway-error-code"], "inference_paused");
        drop(r);
        idle(&h).await;
        assert_eq!(h.provider.calls.lock().await.len(), 1);
        h.close().await;
    }
}
#[path = "support/finance.rs"]
mod finance;
#[path = "support/pg_proxy.rs"]
mod pg_proxy;
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn responses_uncertain_dispatch_original_id_expires_unknown_without_reissuing_permit() {
    use m_gated::{
        config::{ApiKind, ProviderId},
        ledger::finance::{Finalized, LedgerError, RequestSpec},
        providers::transport::{NativeClient, SendOptions},
    };
    use std::sync::atomic::Ordering;
    let db = postgres::TestDb::new().await;
    let principal = finance::human(db.store.pool()).await;
    sqlx::query("UPDATE gw_policies SET allow_rules=$2,budget_mode='reserved',budget_limit_usd=1,tpm_limit=100000 WHERE principal_id=$1").bind(principal).bind(grants()).execute(db.store.pool()).await.unwrap();
    let current = finance::current(db.store.pool(), principal).await;
    let proxy = pg_proxy::CommitProxy::new().await;
    let store = db.clock_store(Some(proxy.port)).await;
    let cfg = verified_configuration();
    let ledger = PgLedger::new(store.clone(), AccessSettings::from_config(&cfg), [1; 32]);
    let spec = RequestSpec::from_profile(
        ProviderId::Openai,
        &cfg.providers()[0].profiles[0],
        Some(100),
    );
    ledger.admit_request(&current, &spec).await.unwrap();
    let provider = provider::Provider::new();
    let server = provider.serve().await;
    let client =
        NativeClient::openai_loopback_for_test(server.base.clone(), "synthetic-company-key".into())
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
                        api: ApiKind::Responses,
                        headers: &headers,
                        profile: &cfg.providers()[0].profiles[0],
                        deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(1),
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
    assert_eq!(row.get::<String, _>("held"), "0.004296000000");
    assert!(row.get::<String, _>("period").starts_with("2025-03-01"));
    assert_eq!(row.get::<i64, _>("unknown_count"), 1);
    assert!(provider.calls.lock().await.is_empty());
    store.pool().close().await;
    healthy.pool().close().await;
    drop(proxy);
    db.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn responses_native_error_event_counts_final_crlf_in_64k_wire_limit() {
    for (size, crlf) in [
        (64 * 1024 - 1, true),
        (64 * 1024, true),
        (64 * 1024 + 1, true),
        (64 * 1024, false),
    ] {
        let h = Harness::with_config(verified_configuration()).await;
        strict(&h).await;
        let (tx, stopped) = h.provider.stream().await;
        let send = h
            .request(
                "/openai/v1/responses",
                BODY.replacen('{', "{\"stream\":true,", 1),
            )
            .send();
        let produce = async {
            tx.send(Bytes::from(start())).await.unwrap();
        };
        let (r, ()) = tokio::join!(send, produce);
        let r = r.unwrap();
        assert_eq!(r.status(), 200);
        let prefix = "event: error\r\ndata: {\"type\":\"error\",\"sequence_number\":1,\"code\":\"server_error\",\"message\":\"";
        let suffix = if crlf { "\"}\r\n\r\n" } else { "\"}\r\r" };
        let error = format!(
            "{prefix}{}{suffix}",
            "x".repeat(size - prefix.len() - suffix.len())
        );
        assert_eq!(error.len(), size);
        tx.send(Bytes::from(error.clone())).await.unwrap();
        drop(tx);
        let result = r.bytes().await;
        assert_eq!(
            result.is_ok(),
            size <= 64 * 1024,
            "native error 실제wire상한={size},crlf={crlf}"
        );
        if let Ok(bytes) = result {
            assert_eq!(bytes, format!("{}{error}", start()));
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), stopped)
            .await
            .unwrap()
            .unwrap();
        idle(&h).await;
        unknown(&h).await;
        h.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn responses_nonterminal_contradictions_hold_reservation_despite_later_valid_terminal() {
    let mut incorrectly_settled = 0;
    for kind in ["response.created", "response.in_progress"] {
        for (field, value) in [
            ("error", serde_json::json!({"code":"server_error"})),
            ("error", serde_json::json!(false)),
            ("error", serde_json::json!("synthetic")),
            ("status", serde_json::json!("failed")),
            ("status", serde_json::json!("completed")),
            ("status", serde_json::json!("incomplete")),
            ("status", serde_json::Value::Null),
            (
                "incomplete_details",
                serde_json::json!({"reason":"max_output_tokens"}),
            ),
            ("incomplete_details", serde_json::json!(false)),
            ("usage", serde_json::json!({})),
        ] {
            let h = Harness::with_config(verified_configuration()).await;
            strict(&h).await;
            let mut response = serde_json::json!({"id":"resp_fixture","object":"response","model":"synthetic-text","status":"in_progress","error":null,"incomplete_details":null,"usage":null});
            response[field] = value;
            let seq = u64::from(kind == "response.in_progress");
            let bad = serde_json::json!({"type":kind,"sequence_number":seq,"response":response});
            let tail = serde_json::json!({"type":"response.completed","sequence_number":seq+1,"response":serde_json::from_str::<serde_json::Value>(&native("completed")).unwrap()});
            let wire = format!(
                "{}data: {bad}\n\ndata: {tail}\n\n",
                if seq == 1 { start() } else { String::new() }
            );
            h.provider
                .set_chunked(
                    StatusCode::OK,
                    HeaderMap::from_iter([(
                        "content-type".parse().unwrap(),
                        "text/event-stream".parse().unwrap(),
                    )]),
                    Bytes::from(wire),
                )
                .await;
            let (started, release) = h.provider.hold_response().await;
            let body = BODY.replacen('{', "{\"stream\":true,", 1);
            let send = h.request("/openai/v1/responses", body).send();
            let barrier = async {
                started.await.unwrap();
                reservation(&h).await;
                release.send(()).unwrap();
            };
            let (r, ()) = tokio::join!(send, barrier);
            let r = r.unwrap();
            assert!([200, 502].contains(&r.status().as_u16()));
            let _ = r.bytes().await;
            idle(&h).await;
            let held = sqlx::query_scalar::<_, bool>("SELECT r.accounting_state='unknown' AND r.cost_usd IS NULL AND b.remaining_reservation_usd=0.004296 AND b.remaining_reserved_tokens=4196 AND t.reserved_usd=0.004296 AND t.unknown_count=1 AND w.reserved_tokens=4196 AND w.settled_tokens=0 FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id JOIN gw_usage_totals t ON (t.scope_id,t.period_start)=(b.scope_id,b.period_start) JOIN gw_rate_windows w ON (w.scope_id,w.lane,w.window_start)=(b.scope_id,b.rate_lane,b.rate_start)").fetch_one(h.db.store.pool()).await.unwrap();
            let next = h
                .request("/openai/v1/responses", BODY)
                .send()
                .await
                .unwrap();
            let blocked = next.status() == 403
                && next
                    .headers()
                    .get("x-gateway-error-code")
                    .is_some_and(|v| v == "unsettled_usage_block")
                && next
                    .headers()
                    .get("x-gateway-dispatch-state")
                    .is_some_and(|v| v == "not_dispatched");
            let _ = next.bytes().await;
            idle(&h).await;
            let calls = h.provider.calls.lock().await.len();
            incorrectly_settled += usize::from(!(held && blocked && calls == 1));
            h.close().await;
        }
    }
    assert_eq!(
        incorrectly_settled, 0,
        "상충 metadata 뒤 정산/예약 해제/후속 송신을 허용했다"
    );
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn responses_output_message_variants_reject_before_financial_start_for_any_role_and_order() {
    let h = Harness::with_config(verified_configuration()).await;
    let mut not_rejected = 0;
    for role in ["user", "system", "developer", "assistant"] {
        for variant in ["output_text", "refusal"] {
            for role_first in [false, true] {
                let field = if variant == "refusal" {
                    "refusal"
                } else {
                    "text"
                };
                let content = format!(r#"[{{"type":"{variant}","{field}":"synthetic"}}]"#);
                let item = if role_first {
                    format!(r#"{{"role":"{role}","content":{content}}}"#)
                } else {
                    format!(r#"{{"content":{content},"role":"{role}"}}"#)
                };
                let body = format!(
                    r#"{{"model":"synthetic-text","input":[{item}],"store":false,"max_output_tokens":1}}"#
                );
                let r = h
                    .request("/openai/v1/responses", body)
                    .send()
                    .await
                    .unwrap();
                let rejected = r.status() == 400
                    && r.headers()
                        .get("x-gateway-dispatch-state")
                        .is_some_and(|v| v == "not_dispatched");
                let v: serde_json::Value =
                    serde_json::from_slice(&r.bytes().await.unwrap()).unwrap();
                not_rejected +=
                    usize::from(!(rejected && v["error"]["type"] == "invalid_request_error"));
                idle(&h).await;
            }
        }
    }
    let calls = h.provider.calls.lock().await.len();
    let rows = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM gw_requests")
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    h.close().await;
    assert_eq!(
        (not_rejected, calls, rows),
        (0, 0, 0),
        "OutputMessage 입력은400/not_dispatched·송신0·시작행0이어야 한다"
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
            "responses",
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
async fn s9_responses_native_failure_sdk_abort_preserves_observed_failed() {
    for kind in ["response.failed", "error"] {
        let h = Harness::with_config(verified_configuration()).await;
        strict(&h).await;
        let (tx, stopped) = h.provider.stream().await;
        let failure = if kind == "error" {
            serde_json::json!({"type":"error","sequence_number":1,"code":"server_error","message":"synthetic error","param":null})
        } else {
            serde_json::json!({"type":"response.failed","sequence_number":1,"response":{"id":"resp_fixture","object":"response","model":"synthetic-text","status":"failed","usage":null,"error":{"code":"server_error","message":"synthetic error"},"incomplete_details":null}})
        };
        tx.send(Bytes::from(format!(
            "{}event: {kind}\ndata: {failure}\n\n",
            start()
        )))
        .await
        .unwrap();
        sdk(&h, "responses_stream_failed", 200, kind, &h.token).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), stopped)
            .await
            .unwrap()
            .unwrap();
        drop(tx);
        h.runtime.wait_requests().await;
        let row: (String, String, String, bool) = sqlx::query_as(
            "SELECT outcome,reason_code,accounting_state,cost_usd IS NULL FROM gw_requests",
        )
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
        assert_eq!(
            row,
            (
                "failed".into(),
                "provider_error".into(),
                "unknown".into(),
                true
            )
        );
        assert_eq!(h.provider.calls.lock().await.len(), 1);
        let held:(String,String,i64,i64)=sqlx::query_as("SELECT reservation_usd::text,remaining_reservation_usd::text,reserved_tokens,remaining_reserved_tokens FROM gw_request_budgets").fetch_one(h.db.store.pool()).await.unwrap();
        assert_eq!(
            held,
            ("0.004296000000".into(), "0.004296000000".into(), 4196, 4196)
        );
        assert_eq!(h.runtime.available_inference(), 16);
        assert_eq!(h.runtime.request_resources().body_bytes, 0);
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
            let h = Harness::with_config(verified_configuration()).await;
            strict(&h).await;
            let body = BODY.replacen('{', "{\"stream\":true,", 1);
            let prefix = start();
            let (tx, stopped) = h.provider.stream().await;
            if mode == "h1" {
                tx.send(Bytes::from(prefix.clone())).await.unwrap();
            } else {
                tx.send(Bytes::from(bad.clone())).await.unwrap();
            }
            if mode == "sdk" {
                error_compat::error_stream_sdk(&h.base, &h.token, "responses", 502, code, true)
                    .await;
            } else {
                let mut r = h
                    .request("/openai/v1/responses", body)
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
    let h = Harness::with_config(verified_configuration()).await;
    strict(&h).await;

    let body = BODY.replacen('{', "{\"stream\":true,", 1);
    let native = format!("{}{}", start(), terminal("completed"));
    let (tx, stopped) = h.provider.stream().await;
    tx.send(Bytes::from(native.clone())).await.unwrap();
    let mut r = h
        .request("/openai/v1/responses", body.clone())
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
        .request("/openai/v1/responses", body)
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
    let h = Harness::with_config(verified_configuration()).await;
    strict(&h).await;

    let body = BODY.replacen('{', "{\"stream\":true,", 1);
    let native = format!(
        "{}{}",
        start(),
        terminal("completed").replace(
            "\"status\":\"completed\"",
            "\"status\":\"completed\",\"service_tier\":\"unverified\""
        )
    );
    let (tx, stopped) = h.provider.stream().await;
    tx.send(Bytes::from(native.clone())).await.unwrap();
    let mut r = h
        .request("/openai/v1/responses", body.clone())
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
        .request("/openai/v1/responses", body)
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

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s9_terminal_cr_boundary_requires_optional_lf_wire_validation_before_settlement() {
    for ending in ["eof", "next_non_lf", "optional_lf"] {
        let h = Harness::with_config(verified_configuration()).await;
        strict(&h).await;
        let head = "event: response.completed\rdata: {\"type\":\"response.completed\",\"sequence_number\":1,\"response\":";
        let tail = "}\r\r";
        let object = padded_native("completed", 16 * 1024 * 1024 - head.len() - tail.len());
        let mut wire = format!("{}{head}{object}{tail}", start());
        if ending == "next_non_lf" {
            wire.push_str(&format!("event: {}\n", "x".repeat(129)));
        }
        if ending == "optional_lf" {
            wire.push('\n');
        }
        h.provider
            .set(
                StatusCode::OK,
                HeaderMap::from_iter([(
                    "content-type".parse().unwrap(),
                    "text/event-stream".parse().unwrap(),
                )]),
                Bytes::from(wire.clone()),
            )
            .await;
        let r = h
            .request(
                "/openai/v1/responses",
                BODY.replacen('{', "{\"stream\":true,", 1),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let bytes = r.bytes().await;
        if ending == "eof" {
            assert_eq!(bytes.unwrap(), wire.as_bytes());
        } else {
            assert!(bytes.is_err(), "상한 초과를 성공 EOF로 바꾸지 않음");
        }
        idle(&h).await;
        let row: (String, String, Option<String>) =
            sqlx::query_as("SELECT accounting_state,reason_code,cost_usd::text FROM gw_requests")
                .fetch_one(h.db.store.pool())
                .await
                .unwrap();
        assert_eq!(
            row,
            if ending == "optional_lf" {
                ("unknown".into(), "stream_limit".into(), None)
            } else {
                (
                    "settled".into(),
                    if ending == "eof" {
                        "completed".into()
                    } else {
                        "stream_limit".into()
                    },
                    Some("0.000120000000".into()),
                )
            }
        );
        assert_eq!(h.provider.calls.lock().await.len(), 1);
        assert_eq!(h.runtime.request_resources().body_bytes, 0);
        h.close().await;
    }
}

#[path = "support/s9_finality.rs"]
mod s9_finality;
async fn s9_finality_fixture(
    incomplete: bool,
) -> (Harness, &'static str, String, String, String, &'static str) {
    let h = Harness::with_config(verified_configuration()).await;
    strict(&h).await;
    (
        h,
        "/openai/v1/responses",
        BODY.replacen('{', "{\"stream\":true,", 1),
        format!(
            "{}{}",
            start(),
            terminal(if incomplete {
                "incomplete"
            } else {
                "completed"
            })
        ),
        start(),
        "0.000120000000",
    )
}

fn s9_native_error_tail(length: usize) -> String {
    format!(
        "event: error\ndata: {{\"type\":\"error\",\"sequence_number\":2,\"code\":\"server_error\",\"message\":\"{}\"}}\n\n",
        "x".repeat(length)
    )
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s9_terminal_at_cr_wire_limit_before_cancel_has_no_supervisor_final() {
    for shutdown in [false, true] {
        let h = Harness::with_config(verified_configuration()).await;
        strict(&h).await;
        let head = "event: response.completed\rdata: {\"type\":\"response.completed\",\"sequence_number\":1,\"response\":";
        let tail = "}\r\r";
        let object = padded_native("completed", 16 * 1024 * 1024 - head.len() - tail.len());
        let wire = format!("{}{head}{object}{tail}", start());
        let (tx, stopped) = h.provider.stream().await;
        tx.send(Bytes::from(wire.clone())).await.unwrap();
        let body = BODY.replacen('{', "{\"stream\":true,", 1);
        let mut r = h
            .request("/openai/v1/responses", body.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let mut received = 0;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while received < wire.len() {
                received += r.chunk().await.unwrap().unwrap().len();
            }
        })
        .await
        .unwrap();
        assert_eq!(received, wire.len());
        if shutdown {
            h.runtime.cancel_requests();
        } // 본문 drop과 독립한 runtime 종료 신호
        drop(r);
        stopped.await.unwrap();
        drop(tx);
        h.runtime.wait_requests().await;
        let row:(String,bool,i64,String,i64)=sqlx::query_as("SELECT r.accounting_state,r.cost_usd IS NULL,(SELECT count(*) FROM gw_usage_events WHERE request_id=r.id AND kind='final'),b.remaining_reservation_usd::text,b.remaining_reserved_tokens FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id").fetch_one(h.db.store.pool()).await.unwrap();
        let resources = h.runtime.request_resources();
        h.close().await;
        assert_eq!(
            row,
            ("unknown".into(), true, 0, "0.004296000000".into(), 4196)
        );
        assert_eq!(resources.body_bytes, 0);
        assert_eq!(resources.queue_bytes, 0);
        assert!(resources.parser_peak <= 256 * 1024);
    }
}
