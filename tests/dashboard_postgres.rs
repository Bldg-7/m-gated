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
        dashboard::UsageQuery,
        finance::{FinalObservation, FinalOutcome, RequestSpec},
    },
    runtime::{Dependency, Runtime},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;
fn config() -> config::ValidatedConfig {
    let mut env = support::environment();
    env.insert("ALLOWED_EMAILS".into(), "alice@example.test".into());
    config::load(ConfigInputs {
        yaml: support::YAML,
        file_environment: None,
        environment: &env,
    })
    .unwrap()
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn current_scope_balances_are_not_filtered_usage() {
    let db = postgres::TestDb::new().await;
    let id = finance::human(db.store.pool()).await;
    let cfg = config();
    let grants =
        json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}]);
    let ledger = PgLedger::new(db.store.clone(), AccessSettings::from_config(&cfg), [7; 32]);
    let current = finance::current(db.store.pool(), id).await;
    let spec = RequestSpec::from_profile(
        config::ProviderId::Anthropic,
        &cfg.providers()[0].profiles[0],
        Some(16),
    );
    let request = ledger.admit_request(&current, &spec).await.unwrap();
    let _send = ledger.authorize_dispatch(request).await.unwrap();
    ledger
        .finalize_request(
            request,
            &FinalObservation {
                outcome: FinalOutcome::Completed,
                usage: None,
                provider_request_id: None,
                http_status: Some(200),
                reason: "usage_unresolved",
                counted_input_tokens: None,
            },
        )
        .await
        .unwrap();
    let q=UsageQuery::parse(Some("from=2025-01-01T00%3A00%3A00Z&to=2025-02-01T00%3A00%3A00Z&provider=deepseek&model=past-inactive")).unwrap();
    let usage = db
        .store
        .dashboard_usage(id, q, &grants, "abcdef", b"synthetic-key")
        .await
        .unwrap();
    assert_eq!(usage["settled_usd"], "0.000000000000");
    assert_eq!(usage["period_unknown_count"], "0");
    assert_eq!(usage["global_unknown_count"], "1");
    assert_eq!(usage["budget_balances"][0]["remaining_usd"], Value::Null);
    assert_eq!(usage["budget_balances"][0]["balance_state"], "unresolved");
    assert!(
        usage["budget_balances"][0]["period_start"]
            .as_str()
            .unwrap()
            .starts_with("2026-")
    );
    assert_eq!(usage["block_state"]["blocked"], true);
    db.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn cookie_login_me_usage_and_current_server_access() {
    let db = postgres::TestDb::new().await;
    let cfg = config();
    let fake = sso::FakeJwks::new(vec![sso::Signer::shared().jwk.clone()]);
    let server = fake.serve().await;
    let verifier = SsoVerifier::new(
        cfg.auth(),
        Arc::new(JwksCache::loopback_for_test(server.uri.clone()).unwrap()),
    );
    let auth = Arc::new(SsoAuthenticator::new(&cfg, verifier, db.store.clone()));
    let api = Arc::new(m_gated::admin::DashboardApi::new(
        &cfg,
        auth.clone(),
        db.store.clone(),
    ));
    let runtime = Runtime::new(cfg.limits().clone()).unwrap();
    runtime.start_http().unwrap();
    for dependency in [
        Dependency::Database,
        Dependency::Jwks,
        Dependency::Schema,
        Dependency::Ui,
    ] {
        runtime.record_health(dependency, true, tokio::time::Instant::now());
    }
    let router = http::router_with_components(
        AppState {
            runtime: runtime.clone(),
        },
        None,
        api.routes(runtime.clone()),
    );
    let token = sso::Signer::shared().sign(&sso::claims(), "key1");
    let cookie = format!("mgated_access={token}");
    let request = |path: &str, cookie: Option<&str>, auth: Option<&str>| {
        let mut r = Request::builder().uri(path);
        if let Some(v) = cookie {
            r = r.header("cookie", v);
        }
        if let Some(v) = auth {
            r = r.header("authorization", v);
        }
        r.body(Body::empty()).unwrap()
    };
    let r = router
        .clone()
        .oneshot(request(
            "/auth/login?return_to=https://evil.invalid",
            None,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 302);
    assert_eq!(
        r.headers()["location"],
        "https://auth.example.test/sso/login"
    );
    let r = router
        .clone()
        .oneshot(request("/auth/login", Some(&cookie), None))
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    assert_eq!(r.headers()["location"], "/ui/");
    let r = router
        .clone()
        .oneshot(request("/gateway/me", Some(&cookie), None))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["cache-control"], "no-store");
    let me: Value =
        serde_json::from_slice(&to_bytes(r.into_body(), 1024 * 1024).await.unwrap()).unwrap();
    assert_eq!(me["role"], "user");
    let keys = router
        .clone()
        .oneshot(request("/gateway/keys", Some(&cookie), None))
        .await
        .unwrap();
    assert_eq!(keys.status(), 200, "S5: 본인 키 목록은 실제 인증된 경로다");
    let keys: Value =
        serde_json::from_slice(&to_bytes(keys.into_body(), 1024 * 1024).await.unwrap()).unwrap();
    assert_eq!(keys["items"], json!([]));
    assert_eq!(keys["next_cursor"], Value::Null);
    let id = uuid::Uuid::parse_str(me["principal"]["id"].as_str().unwrap()).unwrap();
    assert!(me["principal"].get("effective_grants").is_none());
    for path in ["/gateway/me", "/gateway/usage", "/auth/login"] {
        let r = router
            .clone()
            .oneshot(request(path, Some(&cookie), Some("Basic invalid")))
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
    }
    let r = router
        .clone()
        .oneshot(request(
            "/gateway/usage?group_by=provider",
            Some(&cookie),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let usage: Value =
        serde_json::from_slice(&to_bytes(r.into_body(), 1024 * 1024).await.unwrap()).unwrap();
    assert_eq!(usage["buckets"]["items"], json!([]));
    for query in [
        "principal_id=00000000-0000-0000-0000-000000000000",
        "provider=bad",
        "limit=101",
        "group_by=provider&limit=101",
        "model=x&model=y",
    ] {
        let r = router
            .clone()
            .oneshot(request(
                &format!("/gateway/usage?{query}"),
                Some(&cookie),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{query}");
    }
    let other = uuid::Uuid::new_v4();
    let r = router
        .clone()
        .oneshot(request(
            &format!("/gateway/usage?key_id={other}"),
            Some(&cookie),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    sqlx::query("UPDATE gw_principals SET disabled_at=clock_timestamp(),disabled_reason='test_stop' WHERE id=$1").bind(id).execute(db.store.pool()).await.unwrap();
    for path in ["/gateway/me", "/gateway/usage", "/auth/login"] {
        let r = router
            .clone()
            .oneshot(request(path, Some(&cookie), None))
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
    }
    runtime.stop();
    db.close().await;
}

async fn settled_fixture(db: &postgres::TestDb) -> (uuid::Uuid, uuid::Uuid, Arc<PgLedger>) {
    let id = finance::human(db.store.pool()).await;
    let cfg = config();
    let ledger = Arc::new(PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&cfg),
        [7; 32],
    ));
    let human = finance::current(db.store.pool(), id).await;
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
            &FinalObservation::completed(m_gated::ledger::finance::FinalUsage {
                actual_model: "synthetic-text".into(),
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: None,
            }),
        )
        .await
        .unwrap();
    (id, spec.id, ledger)
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn bucket_cursor_uses_typed_group_keys_and_freezes_filters() {
    let db = postgres::TestDb::new().await;
    let (id, source, _) = settled_fixture(&db).await;
    let grants = json!([]);
    // 승인된 과거 alias의 합성 SQL 행들이다. 현행 profile 필터로 이력을 지우지 않는다.
    for index in 0..102 {
        let clone = uuid::Uuid::new_v4();
        let model = format!("historic-{index:03}");
        sqlx::query("INSERT INTO gw_requests SELECT (jsonb_populate_record(NULL::gw_requests,to_jsonb(r)||jsonb_build_object('id',$2::uuid,'requested_model',$3::text,'pricing_snapshot',r.pricing_snapshot||jsonb_build_object('requested_model',$3::text)))).* FROM gw_requests r WHERE id=$1").bind(source).bind(clone).bind(model).execute(db.store.pool()).await.unwrap();
        sqlx::query("INSERT INTO gw_request_budgets SELECT (jsonb_populate_record(NULL::gw_request_budgets,to_jsonb(b)||jsonb_build_object('request_id',$2::uuid))).* FROM gw_request_budgets b WHERE request_id=$1").bind(source).bind(clone).execute(db.store.pool()).await.unwrap();
    }
    sqlx::query("UPDATE gw_usage_totals SET cost_usd=(SELECT sum(b.applied_cost_usd) FROM gw_request_budgets b WHERE b.scope_id=gw_usage_totals.scope_id AND b.period_start=gw_usage_totals.period_start),settled_count=103,input_tokens=103,output_tokens=103 WHERE scope_id=(SELECT id FROM gw_policies WHERE principal_id=$1 AND key_id IS NULL)").bind(id).execute(db.store.pool()).await.unwrap();
    let first = db
        .store
        .dashboard_usage(
            id,
            UsageQuery::parse(Some("group_by=model&limit=100")).unwrap(),
            &grants,
            "digest",
            b"cursor-key",
        )
        .await
        .unwrap();
    assert_eq!(first["buckets"]["items"].as_array().unwrap().len(), 100);
    assert_eq!(
        first["buckets"]["items"][0]["group"],
        json!({"kind":"model","provider":"anthropic","model":"historic-000"})
    );
    let cursor = first["buckets"]["next_cursor"].as_str().unwrap();
    let encoded = url::form_urlencoded::byte_serialize(cursor.as_bytes()).collect::<String>();
    let next = db
        .store
        .dashboard_usage(
            id,
            UsageQuery::parse(Some(&format!("cursor={encoded}"))).unwrap(),
            &grants,
            "digest",
            b"cursor-key",
        )
        .await
        .unwrap();
    assert_eq!(next["buckets"]["items"].as_array().unwrap().len(), 3);
    assert_eq!(next["buckets"]["next_cursor"], Value::Null);
    assert_eq!(next["from"], first["from"]);
    assert_eq!(next["buckets"]["as_of"], first["buckets"]["as_of"]);
    assert_eq!(next["settled_usd"], first["settled_usd"]);
    let from = first["from"].as_str().unwrap();
    let to = first["to"].as_str().unwrap();
    let offset = time::UtcOffset::from_hms(9, 0, 0).unwrap();
    let offset_stamp = |s: &str| {
        time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
            .unwrap()
            .to_offset(offset)
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    };
    let fractional_from = format!(
        "{}.000000000Z",
        from.split('.').next().unwrap().trim_end_matches('Z')
    );
    let mut equivalent_pages = Vec::new();
    for (name, from, to) in [
        ("response_echo", from.to_owned(), to.to_owned()),
        ("zero_fraction", fractional_from, to.to_owned()),
        ("same_utc_offset", offset_stamp(from), offset_stamp(to)),
    ] {
        let q = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("cursor", cursor)
            .append_pair("from", &from)
            .append_pair("to", &to)
            .finish();
        let page = db
            .store
            .dashboard_usage(
                id,
                UsageQuery::parse(Some(&q)).unwrap(),
                &grants,
                "digest",
                b"cursor-key",
            )
            .await;
        equivalent_pages.push((name, page));
    }
    let changed_from =
        (time::OffsetDateTime::parse(from, &time::format_description::well_known::Rfc3339)
            .unwrap()
            + time::Duration::seconds(1))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let changed_period = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("from", &changed_from)
        .append_pair("to", to)
        .finish();
    for suffix in [
        "group_by=provider",
        "model=different",
        "limit=99",
        &changed_period,
    ] {
        assert!(matches!(
            db.store
                .dashboard_usage(
                    id,
                    UsageQuery::parse(Some(&format!("cursor={encoded}&{suffix}"))).unwrap(),
                    &grants,
                    "digest",
                    b"cursor-key"
                )
                .await,
            Err(m_gated::ledger::dashboard::QueryError::InvalidCursor)
        ));
    }
    let key = db
        .store
        .dashboard_usage(
            id,
            UsageQuery::parse(Some("group_by=key")).unwrap(),
            &grants,
            "digest",
            b"cursor-key",
        )
        .await
        .unwrap();
    assert_eq!(
        key["buckets"]["items"][0]["group"],
        json!({"kind":"key","key_id":null})
    );
    db.close().await;
    // 양성 세 반례를 모두 실행하고 자기 schema를 정리한 뒤 실패를 보고한다.
    let failures: Vec<_> = equivalent_pages
        .iter()
        .filter_map(|(name, result)| result.as_ref().err().map(|e| (*name, e)))
        .collect();
    assert!(
        failures.is_empty(),
        "동일 시각의 필터를 수용해야 한다: {failures:?}"
    );
    for (_, page) in equivalent_pages {
        let page = page.unwrap();
        assert_eq!(page["buckets"], next["buckets"]);
        assert_eq!(page["from"], first["from"]);
        assert_eq!(page["to"], first["to"]);
        assert_eq!(page["settled_usd"], first["settled_usd"]);
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn retained_month_total_does_not_fabricate_deleted_detail() {
    let db = postgres::TestDb::new().await;
    let (id, source, _) = settled_fixture(&db).await;
    let grants = json!([]);
    let initial = db
        .store
        .dashboard_usage(
            id,
            UsageQuery::parse(None).unwrap(),
            &grants,
            "digest",
            b"cursor-key",
        )
        .await
        .unwrap();
    let start = initial["budget_balances"][0]["period_start"]
        .as_str()
        .unwrap();
    let end = initial["budget_balances"][0]["period_end"]
        .as_str()
        .unwrap();
    let mut tx = db.store.pool().begin().await.unwrap();
    sqlx::query("UPDATE gw_usage_totals SET carried_cost_usd=gw_amount(3),cost_usd=gw_amount(3),carried_settled_count=settled_count,carried_input_tokens=input_tokens,carried_output_tokens=output_tokens,detail_deleted_through=(SELECT admitted_at FROM gw_requests WHERE id=$1) WHERE scope_id=(SELECT id FROM gw_policies WHERE principal_id=$2 AND key_id IS NULL)").bind(source).bind(id).execute(&mut *tx).await.unwrap();
    sqlx::query("DELETE FROM gw_usage_events WHERE request_id=$1")
        .bind(source)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM gw_request_budgets WHERE request_id=$1")
        .bind(source)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM gw_requests WHERE id=$1")
        .bind(source)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let q = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("from", start)
        .append_pair("to", end)
        .finish();
    let totals = db
        .store
        .dashboard_usage(
            id,
            UsageQuery::parse(Some(&q)).unwrap(),
            &grants,
            "digest",
            b"cursor-key",
        )
        .await
        .unwrap();
    assert_eq!(totals["aggregation"], "period_totals");
    assert_eq!(totals["settled_usd"], "3.000000000000");
    assert!(totals["detail_available_from"].is_string());
    for suffix in [
        "provider=anthropic",
        "model=historic",
        "group_by=provider",
        "key_id=00000000-0000-0000-0000-000000000000",
    ] {
        let result = db
            .store
            .dashboard_usage(
                id,
                UsageQuery::parse(Some(&format!("{q}&{suffix}"))).unwrap(),
                &grants,
                "digest",
                b"cursor-key",
            )
            .await;
        if suffix.starts_with("key_id") {
            assert!(matches!(
                result,
                Err(m_gated::ledger::dashboard::QueryError::NotFound)
            ));
        } else {
            assert!(matches!(
                result,
                Err(m_gated::ledger::dashboard::QueryError::DetailUnavailable(_))
            ));
        }
    }
    db.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn schema_health_checks_all_required_financial_tables() {
    let db = postgres::TestDb::new().await;
    assert_eq!(db.store.health().await, (true, true));
    sqlx::query("DROP TABLE gw_usage_events")
        .execute(db.store.pool())
        .await
        .unwrap();
    assert_eq!(
        db.store.health().await,
        (true, false),
        "migration checksum만으로 실제 누락 테이블을 정상으로 판단하지 않는다"
    );
    db.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn selected_key_balance_and_null_tokens_are_independent_of_period_filters() {
    let db = postgres::TestDb::new().await;
    let (id, source, _) = settled_fixture(&db).await;
    let grants = json!([]);
    let key = uuid::Uuid::new_v4();
    let scope = uuid::Uuid::new_v4();
    let command = uuid::Uuid::new_v4();
    let other = finance::human(db.store.pool()).await;
    // S5 키 발급/인증 증거가 아닌 FK가 유효한 조회 fixture다.
    let mut tx = db.store.pool().begin().await.unwrap();
    sqlx::query("INSERT INTO gw_commands(actor_principal_id,command_id,input_digest,operation,target_kind,target_id,result_status,result_projection) VALUES($1,$2,decode(repeat('01',32),'hex'),'issue_key','key',$3,201,'{}')").bind(id).bind(command).bind(key).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO gw_virtual_keys(id,principal_id,secret_hash,hash_version,public_prefix,name,grant_snapshot,validity_preset,created_by,created_command_id) VALUES($1,$2,decode(repeat('02',32),'hex'),1,'fixture','fixture','[]','unlimited',$2,$3)").bind(key).bind(id).bind(command).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO gw_policies(id,principal_id,key_id,allow_rules,budget_mode,budget_limit_usd,period_kind,period_timezone,updated_by) VALUES($1,$2,$3,'[]','reserved',gw_amount(10),'calendar_month','UTC',$2)").bind(scope).bind(id).bind(key).execute(&mut *tx).await.unwrap();
    sqlx::query("UPDATE gw_request_budgets SET applied_input_tokens=NULL,applied_output_tokens=NULL WHERE request_id=$1").bind(source).execute(&mut *tx).await.unwrap();
    sqlx::query("UPDATE gw_usage_totals SET input_tokens=0,output_tokens=0,settled_unknown_tokens_count=1 WHERE scope_id=(SELECT id FROM gw_policies WHERE principal_id=$1 AND key_id IS NULL)").bind(id).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let q = format!(
        "key_id={key}&from=2025-01-01T00%3A00%3A00Z&to=2025-02-01T00%3A00%3A00Z&provider=deepseek&group_by=key"
    );
    let usage = db
        .store
        .dashboard_usage(
            id,
            UsageQuery::parse(Some(&q)).unwrap(),
            &grants,
            "digest",
            b"cursor-key",
        )
        .await
        .unwrap();
    assert_eq!(usage["budget_balances"].as_array().unwrap().len(), 2);
    assert_eq!(usage["balance_context"], "current_principal_and_key");
    assert_eq!(usage["budget_balances"][1]["scope_id"], scope.to_string());
    assert_eq!(usage["budget_balances"][1]["mode"], "reserved");
    assert_eq!(
        usage["budget_balances"][1]["remaining_usd"],
        "10.000000000000"
    );
    assert_eq!(usage["settled_usd"], "0.000000000000");
    assert!(matches!(
        db.store
            .dashboard_usage(
                other,
                UsageQuery::parse(Some(&q)).unwrap(),
                &grants,
                "digest",
                b"cursor-key"
            )
            .await,
        Err(m_gated::ledger::dashboard::QueryError::NotFound)
    ));
    let tokens = db
        .store
        .dashboard_usage(
            id,
            UsageQuery::parse(Some("group_by=provider")).unwrap(),
            &grants,
            "digest",
            b"cursor-key",
        )
        .await
        .unwrap();
    assert_eq!(tokens["buckets"]["items"][0]["tokens_complete"], false);
    assert_eq!(tokens["buckets"]["items"][0]["input_tokens"], Value::Null);
    assert_ne!(tokens["settled_usd"], "0.000000000000");
    db.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn multimonth_usage_never_emits_an_out_of_range_amount() {
    let db = postgres::TestDb::new().await;
    let (id, source, _) = settled_fixture(&db).await;
    let clone = uuid::Uuid::new_v4();
    let max = "9999999999999999.999999999999";
    let mut tx = db.store.pool().begin().await.unwrap();
    sqlx::query("UPDATE gw_requests SET cost_usd=gw_amount($2::numeric) WHERE id=$1")
        .bind(source)
        .bind(max)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE gw_request_budgets SET applied_cost_usd=gw_amount($2::numeric) WHERE request_id=$1",
    )
    .bind(source)
    .bind(max)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("UPDATE gw_usage_totals SET cost_usd=gw_amount($2::numeric) WHERE scope_id=(SELECT scope_id FROM gw_request_budgets WHERE request_id=$1)").bind(source).bind(max).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO gw_requests SELECT (jsonb_populate_record(NULL::gw_requests,to_jsonb(r)||jsonb_build_object('id',$2::uuid,'admitted_at',admitted_at-interval '1 month','admit_expires_at',admit_expires_at-interval '1 month','transport_deadline',transport_deadline-interval '1 month','execution_deadline',execution_deadline-interval '1 month','dispatch_authorized_at',dispatch_authorized_at-interval '1 month','lease_expires_at',lease_expires_at-interval '1 month','finished_at',finished_at-interval '1 month'))).* FROM gw_requests r WHERE id=$1").bind(source).bind(clone).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO gw_usage_totals SELECT (jsonb_populate_record(NULL::gw_usage_totals,to_jsonb(t)||jsonb_build_object('period_start',period_start-interval '1 month','period_end',period_end-interval '1 month'))).* FROM gw_usage_totals t WHERE scope_id=(SELECT scope_id FROM gw_request_budgets WHERE request_id=$1)").bind(source).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO gw_request_budgets SELECT (jsonb_populate_record(NULL::gw_request_budgets,to_jsonb(b)||jsonb_build_object('request_id',$2::uuid,'period_start',period_start-interval '1 month','period_end',period_end-interval '1 month'))).* FROM gw_request_budgets b WHERE request_id=$1").bind(source).bind(clone).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let now = time::OffsetDateTime::now_utc();
    let from = (now - time::Duration::days(60))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let to = now
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let q = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("from", &from)
        .append_pair("to", &to)
        .finish();
    let result = db
        .store
        .dashboard_usage(
            id,
            UsageQuery::parse(Some(&q)).unwrap(),
            &json!([]),
            "digest",
            b"cursor-key",
        )
        .await;
    assert!(
        matches!(
            result,
            Err(m_gated::ledger::dashboard::QueryError::Invariant)
        ),
        "여러 월의 합이 Amount 범위를 넘으면 0/비정규 금액으로 바꾸지 않는다"
    );
    db.close().await;
}
