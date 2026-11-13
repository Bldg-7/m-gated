#[path = "support/postgres.rs"]
mod postgres;
use sqlx::{Row, ValueRef};

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn finance_schema_and_amount_boundary_are_real_sql() {
    let db = postgres::TestDb::new().await;
    let exists: bool = sqlx::query_scalar("SELECT to_regclass('gw_requests') IS NOT NULL")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    if !exists {
        db.close().await;
        panic!("financial request ledger missing");
    }
    for bad in [
        "-0.0000000000001",
        "0.0000000000001",
        "NaN",
        "Infinity",
        "10000000000000000",
    ] {
        assert!(
            sqlx::query("SELECT gw_amount($1::numeric)")
                .bind(bad)
                .execute(db.store.pool())
                .await
                .is_err(),
            "SQL amount boundary accepted rounding/overflow"
        );
    }
    let r = sqlx::query("SELECT gw_amount('0.000000000001'::numeric)::text AS amount")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    assert_eq!(r.get::<String, _>("amount"), "0.000000000001");
    db.close().await;
}

#[path = "support/finance.rs"]
mod finance;
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn request_check_rejects_finished_pending_and_null_bypasses() {
    let db = postgres::TestDb::new().await;
    let principal = finance::human(db.store.pool()).await;
    let mut accepted = 0;
    for phase in ["admitted", "dispatching", "finished"] {
        for accounting in ["pending", "settled", "unknown"] {
            for outcome in [
                None,
                Some("completed"),
                Some("rejected"),
                Some("failed"),
                Some("cancelled"),
            ] {
                for cost in [
                    None,
                    Some("0.000000000000"),
                    Some("1.000000000000"),
                    Some("-0.000000000001"),
                ] {
                    for finished in [false, true] {
                        for dispatched in [false, true] {
                            for lease in [false, true] {
                                let expected = match phase {
                                    "admitted" => {
                                        accounting == "pending"
                                            && outcome.is_none()
                                            && cost.is_none()
                                            && !finished
                                            && !dispatched
                                            && !lease
                                    }
                                    "dispatching" => {
                                        accounting == "pending"
                                            && outcome.is_none()
                                            && cost.is_none()
                                            && !finished
                                            && dispatched
                                            && lease
                                    }
                                    _ => {
                                        outcome.is_some()
                                            && finished
                                            && ((accounting == "settled"
                                                && cost.is_some_and(|n| !n.starts_with('-')))
                                                || (accounting == "unknown" && cost.is_none()))
                                            && ((dispatched && lease)
                                                || (!dispatched
                                                    && !lease
                                                    && accounting == "settled"
                                                    && cost == Some("0.000000000000")
                                                    && outcome != Some("completed")))
                                    }
                                };
                                let mut tx = db.store.pool().begin().await.unwrap();
                                let result = sqlx::query("INSERT INTO gw_requests(id,principal_id,purpose,provider,requested_model,api,admitted_at,admit_expires_at,transport_deadline,execution_deadline,executor_id,phase,outcome,accounting_state,cost_usd,finished_at,dispatch_authorized_at,lease_expires_at,pricing_snapshot,execution_snapshot,auth_config_digest) SELECT $1,$2,'root','anthropic','synthetic-text','messages',t,t+interval '60 seconds',t+interval '895 seconds',t+interval '900 seconds',$1,$3,$4,$5,$6::numeric,CASE WHEN $7 THEN t+interval '2 seconds' END,CASE WHEN $8 THEN t+interval '1 second' END,CASE WHEN $9 THEN t+interval '900 seconds' END,'{}','{}',decode(repeat('00',32),'hex') FROM (SELECT clock_timestamp() t) n")
                                    .bind(uuid::Uuid::new_v4()).bind(principal).bind(phase).bind(outcome).bind(accounting).bind(cost).bind(finished).bind(dispatched).bind(lease).execute(&mut *tx).await;
                                let actual = result.is_ok();
                                tx.rollback().await.unwrap();
                                assert_eq!(
                                    actual, expected,
                                    "phase={phase} accounting={accounting} outcome={outcome:?} cost={cost:?} finished={finished} dispatch={dispatched} lease={lease}"
                                );
                                accepted += usize::from(actual);
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(accepted, 17); // admitted1+dispatching1+finished(4*3+3)
    db.close().await;
}

mod support;
use m_gated::{
    auth::AccessSettings,
    config::{self, ConfigInputs, ProviderId},
    ledger::{
        PgLedger,
        finance::{FinalObservation, FinalUsage, Finalized, LedgerError, RequestSpec},
    },
};
fn configuration() -> config::ValidatedConfig {
    configuration_with_count_response_limit(256 * 1024)
}
fn configuration_with_count_response_limit(limit: usize) -> config::ValidatedConfig {
    configuration_from_yaml(&finance_yaml().replace(
        "max_response_bytes: 262144",
        &format!("max_response_bytes: {limit}"),
    ))
}
fn finance_yaml() -> String {
    format!(
        "{}{}",
        support::YAML,
        r#"      - model: synthetic-text
        api: count_tokens
        profile_version: count-fixture-v1
        pricing_version: free-price-v1
        fixture_version: free-count-v1
        actual_models: [synthetic-text]
        billing_class: guaranteed_free
        max_input_tokens: 4096
        max_body_bytes: 8388608
        max_response_bytes: 262144
        rates:
          input: '0.000000000000'
"#
    )
}
fn configuration_from_yaml(yaml: &str) -> config::ValidatedConfig {
    let mut env = support::environment();
    env.insert("ALLOWED_EMAILS".into(), "alice@example.test".into());
    config::load(ConfigInputs {
        yaml,
        file_environment: None,
        environment: &env,
    })
    .unwrap()
}
fn specification() -> RequestSpec {
    RequestSpec::from_profile(
        ProviderId::Anthropic,
        &configuration().providers()[0].profiles[0],
        Some(100),
    )
}
async fn assert_first_usage_event_kind(pool: &sqlx::PgPool, id: uuid::Uuid, expected: &str) {
    let kinds: Vec<String> =
        sqlx::query_scalar("SELECT kind FROM gw_usage_events WHERE request_id=$1 ORDER BY kind")
            .bind(id)
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(kinds, vec![expected], "first observation kind for {id}");
}
fn observation() -> FinalObservation {
    FinalObservation::completed(FinalUsage {
        actual_model: "synthetic-text".into(),
        input_tokens: 100,
        output_tokens: 20,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: None,
    })
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn allowed_anthropic_request_is_recorded_once() {
    let db = postgres::TestDb::new().await;
    let id = finance::human(db.store.pool()).await;
    let current = finance::current(db.store.pool(), id).await;
    let ledger = PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    );
    let spec = specification();
    assert_eq!(
        ledger.admit_request(&current, &spec).await.unwrap(),
        spec.id
    );
    let _permit = ledger.authorize_dispatch(spec.id).await.unwrap();
    assert!(matches!(
        ledger.authorize_dispatch(spec.id).await,
        Err(LedgerError::Conflict)
    ));
    let final_usage = observation();
    assert_eq!(
        ledger
            .finalize_request(spec.id, &final_usage)
            .await
            .unwrap(),
        Finalized::Settled
    );
    assert_eq!(
        ledger
            .finalize_request(spec.id, &final_usage)
            .await
            .unwrap(),
        Finalized::Noop
    );
    let mut conflicting = observation();
    conflicting.usage.as_mut().unwrap().output_tokens += 1;
    assert_eq!(
        ledger
            .finalize_request(spec.id, &conflicting)
            .await
            .unwrap_err(),
        LedgerError::Conflict
    );
    let row=sqlx::query("SELECT r.phase,r.accounting_state,r.cost_usd::text AS cost,t.cost_usd::text AS total,t.settled_count,t.unknown_count,t.input_tokens,t.output_tokens,(SELECT count(*) FROM gw_usage_events) AS events FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id JOIN gw_usage_totals t ON t.scope_id=b.scope_id AND t.period_start=b.period_start WHERE r.id=$1").bind(spec.id).fetch_one(db.store.pool()).await.unwrap();
    assert_eq!(row.get::<String, _>("cost"), "0.000140000000");
    assert_eq!(row.get::<String, _>("total"), "0.000140000000");
    assert_eq!(row.get::<String, _>("accounting_state"), "settled");
    assert_eq!(row.get::<i64, _>("settled_count"), 1);
    assert_eq!(row.get::<i64, _>("unknown_count"), 0);
    assert_eq!(row.get::<i64, _>("events"), 1);
    assert_eq!(row.get::<i64, _>("input_tokens"), 100);
    assert_eq!(row.get::<i64, _>("output_tokens"), 20);
    db.close().await;
}

#[path = "support/provider.rs"]
mod provider;
// 다른 auth target과 공유하는 fixture의 일부 API만 사용한다.
#[allow(dead_code)]
#[path = "support/sso.rs"]
mod sso;
use axum::{
    body::Bytes,
    http::{HeaderMap, StatusCode},
};
use std::sync::Arc;
type ObservedGatewayErrors = Arc<tokio::sync::Mutex<Vec<(u16, Option<String>)>>>;
struct Harness {
    db: postgres::TestDb,
    database: Arc<m_gated::ledger::PgIdentityStore>,
    // 읽는 시험이 macOS 전용이라 다른 OS에서는 dead code가 된다.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    wire: ObservedGatewayErrors,
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
        sqlx::query("UPDATE gw_policies SET allow_rules=$2 WHERE id=$1").bind(current.policy_id).bind(serde_json::json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages","count_tokens"]}])).execute(database.pool()).await.unwrap();
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
        let client = AnthropicClient::loopback_for_test(
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
        let app = bootstrap::build(
            cfg,
            Dependencies::unassembled()
                .with_authentication(auth)
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
        let wire = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let capture = wire.clone();
        let router = app.router().layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let capture = capture.clone();
                async move {
                    let response = next.run(request).await;
                    capture.lock().await.push((
                        response.status().as_u16(),
                        response
                            .headers()
                            .get("x-gateway-error-code")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned),
                    ));
                    response
                }
            },
        ));
        let task = tokio::spawn(m_gated::server::serve(
            listener,
            router,
            runtime.clone(),
            std::future::pending(),
        ));
        Self {
            db,
            database,
            wire,
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
        reqwest::Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .body(body)
    }
    async fn close(self) {
        self.task.abort();
        let _ = self.task.await;
        self.database.pool().close().await;
        self.db.close().await;
    }
}
const BODY: &str = " { \"messages\" : [{\"role\":\"user\", \"content\":\"synthetic prompt sentinel\"}],\"max_tokens\":100,\"model\":\"synthetic-text\" } ";
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn native_anthropic_and_alias_preserve_bytes_credentials_and_accounting() {
    let h = Harness::new().await;
    let escaped = BODY
        .replace("\"model\"", "\"mo\\u0064el\"")
        .replace("synthetic-text", "synthetic-\\u0074ext");
    let cases = [
        ("/anthropic/v1/messages", BODY.to_owned()),
        ("/v1/messages", escaped),
    ];
    for (path, body) in &cases {
        let response = h
            .request(path, body.clone())
            .header("cookie", "mgated_access=synthetic-private-cookie")
            .header("x-gateway-request-id", "client-id")
            .header("idempotency-key", "never-forward")
            .header("baggage", "private")
            .header("forwarded", "for=private")
            .header("openai-project", "client-project")
            .header("connection", "x-private")
            .header("x-private", "private")
            .header("x-stainless-retry-count", "2")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["request-id"], "provider-synthetic-id");
        assert_ne!(response.headers()["x-gateway-request-id"], "client-id");
        assert!(!response.headers().contains_key("x-gateway-dispatch-state"));
        assert_eq!(response.headers()["x-should-retry"], "false");
        let request_id = response.headers()["x-gateway-request-id"]
            .to_str()
            .unwrap()
            .parse::<uuid::Uuid>()
            .unwrap();
        let bytes = response.bytes().await.unwrap();
        assert!(
            bytes
                .windows(b"synthetic response".len())
                .any(|s| s == b"synthetic response")
        );
        let cost: String = sqlx::query_scalar("SELECT cost_usd::text FROM gw_requests WHERE id=$1")
            .bind(request_id)
            .fetch_one(h.db.store.pool())
            .await
            .unwrap();
        assert_eq!(cost, "0.000140000000");
    }
    let calls = h.provider.calls.lock().await;
    assert_eq!(calls.len(), 2);
    for (call, (_, body)) in calls.iter().zip(&cases) {
        assert_eq!(call.method, "POST");
        assert_eq!(call.path, "/v1/messages");
        assert!(
            call.body.as_ref() == body.as_bytes(),
            "approved escaped/reordered original bytes were rewritten"
        );
        assert_eq!(call.headers["x-api-key"], "synthetic-company-key");
        assert_eq!(call.headers["anthropic-version"], "2023-06-01");
        for forbidden in [
            "authorization",
            "cookie",
            "idempotency-key",
            "baggage",
            "forwarded",
            "openai-project",
            "x-private",
            "x-stainless-retry-count",
            "x-gateway-request-id",
            "accept-encoding",
        ] {
            assert!(
                !call.headers.contains_key(forbidden),
                "forbidden client header forwarded"
            );
        }
    }
    drop(calls);
    let row = sqlx::query(
        "SELECT sum(cost_usd)::text AS cost,sum(settled_count)::bigint AS n FROM gw_usage_totals",
    )
    .fetch_one(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("cost"), "0.000280000000");
    assert_eq!(row.get::<i64, _>("n"), 2);
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn invalid_native_requests_never_create_start_or_send() {
    let h = Harness::new().await;
    let deep = format!(
        r#"{{"model":"synthetic-text","max_tokens":1,"messages":[{{"role":"user","content":"x"}}],"metadata":{}0{}}}"#,
        "[".repeat(33),
        "]".repeat(33)
    );
    for body in [
        r#"{"model":"synthetic-text","\u006dodel":"other","max_tokens":1,"messages":[]}"#,
        r#"{"model":"synthetic-text","max_tokens":1,"messages":[{"role":"user","content":[{"type":"tool_use","id":"a","name":"f","input":{"x":1,"\u0078":2}}]}]}"#,
        r#"{"model":"synthetic-text","max_tokens":1,"messages":[{"role":"user","content":"x"}],"stream":null}"#,
        r#"{"model":"synthetic-text","max_tokens":0,"messages":[]}"#,
        r#"{"model":"unknown","max_tokens":1,"messages":[]}"#,
        r#"{"model":"synthetic-text","max_tokens":1,"messages":[{"role":"user","content":[{"type":"document","source":{"type":"url","url":"http://example.test"}}]}]}"#,
        &deep,
        &format!("{BODY} null"),
    ] {
        let r = h
            .request("/v1/messages", body.to_owned())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert_eq!(r.headers()["x-gateway-dispatch-state"], "not_dispatched");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&r.bytes().await.unwrap()).unwrap()["type"],
            "error"
        );
    }
    for path in ["/v1/messages", "/anthropic/v1/messages"] {
        for stream in ["null", "1", "\"true\"", "[]", "{}"] {
            let body = format!(
                "{{\"model\":\"synthetic-text\",\"max_tokens\":1,\"messages\":[{{\"role\":\"user\",\"content\":\"x\"}}],\"stream\":{stream}}}"
            );
            let r = h.request(path, body).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::BAD_REQUEST);
            assert_eq!(r.headers()["x-gateway-dispatch-state"], "not_dispatched");
        }
    }
    for path in [
        "/v1/messages/count_tokens",
        "/anthropic/v1/messages/count_tokens",
    ] {
        for stream in ["true", "false", "null"] {
            let body = format!(
                "{{\"model\":\"synthetic-text\",\"messages\":[{{\"role\":\"user\",\"content\":\"x\"}}],\"stream\":{stream}}}"
            );
            let r = h.request(path, body).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::BAD_REQUEST);
            assert_eq!(r.headers()["x-gateway-dispatch-state"], "not_dispatched");
        }
    }
    for header in [
        ("anthropic-version", "2099-01-01"),
        ("anthropic-beta", "unknown-beta"),
        ("content-encoding", "gzip"),
    ] {
        let r = h
            .request("/v1/messages", BODY)
            .header(header.0, header.1)
            .send()
            .await
            .unwrap();
        assert!(r.status().is_client_error());
    }
    assert!(h.provider.calls.lock().await.is_empty());
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_requests")
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    assert_eq!(rows, 0);
    h.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn provider_auth_redirect_encoding_and_retry_have_no_second_send() {
    for (status,extra,body,expected,code) in [
        (401,None,br#"{"error":{"message":"company credential sentinel"}}"#.as_slice(),502,"upstream_auth_error"),
        (403,None,br#"{"error":{"message":"company credential sentinel"}}"#.as_slice(),502,"upstream_auth_error"),
        (307,Some(("location","/redirect-target")),b"".as_slice(),502,"upstream_redirect"),
        (200,Some(("content-encoding","gzip")),b"gzip must not be transparently decoded".as_slice(),502,"upstream_encoding_error"),
        (503,Some(("retry-after","1")),br#"{"type":"error","error":{"type":"overloaded_error","message":"temporary synthetic error"}}"#.as_slice(),503,""),
    ] {
        let h=Harness::new().await;let mut headers=HeaderMap::new();headers.insert("content-type","application/json".parse().unwrap());headers.insert("set-cookie","private=never".parse().unwrap());headers.insert("www-authenticate","never".parse().unwrap());if let Some((name,value))=extra{headers.insert(name,value.parse().unwrap());}
        h.provider.set(StatusCode::from_u16(status).unwrap(),headers,Bytes::copy_from_slice(body)).await;
        let r=h.request("/v1/messages",BODY).send().await.unwrap();assert_eq!(r.status().as_u16(),expected);assert_eq!(r.headers()["x-should-retry"],"false");assert!(!r.headers().contains_key("x-gateway-dispatch-state"));
        for name in ["set-cookie","www-authenticate","location","content-encoding"]{assert!(!r.headers().contains_key(name));}
        if !code.is_empty(){assert_eq!(r.headers()["x-gateway-error-code"],code);}
        let bytes=r.bytes().await.unwrap();assert!(!bytes.windows(b"company credential sentinel".len()).any(|b|b==b"company credential sentinel"));
        assert_eq!(h.provider.calls.lock().await.len(),1);
        let state:String=sqlx::query_scalar("SELECT accounting_state FROM gw_requests WHERE principal_id=$1").bind(h.principal).fetch_one(h.db.store.pool()).await.unwrap();assert_eq!(state,"unknown");
        let blocked=h.request("/anthropic/v1/messages",BODY).send().await.unwrap();assert_eq!(blocked.status(),StatusCode::FORBIDDEN);assert_eq!(blocked.headers()["x-gateway-error-code"],"unsettled_usage_block");assert_eq!(h.provider.calls.lock().await.len(),1);
        h.close().await;
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn missing_usage_unknown_model_and_price_remain_native_but_block_paid_calls() {
    for usage in [
        serde_json::Value::Null,
        serde_json::json!({"input_tokens":100}),
        serde_json::json!({"input_tokens":100,"output_tokens":20,"cache_creation_input_tokens":2}),
        serde_json::json!({"input_tokens":100,"output_tokens":20,"new_unapproved_usage":1}),
    ] {
        let h = Harness::new().await;
        let response = serde_json::json!({"id":"msg_fixture","type":"message","role":"assistant","model":"synthetic-text","content":[],"stop_reason":"end_turn","usage":usage});
        let bytes = serde_json::to_vec(&response).unwrap();
        h.provider
            .set(
                StatusCode::OK,
                HeaderMap::from_iter([(
                    "content-type".parse().unwrap(),
                    "application/json".parse().unwrap(),
                )]),
                Bytes::from(bytes.clone()),
            )
            .await;
        let r = h.request("/v1/messages", BODY).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let id: uuid::Uuid = r.headers()["x-gateway-request-id"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(r.bytes().await.unwrap().as_ref(), bytes);
        let row=sqlx::query("SELECT accounting_state,cost_usd IS NULL AS unresolved FROM gw_requests WHERE principal_id=$1").bind(h.principal).fetch_one(h.db.store.pool()).await.unwrap();
        assert_eq!(row.get::<String, _>("accounting_state"), "unknown");
        assert!(row.get::<bool, _>("unresolved"));
        let r = h.request("/v1/messages", BODY).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert_eq!(h.provider.calls.lock().await.len(), 1);
        let expected_kind = if usage.get("cache_creation_input_tokens").is_some() {
            "final"
        } else {
            "partial"
        };
        assert_first_usage_event_kind(h.db.store.pool(), id, expected_kind).await;
        if expected_kind == "final" {
            let event = sqlx::query("SELECT provider_usage,normalized_usage,pricing_resolution FROM gw_usage_events WHERE request_id=$1").bind(id).fetch_one(h.db.store.pool()).await.unwrap();
            assert_eq!(
                event.get::<serde_json::Value, _>("provider_usage")["cache_creation_input_tokens"],
                2
            );
            assert_eq!(
                event.get::<serde_json::Value, _>("normalized_usage")["cache_write_tokens"],
                "2"
            );
            let resolution: serde_json::Value = event.get("pricing_resolution");
            assert!(resolution["cost_usd"].is_null());
            assert_eq!(resolution["unresolved_reason"], "price_unresolved");
        }
        h.close().await;
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn durable_start_failure_is_not_a_provider_attempt() {
    let h = Harness::new().await;
    sqlx::raw_sql("CREATE FUNCTION gw_test_fail_start() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic insert failure'; END $$; CREATE TRIGGER gw_test_fail_start BEFORE INSERT ON gw_requests FOR EACH ROW EXECUTE FUNCTION gw_test_fail_start();").execute(h.db.store.pool()).await.unwrap();
    let r = h.request("/v1/messages", BODY).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(r.headers()["x-gateway-dispatch-state"], "not_dispatched");
    assert!(h.provider.calls.lock().await.is_empty());
    for table in [
        "gw_requests",
        "gw_request_budgets",
        "gw_usage_totals",
        "gw_rate_windows",
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(h.db.store.pool())
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn free_count_result_and_lost_result_are_not_paid_usage_or_missing_tokens() {
    let h = Harness::new().await;
    let headers = HeaderMap::from_iter([(
        "content-type".parse().unwrap(),
        "application/json".parse().unwrap(),
    )]);
    h.provider.set(StatusCode::OK,headers.clone(),Bytes::from_static(br#"{"type":"message","role":"assistant","model":"synthetic-text","content":[],"stop_reason":"end_turn"}"#)).await;
    assert_eq!(
        h.request("/v1/messages", BODY)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    for (path, status, body, expected) in [
        (
            "/anthropic/v1/messages/count_tokens",
            200,
            br#"{"input_tokens":12}"#.as_slice(),
            Some("12"),
        ),
        (
            "/v1/messages/count_tokens",
            401,
            b"company credential sentinel".as_slice(),
            None,
        ),
    ] {
        h.provider
            .set(
                StatusCode::from_u16(status).unwrap(),
                headers.clone(),
                Bytes::copy_from_slice(body),
            )
            .await;
        let r=h.request(path,r#"{"model":"synthetic-text","messages":[{"role":"user","content":"synthetic count"}]}"#).send().await.unwrap();
        assert_eq!(r.status().as_u16(), if status == 200 { 200 } else { 502 });
        let id: uuid::Uuid = r.headers()["x-gateway-request-id"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let row=sqlx::query("SELECT r.accounting_state,r.cost_usd::text AS cost,e.normalized_usage FROM gw_requests r JOIN gw_usage_events e ON e.request_id=r.id WHERE r.id=$1").bind(id).fetch_one(h.db.store.pool()).await.unwrap();
        assert_eq!(row.get::<String, _>("accounting_state"), "settled");
        assert_eq!(row.get::<String, _>("cost"), "0.000000000000");
        let usage: serde_json::Value = row.get("normalized_usage");
        assert_eq!(usage["source"], "profile_no_charge");
        assert_eq!(usage["input_tokens"], "0");
        assert_eq!(usage["output_tokens"], "0");
        assert_eq!(usage["counted_input_tokens"].as_str(), expected);
        assert_first_usage_event_kind(h.db.store.pool(), id, "final").await;
    }
    let row=sqlx::query("SELECT unknown_count,settled_count,settled_unknown_tokens_count,input_tokens,output_tokens FROM gw_usage_totals").fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(row.get::<i64, _>("unknown_count"), 1);
    assert_eq!(row.get::<i64, _>("settled_count"), 2);
    assert_eq!(row.get::<i64, _>("settled_unknown_tokens_count"), 0);
    assert_eq!(row.get::<i64, _>("input_tokens"), 0);
    assert_eq!(row.get::<i64, _>("output_tokens"), 0);
    let calls = h.provider.calls.lock().await;
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[1].path, "/v1/messages/count_tokens");
    assert_eq!(calls[2].path, "/v1/messages/count_tokens");
    drop(calls);
    h.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn fixed_price_and_no_dispatch_fact_do_not_rewrite_prior_usage() {
    let db = postgres::TestDb::new().await;
    let id = finance::human(db.store.pool()).await;
    let current = finance::current(db.store.pool(), id).await;
    let ledger = PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    );
    let spec = specification();
    ledger.admit_request(&current, &spec).await.unwrap();
    let _p = ledger.authorize_dispatch(spec.id).await.unwrap();
    let mut new_profile = configuration().providers()[0].profiles[0].clone();
    new_profile
        .rates
        .insert("input".into(), "999.000000000000".into());
    let changed = RequestSpec::from_profile(ProviderId::Anthropic, &new_profile, Some(100));
    assert_ne!(changed.pricing, spec.pricing);
    ledger
        .finalize_request(spec.id, &observation())
        .await
        .unwrap();
    let no_send = specification();
    ledger.admit_request(&current, &no_send).await.unwrap();
    sqlx::query("UPDATE gw_policies SET version=version+1 WHERE id=$1")
        .bind(current.policy_id)
        .execute(db.store.pool())
        .await
        .unwrap();
    assert!(matches!(
        ledger.authorize_dispatch(no_send.id).await,
        Err(LedgerError::PolicyChanged)
    ));
    let no_dispatch = FinalObservation {
        outcome: m_gated::ledger::finance::FinalOutcome::Rejected,
        usage: None,
        counted_input_tokens: None,
        reason: "policy_changed",
        http_status: None,
        provider_request_id: None,
    };
    ledger
        .finalize_request(no_send.id, &no_dispatch)
        .await
        .unwrap();
    let e: serde_json::Value =
        sqlx::query_scalar("SELECT normalized_usage FROM gw_usage_events WHERE request_id=$1")
            .bind(no_send.id)
            .fetch_one(db.store.pool())
            .await
            .unwrap();
    assert_first_usage_event_kind(db.store.pool(), no_send.id, "final").await;
    assert_eq!(e["source"], "gateway_no_dispatch");
    assert_eq!(e["input_tokens"], "0");
    assert_eq!(e["output_tokens"], "0");
    assert!(e["actual_model"].is_null());
    let row=sqlx::query("SELECT cost_usd::text AS cost,settled_count,settled_unknown_tokens_count,input_tokens,output_tokens FROM gw_usage_totals").fetch_one(db.store.pool()).await.unwrap();
    assert_eq!(row.get::<String, _>("cost"), "0.000140000000");
    assert_eq!(row.get::<i64, _>("settled_count"), 2);
    assert_eq!(row.get::<i64, _>("settled_unknown_tokens_count"), 0);
    assert_eq!(row.get::<i64, _>("input_tokens"), 100);
    assert_eq!(row.get::<i64, _>("output_tokens"), 20);
    db.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn two_finalizers_apply_one_cost_and_conflicting_final_rolls_back() {
    let db = postgres::TestDb::new().await;
    let id = finance::human(db.store.pool()).await;
    let current = finance::current(db.store.pool(), id).await;
    let ledger = Arc::new(PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    ));
    let spec = specification();
    ledger.admit_request(&current, &spec).await.unwrap();
    let _p = ledger.authorize_dispatch(spec.id).await.unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let mut jobs = Vec::new();
    for _ in 0..2 {
        let b = barrier.clone();
        let l = ledger.clone();
        let id = spec.id;
        jobs.push(tokio::spawn(async move {
            b.wait().await;
            l.finalize_request(id, &observation()).await.unwrap()
        }));
    }
    barrier.wait().await;
    let mut outcomes = Vec::new();
    for j in jobs {
        outcomes.push(j.await.unwrap());
    }
    assert!(outcomes.contains(&Finalized::Settled));
    assert!(outcomes.contains(&Finalized::Noop));
    let mut conflict = observation();
    conflict.usage.as_mut().unwrap().output_tokens += 1;
    assert_eq!(
        ledger
            .finalize_request(spec.id, &conflict)
            .await
            .unwrap_err(),
        LedgerError::Conflict
    );
    let row=sqlx::query("SELECT cost_usd::text AS cost,settled_count,(SELECT count(*) FROM gw_usage_events) AS events FROM gw_usage_totals").fetch_one(db.store.pool()).await.unwrap();
    assert_eq!(row.get::<String, _>("cost"), "0.000140000000");
    assert_eq!(row.get::<i64, _>("settled_count"), 1);
    assert_eq!(row.get::<i64, _>("events"), 1);
    db.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn safety_cost_audit_and_separate_version_are_atomic() {
    let db = postgres::TestDb::new().await;
    let id = finance::human(db.store.pool()).await;
    let current = finance::current(db.store.pool(), id).await;
    let ledger = PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    );
    let mut spec = specification();
    spec.cost_bound = Some("0.000000000001".parse().unwrap());
    ledger.admit_request(&current, &spec).await.unwrap();
    let _p = ledger.authorize_dispatch(spec.id).await.unwrap();
    let prepared = specification();
    ledger.admit_request(&current, &prepared).await.unwrap();
    sqlx::raw_sql("CREATE FUNCTION gw_test_audit_failure() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic audit failure'; END $$; CREATE TRIGGER gw_test_audit_failure BEFORE INSERT ON gw_audit_events FOR EACH ROW EXECUTE FUNCTION gw_test_audit_failure();").execute(db.store.pool()).await.unwrap();
    assert_eq!(
        ledger
            .finalize_request(spec.id, &observation())
            .await
            .unwrap_err(),
        LedgerError::DatabaseUnavailable
    );
    let row=sqlx::query("SELECT phase,(SELECT count(*) FROM gw_usage_events) AS events,(SELECT inference_safety_version FROM gw_principals WHERE id=$2) AS safety FROM gw_requests WHERE id=$1").bind(spec.id).bind(id).fetch_one(db.store.pool()).await.unwrap();
    assert_eq!(row.get::<String, _>("phase"), "dispatching");
    assert_eq!(row.get::<i64, _>("events"), 0);
    assert_eq!(row.get::<i64, _>("safety"), 1);
    sqlx::raw_sql("DROP TRIGGER gw_test_audit_failure ON gw_audit_events; DROP FUNCTION gw_test_audit_failure();").execute(db.store.pool()).await.unwrap();
    assert_eq!(
        ledger
            .finalize_request(spec.id, &observation())
            .await
            .unwrap(),
        Finalized::Settled
    );
    assert_eq!(
        ledger
            .finalize_request(spec.id, &observation())
            .await
            .unwrap(),
        Finalized::Noop
    );
    let row=sqlx::query("SELECT version,inference_safety_version,inference_pause_reason,disabled_at IS NULL AS active,(SELECT count(*) FROM gw_audit_events WHERE actor_kind='system' AND actor_principal_id IS NULL) AS audits FROM gw_principals WHERE id=$1").bind(id).fetch_one(db.store.pool()).await.unwrap();
    assert_eq!(row.get::<i64, _>("version"), 1);
    assert_eq!(row.get::<i64, _>("inference_safety_version"), 2);
    assert_eq!(
        row.get::<String, _>("inference_pause_reason"),
        "cost_bound_violation"
    );
    assert!(row.get::<bool, _>("active"));
    assert_eq!(row.get::<i64, _>("audits"), 1);
    assert!(matches!(
        ledger.authorize_dispatch(prepared.id).await,
        Err(LedgerError::PolicyChanged)
    ));
    // S5의 관리자 release API가 아닌, 별도 safety version 비교를 위한 SQL fixture다.
    sqlx::query("UPDATE gw_principals SET inference_paused_at=NULL,inference_pause_reason=NULL,inference_pause_request_id=NULL,inference_safety_version=inference_safety_version+1 WHERE id=$1").bind(id).execute(db.store.pool()).await.unwrap();
    assert!(matches!(
        ledger.authorize_dispatch(prepared.id).await,
        Err(LedgerError::PolicyChanged)
    ));
    db.close().await;
}

fn system_block_configuration() -> config::ValidatedConfig {
    configuration_from_yaml(
        &finance_yaml()
            .replace("fixture_version: text-v1", "fixture_version: text-v1\n        features: [client_tools, inline_png, cache]")
            .replace("fixture_version: free-count-v1", "fixture_version: free-count-v1\n        features: [client_tools, inline_png, cache]")
            .replace("output: '2.000000000000'", "output: '2.000000000000'\n          cache_read: '0.100000000000'\n          cache_write: '2.500000000000'")
            .replace("input: '0.000000000000'", "input: '0.000000000000'\n          cache_read: '0.000000000000'\n          cache_write: '0.000000000000'"),
    )
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn system_blocks_are_text_only_on_messages_count_and_aliases() {
    let h = Harness::with_config(system_block_configuration()).await;
    let blocks = [
        serde_json::json!({"type":"tool_use","id":"tool_1","name":"synthetic_tool","input":{}}),
        serde_json::json!({"type":"tool_result","tool_use_id":"tool_1","content":"synthetic result"}),
        serde_json::json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScLbtAAAAABJRU5ErkJggg=="}}),
    ];
    let mut violations = Vec::new();
    for path in [
        "/anthropic/v1/messages",
        "/v1/messages",
        "/anthropic/v1/messages/count_tokens",
        "/v1/messages/count_tokens",
    ] {
        for block in &blocks {
            let mut body = serde_json::json!({"model":"synthetic-text","system":[block],"messages":[{"role":"user","content":"synthetic text"}]});
            if !path.ends_with("count_tokens") {
                body["max_tokens"] = 100.into();
            }
            let response = h
                .request(path, serde_json::to_vec(&body).unwrap())
                .send()
                .await
                .unwrap();
            let rejected = response.status() == StatusCode::BAD_REQUEST
                && response
                    .headers()
                    .get("x-gateway-dispatch-state")
                    .is_some_and(|v| v == "not_dispatched");
            let status = response.status();
            let error: serde_json::Value =
                serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
            if !rejected
                || error["type"] != "error"
                || error["error"]["type"] != "invalid_request_error"
            {
                violations.push(format!("{path}: {} accepted ({status})", block["type"]));
            }
        }
    }
    let requests: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_requests")
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    let calls = h.provider.calls.lock().await.len();
    h.close().await;
    assert!(
        violations.is_empty() && requests == 0 && calls == 0,
        "illegal system blocks: {violations:?}; requests={requests}, provider_calls={calls}"
    );
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn tool_result_content_rejects_nested_tool_blocks_on_all_native_routes() {
    let h = Harness::with_config(system_block_configuration()).await;
    let mut violations = Vec::new();
    for path in [
        "/anthropic/v1/messages",
        "/v1/messages",
        "/anthropic/v1/messages/count_tokens",
        "/v1/messages/count_tokens",
    ] {
        for block in [
            serde_json::json!({"type":"tool_use","id":"tool_2","name":"synthetic_tool","input":{}}),
            serde_json::json!({"type":"tool_result","tool_use_id":"tool_2","content":"synthetic result"}),
        ] {
            let mut request = serde_json::json!({"model":"synthetic-text","messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool_1","content":[block]}]}]});
            if !path.ends_with("count_tokens") {
                request["max_tokens"] = 100.into();
            }
            let response = h
                .request(path, serde_json::to_vec(&request).unwrap())
                .send()
                .await
                .unwrap();
            let rejected = response.status() == StatusCode::BAD_REQUEST
                && response
                    .headers()
                    .get("x-gateway-dispatch-state")
                    .is_some_and(|v| v == "not_dispatched");
            let error: serde_json::Value =
                serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
            if !rejected || error["type"] != "error" {
                violations.push(format!("{path}: {}", block["type"]));
            }
        }
    }
    let requests: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_requests")
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    let calls = h.provider.calls.lock().await.len();
    h.close().await;
    assert!(
        violations.is_empty() && requests == 0 && calls == 0,
        "nested tool blocks: {violations:?}; requests={requests}, provider_calls={calls}"
    );
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn system_text_and_message_tool_data_preserve_original_bytes_and_attribution() {
    let h = Harness::with_config(system_block_configuration()).await;
    let mut sent = Vec::new();
    for path in [
        "/anthropic/v1/messages",
        "/v1/messages",
        "/anthropic/v1/messages/count_tokens",
        "/v1/messages/count_tokens",
    ] {
        let count = path.ends_with("count_tokens");
        for system in [
            serde_json::json!("synthetic instruction"),
            serde_json::json!([{"type":"text","text":"synthetic instruction"}]),
            serde_json::json!([{"cache_control":{"ttl":"5m","type":"ephemeral"},"text":"synthetic instruction","type":"text"}]),
        ] {
            let mut request = serde_json::json!({"model":"synthetic-text","system":system,"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"tool_1","name":"synthetic_tool","input":{"system":[{"type":"image","source":"opaque data"}],"nested":{"type":"tool_result","input":[1,null,true]}}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool_1","content":[{"type":"text","text":"synthetic result"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScLbtAAAAABJRU5ErkJggg=="}}]}]}],"tools":[{"name":"synthetic_tool","input_schema":{"type":"object","properties":{"system":{"type":"array","items":{"type":"object"}}}}}]});
            if !count {
                request["max_tokens"] = 100.into();
            }
            let bytes = serde_json::to_vec_pretty(&request).unwrap();
            if count {
                h.provider
                    .set(
                        StatusCode::OK,
                        HeaderMap::from_iter([(
                            "content-type".parse().unwrap(),
                            "application/json".parse().unwrap(),
                        )]),
                        Bytes::from_static(br#"{"input_tokens":12}"#),
                    )
                    .await;
            } else {
                h.provider.set(StatusCode::OK, HeaderMap::from_iter([("content-type".parse().unwrap(), "application/json".parse().unwrap())]), Bytes::from_static(br#"{"type":"message","role":"assistant","model":"synthetic-text","content":[],"stop_reason":"end_turn","usage":{"input_tokens":100,"output_tokens":20}}"#)).await;
            }
            let response = h.request(path, bytes.clone()).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let id: uuid::Uuid = response.headers()["x-gateway-request-id"]
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            response.bytes().await.unwrap();
            let row = sqlx::query("SELECT r.cost_usd::text AS cost,r.accounting_state,e.kind,e.normalized_usage,b.applied_input_tokens,b.applied_output_tokens FROM gw_requests r JOIN gw_usage_events e ON e.request_id=r.id JOIN gw_request_budgets b ON b.request_id=r.id WHERE r.id=$1").bind(id).fetch_one(h.db.store.pool()).await.unwrap();
            assert_eq!(row.get::<String, _>("accounting_state"), "settled");
            assert_eq!(row.get::<String, _>("kind"), "final");
            assert_eq!(
                row.get::<String, _>("cost"),
                if count {
                    "0.000000000000"
                } else {
                    "0.000140000000"
                }
            );
            let usage: serde_json::Value = row.get("normalized_usage");
            assert_eq!(
                usage["source"],
                if count {
                    "profile_no_charge"
                } else {
                    "provider"
                }
            );
            assert_eq!(
                row.get::<i64, _>("applied_input_tokens"),
                if count { 0 } else { 100 }
            );
            assert_eq!(
                row.get::<i64, _>("applied_output_tokens"),
                if count { 0 } else { 20 }
            );
            if count {
                assert_eq!(usage["counted_input_tokens"], "12");
            }
            sent.push((
                if count {
                    "/v1/messages/count_tokens"
                } else {
                    "/v1/messages"
                },
                bytes,
            ));
        }
    }
    let calls = h.provider.calls.lock().await;
    assert_eq!(calls.len(), sent.len());
    for (call, (path, bytes)) in calls.iter().zip(sent) {
        assert_eq!(call.path, path);
        assert!(
            call.body.as_ref() == bytes,
            "system/tool bytes were rewritten"
        );
    }
    drop(calls);
    let totals = sqlx::query(
        "SELECT cost_usd::text AS cost,settled_count,unknown_count FROM gw_usage_totals",
    )
    .fetch_one(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(totals.get::<String, _>("cost"), "0.000840000000");
    assert_eq!(totals.get::<i64, _>("settled_count"), 12);
    assert_eq!(totals.get::<i64, _>("unknown_count"), 0);
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn approved_native_features_preserve_text_image_tools_thinking_cache_and_schema() {
    let mut env = support::environment();
    env.insert("ALLOWED_EMAILS".into(), "alice@example.test".into());
    let yaml=support::YAML.replace("fixture_version: text-v1","fixture_version: native-v1\n        features: [client_tools, thinking, cache, json_schema, inline_png]\n        approved_beta: [synthetic-native-2026-01-01]").replace("max_output_tokens: 1024","max_output_tokens: 4096").replace("output: '2.000000000000'","output: '2.000000000000'\n          cache_read: '0.100000000000'\n          cache_write: '2.500000000000'");
    let cfg = config::load(ConfigInputs {
        yaml: &yaml,
        file_environment: None,
        environment: &env,
    })
    .unwrap();
    let h = Harness::with_config(cfg).await;
    let request = serde_json::json!({"model":"synthetic-text","max_tokens":2048,"system":[{"type":"text","text":"synthetic instruction","cache_control":{"type":"ephemeral","ttl":"5m"}}],"messages":[{"role":"user","content":[{"type":"text","text":"synthetic text"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScLbtAAAAABJRU5ErkJggg=="}}]},{"role":"assistant","content":[{"type":"tool_use","id":"tool_1","name":"synthetic_tool","input":{"schema":"not a schema position","nested":{"value":1}}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool_1","content":"synthetic result","is_error":false}]}],"tools":[{"name":"synthetic_tool","description":"synthetic client tool","input_schema":{"type":"object","properties":{"value":{"type":"number"}}}}],"tool_choice":{"type":"auto","disable_parallel_tool_use":true},"thinking":{"type":"enabled","budget_tokens":1024},"output_config":{"format":{"type":"json_schema","schema":{"type":"object","properties":{"answer":{"type":"string"}}}}}});
    let bytes = serde_json::to_vec_pretty(&request).unwrap();
    let response=br#"{"type":"message","role":"assistant","model":"synthetic-text","content":[{"type":"thinking","thinking":"synthetic thinking"},{"type":"text","text":"synthetic result"}],"stop_reason":"end_turn","usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":4,"cache_creation_input_tokens":8,"cache_creation":{"ephemeral_5m_input_tokens":8,"ephemeral_1h_input_tokens":0}}}"#;
    h.provider
        .set(
            StatusCode::OK,
            HeaderMap::from_iter([(
                "content-type".parse().unwrap(),
                "application/json".parse().unwrap(),
            )]),
            Bytes::from_static(response),
        )
        .await;
    let r = h
        .request("/anthropic/v1/messages", bytes.clone())
        .header("anthropic-beta", "synthetic-native-2026-01-01")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.bytes().await.unwrap().as_ref(), response);
    let calls = h.provider.calls.lock().await;
    assert_eq!(calls.len(), 1);
    assert!(calls[0].body.as_ref() == bytes);
    assert_eq!(
        calls[0].headers["anthropic-beta"],
        "synthetic-native-2026-01-01"
    );
    drop(calls);
    let cost: String =
        sqlx::query_scalar("SELECT cost_usd::text FROM gw_requests WHERE principal_id=$1")
            .bind(h.principal)
            .fetch_one(h.db.store.pool())
            .await
            .unwrap();
    assert_eq!(cost, "0.000160400000");
    h.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn large_nonstream_json_is_scanned_without_a_second_content_tree() {
    let h = Harness::new().await;
    let mut response=br#"{"type":"message","role":"assistant","model":"synthetic-text","content":[{"type":"text","text":""#.to_vec();
    response.extend(std::iter::repeat_n(b'a', 2 * 1024 * 1024));
    response.extend_from_slice(
        br#""}],"stop_reason":"max_tokens","usage":{"input_tokens":100,"output_tokens":20}}"#,
    );
    h.provider
        .set(
            StatusCode::OK,
            HeaderMap::from_iter([(
                "content-type".parse().unwrap(),
                "application/json".parse().unwrap(),
            )]),
            Bytes::from(response.clone()),
        )
        .await;
    let r = h.request("/v1/messages", BODY).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert!(r.bytes().await.unwrap().as_ref() == response);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_usage_events WHERE kind='final'")
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
    h.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn model_price_overflow_and_totals_overflow_never_become_zero() {
    for variant in [
        "cache_rate",
        "alias",
        "price_overflow",
        "totals_overflow",
        "token_overflow",
    ] {
        let db = postgres::TestDb::new().await;
        let id = finance::human(db.store.pool()).await;
        let current = finance::current(db.store.pool(), id).await;
        let ledger = PgLedger::new(
            db.store.clone(),
            AccessSettings::from_config(&configuration()),
            [1; 32],
        );
        let mut spec = specification();
        if variant == "price_overflow" {
            for rate in &mut spec.pricing.rates {
                rate.usd_per_million_tokens = "9999999999999999.999999999999".into();
            }
        }
        if variant == "token_overflow" {
            for rate in &mut spec.pricing.rates {
                rate.usd_per_million_tokens = "0.000000000000".into();
            }
        }
        ledger.admit_request(&current, &spec).await.unwrap();
        let _p = ledger.authorize_dispatch(spec.id).await.unwrap();
        if variant == "totals_overflow" {
            sqlx::query("UPDATE gw_usage_totals SET cost_usd=gw_amount('9999999999999999.999999999999'),carried_cost_usd=gw_amount('9999999999999999.999999999999')").execute(db.store.pool()).await.unwrap();
        }
        let mut obs = observation();
        if variant == "cache_rate" {
            obs.usage.as_mut().unwrap().cache_write_tokens = 2;
        }
        if variant == "alias" {
            obs.usage.as_mut().unwrap().actual_model = "unapproved-actual-model".into();
        }
        if variant == "price_overflow" {
            obs.usage.as_mut().unwrap().input_tokens = i64::MAX as u64;
        }
        if variant == "token_overflow" {
            obs.usage.as_mut().unwrap().input_tokens = i64::MAX as u64;
            obs.usage.as_mut().unwrap().output_tokens = 1;
        }

        assert_eq!(
            ledger.finalize_request(spec.id, &obs).await.unwrap(),
            Finalized::Unknown
        );
        let row=sqlx::query("SELECT r.cost_usd IS NULL AS unresolved,r.accounting_state,p.inference_pause_reason,p.inference_safety_version FROM gw_requests r JOIN gw_principals p ON p.id=r.principal_id WHERE r.id=$1").bind(spec.id).fetch_one(db.store.pool()).await.unwrap();
        assert!(row.get::<bool, _>("unresolved"));
        assert_eq!(row.get::<String, _>("accounting_state"), "unknown");
        if ["cache_rate", "alias"].contains(&variant) {
            assert!(
                row.get::<Option<String>, _>("inference_pause_reason")
                    .is_none()
            );
        } else {
            assert_eq!(
                row.get::<Option<String>, _>("inference_pause_reason")
                    .as_deref(),
                Some("amount_overflow")
            );
            assert_eq!(row.get::<i64, _>("inference_safety_version"), 2);
        }
        assert_eq!(
            ledger.finalize_request(spec.id, &obs).await.unwrap(),
            Finalized::Noop
        );
        let audits: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM gw_audit_events WHERE actor_kind='system' AND request_id=$1",
        )
        .bind(spec.id)
        .fetch_one(db.store.pool())
        .await
        .unwrap();
        assert_eq!(
            audits,
            if ["cache_rate", "alias"].contains(&variant) {
                0
            } else {
                1
            }
        );
        let event = sqlx::query("SELECT r.pricing_snapshot,r.reason_code,e.provider_usage,e.normalized_usage,e.pricing_resolution FROM gw_requests r JOIN gw_usage_events e ON e.request_id=r.id WHERE r.id=$1").bind(spec.id).fetch_one(db.store.pool()).await.unwrap();
        assert_eq!(
            event.get::<serde_json::Value, _>("pricing_snapshot"),
            serde_json::to_value(&spec.pricing).unwrap()
        );
        assert_eq!(
            event.get::<serde_json::Value, _>("provider_usage")["output_tokens"],
            obs.usage.as_ref().unwrap().output_tokens
        );
        assert_eq!(
            event.get::<serde_json::Value, _>("normalized_usage")["cache_write_tokens"],
            obs.usage.as_ref().unwrap().cache_write_tokens.to_string()
        );
        let resolution: serde_json::Value = event.get("pricing_resolution");
        assert!(resolution["cost_usd"].is_null());
        if variant == "cache_rate" {
            assert_eq!(event.get::<String, _>("reason_code"), "price_unresolved");
            assert_eq!(resolution["unresolved_reason"], "price_unresolved");
        }
        assert_first_usage_event_kind(db.store.pool(), spec.id, "final").await;
        db.close().await;
    }
}

#[path = "support/pg_proxy.rs"]
mod pg_proxy;
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn real_commit_ack_loss_never_reconstructs_a_send_permit() {
    use m_gated::{
        config::ApiKind,
        providers::anthropic::{AnthropicClient, SendOptions},
    };
    use std::sync::atomic::Ordering;
    for free in [false, true] {
        let db = postgres::TestDb::new().await;
        let principal = finance::human(db.store.pool()).await;
        let current = finance::current(db.store.pool(), principal).await;
        let proxy = pg_proxy::CommitProxy::new().await;
        let proxy_store = db.store_via_proxy(proxy.port).await;
        let ledger = PgLedger::new(
            proxy_store.clone(),
            AccessSettings::from_config(&configuration()),
            [1; 32],
        );
        let cfg = configuration();
        let profiles = &cfg.providers()[0].profiles;
        let spec = RequestSpec::from_profile(
            ProviderId::Anthropic,
            &profiles[usize::from(free)],
            if free { None } else { Some(100) },
        );
        ledger.admit_request(&current, &spec).await.unwrap();
        proxy.armed.store(true, Ordering::SeqCst);
        let provider = provider::Provider::new();
        let server = provider.serve().await;
        let client =
            AnthropicClient::loopback_for_test(server.base.clone(), "synthetic-company-key".into())
                .unwrap();
        let runtime = m_gated::runtime::Runtime::new(config::Limits::default()).unwrap();
        let headers = HeaderMap::from_iter([(
            "anthropic-version".parse().unwrap(),
            "2023-06-01".parse().unwrap(),
        )]);
        match ledger.authorize_dispatch(spec.id).await {
            Err(e) => assert_eq!(e, LedgerError::DispatchUncertain),
            Ok(permit) => {
                let _ = client
                    .send(
                        permit,
                        Bytes::from_static(BODY.as_bytes()),
                        SendOptions {
                            api: if free {
                                ApiKind::CountTokens
                            } else {
                                ApiKind::Messages
                            },
                            headers: &headers,
                            profile: &profiles[usize::from(free)],
                            deadline: tokio::time::Instant::now()
                                + std::time::Duration::from_secs(1),
                            runtime: &runtime,
                        },
                    )
                    .await;
                panic!("commit ACK loss issued a send permit");
            }
        }
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            proxy.committed.notified(),
        )
        .await
        .unwrap();
        let state: String = sqlx::query_scalar("SELECT phase FROM gw_requests WHERE id=$1")
            .bind(spec.id)
            .fetch_one(db.store.pool())
            .await
            .unwrap();
        assert_eq!(state, "dispatching");
        let second = PgLedger::new(
            db.store.clone(),
            AccessSettings::from_config(&configuration()),
            [1; 32],
        );
        assert!(matches!(
            second.authorize_dispatch(spec.id).await,
            Err(LedgerError::Conflict)
        ));
        assert!(provider.calls.lock().await.is_empty());
        let dispatches: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM gw_requests WHERE id=$1 AND dispatch_authorized_at IS NOT NULL",
        )
        .bind(spec.id)
        .fetch_one(db.store.pool())
        .await
        .unwrap();
        assert_eq!(dispatches, 1);
        proxy_store.pool().close().await;
        drop(proxy);
        db.close().await;
    }
}

async fn sdk(h: &Harness, prefix: &str, kind: &str, status: u16, token: &str) {
    sdk_with_error_length(h, prefix, kind, status, token, None).await;
}
async fn sdk_with_error_length(
    h: &Harness,
    prefix: &str,
    kind: &str,
    status: u16,
    token: &str,
    error_length: Option<usize>,
) {
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
    let home = std::path::PathBuf::from(format!("/tmp/m-gated-s2-sdk-{}", uuid::Uuid::new_v4()));
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
        .env("MGATED_GATEWAY_BASE", format!("{}{prefix}", h.base))
        .env("MGATED_GATEWAY_TOKEN", token)
        .env("MGATED_SDK_KIND", kind)
        .env("MGATED_EXPECTED_STATUS", status.to_string())
        .env(
            "MGATED_EXPECTED_NATIVE_ERROR_BYTES",
            error_length.map(|n| n.to_string()).unwrap_or_default(),
        )
        .args([
            "exec",
            "--using",
            "22.22.2",
            "npm",
            "--prefix",
            "tests/compat",
            "test",
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
                "SDK_CASE_OK kind={kind} status={status} attempts=1 maxRetries=0"
            ))
            && stderr.is_empty(),
        "sanitized SDK case failed; no token/body output retained; {}",
        stderr
            .lines()
            .filter(|line| line.starts_with("SDK_DIAGNOSTIC ")
                && line.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_uppercase()
                    || b.is_ascii_digit()
                    || b" _=-".contains(&b)))
            .collect::<Vec<_>>()
            .join(" ")
    );
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL and TEST_FNM_DIR, installed compat SDK"]
async fn fixed_anthropic_sdk_nonstream_alias_count_and_errors_have_one_attempt() {
    let h = Harness::new().await;
    sdk(&h, "/anthropic", "messages", 200, &h.token).await;
    sdk(&h, "", "messages", 200, &h.token).await;
    h.provider
        .set(
            StatusCode::OK,
            HeaderMap::from_iter([(
                "content-type".parse().unwrap(),
                "application/json".parse().unwrap(),
            )]),
            Bytes::from_static(br#"{"input_tokens":12}"#),
        )
        .await;
    sdk(&h, "/anthropic", "count_tokens", 200, &h.token).await;
    sdk(&h, "", "count_tokens", 200, &h.token).await;
    assert_eq!(h.provider.calls.lock().await.len(), 4);
    let row = sqlx::query(
        "SELECT cost_usd::text AS cost,settled_count,unknown_count FROM gw_usage_totals",
    )
    .fetch_one(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("cost"), "0.000280000000");
    assert_eq!(row.get::<i64, _>("settled_count"), 4);
    assert_eq!(row.get::<i64, _>("unknown_count"), 0);
    h.close().await;
    for status in [401, 403, 429, 502, 503] {
        let h = Harness::new().await;
        h.provider.set(StatusCode::from_u16(status).unwrap(),HeaderMap::from_iter([("content-type".parse().unwrap(),"application/json".parse().unwrap())]),Bytes::from_static(br#"{"type":"error","error":{"type":"api_error","message":"synthetic upstream failure"}}"#)).await;
        sdk(
            &h,
            "/anthropic",
            "messages",
            if [401, 403].contains(&status) {
                502
            } else {
                status
            },
            &h.token,
        )
        .await;
        assert_eq!(h.provider.calls.lock().await.len(), 1);
        let unknown: i64 = sqlx::query_scalar("SELECT unknown_count FROM gw_usage_totals")
            .fetch_one(h.db.store.pool())
            .await
            .unwrap();
        assert_eq!(unknown, 1);
        h.close().await;
    }
    let h = Harness::new().await;
    sdk(&h, "/anthropic", "messages", 401, "invalid.synthetic.jwt").await;
    assert!(h.provider.calls.lock().await.is_empty());
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_requests")
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    assert_eq!(n, 0);
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn provider_environment_proxy_is_disabled_in_an_env_clear_child() {
    use std::os::unix::fs::DirBuilderExt;
    if std::env::var_os("MGATED_PROXY_CHILD").is_some() {
        let h = Harness::new().await;
        let r = h.request("/v1/messages", BODY).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(h.provider.calls.lock().await.len(), 1);
        h.close().await;
        println!("ENV_PROXY_CHILD_OK");
        return;
    }
    let raw = std::env::var("TEST_DATABASE_URL").expect("explicit TEST_DATABASE_URL required");
    postgres::test_options(&raw).unwrap();
    let proxy = provider::Provider::new();
    let server = proxy.serve().await;
    let home = std::path::PathBuf::from(format!("/tmp/m-gated-s2-proxy-{}", uuid::Uuid::new_v4()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&home)
        .unwrap();
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env("TEST_DATABASE_URL", raw)
        .env("MGATED_PROXY_CHILD", "1")
        .env("NO_PROXY", "")
        .env("no_proxy", "");
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env(name, server.base.as_str());
    }
    command
        .args([
            "--ignored",
            "--exact",
            "provider_environment_proxy_is_disabled_in_an_env_clear_child",
            "--nocapture",
        ])
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(15), command.output())
        .await
        .unwrap()
        .unwrap();
    std::fs::remove_dir_all(home).unwrap();
    assert!(
        output.status.success()
            && String::from_utf8(output.stdout)
                .unwrap()
                .contains("ENV_PROXY_CHILD_OK")
            && output.stderr.is_empty(),
        "isolated proxy child contract failed without retaining raw output"
    );
    assert!(
        proxy.calls.lock().await.is_empty(),
        "environment proxy received a gateway request"
    );
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn expired_chargeable_and_free_dispatch_have_different_facts_and_one_effect() {
    use m_gated::ledger::finance::FinalOutcome;
    for free in [false, true] {
        let db = postgres::TestDb::new().await;
        let principal = finance::human(db.store.pool()).await;
        let current = finance::current(db.store.pool(), principal).await;
        let ledger = Arc::new(PgLedger::new(
            db.store.clone(),
            AccessSettings::from_config(&configuration()),
            [1; 32],
        ));
        let cfg = configuration();
        let spec = RequestSpec::from_profile(
            ProviderId::Anthropic,
            &cfg.providers()[0].profiles[usize::from(free)],
            if free { None } else { Some(100) },
        );
        ledger.admit_request(&current, &spec).await.unwrap();
        let expired = finance::expired_copy(db.store.pool(), spec.id, "dispatching").await;
        let obs = FinalObservation {
            outcome: FinalOutcome::Failed,
            usage: None,
            counted_input_tokens: None,
            reason: "upstream_read_timeout",
            http_status: None,
            provider_request_id: None,
        };
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let mut jobs = Vec::new();
        for _ in 0..2 {
            let l = ledger.clone();
            let b = barrier.clone();
            let obs = obs.clone();
            jobs.push(tokio::spawn(async move {
                b.wait().await;
                l.finalize_request(expired, &obs).await.unwrap()
            }));
        }
        barrier.wait().await;
        let mut outcomes = Vec::new();
        for job in jobs {
            outcomes.push(job.await.unwrap());
        }
        assert!(outcomes.contains(&Finalized::Noop));
        assert!(outcomes.contains(&if free {
            Finalized::Settled
        } else {
            Finalized::Unknown
        }));
        let row=sqlx::query("SELECT r.accounting_state,r.cost_usd::text AS cost,r.execution_stopped_at IS NULL AS no_stop,e.normalized_usage,t.cost_usd::text AS total,t.settled_count,t.unknown_count,b.state FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id JOIN gw_usage_totals t ON t.scope_id=b.scope_id AND t.period_start=b.period_start JOIN gw_usage_events e ON e.request_id=r.id WHERE r.id=$1").bind(expired).fetch_one(db.store.pool()).await.unwrap();
        assert!(row.get::<bool, _>("no_stop"));
        let usage: serde_json::Value = row.get("normalized_usage");
        if free {
            assert_eq!(row.get::<String, _>("accounting_state"), "settled");
            assert_eq!(
                row.get::<Option<String>, _>("cost").as_deref(),
                Some("0.000000000000")
            );
            assert_eq!(usage["source"], "profile_no_charge");
            assert!(usage["counted_input_tokens"].is_null());
            assert_eq!(usage["input_tokens"], "0");
            assert_eq!(row.get::<i64, _>("unknown_count"), 0);
            assert_eq!(row.get::<i64, _>("settled_count"), 1);
        } else {
            assert_eq!(row.get::<String, _>("accounting_state"), "unknown");
            assert!(row.get::<Option<String>, _>("cost").is_none());
            assert_eq!(row.get::<i64, _>("unknown_count"), 1);
            assert_eq!(row.get::<i64, _>("settled_count"), 0);
            assert_eq!(row.get::<String, _>("state"), "unknown");
        }
        assert_eq!(row.get::<String, _>("total"), "0.000000000000");
        assert_first_usage_event_kind(
            db.store.pool(),
            expired,
            if free { "final" } else { "partial" },
        )
        .await;
        let own_no_send = finance::expired_copy(db.store.pool(), spec.id, "admitted").await;
        assert!(matches!(
            ledger.authorize_dispatch(own_no_send).await,
            Err(LedgerError::Expired)
        ));
        assert_eq!(
            ledger.finalize_request(own_no_send, &obs).await.unwrap(),
            Finalized::Settled
        );
        let source: String = sqlx::query_scalar(
            "SELECT normalized_usage->>'source' FROM gw_usage_events WHERE request_id=$1",
        )
        .bind(own_no_send)
        .fetch_one(db.store.pool())
        .await
        .unwrap();
        assert_eq!(source, "gateway_no_dispatch");
        assert_first_usage_event_kind(db.store.pool(), own_no_send, "final").await;
        if !free {
            assert!(matches!(
                ledger.authorize_dispatch(spec.id).await,
                Err(LedgerError::Unsettled)
            ));
        }
        db.close().await;
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn expired_first_final_is_final_observation_but_unknown_accounting() {
    let db = postgres::TestDb::new().await;
    let principal = finance::human(db.store.pool()).await;
    let current = finance::current(db.store.pool(), principal).await;
    let ledger = PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    );
    let spec = specification();
    ledger.admit_request(&current, &spec).await.unwrap();
    let expired = finance::expired_copy(db.store.pool(), spec.id, "dispatching").await;
    let before = sqlx::query("SELECT pricing_snapshot,execution_snapshot,admitted_at::text AS admitted_at FROM gw_requests WHERE id=$1").bind(expired).fetch_one(db.store.pool()).await.unwrap();
    let obs = observation();
    assert_eq!(
        ledger.finalize_request(expired, &obs).await.unwrap(),
        Finalized::Unknown
    );
    assert_eq!(
        ledger.finalize_request(expired, &obs).await.unwrap(),
        Finalized::Noop
    );
    let mut conflicting = obs.clone();
    conflicting.usage.as_mut().unwrap().output_tokens += 1;
    assert_eq!(
        ledger
            .finalize_request(expired, &conflicting)
            .await
            .unwrap_err(),
        LedgerError::Conflict
    );
    let row = sqlx::query("SELECT r.accounting_state,r.cost_usd::text AS cost,r.reason_code,r.version,r.result_digest,r.execution_stopped_at IS NULL AS no_stop,r.pricing_snapshot,r.execution_snapshot,r.admitted_at::text AS admitted_at,e.actual_model,e.provider_usage,e.normalized_usage,e.pricing_resolution,e.content_digest,b.state,b.remaining_reservation_usd=b.reservation_usd AS held_money,b.remaining_reserved_tokens=b.reserved_tokens AS held_tokens,t.cost_usd::text AS total,t.unknown_count,t.settled_count,w.settled_tokens,p.inference_safety_version FROM gw_requests r JOIN gw_usage_events e ON e.request_id=r.id JOIN gw_request_budgets b ON b.request_id=r.id JOIN gw_usage_totals t ON t.scope_id=b.scope_id AND t.period_start=b.period_start JOIN gw_rate_windows w ON w.scope_id=b.scope_id AND w.lane=b.rate_lane AND w.window_start=b.rate_start JOIN gw_principals p ON p.id=r.principal_id WHERE r.id=$1").bind(expired).fetch_one(db.store.pool()).await.unwrap();
    assert_eq!(row.get::<String, _>("accounting_state"), "unknown");
    assert!(row.get::<Option<String>, _>("cost").is_none());
    assert_eq!(row.get::<String, _>("reason_code"), "lease_expired");
    assert_eq!(row.get::<i64, _>("version"), 2);
    assert_eq!(
        row.get::<Vec<u8>, _>("result_digest"),
        row.get::<Vec<u8>, _>("content_digest")
    );
    assert!(row.get::<bool, _>("no_stop"));
    assert!(row.get::<bool, _>("held_money"));
    assert!(row.get::<bool, _>("held_tokens"));
    for field in ["pricing_snapshot", "execution_snapshot"] {
        assert_eq!(
            row.get::<serde_json::Value, _>(field),
            before.get::<serde_json::Value, _>(field)
        );
    }
    assert_eq!(
        row.get::<String, _>("admitted_at"),
        before.get::<String, _>("admitted_at")
    );
    assert_eq!(row.get::<String, _>("actual_model"), "synthetic-text");
    assert_eq!(
        row.get::<serde_json::Value, _>("provider_usage")["input_tokens"],
        100
    );
    assert_eq!(
        row.get::<serde_json::Value, _>("normalized_usage")["output_tokens"],
        "20"
    );
    let resolution: serde_json::Value = row.get("pricing_resolution");
    assert!(resolution["cost_usd"].is_null());
    assert_eq!(resolution["unresolved_reason"], "lease_expired");
    assert_eq!(row.get::<String, _>("state"), "unknown");
    assert_eq!(row.get::<String, _>("total"), "0.000000000000");
    assert_eq!(row.get::<i64, _>("unknown_count"), 1);
    assert_eq!(row.get::<i64, _>("settled_count"), 0);
    assert_eq!(row.get::<i64, _>("settled_tokens"), 0);
    assert_eq!(row.get::<i64, _>("inference_safety_version"), 1);
    assert!(matches!(
        ledger.authorize_dispatch(spec.id).await,
        Err(LedgerError::Unsettled)
    ));
    assert_first_usage_event_kind(db.store.pool(), expired, "final").await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn previous_month_unknown_and_expired_dispatch_block_other_provider() {
    use m_gated::{config::ApiKind, ledger::finance::FinalOutcome};
    for finished_unknown in [false, true] {
        let db = postgres::TestDb::new().await;
        let principal = finance::human(db.store.pool()).await;
        let current = finance::current(db.store.pool(), principal).await;
        sqlx::query("UPDATE gw_policies SET allow_rules=$2 WHERE id=$1").bind(current.policy_id).bind(serde_json::json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages","count_tokens"]},{"provider":"openai","model":"synthetic-text","capabilities":["responses"]}])).execute(db.store.pool()).await.unwrap();
        let ledger = PgLedger::new(
            db.store.clone(),
            AccessSettings::from_config(&configuration()),
            [1; 32],
        );
        let prior = specification();
        ledger.admit_request(&current, &prior).await.unwrap();
        let expired = finance::expired_copy(db.store.pool(), prior.id, "dispatching").await;
        if finished_unknown {
            let obs = FinalObservation {
                outcome: FinalOutcome::Failed,
                usage: None,
                counted_input_tokens: None,
                reason: "upstream_read_timeout",
                http_status: None,
                provider_request_id: None,
            };
            ledger.finalize_request(expired, &obs).await.unwrap();
        }
        let mut model = configuration().providers()[0].profiles[0].clone();
        model.api = ApiKind::Responses;
        let next = RequestSpec::from_profile(ProviderId::Openai, &model, Some(100));
        ledger.admit_request(&current, &next).await.unwrap();
        assert!(matches!(
            ledger.authorize_dispatch(next.id).await,
            Err(LedgerError::Unsettled)
        ));
        let other = finance::human(db.store.pool()).await;
        let other_current = finance::current(db.store.pool(), other).await;
        let independent = specification();
        ledger
            .admit_request(&other_current, &independent)
            .await
            .unwrap();
        let _p = ledger.authorize_dispatch(independent.id).await.unwrap();
        let row=sqlx::query("SELECT date_trunc('month',admitted_at)<date_trunc('month',clock_timestamp()) AS old FROM gw_requests WHERE id=$1").bind(expired).fetch_one(db.store.pool()).await.unwrap();
        assert!(row.get::<bool, _>("old"));
        db.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn single_send_rechecks_deadline_and_shutdown_without_claiming_free_cost() {
    use m_gated::{
        config::ApiKind,
        ledger::finance::FinalOutcome,
        providers::anthropic::{AnthropicClient, SendOptions, UpstreamError},
        runtime::Runtime,
    };
    let db = postgres::TestDb::new().await;
    let id = finance::human(db.store.pool()).await;
    let current = finance::current(db.store.pool(), id).await;
    let ledger = PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    );
    let first = specification();
    let second = specification();
    ledger.admit_request(&current, &first).await.unwrap();
    ledger.admit_request(&current, &second).await.unwrap();
    let p1 = ledger.authorize_dispatch(first.id).await.unwrap();
    let p2 = ledger.authorize_dispatch(second.id).await.unwrap();
    let provider = provider::Provider::new();
    let server = provider.serve().await;
    let client =
        AnthropicClient::loopback_for_test(server.base.clone(), "synthetic-company-key".into())
            .unwrap();
    let runtime = Runtime::new(config::Limits::default()).unwrap();
    let cfg = configuration();
    let profile = &cfg.providers()[0].profiles[0];
    let headers = HeaderMap::new();
    let result = client
        .send(
            p1,
            Bytes::from_static(BODY.as_bytes()),
            SendOptions {
                api: ApiKind::Messages,
                headers: &headers,
                profile,
                deadline: tokio::time::Instant::now(),
                runtime: &runtime,
            },
        )
        .await;
    assert!(matches!(result, Err(UpstreamError::Timeout)));
    runtime.start_http().unwrap();
    runtime.begin_draining();
    let result = client
        .send(
            p2,
            Bytes::from_static(BODY.as_bytes()),
            SendOptions {
                api: ApiKind::Messages,
                headers: &headers,
                profile,
                deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(1),
                runtime: &runtime,
            },
        )
        .await;
    assert!(matches!(result, Err(UpstreamError::Cancelled)));
    assert!(provider.calls.lock().await.is_empty());
    for id in [first.id, second.id] {
        let obs = FinalObservation {
            outcome: FinalOutcome::Cancelled,
            usage: None,
            counted_input_tokens: None,
            reason: "shutdown_cancelled",
            http_status: None,
            provider_request_id: None,
        };
        assert_eq!(
            ledger.finalize_request(id, &obs).await.unwrap(),
            Finalized::Unknown
        );
        assert!(matches!(
            ledger.authorize_dispatch(id).await,
            Err(LedgerError::Conflict)
        ));
    }
    let n: i64 = sqlx::query_scalar("SELECT unknown_count FROM gw_usage_totals")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    assert_eq!(n, 2);
    db.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn immutable_snapshots_dispatch_facts_and_pause_owner_fk_are_real_sql() {
    let db = postgres::TestDb::new().await;
    let p = finance::human(db.store.pool()).await;
    let current = finance::current(db.store.pool(), p).await;
    let ledger = PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    );
    let spec = specification();
    ledger.admit_request(&current, &spec).await.unwrap();
    let _permit = ledger.authorize_dispatch(spec.id).await.unwrap();
    ledger
        .finalize_request(spec.id, &observation())
        .await
        .unwrap();
    for assignment in [
        "phase=NULL",
        "accounting_state=NULL",
        "pricing_snapshot='{}'",
        "execution_snapshot='{}'",
        "principal_id=gen_random_uuid()",
        "key_id=gen_random_uuid()",
        "admitted_at=admitted_at-interval '1 day'",
        "dispatch_authorized_at=NULL,lease_expires_at=NULL,cost_usd=0,outcome='failed'",
        "safety_violation_recorded=NULL",
    ] {
        assert!(
            sqlx::query(&format!("UPDATE gw_requests SET {assignment} WHERE id=$1"))
                .bind(spec.id)
                .execute(db.store.pool())
                .await
                .is_err(),
            "request immutable/NOT NULL fact was changed"
        );
    }
    let other = finance::human(db.store.pool()).await;
    assert!(sqlx::query("UPDATE gw_principals SET inference_paused_at=clock_timestamp(),inference_pause_reason='cost_bound_violation',inference_pause_request_id=$2 WHERE id=$1").bind(other).bind(spec.id).execute(db.store.pool()).await.is_err());
    let row =
        sqlx::query("SELECT execution_snapshot,pricing_snapshot FROM gw_requests WHERE id=$1")
            .bind(spec.id)
            .fetch_one(db.store.pool())
            .await
            .unwrap();
    let exec: serde_json::Value = row.get("execution_snapshot");
    assert_eq!(
        exec["auth_dependency_versions"]["principals"][0]["version"],
        "1"
    );
    assert_eq!(
        exec["auth_dependency_versions"]["target_inference_safety_version"],
        "1"
    );
    assert!(!exec.to_string().contains("synthetic prompt"));
    db.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn actual_budget_and_rate_denials_are_not_dispatch_and_free_count_is_independent() {
    let h = Harness::new().await;
    sqlx::query("UPDATE gw_policies SET budget_limit_usd=gw_amount('0.000000000000'),count_rpm_limit=1 WHERE principal_id=$1").bind(h.principal).execute(h.db.store.pool()).await.unwrap();
    let r = h.request("/v1/messages", BODY).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.headers()["x-gateway-error-code"], "budget_exhausted");
    assert!(!r.headers().contains_key("retry-after"));
    assert_eq!(r.headers()["x-gateway-dispatch-state"], "not_dispatched");
    assert!(h.provider.calls.lock().await.is_empty());
    h.provider
        .set(
            StatusCode::OK,
            HeaderMap::from_iter([(
                "content-type".parse().unwrap(),
                "application/json".parse().unwrap(),
            )]),
            Bytes::from_static(br#"{"input_tokens":12}"#),
        )
        .await;
    let body =
        r#"{"model":"synthetic-text","messages":[{"role":"user","content":"synthetic count"}]}"#;
    assert_eq!(
        h.request("/v1/messages/count_tokens", body)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let r = h
        .request("/v1/messages/count_tokens", body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.headers()["x-gateway-error-code"], "rate_limited");
    assert_eq!(r.headers()["x-gateway-dispatch-state"], "not_dispatched");
    assert_eq!(h.provider.calls.lock().await.len(), 1);
    h.close().await;
}

#[tokio::test]
async fn explicit_pg_suite_has_no_product_database_fallback() {
    let trap = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    trap.set_nonblocking(true).unwrap();
    let port = trap.local_addr().unwrap().port();
    for test_url in [
        None,
        Some(format!("postgres://m_gated_test@127.0.0.1:{port}/company")),
    ] {
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap());
        child
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env(
                "DATABASE_URL",
                format!("postgres://synthetic@127.0.0.1:{port}/company"),
            )
            .env("DB_HOST", "127.0.0.1")
            .env("DB_PORT", port.to_string())
            .env("DB_NAME", "company")
            .env("DB_USER", "synthetic")
            .env("DB_PASSWORD", "never-read-synthetic-secret")
            .args([
                "--ignored",
                "--exact",
                "finance_schema_and_amount_boundary_are_real_sql",
            ])
            .kill_on_drop(true);
        if let Some(url) = test_url {
            child.env("TEST_DATABASE_URL", url);
        }
        let output = tokio::time::timeout(std::time::Duration::from_secs(3), child.output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            !output.status.success(),
            "ignored suite accepted an absent or non-test URL"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("never-read-synthetic-secret"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("never-read-synthetic-secret"));
    }
    assert!(
        matches!(trap.accept(),Err(e)if e.kind()==std::io::ErrorKind::WouldBlock),
        "product DB fallback attempted a TCP connection"
    );
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn unclosed_test_fixture_cleans_only_its_owned_schema() {
    let db = postgres::TestDb::new().await;
    let other = postgres::TestDb::new().await;
    let own = db.schema.clone();
    let pool = db.store.pool().clone();
    drop(db);
    let row=sqlx::query("SELECT to_regnamespace($1) IS NULL AS own_gone,to_regnamespace($2) IS NOT NULL AS other_preserved").bind(&own).bind(&other.schema).fetch_one(&pool).await.unwrap();
    assert!(row.get::<bool, _>("own_gone"));
    assert!(row.get::<bool, _>("other_preserved"));
    assert!(
        !std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-owned-schemas")
            .join(own)
            .exists()
    );
    pool.close().await;
    other.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn unified_migrator_accepts_all_versions_but_rejects_changed_checksum_and_future() {
    for future in [false, true] {
        let db = postgres::TestDb::new().await;
        db.store.migrate().await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_schema_migrations")
            .fetch_one(db.store.pool())
            .await
            .unwrap();
        assert_eq!(n, 6);
        if future {
            sqlx::query("INSERT INTO gw_schema_migrations VALUES(7,decode(repeat('00',32),'hex'))")
                .execute(db.store.pool())
                .await
                .unwrap();
        } else {
            sqlx::query("UPDATE gw_schema_migrations SET checksum=decode(repeat('00',32),'hex') WHERE version=2").execute(db.store.pool()).await.unwrap();
        }
        assert!(matches!(
            db.store.migrate().await,
            Err(m_gated::auth::AuthError::DatabaseUnavailable)
        ));
        db.close().await;
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn internal_count_relationship_and_budget_states_are_constrained() {
    let db = postgres::TestDb::new().await;
    let principal = finance::human(db.store.pool()).await;
    let current = finance::current(db.store.pool(), principal).await;
    let ledger = PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    );
    let parent = specification();
    ledger.admit_request(&current, &parent).await.unwrap();
    let cfg = configuration();
    let count =
        RequestSpec::from_profile(ProviderId::Anthropic, &cfg.providers()[0].profiles[1], None);
    ledger.admit_request(&current, &count).await.unwrap();
    let statement = "INSERT INTO gw_requests(id,principal_id,key_id,parent_request_id,purpose,provider,requested_model,api,admitted_at,admit_expires_at,transport_deadline,execution_deadline,executor_id,phase,accounting_state,pricing_snapshot,execution_snapshot,auth_config_digest) SELECT $1,principal_id,key_id,$3,'internal_count','anthropic',requested_model,'count_tokens',admitted_at,admit_expires_at,transport_deadline,execution_deadline,executor_id,phase,accounting_state,pricing_snapshot,execution_snapshot,auth_config_digest FROM gw_requests WHERE id=$2";
    let child = uuid::Uuid::new_v4();
    sqlx::query(statement)
        .bind(child)
        .bind(count.id)
        .bind(parent.id)
        .execute(db.store.pool())
        .await
        .unwrap();
    assert!(
        sqlx::query(statement)
            .bind(uuid::Uuid::new_v4())
            .bind(count.id)
            .bind(parent.id)
            .execute(db.store.pool())
            .await
            .is_err(),
        "second internal child accepted"
    );
    assert!(
        sqlx::query(statement)
            .bind(uuid::Uuid::new_v4())
            .bind(count.id)
            .bind(child)
            .execute(db.store.pool())
            .await
            .is_err(),
        "recursive child accepted"
    );
    for assignment in [
        "state='settled'",
        "applied_cost_usd=1",
        "remaining_reservation_usd=1",
        "remaining_reserved_tokens=1",
        "reservation_usd='NaN'",
        "applied_input_tokens=-1",
        "state=NULL",
    ] {
        assert!(
            sqlx::query(&format!(
                "UPDATE gw_request_budgets SET {assignment} WHERE request_id=$1"
            ))
            .bind(parent.id)
            .execute(db.store.pool())
            .await
            .is_err(),
            "invalid budget state accepted"
        );
    }
    db.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn free_count_ignores_generation_tpm_reserved_budget_unknown_and_safety_only() {
    let h = Harness::new().await;
    let headers = HeaderMap::from_iter([(
        "content-type".parse().unwrap(),
        "application/json".parse().unwrap(),
    )]);
    h.provider.set(StatusCode::OK,headers.clone(),Bytes::from_static(br#"{"type":"message","role":"assistant","model":"synthetic-text","content":[],"stop_reason":"end_turn"}"#)).await;
    assert_eq!(
        h.request("/v1/messages", BODY)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    sqlx::query("UPDATE gw_policies SET budget_mode='reserved',budget_limit_usd=gw_amount('0.000000000000'),tpm_limit=1,count_rpm_limit=1 WHERE principal_id=$1").bind(h.principal).execute(h.db.store.pool()).await.unwrap();
    sqlx::query("UPDATE gw_principals SET inference_paused_at=clock_timestamp(),inference_pause_reason='amount_overflow',inference_pause_request_id=(SELECT id FROM gw_requests WHERE principal_id=$1 LIMIT 1),inference_safety_version=inference_safety_version+1 WHERE id=$1").bind(h.principal).execute(h.db.store.pool()).await.unwrap();
    h.provider
        .set(
            StatusCode::OK,
            headers,
            Bytes::from_static(br#"{"input_tokens":12}"#),
        )
        .await;
    let body =
        r#"{"model":"synthetic-text","messages":[{"role":"user","content":"synthetic count"}]}"#;
    let r = h
        .request("/v1/messages/count_tokens", body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        StatusCode::OK,
        "guaranteed-free count was blocked by a generation-only monetary/TPM guard"
    );
    let r = h
        .request("/v1/messages/count_tokens", body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.headers()["x-gateway-error-code"], "rate_limited");
    assert_eq!(h.provider.calls.lock().await.len(), 2);
    let b=sqlx::query("SELECT b.budget_mode,b.reservation_usd::text AS reserved,b.policy_snapshot FROM gw_request_budgets b JOIN gw_requests r ON r.id=b.request_id WHERE r.api='count_tokens'").fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(b.get::<String, _>("budget_mode"), "reserved");
    assert_eq!(b.get::<String, _>("reserved"), "0.000000000000");
    let p: serde_json::Value = b.get("policy_snapshot");
    assert_eq!(p["limits"]["budget"]["mode"], "reserved");
    sqlx::query("UPDATE gw_principals SET disabled_at=clock_timestamp(),disabled_reason='synthetic_access_denial' WHERE id=$1").bind(h.principal).execute(h.db.store.pool()).await.unwrap();
    assert_eq!(
        h.request("/v1/messages/count_tokens", body)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(h.provider.calls.lock().await.len(), 2);
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn invalid_price_currency_or_pre_send_overflow_never_admits() {
    let db = postgres::TestDb::new().await;
    let principal = finance::human(db.store.pool()).await;
    let current = finance::current(db.store.pool(), principal).await;
    let ledger = PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    );
    let mut euro = specification();
    euro.pricing.currency = "EUR".into();
    assert_eq!(
        ledger.admit_request(&current, &euro).await.unwrap_err(),
        LedgerError::InvalidSnapshot
    );
    let mut exponent = specification();
    exponent.pricing.rates[0].usd_per_million_tokens = "1e0".into();
    assert_eq!(
        ledger.admit_request(&current, &exponent).await.unwrap_err(),
        LedgerError::InvalidSnapshot
    );
    let mut duplicate = specification();
    duplicate
        .pricing
        .rates
        .push(duplicate.pricing.rates[0].clone());
    assert_eq!(
        ledger
            .admit_request(&current, &duplicate)
            .await
            .unwrap_err(),
        LedgerError::InvalidSnapshot
    );
    let mut forged_free = specification();
    forged_free.billing_class = config::BillingClass::GuaranteedFree;
    assert_eq!(
        ledger
            .admit_request(&current, &forged_free)
            .await
            .unwrap_err(),
        LedgerError::InvalidSnapshot
    );
    let mut profile = configuration().providers()[0].profiles[0].clone();
    profile.max_input_tokens = 1_000_000_000;
    for rate in profile.rates.values_mut() {
        *rate = "9999999999999999.999999999999".into();
    }
    let overflow = RequestSpec::from_profile(ProviderId::Anthropic, &profile, Some(100));
    assert_eq!(
        ledger.admit_request(&current, &overflow).await.unwrap_err(),
        LedgerError::AmountOverflow
    );
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_requests")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    assert_eq!(n, 0);
    db.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn native_actual_model_alias_is_explicit_not_a_provider_route() {
    let yaml = support::YAML.replace(
        "          - synthetic-text",
        "          - synthetic-text\n          - synthetic-versioned-model",
    );
    let mut env = support::environment();
    env.insert("ALLOWED_EMAILS".into(), "alice@example.test".into());
    let cfg = config::load(ConfigInputs {
        yaml: &yaml,
        file_environment: None,
        environment: &env,
    })
    .unwrap();
    let h = Harness::with_config(cfg).await;
    let body=br#"{"type":"message","role":"assistant","model":"synthetic-versioned-model","content":[],"stop_reason":"end_turn","usage":{"input_tokens":100,"output_tokens":20}}"#;
    h.provider
        .set(
            StatusCode::OK,
            HeaderMap::from_iter([(
                "content-type".parse().unwrap(),
                "application/json".parse().unwrap(),
            )]),
            Bytes::from_static(body),
        )
        .await;
    let r = h.request("/v1/messages", BODY).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert!(r.bytes().await.unwrap().as_ref() == body);
    let row=sqlx::query("SELECT r.provider,r.requested_model,r.cost_usd::text AS cost,e.actual_model FROM gw_requests r JOIN gw_usage_events e ON e.request_id=r.id WHERE r.principal_id=$1").bind(h.principal).fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(row.get::<String, _>("provider"), "anthropic");
    assert_eq!(row.get::<String, _>("requested_model"), "synthetic-text");
    assert_eq!(
        row.get::<String, _>("actual_model"),
        "synthetic-versioned-model"
    );
    assert_eq!(row.get::<String, _>("cost"), "0.000140000000");
    assert_eq!(h.provider.calls.lock().await[0].path, "/v1/messages");
    h.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn malformed_nonstream_and_response_limit_do_not_promote_early_usage() {
    for (body,code) in [(Bytes::from_static(br#"{"type":"message","role":"assistant","model":"synthetic-text","content":[],"usage":{"input_tokens":100,"output_tokens":20}"#),"upstream_protocol_error"),(Bytes::from_static(br#"{"usage":{"input_tokens":100,"output_tokens":20},"type":"message","role":"assistant","model":"synthetic-text","content":[],"stop_reason":"end_turn"} null"#),"upstream_protocol_error"),(Bytes::from_static(br#"{"usage":{"input_tokens":100,"output_tokens":20},"type":"message","role":"assistant","model":"synthetic-text","content":[{"type":"text","text":"x","text":"y"}],"stop_reason":"end_turn"}"#),"upstream_protocol_error"),(Bytes::from_static(br#"{"type":"message","role":"assistant","model":"synthetic-text","content":[],"usage":{"input_tokens":100,"output_tokens":20}}"#),"upstream_protocol_error"),(Bytes::from(vec![b'a';16*1024*1024+1]),"upstream_response_too_large")] {
        let h=Harness::new().await;h.provider.set(StatusCode::OK,HeaderMap::from_iter([("content-type".parse().unwrap(),"application/json".parse().unwrap())]),body).await;let r=h.request("/v1/messages",BODY).send().await.unwrap();assert_eq!(r.status(),StatusCode::BAD_GATEWAY);assert_eq!(r.headers()["x-gateway-error-code"],code);assert_eq!(r.headers()["x-should-retry"],"false");let n:i64=sqlx::query_scalar("SELECT unknown_count FROM gw_usage_totals").fetch_one(h.db.store.pool()).await.unwrap();assert_eq!(n,1);assert_eq!(h.provider.calls.lock().await.len(),1);
        let id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM gw_requests WHERE principal_id=$1").bind(h.principal).fetch_one(h.db.store.pool()).await.unwrap();
        assert_first_usage_event_kind(h.db.store.pool(), id, "partial").await;
        h.close().await;
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn body_exact_limit_is_preserved_and_declared_or_chunked_overflow_never_dispatch() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let h = Harness::new().await;
    let mut exact = BODY.as_bytes().to_vec();
    exact.resize(8 * 1024 * 1024, b' ');
    let r = h
        .request("/v1/messages", exact.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert!(h.provider.calls.lock().await[0].body.as_ref() == exact);
    h.provider.calls.lock().await.clear();
    let address = url::Url::parse(&h.base)
        .unwrap()
        .socket_addrs(|| None)
        .unwrap()[0];
    for chunked in [false, true] {
        let socket = tokio::net::TcpStream::connect(address).await.unwrap();
        let (mut reader, mut writer) = socket.into_split();
        let framing = if chunked {
            "Transfer-Encoding: chunked".to_owned()
        } else {
            "Content-Length: 8388609".to_owned()
        };
        let mut wire=format!("POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAnthropic-Version: 2023-06-01\r\n{framing}\r\n\r\n",h.token).into_bytes();
        if chunked {
            wire.extend_from_slice(b"800001\r\n");
            wire.extend_from_slice(&exact);
            wire.extend_from_slice(b" \r\n0\r\n\r\n");
        }
        let read = async {
            let mut response = Vec::new();
            if let Err(error) = reader.read_to_end(&mut response).await {
                assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
            }
            response
        };
        let write = async {
            let _ = writer.write_all(&wire).await;
        };
        let (response, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(read, write)
        })
        .await
        .unwrap();
        assert!(
            response.starts_with(b"HTTP/1.1 413"),
            "body limit did not produce a bounded 413 response"
        );
        let end = response.windows(4).position(|b| b == b"\r\n\r\n").unwrap() + 4;
        let headers = std::str::from_utf8(&response[..end])
            .unwrap()
            .to_ascii_lowercase();
        assert!(headers.contains("x-gateway-error-code: body_too_large"));
        let length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .unwrap()
            .trim()
            .parse::<usize>()
            .unwrap();
        assert_eq!(response.len() - end, length);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&response[end..]).unwrap()["type"],
            "error"
        );
    }
    assert!(h.provider.calls.lock().await.is_empty());
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_requests")
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    assert_eq!(n, 1);
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn provider_connection_reset_after_post_body_never_retries_or_settles_zero() {
    let h = Harness::with_reset(configuration(), true).await;
    let r = h.request("/v1/messages", BODY).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        r.headers()["x-gateway-error-code"],
        "upstream_transport_error"
    );
    assert_eq!(r.headers()["x-should-retry"], "false");
    assert!(!r.headers().contains_key("x-gateway-dispatch-state"));
    assert_eq!(h.provider.calls.lock().await.len(), 1);
    let row=sqlx::query("SELECT accounting_state,cost_usd IS NULL AS unresolved FROM gw_requests WHERE principal_id=$1").bind(h.principal).fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(row.get::<String, _>("accounting_state"), "unknown");
    assert!(row.get::<bool, _>("unresolved"));
    let r = h.request("/v1/messages", BODY).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert_eq!(h.provider.calls.lock().await.len(), 1);
    let id: uuid::Uuid = sqlx::query_scalar(
        "SELECT id FROM gw_requests WHERE principal_id=$1 AND accounting_state='unknown'",
    )
    .bind(h.principal)
    .fetch_one(h.db.store.pool())
    .await
    .unwrap();
    assert_first_usage_event_kind(h.db.store.pool(), id, "partial").await;
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn registered_native_posts_keep_native_envelope_at_http_capacity() {
    let h = Harness::new().await;
    let held = (0..h.runtime.limits().http_requests)
        .map(|_| h.runtime.try_http().unwrap())
        .collect::<Vec<_>>();
    for path in [
        "/anthropic/v1/messages",
        "/anthropic/v1/messages/count_tokens",
        "/v1/messages",
        "/v1/messages/count_tokens",
    ] {
        for method in [
            reqwest::Method::POST,
            reqwest::Method::GET,
            reqwest::Method::PUT,
        ] {
            let r = h.request(path, BODY).build().unwrap();
            let mut r = r;
            *r.method_mut() = method;
            let r = reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .execute(r)
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(r.headers()["cache-control"], "no-store");
            assert_eq!(r.headers()["x-gateway-dispatch-state"], "not_dispatched");
            let id = uuid::Uuid::parse_str(r.headers()["x-gateway-request-id"].to_str().unwrap())
                .unwrap();
            assert_eq!(id.get_version_num(), 4);
            let retry: u32 = r.headers()["retry-after"]
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            assert!((1..=120).contains(&retry));
            let body: serde_json::Value =
                serde_json::from_slice(&r.bytes().await.unwrap()).unwrap();
            assert_eq!(
                body["type"], "error",
                "HTTP saturation bypassed native formatter"
            );
            assert_eq!(body["error"]["type"], "overloaded_error");
        }
    }
    let head = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .head(format!("{}/v1/messages", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(head.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(head.bytes().await.unwrap().is_empty());
    assert!(h.provider.calls.lock().await.is_empty());
    drop(held);
    let r = reqwest::Client::new()
        .get(format!("{}/v1/messages", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(r.headers()["allow"], "POST");
    let r = h.request("/unregistered", BODY).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let body: serde_json::Value = serde_json::from_slice(&r.bytes().await.unwrap()).unwrap();
    assert_eq!(body["error"]["code"], "route_not_found");
    assert!(body.get("type").is_none());
    assert!(h.provider.calls.lock().await.is_empty());
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn count_response_obeys_smaller_profile_and_default_limits_on_both_framings() {
    for profile_limit in [32, 256 * 1024] {
        let cfg = configuration_with_count_response_limit(profile_limit);
        let h = Harness::with_config(cfg).await;
        for chunked in [false, true] {
            for size in [profile_limit, profile_limit + 1] {
                let mut bytes = br#"{"input_tokens":7}"#.to_vec();
                bytes.resize(size, b' ');
                let headers = HeaderMap::from_iter([(
                    "content-type".parse().unwrap(),
                    "application/json".parse().unwrap(),
                )]);
                if chunked {
                    h.provider
                        .set_chunked(StatusCode::OK, headers, Bytes::from(bytes))
                        .await;
                } else {
                    h.provider
                        .set(StatusCode::OK, headers, Bytes::from(bytes))
                        .await;
                }
                let before = h.provider.calls.lock().await.len();
                let r = h.request("/v1/messages/count_tokens", r#"{"model":"synthetic-text","messages":[{"role":"user","content":"count"}]}"#).send().await.unwrap();
                assert_eq!(
                    r.status(),
                    if size == profile_limit {
                        StatusCode::OK
                    } else {
                        StatusCode::BAD_GATEWAY
                    },
                    "profile_limit={profile_limit}, size={size}, chunked={chunked}"
                );
                if size > profile_limit {
                    assert_eq!(
                        r.headers()["x-gateway-error-code"],
                        "upstream_response_too_large"
                    );
                    assert_eq!(r.headers()["x-should-retry"], "false");
                }
                let _ = r.bytes().await.unwrap();
                assert_eq!(h.provider.calls.lock().await.len(), before + 1);
            }
        }
        let facts: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_requests r JOIN gw_usage_events e ON e.request_id=r.id WHERE r.accounting_state='settled' AND r.cost_usd=0 AND e.normalized_usage->>'source'='profile_no_charge' AND e.normalized_usage->>'input_tokens'='0' AND e.normalized_usage->>'output_tokens'='0'").fetch_one(h.db.store.pool()).await.unwrap();
        assert_eq!(facts, 4);
        let unknown: i64 =
            sqlx::query_scalar("SELECT sum(unknown_count)::bigint FROM gw_usage_totals")
                .fetch_one(h.db.store.pool())
                .await
                .unwrap();
        assert_eq!(unknown, 0);
        h.close().await;
    }
}

async fn accumulated_counter_boundary(field: &str, increment: i64) {
    for carried in [false, true] {
        for overflow in [false, true] {
            let db = postgres::TestDb::new().await;
            let principal = finance::human(db.store.pool()).await;
            let current = finance::current(db.store.pool(), principal).await;
            let ledger = PgLedger::new(
                db.store.clone(),
                AccessSettings::from_config(&configuration()),
                [1; 32],
            );
            let spec = specification();
            ledger.admit_request(&current, &spec).await.unwrap();
            let _permit = ledger.authorize_dispatch(spec.id).await.unwrap();
            let initial = i64::MAX - increment + i64::from(overflow);
            let sql = if field == "settled_tokens" {
                "UPDATE gw_rate_windows SET settled_tokens=$1".to_owned()
            } else {
                let carry = if carried {
                    format!(",carried_{field}=$1")
                } else {
                    String::new()
                };
                format!("UPDATE gw_usage_totals SET {field}=$1{carry}")
            };
            sqlx::query(&sql)
                .bind(initial)
                .execute(db.store.pool())
                .await
                .unwrap();
            let obs = observation();
            let result = ledger.finalize_request(spec.id, &obs).await;
            assert_eq!(
                result,
                Ok(if overflow {
                    Finalized::Unknown
                } else {
                    Finalized::Settled
                }),
                "field={field}, carried={carried}, overflow={overflow}"
            );
            assert_eq!(
                ledger.finalize_request(spec.id, &obs).await.unwrap(),
                Finalized::Noop
            );
            let value: i64 = sqlx::query_scalar(&if field == "settled_tokens" {
                "SELECT settled_tokens FROM gw_rate_windows".into()
            } else {
                format!("SELECT {field} FROM gw_usage_totals")
            })
            .fetch_one(db.store.pool())
            .await
            .unwrap();
            assert_eq!(value, if overflow { initial } else { i64::MAX });
            if carried && field != "settled_tokens" {
                let carried_value: i64 =
                    sqlx::query_scalar(&format!("SELECT carried_{field} FROM gw_usage_totals"))
                        .fetch_one(db.store.pool())
                        .await
                        .unwrap();
                assert_eq!(carried_value, initial, "finalizer rewrote carried counter");
            }
            let unknown: i64 = sqlx::query_scalar("SELECT unknown_count FROM gw_usage_totals")
                .fetch_one(db.store.pool())
                .await
                .unwrap();
            assert_eq!(unknown, i64::from(overflow));
            let audit: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_audit_events")
                .fetch_one(db.store.pool())
                .await
                .unwrap();
            assert_eq!(audit, i64::from(overflow));
            let row = sqlx::query("SELECT disabled_at,inference_pause_reason,inference_safety_version,version FROM gw_principals WHERE id=$1").bind(principal).fetch_one(db.store.pool()).await.unwrap();
            assert!(
                row.get::<Option<String>, _>("inference_pause_reason")
                    .is_some()
                    == overflow
            );
            if overflow {
                assert_eq!(
                    row.get::<String, _>("inference_pause_reason"),
                    "amount_overflow"
                );
            }
            assert_eq!(row.get::<i64, _>("version"), 1);
            assert_eq!(
                row.get::<i64, _>("inference_safety_version"),
                if overflow { 2 } else { 1 }
            );
            assert!(row.try_get_raw("disabled_at").unwrap().is_null());
            let event = sqlx::query("SELECT normalized_usage,provider_usage,pricing_resolution FROM gw_usage_events WHERE request_id=$1").bind(spec.id).fetch_one(db.store.pool()).await.unwrap();
            assert_eq!(
                event.get::<serde_json::Value, _>("normalized_usage")["input_tokens"],
                "100"
            );
            assert_eq!(
                event.get::<serde_json::Value, _>("provider_usage")["output_tokens"],
                20
            );
            assert_eq!(
                event.get::<serde_json::Value, _>("pricing_resolution")["pricing_version"],
                spec.pricing.pricing_version
            );
            if overflow {
                assert!(
                    event.get::<serde_json::Value, _>("pricing_resolution")["cost_usd"].is_null()
                );
            }
            assert_first_usage_event_kind(db.store.pool(), spec.id, "final").await;
            db.close().await;
        }
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn accumulated_input_tokens_boundary_is_checked() {
    accumulated_counter_boundary("input_tokens", 100).await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn accumulated_output_tokens_boundary_is_checked() {
    accumulated_counter_boundary("output_tokens", 20).await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn accumulated_rate_tokens_boundary_is_checked() {
    accumulated_counter_boundary("settled_tokens", 120).await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn accumulated_settled_count_boundary_is_checked() {
    accumulated_counter_boundary("settled_count", 1).await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn huge_integer_tool_input_preserves_bytes_but_numeric_options_never_dispatch() {
    let mut env = support::environment();
    env.insert("ALLOWED_EMAILS".into(), "alice@example.test".into());
    let yaml = support::YAML.replace(
        "fixture_version: text-v1",
        "fixture_version: native-v1\n        features: [client_tools]",
    );
    let cfg = config::load(ConfigInputs {
        yaml: &yaml,
        file_environment: None,
        environment: &env,
    })
    .unwrap();
    let h = Harness::with_config(cfg).await;
    let huge = "7".repeat(1024 * 1024);
    let body = format!(
        r#"{{"model":"synthetic-text","max_tokens":100,"messages":[{{"role":"assistant","content":[{{"type":"tool_use","id":"tool_1","name":"synthetic_tool","input":{{"value":{huge}}}}}]}}]}}"#
    );
    let r = h
        .request("/v1/messages", body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let _ = r.bytes().await.unwrap();
    assert_eq!(
        h.provider.calls.lock().await[0].body.as_ref(),
        body.as_bytes()
    );
    for body in [
        format!(
            r#"{{"model":"synthetic-text","max_tokens":{huge},"messages":[{{"role":"user","content":"x"}}]}}"#
        ),
        format!(
            r#"{{"model":"synthetic-text","max_tokens":100,"temperature":0.{huge},"messages":[{{"role":"user","content":"x"}}]}}"#
        ),
    ] {
        let r = h.request("/v1/messages", body).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert_eq!(r.headers()["x-gateway-dispatch-state"], "not_dispatched");
        let _ = r.bytes().await.unwrap();
    }
    // 비용과 무관한 미지 top-level 옵션은 해석·재직렬화 없이 원본 bytes 그대로 전달한다.
    let passthrough = format!(
        r#"{{"model":"synthetic-text","max_tokens":100,"unknown":{huge},"messages":[{{"role":"user","content":"x"}}]}}"#
    );
    let r = h
        .request("/v1/messages", passthrough.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let _ = r.bytes().await.unwrap();
    let calls = h.provider.calls.lock().await;
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].body.as_ref(), passthrough.as_bytes());
    drop(calls);
    h.close().await;
}

// S5 인증/발급 완료 증거가 아닌, 유효한 key FK/두 귀속을 가진 durable finalizer fixture다.
async fn two_scope_finalizer_fixture(pool: &sqlx::PgPool, source: uuid::Uuid) -> uuid::Uuid {
    let key = uuid::Uuid::new_v4();
    let command = uuid::Uuid::new_v4();
    let scope = uuid::Uuid::new_v4();
    let id = uuid::Uuid::new_v4();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO gw_commands(actor_principal_id,command_id,input_digest,operation,target_kind,target_id,result_status,result_projection) SELECT principal_id,$2,decode(repeat('00',32),'hex'),'issue_key','key',$3,201,'{}' FROM gw_requests WHERE id=$1").bind(source).bind(command).bind(key).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO gw_virtual_keys(id,principal_id,secret_hash,hash_version,public_prefix,name,grant_snapshot,validity_preset,created_by,created_command_id) SELECT $2,principal_id,decode(repeat('01',32),'hex'),1,'fixture','fixture','[]','unlimited',principal_id,$3 FROM gw_requests WHERE id=$1").bind(source).bind(key).bind(command).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO gw_policies(id,principal_id,key_id,allow_rules,budget_mode,period_kind,period_timezone,updated_by) SELECT $2,principal_id,$3,allow_rules,budget_mode,period_kind,period_timezone,updated_by FROM gw_policies WHERE id=(SELECT scope_id FROM gw_request_budgets WHERE request_id=$1)").bind(source).bind(scope).bind(key).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO gw_requests(id,principal_id,key_id,purpose,provider,requested_model,api,admitted_at,admit_expires_at,transport_deadline,execution_deadline,dispatch_authorized_at,lease_expires_at,executor_id,phase,accounting_state,pricing_snapshot,execution_snapshot,auth_config_digest) SELECT $2,principal_id,$3,purpose,provider,requested_model,api,admitted_at,admit_expires_at,transport_deadline,execution_deadline,dispatch_authorized_at,lease_expires_at,executor_id,phase,accounting_state,pricing_snapshot,jsonb_set(execution_snapshot,'{auth_dependency_versions,key}',jsonb_build_object('id',$3::uuid::text,'version','1')),auth_config_digest FROM gw_requests WHERE id=$1").bind(source).bind(id).bind(key).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO gw_usage_totals(scope_id,period_start,period_end,timezone_snapshot) SELECT $2,period_start,period_end,timezone_snapshot FROM gw_request_budgets WHERE request_id=$1").bind(source).bind(scope).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO gw_rate_windows(scope_id,lane,window_start,window_end,admitted_count) SELECT $2,rate_lane,rate_start,rate_end,1 FROM gw_request_budgets WHERE request_id=$1").bind(source).bind(scope).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO gw_request_budgets(request_id,scope_id,policy_version,policy_snapshot,period_start,period_end,timezone_snapshot,rate_lane,rate_start,rate_end,budget_mode,reservation_usd,remaining_reservation_usd,reserved_tokens,remaining_reserved_tokens,state) SELECT $2,sc.id,1,b.policy_snapshot,b.period_start,b.period_end,b.timezone_snapshot,b.rate_lane,b.rate_start,b.rate_end,b.budget_mode,gw_amount(0.000001),gw_amount(0.000001),3,3,'active' FROM gw_request_budgets b CROSS JOIN (SELECT scope_id AS id FROM gw_request_budgets WHERE request_id=$1 UNION ALL SELECT $3) sc WHERE b.request_id=$1").bind(source).bind(id).bind(scope).execute(&mut *tx).await.unwrap();
    sqlx::query("UPDATE gw_usage_totals SET reserved_usd=gw_amount(0.000001)")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE gw_rate_windows SET reserved_tokens=3")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    id
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn accumulated_overflow_finalizers_and_audit_are_atomic_across_scopes() {
    for field in [
        "input_tokens",
        "output_tokens",
        "settled_tokens",
        "settled_count",
    ] {
        let db = postgres::TestDb::new().await;
        let principal = finance::human(db.store.pool()).await;
        let current = finance::current(db.store.pool(), principal).await;
        let ledger = Arc::new(PgLedger::new(
            db.store.clone(),
            AccessSettings::from_config(&configuration()),
            [1; 32],
        ));
        let spec = specification();
        ledger.admit_request(&current, &spec).await.unwrap();
        let _permit = ledger.authorize_dispatch(spec.id).await.unwrap();
        let id = two_scope_finalizer_fixture(db.store.pool(), spec.id).await;
        // UUID상 뒤 scope만 overflow: 앞 scope의 정상 정산도 전체 unknown으로 바뀌어야 한다.
        let sql = if field == "settled_tokens" {
            "UPDATE gw_rate_windows SET settled_tokens=$1 WHERE scope_id=(SELECT max(scope_id::text)::uuid FROM gw_request_budgets WHERE request_id=$2)".to_owned()
        } else {
            format!(
                "UPDATE gw_usage_totals SET {field}=$1 WHERE scope_id=(SELECT max(scope_id::text)::uuid FROM gw_request_budgets WHERE request_id=$2)"
            )
        };
        sqlx::query(&sql)
            .bind(i64::MAX)
            .bind(id)
            .execute(db.store.pool())
            .await
            .unwrap();
        sqlx::query("ALTER TABLE gw_audit_events ADD CONSTRAINT injected_audit_failure CHECK(false) NOT VALID").execute(db.store.pool()).await.unwrap();
        let obs = observation();
        assert_eq!(
            ledger.finalize_request(id, &obs).await,
            Err(LedgerError::DatabaseUnavailable)
        );
        let unchanged: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_usage_totals WHERE unknown_count=0 AND settled_count=CASE WHEN $1='settled_count' AND scope_id=(SELECT max(scope_id::text)::uuid FROM gw_request_budgets WHERE request_id=$2) THEN 9223372036854775807 ELSE 0 END AND cost_usd=0 AND reserved_usd=0.000001").bind(field).bind(id).fetch_one(db.store.pool()).await.unwrap();
        assert_eq!(unchanged, 2, "audit failure partially mutated totals");
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM gw_usage_events")
                .fetch_one(db.store.pool())
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT phase FROM gw_requests WHERE id=$1")
                .bind(id)
                .fetch_one(db.store.pool())
                .await
                .unwrap(),
            "dispatching"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT inference_safety_version FROM gw_principals WHERE id=$1"
            )
            .bind(principal)
            .fetch_one(db.store.pool())
            .await
            .unwrap(),
            1
        );
        sqlx::query("ALTER TABLE gw_audit_events DROP CONSTRAINT injected_audit_failure")
            .execute(db.store.pool())
            .await
            .unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut jobs = Vec::new();
        for _ in 0..2 {
            let ledger = ledger.clone();
            let barrier = barrier.clone();
            let obs = observation();
            jobs.push(tokio::spawn(async move {
                barrier.wait().await;
                ledger.finalize_request(id, &obs).await
            }));
        }
        let mut results = Vec::new();
        for job in jobs {
            results.push(job.await.unwrap().unwrap());
        }
        assert!(results.contains(&Finalized::Unknown));
        assert!(results.contains(&Finalized::Noop));
        let mut conflict = observation();
        conflict.usage.as_mut().unwrap().output_tokens += 1;
        assert_eq!(
            ledger.finalize_request(id, &conflict).await,
            Err(LedgerError::Conflict)
        );
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM gw_usage_totals WHERE unknown_count=1 AND cost_usd=0 AND reserved_usd=0.000001").fetch_one(db.store.pool()).await.unwrap(), 2);
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM gw_request_budgets WHERE request_id=$1 AND state='unknown' AND remaining_reservation_usd=0.000001 AND remaining_reserved_tokens=3").bind(id).fetch_one(db.store.pool()).await.unwrap(), 2);
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM gw_rate_windows WHERE reserved_tokens=3"
            )
            .fetch_one(db.store.pool())
            .await
            .unwrap(),
            2
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM gw_usage_events WHERE request_id=$1"
            )
            .bind(id)
            .fetch_one(db.store.pool())
            .await
            .unwrap(),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM gw_audit_events WHERE request_id=$1"
            )
            .bind(id)
            .fetch_one(db.store.pool())
            .await
            .unwrap(),
            1
        );
        let r = sqlx::query("SELECT cost_usd::text,accounting_state,safety_violation_recorded,pricing_snapshot FROM gw_requests WHERE id=$1").bind(id).fetch_one(db.store.pool()).await.unwrap();
        assert_eq!(r.get::<String, _>("accounting_state"), "unknown");
        assert!(r.get::<Option<String>, _>("cost_usd").is_none());
        assert!(r.get::<bool, _>("safety_violation_recorded"));
        assert_eq!(
            r.get::<serde_json::Value, _>("pricing_snapshot")["pricing_version"],
            spec.pricing.pricing_version
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT inference_safety_version FROM gw_principals WHERE id=$1"
            )
            .bind(principal)
            .fetch_one(db.store.pool())
            .await
            .unwrap(),
            2
        );
        db.close().await;
    }
}

async fn unrepresentable_finalization_fallback(corrupt: &str) {
    let db = postgres::TestDb::new().await;
    let principal = finance::human(db.store.pool()).await;
    let current = finance::current(db.store.pool(), principal).await;
    let ledger = PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    );
    let spec = if corrupt == "free_count" {
        RequestSpec::from_profile(
            ProviderId::Anthropic,
            &configuration().providers()[0].profiles[1],
            None,
        )
    } else {
        specification()
    };
    ledger.admit_request(&current, &spec).await.unwrap();
    if corrupt != "no_dispatch" {
        let _permit = ledger.authorize_dispatch(spec.id).await.unwrap();
    }
    match corrupt {
        "unknown_count" => {
            sqlx::query("UPDATE gw_usage_totals SET input_tokens=$1,unknown_count=$1")
                .bind(i64::MAX)
                .execute(db.store.pool())
                .await
                .unwrap();
        }
        "request_version" => {
            sqlx::query("UPDATE gw_requests SET version=$1 WHERE id=$2")
                .bind(i64::MAX)
                .bind(spec.id)
                .execute(db.store.pool())
                .await
                .unwrap();
        }
        "safety_version" => {
            sqlx::query("UPDATE gw_usage_totals SET input_tokens=$1")
                .bind(i64::MAX)
                .execute(db.store.pool())
                .await
                .unwrap();
            sqlx::query("UPDATE gw_principals SET inference_safety_version=$1")
                .bind(i64::MAX)
                .execute(db.store.pool())
                .await
                .unwrap();
        }
        _ => {
            sqlx::query("UPDATE gw_usage_totals SET settled_count=$1")
                .bind(i64::MAX)
                .execute(db.store.pool())
                .await
                .unwrap();
        }
    }
    assert_eq!(
        ledger.finalize_request(spec.id, &observation()).await,
        Err(LedgerError::InvariantViolation),
        "corrupt={corrupt}"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM gw_usage_events")
            .fetch_one(db.store.pool())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM gw_audit_events")
            .fetch_one(db.store.pool())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT accounting_state FROM gw_requests WHERE id=$1")
            .bind(spec.id)
            .fetch_one(db.store.pool())
            .await
            .unwrap(),
        "pending"
    );
    db.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn unrepresentable_finalization_fallback_is_a_nontransient_invariant_error() {
    for corrupt in ["unknown_count", "request_version", "safety_version"] {
        unrepresentable_finalization_fallback(corrupt).await;
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn free_count_unrepresentable_counter_never_becomes_money_unknown() {
    unrepresentable_finalization_fallback("free_count").await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn no_dispatch_unrepresentable_counter_never_becomes_money_unknown() {
    unrepresentable_finalization_fallback("no_dispatch").await;
}

#[path = "support/s3_contract.rs"]
mod s3_contract;

#[tokio::test]
#[ignore = "requires dedicated TEST_DATABASE_URL, fixed SDK and explicit TEST_FNM_DIR"]
async fn anthropic_sdk_streaming_alias_and_native_error_have_one_attempt_and_real_accounting() {
    for (prefix, is_error) in [("/anthropic", false), ("", false), ("/anthropic", true)] {
        let h = Harness::new().await;
        let wire = if is_error {
            format!(
                "{}event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"overloaded_error\",\"message\":\"synthetic provider error\"}}}}\n\n",
                s3_contract::START
            )
        } else {
            format!("{}{}", s3_contract::START, s3_contract::END)
        };
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
        sdk(
            &h,
            prefix,
            if is_error {
                "messages_stream_error"
            } else {
                "messages_stream"
            },
            200,
            &h.token,
        )
        .await;
        tokio::time::timeout(std::time::Duration::from_secs(6), h.runtime.wait_requests())
            .await
            .unwrap();
        assert_eq!(h.provider.calls.lock().await.len(), 1);
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT accounting_state FROM gw_requests")
                .fetch_one(h.db.store.pool())
                .await
                .unwrap(),
            if is_error { "unknown" } else { "settled" }
        );
        h.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_reserved_last_budget_is_atomic() {
    let db = postgres::TestDb::new().await;
    let id = finance::human(db.store.pool()).await;
    sqlx::query("UPDATE gw_policies SET budget_mode='reserved',budget_limit_usd=gw_amount('0.004296000000') WHERE principal_id=$1").bind(id).execute(db.store.pool()).await.unwrap();
    let current = finance::current(db.store.pool(), id).await;
    let ledger = Arc::new(PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    ));
    let a = s4::strict_specification();
    let b = s4::strict_specification();
    assert!(
        ledger.admit_request(&current, &a).await.is_ok(),
        "verified reserved profile must be admitted"
    );
    ledger.admit_request(&current, &b).await.unwrap();
    s4::complete_count(&ledger, &current, &a).await;
    s4::complete_count(&ledger, &current, &b).await;
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let (one, two) = tokio::join!(
        async {
            barrier.wait().await;
            ledger.authorize_dispatch(a.id).await
        },
        async {
            barrier.wait().await;
            ledger.authorize_dispatch(b.id).await
        }
    );
    assert_eq!(usize::from(one.is_ok()) + usize::from(two.is_ok()), 1);
    assert!(
        matches!(one, Err(LedgerError::BudgetExhausted))
            || matches!(two, Err(LedgerError::BudgetExhausted))
    );
    let reserved: String = sqlx::query_scalar("SELECT reserved_usd::text FROM gw_usage_totals")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    assert_eq!(reserved, "0.004296000000");
    let (winner, permit) = match (one, two) {
        (Ok(permit), Err(_)) => (a.id, permit),
        (Err(_), Ok(permit)) => (b.id, permit),
        _ => panic!("정확히 하나의 송신 권한"),
    };
    let provider = provider::Provider::new();
    let server = provider.serve().await;
    let client = m_gated::providers::anthropic::AnthropicClient::loopback_for_test(
        server.base.clone(),
        "synthetic-company-key".into(),
    )
    .unwrap();
    let cfg = configuration();
    let runtime = m_gated::runtime::Runtime::new(config::Limits::default()).unwrap();
    let headers = HeaderMap::from_iter([(
        "anthropic-version".parse().unwrap(),
        "2023-06-01".parse().unwrap(),
    )]);
    let result = client
        .send(
            permit,
            Bytes::from_static(BODY.as_bytes()),
            m_gated::providers::anthropic::SendOptions {
                api: config::ApiKind::Messages,
                headers: &headers,
                profile: &cfg.providers()[0].profiles[0],
                deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(2),
                runtime: &runtime,
            },
        )
        .await
        .unwrap();
    ledger
        .finalize_request(winner, &result.observation)
        .await
        .unwrap();
    let calls = provider.calls.lock().await.len();
    db.close().await;
    assert_eq!(calls, 1);
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_internal_count_is_independent_native_call() {
    let cfg = configuration_from_yaml(&finance_yaml().replace(
        "api: messages",
        "api: messages\n        internal_count: true",
    ));
    let h = Harness::with_config(cfg).await;
    h.provider
        .set_count(
            StatusCode::OK,
            HeaderMap::from_iter([(
                "content-type".parse().unwrap(),
                "application/json".parse().unwrap(),
            )]),
            Bytes::from_static(br#"{"input_tokens":321}"#),
        )
        .await;
    let response = h.request("/v1/messages", BODY).send().await.unwrap();
    let status = response.status();
    let _ = response.bytes().await.unwrap();
    let rows=sqlx::query("SELECT purpose,api,cost_usd::text AS cost,parent_request_id,(SELECT normalized_usage->>'counted_input_tokens' FROM gw_usage_events e WHERE e.request_id=r.id AND kind='final') AS counted FROM gw_requests r ORDER BY purpose").fetch_all(h.db.store.pool()).await.unwrap();
    let calls = h.provider.calls.lock().await.clone();
    h.close().await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rows.len(), 2, "internal count needs its own durable child");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].path, "/v1/messages/count_tokens");
    assert_eq!(calls[1].path, "/v1/messages");
    assert_eq!(calls[1].body.as_ref(), BODY.as_bytes());
    let projected: serde_json::Value = serde_json::from_slice(&calls[0].body).unwrap();
    assert!(projected.get("max_tokens").is_none());
    assert_eq!(
        projected["messages"][0]["content"],
        "synthetic prompt sentinel"
    );
    assert_eq!(rows[0].get::<String, _>("purpose"), "internal_count");
    assert_eq!(rows[0].get::<String, _>("cost"), "0.000000000000");
    assert_eq!(rows[0].get::<String, _>("counted"), "321");
    assert!(
        rows[0]
            .get::<Option<uuid::Uuid>, _>("parent_request_id")
            .is_some()
    );
}

#[path = "support/s4.rs"]
mod s4;

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
        h.provider.set(StatusCode::from_u16(upstream_status).unwrap(),HeaderMap::from_iter([("content-type".parse().unwrap(),"application/json".parse().unwrap()),("retry-after".parse().unwrap(),"1".parse().unwrap())]),Bytes::from_static(br#"{"type":"error","error":{"type":"overloaded_error","message":"synthetic error"}}"#)).await;
        error_compat::error_sdk(
            &h.base,
            if input == "invalid" {
                "synthetic-invalid"
            } else {
                &h.token
            },
            "anthropic",
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

#[cfg(target_os = "macos")]
#[path = "support/claude_code.rs"]
mod claude_code;
#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL, pinned CLI and macOS sandbox"]
async fn s9_pinned_claude_code_read_only_initialization_is_blocked_before_auth_and_http() {
    let h = Harness::new().await;
    let cli = claude_code::read_only_initialization(&h.base, &h.token).await;
    h.runtime.wait_requests().await;
    eprintln!(
        "CLI_DIAGNOSTIC exit={} helper={} permission={} status400={} http={} provider={}",
        cli.exit,
        cli.helpers,
        cli.permission,
        cli.status400,
        h.wire.lock().await.len(),
        h.provider.calls.lock().await.len()
    );
    error_compat::assert_error_ledger(h.db.store.pool(), false).await;
    assert_eq!(cli.helpers, 0);
    assert_eq!(cli.exit, 1);
    assert!(cli.permission && cli.config_open_denied);
    assert!(!cli.status400);
    assert_eq!(h.runtime.request_resources().finished, 0);
    assert!(h.wire.lock().await.is_empty());
    assert_eq!(h.runtime.available_inference(), 16);
    assert_eq!(h.runtime.active_requests(), 0);
    assert!(h.provider.calls.lock().await.is_empty());
    error_compat::assert_error_ledger(h.db.store.pool(), false).await;
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL and fixed SDK"]
async fn s9_stream_syntax_and_limit_have_distinct_h0_h1_and_sdk_causes() {
    for (bad, code) in [
        ("data: {bad}\n\n".to_string(), "stream_parse_error"),
        (format!("event: {}\n", "x".repeat(129)), "stream_limit"),
    ] {
        for mode in ["h0", "h1", "sdk"] {
            let h = Harness::with_config(configuration()).await;
            let body = BODY.replacen('{', "{\"stream\":true,", 1);
            let prefix = s3_contract::START.to_string();
            let (tx, stopped) = h.provider.stream().await;
            if mode == "h1" {
                tx.send(Bytes::from(prefix.clone())).await.unwrap();
            } else {
                tx.send(Bytes::from(bad.clone())).await.unwrap();
            }
            if mode == "sdk" {
                error_compat::error_stream_sdk(&h.base, &h.token, "anthropic", 502, code, true)
                    .await;
            } else {
                let mut r = h
                    .request("/anthropic/v1/messages", body)
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
                    assert_eq!(v["error"]["type"], "api_error");
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
                ("0.000000000000".into(), "0.000000000000".into(), 0, 0)
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
async fn s9_body_timeout_and_draining_are_n_with_zero_dispatch_and_rows() {
    let h = Harness::with_config(configuration_from_yaml(&format!(
        "{}\nlimits:\n  body_timeout_ms: 100\n  body_idle_timeout_ms: 50\n",
        finance_yaml()
    )))
    .await;
    let pending = futures_util::stream::pending::<Result<Bytes, std::io::Error>>();
    let r = h
        .request(
            "/anthropic/v1/messages",
            reqwest::Body::wrap_stream(pending),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 408);
    assert_eq!(r.headers()["x-gateway-error-code"], "request_body_timeout");
    assert_eq!(r.headers()["x-gateway-dispatch-state"], "not_dispatched");
    assert!(!r.headers().contains_key("retry-after"));
    let body: serde_json::Value = serde_json::from_slice(&r.bytes().await.unwrap()).unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    h.runtime.wait_requests().await;
    error_compat::assert_error_ledger(h.db.store.pool(), false).await;
    h.runtime.begin_draining();
    let r = h
        .request("/anthropic/v1/messages", BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    assert_eq!(r.headers()["x-gateway-error-code"], "draining");
    assert_eq!(r.headers()["x-gateway-dispatch-state"], "not_dispatched");
    assert_eq!(r.headers()["retry-after"], "1");
    let body: serde_json::Value = serde_json::from_slice(&r.bytes().await.unwrap()).unwrap();
    assert_eq!(body["error"]["type"], "overloaded_error");
    assert!(h.provider.calls.lock().await.is_empty());
    assert_eq!(h.runtime.request_resources().body_bytes, 0);
    error_compat::assert_error_ledger(h.db.store.pool(), false).await;
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s9_finalization_h0_failure_is_503_pending_not_unknown_or_success() {
    let h = Harness::new().await;
    sqlx::raw_sql("CREATE FUNCTION gw_s9_fail_final() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic finalization failure'; END $$; CREATE TRIGGER gw_s9_fail_final BEFORE UPDATE ON gw_requests FOR EACH ROW WHEN (NEW.phase='finished') EXECUTE FUNCTION gw_s9_fail_final();").execute(h.db.store.pool()).await.unwrap();
    let r = h
        .request("/anthropic/v1/messages", BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    assert_eq!(
        r.headers()["x-gateway-error-code"],
        "finalization_unavailable"
    );
    assert_eq!(r.headers()["x-should-retry"], "false");
    assert!(!r.headers().contains_key("x-gateway-dispatch-state"));
    assert!(!r.headers().contains_key("retry-after"));
    let id = r.headers()["x-gateway-request-id"]
        .to_str()
        .unwrap()
        .parse::<uuid::Uuid>()
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&r.bytes().await.unwrap()).unwrap();
    assert_eq!(body["error"]["type"], "overloaded_error");
    h.runtime.wait_requests().await;
    let row:(String,String,bool,i64,i64,bool)=sqlx::query_as("SELECT phase,accounting_state,cost_usd IS NULL,(SELECT count(*) FROM gw_usage_events),(SELECT COALESCE(sum(unknown_count),0)::bigint FROM gw_usage_totals),(SELECT bool_and(remaining_reservation_usd=0 AND remaining_reserved_tokens=0) FROM gw_request_budgets) FROM gw_requests WHERE id=$1").bind(id).fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(
        row,
        ("dispatching".into(), "pending".into(), true, 0, 0, true)
    );
    assert_eq!(h.runtime.failed_request_ids(), vec![id]);
    assert_eq!(h.provider.calls.lock().await.len(), 1);
    assert_eq!(h.runtime.request_resources().body_bytes, 0);
    assert_eq!(h.runtime.available_inference(), 16);
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s9_verified_terminal_before_stream_limit_settles_once_and_releases_reservation() {
    let h = Harness::with_config(configuration()).await;

    let body = BODY.replacen('{', "{\"stream\":true,", 1);
    let native = format!("{}{}", s3_contract::START, s3_contract::END);
    let (tx, stopped) = h.provider.stream().await;
    tx.send(Bytes::from(native.clone())).await.unwrap();
    let mut r = h
        .request("/anthropic/v1/messages", body.clone())
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
            Some("0.000140000000".into())
        )
    );
    let budget:(String,i64,i64,String,i64)=sqlx::query_as("SELECT b.remaining_reservation_usd::text,b.remaining_reserved_tokens,t.unknown_count,t.cost_usd::text,(SELECT count(*) FROM gw_usage_events WHERE request_id=$1 AND kind='final') FROM gw_request_budgets b JOIN gw_usage_totals t ON t.scope_id=b.scope_id AND t.period_start=b.period_start WHERE b.request_id=$1").bind(id).fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(
        budget,
        ("0.000000000000".into(), 0, 0, "0.000140000000".into(), 1)
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
        .request("/anthropic/v1/messages", body)
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
    let h = Harness::with_config(configuration()).await;

    let body = BODY.replacen('{', "{\"stream\":true,", 1);
    let native = format!(
        "{}{}",
        s3_contract::START.replace("synthetic-text", "synthetic-unpriced"),
        s3_contract::END
    );
    let (tx, stopped) = h.provider.stream().await;
    tx.send(Bytes::from(native.clone())).await.unwrap();
    let mut r = h
        .request("/anthropic/v1/messages", body.clone())
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
            "0.000000000000".into(),
            0,
            1
        )
    );
    let blocked = h
        .request("/anthropic/v1/messages", body)
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
async fn s9_admitted_main_before_authorize_is_n_even_with_dispatched_count_child() {
    s9_count_cancellation_dispatch_evidence(false).await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s9_authorize_entered_main_is_conservative_not_n_when_cancelled() {
    s9_count_cancellation_dispatch_evidence(true).await;
}
async fn s9_count_cancellation_dispatch_evidence(enter_authorize: bool) {
    let cfg = configuration_from_yaml(&finance_yaml().replace(
        "api: messages",
        "api: messages\n        internal_count: true",
    ));
    let h = Harness::with_config(cfg).await;
    h.provider
        .set_count(
            StatusCode::OK,
            HeaderMap::from_iter([(
                "content-type".parse().unwrap(),
                "application/json".parse().unwrap(),
            )]),
            Bytes::from_static(br#"{"input_tokens":321}"#),
        )
        .await;
    let (started, release) = h.provider.hold_count().await;
    let mut phase_changes = h.runtime.resource_changes();
    let request = h.request("/anthropic/v1/messages", BODY).send();
    let interrupt = async {
        started.await.unwrap();
        let root: uuid::Uuid =
            sqlx::query_scalar("SELECT id FROM gw_requests WHERE purpose='root'")
                .fetch_one(h.db.store.pool())
                .await
                .unwrap();
        let child_dispatch:bool=sqlx::query_scalar("SELECT dispatch_authorized_at IS NOT NULL FROM gw_requests WHERE purpose='internal_count'").fetch_one(h.db.store.pool()).await.unwrap();
        assert!(child_dispatch);
        if enter_authorize {
            let mut holder = h.db.store.pool().begin().await.unwrap();
            sqlx::query("SELECT id FROM gw_requests WHERE id=$1 FOR UPDATE")
                .bind(root)
                .fetch_one(&mut *holder)
                .await
                .unwrap();
            release.send(()).unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if h.runtime.request_phases().iter().any(|(id, phase)| {
                        *id == root
                            && *phase == m_gated::runtime::request::RequestPhase::Dispatching
                    }) {
                        break;
                    }
                    phase_changes.changed().await.unwrap();
                }
            })
            .await
            .unwrap();
            // phase는 진입 의도이며 COMMIT 증명이 아니다. root 잠금은 승인 완료를 막는다.
            h.runtime.cancel_requests();
            holder.rollback().await.unwrap();
        } else {
            h.runtime.cancel_requests();
            // 응답 준비 중인 count를 먼저 해제하지 않아 main authorize 이전 경계를 유지한다.
            drop(release);
        }
    };
    let (response, ()) = tokio::time::timeout(std::time::Duration::from_secs(6), async {
        tokio::join!(request, interrupt)
    })
    .await
    .unwrap();
    let r = response.unwrap();
    assert_eq!(r.status(), 504);
    let header_n = r
        .headers()
        .get("x-gateway-dispatch-state")
        .map(|v| v.as_bytes().to_vec());
    let retry = r
        .headers()
        .get("x-should-retry")
        .map(|v| v.as_bytes().to_vec());
    r.bytes().await.unwrap();
    h.runtime.wait_requests().await;
    let rows:Vec<(String,String,bool,String)>=sqlx::query_as("SELECT purpose,accounting_state,dispatch_authorized_at IS NOT NULL,cost_usd::text FROM gw_requests ORDER BY purpose").fetch_all(h.db.store.pool()).await.unwrap();
    let paths: Vec<String> = h
        .provider
        .calls
        .lock()
        .await
        .iter()
        .map(|c| c.path.clone())
        .collect();
    let available = h.runtime.available_inference();
    h.close().await;
    assert_eq!(
        rows,
        vec![
            (
                "internal_count".into(),
                "settled".into(),
                true,
                "0.000000000000".into()
            ),
            (
                "root".into(),
                "settled".into(),
                false,
                "0.000000000000".into()
            )
        ]
    );
    assert_eq!(paths, vec!["/v1/messages/count_tokens"]);
    assert_eq!(available, 16);
    assert_eq!(
        header_n,
        (!enter_authorize).then(|| b"not_dispatched".to_vec())
    );
    assert_eq!(retry, enter_authorize.then(|| b"false".to_vec()));
}

#[path = "support/s9_finality.rs"]
mod s9_finality;
async fn s9_finality_fixture(
    alias: bool,
) -> (Harness, &'static str, String, String, String, &'static str) {
    let h = Harness::with_config(configuration()).await;
    (
        h,
        if alias {
            "/v1/messages"
        } else {
            "/anthropic/v1/messages"
        },
        BODY.replacen('{', "{\"stream\":true,", 1),
        format!("{}{}", s3_contract::START, s3_contract::END),
        s3_contract::START.to_string(),
        "0.000140000000",
    )
}

#[path = "support/s9_anthropic_error.rs"]
mod s9_anthropic_error;

fn s9_native_error_tail(length: usize) -> String {
    format!(
        "event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"api_error\",\"message\":\"{}\"}}}}\n\n",
        "x".repeat(length)
    )
}

#[path = "support/s10.rs"]
mod s10;

#[path = "support/s10_process.rs"]
mod s10_process;

#[path = "support/closure.rs"]
mod closure;
