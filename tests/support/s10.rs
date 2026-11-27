use super::*;
use std::time::Duration;

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn running_app_reaps_expiry_and_confirms_family_latch_without_claiming_stopped() {
    use m_gated::{
        auth::{SsoAuthenticator, SsoVerifier, jwks::JwksCache},
        bootstrap::{self, Dependencies},
        gateway::InferenceGateway,
        runtime::Lifecycle,
    };
    let db = postgres::TestDb::new().await;
    postgres::grant_access(&db.store, &configuration(), ACCESS).await;
    let cfg = configuration();
    let principal = finance::human(db.store.pool()).await;
    let current = finance::current(db.store.pool(), principal).await;
    let ledger = Arc::new(PgLedger::new(db.store.clone(), [1; 32]));
    let spec = specification();
    ledger.admit_request(&current, &spec).await.unwrap();
    let admitted = finance::expired_copy(db.store.pool(), spec.id, "admitted").await;
    let paid = finance::expired_copy(db.store.pool(), spec.id, "dispatching").await;
    let free =
        RequestSpec::from_profile(ProviderId::Anthropic, &cfg.providers()[0].profiles[1], None);
    ledger.admit_request(&current, &free).await.unwrap();
    let expired_free = finance::expired_copy(db.store.pool(), free.id, "dispatching").await;
    let fake = sso::FakeJwks::new(vec![sso::Signer::shared().jwk.clone()]);
    let jwks = fake.serve().await;
    let verifier = SsoVerifier::new(
        cfg.auth(),
        Arc::new(JwksCache::loopback_for_test(jwks.uri.clone()).unwrap()),
    );
    let auth = Arc::new(SsoAuthenticator::new(&cfg, verifier, db.store.clone()));
    let gateway = Arc::new(
        InferenceGateway::new(
            &cfg,
            auth.clone(),
            ledger.clone(),
            Arc::new(m_gated::credentials::CredentialCache::default()),
        )
        .unwrap(),
    );
    let ui = Arc::new(
        m_gated::dashboard::UiBundle::load(
            std::path::Path::new("web/dist"),
            &cfg.auth().issuer.origin().ascii_serialization(),
        )
        .unwrap(),
    );
    let app = bootstrap::build(
        cfg,
        Dependencies::unassembled()
            .with_inference(gateway)
            .with_authentication(auth)
            .with_ui(ui),
    )
    .unwrap();
    let runtime = app.runtime().clone();
    let mut changes = runtime.resource_changes();
    runtime.record_family_finalization_failure(admitted, Some(paid));
    runtime.record_request_finalization_failure(expired_free);
    let mut listener = sqlx::postgres::PgListener::connect_with(db.store.pool())
        .await
        .unwrap();
    let channel = format!("s10_{}", uuid::Uuid::new_v4().simple());
    listener.listen(&channel).await.unwrap();
    sqlx::raw_sql(&format!("CREATE FUNCTION gw_fixture_finish_notify() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.phase='finished' THEN PERFORM pg_notify('{channel}',NEW.id::text); END IF; RETURN NEW; END $$; CREATE TRIGGER gw_fixture_finish_notify AFTER UPDATE ON gw_requests FOR EACH ROW EXECUTE FUNCTION gw_fixture_finish_notify();")).execute(db.store.pool()).await.unwrap();
    let (stop, shutdown) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(app.run("127.0.0.1:0".parse().unwrap(), async {
        let _ = shutdown.await;
    }));
    let observed = tokio::time::timeout(Duration::from_secs(7), async {
        let mut ids = std::collections::BTreeSet::new();
        while ids.len() < 3 {
            ids.insert(listener.recv().await.unwrap().payload().to_owned());
        }
    })
    .await
    .is_ok();
    let automatically_confirmed = tokio::time::timeout(Duration::from_secs(7), async {
        while !runtime.failed_request_ids().is_empty() {
            changes.changed().await.unwrap();
        }
    })
    .await
    .is_ok();
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    let rows = sqlx::query("SELECT id,phase,accounting_state,cost_usd::text AS cost,execution_stopped_at IS NULL AS not_stopped FROM gw_requests WHERE id=ANY($1)").bind(vec![admitted,paid,expired_free]).fetch_all(db.store.pool()).await.unwrap();
    let remaining = runtime.failed_request_ids();
    let live_phase: String = sqlx::query_scalar("SELECT phase FROM gw_requests WHERE id=$1")
        .bind(spec.id)
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    drop(listener);
    db.close().await;
    assert!(
        observed,
        "실제 RunningApp의 유한 주기 복구가 만료 세 행을 마감해야 한다"
    );
    assert!(
        automatically_confirmed,
        "실행 중 자동 durable 가족 확인이 완료돼야 한다"
    );
    assert!(
        remaining.is_empty(),
        "durable 가족 확인 뒤에만 latch 추적을 제거한다"
    );
    assert_eq!(runtime.lifecycle(), Lifecycle::Stopped);
    assert_eq!(live_phase, "admitted");
    for row in rows {
        assert_eq!(row.get::<String, _>("phase"), "finished");
        assert!(row.get::<bool, _>("not_stopped"));
        assert_eq!(
            row.get::<String, _>("accounting_state"),
            if row.get::<uuid::Uuid, _>("id") == paid {
                "unknown"
            } else {
                "settled"
            }
        );
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn retention_carries_atomic_month_totals_keeps_unknown_and_replays_once() {
    use m_gated::{
        config::{DurationPreset, Retention},
        ledger::{commands::CommandContext, retention::RetentionDelete},
    };
    let db = postgres::TestDb::new().await;
    postgres::grant_access(
        &db.store,
        &configuration_from_yaml(&configuration_yaml_for_s10()),
        ACCESS,
    )
    .await;
    let database = db.clock_store(None).await;
    let cfg = configuration_from_yaml(&configuration_yaml_for_s10());
    let settings = AccessSettings::from_config(&cfg);
    let principal = finance::human(database.pool()).await;
    let current = finance::current(database.pool(), principal).await;
    let ledger = PgLedger::new(database.clone(), [1; 32]);
    let mut requests = Vec::new();
    for resolved in [false, true, true] {
        let spec = specification();
        ledger.admit_request(&current, &spec).await.unwrap();
        ledger.authorize_dispatch(spec.id).await.unwrap();
        if resolved {
            ledger
                .finalize_request(spec.id, &observation())
                .await
                .unwrap();
        } else {
            ledger
                .finalize_request(
                    spec.id,
                    &FinalObservation {
                        outcome: m_gated::ledger::finance::FinalOutcome::Failed,
                        usage: None,
                        counted_input_tokens: None,
                        reason: "upstream_transport_error",
                        http_status: None,
                        provider_request_id: None,
                    },
                )
                .await
                .unwrap();
        }
        requests.push(spec.id);
        // unknown을 지워 새 호출을 허용하지 않는다. 양성 fixture는 다른 주체로 분리해야 한다.
        if !resolved {
            break;
        }
    }
    let payer = finance::human(database.pool()).await;
    let payer_current = finance::current(database.pool(), payer).await;
    for _ in 0..2 {
        let spec = specification();
        ledger.admit_request(&payer_current, &spec).await.unwrap();
        ledger.authorize_dispatch(spec.id).await.unwrap();
        ledger
            .finalize_request(spec.id, &observation())
            .await
            .unwrap();
        requests.push(spec.id);
    }
    // 더 오래된 unknown을 같은 principal에 남긴다. 기존 settled 뒤 합성 시각만 과거로 고정한다.
    sqlx::query("UPDATE gw_fixture_clock SET observed='2025-03-01T00:00:00Z'")
        .execute(database.pool())
        .await
        .unwrap();
    let old_unknown = specification();
    ledger
        .admit_request(&payer_current, &old_unknown)
        .await
        .unwrap();
    ledger.authorize_dispatch(old_unknown.id).await.unwrap();
    ledger
        .finalize_request(
            old_unknown.id,
            &FinalObservation {
                outcome: m_gated::ledger::finance::FinalOutcome::Failed,
                usage: None,
                counted_input_tokens: None,
                reason: "upstream_transport_error",
                http_status: None,
                provider_request_id: None,
            },
        )
        .await
        .unwrap();
    // SQL DEFAULT는 migration 때 pg_catalog 함수 OID에 결합된다. 합성 event 시각도 명시한다.
    sqlx::query("UPDATE gw_usage_events e SET observed_at=r.admitted_at FROM gw_requests r WHERE r.id=e.request_id")
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE gw_fixture_clock SET observed='2025-07-01T00:00:00Z'")
        .execute(database.pool())
        .await
        .unwrap();
    let grants = serde_json::json!([]);
    let ctx = CommandContext {
        actor: principal,
        command_id: uuid::Uuid::new_v4(),
        method: "MAINTENANCE",
        path: "retention_delete",
        settings: &settings,
        grants: &grants,
        digest: [1; 32],
        ceiling: cfg.execution_ceiling(),
        execution_running: false,
    };
    let mut input = RetentionDelete {
        after: None,
        limit: 50,
        deletion_approved: false,
        approval_ref: "synthetic-approval-local".into(),
        reason: "합성 보관 시험".into(),
    };
    let policy = Retention {
        ledger: Some(DurationPreset::ThreeMonths),
        audit: Some(DurationPreset::OneYear),
        commands: Some(DurationPreset::ThreeYears),
    };
    assert!(
        ledger
            .retention_delete(&ctx, &policy, &input)
            .await
            .is_err()
    );
    input.deletion_approved = true;
    let absent = ledger
        .retention_delete(&ctx, &Retention::default(), &input)
        .await
        .unwrap();
    assert_eq!(absent["result"]["deleted_requests"], 0);
    let unlimited = Retention {
        ledger: Some(DurationPreset::Unlimited),
        ..Retention::default()
    };
    let changed_selection = ledger.retention_delete(&ctx, &unlimited, &input).await;
    let selection_conflicted = matches!(
        changed_selection,
        Err(m_gated::ledger::management::ManagementError::CommandConflict)
    );
    let ctx = CommandContext {
        command_id: uuid::Uuid::new_v4(),
        ..ctx
    };
    // 실제 DELETE 실패는 앞선 이월/경계와 command/audit도 rollback해야 한다.
    sqlx::raw_sql("CREATE FUNCTION gw_fixture_deny_delete() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic delete fault'; END $$; CREATE TRIGGER gw_fixture_deny_delete BEFORE DELETE ON gw_requests FOR EACH ROW EXECUTE FUNCTION gw_fixture_deny_delete()").execute(database.pool()).await.unwrap();
    assert!(
        ledger
            .retention_delete(&ctx, &policy, &input)
            .await
            .is_err()
    );
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM gw_usage_totals WHERE carried_settled_count<>0 OR detail_deleted_through IS NOT NULL").fetch_one(database.pool()).await.unwrap(),0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM gw_commands WHERE command_id=$1")
            .bind(ctx.command_id)
            .fetch_one(database.pool())
            .await
            .unwrap(),
        0
    );
    sqlx::query("DROP TRIGGER gw_fixture_deny_delete ON gw_requests")
        .execute(database.pool())
        .await
        .unwrap();
    let result = ledger
        .retention_delete(&ctx, &policy, &input)
        .await
        .unwrap();
    let replay = ledger
        .retention_delete(&ctx, &policy, &input)
        .await
        .unwrap();
    let totals=sqlx::query("SELECT cost_usd::text AS cost,carried_cost_usd::text AS carried,settled_count,carried_settled_count,input_tokens,carried_input_tokens,detail_deleted_through IS NOT NULL AS deleted FROM gw_usage_totals t JOIN gw_policies p ON p.id=t.scope_id WHERE p.principal_id=$1").bind(payer).fetch_one(database.pool()).await.unwrap();
    let unknown: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gw_requests WHERE accounting_state='unknown' AND id=$1",
    )
    .bind(requests[0])
    .fetch_one(database.pool())
    .await
    .unwrap();
    use m_gated::ledger::dashboard::{QueryError, UsageQuery};
    let month = "from=2025-03-01T00%3A00%3A00Z&to=2025-04-01T00%3A00%3A00Z";
    let usage = database
        .dashboard_usage(
            payer,
            UsageQuery::parse(Some(month)).unwrap(),
            &grants,
            "abcdef",
            b"synthetic-key",
        )
        .await
        .unwrap();
    let filtered = database
        .dashboard_usage(
            payer,
            UsageQuery::parse(Some(&format!("{month}&provider=anthropic"))).unwrap(),
            &grants,
            "abcdef",
            b"synthetic-key",
        )
        .await;
    // 새 pool/기본 DB 함수로 재조회해 in-memory 경계가 아닌 영속 경계를 관찰한다.
    let restarted = db
        .store
        .dashboard_usage(
            payer,
            UsageQuery::parse(Some(month)).unwrap(),
            &grants,
            "abcdef",
            b"synthetic-key",
        )
        .await
        .unwrap();
    let old_survived: bool =
        sqlx::query_scalar("SELECT accounting_state='unknown' FROM gw_requests WHERE id=$1")
            .bind(old_unknown.id)
            .fetch_one(database.pool())
            .await
            .unwrap();
    database.pool().close().await;
    db.close().await;
    assert!(old_survived);
    assert_eq!(usage["aggregation"], "period_totals");
    assert_eq!(usage["settled_usd"], "0.000280000000");
    assert_eq!(usage["global_unknown_count"], "1");
    assert_eq!(
        restarted["detail_available_from"],
        usage["detail_available_from"]
    );
    assert_eq!(restarted["settled_usd"], usage["settled_usd"]);
    assert!(matches!(filtered, Err(QueryError::DetailUnavailable(_))));
    assert!(
        selection_conflicted,
        "미선택과 명시 unlimited는 서로 다른 승인 입력이다"
    );
    assert_eq!(result["result"]["deleted_requests"], 2);
    assert_eq!(replay["result"], result["result"]);
    assert_eq!(replay["ack"]["replayed"], true);
    assert_eq!(unknown, 1);
    assert_eq!(totals.get::<String, _>("cost"), "0.000280000000");
    assert_eq!(totals.get::<String, _>("carried"), "0.000280000000");
    assert_eq!(totals.get::<i64, _>("settled_count"), 2);
    assert_eq!(totals.get::<i64, _>("carried_settled_count"), 2);
    assert_eq!(totals.get::<i64, _>("input_tokens"), 200);
    assert_eq!(totals.get::<i64, _>("carried_input_tokens"), 200);
    assert!(totals.get::<bool, _>("deleted"));
}
fn configuration_yaml_for_s10() -> String {
    support::YAML.replace("admin@example.invalid", "alice@example.test")
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn owner_before_service_nowait_skip_rolls_back_all_candidate_locks() {
    let db = postgres::TestDb::new().await;
    postgres::grant_access(&db.store, &configuration(), ACCESS).await;
    let owner = uuid::Uuid::from_u128(1);
    let service = uuid::Uuid::from_u128(2);
    sqlx::query("INSERT INTO gw_principals(id,kind,issuer,subject,email,display_name) VALUES($1,'human','https://issuer.example','owner','alice@example.test','Owner')").bind(owner).execute(db.store.pool()).await.unwrap();
    sqlx::query("INSERT INTO gw_principals(id,kind,display_name,owner_principal_id) VALUES($1,'service','Service',$2)").bind(service).bind(owner).execute(db.store.pool()).await.unwrap();
    sqlx::query("INSERT INTO gw_policies(id,principal_id,allow_rules,period_kind,period_timezone,updated_by) VALUES($1,$2,$3,'calendar_month','UTC',$4)").bind(uuid::Uuid::new_v4()).bind(service).bind(serde_json::json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages","count_tokens"]}])).bind(owner).execute(db.store.pool()).await.unwrap();
    let ledger = Arc::new(PgLedger::new(db.store.clone(), [1; 32]));
    let current = finance::current(db.store.pool(), service).await;
    let spec = specification();
    ledger.admit_request(&current, &spec).await.unwrap();
    let expired = finance::expired_copy(db.store.pool(), spec.id, "dispatching").await;
    for (held, available) in [(owner, service), (service, owner)] {
        let mut first = db.store.pool().begin().await.unwrap();
        sqlx::query("SELECT id FROM gw_principals WHERE id=$1 FOR UPDATE")
            .bind(held)
            .execute(&mut *first)
            .await
            .unwrap();
        assert_eq!(ledger.recover_expired(expired).await.unwrap(), None);
        let mut probe = db.store.pool().begin().await.unwrap();
        let released = sqlx::query("SELECT id FROM gw_principals WHERE id=$1 FOR UPDATE NOWAIT")
            .bind(available)
            .execute(&mut *probe)
            .await;
        probe.rollback().await.unwrap();
        first.rollback().await.unwrap();
        assert!(
            released.is_ok(),
            "O<S 공통순서와 후보전체rollback은 다른 principal 잠금을 남기면 안 된다"
        );
    }
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let (one, two) = tokio::join!(
        async {
            barrier.wait().await;
            ledger.recover_expired(expired).await
        },
        async {
            barrier.wait().await;
            ledger.finalize_request(expired, &observation()).await
        }
    );
    assert!(one.is_ok());
    assert!(two.is_ok() || two == Err(LedgerError::Conflict));
    let row=sqlx::query("SELECT accounting_state,execution_stopped_at IS NULL AS not_stopped,(SELECT sum(unknown_count)::bigint FROM gw_usage_totals) AS unknown,(SELECT count(*) FROM gw_usage_events WHERE request_id=$1) AS events FROM gw_requests WHERE id=$1").bind(expired).fetch_one(db.store.pool()).await.unwrap();
    db.close().await;
    assert_eq!(row.get::<String, _>("accounting_state"), "unknown");
    assert!(row.get::<bool, _>("not_stopped"));
    assert_eq!(row.get::<i64, _>("unknown"), 1);
    assert_eq!(row.get::<i64, _>("events"), 1);
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn retention_preserves_manual_null_tokens_in_both_scopes_and_latest_event_expiry() {
    use m_gated::{
        config::{DurationPreset, Retention},
        ledger::{
            commands::CommandContext,
            finance::KeyScope,
            management::{Command, IssueKey, Settlement},
            retention::RetentionDelete,
        },
    };
    let db = postgres::TestDb::new().await;
    postgres::grant_access(
        &db.store,
        &configuration_from_yaml(&configuration_yaml_for_s10()),
        ACCESS,
    )
    .await;
    let database = db.clock_store(None).await;
    let cfg = configuration_from_yaml(&configuration_yaml_for_s10());
    let settings = AccessSettings::from_config(&cfg);
    let principal = finance::human(database.pool()).await;
    let current = finance::current(database.pool(), principal).await;
    let ledger = PgLedger::new(database.clone(), [1; 32]);
    let grants = serde_json::json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["count_tokens","messages"]}]);
    let ctx = CommandContext {
        actor: principal,
        command_id: uuid::Uuid::new_v4(),
        method: "POST",
        path: "/api/keys",
        settings: &settings,
        grants: &grants,
        digest: [1; 32],
        ceiling: cfg.execution_ceiling(),
        execution_running: false,
    };
    let issue:IssueKey=serde_json::from_value(finance::with_credentials(serde_json::json!({"name":"보관 검증 키","validity":"unlimited","grants":grants,"limits":{"budget":null,"rpm":null,"tpm":null,"count_rpm":null}}))).unwrap();
    let issued = ledger
        .management_command(&ctx, Command::Issue(issue))
        .await
        .unwrap();
    let key = KeyScope {
        id: issued.body.unwrap()["metadata"]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
        version: 1,
    };
    let spec = specification();
    ledger
        .admit_scoped(&current, &spec, Some(key), None)
        .await
        .unwrap();
    ledger.authorize_dispatch(spec.id).await.unwrap();
    ledger
        .finalize_request(
            spec.id,
            &FinalObservation {
                outcome: m_gated::ledger::finance::FinalOutcome::Failed,
                usage: None,
                counted_input_tokens: None,
                reason: "upstream_transport_error",
                http_status: None,
                provider_request_id: None,
            },
        )
        .await
        .unwrap();
    sqlx::query("UPDATE gw_usage_events SET observed_at=(SELECT observed FROM gw_fixture_clock)")
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE gw_fixture_clock SET observed='2025-07-01T00:00:00Z'")
        .execute(database.pool())
        .await
        .unwrap();
    let version: String = sqlx::query_scalar("SELECT version::text FROM gw_requests WHERE id=$1")
        .bind(spec.id)
        .fetch_one(database.pool())
        .await
        .unwrap();
    let settle = CommandContext {
        command_id: uuid::Uuid::new_v4(),
        method: "PUT",
        path: "/api/admin/requests/settlement",
        ..ctx
    };
    ledger
        .management_command(
            &settle,
            Command::Settle(
                spec.id,
                Settlement {
                    expected_version: version,
                    decision: "charged".into(),
                    cost_usd: "0.000310000000".into(),
                    reason: "합성 수동 입력".into(),
                    execution_stopped_confirmed: Some(true),
                },
            ),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE gw_usage_events SET observed_at=(SELECT observed FROM gw_fixture_clock) WHERE kind='settlement'").execute(database.pool()).await.unwrap();
    let policy = Retention {
        ledger: Some(DurationPreset::ThreeMonths),
        ..Retention::default()
    };
    let input = RetentionDelete {
        after: None,
        limit: 50,
        deletion_approved: true,
        approval_ref: "synthetic-approval-local".into(),
        reason: "합성 기간 검증".into(),
    };
    let retention = CommandContext {
        command_id: uuid::Uuid::new_v4(),
        method: "MAINTENANCE",
        path: "retention_delete",
        ..settle
    };
    let recent = ledger
        .retention_delete(&retention, &policy, &input)
        .await
        .unwrap();
    sqlx::query("UPDATE gw_fixture_clock SET observed='2025-10-01T00:00:00Z'")
        .execute(database.pool())
        .await
        .unwrap();
    let retention = CommandContext {
        command_id: uuid::Uuid::new_v4(),
        ..retention
    };
    let expired = ledger
        .retention_delete(&retention, &policy, &input)
        .await
        .unwrap();
    let totals=sqlx::query("SELECT cost_usd::text AS cost,carried_cost_usd::text AS carried,settled_count,carried_settled_count,settled_unknown_tokens_count,carried_settled_unknown_tokens_count,input_tokens,output_tokens,detail_deleted_through IS NOT NULL AS deleted FROM gw_usage_totals ORDER BY scope_id").fetch_all(database.pool()).await.unwrap();
    let key_and_issue:i64=sqlx::query_scalar("SELECT count(*) FROM gw_virtual_keys k JOIN gw_commands c ON c.actor_principal_id=k.created_by AND c.command_id=k.created_command_id WHERE k.id=$1").bind(key.id).fetch_one(database.pool()).await.unwrap();
    let before_restart = filtered_usage_http(&cfg, database.clone(), principal).await;
    database.pool().close().await;
    db.store.pool().close().await;
    let restarted = db.store_via_proxy(64603).await;
    let after_restart = filtered_usage_http(&cfg, restarted.clone(), principal).await;
    let monthly = usage_http(
        &cfg,
        restarted.clone(),
        principal,
        "from=2025-03-01T00%3A00%3A00Z&to=2025-04-01T00%3A00%3A00Z",
    )
    .await;
    restarted.pool().close().await;
    db.close().await;
    assert_eq!(before_restart.0, 409);
    assert_eq!(after_restart.0, 409);
    assert_eq!(
        before_restart.1["error"]["details"]["detail_available_from"],
        after_restart.1["error"]["details"]["detail_available_from"]
    );
    assert_eq!(monthly.0, 200);
    assert_eq!(monthly.1["aggregation"], "period_totals");
    assert_eq!(monthly.1["settled_usd"], "0.000310000000");
    assert_eq!(
        recent["result"]["deleted_requests"], 0,
        "오래된 admission이 아니라 최신 수동 처리 event부터3개월이다"
    );
    assert_eq!(expired["result"]["deleted_requests"], 1);
    assert_eq!(key_and_issue, 1);
    assert_eq!(totals.len(), 2);
    for t in totals {
        for field in ["cost", "carried"] {
            assert_eq!(t.get::<String, _>(field), "0.000310000000");
        }
        for field in [
            "settled_count",
            "carried_settled_count",
            "settled_unknown_tokens_count",
            "carried_settled_unknown_tokens_count",
        ] {
            assert_eq!(t.get::<i64, _>(field), 1);
        }
        assert_eq!(t.get::<i64, _>("input_tokens"), 0);
        assert_eq!(t.get::<i64, _>("output_tokens"), 0);
        assert!(t.get::<bool, _>("deleted"));
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn retention_protects_entire_safety_family_then_deletes_child_before_root() {
    use m_gated::{
        config::{DurationPreset, Retention},
        ledger::{
            commands::CommandContext,
            management::{Command, ReleaseSafety},
            retention::RetentionDelete,
        },
    };
    let db = postgres::TestDb::new().await;
    postgres::grant_access(
        &db.store,
        &configuration_from_yaml(&configuration_yaml_for_s10()),
        ACCESS,
    )
    .await;
    let database = db.clock_store(None).await;
    let cfg = configuration_from_yaml(&configuration_yaml_for_s10());
    let settings = AccessSettings::from_config(&cfg);
    let principal = finance::human(database.pool()).await;
    let current = finance::current(database.pool(), principal).await;
    let ledger = PgLedger::new(database.clone(), [1; 32]);
    let root = specification();
    ledger.admit_request(&current, &root).await.unwrap();
    let count_cfg = configuration();
    let child = RequestSpec::from_profile(
        ProviderId::Anthropic,
        &count_cfg.providers()[0].profiles[1],
        None,
    );
    ledger
        .admit_scoped(&current, &child, None, Some(root.id))
        .await
        .unwrap();
    ledger.authorize_dispatch(child.id).await.unwrap();
    ledger
        .finalize_request(
            child.id,
            &FinalObservation {
                outcome: m_gated::ledger::finance::FinalOutcome::Completed,
                usage: None,
                counted_input_tokens: Some(321),
                reason: "completed",
                http_status: Some(200),
                provider_request_id: None,
            },
        )
        .await
        .unwrap();
    ledger.authorize_dispatch(root.id).await.unwrap();
    let mut violation = observation();
    violation.usage.as_mut().unwrap().input_tokens = 5000;
    ledger.finalize_request(root.id, &violation).await.unwrap();
    let safety:i64=sqlx::query_scalar("SELECT inference_safety_version FROM gw_principals WHERE id=$1 AND inference_paused_at IS NOT NULL AND inference_pause_request_id=$2").bind(principal).bind(root.id).fetch_one(database.pool()).await.unwrap();
    sqlx::query("UPDATE gw_usage_events e SET observed_at=r.admitted_at FROM gw_requests r WHERE r.id=e.request_id").execute(database.pool()).await.unwrap();
    sqlx::query("UPDATE gw_fixture_clock SET observed='2025-07-01T00:00:00Z'")
        .execute(database.pool())
        .await
        .unwrap();
    let grants = serde_json::json!([]);
    let ctx = CommandContext {
        actor: principal,
        command_id: uuid::Uuid::new_v4(),
        method: "MAINTENANCE",
        path: "retention_delete",
        settings: &settings,
        grants: &grants,
        digest: [1; 32],
        ceiling: cfg.execution_ceiling(),
        execution_running: false,
    };
    let policy = Retention {
        ledger: Some(DurationPreset::ThreeMonths),
        ..Retention::default()
    };
    let input = RetentionDelete {
        after: None,
        limit: 1,
        deletion_approved: true,
        approval_ref: "synthetic-approval-local".into(),
        reason: "합성 가족 보관".into(),
    };
    let protected = ledger
        .retention_delete(&ctx, &policy, &input)
        .await
        .unwrap();
    let release = CommandContext {
        command_id: uuid::Uuid::new_v4(),
        method: "POST",
        path: "/api/admin/principals/inference-pause/release",
        ..ctx
    };
    ledger
        .management_command(
            &release,
            Command::Release(
                principal,
                ReleaseSafety {
                    expected_version: safety.to_string(),
                    reason: "합성 명시 해제".into(),
                },
            ),
        )
        .await
        .unwrap();
    let ctx = CommandContext {
        command_id: uuid::Uuid::new_v4(),
        method: "MAINTENANCE",
        path: "retention_delete",
        ..release
    };
    let removed = ledger
        .retention_delete(&ctx, &policy, &input)
        .await
        .unwrap();
    let total=sqlx::query("SELECT cost_usd::text AS cost,carried_cost_usd::text AS carried,carried_settled_count,input_tokens,output_tokens FROM gw_usage_totals").fetch_one(database.pool()).await.unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_requests WHERE id=ANY($1)")
        .bind(vec![root.id, child.id])
        .fetch_one(database.pool())
        .await
        .unwrap();
    database.pool().close().await;
    db.close().await;
    assert_eq!(protected["result"]["deleted_requests"], 0);
    assert_eq!(removed["result"]["deleted_requests"], 2);
    assert_eq!(left, 0);
    assert_eq!(total.get::<String, _>("cost"), "0.005040000000");
    assert_eq!(total.get::<String, _>("carried"), "0.005040000000");
    assert_eq!(total.get::<i64, _>("carried_settled_count"), 2);
    assert_eq!(total.get::<i64, _>("input_tokens"), 5000);
    assert_eq!(total.get::<i64, _>("output_tokens"), 20);
}

async fn filtered_usage_http(
    cfg: &config::ValidatedConfig,
    store: Arc<m_gated::ledger::PgIdentityStore>,
    target: uuid::Uuid,
) -> (u16, serde_json::Value) {
    usage_http(
        cfg,
        store,
        target,
        "from=2025-02-28T15%3A00%3A00Z&to=2025-04-01T00%3A00%3A00Z&provider=anthropic",
    )
    .await
}
async fn usage_http(
    cfg: &config::ValidatedConfig,
    store: Arc<m_gated::ledger::PgIdentityStore>,
    target: uuid::Uuid,
    query: &str,
) -> (u16, serde_json::Value) {
    use m_gated::{
        auth::{SsoAuthenticator, SsoVerifier, jwks::JwksCache},
        http::{self, AppState},
        runtime::{Dependency, Runtime},
    };
    use tower::ServiceExt;
    let fake = sso::FakeJwks::new(vec![sso::Signer::shared().jwk.clone()]);
    let server = fake.serve().await;
    let verifier = SsoVerifier::new(
        cfg.auth(),
        Arc::new(JwksCache::loopback_for_test(server.uri.clone()).unwrap()),
    );
    let auth = Arc::new(SsoAuthenticator::new(cfg, verifier, store.clone()));
    let api = Arc::new(m_gated::admin::DashboardApi::new(cfg, auth, store));
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
        api.routes(runtime),
    );
    let token = sso::Signer::shared().sign(&sso::claims(), "key1");
    let response = router
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/api/admin/usage?principal_id={target}&{query}"))
                .header("authorization", format!("Bearer {token}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn telemetry_actual_sql_jwks_provider_success_unknown_and_company_key_are_redacted() {
    use axum::body::Bytes;
    use axum::http::{HeaderMap, StatusCode};
    use m_gated::telemetry::{self, Metric};
    let tracked = [
        Metric::Authentication,
        Metric::JwksFetch,
        Metric::DbAcquire,
        Metric::DbLock,
        Metric::Finalization,
        Metric::Upstream2xx,
        Metric::Upstream4xx,
        Metric::Streams,
        Metric::Ttft,
        Metric::StreamMicros,
        Metric::AlarmUnknown,
        Metric::AlarmCompanyKey,
    ];
    let before = tracked.map(telemetry::get);
    let h = Harness::new().await;
    let good = h
        .request("/anthropic/v1/messages", BODY)
        .send()
        .await
        .unwrap();
    let good_status = good.status();
    let _ = good.bytes().await.unwrap();
    h.runtime.wait_requests().await;
    h.provider
        .set_chunked(
            StatusCode::OK,
            HeaderMap::from_iter([(
                "content-type".parse().unwrap(),
                "text/event-stream".parse().unwrap(),
            )]),
            Bytes::from(format!("{}{}", s3_contract::START, s3_contract::END)),
        )
        .await;
    let stream = h
        .request(
            "/anthropic/v1/messages",
            BODY.replacen('{', "{\"stream\":true,", 1),
        )
        .send()
        .await
        .unwrap();
    let stream_status = stream.status();
    let _ = stream.bytes().await.unwrap();
    h.runtime.wait_requests().await;
    h.provider
        .set(
            StatusCode::UNAUTHORIZED,
            HeaderMap::new(),
            Bytes::from_static(b"synthetic-company-key-conversation-sentinel"),
        )
        .await;
    let failed = h
        .request("/anthropic/v1/messages", BODY)
        .send()
        .await
        .unwrap();
    let failed_status = failed.status();
    let _ = failed.bytes().await.unwrap();
    h.runtime.wait_requests().await;
    // 회사 키 인증 오류는 0원으로 정산하고 막지 않는다(결정 0005). 미정산 계측은 해석하지 못한
    // 응답(gzip)으로 만든다.
    h.provider
        .set(
            StatusCode::OK,
            HeaderMap::from_iter([
                (
                    "content-type".parse().unwrap(),
                    "application/json".parse().unwrap(),
                ),
                ("content-encoding".parse().unwrap(), "gzip".parse().unwrap()),
            ]),
            Bytes::from_static(b"synthetic-company-key-conversation-sentinel"),
        )
        .await;
    let unreadable = h
        .request("/anthropic/v1/messages", BODY)
        .send()
        .await
        .unwrap();
    let unreadable_status = unreadable.status();
    let _ = unreadable.bytes().await.unwrap();
    h.runtime.wait_requests().await;
    let ledger = PgLedger::new(h.database.clone(), [1; 32]);
    ledger.recovery_tick(&h.runtime).await.unwrap();
    let unknown = telemetry::get(Metric::Unknown);
    let blocked = telemetry::get(Metric::BlockedPrincipals);
    h.runtime.observe_telemetry(tokio::time::Instant::now());
    let snapshot = telemetry::snapshot().to_string();
    let after = tracked.map(telemetry::get);
    let sql_unknown: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gw_requests WHERE accounting_state='unknown'")
            .fetch_one(h.database.pool())
            .await
            .unwrap();
    h.close().await;
    assert_eq!(good_status, 200);
    assert_eq!(stream_status, 200);
    assert_eq!(failed_status, 502);
    assert_eq!(unreadable_status, 502);
    assert_eq!(telemetry::get(Metric::InputBytes), 0);
    assert_eq!(sql_unknown, 1);
    assert!(unknown >= 1 && blocked >= 1);
    for (previous, current) in before.into_iter().zip(after) {
        assert!(current > previous, "실제 경계 계측 누락");
    }
    for forbidden in [
        "synthetic-company-key",
        "conversation-sentinel",
        "synthetic prompt sentinel",
        "alice@example.test",
        "key1",
    ] {
        assert!(!snapshot.contains(forbidden));
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn telemetry_pricing_alarm_follows_durable_pause_and_replay_does_not_duplicate() {
    use m_gated::telemetry::{self, Metric};
    let db = postgres::TestDb::new().await;
    postgres::grant_access(&db.store, &configuration(), ACCESS).await;
    let user = finance::human(db.store.pool()).await;
    sqlx::query(
        "UPDATE gw_policies SET budget_limit_usd=1 WHERE principal_id=$1 AND key_id IS NULL",
    )
    .bind(user)
    .execute(db.store.pool())
    .await
    .unwrap();
    let current = finance::current(db.store.pool(), user).await;
    let ledger = PgLedger::new(db.store.clone(), [1; 32]);
    // 비용 상한은 입력 상한 계약 없이도 profile의 최대 입력과 요청의 max_tokens로 계산한다.
    let request = specification();
    ledger.admit_request(&current, &request).await.unwrap();
    ledger.authorize_dispatch(request.id).await.unwrap();
    let before = telemetry::get(Metric::AlarmBound);
    let mut final_usage = observation();
    final_usage.usage.as_mut().unwrap().input_tokens = 5000;
    let finalized = ledger
        .finalize_request(request.id, &final_usage)
        .await
        .unwrap();
    let replay = ledger
        .finalize_request(request.id, &final_usage)
        .await
        .unwrap();
    let count = telemetry::get(Metric::AlarmBound);
    let durable:bool=sqlx::query_scalar("SELECT p.inference_pause_request_id=r.id AND r.safety_violation_recorded AND EXISTS(SELECT 1 FROM gw_audit_events WHERE request_id=r.id AND actor_kind='system') FROM gw_requests r JOIN gw_principals p ON p.id=r.principal_id WHERE r.id=$1").bind(request.id).fetch_one(db.store.pool()).await.unwrap();
    db.close().await;
    assert_eq!(finalized, Finalized::Settled);
    assert_eq!(replay, Finalized::Noop);
    assert!(durable);
    assert_eq!(count, before + 1);
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL; holds the database-wide migration lock for actual30s and120s timeout boundaries"]
async fn two_migrators_serialize_and_actual_advisory30s_total120s_fail_closed() {
    let db = postgres::TestDb::new().await;
    postgres::grant_access(&db.store, &configuration(), ACCESS).await;
    let mut holder = db.store.pool().begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(7234638650625911)")
        .execute(&mut *holder)
        .await
        .unwrap();
    let start = tokio::time::Instant::now();
    let timed_out = db.store.migrate().await.is_err();
    let elapsed = start.elapsed();
    holder.rollback().await.unwrap();
    let barrier = tokio::sync::Barrier::new(2);
    let (one, two) = tokio::join!(
        async {
            barrier.wait().await;
            db.store.migrate().await
        },
        async {
            barrier.wait().await;
            db.store.migrate().await
        }
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_schema_migrations")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    let proxy = pg_proxy::CommitProxy::migration_stall().await;
    let faulty = db.store_via_proxy(proxy.port).await;
    proxy.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let start_total = tokio::time::Instant::now();
    let (total_timeout, reached) = tokio::join!(faulty.migrate(), async {
        tokio::time::timeout(Duration::from_secs(5), proxy.committed.notified())
            .await
            .is_ok()
    });
    let total_elapsed = start_total.elapsed();
    drop(proxy);
    faulty.pool().close().await;
    let clean = db.store.migrate().await.is_ok();
    db.close().await;
    assert!(timed_out && elapsed >= Duration::from_secs(29) && elapsed < Duration::from_secs(35));
    assert!(one.is_ok() && two.is_ok() && count == 18);
    assert!(
        reached
            && total_timeout.is_err()
            && total_elapsed >= Duration::from_secs(119)
            && total_elapsed < Duration::from_secs(125)
    );
    assert!(
        clean,
        "timeout 뒤 advisory/transaction 잠금이 남으면 안 된다"
    );
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn owner_before_service_two_reapers_and_real_dispatch_cross_without_reverse_lock_or_resend() {
    use m_gated::{
        providers::anthropic::{AnthropicClient, SendOptions},
        runtime::{Dependency, Runtime},
    };
    for phase in ["admitted", "dispatching"] {
        let db = postgres::TestDb::new().await;
        postgres::grant_access(&db.store, &configuration(), ACCESS).await;
        let owner = uuid::Uuid::from_u128(1);
        let service = uuid::Uuid::from_u128(2);
        sqlx::query("INSERT INTO gw_principals(id,kind,issuer,subject,email,display_name) VALUES($1,'human','https://issuer.example','owner','alice@example.test','Owner')").bind(owner).execute(db.store.pool()).await.unwrap();
        sqlx::query("INSERT INTO gw_principals(id,kind,display_name,owner_principal_id) VALUES($1,'service','Service',$2)").bind(service).bind(owner).execute(db.store.pool()).await.unwrap();
        sqlx::query("INSERT INTO gw_policies(id,principal_id,allow_rules,period_kind,period_timezone,updated_by) VALUES($1,$2,$3,'calendar_month','UTC',$4)").bind(uuid::Uuid::new_v4()).bind(service).bind(serde_json::json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages","count_tokens"]}])).bind(owner).execute(db.store.pool()).await.unwrap();
        let cfg = configuration();
        let ledger = PgLedger::new(db.store.clone(), [1; 32]);
        let current = finance::current(db.store.pool(), service).await;
        let r2 = specification();
        ledger.admit_request(&current, &r2).await.unwrap();
        let r1 = finance::expired_copy(db.store.pool(), r2.id, phase).await;
        let barrier = tokio::sync::Barrier::new(3);
        let (first, second, dispatch) = tokio::join!(
            async {
                barrier.wait().await;
                ledger.recover_expired(r1).await
            },
            async {
                barrier.wait().await;
                ledger.recover_expired(r1).await
            },
            async {
                barrier.wait().await;
                ledger.authorize_dispatch(r2.id).await
            }
        );
        // dispatch가 O를 먼저 잠그면 두 NOWAIT 후보 모두 정상 skip이다. rollback ACK barrier 뒤 다음 tick에서만 복구한다.
        let mut ack = db.store.pool().begin().await.unwrap();
        sqlx::query("SELECT id FROM gw_principals WHERE id=ANY($1) ORDER BY id FOR UPDATE")
            .bind(vec![owner, service])
            .fetch_all(&mut *ack)
            .await
            .unwrap();
        ack.rollback().await.unwrap();
        let next_tick = ledger.recover_expired(r1).await.unwrap();
        let provider = provider::Provider::new();
        let server = provider.serve().await;
        let client =
            AnthropicClient::loopback_for_test(server.base.clone(), "synthetic-company-key".into())
                .unwrap();
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
        let dispatch_error = dispatch.as_ref().err().copied();
        if let Ok(permit) = dispatch {
            let headers = axum::http::HeaderMap::from_iter([
                (
                    "content-type".parse().unwrap(),
                    "application/json".parse().unwrap(),
                ),
                (
                    "anthropic-version".parse().unwrap(),
                    "2023-06-01".parse().unwrap(),
                ),
            ]);
            let result = client
                .send(
                    permit,
                    BODY,
                    SendOptions {
                        api: config::ApiKind::Messages,
                        headers: &headers,
                        profile: &cfg.providers()[0].profiles[0],
                        deadline: tokio::time::Instant::now() + Duration::from_secs(3),
                        runtime: &runtime,
                    },
                )
                .await
                .unwrap();
            ledger
                .finalize_request(r2.id, &result.observation)
                .await
                .unwrap();
        }
        let row=sqlx::query("SELECT accounting_state,cost_usd::text,execution_stopped_at IS NULL AS not_stopped,(SELECT count(*) FROM gw_usage_events WHERE request_id=$1)::bigint AS events FROM gw_requests WHERE id=$1").bind(r1).fetch_one(db.store.pool()).await.unwrap();
        let mut probe = db.store.pool().begin().await.unwrap();
        let unlocked = sqlx::query(
            "SELECT id FROM gw_principals WHERE id=ANY($1) ORDER BY id FOR UPDATE NOWAIT",
        )
        .bind(vec![owner, service])
        .fetch_all(&mut *probe)
        .await
        .is_ok();
        probe.rollback().await.unwrap();
        let sends = provider.calls.lock().await.len();
        let second_row = sqlx::query("SELECT phase,accounting_state,cost_usd::text,dispatch_authorized_at IS NOT NULL AS authorized FROM gw_requests WHERE id=$1").bind(r2.id).fetch_one(db.store.pool()).await.unwrap();
        db.close().await;
        assert!(first.is_ok() && second.is_ok());
        assert_eq!(
            usize::from(first.unwrap().is_some())
                + usize::from(second.unwrap().is_some())
                + usize::from(next_tick.is_some()),
            1
        );
        assert!(unlocked && row.get::<bool, _>("not_stopped"));
        assert_eq!(row.get::<i64, _>("events"), 1);
        assert_eq!(
            row.get::<String, _>("accounting_state"),
            if phase == "admitted" {
                "settled"
            } else {
                "unknown"
            }
        );
        match dispatch_error {
            None => {
                assert_eq!(sends, 1, "확정 permit 하나의 실제 송신만 허용한다");
                assert!(second_row.get::<bool, _>("authorized"));
                assert_eq!(second_row.get::<String, _>("phase"), "finished");
                assert_eq!(second_row.get::<String, _>("accounting_state"), "settled");
                assert_eq!(second_row.get::<String, _>("cost_usd"), "0.000140000000");
            }
            Some(LedgerError::Unsettled | LedgerError::DatabaseUnavailable) => {
                // 경합의 유한 DB 거부/금전 차단은 permit을 뜻하지 않는다. 다른 오류를 성공으로 합치지 않는다.
                assert_eq!(sends, 0);
                assert!(!second_row.get::<bool, _>("authorized"));
                assert_eq!(second_row.get::<String, _>("phase"), "admitted");
                assert_eq!(second_row.get::<String, _>("accounting_state"), "pending");
                if dispatch_error == Some(LedgerError::Unsettled) {
                    assert_eq!(row.get::<String, _>("accounting_state"), "unknown");
                }
            }
            Some(error) => panic!("예상하지 않은 dispatch 거부 enum: {error:?}"),
        }
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn retention_all_finite_presets_expire_at_utc_calendar_month_end_including_leap_year() {
    use m_gated::{
        config::{DurationPreset, Retention},
        ledger::{commands::CommandContext, retention::RetentionDelete},
    };
    for (preset, created, expires) in [
        (
            DurationPreset::ThreeMonths,
            "2023-11-30T12:00:00Z",
            "2024-02-29T12:00:00Z",
        ),
        (
            DurationPreset::SixMonths,
            "2023-08-31T12:00:00Z",
            "2024-02-29T12:00:00Z",
        ),
        (
            DurationPreset::OneYear,
            "2024-02-29T12:00:00Z",
            "2025-02-28T12:00:00Z",
        ),
        (
            DurationPreset::ThreeYears,
            "2024-02-29T12:00:00Z",
            "2027-02-28T12:00:00Z",
        ),
    ] {
        let db = postgres::TestDb::new().await;
        postgres::grant_access(
            &db.store,
            &configuration_from_yaml(&configuration_yaml_for_s10()),
            ACCESS,
        )
        .await;
        let database = db.clock_store(None).await;
        sqlx::query("UPDATE gw_fixture_clock SET observed=$1::timestamptz")
            .bind(created)
            .execute(database.pool())
            .await
            .unwrap();
        let cfg = configuration_from_yaml(&configuration_yaml_for_s10());
        let settings = AccessSettings::from_config(&cfg);
        let actor = finance::human(database.pool()).await;
        let current = finance::current(database.pool(), actor).await;
        let ledger = PgLedger::new(database.clone(), [1; 32]);
        let request = specification();
        ledger.admit_request(&current, &request).await.unwrap();
        ledger
            .finalize_request(
                request.id,
                &FinalObservation {
                    outcome: m_gated::ledger::finance::FinalOutcome::Rejected,
                    usage: None,
                    counted_input_tokens: None,
                    reason: "policy_changed",
                    http_status: None,
                    provider_request_id: None,
                },
            )
            .await
            .unwrap();
        sqlx::query("UPDATE gw_usage_events SET observed_at=$1::timestamptz")
            .bind(created)
            .execute(database.pool())
            .await
            .unwrap();
        let grants = serde_json::json!([]);
        let ctx = CommandContext {
            actor,
            command_id: uuid::Uuid::new_v4(),
            method: "MAINTENANCE",
            path: "retention",
            settings: &settings,
            grants: &grants,
            digest: [1; 32],
            ceiling: cfg.execution_ceiling(),
            execution_running: false,
        };
        let input = RetentionDelete {
            after: None,
            limit: 50,
            deletion_approved: true,
            approval_ref: "synthetic-approval-local".into(),
            reason: "유한 보관 달력 경계".into(),
        };
        let policy = Retention {
            ledger: Some(preset),
            audit: None,
            commands: None,
        };
        sqlx::query(
            "UPDATE gw_fixture_clock SET observed=$1::timestamptz-interval '1 microsecond'",
        )
        .bind(expires)
        .execute(database.pool())
        .await
        .unwrap();
        let before = ledger
            .retention_delete(&ctx, &policy, &input)
            .await
            .unwrap();
        sqlx::query("UPDATE gw_fixture_clock SET observed=$1::timestamptz")
            .bind(expires)
            .execute(database.pool())
            .await
            .unwrap();
        let exact = ledger
            .retention_delete(
                &CommandContext {
                    command_id: uuid::Uuid::new_v4(),
                    ..ctx
                },
                &policy,
                &input,
            )
            .await
            .unwrap();
        let row=sqlx::query("SELECT settled_count,carried_settled_count,cost_usd::text,detail_deleted_through IS NOT NULL AS boundary FROM gw_usage_totals").fetch_one(database.pool()).await.unwrap();
        database.pool().close().await;
        db.close().await;
        assert_eq!(before["result"]["deleted_requests"], 0);
        assert_eq!(exact["result"]["deleted_requests"], 1);
        assert_eq!(row.get::<i64, _>("settled_count"), 1);
        assert_eq!(row.get::<i64, _>("carried_settled_count"), 1);
        assert_eq!(row.get::<String, _>("cost_usd"), "0.000000000000");
        assert!(row.get::<bool, _>("boundary"));
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn retention_page_preserves_totals_and_remaining_references() {
    use m_gated::{
        config::{DurationPreset, Retention},
        ledger::{commands::CommandContext, retention::RetentionDelete},
    };
    let db = postgres::TestDb::new().await;
    postgres::grant_access(
        &db.store,
        &configuration_from_yaml(&configuration_yaml_for_s10()),
        ACCESS,
    )
    .await;
    let database = db.clock_store(None).await;
    sqlx::query("UPDATE gw_fixture_clock SET observed='2025-01-31T12:00:00Z'")
        .execute(database.pool())
        .await
        .unwrap();
    let cfg = configuration_from_yaml(&configuration_yaml_for_s10());
    let settings = AccessSettings::from_config(&cfg);
    let actor = finance::human(database.pool()).await;
    let current = finance::current(database.pool(), actor).await;
    let ledger = PgLedger::new(database.clone(), [1; 32]);
    for _ in 0..51 {
        let root = specification();
        ledger.admit_request(&current, &root).await.unwrap();
        ledger
            .finalize_request(
                root.id,
                &FinalObservation {
                    outcome: m_gated::ledger::finance::FinalOutcome::Rejected,
                    usage: None,
                    counted_input_tokens: None,
                    reason: "policy_changed",
                    http_status: None,
                    provider_request_id: None,
                },
            )
            .await
            .unwrap();
    }
    sqlx::query("UPDATE gw_usage_events SET observed_at='2025-01-31T12:00:00Z'")
        .execute(database.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE gw_fixture_clock SET observed='2025-04-30T12:00:00Z'")
        .execute(database.pool())
        .await
        .unwrap();
    let grants = serde_json::json!([]);
    let ctx = CommandContext {
        actor,
        command_id: uuid::Uuid::new_v4(),
        method: "MAINTENANCE",
        path: "retention",
        settings: &settings,
        grants: &grants,
        digest: [1; 32],
        ceiling: cfg.execution_ceiling(),
        execution_running: false,
    };
    let input = RetentionDelete {
        after: None,
        limit: 50,
        deletion_approved: true,
        approval_ref: "synthetic-approval-local".into(),
        reason: "50행 배치".into(),
    };
    let policy = Retention {
        ledger: Some(DurationPreset::ThreeMonths),
        audit: None,
        commands: None,
    };
    let first = ledger
        .retention_delete(&ctx, &policy, &input)
        .await
        .unwrap();
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_requests")
        .fetch_one(database.pool())
        .await
        .unwrap();
    let protected_window:i64=sqlx::query_scalar("SELECT count(*) FROM gw_rate_windows w WHERE EXISTS(SELECT 1 FROM gw_request_budgets b WHERE b.scope_id=w.scope_id AND b.rate_lane=w.lane AND b.rate_start=w.window_start)").fetch_one(database.pool()).await.unwrap();
    let cursor = first["result"]["next_after"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let second = ledger
        .retention_delete(
            &CommandContext {
                command_id: uuid::Uuid::new_v4(),
                ..ctx
            },
            &policy,
            &RetentionDelete {
                after: Some(cursor),
                ..input
            },
        )
        .await
        .unwrap();
    let totals=sqlx::query("SELECT settled_count,carried_settled_count,cost_usd::text,settled_unknown_tokens_count FROM gw_usage_totals").fetch_one(database.pool()).await.unwrap();
    database.pool().close().await;
    db.close().await;
    // 한 쪽에 요청 50개까지 지운다. 남은 요청이 가리키는 1분 칸은 지우지 않는다.
    assert_eq!(first["result"]["deleted_requests"], 50);
    assert_eq!(remaining, 1);
    assert!(protected_window > 0);
    assert_eq!(second["result"]["deleted_requests"], 1);
    assert_eq!(totals.get::<i64, _>("settled_count"), 51);
    assert_eq!(totals.get::<i64, _>("carried_settled_count"), 51);
    assert_eq!(totals.get::<String, _>("cost_usd"), "0.000000000000");
    assert_eq!(totals.get::<i64, _>("settled_unknown_tokens_count"), 0);
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn existing_permit_before_unknown_is_not_revoked_but_unknown_before_permit_never_sends() {
    use m_gated::{
        providers::anthropic::{AnthropicClient, SendOptions},
        runtime::{Dependency, RequestDeadline, Runtime},
    };
    for permit_first in [true, false] {
        let db = postgres::TestDb::new().await;
        postgres::grant_access(&db.store, &configuration(), ACCESS).await;
        let database = db.clock_store(None).await;
        let user = finance::human(database.pool()).await;
        let current = finance::current(database.pool(), user).await;
        let cfg = configuration();
        let ledger = PgLedger::new(database.clone(), [1; 32]);
        let mut first = specification();
        let now = tokio::time::Instant::now();
        first.deadline =
            RequestDeadline::new(now + Duration::from_secs(25), now + Duration::from_secs(30));
        ledger.admit_request(&current, &first).await.unwrap();
        ledger.authorize_dispatch(first.id).await.unwrap();
        let second = specification();
        ledger.admit_request(&current, &second).await.unwrap();
        let prior = if permit_first {
            Some(ledger.authorize_dispatch(second.id).await.unwrap())
        } else {
            None
        };
        sqlx::query("UPDATE gw_fixture_clock SET observed=(SELECT lease_expires_at FROM gw_requests WHERE id=$1)").bind(first.id).execute(database.pool()).await.unwrap();
        assert_eq!(
            ledger.recover_expired(first.id).await.unwrap(),
            Some(Finalized::Unknown)
        );
        let permit = if permit_first {
            prior
        } else {
            let rejected = ledger.authorize_dispatch(second.id).await;
            assert_eq!(rejected.err(), Some(LedgerError::Unsettled));
            None
        };
        let provider = provider::Provider::new();
        let server = provider.serve().await;
        let client =
            AnthropicClient::loopback_for_test(server.base.clone(), "synthetic-company-key".into())
                .unwrap();
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
        if let Some(permit) = permit {
            let headers = axum::http::HeaderMap::from_iter([
                (
                    "content-type".parse().unwrap(),
                    "application/json".parse().unwrap(),
                ),
                (
                    "anthropic-version".parse().unwrap(),
                    "2023-06-01".parse().unwrap(),
                ),
            ]);
            let result = client
                .send(
                    permit,
                    BODY,
                    SendOptions {
                        api: config::ApiKind::Messages,
                        headers: &headers,
                        profile: &cfg.providers()[0].profiles[0],
                        deadline: tokio::time::Instant::now() + Duration::from_secs(3),
                        runtime: &runtime,
                    },
                )
                .await
                .unwrap();
            ledger
                .finalize_request(second.id, &result.observation)
                .await
                .unwrap();
        }
        let blocked = ledger.authorize_dispatch(second.id).await.is_err();
        let calls = provider.calls.lock().await.len();
        database.pool().close().await;
        db.close().await;
        assert_eq!(calls, usize::from(permit_first));
        assert!(blocked);
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL; isolated process telemetry counters"]
async fn telemetry_final_commit_ack_loss_reobserves_durable_alarm_once_after_reassembly() {
    use m_gated::telemetry::{self, Collector, Metric};
    use std::{
        io::Read,
        os::{fd::OwnedFd, unix::net::UnixStream},
        sync::atomic::Ordering,
    };
    for bound in [false, true] {
        let db = postgres::TestDb::new().await;
        postgres::grant_access(&db.store, &configuration(), ACCESS).await;
        let user = finance::human(db.store.pool()).await;
        sqlx::query(
            "UPDATE gw_policies SET budget_limit_usd=1 WHERE principal_id=$1 AND key_id IS NULL",
        )
        .bind(user)
        .execute(db.store.pool())
        .await
        .unwrap();
        let current = finance::current(db.store.pool(), user).await;
        let cfg = configuration();
        let healthy = PgLedger::new(db.store.clone(), [1; 32]);
        let request = specification();
        healthy.admit_request(&current, &request).await.unwrap();
        healthy.authorize_dispatch(request.id).await.unwrap();
        let proxy = pg_proxy::CommitProxy::new().await;
        let faulty = db.store_via_proxy(proxy.port).await;
        let ledger = PgLedger::new(faulty.clone(), [1; 32]);
        let mut final_usage = observation();
        if bound {
            final_usage.usage.as_mut().unwrap().input_tokens = 5000;
        } else {
            final_usage.usage = None;
        }
        let metric = if bound {
            Metric::AlarmBound
        } else {
            Metric::AlarmUnknown
        };
        let before = telemetry::get(metric);
        sqlx::raw_sql("CREATE FUNCTION gw_test_alarm_failure() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic alarm write failure'; END $$; CREATE TRIGGER gw_test_alarm_failure BEFORE UPDATE ON gw_alarm_totals FOR EACH ROW EXECUTE FUNCTION gw_test_alarm_failure()")
            .execute(db.store.pool()).await.unwrap();
        assert!(
            healthy
                .finalize_request(request.id, &final_usage)
                .await
                .is_err()
        );
        let mut barrier = db.store.pool().begin().await.unwrap();
        sqlx::query("SELECT id FROM gw_principals WHERE id=$1 FOR UPDATE")
            .bind(user)
            .execute(&mut *barrier)
            .await
            .unwrap();
        barrier.rollback().await.unwrap();
        let unchanged:bool=sqlx::query_scalar("SELECT r.phase='dispatching' AND NOT r.safety_violation_recorded AND NOT EXISTS(SELECT 1 FROM gw_usage_events WHERE request_id=r.id) AND NOT EXISTS(SELECT 1 FROM gw_audit_events WHERE request_id=r.id) AND NOT EXISTS(SELECT 1 FROM gw_alarm_totals WHERE total<>0) FROM gw_requests r WHERE id=$1").bind(request.id).fetch_one(db.store.pool()).await.unwrap();
        assert!(unchanged, "경보 저장실패는 금융/안전/감사와 함께 rollback");
        sqlx::query("DROP TRIGGER gw_test_alarm_failure ON gw_alarm_totals")
            .execute(db.store.pool())
            .await
            .unwrap();

        proxy.armed.store(true, Ordering::SeqCst);
        let lost = ledger.finalize_request(request.id, &final_usage).await;
        tokio::time::timeout(Duration::from_secs(2), proxy.committed.notified())
            .await
            .unwrap();
        faulty.pool().close().await;
        drop(ledger);
        drop(proxy);
        // 같은 DB의 새 ledger와 replay/reaper는 송신권한 없이 durable 사실만 관찰한다.
        let reconnected = db.store_via_proxy(64603).await;
        let rebuilt = PgLedger::new(reconnected.clone(), [1; 32]);
        let replay = rebuilt.finalize_request(request.id, &final_usage).await;
        let runtime = m_gated::runtime::Runtime::new(cfg.limits().clone()).unwrap();
        rebuilt.recovery_tick(&runtime).await.unwrap();
        rebuilt.recovery_tick(&runtime).await.unwrap();
        let (writer, mut reader) = UnixStream::pair().unwrap();
        Collector::from_fd(OwnedFd::from(writer)).unwrap().publish();
        let mut bytes = Vec::new();
        let mut byte = [0];
        while reader.read_exact(&mut byte).is_ok() {
            bytes.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        let collector: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let alarm = collector["telemetry"][if bound {
            "alarm_pricing_bound_total"
        } else {
            "alarm_new_unknown_total"
        }]
        .as_u64()
        .unwrap();
        let row=sqlx::query("SELECT accounting_state,cost_usd::text AS cost,execution_stopped_at IS NULL AS stopped_unproven,safety_violation_recorded,(SELECT count(*) FROM gw_usage_events WHERE request_id=r.id AND kind='final') AS finals,(SELECT count(*) FROM gw_audit_events WHERE request_id=r.id AND actor_kind='system') AS audits FROM gw_requests r WHERE id=$1").bind(request.id).fetch_one(db.store.pool()).await.unwrap();
        let no_permit = rebuilt.authorize_dispatch(request.id).await.err();
        reconnected.pool().close().await;
        db.close().await;
        assert!(lost.is_err());
        assert_eq!(replay.unwrap(), Finalized::Noop);
        assert_eq!(
            alarm,
            before + 1,
            "COMMIT ACK 유실 뒤 durable 경보 누락/중복"
        );
        assert_eq!(no_permit, Some(LedgerError::Conflict));
        assert_eq!(
            row.get::<String, _>("accounting_state"),
            if bound { "settled" } else { "unknown" }
        );
        assert!(row.get::<bool, _>("stopped_unproven"));
        assert_eq!(row.get::<bool, _>("safety_violation_recorded"), bound);
        assert_eq!(row.get::<i64, _>("audits"), i64::from(bound));
        assert_eq!(row.get::<i64, _>("finals"), i64::from(bound));
        assert_eq!(runtime.active_requests(), 0);
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn pool_initialization_failure_cleans_only_owned_schema_and_marker() {
    use futures_util::FutureExt;
    use sqlx::Connection;
    let schema = format!("gw_test_{}", uuid::Uuid::new_v4().simple());
    let failed = std::panic::AssertUnwindSafe(postgres::TestDb::injected_pool_connect_failure(
        schema.clone(),
    ))
    .catch_unwind()
    .await;
    let raw = std::env::var("TEST_DATABASE_URL").expect("명시 테스트 DB 필요");
    let options = postgres::test_options(&raw).unwrap();
    let mut connection = sqlx::PgConnection::connect_with(&options).await.unwrap();
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname=$1)")
            .bind(&schema)
            .fetch_one(&mut connection)
            .await
            .unwrap();
    let marker = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-owned-schemas")
        .join(&schema);
    let marker_exists = marker.exists();
    // Red도 다른 자원을 남기지 않는다. 이 시험이 만든 정확한 UUID/소유 marker만 정리한다.
    if exists {
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "m_gated_test/127.0.0.1:64603\n"
        );
        sqlx::raw_sql("SET lock_timeout='500ms'; SET statement_timeout='2s'")
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&mut connection)
            .await
            .unwrap();
        for _ in 0..2 {
            let absent: bool = sqlx::query_scalar(
                "SELECT NOT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname=$1)",
            )
            .bind(&schema)
            .fetch_one(&mut connection)
            .await
            .unwrap();
            assert!(absent);
        }
        std::fs::remove_file(&marker).unwrap();
    }
    connection.close().await.unwrap();
    assert!(failed.is_err(), "주입 실패는 초기화 성공이 아니다");
    assert!(
        !exists && !marker_exists,
        "pool 초기화 실패도 생성한 자기 schema와 marker를 정리해야 한다"
    );
    let message = failed
        .as_ref()
        .err()
        .and_then(|payload| payload.downcast_ref::<String>());
    assert_eq!(
        message.map(String::as_str),
        Some("TEST_POOL_INITIALIZATION_FAILED code=pool_timeout owned_cleanup=true")
    );
}
