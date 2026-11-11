#[path = "support/finance.rs"]
#[allow(dead_code)]
mod finance;
#[path = "support/postgres.rs"]
mod postgres;
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
    config::{self, ConfigInputs},
    http::{self, AppState},
    ledger::{
        PgLedger,
        commands::CommandContext,
        finance::{FinalObservation, FinalOutcome, RequestSpec},
        management::{Command, ManagementError},
        management_query::QueryContext,
    },
    runtime::{Dependency, Runtime},
};
use serde_json::{Value, json};
use sqlx::Row;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;
fn config() -> config::ValidatedConfig {
    let mut e = support::environment();
    e.insert(
        "ALLOWED_EMAILS".into(),
        "alice@example.test,bob@example.test".into(),
    );
    e.insert(
        "ADMIN_EMAILS".into(),
        "alice@example.test,bob@example.test".into(),
    );
    config::load(ConfigInputs {
        yaml: &format!("{}{}",support::YAML,"      - model: synthetic-text\n        api: count_tokens\n        profile_version: count-v1\n        pricing_version: count-price-v1\n        fixture_version: count-v1\n        billing_class: guaranteed_free\n        actual_models: [synthetic-text]\n        max_input_tokens: 4096\n        max_body_bytes: 8388608\n        max_response_bytes: 262144\n        rates:\n          input: '0.000000000000'\n"),
        file_environment: None,
        environment: &e,
    })
    .unwrap()
}
fn grants() -> Value {
    json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["count_tokens","messages"]}])
}
fn issue() -> Command {
    Command::Issue(serde_json::from_value(json!({"name":"테스트 키","validity":"3_months","grants":grants(),"limits":{"budget":null,"rpm":null,"tpm":null,"count_rpm":null}})).unwrap())
}
fn ctx<'a>(
    actor: Uuid,
    command_id: Uuid,
    settings: &'a AccessSettings,
    grants: &'a Value,
    ceiling: &'a m_gated::ledger::policy::PolicyLimits,
    method: &'a str,
    path: &'a str,
) -> CommandContext<'a> {
    CommandContext {
        actor,
        command_id,
        settings,
        grants,
        ceiling,
        method,
        path,
        digest: [7; 32],
        execution_running: false,
    }
}
fn qctx<'a>(
    actor: Uuid,
    settings: &'a AccessSettings,
    grants: &'a Value,
    admin: bool,
) -> QueryContext<'a> {
    QueryContext {
        actor,
        admin,
        settings,
        grants,
        digest: "0707070707070707070707070707070707070707070707070707070707070707",
        signing_key: b"synthetic-only-signing-key",
    }
}
async fn bob(db: &postgres::TestDb) -> Uuid {
    let id = finance::human(db.store.pool()).await;
    sqlx::query("UPDATE gw_principals SET email='bob@example.test' WHERE id=$1")
        .bind(id)
        .execute(db.store.pool())
        .await
        .unwrap();
    id
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn lost_key_issue_response_has_one_effect_and_no_secret_replay() {
    let db = postgres::TestDb::new().await;
    let cfg = config();
    let settings = AccessSettings::from_config(&cfg);
    let g = grants();
    let ceiling = Default::default();
    let actor = finance::human(db.store.pool()).await;
    let ledger = PgLedger::new(db.store.clone(), settings.clone(), [7; 32]);
    let command = Uuid::new_v4();
    let c = ctx(
        actor,
        command,
        &settings,
        &g,
        &ceiling,
        "POST",
        "/gateway/keys",
    );
    let first = ledger.management_command(&c, issue()).await.unwrap();
    assert_eq!(first.status, 201);
    let body = first.body.unwrap();
    let raw = body["key"].as_str().unwrap();
    let id = Uuid::parse_str(body["metadata"]["id"].as_str().unwrap()).unwrap();
    let stored = sqlx::query(
        "SELECT secret_hash,hash_version,public_prefix FROM gw_virtual_keys WHERE id=$1",
    )
    .bind(id)
    .fetch_one(db.store.pool())
    .await
    .unwrap();
    assert_eq!(stored.get::<Vec<u8>, _>("secret_hash").len(), 32);
    assert_eq!(stored.get::<i16, _>("hash_version"), 1);
    assert!(
        !stored
            .get::<String, _>("public_prefix")
            .contains(&raw[40..])
    );
    assert!(!body["metadata"].to_string().contains("secret_hash"));
    assert!(
        matches!(ledger.management_command(&c,issue()).await,Err(ManagementError::SecretUnavailable(k)) if k==id)
    );
    let mut conflict = issue();
    if let Command::Issue(v) = &mut conflict {
        v.name = "다른 이름".into();
    }
    assert!(matches!(
        ledger.management_command(&c, conflict).await,
        Err(ManagementError::CommandConflict)
    ));
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_virtual_keys")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    assert_eq!(rows, 1);
    let audit: Value = sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(a)) FROM gw_audit_events a")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    let commands: Value = sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(c)) FROM gw_commands c")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    assert!(!audit.to_string().contains(raw));
    assert!(!commands.to_string().contains(raw));
    let p = db.store.authenticate_key(raw, &settings).await.unwrap();
    assert_eq!(p.current.id, actor);
    assert_eq!(p.key.unwrap().id, id);
    let current = finance::current(db.store.pool(), actor).await;
    let spec = RequestSpec::from_profile(
        config::ProviderId::Anthropic,
        &cfg.providers()[0].profiles[0],
        Some(16),
    );
    ledger
        .admit_scoped(&current, &spec, p.key, None)
        .await
        .unwrap();
    let _permit = ledger.authorize_dispatch(spec.id).await.unwrap();
    let other = bob(&db).await;
    let own = db
        .store
        .management_list(&qctx(other, &settings, &g, false), "keys", None)
        .await
        .unwrap();
    assert_eq!(own["items"], json!([]));
    let all = db
        .store
        .management_list(&qctx(other, &settings, &g, true), "keys", None)
        .await
        .unwrap();
    assert_eq!(all["items"].as_array().unwrap().len(), 1);
    assert!(!all.to_string().contains(raw));
    assert!(!all.to_string().contains("secret_hash"));
    let c = ctx(
        actor,
        Uuid::new_v4(),
        &settings,
        &g,
        &ceiling,
        "DELETE",
        "/gateway/keys/id",
    );
    assert!(matches!(
        ledger
            .management_command(
                &c,
                Command::Revoke {
                    id,
                    expected: "2".into()
                }
            )
            .await,
        Err(ManagementError::Version(1))
    ));
    assert_eq!(
        ledger
            .management_command(
                &c,
                Command::Revoke {
                    id,
                    expected: "1".into()
                }
            )
            .await
            .unwrap()
            .status,
        204
    );
    let c = ctx(
        actor,
        Uuid::new_v4(),
        &settings,
        &g,
        &ceiling,
        "DELETE",
        "/gateway/keys/id",
    );
    assert_eq!(
        ledger
            .management_command(
                &c,
                Command::Revoke {
                    id,
                    expected: "1".into()
                }
            )
            .await
            .unwrap()
            .status,
        204
    );
    assert!(matches!(
        db.store.authenticate_key(raw, &settings).await,
        Err(m_gated::auth::AuthError::InvalidCredentials)
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM gw_audit_events WHERE operation='revoke_key'"
        )
        .fetch_one(db.store.pool())
        .await
        .unwrap(),
        1
    );
    db.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn command_cas_and_audit_are_atomic() {
    let db = postgres::TestDb::new().await;
    let cfg = config();
    let settings = AccessSettings::from_config(&cfg);
    let g = grants();
    let ceiling = Default::default();
    let actor = finance::human(db.store.pool()).await;
    let ledger = PgLedger::new(db.store.clone(), settings.clone(), [7; 32]);
    let c = ctx(
        actor,
        Uuid::new_v4(),
        &settings,
        &g,
        &ceiling,
        "POST",
        "/gateway/keys",
    );
    sqlx::raw_sql("CREATE FUNCTION gw_test_audit_fail() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RAISE EXCEPTION 'synthetic'; END$$;CREATE TRIGGER gw_test_audit_fail BEFORE INSERT ON gw_audit_events FOR EACH ROW EXECUTE FUNCTION gw_test_audit_fail();").execute(db.store.pool()).await.unwrap();
    assert!(ledger.management_command(&c, issue()).await.is_err());
    for table in ["gw_commands", "gw_virtual_keys", "gw_audit_events"] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(db.store.pool())
                .await
                .unwrap(),
            0
        );
    }
    sqlx::query("DROP TRIGGER gw_test_audit_fail ON gw_audit_events")
        .execute(db.store.pool())
        .await
        .unwrap();
    assert_eq!(
        ledger.management_command(&c, issue()).await.unwrap().status,
        201
    );
    // 같은 actor/command를 barrier로 두 실제 연결에서 동시에 시작한다.
    let command = Uuid::new_v4();
    let c = ctx(
        actor,
        command,
        &settings,
        &g,
        &ceiling,
        "POST",
        "/gateway/keys",
    );
    let barrier = tokio::sync::Barrier::new(2);
    let a = async {
        barrier.wait().await;
        ledger.management_command(&c, issue()).await
    };
    let b = async {
        barrier.wait().await;
        ledger.management_command(&c, issue()).await
    };
    let (a, b) = tokio::join!(a, b);
    assert!(matches!(
        (&a, &b),
        (Ok(_), Err(ManagementError::SecretUnavailable(_)))
            | (Err(ManagementError::SecretUnavailable(_)), Ok(_))
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM gw_virtual_keys")
            .fetch_one(db.store.pool())
            .await
            .unwrap(),
        2
    );
    db.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn principal_summary_and_resource_cursors_are_distinct() {
    let db = postgres::TestDb::new().await;
    let cfg = config();
    let settings = AccessSettings::from_config(&cfg);
    let g = grants();
    let ceiling = Default::default();
    let actor = finance::human(db.store.pool()).await;
    let other = bob(&db).await;
    let ledger = PgLedger::new(db.store.clone(), settings.clone(), [7; 32]);
    for _ in 0..2 {
        let c = ctx(
            actor,
            Uuid::new_v4(),
            &settings,
            &g,
            &ceiling,
            "POST",
            "/gateway/keys",
        );
        ledger.management_command(&c, issue()).await.unwrap();
    }
    let q = qctx(actor, &settings, &g, true);
    let first = db
        .store
        .management_list(&q, "keys", Some("limit=1"))
        .await
        .unwrap();
    let cursor = first["next_cursor"].as_str().unwrap();
    let query = format!("cursor={cursor}");
    let second = db
        .store
        .management_list(&q, "keys", Some(&query))
        .await
        .unwrap();
    assert_ne!(first["items"][0]["id"], second["items"][0]["id"]);
    for resource in ["principals", "requests"] {
        assert!(matches!(
            db.store.management_list(&q, resource, Some(&query)).await,
            Err(ManagementError::Query(
                m_gated::ledger::dashboard::QueryError::InvalidCursor
            ))
        ));
    }
    assert!(
        db.store
            .management_list(&qctx(other, &settings, &g, true), "keys", Some(&query))
            .await
            .is_err()
    );
    let principals = db
        .store
        .management_list(&q, "principals", Some("limit=1"))
        .await
        .unwrap();
    let summary = &principals["items"][0];
    assert!(summary.get("policy").is_none());
    assert!(summary.get("policy_version").is_some());
    assert_eq!(summary["disabled"], false);
    assert!(summary["management_owner_id"].is_null());
    let id = Uuid::parse_str(summary["id"].as_str().unwrap()).unwrap();
    let detail = db
        .store
        .management_detail(&q, "principals", id, &ceiling)
        .await
        .unwrap();
    assert!(detail["principal"].get("policy").is_some());
    assert!(detail["principal"].get("policy_version").is_none());
    let c = principals["next_cursor"].as_str().unwrap();
    assert!(
        db.store
            .management_list(&q, "keys", Some(&format!("cursor={c}")))
            .await
            .is_err()
    );
    for f in [
        "limit=101",
        "kind=no",
        "disabled=1",
        "kind=human&kind=service",
        "role=admin",
    ] {
        assert!(
            db.store
                .management_list(&q, "principals", Some(f))
                .await
                .is_err()
        );
    }
    let usage = db
        .store
        .management_usage(&q, Some("group_by=provider"), &ceiling)
        .await
        .unwrap();
    assert_eq!(usage["balance_context"], "aggregate_no_balance");
    assert_eq!(usage["budget_balances"], json!([]));
    assert_eq!(usage["block_state"], Value::Null);
    db.close().await;
}
async fn unknown(
    db: &postgres::TestDb,
    ledger: &PgLedger,
    cfg: &config::ValidatedConfig,
    actor: Uuid,
) -> Uuid {
    let current = finance::current(db.store.pool(), actor).await;
    let spec = RequestSpec::from_profile(
        config::ProviderId::Anthropic,
        &cfg.providers()[0].profiles[0],
        Some(16),
    );
    ledger.admit_request(&current, &spec).await.unwrap();
    let _p = ledger.authorize_dispatch(spec.id).await.unwrap();
    ledger
        .finalize_request(
            spec.id,
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
    spec.id
}
fn settlement(id: Uuid, expected: &str, reason: &str, confirmed: bool) -> Command {
    Command::Settle(id,serde_json::from_value(json!({"expected_version":expected,"decision":"charged","cost_usd":"0.123456789012","reason":reason,"execution_stopped_confirmed":if confirmed{Some(true)}else{None}})).unwrap())
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn manual_settlement_records_admin_entry_and_late_final_without_repricing() {
    let db = postgres::TestDb::new().await;
    let cfg = config();
    let settings = AccessSettings::from_config(&cfg);
    let g = grants();
    let ceiling = Default::default();
    let actor = finance::human(db.store.pool()).await;
    let admin = bob(&db).await;
    let ledger = PgLedger::new(db.store.clone(), settings.clone(), [7; 32]);
    let id = unknown(&db, &ledger, &cfg, actor).await;
    let c = ctx(
        admin,
        Uuid::new_v4(),
        &settings,
        &g,
        &ceiling,
        "PUT",
        "/gateway/admin/requests/id/settlement",
    );
    assert!(matches!(
        ledger
            .management_command(&c, settlement(id, "3", "확인 전", false))
            .await,
        Err(ManagementError::ExecutionNotFinal)
    ));
    let first = ledger
        .management_command(&c, settlement(id, "3", "관리자 입력", true))
        .await
        .unwrap();
    assert_eq!(first.status, 200);
    assert_eq!(first.body.unwrap()["effect"], "changed");
    let c = ctx(
        admin,
        Uuid::new_v4(),
        &settings,
        &g,
        &ceiling,
        "PUT",
        "/gateway/admin/requests/id/settlement",
    );
    let noop = ledger
        .management_command(&c, settlement(id, "1", "관리자 입력", false))
        .await
        .unwrap();
    assert_eq!(noop.body.unwrap()["effect"], "noop");
    assert!(matches!(
        ledger
            .management_command(
                &ctx(
                    admin,
                    Uuid::new_v4(),
                    &settings,
                    &g,
                    &ceiling,
                    "PUT",
                    "/gateway/admin/requests/id/settlement"
                ),
                settlement(id, "1", "상충", true)
            )
            .await,
        Err(ManagementError::SettlementConflict)
    ));
    let detail = db
        .store
        .management_detail(&qctx(admin, &settings, &g, true), "requests", id, &ceiling)
        .await
        .unwrap();
    assert_eq!(detail["settlement"]["source"], "manual_entry");
    assert_eq!(
        detail["settlement"]["execution_stop_source"],
        "admin_confirmation"
    );
    assert_eq!(detail["cost_usd"], "0.123456789012");
    assert!(detail["usage"]["final"]["input_tokens"].is_null());
    let usage = db
        .store
        .management_usage(
            &qctx(admin, &settings, &g, true),
            Some(&format!("principal_id={actor}&group_by=provider")),
            &ceiling,
        )
        .await
        .unwrap();
    assert_eq!(usage["settled_usd"], "0.123456789012");
    assert_eq!(usage["buckets"]["items"][0]["tokens_complete"], false);
    assert_eq!(usage["buckets"]["items"][0]["input_tokens"], Value::Null);
    let final_usage = m_gated::ledger::finance::FinalUsage {
        actual_model: "synthetic-text".into(),
        input_tokens: 10,
        output_tokens: 2,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: None,
    };
    let late = ledger
        .finalize_request(id, &FinalObservation::completed(final_usage))
        .await;
    assert!(
        late.is_ok(),
        "S5: 늦은 final은 수동 금액을 덮지 않는 별도 관측이어야 함: {late:?}"
    );
    let d = db
        .store
        .management_detail(&qctx(admin, &settings, &g, true), "requests", id, &ceiling)
        .await
        .unwrap();
    assert_eq!(d["cost_usd"], "0.123456789012");
    assert_eq!(d["usage"]["late_final"]["input_tokens"], "10");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM gw_audit_events WHERE operation='settle_unknown'"
        )
        .fetch_one(db.store.pool())
        .await
        .unwrap(),
        1
    );
    db.close().await;
}
#[path = "support/provider.rs"]
#[allow(dead_code)]
mod provider;
struct Harness {
    db: postgres::TestDb,
    cfg: config::ValidatedConfig,
    router: axum::Router,
    runtime: Arc<Runtime>,
    actor: Uuid,
    token: String,
    provider: Arc<provider::Provider>,
    _provider_server: provider::Server,
    _jwks: sso::Server,
}
impl Harness {
    async fn new() -> Self {
        Self::with_clock(false).await
    }
    async fn with_clock(clock: bool) -> Self {
        Self::with_config(clock, config()).await
    }
    async fn with_config(clock: bool, cfg: config::ValidatedConfig) -> Self {
        let mut db = postgres::TestDb::new().await;
        if clock {
            let store = db.clock_store(None).await;
            let old = std::mem::replace(&mut db.store, store);
            old.pool().close().await;
        }
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
        let actor = current.id;
        sqlx::query("UPDATE gw_policies SET allow_rules=$2 WHERE id=$1")
            .bind(current.policy_id)
            .bind(grants())
            .execute(db.store.pool())
            .await
            .unwrap();
        let auth = Arc::new(SsoAuthenticator::new(&cfg, verifier, db.store.clone()));
        let api = Arc::new(m_gated::admin::DashboardApi::new(
            &cfg,
            auth.clone(),
            db.store.clone(),
        ));
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
        let gateway = Arc::new(m_gated::gateway::InferenceGateway::assemble(
            &cfg.providers()[0],
            auth,
            ledger,
            client,
        ));
        let router = http::router_with_components(
            AppState {
                runtime: runtime.clone(),
            },
            Some(gateway),
            api.routes(runtime.clone()),
        );
        Self {
            db,
            cfg,
            router,
            runtime,
            actor,
            token,
            provider: p,
            _provider_server: provider_server,
            _jwks: jwks,
        }
    }
    #[allow(clippy::too_many_arguments)]
    async fn request(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        cookie: bool,
        body: Option<Value>,
        command: Option<Uuid>,
        extra: &[(&str, &str)],
    ) -> (u16, Value, axum::http::HeaderMap) {
        let mut r = Request::builder().method(method).uri(path);
        if let Some(token) = token {
            r = if cookie {
                r.header("cookie", format!("mgated_access={token}"))
            } else {
                r.header("authorization", format!("Bearer {token}"))
            };
        }
        if let Some(id) = command {
            r = r.header("x-gateway-command-id", id.to_string());
        }
        for (k, v) in extra {
            r = r.header(*k, *v);
        }
        let body = if let Some(v) = body {
            r = r.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        } else {
            Body::empty()
        };
        let r = self
            .router
            .clone()
            .oneshot(r.body(body).unwrap())
            .await
            .unwrap();
        let status = r.status().as_u16();
        let headers = r.headers().clone();
        let bytes = to_bytes(r.into_body(), 1024 * 1024).await.unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, value, headers)
    }
    async fn close(self) {
        self.runtime.stop();
        self.db.close().await;
    }
}
fn key_input() -> Value {
    json!({"name":"개인 키","validity":"unlimited","grants":grants(),"limits":{"budget":null,"rpm":null,"tpm":null,"count_rpm":null}})
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn every_cookie_write_requires_origin_and_bound_csrf_and_virtual_is_inference_only() {
    let h = Harness::new().await;
    let (s, v, _) = h
        .request(
            "GET",
            "/gateway/csrf",
            Some(&h.token),
            true,
            None,
            None,
            &[],
        )
        .await;
    assert_eq!(s, 200);
    let csrf = v["token"].as_str().unwrap();
    let origin = h.cfg.public_origin().as_str().trim_end_matches('/');
    let command = Uuid::new_v4();
    for extra in [
        vec![],
        vec![("origin", "null"), ("x-csrf-token", csrf)],
        vec![("origin", origin)],
        vec![("origin", origin), ("x-csrf-token", "bad")],
    ] {
        let (s, _, _) = h
            .request(
                "POST",
                "/gateway/keys",
                Some(&h.token),
                true,
                Some(key_input()),
                Some(command),
                &extra,
            )
            .await;
        assert_eq!(s, 403);
    }
    let (s, body, headers) = h
        .request(
            "POST",
            "/gateway/keys",
            Some(&h.token),
            true,
            Some(key_input()),
            Some(command),
            &[("origin", origin), ("x-csrf-token", csrf)],
        )
        .await;
    assert_eq!(s, 201);
    assert_eq!(headers["x-gateway-command-replayed"], "false");
    let raw = body["key"].as_str().unwrap();
    let id = body["metadata"]["id"].as_str().unwrap();
    let (s, replay, headers) = h
        .request(
            "POST",
            "/gateway/keys",
            Some(&h.token),
            false,
            Some(key_input()),
            Some(command),
            &[],
        )
        .await;
    assert_eq!(s, 409);
    assert_eq!(replay["error"]["code"], "key_secret_unavailable");
    assert_eq!(replay["error"]["details"]["key_id"], id);
    assert_eq!(headers["x-gateway-command-id"], command.to_string());
    assert_eq!(headers["x-gateway-command-replayed"], "true");
    for path in [
        "/gateway/me",
        "/gateway/keys",
        "/gateway/usage",
        "/gateway/admin/principals",
        "/gateway/admin/requests",
        "/gateway/csrf",
    ] {
        let (s, _, _) = h
            .request("GET", path, Some(raw), false, None, None, &[])
            .await;
        assert_eq!(s, 403);
    }
    let request = json!({"model":"synthetic-text","max_tokens":16,"messages":[{"role":"user","content":"synthetic-only"}]});
    let (s, _, _) = h
        .request(
            "POST",
            "/anthropic/v1/messages",
            Some(raw),
            false,
            Some(request),
            None,
            &[("anthropic-version", "2023-06-01")],
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(h.provider.calls.lock().await.len(), 1);
    let path = format!("/gateway/keys/{id}?expected_version=1");
    let (s, _, _) = h
        .request(
            "DELETE",
            &path,
            Some(&h.token),
            true,
            None,
            Some(Uuid::new_v4()),
            &[],
        )
        .await;
    assert_eq!(s, 403);
    let (s, _, headers) = h
        .request(
            "DELETE",
            &path,
            Some(&h.token),
            true,
            None,
            Some(Uuid::new_v4()),
            &[("origin", origin), ("x-csrf-token", csrf)],
        )
        .await;
    assert_eq!(s, 204);
    assert_eq!(headers["x-gateway-command-replayed"], "false");
    let (s, _, _) = h
        .request("GET", "/gateway/me", Some(raw), false, None, None, &[])
        .await;
    assert_eq!(s, 401);
    h.close().await;
}
fn policy(id: Uuid, expected: &str, disabled: bool, reason: &str) -> Command {
    Command::Policy(id,serde_json::from_value(json!({"expected_version":expected,"disabled":disabled,"grants":grants(),"limits":{"budget":null,"rpm":null,"tpm":null,"count_rpm":null},"reason":reason})).unwrap())
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn service_owner_and_policy_versions_are_dispatch_dependencies_not_ledger_transfers() {
    let db = postgres::TestDb::new().await;
    let cfg = config();
    let settings = AccessSettings::from_config(&cfg);
    let g = grants();
    let ceiling = Default::default();
    let actor = finance::human(db.store.pool()).await;
    let other = bob(&db).await;
    let ledger = PgLedger::new(db.store.clone(), settings.clone(), [7; 32]);
    let c = ctx(
        actor,
        Uuid::new_v4(),
        &settings,
        &g,
        &ceiling,
        "POST",
        "/gateway/admin/principals",
    );
    let create = || {
        Command::Create(
            serde_json::from_value(json!({"display_name":"서비스","management_owner_id":actor}))
                .unwrap(),
        )
    };
    let first = ledger.management_command(&c, create()).await.unwrap();
    let ack = first.body.unwrap();
    let service = Uuid::parse_str(ack["target"]["id"].as_str().unwrap()).unwrap();
    assert_eq!(ack["operation"], "create_service");
    assert_eq!(
        ledger
            .management_command(&c, create())
            .await
            .unwrap()
            .body
            .unwrap()["replayed"],
        true
    );
    let mut input = issue();
    if let Command::Issue(v) = &mut input {
        v.service_principal_id = Some(service);
    }
    let c = ctx(
        actor,
        Uuid::new_v4(),
        &settings,
        &g,
        &ceiling,
        "POST",
        "/gateway/keys",
    );
    assert!(matches!(
        ledger.management_command(&c, input).await,
        Err(ManagementError::Forbidden)
    ));
    let c = ctx(
        actor,
        Uuid::new_v4(),
        &settings,
        &g,
        &ceiling,
        "PUT",
        "/gateway/admin/principals/id/policy",
    );
    ledger
        .management_command(&c, policy(service, "1", false, "서비스 허용"))
        .await
        .unwrap();
    let c = ctx(
        actor,
        Uuid::new_v4(),
        &settings,
        &g,
        &ceiling,
        "POST",
        "/gateway/keys",
    );
    let mut input = issue();
    if let Command::Issue(v) = &mut input {
        v.service_principal_id = Some(service);
    }
    let body = ledger
        .management_command(&c, input)
        .await
        .unwrap()
        .body
        .unwrap();
    let raw = body["key"].as_str().unwrap();
    let auth = db.store.authenticate_key(raw, &settings).await.unwrap();
    assert_eq!(auth.current.id, service);
    let spec = RequestSpec::from_profile(
        config::ProviderId::Anthropic,
        &cfg.providers()[0].profiles[0],
        Some(16),
    );
    ledger
        .admit_scoped(&auth.current, &spec, auth.key, None)
        .await
        .unwrap();
    let deps:Value=sqlx::query_scalar("SELECT execution_snapshot->'auth_dependency_versions'->'principals' FROM gw_requests WHERE id=$1").bind(spec.id).fetch_one(db.store.pool()).await.unwrap();
    assert_eq!(deps.as_array().unwrap().len(), 2);
    let c = ctx(
        actor,
        Uuid::new_v4(),
        &settings,
        &g,
        &ceiling,
        "PATCH",
        "/gateway/admin/principals/id",
    );
    ledger
        .management_command(
            &c,
            Command::Update(
                service,
                serde_json::from_value(json!({"expected_version":"1","management_owner_id":other}))
                    .unwrap(),
            ),
        )
        .await
        .unwrap();
    assert!(matches!(
        ledger.authorize_dispatch(spec.id).await,
        Err(m_gated::ledger::finance::LedgerError::PolicyChanged)
    ));
    ledger
        .finalize_request(
            spec.id,
            &FinalObservation {
                outcome: FinalOutcome::Rejected,
                usage: None,
                counted_input_tokens: None,
                reason: "policy_changed",
                http_status: None,
                provider_request_id: None,
            },
        )
        .await
        .unwrap();
    // owner의 회계 unknown/자동pause는 서비스의 별도 장부에 전파하지 않는다.
    let owner_unknown = unknown(&db, &ledger, &cfg, other).await;
    sqlx::query("UPDATE gw_principals SET inference_paused_at=clock_timestamp(),inference_pause_reason='cost_bound_violation',inference_pause_request_id=$2,inference_safety_version=inference_safety_version+1 WHERE id=$1").bind(other).bind(owner_unknown).execute(db.store.pool()).await.unwrap();
    let auth = db.store.authenticate_key(raw, &settings).await.unwrap();
    let spec = RequestSpec::from_profile(
        config::ProviderId::Anthropic,
        &cfg.providers()[0].profiles[0],
        Some(16),
    );
    ledger
        .admit_scoped(&auth.current, &spec, auth.key, None)
        .await
        .unwrap();
    let _p = ledger.authorize_dispatch(spec.id).await.unwrap();
    sqlx::query("UPDATE gw_principals SET disabled_at=clock_timestamp(),disabled_reason='test',version=version+1 WHERE id=$1").bind(other).execute(db.store.pool()).await.unwrap();
    assert!(db.store.authenticate_key(raw, &settings).await.is_err());
    let detail = db
        .store
        .management_detail(
            &qctx(actor, &settings, &g, true),
            "principals",
            service,
            &ceiling,
        )
        .await
        .unwrap();
    assert!(
        detail["principal"]["block_state"]["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("owner_inactive"))
    );
    db.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn fence_and_separate_safety_cas_preserve_current_admin_and_unknown() {
    let db = postgres::TestDb::new().await;
    let cfg = config();
    let settings = AccessSettings::from_config(&cfg);
    let g = grants();
    let ceiling = Default::default();
    let alice = finance::human(db.store.pool()).await;
    let bob = bob(&db).await;
    let ledger = PgLedger::new(db.store.clone(), settings.clone(), [7; 32]);
    let c = ctx(
        alice,
        Uuid::new_v4(),
        &settings,
        &g,
        &ceiling,
        "PUT",
        "/gateway/admin/principals/id/policy",
    );
    assert!(matches!(
        ledger
            .management_command(&c, policy(alice, "1", true, "self"))
            .await,
        Err(ManagementError::SecurityFence)
    ));
    let barrier = tokio::sync::Barrier::new(2);
    let a = async {
        barrier.wait().await;
        ledger
            .management_command(
                &ctx(
                    alice,
                    Uuid::new_v4(),
                    &settings,
                    &g,
                    &ceiling,
                    "PUT",
                    "/gateway/admin/principals/id/policy",
                ),
                policy(bob, "1", true, "security_config_fence:change-1"),
            )
            .await
    };
    let b = async {
        barrier.wait().await;
        ledger
            .management_command(
                &ctx(
                    bob,
                    Uuid::new_v4(),
                    &settings,
                    &g,
                    &ceiling,
                    "PUT",
                    "/gateway/admin/principals/id/policy",
                ),
                policy(alice, "1", true, "security_config_fence:change-1"),
            )
            .await
    };
    let (a, b) = tokio::join!(a, b);
    assert_ne!(a.is_ok(), b.is_ok());
    let (admin, target) = if a.is_ok() {
        (alice, bob)
    } else {
        (bob, alice)
    };
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM gw_principals WHERE disabled_at IS NULL"
        )
        .fetch_one(db.store.pool())
        .await
        .unwrap(),
        1
    );
    let c = ctx(
        admin,
        Uuid::new_v4(),
        &settings,
        &g,
        &ceiling,
        "PUT",
        "/gateway/admin/principals/id/policy",
    );
    assert!(matches!(
        ledger
            .management_command(&c, policy(target, "2", false, "normal release"))
            .await,
        Err(ManagementError::SecurityFence)
    ));
    let mut input = policy(target, "2", false, "새 설정에서 해제");
    if let Command::Policy(v, ref mut p) = input {
        assert_eq!(v, target);
        p.fence_release = Some(m_gated::ledger::management::FenceRelease {
            fence_ref: "change-1".into(),
            old_processes_stopped_ref: "old-executor-stopped".into(),
            confirmed: true,
        });
    }
    ledger.management_command(&c, input).await.unwrap();
    let id = unknown(&db, &ledger, &cfg, target).await;
    sqlx::query("UPDATE gw_principals SET inference_paused_at=clock_timestamp(),inference_pause_reason='cost_bound_violation',inference_pause_request_id=$2,inference_safety_version=7 WHERE id=$1").bind(target).bind(id).execute(db.store.pool()).await.unwrap();
    let release = |version: &str| {
        Command::Release(
            target,
            serde_json::from_value(
                json!({"expected_version":version,"reason":"원인 확인 후 별도 해제"}),
            )
            .unwrap(),
        )
    };
    let c = ctx(
        admin,
        Uuid::new_v4(),
        &settings,
        &g,
        &ceiling,
        "POST",
        "/gateway/admin/principals/id/inference-pause/release",
    );
    assert!(matches!(
        ledger.management_command(&c, release("3")).await,
        Err(ManagementError::Version(7))
    ));
    let first = ledger.management_command(&c, release("7")).await.unwrap();
    assert_eq!(first.body.unwrap()["result_version"], "8");
    assert_eq!(
        ledger
            .management_command(&c, release("7"))
            .await
            .unwrap()
            .body
            .unwrap()["replayed"],
        true
    );
    let c = ctx(
        admin,
        Uuid::new_v4(),
        &settings,
        &g,
        &ceiling,
        "POST",
        "/gateway/admin/principals/id/inference-pause/release",
    );
    assert!(matches!(
        ledger.management_command(&c, release("7")).await,
        Err(ManagementError::Version(8))
    ));
    assert_eq!(
        ledger
            .management_command(&c, release("8"))
            .await
            .unwrap()
            .body
            .unwrap()["effect"],
        "noop"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT accounting_state FROM gw_requests WHERE id=$1")
            .bind(id)
            .fetch_one(db.store.pool())
            .await
            .unwrap(),
        "unknown"
    );
    db.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn key_quota_validity_and_month_end_use_first_database_creation_time() {
    let db = postgres::TestDb::new().await;
    let cfg = config();
    let settings = AccessSettings::from_config(&cfg);
    let g = grants();
    let ceiling = Default::default();
    let actor = finance::human(db.store.pool()).await;
    let ledger = PgLedger::new(db.store.clone(), settings.clone(), [7; 32]);
    for preset in ["3_months", "6_months", "1_year", "3_years", "unlimited"] {
        let mut input = issue();
        if let Command::Issue(v) = &mut input {
            v.validity = serde_json::from_value(json!(preset)).unwrap();
        }
        let c = ctx(
            actor,
            Uuid::new_v4(),
            &settings,
            &g,
            &ceiling,
            "POST",
            "/gateway/keys",
        );
        let body = ledger
            .management_command(&c, input)
            .await
            .unwrap()
            .body
            .unwrap();
        let id = Uuid::parse_str(body["metadata"]["id"].as_str().unwrap()).unwrap();
        assert_eq!(body["metadata"]["validity"], preset);
        let exact:bool=sqlx::query_scalar("SELECT CASE validity_preset WHEN 'unlimited' THEN expires_at IS NULL ELSE expires_at=((created_at AT TIME ZONE 'UTC')+make_interval(months=>CASE validity_preset WHEN '3_months' THEN 3 WHEN '6_months' THEN 6 WHEN '1_year' THEN 12 ELSE 36 END)) AT TIME ZONE 'UTC' END FROM gw_virtual_keys WHERE id=$1").bind(id).fetch_one(db.store.pool()).await.unwrap();
        assert!(exact);
    }
    for _ in 5..32 {
        ledger
            .management_command(
                &ctx(
                    actor,
                    Uuid::new_v4(),
                    &settings,
                    &g,
                    &ceiling,
                    "POST",
                    "/gateway/keys",
                ),
                issue(),
            )
            .await
            .unwrap();
    }
    assert!(matches!(
        ledger
            .management_command(
                &ctx(
                    actor,
                    Uuid::new_v4(),
                    &settings,
                    &g,
                    &ceiling,
                    "POST",
                    "/gateway/keys"
                ),
                issue()
            )
            .await,
        Err(ManagementError::Quota)
    ));
    let expired = db
        .store
        .management_list(
            &qctx(actor, &settings, &g, false),
            "keys",
            Some("status=expired"),
        )
        .await
        .unwrap();
    assert_eq!(expired["items"], json!([]));
    db.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn key_dto_requires_all_limits_fields_and_rejects_client_computed_expiry() {
    let h = Harness::new().await;
    let mut results = Vec::new();
    for change in [
        "validity",
        "limits.budget",
        "limits.rpm",
        "limits.tpm",
        "limits.count_rpm",
    ] {
        let mut v = key_input();
        if let Some(field) = change.strip_prefix("limits.") {
            v["limits"].as_object_mut().unwrap().remove(field);
        } else {
            v.as_object_mut().unwrap().remove(change);
        }
        let (s, _, _) = h
            .request(
                "POST",
                "/gateway/keys",
                Some(&h.token),
                false,
                Some(v),
                Some(Uuid::new_v4()),
                &[],
            )
            .await;
        results.push(s);
    }
    for field in ["expires_at", "principal_id", "role"] {
        let mut v = key_input();
        v[field] = json!("2027-01-01T00:00:00Z");
        results.push(
            h.request(
                "POST",
                "/gateway/keys",
                Some(&h.token),
                false,
                Some(v),
                Some(Uuid::new_v4()),
                &[],
            )
            .await
            .0,
        );
    }
    let actor = h.actor;
    let other = bob(&h.db).await;
    assert_ne!(actor, other);
    h.close().await;
    assert!(
        results.iter().all(|s| *s == 400),
        "S5 DTO의 필수 필드 누락·서버 계산값 입력은400: {results:?}"
    );
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn every_admin_route_uses_current_sso_role_and_command_ack_not_resource_snapshot() {
    let h = Harness::new().await;
    let token = &h.token;
    let service_input = json!({"display_name":"API 서비스","management_owner_id":h.actor});
    let command = Uuid::new_v4();
    let (s, ack, headers) = h
        .request(
            "POST",
            "/gateway/admin/principals",
            Some(token),
            false,
            Some(service_input.clone()),
            Some(command),
            &[],
        )
        .await;
    assert_eq!(s, 201);
    assert_eq!(ack["effect"], "changed");
    assert!(ack.get("principal").is_none());
    assert_eq!(headers["x-gateway-command-replayed"], "false");
    let service = ack["target"]["id"].as_str().unwrap();
    let (s, replay, headers) = h
        .request(
            "POST",
            "/gateway/admin/principals",
            Some(token),
            false,
            Some(service_input),
            Some(command),
            &[],
        )
        .await;
    assert_eq!(s, 201);
    assert_eq!(replay["target"]["id"], service);
    assert_eq!(headers["x-gateway-command-replayed"], "true");
    let path = format!("/gateway/admin/principals/{service}");
    let (s, detail, _) = h
        .request("GET", &path, Some(token), false, None, None, &[])
        .await;
    assert_eq!(s, 200);
    assert_eq!(detail["principal"]["policy"]["grants"], json!([]));
    let (s, ack, _) = h
        .request(
            "PATCH",
            &path,
            Some(token),
            false,
            Some(json!({"expected_version":"1","display_name":"이름 변경"})),
            Some(Uuid::new_v4()),
            &[],
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(ack["result_version"], "2");
    let (s, v, _) = h
        .request(
            "PATCH",
            &path,
            Some(token),
            false,
            Some(json!({"expected_version":"1","display_name":"stale"})),
            Some(Uuid::new_v4()),
            &[],
        )
        .await;
    assert_eq!(s, 409);
    assert_eq!(v["error"]["details"]["current_version"], "2");
    let policy_path = format!("{path}/policy");
    let policy = json!({"expected_version":"1","disabled":false,"grants":grants(),"limits":{"budget":null,"rpm":null,"tpm":null,"count_rpm":null},"reason":"서비스 정책"});
    let (s, v, _) = h
        .request(
            "PUT",
            &policy_path,
            Some(token),
            false,
            Some(policy),
            Some(Uuid::new_v4()),
            &[],
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(v["result_version"], "2");
    let mut key = key_input();
    key["service_principal_id"] = json!(service);
    let (s, key, _) = h
        .request(
            "POST",
            "/gateway/keys",
            Some(token),
            false,
            Some(key),
            Some(Uuid::new_v4()),
            &[],
        )
        .await;
    assert_eq!(s, 201);
    let raw = key["key"].as_str().unwrap();
    h.provider
        .set_count(
            axum::http::StatusCode::OK,
            axum::http::HeaderMap::from_iter([(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/json"),
            )]),
            axum::body::Bytes::from_static(br#"{"input_tokens":3}"#),
        )
        .await;
    let(s,_,_)=h.request("POST","/anthropic/v1/messages/count_tokens",Some(raw),false,Some(json!({"model":"synthetic-text","messages":[{"role":"user","content":"synthetic-only"}]})),None,&[("anthropic-version","2023-06-01")]).await;
    assert_eq!(s, 200);
    let (s, list, _) = h
        .request(
            "GET",
            "/gateway/admin/requests?accounting_state=settled",
            Some(token),
            false,
            None,
            None,
            &[],
        )
        .await;
    assert_eq!(s, 200);
    let request = list["items"][0]["id"].as_str().unwrap();
    let (s, detail, _) = h
        .request(
            "GET",
            &format!("/gateway/admin/requests/{request}"),
            Some(token),
            false,
            None,
            None,
            &[],
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(detail["principal_id"], service);
    assert!(detail.get("usage").is_some());
    for route in [
        format!("/gateway/admin/usage?principal_id={service}&group_by=key"),
        format!("/gateway/admin/keys?principal_id={service}"),
        "/gateway/admin/principals?kind=service&disabled=false".into(),
    ] {
        assert_eq!(
            h.request("GET", &route, Some(token), false, None, None, &[])
                .await
                .0,
            200
        );
    }
    let (s, _, _) = h
        .request(
            "POST",
            &format!("{path}/inference-pause/release"),
            Some(token),
            false,
            Some(json!({"expected_version":"1","reason":"해제 상태 확인"})),
            Some(Uuid::new_v4()),
            &[],
        )
        .await;
    assert_eq!(s, 200);
    // 현재 설정에서 admin만 제거한 두 번째 서버는 동일 JWT로 관리자 경로를 거부한다.
    let mut env = support::environment();
    env.insert("ALLOWED_EMAILS".into(), "alice@example.test".into());
    let user_cfg = config::load(ConfigInputs {
        yaml: support::YAML,
        file_environment: None,
        environment: &env,
    })
    .unwrap();
    let fake = sso::FakeJwks::new(vec![sso::Signer::shared().jwk.clone()]);
    let server = fake.serve().await;
    let auth = Arc::new(SsoAuthenticator::new(
        &user_cfg,
        SsoVerifier::new(
            user_cfg.auth(),
            Arc::new(JwksCache::loopback_for_test(server.uri.clone()).unwrap()),
        ),
        h.db.store.clone(),
    ));
    let api = Arc::new(m_gated::admin::DashboardApi::new(
        &user_cfg,
        auth,
        h.db.store.clone(),
    ));
    let router = http::router_with_components(
        AppState {
            runtime: h.runtime.clone(),
        },
        None,
        api.routes(h.runtime.clone()),
    );
    let routes = [
        ("GET", "/gateway/admin/principals"),
        ("POST", "/gateway/admin/principals"),
        ("GET", &path),
        ("PATCH", &path),
        ("PUT", &policy_path),
        ("GET", "/gateway/admin/keys"),
        ("GET", "/gateway/admin/usage"),
        ("GET", "/gateway/admin/requests"),
    ];
    for (method, path) in routes {
        let r = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let s = r.status();
        drop(to_bytes(r.into_body(), 1024 * 1024).await.unwrap());
        assert_eq!(s, 403);
    }
    h.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn admin_usage_cursor_is_actor_and_scope_bound_and_all_filters_are_bounded() {
    let db = postgres::TestDb::new().await;
    let cfg = config();
    let settings = AccessSettings::from_config(&cfg);
    let g = grants();
    let ceiling = Default::default();
    let actor = finance::human(db.store.pool()).await;
    let other = bob(&db).await;
    let ledger = PgLedger::new(db.store.clone(), settings.clone(), [7; 32]);
    let id = unknown(&db, &ledger, &cfg, actor).await;
    let q = qctx(other, &settings, &g, true);
    let list = db
        .store
        .management_list(&q, "requests", None)
        .await
        .unwrap();
    assert_eq!(list["items"][0]["id"], id.to_string());
    assert_eq!(list["items"][0]["cost_usd"], Value::Null);
    let pending = RequestSpec::from_profile(
        config::ProviderId::Anthropic,
        &cfg.providers()[0].profiles[0],
        Some(16),
    );
    ledger
        .admit_request(&finance::current(db.store.pool(), actor).await, &pending)
        .await
        .unwrap();
    let old = finance::expired_copy(db.store.pool(), pending.id, "admitted").await;
    let list = db
        .store
        .management_list(&q, "requests", Some("accounting_state=pending"))
        .await
        .unwrap();
    assert!(
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["id"] == old.to_string())
    );
    for filter in [
        "limit=101",
        "from=2025-01-01T00%3A00%3A00Z",
        "accounting_state=bad",
        "provider=bad",
        "phase=bad",
        "outcome=bad",
        "principal_id=bad",
        "model=",
        "accounting_state=settled&from=2025-01-01T00%3A00%3A00Z&to=2025-04-03T00%3A00%3A00Z",
    ] {
        assert!(
            db.store
                .management_list(&q, "requests", Some(filter))
                .await
                .is_err()
        );
    }
    // 서로 다른 provider/model 합성 이력은 현재 실행 profile 유무와 관계없이 조회한다.
    for (p, model) in [("openai", "same-history"), ("deepseek", "same-history")] {
        let history = Uuid::new_v4();
        sqlx::query("INSERT INTO gw_requests(id,principal_id,key_id,purpose,provider,requested_model,api,admitted_at,admit_expires_at,transport_deadline,execution_deadline,executor_id,phase,accounting_state,pricing_snapshot,execution_snapshot,auth_config_digest) SELECT $1,principal_id,key_id,'root',$3,$4,'chat_completions',admitted_at,admit_expires_at,transport_deadline,execution_deadline,executor_id,'admitted','pending',pricing_snapshot,execution_snapshot,auth_config_digest FROM gw_requests WHERE id=$2").bind(history).bind(pending.id).bind(p).bind(model).execute(db.store.pool()).await.unwrap();
        sqlx::query("INSERT INTO gw_request_budgets(request_id,scope_id,policy_version,policy_snapshot,period_start,period_end,timezone_snapshot,rate_lane,rate_start,rate_end,budget_mode,reservation_usd,remaining_reservation_usd,reserved_tokens,remaining_reserved_tokens,state) SELECT $1,scope_id,policy_version,policy_snapshot,period_start,period_end,timezone_snapshot,rate_lane,rate_start,rate_end,budget_mode,reservation_usd,remaining_reservation_usd,reserved_tokens,remaining_reserved_tokens,state FROM gw_request_budgets WHERE request_id=$2").bind(history).bind(pending.id).execute(db.store.pool()).await.unwrap();
    }
    let usage = db
        .store
        .management_usage(
            &q,
            Some(&format!("principal_id={actor}&group_by=provider&limit=1")),
            &ceiling,
        )
        .await
        .unwrap();
    let cursor = usage["buckets"]["next_cursor"].as_str().unwrap();
    let second = db
        .store
        .management_usage(&q, Some(&format!("cursor={cursor}")), &ceiling)
        .await
        .unwrap();
    assert_eq!(second["from"], usage["from"]);
    assert_ne!(
        second["buckets"]["items"][0]["group"],
        usage["buckets"]["items"][0]["group"]
    );
    assert!(
        db.store
            .dashboard_usage(
                other,
                m_gated::ledger::dashboard::UsageQuery::parse(Some(&format!("cursor={cursor}")))
                    .unwrap(),
                &g,
                "same",
                b"synthetic-only-signing-key"
            )
            .await
            .is_err()
    );
    assert!(
        db.store
            .management_list(&q, "keys", Some(&format!("cursor={cursor}")))
            .await
            .is_err()
    );
    let usage = db
        .store
        .management_usage(
            &q,
            Some("provider=deepseek&model=same-history&group_by=model"),
            &ceiling,
        )
        .await
        .unwrap();
    assert_eq!(usage["buckets"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(usage["balance_context"], "aggregate_no_balance");
    db.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn keys_freeze_their_issued_policy_at_the_database_boundary() {
    let db = postgres::TestDb::new().await;
    let cfg = config();
    let settings = AccessSettings::from_config(&cfg);
    let g = grants();
    let ceiling = Default::default();
    let actor = finance::human(db.store.pool()).await;
    let ledger = PgLedger::new(db.store.clone(), settings.clone(), [7; 32]);
    let body = ledger
        .management_command(
            &ctx(
                actor,
                Uuid::new_v4(),
                &settings,
                &g,
                &ceiling,
                "POST",
                "/gateway/keys",
            ),
            issue(),
        )
        .await
        .unwrap()
        .body
        .unwrap();
    let key = Uuid::parse_str(body["metadata"]["id"].as_str().unwrap()).unwrap();
    let changed =
        sqlx::query("UPDATE gw_policies SET rpm_limit=3,version=version+1 WHERE key_id=$1")
            .bind(key)
            .execute(db.store.pool())
            .await
            .is_ok();
    db.close().await;
    assert!(
        !changed,
        "S5: 발급 후 key policy 상한은 SQL 경계에서도 불변이어야 한다"
    );
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn virtual_owner_inactive_has_specific_wire_code_and_blocks_current_balance() {
    let h = Harness::new().await;
    let owner = bob(&h.db).await;
    let settings = AccessSettings::from_config(&h.cfg);
    let g = grants();
    let ceiling = Default::default();
    let ledger = PgLedger::new(h.db.store.clone(), settings.clone(), [7; 32]);
    let c = ctx(
        h.actor,
        Uuid::new_v4(),
        &settings,
        &g,
        &ceiling,
        "POST",
        "/gateway/admin/principals",
    );
    let service = ledger
        .management_command(
            &c,
            Command::Create(
                serde_json::from_value(
                    json!({"display_name":"owner경계","management_owner_id":owner}),
                )
                .unwrap(),
            ),
        )
        .await
        .unwrap()
        .body
        .unwrap()["target"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    ledger
        .management_command(
            &ctx(
                h.actor,
                Uuid::new_v4(),
                &settings,
                &g,
                &ceiling,
                "PUT",
                "/gateway/admin/principals/id/policy",
            ),
            policy(service, "1", false, "허용"),
        )
        .await
        .unwrap();
    let mut input = issue();
    if let Command::Issue(v) = &mut input {
        v.service_principal_id = Some(service);
    }
    let key = ledger
        .management_command(
            &ctx(
                h.actor,
                Uuid::new_v4(),
                &settings,
                &g,
                &ceiling,
                "POST",
                "/gateway/keys",
            ),
            input,
        )
        .await
        .unwrap()
        .body
        .unwrap();
    let raw = key["key"].as_str().unwrap();
    sqlx::query("UPDATE gw_principals SET disabled_at=clock_timestamp(),disabled_reason='test',version=version+1 WHERE id=$1").bind(owner).execute(h.db.store.pool()).await.unwrap();
    let (status,_,headers)=h.request("POST","/anthropic/v1/messages",Some(raw),false,Some(json!({"model":"synthetic-text","max_tokens":16,"messages":[{"role":"user","content":"synthetic-only"}]})),None,&[("anthropic-version","2023-06-01")]).await;
    let usage =
        h.db.store
            .management_usage(
                &qctx(h.actor, &settings, &g, true),
                Some(&format!("principal_id={service}")),
                &ceiling,
            )
            .await
            .unwrap();
    let calls = h.provider.calls.lock().await.len();
    h.close().await;
    assert_eq!(status, 403);
    assert_eq!(calls, 0);
    assert_eq!(headers["x-gateway-error-code"], "owner_inactive");
    assert_eq!(usage["block_state"]["blocked"], true);
    assert_eq!(usage["budget_balances"][0]["blocked"], true);
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn validity_presets_clamp_calendar_month_end_in_utc() {
    use m_gated::ledger::management::Validity;
    let db = postgres::TestDb::new().await;
    let mut c = db.store.pool().acquire().await.unwrap();
    for (preset, input, expected) in [
        (
            Validity::ThreeMonths,
            "2024-01-31T23:59:59.123456Z",
            "2024-04-30T23:59:59.123456Z",
        ),
        (
            Validity::SixMonths,
            "2024-08-31T01:00:00Z",
            "2025-02-28T01:00:00.000000Z",
        ),
        (
            Validity::OneYear,
            "2024-02-29T01:00:00Z",
            "2025-02-28T01:00:00.000000Z",
        ),
        (
            Validity::ThreeYears,
            "2024-02-29T10:00:00+09:00",
            "2027-02-28T01:00:00.000000Z",
        ),
    ] {
        assert_eq!(
            preset.expires_at(&mut c, input).await.unwrap().as_deref(),
            Some(expected)
        );
    }
    assert!(
        Validity::Unlimited
            .expires_at(&mut c, "2024-01-31T00:00:00Z")
            .await
            .unwrap()
            .is_none()
    );
    drop(c);
    db.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn audit_failure_rolls_back_every_management_effect_and_financial_settlement() {
    for op in ["create", "update", "policy", "release", "settle"] {
        let db = postgres::TestDb::new().await;
        let cfg = config();
        let settings = AccessSettings::from_config(&cfg);
        let g = grants();
        let ceiling = Default::default();
        let actor = finance::human(db.store.pool()).await;
        let target = bob(&db).await;
        let ledger = PgLedger::new(db.store.clone(), settings.clone(), [7; 32]);
        let create = || {
            Command::Create(
                serde_json::from_value(
                    json!({"display_name":"원자성","management_owner_id":target}),
                )
                .unwrap(),
            )
        };
        let c = ctx(
            actor,
            Uuid::new_v4(),
            &settings,
            &g,
            &ceiling,
            "POST",
            "/gateway/admin/principals",
        );
        let service = ledger
            .management_command(&c, create())
            .await
            .unwrap()
            .body
            .unwrap()["target"]["id"]
            .as_str()
            .unwrap()
            .parse::<Uuid>()
            .unwrap();
        let id = unknown(&db, &ledger, &cfg, target).await;
        if op == "release" {
            sqlx::query("UPDATE gw_principals SET inference_paused_at=clock_timestamp(),inference_pause_reason='cost_bound_violation',inference_pause_request_id=$2 WHERE id=$1").bind(target).bind(id).execute(db.store.pool()).await.unwrap();
        }
        let before:Value=sqlx::query_scalar("SELECT jsonb_build_object('principal_count',(SELECT count(*) FROM gw_principals),'version',(SELECT version FROM gw_principals WHERE id=$1),'policy_version',(SELECT version FROM gw_policies WHERE principal_id=$2 AND key_id IS NULL),'paused',(SELECT inference_paused_at IS NOT NULL FROM gw_principals WHERE id=$2),'cost',(SELECT cost_usd::text FROM gw_requests WHERE id=$3),'state',(SELECT accounting_state FROM gw_requests WHERE id=$3),'unknown',(SELECT sum(unknown_count) FROM gw_usage_totals),'events',(SELECT count(*) FROM gw_usage_events),'commands',(SELECT count(*) FROM gw_commands),'audit',(SELECT count(*) FROM gw_audit_events))").bind(service).bind(target).bind(id).fetch_one(db.store.pool()).await.unwrap();
        sqlx::raw_sql("CREATE FUNCTION gw_test_atomic_fail() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RAISE EXCEPTION 'synthetic'; END$$;CREATE TRIGGER gw_test_atomic_fail BEFORE INSERT ON gw_audit_events FOR EACH ROW EXECUTE FUNCTION gw_test_atomic_fail();").execute(db.store.pool()).await.unwrap();
        let command = match op {
            "create" => create(),
            "update" => Command::Update(
                service,
                serde_json::from_value(json!({"expected_version":"1","management_owner_id":actor}))
                    .unwrap(),
            ),
            "policy" => policy(target, "1", true, "잠금 원자성"),
            "release" => Command::Release(
                target,
                serde_json::from_value(json!({"expected_version":"1","reason":"별도해제"}))
                    .unwrap(),
            ),
            _ => settlement(id, "3", "수동처리", true),
        };
        let result = ledger
            .management_command(
                &ctx(
                    actor,
                    Uuid::new_v4(),
                    &settings,
                    &g,
                    &ceiling,
                    "PUT",
                    "/synthetic-command-path",
                ),
                command,
            )
            .await;
        let after:Value=sqlx::query_scalar("SELECT jsonb_build_object('principal_count',(SELECT count(*) FROM gw_principals),'version',(SELECT version FROM gw_principals WHERE id=$1),'policy_version',(SELECT version FROM gw_policies WHERE principal_id=$2 AND key_id IS NULL),'paused',(SELECT inference_paused_at IS NOT NULL FROM gw_principals WHERE id=$2),'cost',(SELECT cost_usd::text FROM gw_requests WHERE id=$3),'state',(SELECT accounting_state FROM gw_requests WHERE id=$3),'unknown',(SELECT sum(unknown_count) FROM gw_usage_totals),'events',(SELECT count(*) FROM gw_usage_events),'commands',(SELECT count(*) FROM gw_commands),'audit',(SELECT count(*) FROM gw_audit_events))").bind(service).bind(target).bind(id).fetch_one(db.store.pool()).await.unwrap();
        db.close().await;
        assert!(result.is_err(), "audit injection {op}");
        assert_eq!(before, after, "atomic {op}");
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn manual_settlement_runtime_stop_known_running_and_two_admin_races() {
    for same in [false, true] {
        let db = postgres::TestDb::new().await;
        let cfg = config();
        let settings = AccessSettings::from_config(&cfg);
        let g = grants();
        let ceiling = Default::default();
        let actor = finance::human(db.store.pool()).await;
        let admin = bob(&db).await;
        let ledger = PgLedger::new(db.store.clone(), settings.clone(), [7; 32]);
        let id = unknown(&db, &ledger, &cfg, actor).await;
        let mut c = ctx(
            admin,
            Uuid::new_v4(),
            &settings,
            &g,
            &ceiling,
            "PUT",
            "/gateway/admin/requests/id/settlement",
        );
        c.execution_running = true;
        assert!(matches!(
            ledger
                .management_command(&c, settlement(id, "3", "수동", true))
                .await,
            Err(ManagementError::ExecutionNotFinal)
        ));
        ledger.record_execution_stopped(id).await.unwrap();
        let barrier = tokio::sync::Barrier::new(2);
        let a = async {
            barrier.wait().await;
            ledger
                .management_command(
                    &ctx(
                        actor,
                        Uuid::new_v4(),
                        &settings,
                        &g,
                        &ceiling,
                        "PUT",
                        "/gateway/admin/requests/id/settlement",
                    ),
                    settlement(id, "4", "경합", false),
                )
                .await
        };
        let b = async {
            barrier.wait().await;
            ledger
                .management_command(
                    &ctx(
                        admin,
                        Uuid::new_v4(),
                        &settings,
                        &g,
                        &ceiling,
                        "PUT",
                        "/gateway/admin/requests/id/settlement",
                    ),
                    settlement(id, "4", if same { "경합" } else { "상충" }, false),
                )
                .await
        };
        let (a, b) = tokio::join!(a, b);
        if same {
            let a = a.unwrap();
            let b = b.unwrap();
            assert_ne!(a.body.unwrap()["effect"], b.body.unwrap()["effect"]);
        } else {
            assert!(matches!(
                (&a, &b),
                (Ok(_), Err(ManagementError::SettlementConflict))
                    | (Err(ManagementError::SettlementConflict), Ok(_))
            ));
        }
        let detail = db
            .store
            .management_detail(&qctx(admin, &settings, &g, true), "requests", id, &ceiling)
            .await
            .unwrap();
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM gw_usage_events WHERE kind='settlement'")
                .fetch_one(db.store.pool())
                .await
                .unwrap();
        let unknown: i64 =
            sqlx::query_scalar("SELECT sum(unknown_count)::bigint FROM gw_usage_totals")
                .fetch_one(db.store.pool())
                .await
                .unwrap();
        db.close().await;
        assert_eq!(count, 1);
        assert_eq!(unknown, 0);
        assert_eq!(detail["settlement"]["execution_stop_source"], "runtime");
        assert_eq!(detail["cost_usd"], "0.123456789012");
    }
}
#[path = "support/pg_proxy.rs"]
mod pg_proxy;
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn command_commit_ack_loss_never_reissues_secret_or_settlement_cost() {
    for operation in ["issue", "settlement"] {
        let db = postgres::TestDb::new().await;
        let cfg = config();
        let settings = AccessSettings::from_config(&cfg);
        let g = grants();
        let ceiling = Default::default();
        let actor = finance::human(db.store.pool()).await;
        let ledger = PgLedger::new(db.store.clone(), settings.clone(), [7; 32]);
        let id = if operation == "settlement" {
            Some(unknown(&db, &ledger, &cfg, actor).await)
        } else {
            None
        };
        let proxy = pg_proxy::CommitProxy::new().await;
        let store = db.store_via_proxy(proxy.port).await;
        let faulty = PgLedger::new(store.clone(), settings.clone(), [7; 32]);
        let c = ctx(
            actor,
            Uuid::new_v4(),
            &settings,
            &g,
            &ceiling,
            if id.is_some() { "PUT" } else { "POST" },
            if id.is_some() {
                "/gateway/admin/requests/id/settlement"
            } else {
                "/gateway/keys"
            },
        );
        proxy.armed.store(true, std::sync::atomic::Ordering::SeqCst);
        let result = faulty
            .management_command(
                &c,
                if let Some(id) = id {
                    settlement(id, "3", "ACK 유실", true)
                } else {
                    issue()
                },
            )
            .await;
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            proxy.committed.notified(),
        )
        .await
        .unwrap();
        assert!(result.is_err());
        let replay = ledger
            .management_command(
                &c,
                if let Some(id) = id {
                    settlement(id, "3", "ACK 유실", true)
                } else {
                    issue()
                },
            )
            .await;
        if id.is_some() {
            let ack = replay.unwrap();
            assert!(ack.replayed);
            assert_eq!(ack.body.unwrap()["effect"], "changed");
            assert_eq!(
                sqlx::query_scalar::<_, String>("SELECT cost_usd::text FROM gw_usage_totals")
                    .fetch_one(db.store.pool())
                    .await
                    .unwrap(),
                "0.123456789012"
            );
        } else {
            assert!(matches!(replay, Err(ManagementError::SecretUnavailable(_))));
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM gw_virtual_keys")
                    .fetch_one(db.store.pool())
                    .await
                    .unwrap(),
                1
            );
        }
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM gw_commands")
                .fetch_one(db.store.pool())
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM gw_audit_events")
                .fetch_one(db.store.pool())
                .await
                .unwrap(),
            1
        );
        store.pool().close().await;
        drop(proxy);
        db.close().await;
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn every_cookie_write_route_and_delete_rejects_old_jwt_bound_csrf() {
    let h = Harness::new().await;
    let (_, csrf, _) = h
        .request(
            "GET",
            "/gateway/csrf",
            Some(&h.token),
            true,
            None,
            None,
            &[],
        )
        .await;
    let csrf = csrf["token"].as_str().unwrap();
    let origin = h.cfg.public_origin().as_str().trim_end_matches('/');
    let mut claims = sso::claims();
    claims["exp"] = json!(2000000001);
    let refreshed = sso::Signer::shared().sign(&claims, "key1");
    let (s, current_csrf, _) = h
        .request(
            "GET",
            "/gateway/csrf",
            Some(&refreshed),
            true,
            None,
            None,
            &[],
        )
        .await;
    assert_eq!(s, 200);
    let current_csrf = current_csrf["token"].as_str().unwrap();
    let (_, key, _) = h
        .request(
            "POST",
            "/gateway/keys",
            Some(&h.token),
            false,
            Some(key_input()),
            Some(Uuid::new_v4()),
            &[],
        )
        .await;
    let key = key["metadata"]["id"].as_str().unwrap();
    let id = h.actor;
    let writes = [
        ("POST", "/gateway/keys".to_owned()),
        ("DELETE", format!("/gateway/keys/{key}?expected_version=1")),
        ("POST", "/gateway/admin/principals".into()),
        ("PATCH", format!("/gateway/admin/principals/{id}")),
        ("PUT", format!("/gateway/admin/principals/{id}/policy")),
        (
            "POST",
            format!("/gateway/admin/principals/{id}/inference-pause/release"),
        ),
        ("PUT", format!("/gateway/admin/requests/{id}/settlement")),
    ];
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_commands")
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    let audit_before: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_audit_events")
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    let mut results = Vec::new();
    for (method, path) in writes {
        for (index, extra) in [
            vec![],
            vec![("x-csrf-token", current_csrf)],
            vec![("origin", origin)],
            vec![("origin", "null"), ("x-csrf-token", current_csrf)],
            vec![
                ("origin", "https://sibling.example.test"),
                ("x-csrf-token", current_csrf),
            ],
            vec![
                ("origin", "https://m-gated.example.test/"),
                ("x-csrf-token", current_csrf),
            ],
            vec![
                ("origin", origin),
                ("origin", origin),
                ("x-csrf-token", current_csrf),
            ],
            vec![("origin", origin), ("x-csrf-token", "forged")],
            vec![
                ("origin", origin),
                ("x-csrf-token", current_csrf),
                ("x-csrf-token", current_csrf),
            ],
            vec![("origin", origin), ("x-csrf-token", csrf)],
        ]
        .into_iter()
        .enumerate()
        {
            let response = h
                .request(
                    method,
                    &path,
                    Some(&refreshed),
                    true,
                    if method == "DELETE" {
                        None
                    } else {
                        Some(json!({}))
                    },
                    Some(Uuid::new_v4()),
                    &extra,
                )
                .await;
            let expected_code = if [0, 1, 3, 4, 5, 6].contains(&index) {
                "origin_denied"
            } else {
                "csrf_invalid"
            };
            results.push((
                response.0,
                response.1["error"]["code"].as_str().unwrap().to_owned(),
                expected_code,
            ));
        }
    }
    let audit_after: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_audit_events")
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_commands")
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    let header_csrf = h
        .request(
            "GET",
            "/gateway/csrf",
            Some(&h.token),
            false,
            None,
            None,
            &[],
        )
        .await
        .0;
    h.close().await;
    assert_eq!(before, after);
    assert!(
        results
            .iter()
            .all(|(status, code, expected)| *status == 403 && code == expected)
    );
    assert_eq!(audit_before, audit_after);
    assert_eq!(header_csrf, 403);
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn foreign_key_ownership_precedes_new_command_schema_and_replay() {
    let h = Harness::new().await;
    let other = bob(&h.db).await;
    let settings = AccessSettings::from_config(&h.cfg);
    let g = grants();
    let ceiling = Default::default();
    let ledger = PgLedger::new(h.db.store.clone(), settings.clone(), [7; 32]);
    let key = ledger
        .management_command(
            &ctx(
                other,
                Uuid::new_v4(),
                &settings,
                &g,
                &ceiling,
                "POST",
                "/gateway/keys",
            ),
            issue(),
        )
        .await
        .unwrap()
        .body
        .unwrap()["metadata"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // 이 별도 서버에서 Alice는 user다. 소유 아닌 key의 malformed query도 존재를 노출하지 않는다.
    let mut e = support::environment();
    e.insert(
        "ALLOWED_EMAILS".into(),
        "alice@example.test,bob@example.test".into(),
    );
    let cfg = config::load(ConfigInputs {
        yaml: support::YAML,
        file_environment: None,
        environment: &e,
    })
    .unwrap();
    let fake = sso::FakeJwks::new(vec![sso::Signer::shared().jwk.clone()]);
    let server = fake.serve().await;
    let auth = Arc::new(SsoAuthenticator::new(
        &cfg,
        SsoVerifier::new(
            cfg.auth(),
            Arc::new(JwksCache::loopback_for_test(server.uri.clone()).unwrap()),
        ),
        h.db.store.clone(),
    ));
    let api = Arc::new(m_gated::admin::DashboardApi::new(
        &cfg,
        auth,
        h.db.store.clone(),
    ));
    let router = http::router_with_components(
        AppState {
            runtime: h.runtime.clone(),
        },
        None,
        api.routes(h.runtime.clone()),
    );
    let response = router
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/gateway/keys/{key}?expected_version=bad"))
                .header("authorization", format!("Bearer {}", h.token))
                .header("x-gateway-command-id", Uuid::new_v4().to_string())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    drop(to_bytes(response.into_body(), 1024 * 1024).await.unwrap());
    h.close().await;
    assert_eq!(status, 404, "S5: 소유권 검사는 schema/CAS보다 먼저다");
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn quota_last_slot_and_policy_cas_races_have_one_effect() {
    let db = postgres::TestDb::new().await;
    let cfg = config();
    let settings = AccessSettings::from_config(&cfg);
    let g = grants();
    let ceiling = Default::default();
    let actor = finance::human(db.store.pool()).await;
    let admin = bob(&db).await;
    let ledger = PgLedger::new(db.store.clone(), settings.clone(), [7; 32]);
    let original = Uuid::new_v4();
    for i in 0..31 {
        ledger
            .management_command(
                &ctx(
                    actor,
                    if i == 0 { original } else { Uuid::new_v4() },
                    &settings,
                    &g,
                    &ceiling,
                    "POST",
                    "/gateway/keys",
                ),
                issue(),
            )
            .await
            .unwrap();
    }
    let barrier = tokio::sync::Barrier::new(2);
    let a = async {
        barrier.wait().await;
        ledger
            .management_command(
                &ctx(
                    actor,
                    Uuid::new_v4(),
                    &settings,
                    &g,
                    &ceiling,
                    "POST",
                    "/gateway/keys",
                ),
                issue(),
            )
            .await
    };
    let b = async {
        barrier.wait().await;
        ledger
            .management_command(
                &ctx(
                    actor,
                    Uuid::new_v4(),
                    &settings,
                    &g,
                    &ceiling,
                    "POST",
                    "/gateway/keys",
                ),
                issue(),
            )
            .await
    };
    let (a, b) = tokio::join!(a, b);
    assert!(matches!(
        (&a, &b),
        (Ok(_), Err(ManagementError::Quota)) | (Err(ManagementError::Quota), Ok(_))
    ));
    assert!(matches!(
        ledger
            .management_command(
                &ctx(
                    actor,
                    original,
                    &settings,
                    &g,
                    &ceiling,
                    "POST",
                    "/gateway/keys"
                ),
                issue()
            )
            .await,
        Err(ManagementError::SecretUnavailable(_))
    ));
    let barrier = tokio::sync::Barrier::new(2);
    let a = async {
        barrier.wait().await;
        let mut p = policy(actor, "1", false, "CAS a");
        if let Command::Policy(_, p) = &mut p {
            p.limits.rpm = Some(3);
        }
        ledger
            .management_command(
                &ctx(
                    actor,
                    Uuid::new_v4(),
                    &settings,
                    &g,
                    &ceiling,
                    "PUT",
                    "/gateway/admin/principals/id/policy",
                ),
                p,
            )
            .await
    };
    let b = async {
        barrier.wait().await;
        let mut p = policy(actor, "1", false, "CAS b");
        if let Command::Policy(_, p) = &mut p {
            p.limits.rpm = Some(4);
        }
        ledger
            .management_command(
                &ctx(
                    admin,
                    Uuid::new_v4(),
                    &settings,
                    &g,
                    &ceiling,
                    "PUT",
                    "/gateway/admin/principals/id/policy",
                ),
                p,
            )
            .await
    };
    let (a, b) = tokio::join!(a, b);
    assert!(matches!(
        (&a, &b),
        (Ok(_), Err(ManagementError::Version(2))) | (Err(ManagementError::Version(2)), Ok(_))
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM gw_virtual_keys")
            .fetch_one(db.store.pool())
            .await
            .unwrap(),
        32
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM gw_audit_events WHERE operation='replace_policy'"
        )
        .fetch_one(db.store.pool())
        .await
        .unwrap(),
        1
    );
    db.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn manual_and_late_final_races_preserve_money_and_separate_safety_cas() {
    for late_first in [false, true] {
        let db = postgres::TestDb::new().await;
        let cfg = config();
        let settings = AccessSettings::from_config(&cfg);
        let g = grants();
        let ceiling = Default::default();
        let actor = finance::human(db.store.pool()).await;
        let admin = bob(&db).await;
        let ledger = PgLedger::new(db.store.clone(), settings.clone(), [7; 32]);
        let id = unknown(&db, &ledger, &cfg, actor).await;
        let observation = FinalObservation::completed(m_gated::ledger::finance::FinalUsage {
            actual_model: "synthetic-text".into(),
            input_tokens: 5000,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: None,
        });
        if late_first {
            ledger.finalize_request(id, &observation).await.unwrap();
        }
        let c = ctx(
            admin,
            Uuid::new_v4(),
            &settings,
            &g,
            &ceiling,
            "PUT",
            "/gateway/admin/requests/id/settlement",
        );
        let manual = ledger.management_command(&c, settlement(id, "3", "수동과 late", true));
        let (_, _) = if late_first {
            (
                manual.await.unwrap(),
                ledger.finalize_request(id, &observation).await.unwrap(),
            )
        } else {
            let barrier = tokio::sync::Barrier::new(2);
            let (m, l) = tokio::join!(
                async {
                    barrier.wait().await;
                    manual.await
                },
                async {
                    barrier.wait().await;
                    ledger.finalize_request(id, &observation).await
                }
            );
            (m.unwrap(), l.unwrap())
        };
        let detail = db
            .store
            .management_detail(&qctx(admin, &settings, &g, true), "requests", id, &ceiling)
            .await
            .unwrap();
        assert_eq!(detail["cost_usd"], "0.123456789012");
        assert_eq!(detail["usage"]["late_final"]["input_tokens"], "5000");
        let p = db
            .store
            .management_detail(
                &qctx(admin, &settings, &g, true),
                "principals",
                actor,
                &ceiling,
            )
            .await
            .unwrap();
        assert_eq!(p["principal"]["inference_safety"]["paused"], true);
        assert_eq!(p["principal"]["inference_safety"]["version"], "2");
        let system: i64 =
            sqlx::query_scalar("SELECT count(*) FROM gw_audit_events WHERE actor_kind='system'")
                .fetch_one(db.store.pool())
                .await
                .unwrap();
        assert_eq!(system, 1);
        let c = ctx(
            admin,
            Uuid::new_v4(),
            &settings,
            &g,
            &ceiling,
            "POST",
            "/gateway/admin/principals/id/inference-pause/release",
        );
        assert!(matches!(
            ledger
                .management_command(
                    &c,
                    Command::Release(
                        actor,
                        serde_json::from_value(
                            json!({"expected_version":"1","reason":"오래된 화면"})
                        )
                        .unwrap()
                    )
                )
                .await,
            Err(ManagementError::Version(2))
        ));
        assert_eq!(
            ledger.finalize_request(id, &observation).await.unwrap(),
            m_gated::ledger::finance::Finalized::Noop
        );
        db.close().await;
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn known_live_native_sender_cannot_be_settled_by_admin_confirmation() {
    let h = Harness::with_clock(true).await;
    let (tx, closed) = h.provider.stream().await;
    tx.send(axum::body::Bytes::from_static(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"type\":\"message\",\"role\":\"assistant\",\"model\":\"synthetic-text\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n")).await.unwrap();
    let response=h.router.clone().oneshot(Request::builder().method("POST").uri("/anthropic/v1/messages").header("authorization",format!("Bearer {}",h.token)).header("content-type","application/json").header("anthropic-version","2023-06-01").body(Body::from(r#"{"model":"synthetic-text","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"synthetic-only"}]}"#)).unwrap()).await.unwrap();
    assert_eq!(response.status(), 200);
    let id = Uuid::parse_str(response.headers()["x-gateway-request-id"].to_str().unwrap()).unwrap();
    sqlx::query("UPDATE gw_fixture_clock SET observed=(SELECT lease_expires_at FROM gw_requests WHERE id=$1)").bind(id).execute(h.db.store.pool()).await.unwrap();
    let ledger = PgLedger::new(
        h.db.store.clone(),
        AccessSettings::from_config(&h.cfg),
        m_gated::gateway::auth_config_digest(&h.cfg),
    );
    assert_eq!(
        ledger.recover_expired(id).await.unwrap(),
        Some(m_gated::ledger::finance::Finalized::Unknown)
    );
    let path = format!("/gateway/admin/requests/{id}/settlement");
    let input = json!({"expected_version":"3","decision":"no_charge","cost_usd":"0.000000000000","reason":"관리자 종료 확인 주장","execution_stopped_confirmed":true});
    let (status, body, _) = h
        .request(
            "PUT",
            &path,
            Some(&h.token),
            false,
            Some(input),
            Some(Uuid::new_v4()),
            &[],
        )
        .await;
    assert_eq!(status, 409);
    assert_eq!(body["error"]["code"], "execution_not_final");
    drop(response);
    drop(tx);
    tokio::time::timeout(std::time::Duration::from_secs(5), closed)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(6), h.runtime.wait_requests())
        .await
        .unwrap();
    let (_, detail, _) = h
        .request(
            "GET",
            &format!("/gateway/admin/requests/{id}"),
            Some(&h.token),
            false,
            None,
            None,
            &[],
        )
        .await;
    assert!(!detail["execution_stopped_at"].is_null());
    let input = json!({"expected_version":detail["version"],"decision":"no_charge","cost_usd":"0.000000000000","reason":"runtime 종료 확인"});
    let (status, _, _) = h
        .request(
            "PUT",
            &path,
            Some(&h.token),
            false,
            Some(input),
            Some(Uuid::new_v4()),
            &[],
        )
        .await;
    h.close().await;
    assert_eq!(status, 200);
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn metadata_pages_stop_at_bytes_before_row_limit_and_keep_resource_membership() {
    let db = postgres::TestDb::new().await;
    let cfg = config();
    let settings = AccessSettings::from_config(&cfg);
    let actor = finance::human(db.store.pool()).await;
    let mut owners = vec![actor];
    for _ in 0..3 {
        owners.push(finance::human(db.store.pool()).await);
    }
    let grants=Value::Array((0..64).map(|i|json!({"provider":"anthropic","model":format!("{i:03}{}","m".repeat(197)),"capabilities":["messages","count_tokens"]})).collect());
    let mut tx = db.store.pool().begin().await.unwrap();
    for i in 0..101 {
        let id = Uuid::new_v4();
        let command = Uuid::new_v4();
        let principal = owners[i / 32];
        sqlx::query("INSERT INTO gw_commands(actor_principal_id,command_id,input_digest,operation,target_kind,target_id,result_status,result_projection) VALUES($1,$2,decode(repeat('00',32),'hex'),'issue_key','key',$3,201,'{}')").bind(actor).bind(command).bind(id).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO gw_virtual_keys(id,principal_id,secret_hash,hash_version,public_prefix,name,grant_snapshot,validity_preset,created_by,created_command_id) VALUES($1,$2,decode(repeat(replace($1::text,'-',''),2),'hex'),1,'public-id-only',$3,$4,'unlimited',$5,$6)").bind(id).bind(principal).bind("🧪".repeat(100)).bind(&grants).bind(actor).bind(command).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO gw_policies(id,principal_id,key_id,allow_rules,budget_mode,period_kind,period_timezone,updated_by) VALUES($1,$2,$3,$4,'actual','calendar_month','UTC',$5)").bind(Uuid::new_v4()).bind(principal).bind(id).bind(&grants).bind(actor).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    let q = qctx(actor, &settings, &grants, true);
    let mut query = "limit=100".to_owned();
    let mut ids = std::collections::BTreeSet::new();
    let mut pages = 0;
    loop {
        let page = db
            .store
            .management_list(&q, "keys", Some(&query))
            .await
            .unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() <= 1024 * 1024);
        let items = page["items"].as_array().unwrap();
        assert!(items.len() < 100);
        for item in items {
            assert!(ids.insert(item["id"].as_str().unwrap().to_owned()));
            assert!(item.get("secret_hash").is_none());
            assert!(item.get("key").is_none());
        }
        pages += 1;
        if let Some(cursor) = page["next_cursor"].as_str() {
            query = format!("cursor={cursor}");
        } else {
            break;
        }
        assert!(pages < 10);
    }
    db.close().await;
    assert_eq!(ids.len(), 101);
    assert!(pages > 1);
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn manual_settlement_keeps_original_period_other_unknown_and_conservative_tpm() {
    let db = postgres::TestDb::new().await;
    let store = db.clock_store(None).await;
    let cfg = config();
    let settings = AccessSettings::from_config(&cfg);
    let g = grants();
    let ceiling = Default::default();
    let actor = finance::human(store.pool()).await;
    sqlx::query("UPDATE gw_policies SET budget_mode='reserved',budget_limit_usd=gw_amount('1.000000000000'),tpm_limit=10000 WHERE principal_id=$1").bind(actor).execute(store.pool()).await.unwrap();
    let ledger = PgLedger::new(store.clone(), settings.clone(), [7; 32]);
    let mut input = issue();
    if let Command::Issue(v) = &mut input {
        v.limits=serde_json::from_value(json!({"budget":{"mode":"reserved","limit_usd":"1.000000000000","period":{"kind":"calendar_month","timezone":"UTC"}},"tpm":10000})).unwrap();
    }
    let key = ledger
        .management_command(
            &ctx(
                actor,
                Uuid::new_v4(),
                &settings,
                &g,
                &ceiling,
                "POST",
                "/gateway/keys",
            ),
            input,
        )
        .await
        .unwrap()
        .body
        .unwrap();
    let auth = store
        .authenticate_key(key["key"].as_str().unwrap(), &settings)
        .await
        .unwrap();
    let mut profile = cfg.providers()[0].profiles[0].clone();
    profile.internal_count = true;
    let mut roots = Vec::new();
    for _ in 0..2 {
        let root = RequestSpec::from_profile(config::ProviderId::Anthropic, &profile, Some(16));
        ledger
            .admit_scoped(&auth.current, &root, auth.key, None)
            .await
            .unwrap();
        let child = RequestSpec::from_profile(
            config::ProviderId::Anthropic,
            &cfg.providers()[0].profiles[1],
            None,
        );
        ledger
            .admit_scoped(&auth.current, &child, auth.key, Some(root.id))
            .await
            .unwrap();
        let _p = ledger.authorize_dispatch(child.id).await.unwrap();
        ledger
            .finalize_request(
                child.id,
                &FinalObservation {
                    outcome: FinalOutcome::Completed,
                    usage: None,
                    counted_input_tokens: Some(10),
                    reason: "completed",
                    http_status: Some(200),
                    provider_request_id: None,
                },
            )
            .await
            .unwrap();
        let _p = ledger.authorize_dispatch(root.id).await.unwrap();
        roots.push(root);
    }
    sqlx::query("UPDATE gw_fixture_clock SET observed='2025-04-01T00:00:00Z'")
        .execute(store.pool())
        .await
        .unwrap();
    for root in &roots {
        ledger
            .finalize_request(
                root.id,
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
    let original: Value =
        sqlx::query_scalar("SELECT pricing_snapshot FROM gw_requests WHERE id=$1")
            .bind(roots[0].id)
            .fetch_one(store.pool())
            .await
            .unwrap();
    ledger
        .management_command(
            &ctx(
                actor,
                Uuid::new_v4(),
                &settings,
                &g,
                &ceiling,
                "PUT",
                "/gateway/admin/requests/id/settlement",
            ),
            settlement(roots[0].id, "3", "원래 기간의 수동 입력", true),
        )
        .await
        .unwrap();
    let rows=sqlx::query("SELECT t.cost_usd::text AS cost,t.reserved_usd::text AS held,t.unknown_count,t.settled_unknown_tokens_count,t.period_start::text AS start,w.reserved_tokens,w.settled_tokens FROM gw_usage_totals t JOIN gw_rate_windows w ON w.scope_id=t.scope_id AND w.lane='inference' ORDER BY t.scope_id").fetch_all(store.pool()).await.unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert_eq!(row.get::<String, _>("cost"), "0.123456789012");
        assert_eq!(
            row.get::<String, _>("held"),
            roots[1].cost_bound.unwrap().to_string()
        );
        assert_eq!(row.get::<i64, _>("unknown_count"), 1);
        assert_eq!(row.get::<i64, _>("settled_unknown_tokens_count"), 1);
        assert!(row.get::<String, _>("start").starts_with("2025-03-01"));
        assert_eq!(row.get::<i64, _>("reserved_tokens"), 4112);
        assert_eq!(row.get::<i64, _>("settled_tokens"), 4112);
    }
    let detail = store
        .management_detail(
            &qctx(actor, &settings, &g, true),
            "requests",
            roots[0].id,
            &ceiling,
        )
        .await
        .unwrap();
    assert_eq!(detail["pricing_snapshot"], original);
    assert_eq!(detail["usage"]["final"], Value::Null);
    assert!(detail["settlement"].get("evidence_ref").is_none());
    let usage = store
        .management_usage(
            &qctx(actor, &settings, &g, true),
            Some(&format!(
                "principal_id={actor}&from=2025-03-01T00%3A00%3A00Z&to=2025-04-01T00%3A00%3A00Z"
            )),
            &ceiling,
        )
        .await
        .unwrap();
    assert_eq!(usage["global_unknown_count"], "1");
    assert_eq!(usage["budget_balances"][0]["remaining_usd"], Value::Null);
    assert_eq!(usage["budget_balances"][0]["balance_state"], "unresolved");
    store.pool().close().await;
    db.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn old_and_new_configuration_servers_keep_fence_until_explicit_stop_confirmation() {
    let h = Harness::new().await;
    let mut e = support::environment();
    e.insert(
        "ALLOWED_EMAILS".into(),
        "alice@example.test,bob@example.test".into(),
    );
    e.insert("ADMIN_EMAILS".into(), "bob@example.test".into());
    let cfg = config::load(ConfigInputs {
        yaml: support::YAML,
        file_environment: None,
        environment: &e,
    })
    .unwrap();
    let fake = sso::FakeJwks::new(vec![sso::Signer::shared().jwk.clone()]);
    let jwks = fake.serve().await;
    let auth = Arc::new(SsoAuthenticator::new(
        &cfg,
        SsoVerifier::new(
            cfg.auth(),
            Arc::new(JwksCache::loopback_for_test(jwks.uri.clone()).unwrap()),
        ),
        h.db.store.clone(),
    ));
    let api = Arc::new(m_gated::admin::DashboardApi::new(
        &cfg,
        auth,
        h.db.store.clone(),
    ));
    let runtime = Runtime::new(cfg.limits().clone()).unwrap();
    runtime.start_http().unwrap();
    for d in [
        Dependency::Database,
        Dependency::Schema,
        Dependency::Jwks,
        Dependency::Ui,
    ] {
        runtime.record_health(d, true, tokio::time::Instant::now());
    }
    let router = http::router_with_components(
        AppState {
            runtime: runtime.clone(),
        },
        None,
        api.routes(runtime.clone()),
    );
    let mut claims = sso::claims();
    claims["sub"] = json!("new-config-admin");
    claims["email"] = json!("bob@example.test");
    claims["name"] = json!("Bob");
    let token = sso::Signer::shared().sign(&claims, "key1");
    let request = |method: &str, path: String, body: Value| {
        Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .header("x-gateway-command-id", Uuid::new_v4().to_string())
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    };
    let r = router
        .clone()
        .oneshot(request("GET", "/gateway/me".into(), Value::Null))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    drop(to_bytes(r.into_body(), 1024 * 1024).await.unwrap());
    let path = format!("/gateway/admin/principals/{}/policy", h.actor);
    let mut p = json!({"expected_version":"1","disabled":true,"grants":[{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}],"limits":{"budget":null,"rpm":null,"tpm":null,"count_rpm":null},"reason":"security_config_fence:rollout-1"});
    let r = router
        .clone()
        .oneshot(request("PUT", path.clone(), p.clone()))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    drop(to_bytes(r.into_body(), 1024 * 1024).await.unwrap());
    let mut statuses = Vec::new();
    for (method, path) in [
        ("GET", "/gateway/me".into()),
        ("POST", "/gateway/keys".into()),
        ("PUT", path.clone()),
        (
            "PUT",
            format!("/gateway/admin/requests/{}/settlement", h.actor),
        ),
    ] {
        statuses.push(
            h.request(
                method,
                &path,
                Some(&h.token),
                false,
                Some(json!({})),
                Some(Uuid::new_v4()),
                &[],
            )
            .await
            .0,
        );
    }
    p["expected_version"] = json!("2");
    p["disabled"] = json!(false);
    p["reason"] = json!("일반 해제는 fence 우회 불가");
    let r = router.oneshot(request("PUT", path, p)).await.unwrap();
    let refused = r.status();
    drop(to_bytes(r.into_body(), 1024 * 1024).await.unwrap());
    runtime.stop();
    h.close().await;
    assert!(statuses.iter().all(|s| *s == 403));
    assert_eq!(refused, 409);
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn newly_issued_keys_cannot_escape_money_unknown_but_free_count_is_independent() {
    let h = Harness::new().await;
    h.provider.set(axum::http::StatusCode::OK,axum::http::HeaderMap::from_iter([(axum::http::header::CONTENT_TYPE,axum::http::HeaderValue::from_static("application/json"))]),axum::body::Bytes::from_static(br#"{"id":"msg_unknown","type":"message","role":"assistant","model":"synthetic-text","content":[],"stop_reason":"end_turn"}"#)).await;
    let input = json!({"model":"synthetic-text","max_tokens":16,"messages":[{"role":"user","content":"synthetic-only"}]});
    let (s, _, _) = h
        .request(
            "POST",
            "/anthropic/v1/messages",
            Some(&h.token),
            false,
            Some(input.clone()),
            None,
            &[("anthropic-version", "2023-06-01")],
        )
        .await;
    assert_eq!(s, 200);
    let (s, key, _) = h
        .request(
            "POST",
            "/gateway/keys",
            Some(&h.token),
            false,
            Some(key_input()),
            Some(Uuid::new_v4()),
            &[],
        )
        .await;
    assert_eq!(s, 201);
    let raw = key["key"].as_str().unwrap();
    let (s, _, headers) = h
        .request(
            "POST",
            "/anthropic/v1/messages",
            Some(raw),
            false,
            Some(input),
            None,
            &[("anthropic-version", "2023-06-01")],
        )
        .await;
    assert_eq!(s, 403);
    assert_eq!(headers["x-gateway-error-code"], "unsettled_usage_block");
    assert_eq!(h.provider.calls.lock().await.len(), 1);
    h.provider
        .set_count(
            axum::http::StatusCode::OK,
            axum::http::HeaderMap::from_iter([(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/json"),
            )]),
            axum::body::Bytes::from_static(br#"{"input_tokens":5}"#),
        )
        .await;
    let(s,_,_)=h.request("POST","/anthropic/v1/messages/count_tokens",Some(raw),false,Some(json!({"model":"synthetic-text","messages":[{"role":"user","content":"synthetic-only"}]})),None,&[("anthropic-version","2023-06-01")]).await;
    assert_eq!(s, 200);
    assert_eq!(h.provider.calls.lock().await.len(), 2);
    let (s, usage, _) = h
        .request(
            "GET",
            "/gateway/usage",
            Some(&h.token),
            false,
            None,
            None,
            &[],
        )
        .await;
    h.close().await;
    assert_eq!(s, 200);
    assert_eq!(usage["global_unknown_count"], "1");
    assert_eq!(usage["budget_balances"][0]["balance_state"], "unresolved");
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn issued_grants_do_not_expand_with_principal_policy_and_expired_key_is_401() {
    let h = Harness::new().await;
    sqlx::query("UPDATE gw_policies SET allow_rules=$2 WHERE principal_id=$1 AND key_id IS NULL")
        .bind(h.actor)
        .bind(
            json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}]),
        )
        .execute(h.db.store.pool())
        .await
        .unwrap();
    let mut input = key_input();
    input["grants"] =
        json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}]);
    let (s, key, _) = h
        .request(
            "POST",
            "/gateway/keys",
            Some(&h.token),
            false,
            Some(input),
            Some(Uuid::new_v4()),
            &[],
        )
        .await;
    assert_eq!(s, 201);
    let raw = key["key"].as_str().unwrap();
    let(s,_,_)=h.request("PUT",&format!("/gateway/admin/principals/{}/policy",h.actor),Some(&h.token),false,Some(json!({"expected_version":"1","disabled":false,"grants":grants(),"limits":{"budget":null,"rpm":null,"tpm":null,"count_rpm":null},"reason":"확대해도 기존키상한은고정"})),Some(Uuid::new_v4()),&[]).await;
    assert_eq!(s, 200);
    let(s,_,_)=h.request("POST","/anthropic/v1/messages/count_tokens",Some(raw),false,Some(json!({"model":"synthetic-text","messages":[{"role":"user","content":"synthetic-only"}]})),None,&[("anthropic-version","2023-06-01")]).await;
    assert_eq!(s, 403);
    assert_eq!(h.provider.calls.lock().await.len(), 0);
    let expired = m_gated::auth::key::VirtualSecret::generate().unwrap();
    let command = Uuid::new_v4();
    let mut tx = h.db.store.pool().begin().await.unwrap();
    sqlx::query("INSERT INTO gw_commands(actor_principal_id,command_id,input_digest,operation,target_kind,target_id,result_status,result_projection) VALUES($1,$2,decode(repeat('00',32),'hex'),'issue_key','key',$3,201,'{}')").bind(h.actor).bind(command).bind(expired.id).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO gw_virtual_keys(id,principal_id,secret_hash,hash_version,public_prefix,name,grant_snapshot,validity_preset,expires_at,created_at,created_by,created_command_id) VALUES($1,$2,$3,1,'expired-id','expired',$4,'3_months',clock_timestamp()-interval '1 month',clock_timestamp()-interval '4 months',$2,$5)").bind(expired.id).bind(h.actor).bind(expired.hash().as_slice()).bind(grants()).bind(command).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO gw_policies(id,principal_id,key_id,allow_rules,budget_mode,period_kind,period_timezone,updated_by) VALUES($1,$2,$3,$4,'actual','calendar_month','UTC',$2)").bind(Uuid::new_v4()).bind(h.actor).bind(expired.id).bind(grants()).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let raw = expired.expose_once();
    let (s, _, _) = h
        .request("GET", "/gateway/me", Some(&raw), false, None, None, &[])
        .await;
    let (_, list, _) = h
        .request(
            "GET",
            "/gateway/keys?status=expired",
            Some(&h.token),
            false,
            None,
            None,
            &[],
        )
        .await;
    h.close().await;
    assert_eq!(s, 401);
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert_eq!(list["items"][0]["id"], expired.id.to_string());
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn requests_and_principals_pages_enforce_100_and_resource_cursor_expiry() {
    let db = postgres::TestDb::new().await;
    let store = db.clock_store(None).await;
    let cfg = config();
    let settings = AccessSettings::from_config(&cfg);
    let g = grants();
    let actor = finance::human(store.pool()).await;
    let ledger = PgLedger::new(store.clone(), settings.clone(), [7; 32]);
    let current = finance::current(store.pool(), actor).await;
    let spec = RequestSpec::from_profile(
        config::ProviderId::Anthropic,
        &cfg.providers()[0].profiles[0],
        Some(16),
    );
    ledger.admit_request(&current, &spec).await.unwrap();
    let mut tx = store.pool().begin().await.unwrap();
    for _ in 0..100 {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO gw_requests(id,principal_id,purpose,provider,requested_model,api,admitted_at,admit_expires_at,transport_deadline,execution_deadline,executor_id,phase,accounting_state,pricing_snapshot,execution_snapshot,auth_config_digest) SELECT $1,principal_id,purpose,provider,requested_model,api,admitted_at,admit_expires_at,transport_deadline,execution_deadline,executor_id,phase,accounting_state,pricing_snapshot,execution_snapshot,auth_config_digest FROM gw_requests WHERE id=$2").bind(id).bind(spec.id).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO gw_request_budgets(request_id,scope_id,policy_version,policy_snapshot,period_start,period_end,timezone_snapshot,rate_lane,rate_start,rate_end,budget_mode,reservation_usd,remaining_reservation_usd,reserved_tokens,remaining_reserved_tokens,state) SELECT $1,scope_id,policy_version,policy_snapshot,period_start,period_end,timezone_snapshot,rate_lane,rate_start,rate_end,budget_mode,reservation_usd,remaining_reservation_usd,reserved_tokens,remaining_reserved_tokens,state FROM gw_request_budgets WHERE request_id=$2").bind(id).bind(spec.id).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    let q = qctx(actor, &settings, &g, true);
    let page = store
        .management_list(&q, "requests", Some("accounting_state=pending&limit=100"))
        .await
        .unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 100);
    let cursor = page["next_cursor"].as_str().unwrap();
    let next = store
        .management_list(&q, "requests", Some(&format!("cursor={cursor}")))
        .await
        .unwrap();
    assert_eq!(next["items"].as_array().unwrap().len(), 1);
    for resource in ["principals", "keys"] {
        assert!(
            store
                .management_list(&q, resource, Some(&format!("cursor={cursor}")))
                .await
                .is_err()
        );
    }
    assert!(
        store
            .management_list(
                &q,
                "requests",
                Some(&format!("cursor={cursor}&accounting_state=unknown"))
            )
            .await
            .is_err()
    );
    sqlx::query("UPDATE gw_fixture_clock SET observed=observed+interval '901 seconds'")
        .execute(store.pool())
        .await
        .unwrap();
    assert!(matches!(
        store
            .management_list(&q, "requests", Some(&format!("cursor={cursor}")))
            .await,
        Err(ManagementError::Query(
            m_gated::ledger::dashboard::QueryError::InvalidCursor
        ))
    ));
    store.pool().close().await;
    // 실제 현재 시각의 principal 리소스도 같은100+1 keyset 규칙을 따른다.
    for _ in 0..100 {
        finance::human(db.store.pool()).await;
    }
    let page = db
        .store
        .management_list(&q, "principals", Some("limit=100"))
        .await
        .unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 100);
    let cursor = page["next_cursor"].as_str().unwrap();
    let page = db
        .store
        .management_list(&q, "principals", Some(&format!("cursor={cursor}")))
        .await
        .unwrap();
    db.close().await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn new_late_cost_cause_and_safety_release_serialize_without_unpausing_new_cause() {
    let db = postgres::TestDb::new().await;
    let cfg = config();
    let settings = AccessSettings::from_config(&cfg);
    let g = grants();
    let ceiling = Default::default();
    let actor = finance::human(db.store.pool()).await;
    let admin = bob(&db).await;
    let ledger = PgLedger::new(db.store.clone(), settings.clone(), [7; 32]);
    let id = unknown(&db, &ledger, &cfg, actor).await;
    let observation = FinalObservation::completed(m_gated::ledger::finance::FinalUsage {
        actual_model: "synthetic-text".into(),
        input_tokens: 5000,
        output_tokens: 20,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: None,
    });
    let barrier = tokio::sync::Barrier::new(2);
    let (late, release) = tokio::join!(
        async {
            barrier.wait().await;
            ledger.finalize_request(id, &observation).await
        },
        async {
            barrier.wait().await;
            ledger
                .management_command(
                    &ctx(
                        admin,
                        Uuid::new_v4(),
                        &settings,
                        &g,
                        &ceiling,
                        "POST",
                        "/gateway/admin/principals/id/inference-pause/release",
                    ),
                    Command::Release(
                        actor,
                        serde_json::from_value(
                            json!({"expected_version":"1","reason":"경합하는 이전 safety 화면"}),
                        )
                        .unwrap(),
                    ),
                )
                .await
        }
    );
    assert!(late.is_ok());
    assert!(matches!(release, Ok(_) | Err(ManagementError::Version(2))));
    let p = db
        .store
        .management_detail(
            &qctx(admin, &settings, &g, true),
            "principals",
            actor,
            &ceiling,
        )
        .await
        .unwrap();
    let detail = db
        .store
        .management_detail(&qctx(admin, &settings, &g, true), "requests", id, &ceiling)
        .await
        .unwrap();
    db.close().await;
    assert_eq!(p["principal"]["inference_safety"]["paused"], true);
    assert_eq!(p["principal"]["inference_safety"]["version"], "2");
    assert_eq!(detail["accounting_state"], "unknown");
    assert_eq!(detail["cost_usd"], Value::Null);
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn policy_command_preserves_scope_period_and_audit_failed_revoke_rolls_back() {
    let db = postgres::TestDb::new().await;
    let cfg = config();
    let settings = AccessSettings::from_config(&cfg);
    let g = grants();
    let ceiling = Default::default();
    let actor = finance::human(db.store.pool()).await;
    let ledger = PgLedger::new(db.store.clone(), settings.clone(), [7; 32]);
    let id = unknown(&db, &ledger, &cfg, actor).await;
    let scope = finance::current(db.store.pool(), actor).await.policy_id;
    for (mode, tz) in [("reserved", "UTC"), ("actual", "Asia/Seoul")] {
        let mut p = policy(actor, "1", false, "이전 scope 유지");
        if let Command::Policy(_, p) = &mut p {
            p.limits.budget=Some(serde_json::from_value(json!({"mode":mode,"limit_usd":"1.000000000000","period":{"kind":"calendar_month","timezone":tz}})).unwrap());
        }
        assert!(matches!(
            ledger
                .management_command(
                    &ctx(
                        actor,
                        Uuid::new_v4(),
                        &settings,
                        &g,
                        &ceiling,
                        "PUT",
                        "/gateway/admin/principals/id/policy"
                    ),
                    p
                )
                .await,
            Err(ManagementError::PolicyPeriod)
        ));
    }
    assert_eq!(
        finance::current(db.store.pool(), actor).await.policy_id,
        scope
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT accounting_state FROM gw_requests WHERE id=$1")
            .bind(id)
            .fetch_one(db.store.pool())
            .await
            .unwrap(),
        "unknown"
    );
    let key = ledger
        .management_command(
            &ctx(
                actor,
                Uuid::new_v4(),
                &settings,
                &g,
                &ceiling,
                "POST",
                "/gateway/keys",
            ),
            issue(),
        )
        .await
        .unwrap()
        .body
        .unwrap()["metadata"]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    sqlx::raw_sql("CREATE FUNCTION gw_test_revoke_fail() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RAISE EXCEPTION 'synthetic'; END$$;CREATE TRIGGER gw_test_revoke_fail BEFORE INSERT ON gw_audit_events FOR EACH ROW EXECUTE FUNCTION gw_test_revoke_fail();").execute(db.store.pool()).await.unwrap();
    let c = ctx(
        actor,
        Uuid::new_v4(),
        &settings,
        &g,
        &ceiling,
        "DELETE",
        "/gateway/keys/id",
    );
    assert!(
        ledger
            .management_command(
                &c,
                Command::Revoke {
                    id: key,
                    expected: "1".into()
                }
            )
            .await
            .is_err()
    );
    let unchanged: bool = sqlx::query_scalar(
        "SELECT revoked_at IS NULL AND version=1 FROM gw_virtual_keys WHERE id=$1",
    )
    .bind(key)
    .fetch_one(db.store.pool())
    .await
    .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_commands WHERE command_id=$1")
        .bind(c.command_id)
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    db.close().await;
    assert!(unchanged);
    assert_eq!(count, 0);
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn command_lock_wait_is_bounded_without_pending_command_or_effect() {
    let db = postgres::TestDb::new().await;
    let cfg = config();
    let settings = AccessSettings::from_config(&cfg);
    let g = grants();
    let ceiling = Default::default();
    let actor = finance::human(db.store.pool()).await;
    let ledger = PgLedger::new(db.store.clone(), settings.clone(), [7; 32]);
    let mut holder = db.store.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM gw_principals WHERE id=$1 FOR UPDATE")
        .bind(actor)
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    let c = ctx(
        actor,
        Uuid::new_v4(),
        &settings,
        &g,
        &ceiling,
        "POST",
        "/gateway/keys",
    );
    let result = ledger.management_command(&c, issue()).await;
    holder.rollback().await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_commands")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    db.close().await;
    assert!(matches!(result, Err(ManagementError::Busy)));
    assert_eq!(count, 0);
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn same_email_new_sso_subject_never_inherits_issued_keys_or_usage_scope() {
    let h = Harness::new().await;
    let (_, key, _) = h
        .request(
            "POST",
            "/gateway/keys",
            Some(&h.token),
            false,
            Some(key_input()),
            Some(Uuid::new_v4()),
            &[],
        )
        .await;
    let raw = key["key"].as_str().unwrap();
    let mut claims = sso::claims();
    claims["sub"] = json!("different-subject-same-email");
    let token = sso::Signer::shared().sign(&claims, "key1");
    let (s, me, _) = h
        .request("GET", "/gateway/me", Some(&token), false, None, None, &[])
        .await;
    assert_eq!(s, 200);
    assert_eq!(me["role"], "admin");
    let new = me["principal"]["id"].as_str().unwrap();
    assert_ne!(new, h.actor.to_string());
    let (s, keys, _) = h
        .request("GET", "/gateway/keys", Some(&token), false, None, None, &[])
        .await;
    assert_eq!(s, 200);
    assert_eq!(keys["items"], json!([]));
    let original =
        h.db.store
            .authenticate_key(raw, &AccessSettings::from_config(&h.cfg))
            .await
            .unwrap();
    assert_eq!(original.current.id, h.actor);
    let(s,_,_)=h.request("POST","/anthropic/v1/messages",Some(&token),false,Some(json!({"model":"synthetic-text","max_tokens":16,"messages":[{"role":"user","content":"synthetic-only"}]})),None,&[("anthropic-version","2023-06-01")]).await;
    let calls = h.provider.calls.lock().await.len();
    h.close().await;
    assert_eq!(s, 403);
    assert_eq!(calls, 0);
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn admin_aggregate_key_filter_checks_existence_without_computing_balance() {
    let h = Harness::new().await;
    let (status, issued, _) = h
        .request(
            "POST",
            "/gateway/keys",
            Some(&h.token),
            false,
            Some(key_input()),
            Some(Uuid::new_v4()),
            &[],
        )
        .await;
    assert_eq!(status, 201);
    let key = issued["metadata"]["id"].as_str().unwrap();
    let (status, aggregate, _) = h
        .request(
            "GET",
            &format!("/gateway/admin/usage?key_id={key}"),
            Some(&h.token),
            false,
            None,
            None,
            &[],
        )
        .await;
    // 원문 인증/응답을 assertion 진단에 포함하지 않는다.
    assert_eq!(status, 200);
    assert_eq!(aggregate["balance_context"], "aggregate_no_balance");
    assert_eq!(aggregate["budget_balances"], json!([]));
    assert!(aggregate["block_state"].is_null());
    let (status, usage, _) = h
        .request(
            "GET",
            &format!("/gateway/admin/usage?principal_id={}&key_id={key}", h.actor),
            Some(&h.token),
            false,
            None,
            None,
            &[],
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(usage["budget_balances"].as_array().unwrap().len(), 2);
    let other = bob(&h.db).await;
    for path in [
        format!("/gateway/admin/usage?key_id={}", Uuid::new_v4()),
        format!("/gateway/admin/usage?principal_id={other}&key_id={key}"),
    ] {
        assert_eq!(
            h.request("GET", &path, Some(&h.token), false, None, None, &[])
                .await
                .0,
            404
        );
    }
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn admin_usage_missing_principal_is_404_not_self_access_denied() {
    let h = Harness::new().await;
    let missing = format!("/gateway/admin/usage?principal_id={}", Uuid::new_v4());
    let missing_status = h
        .request("GET", &missing, Some(&h.token), false, None, None, &[])
        .await
        .0;
    assert_eq!(
        h.request(
            "GET",
            &format!("/gateway/admin/usage?principal_id={}", h.actor),
            Some(&h.token),
            false,
            None,
            None,
            &[]
        )
        .await
        .0,
        200
    );
    assert_eq!(
        h.request(
            "GET",
            &missing,
            Some("invalid-synthetic-token"),
            false,
            None,
            None,
            &[]
        )
        .await
        .0,
        401
    );
    let mut claims = sso::claims();
    claims["sub"] = json!("ordinary-user");
    claims["email"] = json!("user@example.invalid");
    // 허용된 일반 사용자는 ADMIN_EMAILS에 없고 관리자 대상 존재성을 볼 수 없다.
    let token = sso::Signer::shared().sign(&claims, "key1");
    // 현재 fixture 설정의 일반 허용 이메일을 DB에 주입하는 대신 별도 검증 설정으로 조립한다.
    let mut env = support::environment();
    env.insert(
        "ALLOWED_EMAILS".into(),
        "alice@example.test,user@example.invalid".into(),
    );
    env.insert("ADMIN_EMAILS".into(), "alice@example.test".into());
    let cfg = config::load(ConfigInputs {
        yaml: support::YAML,
        file_environment: None,
        environment: &env,
    })
    .unwrap();
    let auth = Arc::new(SsoAuthenticator::new(
        &cfg,
        SsoVerifier::new(
            cfg.auth(),
            Arc::new(JwksCache::loopback_for_test(h._jwks.uri.clone()).unwrap()),
        ),
        h.db.store.clone(),
    ));
    let api = Arc::new(m_gated::admin::DashboardApi::new(
        &cfg,
        auth,
        h.db.store.clone(),
    ));
    let router = http::router_with_components(
        AppState {
            runtime: h.runtime.clone(),
        },
        None,
        api.routes(h.runtime.clone()),
    );
    let r = router
        .oneshot(
            Request::builder()
                .uri(&missing)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    drop(to_bytes(r.into_body(), 1024 * 1024).await.unwrap());
    h.close().await;
    assert_eq!(missing_status, 404);
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn resource_cursor_compares_typed_normalized_filters() {
    let db = postgres::TestDb::new().await;
    let store = db.clock_store(None).await;
    let cfg = config();
    let settings = AccessSettings::from_config(&cfg);
    let g = grants();
    let actor = finance::human(store.pool()).await;
    let ledger = PgLedger::new(store.clone(), settings.clone(), [7; 32]);
    let current = finance::current(store.pool(), actor).await;
    for _ in 0..3 {
        let spec = RequestSpec::from_profile(
            config::ProviderId::Anthropic,
            &cfg.providers()[0].profiles[0],
            Some(16),
        );
        ledger.admit_request(&current, &spec).await.unwrap();
    }
    let q = qctx(actor, &settings, &g, true);
    let filter = format!(
        "accounting_state=pending&principal_id={}&limit=01&from=2025-04-01T08%3A58%3A00%2B09%3A00&to=2025-04-01T09%3A00%3A00%2B09%3A00",
        actor.hyphenated().to_string().to_uppercase()
    );
    let first = store
        .management_list(&q, "requests", Some(&filter))
        .await
        .unwrap();
    assert_eq!(first["items"].as_array().unwrap().len(), 1);
    let cursor = first["next_cursor"].as_str().unwrap();
    for filter in [
        filter,
        format!(
            "accounting_state=pending&principal_id={}&limit=1&from=2025-03-31T23%3A58%3A00.000000000Z&to=2025-04-01T00%3A00%3A00Z",
            actor.simple()
        ),
    ] {
        let page = store
            .management_list(&q, "requests", Some(&format!("cursor={cursor}&{filter}")))
            .await;
        assert!(page.is_ok(), "동치 정규 필터의 다음 페이지는 허용해야 함");
        let page = page.unwrap();
        assert_eq!(page["as_of"], first["as_of"]);
        assert_ne!(page["items"][0]["id"], first["items"][0]["id"]);
    }
    let next = store
        .management_list(&q, "requests", Some(&format!("cursor={cursor}")))
        .await
        .unwrap();
    assert_eq!(next["as_of"], first["as_of"]);
    for filter in [
        "from=2025-03-31T23%3A58%3A01Z&to=2025-04-01T00%3A00%3A00Z",
        "limit=2",
    ] {
        assert!(matches!(
            store
                .management_list(&q, "requests", Some(&format!("cursor={cursor}&{filter}")))
                .await,
            Err(ManagementError::Query(
                m_gated::ledger::dashboard::QueryError::InvalidCursor
            ))
        ));
    }
    assert!(
        store
            .management_list(&q, "keys", Some(&format!("cursor={cursor}")))
            .await
            .is_err()
    );
    let other = finance::human(store.pool()).await;
    assert!(
        store
            .management_list(
                &qctx(other, &settings, &g, true),
                "requests",
                Some(&format!("cursor={cursor}"))
            )
            .await
            .is_err()
    );
    store.pool().close().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn management_configuration_conflict_is_409_without_retry_or_mutation() {
    let mut env = support::environment();
    env.insert(
        "ALLOWED_EMAILS".into(),
        "alice@example.test,bob@example.test".into(),
    );
    env.insert(
        "ADMIN_EMAILS".into(),
        "alice@example.test,bob@example.test".into(),
    );
    let yaml = format!(
        "{}execution_ceiling: {{budget: {{mode: actual, limit_usd: '1.000000000000', period: {{kind: calendar_month, timezone: UTC}}}}}}\n",
        support::YAML
    );
    let cfg = config::load(ConfigInputs {
        yaml: &yaml,
        file_environment: None,
        environment: &env,
    })
    .unwrap();
    let h = Harness::with_config(false, cfg).await;
    let before: Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(p) ORDER BY id) FROM gw_policies p")
            .fetch_one(h.db.store.pool())
            .await
            .unwrap();
    for (mode, timezone) in [("reserved", "UTC"), ("actual", "Asia/Seoul")] {
        let limits = json!({"budget":{"mode":mode,"limit_usd":"2.000000000000","period":{"kind":"calendar_month","timezone":timezone}},"rpm":null,"tpm":null,"count_rpm":null});
        let mut input = key_input();
        input["limits"] = limits.clone();
        input["grants"] =
            json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}]);
        let policy = json!({"expected_version":"1","disabled":false,"grants":[{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}],"limits":limits,"reason":"비호환 상한"});
        for (method, path, input) in [
            ("POST", "/gateway/keys".to_owned(), input),
            (
                "PUT",
                format!("/gateway/admin/principals/{}/policy", h.actor),
                policy,
            ),
        ] {
            let (status, body, headers) = h
                .request(
                    method,
                    &path,
                    Some(&h.token),
                    false,
                    Some(input),
                    Some(Uuid::new_v4()),
                    &[],
                )
                .await;
            assert_eq!(status, 409);
            assert_eq!(body["error"]["code"], "configuration_conflict");
            assert!(!headers.contains_key("retry-after"));
        }
    }
    let after: Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(p) ORDER BY id) FROM gw_policies p")
            .fetch_one(h.db.store.pool())
            .await
            .unwrap();
    assert_eq!(before, after);
    let n:i64=sqlx::query_scalar("SELECT (SELECT count(*) FROM gw_commands)+(SELECT count(*) FROM gw_audit_events)+(SELECT count(*) FROM gw_virtual_keys)").fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(n, 0);
    // 유효 인증 cache를 유지한 채 실제 pool을 닫으면 DB 의존 실패는 계속503이다.
    h.db.store.pool().close().await;
    let (status, _, headers) = h
        .request(
            "POST",
            "/gateway/keys",
            Some(&h.token),
            false,
            Some(key_input()),
            Some(Uuid::new_v4()),
            &[],
        )
        .await;
    assert_eq!(status, 503);
    assert!(headers.contains_key("retry-after"));
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn late_token_only_bound_violation_pauses_unknown_and_manual_without_repricing() {
    for manual in [false, true] {
        let db = postgres::TestDb::new().await;
        let cfg = config();
        let settings = AccessSettings::from_config(&cfg);
        let g = grants();
        let ceiling = Default::default();
        let actor = finance::human(db.store.pool()).await;
        let admin = bob(&db).await;
        let ledger = PgLedger::new(db.store.clone(), settings.clone(), [7; 32]);
        let id = unknown(&db, &ledger, &cfg, actor).await;
        if manual {
            ledger
                .management_command(
                    &ctx(
                        admin,
                        Uuid::new_v4(),
                        &settings,
                        &g,
                        &ceiling,
                        "PUT",
                        "/gateway/admin/requests/id/settlement",
                    ),
                    settlement(id, "3", "관리자 금액 보존", true),
                )
                .await
                .unwrap();
        }
        let before:Value=sqlx::query_scalar("SELECT jsonb_build_object('cost',cost_usd::text,'state',accounting_state,'version',version) FROM gw_requests WHERE id=$1").bind(id).fetch_one(db.store.pool()).await.unwrap();
        let totals: Value = sqlx::query_scalar(
            "SELECT jsonb_agg(to_jsonb(t) ORDER BY scope_id,period_start) FROM gw_usage_totals t",
        )
        .fetch_one(db.store.pool())
        .await
        .unwrap();
        let observation = FinalObservation::completed(m_gated::ledger::finance::FinalUsage {
            actual_model: "synthetic-text".into(),
            input_tokens: 10,
            output_tokens: 17,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: None,
        });
        ledger.finalize_request(id, &observation).await.unwrap();
        let row = sqlx::query(
            "SELECT inference_safety_version,inference_pause_reason FROM gw_principals WHERE id=$1",
        )
        .bind(actor)
        .fetch_one(db.store.pool())
        .await
        .unwrap();
        assert_eq!(row.get::<i64, _>("inference_safety_version"), 2);
        assert_eq!(
            row.get::<Option<String>, _>("inference_pause_reason")
                .as_deref(),
            Some("cost_bound_violation")
        );
        assert_eq!(
            ledger.finalize_request(id, &observation).await.unwrap(),
            m_gated::ledger::finance::Finalized::Noop
        );
        let after:Value=sqlx::query_scalar("SELECT jsonb_build_object('cost',cost_usd::text,'state',accounting_state,'version',version) FROM gw_requests WHERE id=$1").bind(id).fetch_one(db.store.pool()).await.unwrap();
        assert_eq!(before, after);
        let after: Value = sqlx::query_scalar(
            "SELECT jsonb_agg(to_jsonb(t) ORDER BY scope_id,period_start) FROM gw_usage_totals t",
        )
        .fetch_one(db.store.pool())
        .await
        .unwrap();
        assert_eq!(totals, after);
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM gw_audit_events WHERE actor_kind='system' AND request_id=$1",
        )
        .bind(id)
        .fetch_one(db.store.pool())
        .await
        .unwrap();
        assert_eq!(n, 1);
        db.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn settlement_root_uses_shared_ordinary_six_gate_and_preserves_finalization_two() {
    let db = postgres::TestDb::new().await;
    let cfg = config();
    let settings = AccessSettings::from_config(&cfg);
    let g = grants();
    let actor = finance::human(db.store.pool()).await;
    let ledger = PgLedger::new(db.store.clone(), settings.clone(), [7; 32]);
    let spec = RequestSpec::from_profile(
        config::ProviderId::Anthropic,
        &cfg.providers()[0].profiles[0],
        Some(16),
    );
    ledger
        .admit_request(&finance::current(db.store.pool(), actor).await, &spec)
        .await
        .unwrap();
    let mut child = RequestSpec::from_profile(
        config::ProviderId::Anthropic,
        &cfg.providers()[0].profiles[1],
        None,
    );
    child.deadline = spec.deadline;
    ledger
        .admit_scoped(
            &finance::current(db.store.pool(), actor).await,
            &child,
            None,
            Some(spec.id),
        )
        .await
        .unwrap();
    assert_eq!(
        db.store.settlement_root(child.id).await.unwrap(),
        Some(spec.id)
    );
    ledger.authorize_dispatch(spec.id).await.unwrap();
    let q = qctx(actor, &settings, &g, true);
    let barrier = tokio::sync::Barrier::new(7);
    let release = tokio::sync::Notify::new();
    let holders = (0..6).map(|_| async {
        let operation = db.store.management_list(&q, "requests", None);
        tokio::pin!(operation);
        // 첫 poll은 동일 ordinary permit을 취득하고 실제 PG transaction future로 진입한다.
        assert!(futures_util::poll!(&mut operation).is_pending());
        barrier.wait().await;
        release.notified().await;
        // 이 scope 종료 시 pinned 참조가 아니라 원래 future와 ordinary permit을 함께 해제한다.
    });
    let inspect = async {
        barrier.wait().await;
        let start = tokio::time::Instant::now();
        let root = db.store.settlement_root(spec.id).await;
        let elapsed = start.elapsed();
        let seventh = db.store.management_list(&q, "requests", None).await;
        let first = db.store.pool().acquire().await.unwrap();
        let second = db.store.pool().acquire().await.unwrap();
        drop((first, second));
        let final_result = ledger
            .finalize_request(
                spec.id,
                &FinalObservation::completed(m_gated::ledger::finance::FinalUsage {
                    actual_model: "synthetic-text".into(),
                    input_tokens: 10,
                    output_tokens: 2,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    reasoning_tokens: None,
                }),
            )
            .await;
        release.notify_waiters();
        (root, elapsed, seventh, final_result)
    };
    let (_, (root, elapsed, seventh, final_result)) =
        tokio::join!(futures_util::future::join_all(holders), inspect);
    let normal = db.store.settlement_root(spec.id).await.unwrap();
    db.close().await;
    assert!(
        matches!(root, Err(ManagementError::Database)),
        "포화 root lookup은 DB 진입 전에 거부해야 함"
    );
    assert!(elapsed < std::time::Duration::from_millis(100));
    assert!(matches!(seventh, Err(ManagementError::Database)));
    assert_eq!(normal, None);
    assert_eq!(
        final_result.unwrap(),
        m_gated::ledger::finance::Finalized::Settled
    );
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn late_token_only_violation_audit_failure_rolls_back_unknown_and_manual_observation() {
    for manual in [false, true] {
        let db = postgres::TestDb::new().await;
        let cfg = config();
        let settings = AccessSettings::from_config(&cfg);
        let g = grants();
        let ceiling = Default::default();
        let actor = finance::human(db.store.pool()).await;
        let admin = bob(&db).await;
        let ledger = PgLedger::new(db.store.clone(), settings.clone(), [7; 32]);
        let id = unknown(&db, &ledger, &cfg, actor).await;
        if manual {
            ledger
                .management_command(
                    &ctx(
                        admin,
                        Uuid::new_v4(),
                        &settings,
                        &g,
                        &ceiling,
                        "PUT",
                        "/gateway/admin/requests/id/settlement",
                    ),
                    settlement(id, "3", "late 감사 rollback", true),
                )
                .await
                .unwrap();
        }
        let money:Value=sqlx::query_scalar("SELECT jsonb_build_object('request',(SELECT to_jsonb(r) FROM gw_requests r WHERE id=$1),'budgets',(SELECT jsonb_agg(to_jsonb(b) ORDER BY scope_id) FROM gw_request_budgets b WHERE request_id=$1),'totals',(SELECT jsonb_agg(to_jsonb(t) ORDER BY scope_id,period_start) FROM gw_usage_totals t),'rates',(SELECT jsonb_agg(to_jsonb(w) ORDER BY scope_id,lane,window_start) FROM gw_rate_windows w))").bind(id).fetch_one(db.store.pool()).await.unwrap();
        sqlx::raw_sql("CREATE FUNCTION gw_fail_late_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic_audit_failure'; END $$; CREATE TRIGGER gw_fail_late_audit BEFORE INSERT ON gw_audit_events FOR EACH ROW WHEN (NEW.actor_kind='system') EXECUTE FUNCTION gw_fail_late_audit()").execute(db.store.pool()).await.unwrap();
        let observation = FinalObservation::completed(m_gated::ledger::finance::FinalUsage {
            actual_model: "synthetic-text".into(),
            input_tokens: 10,
            output_tokens: 17,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: None,
        });
        assert!(ledger.finalize_request(id, &observation).await.is_err());
        let after:Value=sqlx::query_scalar("SELECT jsonb_build_object('request',(SELECT to_jsonb(r) FROM gw_requests r WHERE id=$1),'budgets',(SELECT jsonb_agg(to_jsonb(b) ORDER BY scope_id) FROM gw_request_budgets b WHERE request_id=$1),'totals',(SELECT jsonb_agg(to_jsonb(t) ORDER BY scope_id,period_start) FROM gw_usage_totals t),'rates',(SELECT jsonb_agg(to_jsonb(w) ORDER BY scope_id,lane,window_start) FROM gw_rate_windows w))").bind(id).fetch_one(db.store.pool()).await.unwrap();
        assert_eq!(money, after);
        let state:(i64,bool,i64)=sqlx::query_as("SELECT inference_safety_version,inference_paused_at IS NOT NULL,(SELECT count(*) FROM gw_usage_events WHERE request_id=$2 AND kind='late_final') FROM gw_principals WHERE id=$1").bind(actor).bind(id).fetch_one(db.store.pool()).await.unwrap();
        assert_eq!(state, (1, false, 0));
        sqlx::query("DROP TRIGGER gw_fail_late_audit ON gw_audit_events")
            .execute(db.store.pool())
            .await
            .unwrap();
        ledger.finalize_request(id, &observation).await.unwrap();
        assert_eq!(
            ledger.finalize_request(id, &observation).await.unwrap(),
            m_gated::ledger::finance::Finalized::Noop
        );
        let state:(i64,bool,i64)=sqlx::query_as("SELECT inference_safety_version,inference_paused_at IS NOT NULL,(SELECT count(*) FROM gw_usage_events WHERE request_id=$2 AND kind='late_final') FROM gw_principals WHERE id=$1").bind(actor).bind(id).fetch_one(db.store.pool()).await.unwrap();
        db.close().await;
        assert_eq!(state, (2, true, 1));
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn late_token_only_violation_and_release_serialize_for_unknown_and_manual() {
    for manual in [false, true] {
        let db = postgres::TestDb::new().await;
        let cfg = config();
        let settings = AccessSettings::from_config(&cfg);
        let g = grants();
        let ceiling = Default::default();
        let actor = finance::human(db.store.pool()).await;
        let admin = bob(&db).await;
        let ledger = PgLedger::new(db.store.clone(), settings.clone(), [7; 32]);
        let id = unknown(&db, &ledger, &cfg, actor).await;
        if manual {
            ledger
                .management_command(
                    &ctx(
                        admin,
                        Uuid::new_v4(),
                        &settings,
                        &g,
                        &ceiling,
                        "PUT",
                        "/gateway/admin/requests/id/settlement",
                    ),
                    settlement(id, "3", "release 경합", true),
                )
                .await
                .unwrap();
        }
        let observation = FinalObservation::completed(m_gated::ledger::finance::FinalUsage {
            actual_model: "synthetic-text".into(),
            input_tokens: 10,
            output_tokens: 17,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: None,
        });
        let barrier = tokio::sync::Barrier::new(2);
        let (late, release) = tokio::join!(
            async {
                barrier.wait().await;
                ledger.finalize_request(id, &observation).await
            },
            async {
                barrier.wait().await;
                ledger
                    .management_command(
                        &ctx(
                            admin,
                            Uuid::new_v4(),
                            &settings,
                            &g,
                            &ceiling,
                            "POST",
                            "/gateway/admin/principals/id/inference-pause/release",
                        ),
                        Command::Release(
                            actor,
                            serde_json::from_value(
                                json!({"expected_version":"1","reason":"late 경합"}),
                            )
                            .unwrap(),
                        ),
                    )
                    .await
            }
        );
        assert!(late.is_ok());
        assert!(matches!(release, Ok(_) | Err(ManagementError::Version(2))));
        let state:(i64,bool,bool,i64)=sqlx::query_as("SELECT p.inference_safety_version,p.inference_paused_at IS NOT NULL,r.safety_violation_recorded,(SELECT count(*) FROM gw_audit_events WHERE actor_kind='system' AND request_id=$2) FROM gw_principals p JOIN gw_requests r ON r.principal_id=p.id WHERE p.id=$1 AND r.id=$2").bind(actor).bind(id).fetch_one(db.store.pool()).await.unwrap();
        let money: Option<String> =
            sqlx::query_scalar("SELECT cost_usd::text FROM gw_requests WHERE id=$1")
                .bind(id)
                .fetch_one(db.store.pool())
                .await
                .unwrap();
        db.close().await;
        assert_eq!(state, (2, true, true, 1));
        assert_eq!(money, manual.then(|| "0.123456789012".to_owned()));
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn late_input_and_cache_token_only_bounds_use_original_snapshot_with_unresolved_cache_price()
{
    for (input, read, write, overflow) in [
        (4097, 0, 0, false),
        (4095, 2, 0, false),
        (4095, 0, 2, false),
        (i64::MAX as u64, 1, 0, true),
    ] {
        let db = postgres::TestDb::new().await;
        let cfg = config();
        let settings = AccessSettings::from_config(&cfg);
        let actor = finance::human(db.store.pool()).await;
        let ledger = PgLedger::new(db.store.clone(), settings, [7; 32]);
        let id = unknown(&db, &ledger, &cfg, actor).await;
        let usage = m_gated::ledger::finance::FinalUsage {
            actual_model: "synthetic-text".into(),
            input_tokens: input,
            output_tokens: 0,
            cache_read_tokens: read,
            cache_write_tokens: write,
            reasoning_tokens: None,
        };
        ledger
            .finalize_request(id, &FinalObservation::completed(usage))
            .await
            .unwrap();
        let state:(i64,Option<String>,bool,Option<String>)=sqlx::query_as("SELECT p.inference_safety_version,p.inference_pause_reason,r.safety_violation_recorded,r.cost_usd::text FROM gw_principals p JOIN gw_requests r ON p.id=r.principal_id WHERE r.id=$1").bind(id).fetch_one(db.store.pool()).await.unwrap();
        db.close().await;
        assert_eq!(
            state,
            (
                2,
                Some(
                    if overflow {
                        "amount_overflow"
                    } else {
                        "cost_bound_violation"
                    }
                    .into()
                ),
                true,
                None
            )
        );
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn cookie_csrf_actual_expiry_and_explicit_headers_never_fall_back_on_writes() {
    let h = Harness::new().await;
    let cookie = format!("mgated_access={}", h.token);
    let origin = h.cfg.public_origin().as_str().trim_end_matches('/');
    let mut claims = sso::claims();
    claims["exp"] = json!(time::OffsetDateTime::now_utc().unix_timestamp() - 1);
    let expired_in_leeway = sso::Signer::shared().sign(&claims, "key1");
    let (status, value, _) = h
        .request(
            "GET",
            "/gateway/csrf",
            Some(&expired_in_leeway),
            true,
            None,
            None,
            &[],
        )
        .await;
    assert_eq!(status, 401);
    assert_eq!(value["error"]["code"], "invalid_credentials");
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_commands")
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    for headers in [
        vec![("authorization", "")],
        vec![("authorization", "Bearer bad")],
        vec![("x-api-key", "bad")],
        vec![("authorization", "Bearer bad"), ("x-api-key", &h.token)],
    ] {
        let (status, _, _) = h
            .request(
                "POST",
                "/gateway/keys",
                Some(&h.token),
                true,
                Some(key_input()),
                Some(Uuid::new_v4()),
                &headers,
            )
            .await;
        assert_eq!(status, 401, "무효 명시 헤더는 유효 cookie로 쓰지 않는다");
    }
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_commands")
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    assert_eq!(before, after);
    let (status, issued, _) = h
        .request(
            "POST",
            "/gateway/keys",
            Some(&h.token),
            false,
            Some(key_input()),
            Some(Uuid::new_v4()),
            &[
                ("cookie", "mgated_access=invalid"),
                ("origin", "null"),
                ("x-api-key", &h.token),
            ],
        )
        .await;
    assert_eq!(
        status, 201,
        "같은 두 유효 명시 헤더는 쿠키/Origin/CSRF에 의존하지 않는다"
    );
    let raw = issued["key"].as_str().unwrap();
    let key = issued["metadata"]["id"].as_str().unwrap();
    let (status, _, _) = h
        .request(
            "POST",
            "/gateway/keys",
            Some(raw),
            false,
            Some(key_input()),
            Some(Uuid::new_v4()),
            &[("cookie", &cookie)],
        )
        .await;
    assert_eq!(status, 403, "virtual 관리 거부를 cookie로 우회하지 않는다");
    let (status, csrf, headers) = h
        .request(
            "GET",
            "/gateway/csrf",
            Some(&h.token),
            true,
            None,
            None,
            &[],
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(headers["cache-control"], "no-store");
    let csrf = csrf["token"].as_str().unwrap();
    let (status, _, _) = h
        .request(
            "DELETE",
            &format!("/gateway/keys/{key}?expected_version=1"),
            Some(&h.token),
            true,
            None,
            Some(Uuid::new_v4()),
            &[("origin", origin), ("x-csrf-token", csrf)],
        )
        .await;
    assert_eq!(status, 204);
    let revoked: bool =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM gw_virtual_keys WHERE id=$1")
            .bind(Uuid::parse_str(key).unwrap())
            .fetch_one(h.db.store.pool())
            .await
            .unwrap();
    assert!(revoked);
    let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_audit_events")
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    let commands: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_commands")
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    assert_eq!((commands, audits), (2, 2));
    assert_eq!(h.provider.calls.lock().await.len(), 0);
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn every_body_cookie_write_rejects_form_even_with_valid_origin_and_csrf() {
    let h = Harness::new().await;
    let (_, csrf, _) = h
        .request(
            "GET",
            "/gateway/csrf",
            Some(&h.token),
            true,
            None,
            None,
            &[],
        )
        .await;
    let origin = h.cfg.public_origin().as_str().trim_end_matches('/');
    for (method, path) in [
        ("POST", "/gateway/keys".to_owned()),
        ("POST", "/gateway/admin/principals".to_owned()),
        ("PATCH", format!("/gateway/admin/principals/{}", h.actor)),
        (
            "PUT",
            format!("/gateway/admin/principals/{}/policy", h.actor),
        ),
        (
            "POST",
            format!(
                "/gateway/admin/principals/{}/inference-pause/release",
                h.actor
            ),
        ),
        (
            "PUT",
            format!("/gateway/admin/requests/{}/settlement", h.actor),
        ),
    ] {
        let response = h
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("cookie", format!("mgated_access={}", h.token))
                    .header("origin", origin)
                    .header("x-csrf-token", csrf["token"].as_str().unwrap())
                    .header("x-gateway-command-id", Uuid::new_v4().to_string())
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("name=synthetic"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 415);
        assert_eq!(
            response.headers()["x-gateway-error-code"],
            "unsupported_media_type"
        );
        let _ = to_bytes(response.into_body(), 4096).await.unwrap();
    }
    let counts: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM gw_virtual_keys),(SELECT count(*) FROM gw_commands),(SELECT count(*) FROM gw_audit_events)")
        .fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(counts, (0, 0, 0));
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s9_command_busy_has_503_retry_after_and_no_mutation_or_success_audit() {
    let h = Harness::new().await;
    let (status, ack, _) = h
        .request(
            "POST",
            "/gateway/admin/principals",
            Some(&h.token),
            false,
            Some(json!({"display_name":"S9 busy service","management_owner_id":h.actor})),
            Some(Uuid::new_v4()),
            &[],
        )
        .await;
    assert_eq!(status, 201);
    let id = Uuid::parse_str(ack["target"]["id"].as_str().unwrap()).unwrap();
    let command = Uuid::new_v4();
    let mut holder = h.db.store.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM gw_principals WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    let path = format!("/gateway/admin/principals/{id}");
    let (status, body, headers) = h
        .request(
            "PATCH",
            &path,
            Some(&h.token),
            false,
            Some(json!({"expected_version":"1","display_name":"must not apply"})),
            Some(command),
            &[],
        )
        .await;
    holder.rollback().await.unwrap();
    assert_eq!(status, 503);
    assert_eq!(body["error"]["code"], "command_busy");
    assert_eq!(headers["x-gateway-error-code"], "command_busy");
    assert_eq!(headers["x-gateway-dispatch-state"], "not_dispatched");
    assert_eq!(headers["retry-after"], "1");
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(
        body["error"]["request_id"],
        headers["x-gateway-request-id"].to_str().unwrap()
    );
    assert_eq!(body["error"]["details"], json!({}));
    let row:(String,i64,i64,i64)=sqlx::query_as("SELECT display_name,version,(SELECT count(*) FROM gw_commands WHERE command_id=$2),(SELECT count(*) FROM gw_audit_events WHERE command_id=$2) FROM gw_principals WHERE id=$1").bind(id).bind(command).fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(row, ("S9 busy service".into(), 1, 0, 0));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM gw_requests")
            .fetch_one(h.db.store.pool())
            .await
            .unwrap(),
        0
    );
    assert!(h.provider.calls.lock().await.is_empty());
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s9_caught_request_body_panic_is_internal500_not_upstream_timeout() {
    struct SyntheticRequestPanic;
    struct PanickingBody;
    impl http_body::Body for PanickingBody {
        type Data = axum::body::Bytes;
        type Error = std::convert::Infallible;
        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
            // 비밀 없는 단위 payload만 사용하며 실제 panic 문자열은 wire/DB에 넣지 않는다.
            std::panic::panic_any(SyntheticRequestPanic);
        }
    }
    let h = Harness::new().await;
    let request = Request::builder()
        .method("POST")
        .uri("/anthropic/v1/messages")
        .header("authorization", format!("Bearer {}", h.token))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(Body::new(PanickingBody))
        .unwrap();
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        h.router.clone().oneshot(request),
    )
    .await
    .expect("panic H0 응답 유한마감")
    .unwrap();
    let status = r.status();
    let headers = r.headers().clone();
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        to_bytes(r.into_body(), 4096),
    )
    .await
    .expect("panic body 유한마감")
    .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    drop(bytes);
    tokio::time::timeout(std::time::Duration::from_secs(6), h.runtime.wait_requests())
        .await
        .expect("panic 자원 유한마감");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_requests")
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    let sends = h.provider.calls.lock().await.len();
    let available = h.runtime.available_inference();
    let retained = h.runtime.request_resources().body_bytes;
    h.close().await;
    assert_eq!(count, 0);
    assert_eq!(sends, 0);
    assert_eq!(available, 16);
    assert_eq!(retained, 0);
    assert_eq!(status, 500);
    assert_eq!(headers["x-gateway-error-code"], "internal_error");
    assert_eq!(headers["x-gateway-dispatch-state"], "not_dispatched");
    assert!(!headers.contains_key("retry-after"));
    assert_eq!(v["error"]["type"], "api_error");
    assert_eq!(
        v["error"]["message"],
        "Gateway request could not be completed"
    );
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn management_body_limit_applies_to_content_length_and_chunked_wire_at_boundary() {
    let mut env = support::environment();
    env.insert("ALLOWED_EMAILS".into(), "alice@example.test".into());
    env.insert("ADMIN_EMAILS".into(), "alice@example.test".into());
    let cfg = config::load(ConfigInputs {
        yaml: &format!(
            "{}\nlimits:\n  management_body_bytes: 1024\n",
            support::YAML
        ),
        file_environment: None,
        environment: &env,
    })
    .unwrap();
    let h = Harness::with_config(false, cfg).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(m_gated::server::serve(
        listener,
        h.router.clone(),
        h.runtime.clone(),
        std::future::pending(),
    ));
    let client = reqwest::Client::builder()
        .no_proxy()
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    for chunked in [false, true] {
        for size in [1025, 1024] {
            let command = Uuid::new_v4();
            let before: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM gw_principals),(SELECT count(*) FROM gw_commands),(SELECT count(*) FROM gw_audit_events)").fetch_one(h.db.store.pool()).await.unwrap();
            let mut bytes = serde_json::to_vec(
                &json!({"display_name":"합성 서비스","management_owner_id":h.actor}),
            )
            .unwrap();
            bytes.resize(size, b' ');
            let body = if chunked {
                reqwest::Body::wrap_stream(futures_util::stream::iter(vec![
                    Ok::<_, std::io::Error>(bytes[..512].to_vec()),
                    Ok(bytes[512..].to_vec()),
                ]))
            } else {
                reqwest::Body::from(bytes)
            };
            let response = client
                .post(format!("http://{addr}/gateway/admin/principals"))
                .bearer_auth(&h.token)
                .header("content-type", "application/json")
                .header("x-gateway-command-id", command.to_string())
                .body(body)
                .send()
                .await
                .unwrap();
            let status = response.status().as_u16();
            let code = response
                .headers()
                .get("x-gateway-error-code")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let _ = response.bytes().await.unwrap();
            let after: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM gw_principals),(SELECT count(*) FROM gw_commands),(SELECT count(*) FROM gw_audit_events)").fetch_one(h.db.store.pool()).await.unwrap();
            assert_eq!(
                status,
                if size == 1024 { 201 } else { 413 },
                "chunked={chunked} size={size}"
            );
            if size > 1024 {
                assert_eq!(code.as_deref(), Some("body_too_large"));
                assert_eq!(
                    before, after,
                    "초과 입력은 업무/명령/감사를 변경하지 않는다"
                );
            } else {
                assert_eq!(after, (before.0 + 1, before.1 + 1, before.2 + 1));
            }
        }
    }
    server.abort();
    let _ = server.await;
    h.close().await;
}
