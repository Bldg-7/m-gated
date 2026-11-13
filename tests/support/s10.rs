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
    let cfg = configuration();
    let principal = finance::human(db.store.pool()).await;
    let current = finance::current(db.store.pool(), principal).await;
    let ledger = Arc::new(PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&cfg),
        [1; 32],
    ));
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
    let database = db.clock_store(None).await;
    let cfg = configuration_from_yaml(&configuration_yaml_for_s10());
    let settings = AccessSettings::from_config(&cfg);
    let principal = finance::human(database.pool()).await;
    let current = finance::current(database.pool(), principal).await;
    let ledger = PgLedger::new(database.clone(), settings.clone(), [1; 32]);
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
    let owner = uuid::Uuid::from_u128(1);
    let service = uuid::Uuid::from_u128(2);
    sqlx::query("INSERT INTO gw_principals(id,kind,issuer,subject,email,display_name) VALUES($1,'human','https://issuer.example','owner','alice@example.test','Owner')").bind(owner).execute(db.store.pool()).await.unwrap();
    sqlx::query("INSERT INTO gw_principals(id,kind,display_name,owner_principal_id) VALUES($1,'service','Service',$2)").bind(service).bind(owner).execute(db.store.pool()).await.unwrap();
    sqlx::query("INSERT INTO gw_policies(id,principal_id,allow_rules,budget_mode,period_kind,period_timezone,updated_by) VALUES($1,$2,$3,'actual','calendar_month','UTC',$4)").bind(uuid::Uuid::new_v4()).bind(service).bind(serde_json::json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages","count_tokens"]}])).bind(owner).execute(db.store.pool()).await.unwrap();
    let ledger = Arc::new(PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    ));
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
    let database = db.clock_store(None).await;
    let cfg = configuration_from_yaml(&configuration_yaml_for_s10());
    let settings = AccessSettings::from_config(&cfg);
    let principal = finance::human(database.pool()).await;
    let current = finance::current(database.pool(), principal).await;
    let ledger = PgLedger::new(database.clone(), settings.clone(), [1; 32]);
    let grants = serde_json::json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["count_tokens","messages"]}]);
    let ctx = CommandContext {
        actor: principal,
        command_id: uuid::Uuid::new_v4(),
        method: "POST",
        path: "/gateway/keys",
        settings: &settings,
        grants: &grants,
        digest: [1; 32],
        ceiling: cfg.execution_ceiling(),
        execution_running: false,
    };
    let issue:IssueKey=serde_json::from_value(serde_json::json!({"name":"보관 검증 키","validity":"unlimited","grants":grants,"limits":{"budget":null,"rpm":null,"tpm":null,"count_rpm":null}})).unwrap();
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
        path: "/gateway/admin/requests/settlement",
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
    let before_restart = handoff_filtered_http(&cfg, database.clone(), principal).await;
    database.pool().close().await;
    db.store.pool().close().await;
    let restarted = db.store_via_proxy(64603).await;
    let after_restart = handoff_filtered_http(&cfg, restarted.clone(), principal).await;
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
    let database = db.clock_store(None).await;
    let cfg = configuration_from_yaml(&configuration_yaml_for_s10());
    let settings = AccessSettings::from_config(&cfg);
    let principal = finance::human(database.pool()).await;
    let current = finance::current(database.pool(), principal).await;
    let ledger = PgLedger::new(database.clone(), settings.clone(), [1; 32]);
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
        path: "/gateway/admin/principals/inference-pause/release",
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

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn handoff_import_is_atomic_idempotent_and_rollback_keeps_unknown_reservation_and_safety() {
    use m_gated::ledger::commands::CommandContext;
    let db = postgres::TestDb::new().await;
    let database = db.clock_store(None).await;
    sqlx::query("UPDATE gw_fixture_clock SET observed='2025-07-01T00:00:00Z'")
        .execute(database.pool())
        .await
        .unwrap();
    let cfg = configuration_from_yaml(&configuration_yaml_for_s10());
    let settings = AccessSettings::from_config(&cfg);
    let actor = finance::human(database.pool()).await;
    let target = finance::human(database.pool()).await;
    let ledger = PgLedger::new(database.clone(), settings.clone(), [1; 32]);
    let grants = serde_json::json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["count_tokens","messages"]}]);
    let limits = serde_json::json!({"budget":{"mode":"reserved","limit_usd":"10.000000000000","period":{"kind":"calendar_month","timezone":"UTC"}},"rpm":null,"tpm":5000,"count_rpm":null});
    let ctx = CommandContext {
        actor,
        command_id: uuid::Uuid::new_v4(),
        method: "MAINTENANCE",
        path: "handoff",
        settings: &settings,
        grants: &grants,
        digest: [1; 32],
        ceiling: cfg.execution_ceiling(),
        execution_running: false,
    };
    let source = uuid::Uuid::new_v4();
    let scope = uuid::Uuid::new_v4();
    let fence = serde_json::json!({"action":"fence","target_id":target,"expected_version":"1","source_id":source,"grants":[],"limits":{"budget":null,"rpm":null,"tpm":null,"count_rpm":null}});
    ledger
        .handoff(&ctx, serde_json::from_value(fence).unwrap())
        .await
        .unwrap();
    let baseline = serde_json::json!({"action":"import","target_id":target,"expected_version":"2","source_id":source,"source_ref":"synthetic-old-ledger-v1","previous_writer_fenced_ref":"synthetic-old-ingress-blocked","previous_executors_stopped_ref":"synthetic-old-process-joined","cutoff":"2025-04-01T00:00:00Z","bundle":{
      "identity":{"kind":"human","issuer":"https://issuer.example","subject":target.to_string(),"owner_principal_id":null},"original_disabled":false,"source_principal_version":"17",
      "safety":{"inference_paused_at":null,"inference_pause_reason":null,"inference_pause_request_id":null,"inference_safety_version":"4"},
      "keys":[],"scopes":[{"id":scope,"key_id":null,"version":"7","grants":grants,"limits":limits,"budget_mode":"reserved","period_kind":"calendar_month","period_timezone":"UTC","created_at":"2025-03-01T00:00:00Z","updated_at":"2025-03-01T00:00:00Z"}],
      "periods":[{"scope_id":scope,"period_start":"2025-03-01T00:00:00Z","period_end":"2025-04-01T00:00:00Z","timezone_snapshot":"UTC","cost_usd":"3.000000000000","reserved_usd":"0.000000000000","carried_cost_usd":"3.000000000000","unknown_count":"0","settled_count":"3","input_tokens":"10","output_tokens":"20","carried_settled_count":"3","carried_input_tokens":"10","carried_output_tokens":"20","settled_unknown_tokens_count":"1","carried_settled_unknown_tokens_count":"1","detail_deleted_through":null}],"windows":[],"requests":[],"budgets":[],"events":[]}});
    // source bundle 밖의 기존 actor scope를 참조한 rate 행은 target 이전으로 위장해 쓸 수 없다.
    let unrelated_target = finance::human(database.pool()).await;
    let unrelated_source = uuid::Uuid::new_v4();
    let unrelated_scope = uuid::Uuid::new_v4();
    ledger.handoff(&CommandContext{command_id:uuid::Uuid::new_v4(),..ctx},serde_json::from_value(serde_json::json!({"action":"fence","target_id":unrelated_target,"expected_version":"1","source_id":unrelated_source,"grants":[],"limits":{"budget":null,"rpm":null,"tpm":null,"count_rpm":null}})).unwrap()).await.unwrap();
    let actor_scope: uuid::Uuid =
        sqlx::query_scalar("SELECT id FROM gw_policies WHERE principal_id=$1 AND key_id IS NULL")
            .bind(actor)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let mut foreign = baseline.clone();
    foreign["target_id"] = serde_json::json!(unrelated_target);
    foreign["source_id"] = serde_json::json!(unrelated_source);
    foreign["bundle"]["identity"]["subject"] = serde_json::json!(unrelated_target.to_string());
    foreign["bundle"]["scopes"][0]["id"] = serde_json::json!(unrelated_scope);
    foreign["bundle"]["periods"][0]["scope_id"] = serde_json::json!(unrelated_scope);
    foreign["bundle"]["windows"] = serde_json::json!([{"scope_id":actor_scope,"lane":"inference","window_start":"2025-03-01T00:00:00Z","window_end":"2025-03-01T00:01:00Z","admitted_count":"50","reserved_tokens":"0","settled_tokens":"0"}]);
    let foreign_rejected = ledger
        .handoff(
            &CommandContext {
                command_id: uuid::Uuid::new_v4(),
                ..ctx
            },
            serde_json::from_value(foreign).unwrap(),
        )
        .await
        .is_err();
    let foreign_written: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gw_rate_windows WHERE scope_id=$1")
            .bind(actor_scope)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let import_ctx = CommandContext {
        command_id: uuid::Uuid::new_v4(),
        ..ctx
    };
    let mut wrong = baseline.clone();
    wrong["bundle"]["identity"]["subject"] = serde_json::json!("different-sub-same-email");
    let refused = ledger
        .handoff(&import_ctx, serde_json::from_value(wrong).unwrap())
        .await
        .is_err();
    sqlx::raw_sql("CREATE FUNCTION gw_fixture_fail_handoff_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.operation='handoff' THEN RAISE EXCEPTION 'synthetic handoff audit fault'; END IF; RETURN NEW; END $$; CREATE TRIGGER gw_fixture_fail_handoff_audit BEFORE INSERT ON gw_audit_events FOR EACH ROW EXECUTE FUNCTION gw_fixture_fail_handoff_audit()").execute(database.pool()).await.unwrap();
    let rolled_back = ledger
        .handoff(
            &import_ctx,
            serde_json::from_value(baseline.clone()).unwrap(),
        )
        .await
        .is_err();
    let before:i64=sqlx::query_scalar("SELECT count(*) FROM gw_usage_totals t JOIN gw_policies p ON p.id=t.scope_id WHERE p.principal_id=$1").bind(target).fetch_one(database.pool()).await.unwrap();
    sqlx::query("DROP TRIGGER gw_fixture_fail_handoff_audit ON gw_audit_events")
        .execute(database.pool())
        .await
        .unwrap();
    let imported = ledger
        .handoff(
            &import_ctx,
            serde_json::from_value(baseline.clone()).unwrap(),
        )
        .await
        .unwrap();
    let available_baseline: Vec<i64> =
        sqlx::query_scalar("SELECT total FROM gw_alarm_totals ORDER BY code")
            .fetch_all(database.pool())
            .await
            .unwrap();
    assert_eq!(
        available_baseline,
        vec![0, 0],
        "carried 금액/삭제된 상세/안전 version4만으로 과거 경보를 추정하지 않는다"
    );
    let replay = ledger
        .handoff(
            &import_ctx,
            serde_json::from_value(baseline.clone()).unwrap(),
        )
        .await
        .unwrap();
    let new_ctx = CommandContext {
        command_id: uuid::Uuid::new_v4(),
        ..import_ctx
    };
    let mut repeated = baseline.clone();
    repeated["expected_version"] = serde_json::json!("999");
    let noop = ledger
        .handoff(&new_ctx, serde_json::from_value(repeated).unwrap())
        .await
        .unwrap();
    let activate = serde_json::json!({"action":"activate","target_id":target,"expected_version":"18","source_id":source,"previous_executors_stopped_ref":"synthetic-old-process-joined"});
    let active = ledger
        .handoff(
            &CommandContext {
                command_id: uuid::Uuid::new_v4(),
                ..ctx
            },
            serde_json::from_value(activate).unwrap(),
        )
        .await
        .unwrap();
    let current = finance::current(database.pool(), target).await;
    let request = s4::strict_specification();
    ledger.admit_request(&current, &request).await.unwrap();
    s4::complete_count(&ledger, &current, &request).await;
    ledger.authorize_dispatch(request.id).await.unwrap();
    ledger
        .finalize_request(
            request.id,
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
    let mut late = observation();
    late.usage.as_mut().unwrap().input_tokens = 5000;
    ledger.finalize_request(request.id, &late).await.unwrap();
    let policy_version: String =
        sqlx::query_scalar("SELECT version::text FROM gw_policies WHERE id=$1")
            .bind(scope)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let fence = serde_json::json!({"action":"fence","target_id":target,"expected_version":policy_version,"source_id":source,"grants":grants,"limits":limits});
    ledger
        .handoff(
            &CommandContext {
                command_id: uuid::Uuid::new_v4(),
                ..ctx
            },
            serde_json::from_value(fence).unwrap(),
        )
        .await
        .unwrap();
    let current_version: String =
        sqlx::query_scalar("SELECT version::text FROM gw_principals WHERE id=$1")
            .bind(target)
            .fetch_one(database.pool())
            .await
            .unwrap();
    let mut rollback = serde_json::json!({"action":"rollback","target_id":target,"expected_version":current_version,"source_id":source,"new_executors_stopped_ref":"synthetic-new-process-joined","legacy_represents_all_state":false,"legacy_all_credentials_blocked":false});
    let not_proven = ledger
        .handoff(
            &CommandContext {
                command_id: uuid::Uuid::new_v4(),
                ..ctx
            },
            serde_json::from_value(rollback.clone()).unwrap(),
        )
        .await
        .is_err();
    rollback["legacy_all_credentials_blocked"] = serde_json::json!(true);
    let rollback_result = ledger
        .handoff(
            &CommandContext {
                command_id: uuid::Uuid::new_v4(),
                ..ctx
            },
            serde_json::from_value(rollback).unwrap(),
        )
        .await
        .unwrap();
    let held=sqlx::query("SELECT r.accounting_state,r.cost_usd IS NULL AS unresolved,b.remaining_reservation_usd::text AS reserved,b.remaining_reserved_tokens,p.inference_pause_request_id,p.inference_safety_version,p.disabled_at IS NOT NULL AS blocked,(SELECT cost_usd::text FROM gw_usage_totals WHERE scope_id=$2 AND period_start='2025-03-01T00:00:00Z') AS baseline FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id JOIN gw_principals p ON p.id=r.principal_id WHERE r.id=$1").bind(request.id).bind(scope).fetch_one(database.pool()).await.unwrap();
    database.pool().close().await;
    db.close().await;
    assert!(
        foreign_rejected && foreign_written == 0,
        "source 밖의 기존 scope 쓰기를 허용하면 안 된다"
    );
    assert!(refused && rolled_back && before == 0);
    assert_eq!(imported["state"], "imported");
    assert_eq!(replay["ack"]["replayed"], true);
    assert_eq!(noop["ack"]["effect"], "noop");
    assert_eq!(active["state"], "active");
    assert!(not_proven);
    assert_eq!(rollback_result["state"], "rollback_blocked");
    assert!(held.get::<bool, _>("unresolved") && held.get::<bool, _>("blocked"));
    assert_eq!(held.get::<String, _>("accounting_state"), "unknown");
    assert_eq!(held.get::<String, _>("reserved"), "0.004296000000");
    assert_eq!(held.get::<i64, _>("remaining_reserved_tokens"), 4196);
    assert_eq!(
        held.get::<Option<uuid::Uuid>, _>("inference_pause_request_id"),
        Some(request.id)
    );
    assert_eq!(held.get::<i64, _>("inference_safety_version"), 5);
    assert_eq!(held.get::<String, _>("baseline"), "3.000000000000");
}

async fn handoff_fixture_rows(
    pool: &sqlx::PgPool,
    table: &str,
    filter: &str,
    principal: uuid::Uuid,
    remove: &[&str],
    strings: &[&str],
) -> Vec<serde_json::Value> {
    let mut projection = "to_jsonb(t)".to_owned();
    for key in remove {
        projection += &format!("-'{key}'");
    }
    if !strings.is_empty() {
        projection += "||jsonb_build_object(";
        projection += &strings
            .iter()
            .map(|k| format!("'{k}',t.{k}::text"))
            .collect::<Vec<_>>()
            .join(",");
        projection += ")";
    }
    sqlx::query_scalar(&format!(
        "SELECT {projection} FROM {table} t WHERE {filter} LIMIT 401"
    ))
    .bind(principal)
    .fetch_all(pool)
    .await
    .unwrap()
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn handoff_imports_real_source_unknown_key_reservations_and_never_inherits_old_secret() {
    handoff_unknown_key_fixture(None, false).await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL; isolated process telemetry counters"]
async fn telemetry_handoff_import_baseline_is_atomic_once_through_ack_loss_and_reassembly() {
    for lose_ack in [false, true] {
        handoff_unknown_key_fixture(Some(lose_ack), false).await;
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL; isolated process telemetry counters"]
async fn telemetry_handoff_retained_settlement_and_released_safety_keep_baseline_not_active_gauges()
{
    handoff_unknown_key_fixture(Some(false), true).await;
}

async fn handoff_unknown_key_fixture(alarm_fault: Option<bool>, historical: bool) {
    use m_gated::ledger::{
        commands::CommandContext,
        finance::KeyScope,
        management::{Command, IssueKey},
    };
    let old = postgres::TestDb::new().await;
    let old_clock = old.clock_store(None).await;
    let new = postgres::TestDb::new().await;
    let new_clock = new.clock_store(None).await;
    sqlx::query("UPDATE gw_fixture_clock SET observed='2027-01-01T00:00:00Z'")
        .execute(new_clock.pool())
        .await
        .unwrap();
    let cfg = configuration_from_yaml(&configuration_yaml_for_s10());
    let settings = AccessSettings::from_config(&cfg);
    let old_user = finance::human(old_clock.pool()).await;
    let old_admin = finance::human(old_clock.pool()).await;
    sqlx::query("UPDATE gw_policies SET budget_mode='reserved',budget_limit_usd=10,tpm_limit=5000 WHERE principal_id=$1").bind(old_user).execute(old_clock.pool()).await.unwrap();
    let old_ledger = PgLedger::new(old_clock.clone(), settings.clone(), [1; 32]);
    let grants = serde_json::json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["count_tokens","messages"]}]);
    let limits = serde_json::json!({"budget":{"mode":"reserved","limit_usd":"10.000000000000","period":{"kind":"calendar_month","timezone":"UTC"}},"rpm":null,"tpm":5000,"count_rpm":null});
    let old_ctx = CommandContext {
        actor: old_user,
        command_id: uuid::Uuid::new_v4(),
        method: "POST",
        path: "/gateway/keys",
        settings: &settings,
        grants: &grants,
        digest: [1; 32],
        ceiling: cfg.execution_ceiling(),
        execution_running: false,
    };
    let key_input:IssueKey=serde_json::from_value(serde_json::json!({"name":"이전 키","validity":"unlimited","grants":grants,"limits":limits})).unwrap();
    let issued = old_ledger
        .management_command(&old_ctx, Command::Issue(key_input))
        .await
        .unwrap()
        .body
        .unwrap();
    let old_secret = issued["key"].as_str().unwrap().to_owned();
    let key = KeyScope {
        id: issued["metadata"]["id"].as_str().unwrap().parse().unwrap(),
        version: 1,
    };
    let current = finance::current(old_clock.pool(), old_user).await;
    let request = s4::strict_specification();
    old_ledger
        .admit_scoped(&current, &request, Some(key), None)
        .await
        .unwrap();
    let count_cfg = configuration();
    let child = RequestSpec::from_profile(
        ProviderId::Anthropic,
        &count_cfg.providers()[0].profiles[1],
        None,
    );
    old_ledger
        .admit_scoped(&current, &child, Some(key), Some(request.id))
        .await
        .unwrap();
    old_ledger.authorize_dispatch(child.id).await.unwrap();
    old_ledger
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
    old_ledger.authorize_dispatch(request.id).await.unwrap();
    old_ledger
        .finalize_request(
            request.id,
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
    let mut late = observation();
    late.usage.as_mut().unwrap().input_tokens = 5000;
    old_ledger
        .finalize_request(request.id, &late)
        .await
        .unwrap();
    if historical {
        use m_gated::ledger::management::{ReleaseSafety, Settlement};
        let admin_ctx = CommandContext {
            actor: old_admin,
            ..old_ctx
        };
        let version: String =
            sqlx::query_scalar("SELECT version::text FROM gw_requests WHERE id=$1")
                .bind(request.id)
                .fetch_one(old_clock.pool())
                .await
                .unwrap();
        old_ledger
            .management_command(
                &CommandContext {
                    command_id: uuid::Uuid::new_v4(),
                    method: "PUT",
                    path: "/gateway/admin/requests/settlement",
                    ..admin_ctx
                },
                Command::Settle(
                    request.id,
                    Settlement {
                        expected_version: version,
                        decision: "charged".into(),
                        cost_usd: "0.001000000000".into(),
                        reason: "합성 이전 수동 처리".into(),
                        execution_stopped_confirmed: Some(true),
                    },
                ),
            )
            .await
            .unwrap();
        old_ledger
            .management_command(
                &CommandContext {
                    command_id: uuid::Uuid::new_v4(),
                    method: "POST",
                    path: "/gateway/admin/principals/inference-pause/release",
                    ..admin_ctx
                },
                Command::Release(
                    old_user,
                    ReleaseSafety {
                        expected_version: "2".into(),
                        reason: "합성 이전 명시 해제".into(),
                    },
                ),
            )
            .await
            .unwrap();
    }
    let source = uuid::Uuid::new_v4();
    let old_fence = serde_json::json!({"action":"fence","target_id":old_user,"expected_version":"1","source_id":source,"grants":grants,"limits":limits});
    old_ledger
        .handoff(
            &CommandContext {
                actor: old_admin,
                command_id: uuid::Uuid::new_v4(),
                method: "MAINTENANCE",
                path: "handoff",
                ..old_ctx
            },
            serde_json::from_value(old_fence).unwrap(),
        )
        .await
        .unwrap();
    let target = uuid::Uuid::new_v4();
    let actor = finance::human(new_clock.pool()).await;
    sqlx::query("INSERT INTO gw_principals(id,kind,issuer,subject,email,display_name) VALUES($1,'human','https://issuer.example',$2,'alice@example.test','Mapped source identity')").bind(target).bind(old_user.to_string()).execute(new_clock.pool()).await.unwrap();
    sqlx::query("INSERT INTO gw_policies(id,principal_id,allow_rules,budget_mode,period_kind,period_timezone,updated_by) VALUES($1,$2,'[]','actual','calendar_month','UTC',$2)").bind(uuid::Uuid::new_v4()).bind(target).execute(new_clock.pool()).await.unwrap();
    let ledger = PgLedger::new(new_clock.clone(), settings.clone(), [2; 32]);
    let ctx = CommandContext {
        actor,
        command_id: uuid::Uuid::new_v4(),
        method: "MAINTENANCE",
        path: "handoff",
        ..old_ctx
    };
    ledger.handoff(&ctx,serde_json::from_value(serde_json::json!({"action":"fence","target_id":target,"expected_version":"1","source_id":source,"grants":[],"limits":{"budget":null,"rpm":null,"tpm":null,"count_rpm":null}})).unwrap()).await.unwrap();
    let p = "t.principal_id=$1";
    let b = "t.request_id IN(SELECT id FROM gw_requests WHERE principal_id=$1)";
    let s = "t.scope_id IN(SELECT id FROM gw_policies WHERE principal_id=$1)";
    let keys:Vec<serde_json::Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'public_prefix',public_prefix,'name',name,'grant_snapshot',grant_snapshot,'validity_preset',validity_preset,'expires_at',expires_at,'revoked_at',revoked_at,'created_at',created_at,'version',version::text,'original_created_by',created_by,'original_created_command_id',created_command_id) FROM gw_virtual_keys WHERE principal_id=$1 LIMIT 33").bind(old_user).fetch_all(old_clock.pool()).await.unwrap();
    let scopes:Vec<serde_json::Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'key_id',key_id,'version',version::text,'grants',allow_rules,'budget_mode',budget_mode,'period_kind',period_kind,'period_timezone',period_timezone,'limits',jsonb_build_object('budget',CASE WHEN budget_limit_usd IS NULL THEN NULL ELSE jsonb_build_object('mode',budget_mode,'limit_usd',budget_limit_usd::text,'period',jsonb_build_object('kind',period_kind,'timezone',period_timezone)) END,'rpm',rpm_limit,'tpm',tpm_limit,'count_rpm',count_rpm_limit),'created_at',created_at,'updated_at',updated_at) FROM gw_policies WHERE principal_id=$1 LIMIT 34").bind(old_user).fetch_all(old_clock.pool()).await.unwrap();
    let periods = handoff_fixture_rows(
        old_clock.pool(),
        "gw_usage_totals",
        s,
        old_user,
        &["updated_at"],
        &[
            "cost_usd",
            "reserved_usd",
            "carried_cost_usd",
            "unknown_count",
            "settled_count",
            "input_tokens",
            "output_tokens",
            "carried_settled_count",
            "carried_input_tokens",
            "carried_output_tokens",
            "settled_unknown_tokens_count",
            "carried_settled_unknown_tokens_count",
        ],
    )
    .await;
    let windows = handoff_fixture_rows(
        old_clock.pool(),
        "gw_rate_windows",
        s,
        old_user,
        &["updated_at"],
        &["admitted_count", "reserved_tokens", "settled_tokens"],
    )
    .await;
    let requests = handoff_fixture_rows(
        old_clock.pool(),
        "gw_requests",
        p,
        old_user,
        &["principal_id", "phase"],
        &["cost_usd", "version"],
    )
    .await;
    let budgets = handoff_fixture_rows(
        old_clock.pool(),
        "gw_request_budgets",
        b,
        old_user,
        &[],
        &[
            "policy_version",
            "reservation_usd",
            "remaining_reservation_usd",
            "reserved_tokens",
            "remaining_reserved_tokens",
            "applied_cost_usd",
            "applied_input_tokens",
            "applied_output_tokens",
        ],
    )
    .await;
    let events = handoff_fixture_rows(
        old_clock.pool(),
        "gw_usage_events",
        b,
        old_user,
        &["id"],
        &[],
    )
    .await;
    let safety:serde_json::Value=sqlx::query_scalar("SELECT jsonb_build_object('inference_paused_at',inference_paused_at,'inference_pause_reason',inference_pause_reason,'inference_pause_request_id',inference_pause_request_id,'inference_safety_version',inference_safety_version::text) FROM gw_principals WHERE id=$1").bind(old_user).fetch_one(old_clock.pool()).await.unwrap();
    let cutoff: String = sqlx::query_scalar(
        "SELECT to_char(clock_timestamp() AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')",
    )
    .fetch_one(old.store.pool())
    .await
    .unwrap();
    let input = serde_json::json!({"action":"import","target_id":target,"expected_version":"2","source_id":source,"source_ref":"synthetic-two-ledgers","previous_writer_fenced_ref":"actual-old-ledger-fence","previous_executors_stopped_ref":"synthetic-old-joined","cutoff":cutoff,"bundle":{"identity":{"kind":"human","issuer":"https://issuer.example","subject":old_user.to_string(),"owner_principal_id":null},"original_disabled":false,"original_disabled_reason":null,"original_security_fence_ref":null,"source_principal_version":"2","safety":safety,"keys":keys,"scopes":scopes,"periods":periods,"windows":windows,"requests":requests,"budgets":budgets,"events":events}});
    let typed: m_gated::ledger::handoff::Handoff = serde_json::from_value(input.clone()).unwrap();
    let m_gated::ledger::handoff::Handoff::Import(parsed) = typed else {
        unreachable!()
    };
    let full_validation = parsed.validate();
    let full_valid = full_validation.is_ok();
    let import_ctx = CommandContext {
        command_id: uuid::Uuid::new_v4(),
        ..ctx
    };
    let alarm_totals = |pool: sqlx::PgPool| async move {
        sqlx::query_scalar::<_, i64>("SELECT total FROM gw_alarm_totals ORDER BY code")
            .fetch_all(&pool)
            .await
            .unwrap()
    };
    if alarm_fault.is_some() {
        assert_eq!(alarm_totals(new_clock.pool().clone()).await, vec![0, 0]);
        // 감사 실패와 고정 경보 행 갱신 실패 모두 금융/receipt와 함께 rollback한다.
        for table in ["gw_audit_events", "gw_alarm_totals"] {
            let event = if table == "gw_alarm_totals" {
                "UPDATE"
            } else {
                "INSERT"
            };
            sqlx::raw_sql(&format!("CREATE FUNCTION gw_fixture_import_failure() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic import failure'; END $$; CREATE TRIGGER gw_fixture_import_failure BEFORE {event} ON {table} FOR EACH ROW EXECUTE FUNCTION gw_fixture_import_failure()"))
                .execute(new_clock.pool()).await.unwrap();
            assert!(
                ledger
                    .handoff(&import_ctx, serde_json::from_value(input.clone()).unwrap())
                    .await
                    .is_err()
            );
            let mut barrier = new_clock.pool().begin().await.unwrap();
            sqlx::query("SELECT id FROM gw_principals WHERE id=$1 FOR UPDATE")
                .bind(target)
                .execute(&mut *barrier)
                .await
                .unwrap();
            barrier.rollback().await.unwrap();
            let unchanged: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM gw_requests WHERE principal_id=$1) AND NOT EXISTS(SELECT 1 FROM gw_virtual_keys WHERE principal_id=$1) AND NOT EXISTS(SELECT 1 FROM gw_handoffs WHERE principal_id=$1) AND NOT EXISTS(SELECT 1 FROM gw_commands WHERE actor_principal_id=$2 AND command_id=$3) AND NOT EXISTS(SELECT 1 FROM gw_audit_events WHERE actor_principal_id=$2 AND command_id=$3) AND NOT EXISTS(SELECT 1 FROM gw_alarm_totals WHERE total<>0) AND EXISTS(SELECT 1 FROM gw_principals WHERE id=$1 AND version=2 AND inference_pause_reason IS NULL AND inference_safety_version=1)").bind(target).bind(actor).bind(import_ctx.command_id).fetch_one(new_clock.pool()).await.unwrap();
            assert!(
                unchanged,
                "import 실패는 금융/보호/receipt/command/alarm 전체 rollback"
            );
            sqlx::raw_sql(&format!("DROP TRIGGER gw_fixture_import_failure ON {table}; DROP FUNCTION gw_fixture_import_failure()"))
                .execute(new_clock.pool()).await.unwrap();
        }
    }
    let imported = if alarm_fault == Some(true) {
        use std::sync::atomic::Ordering;
        let proxy = pg_proxy::CommitProxy::new().await;
        let faulty = new.clock_store(Some(proxy.port)).await;
        let faulty_ledger = PgLedger::new(faulty.clone(), settings.clone(), [2; 32]);
        proxy.armed.store(true, Ordering::SeqCst);
        let lost = faulty_ledger
            .handoff(&import_ctx, serde_json::from_value(input.clone()).unwrap())
            .await;
        tokio::time::timeout(Duration::from_secs(2), proxy.committed.notified())
            .await
            .unwrap();
        assert!(lost.is_err(), "실제 COMMIT 성공 후 ACK만 유실");
        faulty.pool().close().await;
        drop(faulty_ledger);
        drop(proxy);
        // 정상 pool의 replay는 새 금융 효과가 아니며 최초 command 결과를 반환한다.
        let replay = ledger
            .handoff(&import_ctx, serde_json::from_value(input.clone()).unwrap())
            .await;
        assert_eq!(replay.as_ref().unwrap()["ack"]["replayed"], true);
        replay
    } else {
        ledger
            .handoff(&import_ctx, serde_json::from_value(input.clone()).unwrap())
            .await
    };
    if alarm_fault.is_some() {
        assert!(imported.is_ok(), "baseline 검증 전 실제 import 성공 전제");
        let preserved: bool = if historical {
            sqlx::query_scalar("SELECT r.accounting_state='settled' AND r.cost_usd=0.001 AND r.safety_violation_recorded AND p.inference_pause_request_id IS NULL AND p.inference_safety_version=3 AND (SELECT count(*) FROM gw_usage_events WHERE request_id=r.id AND kind IN ('settlement','late_final'))=2 AND (SELECT count(*) FROM gw_request_budgets WHERE request_id=r.id AND remaining_reservation_usd=0 AND remaining_reserved_tokens=0)=2 AND EXISTS(SELECT 1 FROM gw_handoffs WHERE principal_id=p.id) FROM gw_requests r JOIN gw_principals p ON p.id=r.principal_id WHERE r.id=$1").bind(request.id).fetch_one(new_clock.pool()).await.unwrap()
        } else {
            sqlx::query_scalar("SELECT r.accounting_state='unknown' AND r.cost_usd IS NULL AND r.safety_violation_recorded AND p.inference_pause_request_id=r.id AND p.inference_safety_version=2 AND (SELECT count(*) FROM gw_request_budgets WHERE request_id=r.id AND remaining_reservation_usd=0.004296 AND remaining_reserved_tokens=4196)=2 AND EXISTS(SELECT 1 FROM gw_handoffs WHERE principal_id=p.id) FROM gw_requests r JOIN gw_principals p ON p.id=r.principal_id WHERE r.id=$1").bind(request.id).fetch_one(new_clock.pool()).await.unwrap()
        };
        assert!(
            preserved,
            "baseline 전 unknown/두 scope 예약/안전 원인/receipt 실제 보존 전제"
        );
        assert_eq!(
            alarm_totals(new_clock.pool().clone()).await,
            vec![1, 1],
            "scope2개인 unknown request/안전 원인은 각각 한번만 import"
        );
        let replay = ledger
            .handoff(&import_ctx, serde_json::from_value(input.clone()).unwrap())
            .await
            .unwrap();
        assert_eq!(replay["ack"]["replayed"], true);
        let noop = ledger
            .handoff(
                &CommandContext {
                    command_id: uuid::Uuid::new_v4(),
                    ..ctx
                },
                serde_json::from_value(input.clone()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(noop["ack"]["effect"], "noop");
        let mut conflict = input.clone();
        conflict["source_ref"] = serde_json::json!("different-source-digest");
        assert!(matches!(
            ledger
                .handoff(
                    &CommandContext {
                        command_id: uuid::Uuid::new_v4(),
                        ..ctx
                    },
                    serde_json::from_value(conflict).unwrap()
                )
                .await,
            Err(m_gated::ledger::management::ManagementError::CommandConflict)
        ));
        let rebuilt_store = new.store_via_proxy(64603).await;
        let rebuilt = PgLedger::new(rebuilt_store.clone(), settings.clone(), [2; 32]);
        let runtime = m_gated::runtime::Runtime::new(cfg.limits().clone()).unwrap();
        for _ in 0..2 {
            rebuilt.recovery_tick(&runtime).await.unwrap();
        }
        use std::{
            io::Read,
            os::{fd::OwnedFd, unix::net::UnixStream},
        };
        let (writer, mut reader) = UnixStream::pair().unwrap();
        m_gated::telemetry::Collector::from_fd(OwnedFd::from(writer))
            .unwrap()
            .publish();
        let mut bytes = Vec::new();
        let mut byte = [0];
        while reader.read_exact(&mut byte).is_ok() {
            bytes.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        let collector: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(collector["telemetry"]["alarm_new_unknown_total"], 1);
        assert_eq!(collector["telemetry"]["alarm_pricing_bound_total"], 1);
        assert_eq!(
            collector["telemetry"]["unknown_requests"],
            if historical { 0 } else { 1 }
        );
        assert_eq!(alarm_totals(rebuilt_store.pool().clone()).await, vec![1, 1]);
        let audits: i64 =
            sqlx::query_scalar("SELECT count(*) FROM gw_audit_events WHERE command_id=$1")
                .bind(import_ctx.command_id)
                .fetch_one(rebuilt_store.pool())
                .await
                .unwrap();
        assert_eq!(audits, 1);
        assert_eq!(
            rebuilt.authorize_dispatch(request.id).await.err(),
            Some(LedgerError::Conflict)
        );
        rebuilt_store.pool().close().await;
    }
    let import_error = match imported.as_ref().err() {
        None => "none",
        Some(m_gated::ledger::management::ManagementError::Invalid) => "invalid",
        Some(m_gated::ledger::management::ManagementError::Invariant) => "invariant",
        Some(m_gated::ledger::management::ManagementError::Database) => "database",
        Some(m_gated::ledger::management::ManagementError::Forbidden) => "forbidden",
        Some(m_gated::ledger::management::ManagementError::SecurityFence) => "fence",
        Some(m_gated::ledger::management::ManagementError::Query(_)) => "query",
        Some(_) => "other",
    };
    let old_key_rejected = new_clock
        .authenticate_key(&old_secret, &settings)
        .await
        .is_err();
    let row=sqlx::query("SELECT r.accounting_state,r.cost_usd IS NULL AS unknown,p.inference_pause_request_id,p.inference_safety_version,(SELECT count(*) FROM gw_request_budgets WHERE request_id=r.id AND remaining_reservation_usd=0.004296 AND remaining_reserved_tokens=4196)::bigint AS held,(SELECT count(*) FROM gw_virtual_keys WHERE principal_id=p.id AND revoked_at IS NOT NULL)::bigint AS revoked FROM gw_requests r JOIN gw_principals p ON p.id=r.principal_id WHERE r.id=$1").bind(request.id).fetch_optional(new_clock.pool()).await.unwrap();
    old_clock.pool().close().await;
    new_clock.pool().close().await;
    old.close().await;
    new.close().await;
    assert!(full_valid, "typed source 검증: {full_validation:?}");
    assert!(
        imported.is_ok(),
        "정본 source의 보존된 금융 metadata는 import돼야 한다: {import_error}"
    );
    let row = row.unwrap();
    assert!(old_key_rejected);
    assert_eq!(row.get::<bool, _>("unknown"), !historical);
    assert_eq!(
        row.get::<String, _>("accounting_state"),
        if historical { "settled" } else { "unknown" }
    );
    assert_eq!(row.get::<i64, _>("held"), if historical { 0 } else { 2 });
    assert_eq!(row.get::<i64, _>("revoked"), 1);
    assert_eq!(
        row.get::<i64, _>("inference_safety_version"),
        if historical { 3 } else { 2 }
    );
    assert_eq!(
        row.get::<Option<uuid::Uuid>, _>("inference_pause_request_id"),
        if historical { None } else { Some(request.id) }
    );
}

// 제품과 독립된 이전 입력 fixture. 인증값·대화 본문은 내보내지 않는다.
async fn export_handoff_fixture(pool: &sqlx::PgPool, principal: uuid::Uuid) -> serde_json::Value {
    let p = "t.principal_id=$1";
    let s = "t.scope_id IN(SELECT id FROM gw_policies WHERE principal_id=$1)";
    let b = "t.request_id IN(SELECT id FROM gw_requests WHERE principal_id=$1)";
    let scopes:Vec<serde_json::Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'key_id',key_id,'version',version::text,'grants',allow_rules,'budget_mode',budget_mode,'period_kind',period_kind,'period_timezone',period_timezone,'limits',jsonb_build_object('budget',CASE WHEN budget_limit_usd IS NULL THEN NULL ELSE jsonb_build_object('mode',budget_mode,'limit_usd',budget_limit_usd::text,'period',jsonb_build_object('kind',period_kind,'timezone',period_timezone)) END,'rpm',rpm_limit,'tpm',tpm_limit,'count_rpm',count_rpm_limit),'created_at',created_at,'updated_at',updated_at) FROM gw_policies WHERE principal_id=$1 LIMIT 34").bind(principal).fetch_all(pool).await.unwrap();
    let periods = handoff_fixture_rows(
        pool,
        "gw_usage_totals",
        s,
        principal,
        &["updated_at"],
        &[
            "cost_usd",
            "reserved_usd",
            "carried_cost_usd",
            "unknown_count",
            "settled_count",
            "input_tokens",
            "output_tokens",
            "carried_settled_count",
            "carried_input_tokens",
            "carried_output_tokens",
            "settled_unknown_tokens_count",
            "carried_settled_unknown_tokens_count",
        ],
    )
    .await;
    let windows = handoff_fixture_rows(
        pool,
        "gw_rate_windows",
        s,
        principal,
        &["updated_at"],
        &["admitted_count", "reserved_tokens", "settled_tokens"],
    )
    .await;
    let requests = handoff_fixture_rows(
        pool,
        "gw_requests",
        p,
        principal,
        &["principal_id", "phase"],
        &["cost_usd", "version"],
    )
    .await;
    let budgets = handoff_fixture_rows(
        pool,
        "gw_request_budgets",
        b,
        principal,
        &[],
        &[
            "policy_version",
            "reservation_usd",
            "remaining_reservation_usd",
            "reserved_tokens",
            "remaining_reserved_tokens",
            "applied_cost_usd",
            "applied_input_tokens",
            "applied_output_tokens",
        ],
    )
    .await;
    let events = handoff_fixture_rows(pool, "gw_usage_events", b, principal, &["id"], &[]).await;
    serde_json::json!({"identity":{"kind":"human","issuer":"https://issuer.example","subject":principal.to_string(),"owner_principal_id":null},"original_disabled":false,"original_disabled_reason":null,"original_security_fence_ref":null,"source_principal_version":"1","safety":{"inference_paused_at":null,"inference_pause_reason":null,"inference_pause_request_id":null,"inference_safety_version":"1"},"keys":[],"scopes":scopes,"periods":periods,"windows":windows,"requests":requests,"budgets":budgets,"events":events})
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn handoff_preserves_budgetless_non_utc_scope_after_real_policy_removal() {
    handoff_detail_fixture(true, false, false).await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn handoff_full_detail_without_carried_keeps_null_boundary_and_filtered_usage() {
    handoff_detail_fixture(false, false, false).await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn handoff_carried_only_including_zero_cost_keeps_conservative_boundary_and_http409() {
    for zero in [false, true] {
        handoff_detail_fixture(false, true, zero).await;
    }
}
async fn handoff_detail_fixture(remove_budget: bool, carried: bool, zero: bool) {
    use m_gated::ledger::{commands::CommandContext, management::Command};
    let old = postgres::TestDb::new().await;
    let old_clock = old.clock_store(None).await;
    sqlx::query("UPDATE gw_fixture_clock SET observed='2025-03-15T00:00:00Z'")
        .execute(old_clock.pool())
        .await
        .unwrap();
    let new = postgres::TestDb::new().await;
    let cfg = configuration_from_yaml(&configuration_yaml_for_s10());
    let settings = AccessSettings::from_config(&cfg);
    let owner = finance::human(old_clock.pool()).await;
    let admin = finance::human(old_clock.pool()).await;
    let old_ledger = PgLedger::new(old_clock.clone(), settings.clone(), [1; 32]);
    let grants = serde_json::json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["count_tokens","messages"]}]);
    let limits = serde_json::json!({"budget":{"mode":"reserved","limit_usd":"10.000000000000","period":{"kind":"calendar_month","timezone":if remove_budget {"Asia/Seoul"} else {"UTC"}}},"rpm":null,"tpm":null,"count_rpm":null});
    let ctx = CommandContext {
        actor: admin,
        command_id: uuid::Uuid::new_v4(),
        method: "PUT",
        path: "policy",
        settings: &settings,
        grants: &grants,
        digest: [1; 32],
        ceiling: cfg.execution_ceiling(),
        execution_running: false,
    };
    old_ledger.management_command(&ctx,Command::Policy(owner,serde_json::from_value(serde_json::json!({"expected_version":"1","disabled":false,"grants":grants,"limits":limits,"reason":"원래 기간 설정"})).unwrap())).await.unwrap();
    let current = finance::current(old_clock.pool(), owner).await;
    let spec = s4::strict_specification();
    old_ledger.admit_request(&current, &spec).await.unwrap();
    s4::complete_count(&old_ledger, &current, &spec).await;
    old_ledger.authorize_dispatch(spec.id).await.unwrap();
    let mut final_usage = observation();
    if zero {
        let usage = final_usage.usage.as_mut().unwrap();
        usage.input_tokens = 0;
        usage.output_tokens = 0;
    }
    old_ledger
        .finalize_request(spec.id, &final_usage)
        .await
        .unwrap();
    if remove_budget {
        old_ledger.management_command(&CommandContext{command_id:uuid::Uuid::new_v4(),..ctx},Command::Policy(owner,serde_json::from_value(serde_json::json!({"expected_version":"2","disabled":false,"grants":grants,"limits":{"budget":null,"rpm":null,"tpm":null,"count_rpm":null},"reason":"한도 제거 원래 기간 보존"})).unwrap())).await.unwrap();
    }
    let mut bundle = export_handoff_fixture(old_clock.pool(), owner).await;
    if carried {
        for period in bundle["periods"].as_array_mut().unwrap() {
            for (source, target) in [
                ("cost_usd", "carried_cost_usd"),
                ("settled_count", "carried_settled_count"),
                ("input_tokens", "carried_input_tokens"),
                ("output_tokens", "carried_output_tokens"),
                (
                    "settled_unknown_tokens_count",
                    "carried_settled_unknown_tokens_count",
                ),
            ] {
                period[target] = period[source].clone();
            }
        }
        for detail in ["windows", "requests", "budgets", "events"] {
            bundle[detail] = serde_json::json!([]);
        }
    }
    let target = uuid::Uuid::new_v4();
    let actor = finance::human(new.store.pool()).await;
    sqlx::query("INSERT INTO gw_principals(id,kind,issuer,subject,email,display_name) VALUES($1,'human','https://issuer.example',$2,'alice@example.test','Mapped source')").bind(target).bind(owner.to_string()).execute(new.store.pool()).await.unwrap();
    sqlx::query("INSERT INTO gw_policies(id,principal_id,allow_rules,budget_mode,period_kind,period_timezone,updated_by) VALUES($1,$2,'[]','actual','calendar_month','UTC',$2)").bind(uuid::Uuid::new_v4()).bind(target).execute(new.store.pool()).await.unwrap();
    let ledger = PgLedger::new(new.store.clone(), settings.clone(), [2; 32]);
    let ctx = CommandContext {
        actor,
        command_id: uuid::Uuid::new_v4(),
        method: "MAINTENANCE",
        path: "handoff",
        ..ctx
    };
    let source = uuid::Uuid::new_v4();
    ledger.handoff(&ctx,serde_json::from_value(serde_json::json!({"action":"fence","target_id":target,"expected_version":"1","source_id":source,"grants":[],"limits":{"budget":null,"rpm":null,"tpm":null,"count_rpm":null}})).unwrap()).await.unwrap();
    let cutoff: String = sqlx::query_scalar(
        "SELECT to_char(clock_timestamp() AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')",
    )
    .fetch_one(old.store.pool())
    .await
    .unwrap();
    let imported=ledger.handoff(&CommandContext{command_id:uuid::Uuid::new_v4(),..ctx},serde_json::from_value(serde_json::json!({"action":"import","target_id":target,"expected_version":"2","source_id":source,"source_ref":"real-source-policy-removal","previous_writer_fenced_ref":"synthetic-fence","previous_executors_stopped_ref":"source-process-joined","cutoff":cutoff,"bundle":bundle})).unwrap()).await;
    let row=sqlx::query("SELECT p.budget_mode,p.budget_limit_usd IS NULL AS unlimited,p.period_kind,p.period_timezone,t.period_start::text,t.period_end::text,t.cost_usd::text,t.detail_deleted_through IS NULL AS full_detail FROM gw_policies p JOIN gw_usage_totals t ON t.scope_id=p.id WHERE p.principal_id=$1").bind(target).fetch_optional(new.store.pool()).await.unwrap();
    let filtered = handoff_filtered_http(&cfg, new.store.clone(), target).await;
    old_clock.pool().close().await;
    old.close().await;
    new.close().await;
    assert!(imported.is_ok(), "정상 원래 scope 이전 거부: {imported:?}");
    if carried {
        assert_eq!(filtered.0, 409, "이월 상세 필터 HTTP: {}", filtered.1);
        assert_eq!(filtered.1["error"]["code"], "usage_detail_unavailable");
        assert!(filtered.1.get("settled_usd").is_none());
    } else {
        assert_eq!(filtered.0, 200, "온전한 상세 필터 HTTP: {}", filtered.1);
        assert_eq!(filtered.1["aggregation"], "detail");
        assert_eq!(filtered.1["settled_usd"], "0.000140000000");
    }
    let row = row.unwrap();
    assert_eq!(row.get::<String, _>("budget_mode"), "reserved");
    assert_eq!(row.get::<bool, _>("unlimited"), remove_budget);
    assert_eq!(row.get::<String, _>("period_kind"), "calendar_month");
    assert_eq!(
        row.get::<String, _>("period_timezone"),
        if remove_budget { "Asia/Seoul" } else { "UTC" }
    );
    assert_eq!(
        row.get::<String, _>("cost_usd"),
        if zero {
            "0.000000000000"
        } else {
            "0.000140000000"
        }
    );
    assert_eq!(
        row.get::<bool, _>("full_detail"),
        !carried,
        "원시 상세가 온전한 기간의 삭제 경계를 만들면 안 된다"
    );
}

async fn handoff_filtered_http(
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
                .uri(format!(
                    "/gateway/admin/usage?principal_id={target}&{query}"
                ))
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
    let ledger = PgLedger::new(
        h.database.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    );
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
    let user = finance::human(db.store.pool()).await;
    sqlx::query(
        "UPDATE gw_policies SET budget_mode='reserved',budget_limit_usd=1 WHERE principal_id=$1",
    )
    .bind(user)
    .execute(db.store.pool())
    .await
    .unwrap();
    let current = finance::current(db.store.pool(), user).await;
    let ledger = PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    );
    let request = s4::strict_specification();
    ledger.admit_request(&current, &request).await.unwrap();
    s4::complete_count(&ledger, &current, &request).await;
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
    assert!(one.is_ok() && two.is_ok() && count == 7);
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
        let owner = uuid::Uuid::from_u128(1);
        let service = uuid::Uuid::from_u128(2);
        sqlx::query("INSERT INTO gw_principals(id,kind,issuer,subject,email,display_name) VALUES($1,'human','https://issuer.example','owner','alice@example.test','Owner')").bind(owner).execute(db.store.pool()).await.unwrap();
        sqlx::query("INSERT INTO gw_principals(id,kind,display_name,owner_principal_id) VALUES($1,'service','Service',$2)").bind(service).bind(owner).execute(db.store.pool()).await.unwrap();
        sqlx::query("INSERT INTO gw_policies(id,principal_id,allow_rules,budget_mode,period_kind,period_timezone,updated_by) VALUES($1,$2,$3,'actual','calendar_month','UTC',$4)").bind(uuid::Uuid::new_v4()).bind(service).bind(serde_json::json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages","count_tokens"]}])).bind(owner).execute(db.store.pool()).await.unwrap();
        let cfg = configuration();
        let ledger = PgLedger::new(db.store.clone(), AccessSettings::from_config(&cfg), [1; 32]);
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
        let ledger = PgLedger::new(database.clone(), settings.clone(), [1; 32]);
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
async fn retention_hundred_request_page_preserves_family_totals_and_remaining_references() {
    use m_gated::{
        config::{DurationPreset, Retention},
        ledger::{commands::CommandContext, retention::RetentionDelete},
    };
    let db = postgres::TestDb::new().await;
    let database = db.clock_store(None).await;
    sqlx::query("UPDATE gw_fixture_clock SET observed='2025-01-31T12:00:00Z'")
        .execute(database.pool())
        .await
        .unwrap();
    let cfg = configuration_from_yaml(&configuration_yaml_for_s10());
    let settings = AccessSettings::from_config(&cfg);
    let actor = finance::human(database.pool()).await;
    let current = finance::current(database.pool(), actor).await;
    let ledger = PgLedger::new(database.clone(), settings.clone(), [1; 32]);
    for _ in 0..51 {
        let root = s4::strict_specification();
        ledger.admit_request(&current, &root).await.unwrap();
        s4::complete_count(&ledger, &current, &root).await;
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
        reason: "가족 100행 배치".into(),
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
    assert_eq!(first["result"]["deleted_requests"], 100);
    assert_eq!(remaining, 2);
    assert!(protected_window > 0);
    assert_eq!(second["result"]["deleted_requests"], 2);
    assert_eq!(totals.get::<i64, _>("settled_count"), 102);
    assert_eq!(totals.get::<i64, _>("carried_settled_count"), 102);
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
        let database = db.clock_store(None).await;
        let user = finance::human(database.pool()).await;
        let current = finance::current(database.pool(), user).await;
        let cfg = configuration();
        let ledger = PgLedger::new(database.clone(), AccessSettings::from_config(&cfg), [1; 32]);
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
        let user = finance::human(db.store.pool()).await;
        sqlx::query("UPDATE gw_policies SET budget_mode='reserved',budget_limit_usd=1 WHERE principal_id=$1").bind(user).execute(db.store.pool()).await.unwrap();
        let current = finance::current(db.store.pool(), user).await;
        let cfg = configuration();
        let healthy = PgLedger::new(db.store.clone(), AccessSettings::from_config(&cfg), [1; 32]);
        let request = s4::strict_specification();
        healthy.admit_request(&current, &request).await.unwrap();
        s4::complete_count(&healthy, &current, &request).await;
        healthy.authorize_dispatch(request.id).await.unwrap();
        let proxy = pg_proxy::CommitProxy::new().await;
        let faulty = db.store_via_proxy(proxy.port).await;
        let ledger = PgLedger::new(faulty.clone(), AccessSettings::from_config(&cfg), [1; 32]);
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
        let rebuilt = PgLedger::new(
            reconnected.clone(),
            AccessSettings::from_config(&cfg),
            [1; 32],
        );
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
async fn service_handoff_checks_mapped_owner_and_keeps_unknown_on_activation_and_owner_change() {
    use m_gated::ledger::{commands::CommandContext, management::ManagementError};
    let old = postgres::TestDb::new().await;
    let new = postgres::TestDb::new().await;
    let cfg = configuration_from_yaml(&configuration_yaml_for_s10());
    let settings = AccessSettings::from_config(&cfg);
    let owner = finance::human(old.store.pool()).await;
    let service = uuid::Uuid::from_u128(u128::MAX - 1);
    let scope = uuid::Uuid::new_v4();
    let grants = serde_json::json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages","count_tokens"]}]);
    sqlx::query("INSERT INTO gw_principals(id,kind,display_name,owner_principal_id) VALUES($1,'service','Source service',$2)").bind(service).bind(owner).execute(old.store.pool()).await.unwrap();
    sqlx::query("INSERT INTO gw_policies(id,principal_id,allow_rules,budget_mode,budget_limit_usd,period_kind,period_timezone,updated_by) VALUES($1,$2,$3,'reserved',10,'calendar_month','UTC',$4)").bind(scope).bind(service).bind(&grants).bind(owner).execute(old.store.pool()).await.unwrap();
    let ledger = PgLedger::new(old.store.clone(), settings.clone(), [1; 32]);
    let spec = s4::strict_specification();
    let current = finance::current(old.store.pool(), service).await;
    ledger.admit_request(&current, &spec).await.unwrap();
    s4::complete_count(&ledger, &current, &spec).await;
    ledger.authorize_dispatch(spec.id).await.unwrap();
    ledger
        .finalize_request(
            spec.id,
            &FinalObservation {
                usage: None,
                ..observation()
            },
        )
        .await
        .unwrap();
    let mut bundle = export_handoff_fixture(old.store.pool(), service).await;
    let actor = finance::human(new.store.pool()).await;
    let mapped_owner = finance::human(new.store.pool()).await;
    let other_owner = finance::human(new.store.pool()).await;
    bundle["identity"] = serde_json::json!({"kind":"service","issuer":null,"subject":null,"owner_principal_id":mapped_owner});
    sqlx::query("INSERT INTO gw_principals(id,kind,display_name,owner_principal_id) VALUES($1,'service','Mapped service',$2)").bind(service).bind(mapped_owner).execute(new.store.pool()).await.unwrap();
    sqlx::query("INSERT INTO gw_policies(id,principal_id,allow_rules,budget_mode,period_kind,period_timezone,updated_by) VALUES($1,$2,'[]','actual','calendar_month','UTC',$3)").bind(uuid::Uuid::new_v4()).bind(service).bind(actor).execute(new.store.pool()).await.unwrap();
    let ledger = PgLedger::new(new.store.clone(), settings.clone(), [2; 32]);
    let source = uuid::Uuid::new_v4();
    let ctx = CommandContext {
        actor,
        command_id: uuid::Uuid::new_v4(),
        method: "MAINTENANCE",
        path: "handoff",
        settings: &settings,
        grants: &grants,
        digest: [2; 32],
        ceiling: cfg.execution_ceiling(),
        execution_running: false,
    };
    ledger.handoff(&ctx,serde_json::from_value(serde_json::json!({"action":"fence","target_id":service,"expected_version":"1","source_id":source,"grants":[],"limits":{"budget":null,"rpm":null,"tpm":null,"count_rpm":null}})).unwrap()).await.unwrap();
    let cutoff: String = sqlx::query_scalar(
        "SELECT to_char(clock_timestamp() AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')",
    )
    .fetch_one(old.store.pool())
    .await
    .unwrap();
    let input = serde_json::json!({"action":"import","target_id":service,"expected_version":"2","source_id":source,"source_ref":"actual-service-source","previous_writer_fenced_ref":"synthetic-service-fence","previous_executors_stopped_ref":"synthetic-service-joined","cutoff":cutoff,"bundle":bundle});
    let mut wrong = input.clone();
    wrong["bundle"]["identity"]["owner_principal_id"] = serde_json::json!(other_owner);
    let mismatch = ledger
        .handoff(
            &CommandContext {
                command_id: uuid::Uuid::new_v4(),
                ..ctx
            },
            serde_json::from_value(wrong).unwrap(),
        )
        .await;
    assert!(
        matches!(mismatch, Err(ManagementError::Forbidden)),
        "명시 owner 불일치 경계: {}",
        match mismatch {
            Err(ManagementError::Invalid) => "invalid",
            Err(ManagementError::Database) => "database",
            Err(ManagementError::Invariant) => "invariant",
            Err(ManagementError::Forbidden) => "forbidden",
            Ok(_) => "ok",
            _ => "other",
        }
    );
    ledger
        .handoff(
            &CommandContext {
                command_id: uuid::Uuid::new_v4(),
                ..ctx
            },
            serde_json::from_value(input).unwrap(),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE gw_principals SET disabled_at=clock_timestamp(),disabled_reason='fixture' WHERE id=$1").bind(mapped_owner).execute(new.store.pool()).await.unwrap();
    let activation = serde_json::json!({"action":"activate","target_id":service,"expected_version":"3","source_id":source,"previous_executors_stopped_ref":"synthetic-service-joined"});
    assert!(
        ledger
            .handoff(
                &CommandContext {
                    command_id: uuid::Uuid::new_v4(),
                    ..ctx
                },
                serde_json::from_value(activation.clone()).unwrap()
            )
            .await
            .is_err()
    );
    let still_fenced:bool=sqlx::query_scalar("SELECT disabled_at IS NOT NULL AND security_fence_ref IS NOT NULL FROM gw_principals WHERE id=$1").bind(service).fetch_one(new.store.pool()).await.unwrap();
    assert!(still_fenced);
    sqlx::query("UPDATE gw_principals SET disabled_at=NULL,disabled_reason=NULL WHERE id=$1")
        .bind(mapped_owner)
        .execute(new.store.pool())
        .await
        .unwrap();
    ledger
        .handoff(
            &CommandContext {
                command_id: uuid::Uuid::new_v4(),
                ..ctx
            },
            serde_json::from_value(activation).unwrap(),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE gw_principals SET owner_principal_id=$2,version=version+1 WHERE id=$1")
        .bind(service)
        .bind(other_owner)
        .execute(new.store.pool())
        .await
        .unwrap();
    let held:bool=sqlx::query_scalar("SELECT r.accounting_state='unknown' AND r.cost_usd IS NULL AND b.remaining_reservation_usd>0 AND t.unknown_count=1 AND t.reserved_usd=b.remaining_reservation_usd FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id JOIN gw_usage_totals t ON t.scope_id=b.scope_id AND t.period_start=b.period_start WHERE r.id=$1").bind(spec.id).fetch_one(new.store.pool()).await.unwrap();
    assert!(held);
    let owner_unknown:i64=sqlx::query_scalar("SELECT count(*) FROM gw_requests WHERE principal_id=ANY($1) AND accounting_state='unknown'").bind(vec![mapped_owner,other_owner]).fetch_one(new.store.pool()).await.unwrap();
    assert_eq!(owner_unknown, 0);
    assert!(ledger.authorize_dispatch(spec.id).await.is_err());
    old.close().await;
    new.close().await;
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
