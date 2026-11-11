use super::*;
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_expired_free_count_recovers_zero_without_stopping_or_resending() {
    let db = postgres::TestDb::new().await;
    let id = finance::human(db.store.pool()).await;
    let current = finance::current(db.store.pool(), id).await;
    let ledger = PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    );
    let cfg = configuration();
    let spec =
        RequestSpec::from_profile(ProviderId::Anthropic, &cfg.providers()[0].profiles[1], None);
    ledger.admit_request(&current, &spec).await.unwrap();
    let expired = finance::expired_copy(db.store.pool(), spec.id, "dispatching").await;
    let result = ledger.recover_expired(expired).await;
    let row=sqlx::query("SELECT accounting_state,cost_usd::text AS cost,execution_stopped_at IS NULL AS running FROM gw_requests WHERE id=$1").bind(expired).fetch_one(db.store.pool()).await.unwrap();
    db.close().await;
    assert_eq!(result.unwrap(), Some(Finalized::Settled));
    assert_eq!(row.get::<String, _>("accounting_state"), "settled");
    assert_eq!(row.get::<String, _>("cost"), "0.000000000000");
    assert!(row.get::<bool, _>("running"));
}

use m_gated::ledger::{
    finance::KeyScope,
    policy::{PolicyLimits, validate_policy_transition},
};
fn limits(value: serde_json::Value) -> PolicyLimits {
    serde_json::from_value(value).unwrap()
}
fn budget(mode: &str, amount: &str, tz: &str) -> serde_json::Value {
    serde_json::json!({"budget":{"mode":mode,"limit_usd":amount,"period":{"kind":"calendar_month","timezone":tz}}})
}
async fn key(
    pool: &sqlx::PgPool,
    id: uuid::Uuid,
    limit: Option<&str>,
    tpm: Option<i64>,
    rpm: Option<i64>,
) -> KeyScope {
    let key = uuid::Uuid::new_v4();
    let command = uuid::Uuid::new_v4();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO gw_commands(actor_principal_id,command_id,input_digest,operation,target_kind,target_id,result_status,result_projection) VALUES($1,$2,decode(repeat('00',32),'hex'),'issue_key','key',$3,201,'{}')").bind(id).bind(command).bind(key).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO gw_virtual_keys(id,principal_id,secret_hash,hash_version,public_prefix,name,grant_snapshot,validity_preset,created_by,created_command_id) SELECT $2,$1,decode(replace($2::text,'-','')||replace($2::text,'-',''),'hex'),1,'fixture','fixture',allow_rules,'unlimited',$1,$3 FROM gw_policies WHERE principal_id=$1 AND key_id IS NULL").bind(id).bind(key).bind(command).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO gw_policies(id,principal_id,key_id,allow_rules,budget_mode,budget_limit_usd,period_kind,period_timezone,rpm_limit,tpm_limit,updated_by) SELECT $3,$1,$2,allow_rules,budget_mode,gw_amount($4::numeric),period_kind,period_timezone,$6,$5,$1 FROM gw_policies WHERE principal_id=$1 AND key_id IS NULL").bind(id).bind(key).bind(uuid::Uuid::new_v4()).bind(limit).bind(tpm).bind(rpm).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    KeyScope {
        id: key,
        version: 1,
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_key_budget_and_tpm_failures_roll_back_both_scopes() {
    for tpm in [false, true] {
        let db = postgres::TestDb::new().await;
        let id = finance::human(db.store.pool()).await;
        sqlx::query("UPDATE gw_policies SET budget_mode='reserved',budget_limit_usd=gw_amount('1.000000000000'),tpm_limit=5000 WHERE principal_id=$1").bind(id).execute(db.store.pool()).await.unwrap();
        let current = finance::current(db.store.pool(), id).await;
        let key = key(
            db.store.pool(),
            id,
            Some(if tpm {
                "1.000000000000"
            } else {
                "0.001000000000"
            }),
            Some(if tpm { 4000 } else { 5000 }),
            None,
        )
        .await;
        let ledger = PgLedger::new(
            db.store.clone(),
            AccessSettings::from_config(&configuration()),
            [1; 32],
        );
        let spec = strict_specification();
        ledger
            .admit_scoped(&current, &spec, Some(key), None)
            .await
            .unwrap();
        complete_count_key(&ledger, &current, &spec, Some(key)).await;
        let result = ledger.authorize_dispatch(spec.id).await;
        let money: bool =
            sqlx::query_scalar("SELECT bool_and(reserved_usd=0) FROM gw_usage_totals")
                .fetch_one(db.store.pool())
                .await
                .unwrap();
        let tokens: bool = sqlx::query_scalar(
            "SELECT bool_and(reserved_tokens=0 AND admitted_count=1) FROM gw_rate_windows",
        )
        .fetch_one(db.store.pool())
        .await
        .unwrap();
        let phase: String = sqlx::query_scalar("SELECT phase FROM gw_requests WHERE id=$1")
            .bind(spec.id)
            .fetch_one(db.store.pool())
            .await
            .unwrap();
        db.close().await;
        assert_eq!(
            result.err(),
            Some(if tpm {
                LedgerError::RateLimited
            } else {
                LedgerError::BudgetExhausted
            })
        );
        assert!(money && tokens);
        assert_eq!(phase, "admitted");
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_key_rpm_failure_rolls_back_principal_admission() {
    let db = postgres::TestDb::new().await;
    let id = finance::human(db.store.pool()).await;
    let current = finance::current(db.store.pool(), id).await;
    let key = key(db.store.pool(), id, None, None, Some(1)).await;
    let ledger = PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    );
    ledger
        .admit_scoped(&current, &specification(), Some(key), None)
        .await
        .unwrap();
    let result = ledger
        .admit_scoped(&current, &specification(), Some(key), None)
        .await;
    let counts: Vec<i64> =
        sqlx::query_scalar("SELECT admitted_count FROM gw_rate_windows ORDER BY scope_id")
            .fetch_all(db.store.pool())
            .await
            .unwrap();
    let requests: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_requests")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    db.close().await;
    assert_eq!(result, Err(LedgerError::RateLimited));
    assert_eq!(counts, vec![1, 1]);
    assert_eq!(requests, 1);
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_tpm_reservation_settlement_and_unknown_are_original_window() {
    let db = postgres::TestDb::new().await;
    let id = finance::human(db.store.pool()).await;
    sqlx::query("UPDATE gw_policies SET tpm_limit=5000 WHERE principal_id=$1")
        .bind(id)
        .execute(db.store.pool())
        .await
        .unwrap();
    let current = finance::current(db.store.pool(), id).await;
    let ledger = PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    );
    let a = strict_specification();
    let b = strict_specification();
    ledger.admit_request(&current, &a).await.unwrap();
    ledger.admit_request(&current, &b).await.unwrap();
    complete_count(&ledger, &current, &a).await;
    ledger.authorize_dispatch(a.id).await.unwrap();
    complete_count(&ledger, &current, &b).await;
    assert_eq!(
        ledger.authorize_dispatch(b.id).await.err(),
        Some(LedgerError::RateLimited)
    );
    ledger.finalize_request(a.id, &observation()).await.unwrap();
    ledger.authorize_dispatch(b.id).await.unwrap();
    let unknown = FinalObservation {
        outcome: m_gated::ledger::finance::FinalOutcome::Failed,
        usage: None,
        counted_input_tokens: None,
        reason: "upstream_transport_error",
        http_status: None,
        provider_request_id: None,
    };
    ledger.finalize_request(b.id, &unknown).await.unwrap();
    let row=sqlx::query("SELECT admitted_count,reserved_tokens,settled_tokens,window_end-window_start=interval '60 seconds' AS fixed FROM gw_rate_windows WHERE lane='inference'").fetch_one(db.store.pool()).await.unwrap();
    db.close().await;
    assert_eq!(row.get::<i64, _>("admitted_count"), 2);
    assert_eq!(row.get::<i64, _>("reserved_tokens"), 4196);
    assert_eq!(row.get::<i64, _>("settled_tokens"), 120);
    assert!(row.get::<bool, _>("fixed"));
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_current_ceiling_and_balance_conflict_never_reset_scope() {
    let db = postgres::TestDb::new().await;
    let id = finance::human(db.store.pool()).await;
    sqlx::query(
        "UPDATE gw_policies SET budget_limit_usd=gw_amount('1.000000000000') WHERE principal_id=$1",
    )
    .bind(id)
    .execute(db.store.pool())
    .await
    .unwrap();
    let current = finance::current(db.store.pool(), id).await;
    let ledger = PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    );
    let a = strict_specification();
    ledger.admit_request(&current, &a).await.unwrap();
    complete_count(&ledger, &current, &a).await;
    ledger.authorize_dispatch(a.id).await.unwrap();
    ledger.finalize_request(a.id, &observation()).await.unwrap();
    let ceiling = limits(budget("actual", "0.000100000000", "UTC"));
    let stricter = PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    )
    .with_ceiling(ceiling.clone());
    let b = strict_specification();
    stricter.admit_request(&current, &b).await.unwrap();
    assert_eq!(
        stricter.authorize_dispatch(b.id).await.err(),
        Some(LedgerError::BudgetExhausted)
    );
    let balance = db
        .store
        .dashboard_usage_with_ceiling(
            id,
            m_gated::ledger::dashboard::UsageQuery::parse(None).unwrap(),
            &serde_json::json!([]),
            "digest",
            &[7; 32],
            &ceiling,
        )
        .await
        .unwrap();
    assert_eq!(balance["budget_balances"][0]["limit_usd"], "0.000100000000");
    assert_eq!(
        balance["budget_balances"][0]["remaining_usd"],
        "0.000000000000"
    );
    let incompatible = limits(budget("reserved", "9.000000000000", "UTC"));
    let conflict = db
        .store
        .dashboard_usage_with_ceiling(
            id,
            m_gated::ledger::dashboard::UsageQuery::parse(None).unwrap(),
            &serde_json::json!([]),
            "digest",
            &[7; 32],
            &incompatible,
        )
        .await
        .unwrap();
    assert_eq!(
        conflict["budget_balances"][0]["balance_state"],
        "unresolved"
    );
    assert!(conflict["budget_balances"][0]["remaining_usd"].is_null());
    assert_eq!(
        conflict["budget_balances"][0]["settled_usd"],
        "0.000140000000"
    );
    let weak = limits(budget("actual", "10.000000000000", "UTC"));
    let balance = db
        .store
        .dashboard_usage_with_ceiling(
            id,
            m_gated::ledger::dashboard::UsageQuery::parse(None).unwrap(),
            &serde_json::json!([]),
            "digest",
            &[7; 32],
            &weak,
        )
        .await
        .unwrap();
    assert_eq!(balance["budget_balances"][0]["limit_usd"], "1.000000000000");
    db.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_policy_period_and_mode_transition_keeps_stable_scope_and_cost() {
    let db = postgres::TestDb::new().await;
    let id = finance::human(db.store.pool()).await;
    let current = finance::current(db.store.pool(), id).await;
    let ledger = PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    );
    let spec = strict_specification();
    ledger.admit_request(&current, &spec).await.unwrap();
    let mut tx = db.store.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM gw_principals WHERE id=$1 FOR UPDATE")
        .bind(id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM gw_policies WHERE id=$1 FOR UPDATE")
        .bind(current.policy_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(
        validate_policy_transition(
            &mut tx,
            current.policy_id,
            &limits(budget("actual", "1.000000000000", "America/New_York")),
            &Default::default()
        )
        .await
        .err(),
        Some(LedgerError::Conflict)
    );
    assert_eq!(
        validate_policy_transition(
            &mut tx,
            current.policy_id,
            &limits(budget("reserved", "1.000000000000", "UTC")),
            &Default::default()
        )
        .await
        .err(),
        Some(LedgerError::Conflict)
    );
    tx.rollback().await.unwrap();
    complete_count(&ledger, &current, &spec).await;
    ledger.authorize_dispatch(spec.id).await.unwrap();
    ledger
        .finalize_request(spec.id, &observation())
        .await
        .unwrap();
    let mut tx = db.store.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM gw_principals WHERE id=$1 FOR UPDATE")
        .bind(id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM gw_policies WHERE id=$1 FOR UPDATE")
        .bind(current.policy_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    let normalized = validate_policy_transition(
        &mut tx,
        current.policy_id,
        &limits(budget("reserved", "2.000000000000", "UTC")),
        &limits(budget("reserved", "1.000000000000", "UTC")),
    )
    .await
    .unwrap();
    assert_eq!(normalized.budget.unwrap().limit_usd, "1.000000000000");
    sqlx::query("UPDATE gw_policies SET budget_mode='reserved',budget_limit_usd=gw_amount('1.000000000000'),version=version+1 WHERE id=$1").bind(current.policy_id).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let row = sqlx::query("SELECT scope_id,cost_usd::text AS cost FROM gw_usage_totals")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    db.close().await;
    assert_eq!(row.get::<uuid::Uuid, _>("scope_id"), current.policy_id);
    assert_eq!(row.get::<String, _>("cost"), "0.000140000000");
}

pub(super) fn strict_specification() -> RequestSpec {
    let cfg = configuration_from_yaml(&finance_yaml().replace(
        "api: messages",
        "api: messages\n        internal_count: true",
    ));
    RequestSpec::from_profile(
        ProviderId::Anthropic,
        &cfg.providers()[0].profiles[0],
        Some(100),
    )
}
pub(super) async fn complete_count(
    ledger: &PgLedger,
    current: &m_gated::ledger::CurrentHuman,
    root: &RequestSpec,
) {
    complete_count_key(ledger, current, root, None).await;
}
async fn complete_count_key(
    ledger: &PgLedger,
    current: &m_gated::ledger::CurrentHuman,
    root: &RequestSpec,
    key: Option<KeyScope>,
) {
    let cfg = configuration();
    let mut child =
        RequestSpec::from_profile(ProviderId::Anthropic, &cfg.providers()[0].profiles[1], None);
    child.deadline = root.deadline;
    ledger
        .admit_scoped(current, &child, key, Some(root.id))
        .await
        .unwrap();
    ledger.authorize_dispatch(child.id).await.unwrap();
    let observation = FinalObservation {
        outcome: m_gated::ledger::finance::FinalOutcome::Completed,
        usage: None,
        counted_input_tokens: Some(321),
        reason: "completed",
        http_status: Some(200),
        provider_request_id: None,
    };
    ledger
        .finalize_request(child.id, &observation)
        .await
        .unwrap();
}
pub(super) fn chargeable_configuration(internal: bool) -> config::ValidatedConfig {
    configuration_from_yaml(&finance_yaml().replace("billing_class: guaranteed_free","billing_class: chargeable\n        count_billing: counted_input_tokens\n        input_bound_contract:\n          kind: anthropic_context_v1\n          context_tokens: 4096\n          input_categories: [input, cache_read, cache_write]").replace("input: '0.000000000000'","input: '1.000000000000'").replace("api: messages",if internal{"api: messages\n        internal_count: true"}else{"api: messages"}))
}
pub(super) fn count_observation(free: bool) -> FinalObservation {
    FinalObservation {
        outcome: m_gated::ledger::finance::FinalOutcome::Completed,
        usage: if free {
            None
        } else {
            Some(FinalUsage {
                actual_model: "synthetic-text".into(),
                input_tokens: 10,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: None,
            })
        },
        counted_input_tokens: Some(10),
        reason: "completed",
        http_status: Some(200),
        provider_request_id: None,
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_count_lease_edges_and_both_finalizer_recovery_orders_are_independent() {
    let db = postgres::TestDb::new().await;
    let store = db.clock_store(None).await;
    for internal in [false, true] {
        for free in [false, true] {
            for offset in [-1i64, 0, 1] {
                for order in [0, 1, 2] {
                    sqlx::query("UPDATE gw_fixture_clock SET observed='2025-03-31T23:59:00Z'")
                        .execute(store.pool())
                        .await
                        .unwrap();
                    let id = finance::human(store.pool()).await;
                    let current = finance::current(store.pool(), id).await;
                    let cfg = if free {
                        configuration()
                    } else {
                        chargeable_configuration(false)
                    };
                    let ledger =
                        PgLedger::new(store.clone(), AccessSettings::from_config(&cfg), [1; 32]);
                    let root = specification();
                    if internal {
                        ledger.admit_request(&current, &root).await.unwrap();
                    }
                    let mut spec = RequestSpec::from_profile(
                        ProviderId::Anthropic,
                        &cfg.providers()[0].profiles[1],
                        None,
                    );
                    spec.deadline = root.deadline;
                    ledger
                        .admit_scoped(&current, &spec, None, internal.then_some(root.id))
                        .await
                        .unwrap();
                    ledger.authorize_dispatch(spec.id).await.unwrap();
                    sqlx::query("UPDATE gw_fixture_clock SET observed=(SELECT lease_expires_at FROM gw_requests WHERE id=$1)+$2::bigint*interval '1 microsecond'").bind(spec.id).bind(offset).execute(store.pool()).await.unwrap();
                    let observation = count_observation(free);
                    match order {
                        0 => {
                            ledger
                                .finalize_request(spec.id, &observation)
                                .await
                                .unwrap();
                            assert_eq!(ledger.recover_expired(spec.id).await.unwrap(), None);
                        }
                        1 => {
                            let recovery = ledger.recover_expired(spec.id).await.unwrap();
                            if offset < 0 {
                                assert!(recovery.is_none());
                                ledger
                                    .finalize_request(spec.id, &observation)
                                    .await
                                    .unwrap();
                            } else {
                                assert!(recovery.is_some());
                                if free {
                                    assert_eq!(
                                        ledger.finalize_request(spec.id, &observation).await.err(),
                                        Some(LedgerError::Conflict)
                                    );
                                } else {
                                    assert_eq!(
                                        ledger
                                            .finalize_request(spec.id, &observation)
                                            .await
                                            .unwrap(),
                                        Finalized::Noop
                                    );
                                    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM gw_usage_events WHERE request_id=$1 AND kind='late_final'").bind(spec.id).fetch_one(store.pool()).await.unwrap(),1);
                                }
                            }
                        }
                        _ => {
                            let barrier = tokio::sync::Barrier::new(2);
                            let (a, b) = tokio::join!(
                                async {
                                    barrier.wait().await;
                                    ledger.finalize_request(spec.id, &observation).await
                                },
                                async {
                                    barrier.wait().await;
                                    ledger.recover_expired(spec.id).await
                                }
                            );
                            assert!(a.is_ok() || b.as_ref().is_ok_and(|r| r.is_some()));
                        }
                    }
                    assert_eq!(
                        ledger.authorize_dispatch(spec.id).await.err(),
                        Some(LedgerError::Conflict)
                    );
                    let row=sqlx::query("SELECT r.accounting_state,r.cost_usd::text AS cost,r.execution_stopped_at IS NULL AS running,b.state,b.applied_input_tokens,b.remaining_reservation_usd::text AS held,w.settled_tokens,w.reserved_tokens,t.unknown_count,t.settled_count,e.normalized_usage->>'source' AS source,e.normalized_usage->>'counted_input_tokens' AS counted FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id JOIN gw_usage_totals t ON t.scope_id=b.scope_id AND t.period_start=b.period_start JOIN gw_rate_windows w ON w.scope_id=b.scope_id AND w.lane=b.rate_lane AND w.window_start=b.rate_start JOIN gw_usage_events e ON e.request_id=r.id AND e.kind IN ('partial','final') WHERE r.id=$1").bind(spec.id).fetch_one(store.pool()).await.unwrap();
                    let settled = free || offset < 0;
                    assert_eq!(
                        row.get::<String, _>("accounting_state"),
                        if settled { "settled" } else { "unknown" }
                    );
                    assert!(row.get::<bool, _>("running"));
                    assert_eq!(row.get::<i64, _>("settled_tokens"), 0);
                    assert_eq!(row.get::<i64, _>("reserved_tokens"), 0);
                    assert_eq!(row.get::<i64, _>("unknown_count"), i64::from(!settled));
                    assert_eq!(row.get::<i64, _>("settled_count"), i64::from(settled));
                    if free {
                        assert_eq!(row.get::<String, _>("cost"), "0.000000000000");
                        assert_eq!(row.get::<String, _>("source"), "profile_no_charge");
                        assert_eq!(row.get::<i64, _>("applied_input_tokens"), 0);
                        if order == 1 && offset >= 0 {
                            assert!(row.get::<Option<String>, _>("counted").is_none());
                        }
                    }
                    if internal {
                        let mut zero = observation.clone();
                        zero.usage = None;
                        ledger.finalize_request(root.id, &zero).await.unwrap();
                        let cost: String = sqlx::query_scalar(
                            "SELECT cost_usd::text FROM gw_requests WHERE id=$1",
                        )
                        .bind(root.id)
                        .fetch_one(store.pool())
                        .await
                        .unwrap();
                        assert_eq!(cost, "0.000000000000");
                    }
                }
            }
        }
    }
    store.pool().close().await;
    db.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_paid_count_cost_and_unknown_survive_parent_rejection() {
    for lost in [false, true] {
        let h = Harness::with_config(chargeable_configuration(true)).await;
        sqlx::query("UPDATE gw_policies SET budget_limit_usd=gw_amount('0.000300000000') WHERE principal_id=$1").bind(h.principal).execute(h.db.store.pool()).await.unwrap();
        h.provider
            .set_count(
                StatusCode::OK,
                HeaderMap::from_iter([(
                    "content-type".parse().unwrap(),
                    "application/json".parse().unwrap(),
                )]),
                if lost {
                    Bytes::from_static(br#"{"input_tokens":"unknown"}"#)
                } else {
                    Bytes::from_static(br#"{"input_tokens":321}"#)
                },
            )
            .await;
        let response = h.request("/v1/messages", BODY).send().await.unwrap();
        let status = response.status();
        let _ = response.bytes().await.unwrap();
        let rows=sqlx::query("SELECT purpose,accounting_state,cost_usd::text AS cost FROM gw_requests ORDER BY purpose").fetch_all(h.db.store.pool()).await.unwrap();
        let windows=sqlx::query("SELECT lane,admitted_count,settled_tokens,reserved_tokens FROM gw_rate_windows ORDER BY lane").fetch_all(h.db.store.pool()).await.unwrap();
        let calls = h.provider.calls.lock().await.len();
        h.close().await;
        assert_eq!(
            status,
            if lost {
                StatusCode::BAD_GATEWAY
            } else {
                StatusCode::TOO_MANY_REQUESTS
            }
        );
        assert_eq!(calls, 1);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].get::<String, _>("cost"), "0.000000000000");
        assert_eq!(
            rows[0].get::<String, _>("accounting_state"),
            if lost { "unknown" } else { "settled" }
        );
        if !lost {
            assert_eq!(rows[0].get::<String, _>("cost"), "0.000321000000");
        } else {
            assert!(rows[0].get::<Option<String>, _>("cost").is_none());
        }
        assert_eq!(windows.len(), 2);
        for row in windows {
            assert_eq!(row.get::<i64, _>("admitted_count"), 1);
            assert_eq!(row.get::<i64, _>("settled_tokens"), 0);
            assert_eq!(row.get::<i64, _>("reserved_tokens"), 0);
        }
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_count_wait_policy_recheck_keeps_child_cost_and_parent_zero() {
    let h = Harness::with_config(chargeable_configuration(true)).await;
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
    let (ready, release) = h.provider.hold_count().await;
    let request = h.request("/v1/messages", BODY);
    let job = tokio::spawn(async move {
        let response = request.send().await.unwrap();
        let status = response.status();
        let _ = response.bytes().await.unwrap();
        status
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), ready)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE gw_policies SET version=version+1,allow_rules='[]' WHERE principal_id=$1")
        .bind(h.principal)
        .execute(h.db.store.pool())
        .await
        .unwrap();
    release.send(()).unwrap();
    let status = job.await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), h.runtime.wait_requests())
        .await
        .unwrap();
    let rows=sqlx::query("SELECT purpose,accounting_state,cost_usd::text AS cost,execution_stopped_at IS NOT NULL AS stopped FROM gw_requests ORDER BY purpose").fetch_all(h.db.store.pool()).await.unwrap();
    let calls = h.provider.calls.lock().await.len();
    h.close().await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(calls, 1);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<String, _>("cost"), "0.000321000000");
    assert_eq!(rows[1].get::<String, _>("cost"), "0.000000000000");
    for row in rows {
        assert!(row.get::<bool, _>("stopped"));
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_internal_count_deadline_finalizes_whole_family_without_slot_leak() {
    for free in [false, true] {
        let yaml = format!(
            "{}limits:\n  request_timeout_ms: 7000\n  transport_timeout_ms: 2000\n  body_timeout_ms: 1000\n  body_idle_timeout_ms: 500\n",
            if free {
                finance_yaml().replace(
                    "api: messages",
                    "api: messages\n        internal_count: true",
                )
            } else {
                finance_yaml()
                    .replace(
                        "api: messages",
                        "api: messages\n        internal_count: true",
                    )
                    .replace(
                        "billing_class: guaranteed_free",
                        "billing_class: chargeable\n        count_billing: counted_input_tokens",
                    )
                    .replace("input: '0.000000000000'", "input: '1.000000000000'")
            }
        );
        let h = Harness::with_config(configuration_from_yaml(&yaml)).await;
        let (ready, release) = h.provider.hold_count().await;
        let request = h.request("/v1/messages", BODY);
        let mut job = tokio::spawn(async move {
            match request.send().await {
                Ok(r) => {
                    let status = r.status().as_u16();
                    let _ = r.bytes().await;
                    Some(status)
                }
                Err(_) => None,
            }
        });
        let started = tokio::time::timeout(std::time::Duration::from_secs(3), ready).await;
        if !matches!(started, Ok(Ok(()))) {
            let status = tokio::time::timeout(std::time::Duration::from_secs(1), &mut job)
                .await
                .ok()
                .and_then(Result::ok)
                .flatten();
            job.abort();
            let calls = h.provider.calls.lock().await.len();
            let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_requests")
                .fetch_one(h.db.store.pool())
                .await
                .unwrap();
            h.close().await;
            panic!("count 시작 전제 실패: status={status:?} calls={calls} rows={rows}");
        }
        tokio::time::timeout(std::time::Duration::from_secs(7), job)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), h.runtime.wait_requests())
            .await
            .unwrap();
        let rows=sqlx::query("SELECT purpose,accounting_state,cost_usd::text AS cost,execution_stopped_at IS NOT NULL AS stopped,transport_deadline,execution_deadline FROM gw_requests ORDER BY purpose").fetch_all(h.db.store.pool()).await.unwrap();
        let deadlines:bool=sqlx::query_scalar("SELECT c.transport_deadline<=p.transport_deadline AND c.execution_deadline<=p.execution_deadline AND c.execution_deadline-c.transport_deadline=interval '5 seconds' FROM gw_requests c JOIN gw_requests p ON c.parent_request_id=p.id").fetch_one(h.db.store.pool()).await.unwrap();
        let active = h.runtime.active_requests();
        let calls = h.provider.calls.lock().await.len();
        let _ = release.send(());
        h.close().await;
        assert_eq!(active, 0);
        assert!(deadlines);
        assert_eq!(calls, 1);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].get::<String, _>("accounting_state"),
            if free { "settled" } else { "unknown" }
        );
        if free {
            assert_eq!(rows[0].get::<String, _>("cost"), "0.000000000000");
        }
        assert_eq!(rows[1].get::<String, _>("cost"), "0.000000000000");
        for row in rows {
            assert!(row.get::<bool, _>("stopped"));
        }
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_strict_context_contract_is_enforced_by_synthetic_upstream_not_count_estimate() {
    for oversized in [false, true] {
        let cfg = configuration_from_yaml(&finance_yaml().replace(
            "api: messages",
            "api: messages\n        internal_count: true",
        ));
        let h = Harness::with_config(cfg).await;
        sqlx::query("UPDATE gw_policies SET budget_mode='reserved',budget_limit_usd=gw_amount('1.000000000000'),tpm_limit=5000 WHERE principal_id=$1").bind(h.principal).execute(h.db.store.pool()).await.unwrap();
        h.provider.hardcap(4096).await;
        h.provider
            .set_count(
                StatusCode::OK,
                HeaderMap::from_iter([(
                    "content-type".parse().unwrap(),
                    "application/json".parse().unwrap(),
                )]),
                Bytes::from_static(br#"{"input_tokens":1}"#),
            )
            .await;
        let mut body = BODY.replace("synthetic prompt sentinel", "");
        let length = if oversized { 4097 } else { 4096 };
        body = body.replace(
            "\"content\":\"\"",
            &format!("\"content\":\"{}\"", "x".repeat(length - body.len())),
        );
        assert_eq!(body.len(), length);
        let response = h.request("/v1/messages", body).send().await.unwrap();
        let status = response.status();
        let _ = response.bytes().await.unwrap();
        let row=sqlx::query("SELECT r.accounting_state,b.reservation_usd::text AS reserve,b.reserved_tokens FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id WHERE r.purpose='root'").fetch_one(h.db.store.pool()).await.unwrap();
        let calls = h.provider.calls.lock().await.len();
        h.close().await;
        assert_eq!(calls, 2);
        assert_eq!(
            status,
            if oversized {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::OK
            }
        );
        assert_eq!(
            row.get::<String, _>("accounting_state"),
            if oversized { "unknown" } else { "settled" }
        );
        assert_eq!(row.get::<String, _>("reserve"), "0.004296000000");
        assert_eq!(row.get::<i64, _>("reserved_tokens"), 4196);
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_strict_rejects_unverified_missing_or_wrong_count_and_never_reuses_child() {
    let db = postgres::TestDb::new().await;
    let id = finance::human(db.store.pool()).await;
    sqlx::query("UPDATE gw_policies SET budget_mode='reserved' WHERE principal_id=$1")
        .bind(id)
        .execute(db.store.pool())
        .await
        .unwrap();
    let current = finance::current(db.store.pool(), id).await;
    let ledger = PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    );
    let unverified=configuration_from_yaml(&finance_yaml().replace("api: messages","api: messages\n        internal_count: true").replace("        input_bound_contract:\n          kind: anthropic_context_v1\n          context_tokens: 4096\n          input_categories: [input, cache_read, cache_write]\n",""));
    let unsupported = RequestSpec::from_profile(
        ProviderId::Anthropic,
        &unverified.providers()[0].profiles[0],
        Some(100),
    );
    assert_eq!(
        ledger.admit_request(&current, &unsupported).await.err(),
        Some(LedgerError::UnsupportedStrictPolicy)
    );
    let root = strict_specification();
    let other = strict_specification();
    ledger.admit_request(&current, &root).await.unwrap();
    ledger.admit_request(&current, &other).await.unwrap();
    assert_eq!(
        ledger.authorize_dispatch(root.id).await.err(),
        Some(LedgerError::UnsupportedStrictPolicy)
    );
    complete_count(&ledger, &current, &other).await;
    assert_eq!(
        ledger.authorize_dispatch(root.id).await.err(),
        Some(LedgerError::UnsupportedStrictPolicy)
    );
    let cfg = configuration();
    let child =
        RequestSpec::from_profile(ProviderId::Anthropic, &cfg.providers()[0].profiles[1], None);
    ledger
        .admit_scoped(&current, &child, None, Some(root.id))
        .await
        .unwrap();
    assert_eq!(
        ledger.authorize_dispatch(root.id).await.err(),
        Some(LedgerError::UnsupportedStrictPolicy)
    );
    ledger.authorize_dispatch(child.id).await.unwrap();
    ledger
        .finalize_request(child.id, &count_observation(true))
        .await
        .unwrap();
    assert!(ledger.authorize_dispatch(root.id).await.is_ok());
    let duplicate =
        RequestSpec::from_profile(ProviderId::Anthropic, &cfg.providers()[0].profiles[1], None);
    assert!(
        ledger
            .admit_scoped(&current, &duplicate, None, Some(root.id))
            .await
            .is_err()
    );
    assert!(
        ledger
            .admit_scoped(&current, &duplicate, None, Some(child.id))
            .await
            .is_err()
    );
    let mut different = cfg.providers()[0].profiles[1].clone();
    different.model = "different-model".into();
    different.actual_models = vec![different.model.clone()];
    let spec = RequestSpec::from_profile(ProviderId::Anthropic, &different, None);
    assert!(
        ledger
            .admit_scoped(&current, &spec, None, Some(other.id))
            .await
            .is_err()
    );
    db.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_key_current_revoke_policy_and_independent_balances_are_sql() {
    let db = postgres::TestDb::new().await;
    let id = finance::human(db.store.pool()).await;
    sqlx::query(
        "UPDATE gw_policies SET budget_limit_usd=gw_amount('2.000000000000') WHERE principal_id=$1",
    )
    .bind(id)
    .execute(db.store.pool())
    .await
    .unwrap();
    let current = finance::current(db.store.pool(), id).await;
    let key = key(db.store.pool(), id, Some("1.000000000000"), None, None).await;
    let ledger = PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    );
    let spec = specification();
    ledger
        .admit_scoped(&current, &spec, Some(key), None)
        .await
        .unwrap();
    ledger.authorize_dispatch(spec.id).await.unwrap();
    ledger
        .finalize_request(spec.id, &observation())
        .await
        .unwrap();
    let usage = db
        .store
        .dashboard_usage(
            id,
            m_gated::ledger::dashboard::UsageQuery::parse(Some(&format!("key_id={}", key.id)))
                .unwrap(),
            &serde_json::json!([]),
            "digest",
            &[7; 32],
        )
        .await
        .unwrap();
    assert_eq!(usage["settled_usd"], "0.000140000000");
    assert_eq!(
        usage["budget_balances"][0]["remaining_usd"],
        "1.999860000000"
    );
    assert_eq!(
        usage["budget_balances"][1]["remaining_usd"],
        "0.999860000000"
    );
    let pending = specification();
    ledger
        .admit_scoped(&current, &pending, Some(key), None)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE gw_virtual_keys SET revoked_at=clock_timestamp(),version=version+1 WHERE id=$1",
    )
    .bind(key.id)
    .execute(db.store.pool())
    .await
    .unwrap();
    assert_eq!(
        ledger.authorize_dispatch(pending.id).await.err(),
        Some(LedgerError::AccessDenied)
    );
    let another = key_fixture(&db, id).await;
    let pending = specification();
    ledger
        .admit_scoped(&current, &pending, Some(another), None)
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE gw_policies SET allow_rules='[]',version=version+1 WHERE key_id=$1")
            .bind(another.id)
            .execute(db.store.pool())
            .await
            .is_err()
    );
    // 발급 상한은 불변이다. 현재 principal 정책 축소가 준비된 키 요청도 무효화한다.
    sqlx::query("UPDATE gw_policies SET allow_rules='[]',version=version+1 WHERE principal_id=$1 AND key_id IS NULL")
        .bind(id).execute(db.store.pool()).await.unwrap();
    assert_eq!(
        ledger.authorize_dispatch(pending.id).await.err(),
        Some(LedgerError::PolicyChanged)
    );
    db.close().await;
}
async fn key_fixture(db: &postgres::TestDb, id: uuid::Uuid) -> KeyScope {
    key(db.store.pool(), id, None, None, None).await
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_unknown_and_expired_chargeable_block_new_keys_providers_and_calendar_month() {
    for mode in ["actual", "reserved"] {
        let db = postgres::TestDb::new().await;
        let store = db.clock_store(None).await;
        let id = finance::human(store.pool()).await;
        sqlx::query("UPDATE gw_policies SET budget_mode=$2,budget_limit_usd=gw_amount('9.000000000000') WHERE principal_id=$1").bind(id).bind(mode).execute(store.pool()).await.unwrap();
        let current = finance::current(store.pool(), id).await;
        let cfg = chargeable_configuration(false);
        let ledger = PgLedger::new(store.clone(), AccessSettings::from_config(&cfg), [1; 32]);
        let paid =
            RequestSpec::from_profile(ProviderId::Anthropic, &cfg.providers()[0].profiles[1], None);
        ledger.admit_request(&current, &paid).await.unwrap();
        ledger.authorize_dispatch(paid.id).await.unwrap();
        sqlx::query("UPDATE gw_fixture_clock SET observed=(SELECT lease_expires_at FROM gw_requests WHERE id=$1)+interval '1 month'").bind(paid.id).execute(store.pool()).await.unwrap();
        for recover in [false, true] {
            if recover {
                // 앞선 거부 transaction의 Drop은 rollback ACK를 기다리지 않는다.
                // 자기 principal의 실제 잠금/rollback ACK 뒤에만 무경합 NOWAIT 성공을 기대한다.
                let mut barrier = store.pool().begin().await.unwrap();
                sqlx::query("SELECT id FROM gw_principals WHERE id=$1 FOR UPDATE")
                    .bind(id)
                    .fetch_one(&mut *barrier)
                    .await
                    .unwrap();
                barrier.rollback().await.unwrap();
                let expired: bool = sqlx::query_scalar("SELECT phase='dispatching' AND lease_expires_at<=(SELECT observed FROM gw_fixture_clock) FROM gw_requests WHERE id=$1")
                    .bind(paid.id).fetch_one(store.pool()).await.unwrap();
                assert!(expired, "원래 과금 ID/고정 expiry 전제");
                let recovered = ledger.recover_expired(paid.id).await.unwrap();
                if recovered != Some(Finalized::Unknown) {
                    eprintln!(
                        "S4_RECOVERY_UNCONTENDED mode={mode} expired={expired} unknown=false"
                    );
                    store.pool().close().await;
                    db.close().await;
                    panic!("무경합/원래 expiry 전제 뒤 recovery가 Unknown이 아님");
                }
            }
            let key = key(store.pool(), id, Some("9.000000000000"), None, None).await;
            let next = strict_specification();
            ledger
                .admit_scoped(&current, &next, Some(key), None)
                .await
                .unwrap();
            assert_eq!(
                ledger.authorize_dispatch(next.id).await.err(),
                Some(LedgerError::Unsettled)
            );
            // 후속 OpenAI adapter 완료가 아닌, durable ledger의 전 Provider guard fixture다.
            let other = uuid::Uuid::new_v4();
            sqlx::query("INSERT INTO gw_requests(id,principal_id,purpose,provider,requested_model,api,admitted_at,admit_expires_at,transport_deadline,execution_deadline,executor_id,phase,accounting_state,pricing_snapshot,execution_snapshot,auth_config_digest) SELECT $1,principal_id,'root','openai',requested_model,'responses',admitted_at,admit_expires_at,transport_deadline,execution_deadline,executor_id,'admitted','pending',jsonb_set(jsonb_set(pricing_snapshot,'{provider}','\"openai\"'),'{api}','\"responses\"'),jsonb_set(jsonb_set(execution_snapshot,'{capability}','\"responses\"'),'{auth_dependency_versions,key}','null'),auth_config_digest FROM gw_requests WHERE id=$2").bind(other).bind(next.id).execute(store.pool()).await.unwrap();
            sqlx::query("INSERT INTO gw_request_budgets SELECT $1,scope_id,policy_version,policy_snapshot,period_start,period_end,timezone_snapshot,rate_lane,rate_start,rate_end,budget_mode,reservation_usd,remaining_reservation_usd,reserved_tokens,remaining_reserved_tokens,applied_cost_usd,applied_input_tokens,applied_output_tokens,state FROM gw_request_budgets WHERE request_id=$2 AND scope_id=$3").bind(other).bind(next.id).bind(current.policy_id).execute(store.pool()).await.unwrap();
            assert_eq!(
                ledger.authorize_dispatch(other).await.err(),
                Some(LedgerError::Unsettled)
            );
        }
        let freecfg = configuration();
        let free = RequestSpec::from_profile(
            ProviderId::Anthropic,
            &freecfg.providers()[0].profiles[1],
            None,
        );
        let conflict = PgLedger::new(
            store.clone(),
            AccessSettings::from_config(&freecfg),
            [1; 32],
        )
        .with_ceiling(limits(budget(
            if mode == "actual" {
                "reserved"
            } else {
                "actual"
            },
            "0.000000000000",
            "America/New_York",
        )));
        conflict.admit_request(&current, &free).await.unwrap();
        assert!(conflict.authorize_dispatch(free.id).await.is_ok());
        store.pool().close().await;
        db.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_iana_dst_month_and_fixed_sixty_second_windows_survive_next_month_final() {
    let db = postgres::TestDb::new().await;
    let store = db.clock_store(None).await;
    for (admit, finish, hours) in [
        ("2025-04-01T03:59:00Z", "2025-04-01T04:00:00Z", 743i64),
        ("2025-12-01T04:59:00Z", "2025-12-01T05:00:00Z", 721),
    ] {
        sqlx::query("UPDATE gw_fixture_clock SET observed=$1::timestamptz")
            .bind(admit)
            .execute(store.pool())
            .await
            .unwrap();
        let id = finance::human(store.pool()).await;
        sqlx::query(
            "UPDATE gw_policies SET period_timezone='America/New_York' WHERE principal_id=$1",
        )
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();
        let current = finance::current(store.pool(), id).await;
        let ledger = PgLedger::new(
            store.clone(),
            AccessSettings::from_config(&configuration()),
            [1; 32],
        );
        let spec = specification();
        ledger.admit_request(&current, &spec).await.unwrap();
        ledger.authorize_dispatch(spec.id).await.unwrap();
        sqlx::query("UPDATE gw_fixture_clock SET observed=$1::timestamptz")
            .bind(finish)
            .execute(store.pool())
            .await
            .unwrap();
        ledger
            .finalize_request(spec.id, &observation())
            .await
            .unwrap();
        let next = specification();
        ledger.admit_request(&current, &next).await.unwrap();
        let row=sqlx::query("SELECT extract(epoch FROM b.period_end-b.period_start)::bigint/3600 AS hours,t.cost_usd::text AS cost,b.rate_start=$2::timestamptz AS original_window,w.settled_tokens,w.admitted_count,(SELECT count(*) FROM gw_usage_totals WHERE scope_id=b.scope_id) AS periods,(SELECT period_start FROM gw_request_budgets WHERE request_id=$3)=b.period_end AS new_period FROM gw_request_budgets b JOIN gw_usage_totals t ON t.scope_id=b.scope_id AND t.period_start=b.period_start JOIN gw_rate_windows w ON w.scope_id=b.scope_id AND w.lane=b.rate_lane AND w.window_start=b.rate_start WHERE b.request_id=$1").bind(spec.id).bind(admit).bind(next.id).fetch_one(store.pool()).await.unwrap();
        assert_eq!(row.get::<i64, _>("hours"), hours);
        assert_eq!(row.get::<String, _>("cost"), "0.000140000000");
        assert!(row.get::<bool, _>("original_window"));
        assert!(row.get::<bool, _>("new_period"));
        assert_eq!(row.get::<i64, _>("periods"), 2);
        assert_eq!(row.get::<i64, _>("settled_tokens"), 120);
        assert_eq!(row.get::<i64, _>("admitted_count"), 1);
    }
    store.pool().close().await;
    db.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_free_and_paid_count_commit_ack_loss_recovery_never_reissues_permit() {
    use std::sync::atomic::Ordering;
    let provider = provider::Provider::new();
    let server = provider.serve().await;
    for internal in [false, true] {
        for free in [false, true] {
            let db = postgres::TestDb::new().await;
            let proxy = pg_proxy::CommitProxy::new().await;
            let faulty = db.clock_store(Some(proxy.port)).await;
            let healthy = db.clock_store(None).await;
            let id = finance::human(healthy.pool()).await;
            sqlx::query("UPDATE gw_policies SET budget_mode='reserved',budget_limit_usd=gw_amount('1.000000000000') WHERE principal_id=$1").bind(id).execute(healthy.pool()).await.unwrap();
            let current = finance::current(healthy.pool(), id).await;
            let cfg = if free {
                configuration()
            } else {
                chargeable_configuration(false)
            };
            let ledger = PgLedger::new(faulty.clone(), AccessSettings::from_config(&cfg), [1; 32]);
            let root = strict_specification();
            if internal {
                ledger.admit_request(&current, &root).await.unwrap();
            }
            let mut spec = RequestSpec::from_profile(
                ProviderId::Anthropic,
                &cfg.providers()[0].profiles[1],
                None,
            );
            spec.deadline = root.deadline;
            ledger
                .admit_scoped(&current, &spec, None, internal.then_some(root.id))
                .await
                .unwrap();
            proxy.armed.store(true, Ordering::SeqCst);
            assert_eq!(
                ledger.authorize_dispatch(spec.id).await.err(),
                Some(LedgerError::DispatchUncertain)
            );
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                proxy.committed.notified(),
            )
            .await
            .unwrap();
            assert_eq!(
                ledger.authorize_dispatch(spec.id).await.err(),
                Some(LedgerError::Conflict)
            );
            // Conflict의 Transaction Drop은 rollback을 예약만 한다. 더는 쓰지 않는
            // fault pool을 닫고 실제 principal 잠금/rollback ACK로 완료를 확인한 뒤
            // 무경합 recovery를 검증한다. NOWAIT skip이나 실행 stopped의 증거가 아니다.
            tokio::time::timeout(std::time::Duration::from_secs(3), faulty.pool().close())
                .await
                .unwrap();
            let mut rollback_barrier = healthy.pool().begin().await.unwrap();
            sqlx::query("SELECT id FROM gw_principals WHERE id=$1 FOR UPDATE")
                .bind(id)
                .fetch_one(&mut *rollback_barrier)
                .await
                .unwrap();
            rollback_barrier.rollback().await.unwrap();
            let recovery =
                PgLedger::new(healthy.clone(), AccessSettings::from_config(&cfg), [1; 32]);
            sqlx::query("UPDATE gw_fixture_clock SET observed=(SELECT lease_expires_at FROM gw_requests WHERE id=$1)").bind(spec.id).execute(healthy.pool()).await.unwrap();
            assert_eq!(
                recovery.recover_expired(spec.id).await.unwrap(),
                Some(if free {
                    Finalized::Settled
                } else {
                    Finalized::Unknown
                })
            );
            assert_eq!(recovery.recover_expired(spec.id).await.unwrap(), None);
            let row=sqlx::query("SELECT r.cost_usd::text AS cost,r.execution_stopped_at IS NULL AS running,b.remaining_reservation_usd::text AS held,t.unknown_count,(SELECT normalized_usage->>'source' FROM gw_usage_events WHERE request_id=r.id) AS source,(SELECT normalized_usage->>'counted_input_tokens' FROM gw_usage_events WHERE request_id=r.id) AS counted FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id JOIN gw_usage_totals t ON t.scope_id=b.scope_id AND t.period_start=b.period_start WHERE r.id=$1").bind(spec.id).fetch_one(healthy.pool()).await.unwrap();
            assert!(row.get::<bool, _>("running"));
            assert!(row.get::<Option<String>, _>("counted").is_none());
            assert_eq!(row.get::<i64, _>("unknown_count"), i64::from(!free));
            if free {
                assert_eq!(row.get::<String, _>("cost"), "0.000000000000");
                assert_eq!(row.get::<String, _>("source"), "profile_no_charge");
                assert_eq!(row.get::<String, _>("held"), "0.000000000000");
            } else {
                assert!(row.get::<Option<String>, _>("cost").is_none());
                assert_eq!(row.get::<String, _>("held"), "0.004096000000");
            }
            if internal {
                assert_eq!(
                    recovery.recover_expired(root.id).await.unwrap(),
                    Some(Finalized::Settled)
                );
            }
            healthy.pool().close().await;
            drop(proxy);
            db.close().await;
        }
    }
    assert!(provider.calls.lock().await.is_empty());
    drop(server);
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_recovery_principal_nowait_skips_whole_candidate_and_releases_partial_locks() {
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
    let expired = finance::expired_copy(db.store.pool(), spec.id, "admitted").await;
    let mut held = db.store.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM gw_principals WHERE id=$1 FOR UPDATE")
        .bind(id)
        .execute(&mut *held)
        .await
        .unwrap();
    let skipped = ledger.recover_expired(expired).await.unwrap();
    assert!(skipped.is_none());
    held.rollback().await.unwrap();
    assert_eq!(
        ledger.recover_expired(expired).await.unwrap(),
        Some(Finalized::Settled)
    );
    let mut held = db.store.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM gw_principals WHERE id=$1 FOR UPDATE NOWAIT")
        .bind(id)
        .execute(&mut *held)
        .await
        .unwrap();
    held.rollback().await.unwrap();
    db.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_reserved_count_bound_violation_and_system_audit_are_atomic_for_both_scopes() {
    let db = postgres::TestDb::new().await;
    let id = finance::human(db.store.pool()).await;
    sqlx::query("UPDATE gw_policies SET budget_mode='reserved',budget_limit_usd=gw_amount('1.000000000000') WHERE principal_id=$1").bind(id).execute(db.store.pool()).await.unwrap();
    let current = finance::current(db.store.pool(), id).await;
    let key = key(db.store.pool(), id, Some("1.000000000000"), None, None).await;
    let cfg = chargeable_configuration(false);
    let ledger = PgLedger::new(db.store.clone(), AccessSettings::from_config(&cfg), [1; 32]);
    let a = RequestSpec::from_profile(ProviderId::Anthropic, &cfg.providers()[0].profiles[1], None);
    let b = RequestSpec::from_profile(ProviderId::Anthropic, &cfg.providers()[0].profiles[1], None);
    for spec in [&a, &b] {
        ledger
            .admit_scoped(&current, spec, Some(key), None)
            .await
            .unwrap();
        ledger.authorize_dispatch(spec.id).await.unwrap();
    }
    sqlx::raw_sql("CREATE FUNCTION gw_fixture_fail_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic audit fault'; END $$; CREATE TRIGGER gw_fixture_fail_audit BEFORE INSERT ON gw_audit_events FOR EACH ROW EXECUTE FUNCTION gw_fixture_fail_audit()").execute(db.store.pool()).await.unwrap();
    let mut usage = count_observation(false);
    usage.usage.as_mut().unwrap().input_tokens = 5000;
    usage.counted_input_tokens = Some(5000);
    assert_eq!(
        ledger.finalize_request(a.id, &usage).await.err(),
        Some(LedgerError::DatabaseUnavailable)
    );
    let intact:bool=sqlx::query_scalar("SELECT bool_and(cost_usd=0 AND reserved_usd=gw_amount('0.008192000000') AND settled_count=0 AND unknown_count=0) FROM gw_usage_totals").fetch_one(db.store.pool()).await.unwrap();
    assert!(intact);
    let safety: i64 =
        sqlx::query_scalar("SELECT inference_safety_version FROM gw_principals WHERE id=$1")
            .bind(id)
            .fetch_one(db.store.pool())
            .await
            .unwrap();
    assert_eq!(safety, 1);
    sqlx::raw_sql("DROP TRIGGER gw_fixture_fail_audit ON gw_audit_events; DROP FUNCTION gw_fixture_fail_audit()").execute(db.store.pool()).await.unwrap();
    let barrier = tokio::sync::Barrier::new(2);
    let (one, two) = tokio::join!(
        async {
            barrier.wait().await;
            ledger.finalize_request(a.id, &usage).await
        },
        async {
            barrier.wait().await;
            ledger.finalize_request(a.id, &usage).await
        }
    );
    assert!(matches!(
        (one.unwrap(), two.unwrap()),
        (Finalized::Settled, Finalized::Noop) | (Finalized::Noop, Finalized::Settled)
    ));
    ledger.finalize_request(b.id, &usage).await.unwrap();
    let row=sqlx::query("SELECT version,inference_safety_version,disabled_at IS NULL AS active,(SELECT count(*) FROM gw_audit_events WHERE actor_kind='system' AND actor_principal_id IS NULL) AS audits FROM gw_principals WHERE id=$1").bind(id).fetch_one(db.store.pool()).await.unwrap();
    let totals:bool=sqlx::query_scalar("SELECT bool_and(cost_usd=gw_amount('0.010000000000') AND reserved_usd=0 AND settled_count=2 AND unknown_count=0) FROM gw_usage_totals").fetch_one(db.store.pool()).await.unwrap();
    db.close().await;
    assert!(totals);
    assert_eq!(row.get::<i64, _>("version"), current.version);
    assert_eq!(row.get::<i64, _>("inference_safety_version"), 3);
    assert_eq!(row.get::<i64, _>("audits"), 2);
    assert!(row.get::<bool, _>("active"));
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_child_finalization_sql_failure_latches_root_family_not_two_slots() {
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
    sqlx::raw_sql("CREATE FUNCTION gw_fixture_fail_child() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF EXISTS(SELECT 1 FROM gw_requests WHERE id=NEW.request_id AND purpose='internal_count') THEN RAISE EXCEPTION 'synthetic child persistence fault'; END IF; RETURN NEW; END $$; CREATE TRIGGER gw_fixture_fail_child BEFORE INSERT ON gw_usage_events FOR EACH ROW EXECUTE FUNCTION gw_fixture_fail_child()").execute(h.db.store.pool()).await.unwrap();
    let response = h.request("/v1/messages", BODY).send().await.unwrap();
    let status = response.status();
    let _ = response.bytes().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), h.runtime.wait_requests())
        .await
        .unwrap();
    let ids = h.runtime.failed_request_ids();
    let rows = sqlx::query("SELECT id,purpose,phase FROM gw_requests ORDER BY purpose")
        .fetch_all(h.db.store.pool())
        .await
        .unwrap();
    sqlx::raw_sql("DROP TRIGGER gw_fixture_fail_child ON gw_usage_events; DROP FUNCTION gw_fixture_fail_child()").execute(h.db.store.pool()).await.unwrap();
    for dependency in [
        m_gated::runtime::Dependency::Database,
        m_gated::runtime::Dependency::Jwks,
        m_gated::runtime::Dependency::Schema,
        m_gated::runtime::Dependency::Ui,
    ] {
        h.runtime
            .record_health(dependency, true, tokio::time::Instant::now());
    }
    let ready = h.runtime.lifecycle();
    let calls = h.provider.calls.lock().await.len();
    h.close().await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(calls, 1);
    assert_eq!(rows.len(), 2);
    assert_eq!(ids.len(), 2);
    for row in &rows {
        assert!(ids.contains(&row.get::<uuid::Uuid, _>("id")));
    }
    assert_eq!(rows[0].get::<String, _>("phase"), "dispatching");
    assert_eq!(rows[1].get::<String, _>("phase"), "finished");
    assert_eq!(ready, m_gated::runtime::Lifecycle::NotReady);
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL, TEST_FNM_DIR and installed fixed SDK"]
async fn s4_fixed_sdk_paid_external_and_strict_internal_count_have_one_client_attempt() {
    let h = Harness::with_config(chargeable_configuration(false)).await;
    h.provider
        .set_count(
            StatusCode::OK,
            HeaderMap::from_iter([(
                "content-type".parse().unwrap(),
                "application/json".parse().unwrap(),
            )]),
            Bytes::from_static(br#"{"input_tokens":12}"#),
        )
        .await;
    sdk(&h, "/anthropic", "count_tokens", 200, &h.token).await;
    let cost: String = sqlx::query_scalar("SELECT cost_usd::text FROM gw_requests")
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    assert_eq!(cost, "0.000012000000");
    assert_eq!(h.provider.calls.lock().await.len(), 1);
    h.close().await;
    let cfg = configuration_from_yaml(&finance_yaml().replace(
        "api: messages",
        "api: messages\n        internal_count: true",
    ));
    let h = Harness::with_config(cfg).await;
    sqlx::query("UPDATE gw_policies SET budget_mode='reserved',budget_limit_usd=gw_amount('1.000000000000'),tpm_limit=5000 WHERE principal_id=$1").bind(h.principal).execute(h.db.store.pool()).await.unwrap();
    h.provider.hardcap(4096).await;
    h.provider
        .set_count(
            StatusCode::OK,
            HeaderMap::from_iter([(
                "content-type".parse().unwrap(),
                "application/json".parse().unwrap(),
            )]),
            Bytes::from_static(br#"{"input_tokens":12}"#),
        )
        .await;
    sdk(&h, "", "messages", 200, &h.token).await;
    let costs: Vec<String> =
        sqlx::query_scalar("SELECT cost_usd::text FROM gw_requests ORDER BY purpose")
            .fetch_all(h.db.store.pool())
            .await
            .unwrap();
    let calls = h.provider.calls.lock().await.len();
    h.close().await;
    assert_eq!(costs, vec!["0.000000000000", "0.000140000000"]);
    assert_eq!(calls, 2);
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_paid_actual_count_does_not_need_generation_tpm_bound() {
    for lane in ["principal", "key", "ceiling"] {
        let db = postgres::TestDb::new().await;
        let id = finance::human(db.store.pool()).await;
        if lane == "principal" {
            sqlx::query("UPDATE gw_policies SET tpm_limit=1 WHERE principal_id=$1")
                .bind(id)
                .execute(db.store.pool())
                .await
                .unwrap();
        }
        let current = finance::current(db.store.pool(), id).await;
        let key = if lane == "key" {
            Some(key(db.store.pool(), id, None, Some(1), None).await)
        } else {
            None
        };
        let mut profile = chargeable_configuration(false).providers()[0].profiles[1].clone();
        profile.input_bound_contract = None;
        let spec = RequestSpec::from_profile(ProviderId::Anthropic, &profile, None);
        let ledger = PgLedger::new(
            db.store.clone(),
            AccessSettings::from_config(&configuration()),
            [1; 32],
        )
        .with_ceiling(PolicyLimits {
            tpm: if lane == "ceiling" { Some(1) } else { None },
            ..Default::default()
        });
        let result = ledger.admit_scoped(&current, &spec, key, None).await;
        if result.is_err() {
            db.close().await;
            assert!(
                result.is_ok(),
                "paid actual count must not acquire a generation TPM bound: lane={lane} {:?}",
                result.err()
            );
            return;
        }
        ledger.authorize_dispatch(spec.id).await.unwrap();
        ledger
            .finalize_request(spec.id, &count_observation(false))
            .await
            .unwrap();
        let tokens: i64 = sqlx::query_scalar(
            "SELECT sum(settled_tokens+reserved_tokens)::bigint FROM gw_rate_windows",
        )
        .fetch_one(db.store.pool())
        .await
        .unwrap();
        db.close().await;
        assert_eq!(tokens, 0);
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_actual_internal_count_is_not_a_claim_of_strict_context_evidence() {
    let yaml=finance_yaml().replace("api: messages","api: messages\n        internal_count: true").replace("        input_bound_contract:\n          kind: anthropic_context_v1\n          context_tokens: 4096\n          input_categories: [input, cache_read, cache_write]\n","");
    let h = Harness::with_config(configuration_from_yaml(&yaml)).await;
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
    let calls = h.provider.calls.lock().await.len();
    h.close().await;
    assert_eq!(
        status,
        StatusCode::OK,
        "actual count does not fabricate or require strict context evidence"
    );
    assert_eq!(calls, 2);
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_count_input_limit_boundary_preserves_independent_charge() {
    for free in [false, true] {
        for counted in [4096u64, 4097] {
            let cfg = if free {
                configuration_from_yaml(&finance_yaml().replace(
                    "api: messages",
                    "api: messages\n        internal_count: true",
                ))
            } else {
                chargeable_configuration(true)
            };
            let h = Harness::with_config(cfg).await;
            h.provider
                .set_count(
                    StatusCode::OK,
                    HeaderMap::from_iter([(
                        "content-type".parse().unwrap(),
                        "application/json".parse().unwrap(),
                    )]),
                    Bytes::from(format!("{{\"input_tokens\":{counted}}}")),
                )
                .await;
            let response = h.request("/v1/messages", BODY).send().await.unwrap();
            let status = response.status();
            let _ = response.bytes().await.unwrap();
            let rows = sqlx::query(
                "SELECT purpose,cost_usd::text AS cost FROM gw_requests ORDER BY purpose",
            )
            .fetch_all(h.db.store.pool())
            .await
            .unwrap();
            let calls = h.provider.calls.lock().await.len();
            h.close().await;
            assert_eq!(
                status,
                if counted == 4096 {
                    StatusCode::OK
                } else {
                    StatusCode::BAD_REQUEST
                }
            );
            assert_eq!(calls, if counted == 4096 { 2 } else { 1 });
            assert_eq!(
                rows[1].get::<String, _>("cost"),
                if counted == 4096 {
                    "0.000140000000"
                } else {
                    "0.000000000000"
                }
            );
            assert_eq!(
                rows[0].get::<String, _>("cost"),
                if free {
                    "0.000000000000"
                } else if counted == 4096 {
                    "0.004096000000"
                } else {
                    "0.004097000000"
                }
            );
        }
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_internal_count_authorization_errors_keep_native_403() {
    let mut statuses = Vec::new();
    let mut kinds = Vec::new();
    for paused in [false, true] {
        let cfg = if paused {
            chargeable_configuration(true)
        } else {
            configuration_from_yaml(&finance_yaml().replace(
                "api: messages",
                "api: messages\n        internal_count: true",
            ))
        };
        let access = AccessSettings::from_config(&cfg);
        let digest = m_gated::gateway::auth_config_digest(&cfg);
        let h = Harness::with_config(cfg).await;
        let mut trigger = None;
        if paused {
            // FK가 유효한 기존 미송신 행을 안전차단 원인으로 둔다. S5 해제 API의 대체가 아니다.
            let current = finance::current(h.db.store.pool(), h.principal).await;
            let ledger = PgLedger::new(h.db.store.clone(), access, digest);
            let spec = specification();
            ledger.admit_request(&current, &spec).await.unwrap();
            ledger
                .finalize_request(
                    spec.id,
                    &FinalObservation {
                        outcome: m_gated::ledger::finance::FinalOutcome::Rejected,
                        usage: None,
                        counted_input_tokens: None,
                        reason: "access_denied",
                        http_status: None,
                        provider_request_id: None,
                    },
                )
                .await
                .unwrap();
            sqlx::query("UPDATE gw_principals SET inference_paused_at=clock_timestamp(),inference_pause_reason='amount_overflow',inference_pause_request_id=$2,inference_safety_version=inference_safety_version+1 WHERE id=$1").bind(h.principal).bind(spec.id).execute(h.db.store.pool()).await.unwrap();
            trigger = Some(spec.id);
        } else {
            sqlx::query("UPDATE gw_policies SET allow_rules=$2 WHERE principal_id=$1").bind(h.principal).bind(serde_json::json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}])).execute(h.db.store.pool()).await.unwrap();
        }
        let response = h.request("/v1/messages", BODY).send().await.unwrap();
        statuses.push(response.status());
        let headers = response.headers().clone();
        let body: serde_json::Value =
            serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
        kinds.push(body["error"]["type"].as_str().unwrap().to_owned());
        let rows = sqlx::query("SELECT purpose,phase,accounting_state,cost_usd::text AS cost,dispatch_authorized_at IS NULL AS not_dispatched,execution_stopped_at IS NOT NULL AS stopped FROM gw_requests WHERE $1::uuid IS NULL OR id<>$1 ORDER BY purpose").bind(trigger).fetch_all(h.db.store.pool()).await.unwrap();
        let totals: bool = sqlx::query_scalar("SELECT bool_and(cost_usd=0 AND reserved_usd=0 AND unknown_count=0) FROM gw_usage_totals").fetch_one(h.db.store.pool()).await.unwrap();
        let budgets: bool = sqlx::query_scalar("SELECT bool_and(state='settled' AND remaining_reservation_usd=0 AND remaining_reserved_tokens=0 AND applied_cost_usd=0 AND applied_input_tokens=0 AND applied_output_tokens=0) FROM gw_request_budgets").fetch_one(h.db.store.pool()).await.unwrap();
        let calls = h.provider.calls.lock().await.len();
        h.close().await;
        assert_eq!(
            headers["x-gateway-error-code"],
            if paused {
                "inference_paused"
            } else {
                "access_denied"
            }
        );
        assert_eq!(headers["x-gateway-dispatch-state"], "not_dispatched");
        assert_eq!(headers["cache-control"], "no-store");
        assert!(uuid::Uuid::parse_str(headers["x-gateway-request-id"].to_str().unwrap()).is_ok());
        assert!(!headers.contains_key("retry-after"));
        assert_eq!(calls, 0);
        assert!(totals && budgets);
        assert_eq!(rows.len(), if paused { 2 } else { 1 });
        assert_eq!(rows.last().unwrap().get::<String, _>("purpose"), "root");
        if paused {
            assert_eq!(rows[0].get::<String, _>("purpose"), "internal_count");
        }
        for row in rows {
            assert_eq!(row.get::<String, _>("phase"), "finished");
            assert_eq!(row.get::<String, _>("accounting_state"), "settled");
            assert_eq!(row.get::<String, _>("cost"), "0.000000000000");
            assert!(row.get::<bool, _>("not_dispatched") && row.get::<bool, _>("stopped"));
        }
    }
    assert_eq!(statuses, vec![StatusCode::FORBIDDEN; 2]);
    assert_eq!(kinds, vec!["permission_error"; 2]);
}

// 요율0도 chargeable이다. 무료 계약/검증된 입력 상한은 기존 profile 검증을 그대로 거친다.
fn zero_rate_count_configuration(free: bool) -> config::ValidatedConfig {
    configuration_from_yaml(&zero_rate_count_yaml(free))
}
fn zero_rate_count_yaml(free: bool) -> String {
    if free {
        finance_yaml()
    } else {
        finance_yaml().replace(
            "billing_class: guaranteed_free",
            "billing_class: chargeable\n        count_billing: counted_input_tokens\n        input_bound_contract:\n          kind: anthropic_context_v1\n          context_tokens: 4096\n          input_categories: [input, cache_read, cache_write]",
        )
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_reserved_zero_budget_rejects_zero_rate_chargeable_count() {
    let mut actual = Vec::new();
    let mut expected = Vec::new();
    for mode in ["reserved", "actual"] {
        for scope in ["principal", "key", "ceiling"] {
            for free in [false, true] {
                let db = postgres::TestDb::new().await;
                let id = finance::human(db.store.pool()).await;
                sqlx::query("UPDATE gw_policies SET budget_mode=$2,budget_limit_usd=gw_amount($3::numeric) WHERE principal_id=$1")
                    .bind(id).bind(mode).bind(if scope == "principal" {"0.000000000000"} else {"1.000000000000"}).execute(db.store.pool()).await.unwrap();
                let key = if scope == "key" {
                    Some(key(db.store.pool(), id, Some("0.000000000000"), None, None).await)
                } else {
                    None
                };
                let current = finance::current(db.store.pool(), id).await;
                let ledger = PgLedger::new(
                    db.store.clone(),
                    AccessSettings::from_config(&configuration()),
                    [1; 32],
                )
                .with_ceiling(if scope == "ceiling" {
                    limits(budget(mode, "0.000000000000", "UTC"))
                } else {
                    PolicyLimits::default()
                });
                let cfg = zero_rate_count_configuration(free);
                let spec = RequestSpec::from_profile(
                    ProviderId::Anthropic,
                    &cfg.providers()[0].profiles[1],
                    None,
                );
                assert_eq!(spec.cost_bound, Some(m_gated::ledger::amount::Amount::ZERO));
                ledger
                    .admit_scoped(&current, &spec, key, None)
                    .await
                    .unwrap();
                let error = ledger.authorize_dispatch(spec.id).await.err();
                // 실패 구현에서도 자기 schema를 정리한 뒤 전체 scope 반례를 한 assertion으로 보고한다.
                let observation = if error.is_some() {
                    FinalObservation {
                        outcome: m_gated::ledger::finance::FinalOutcome::Rejected,
                        usage: None,
                        counted_input_tokens: None,
                        reason: "budget_exhausted",
                        http_status: None,
                        provider_request_id: None,
                    }
                } else {
                    count_observation(free)
                };
                ledger
                    .finalize_request(spec.id, &observation)
                    .await
                    .unwrap();
                let totals: bool = sqlx::query_scalar("SELECT bool_and(cost_usd=0 AND reserved_usd=0 AND unknown_count=0) FROM gw_usage_totals").fetch_one(db.store.pool()).await.unwrap();
                let row = sqlx::query("SELECT r.dispatch_authorized_at IS NULL AS not_dispatched,e.normalized_usage->>'source' AS source,e.normalized_usage->>'counted_input_tokens' AS counted,(SELECT count(*) FROM gw_request_budgets WHERE request_id=r.id) AS scopes FROM gw_requests r JOIN gw_usage_events e ON e.request_id=r.id AND e.kind='final' WHERE r.id=$1").bind(spec.id).fetch_one(db.store.pool()).await.unwrap();
                let windows: bool = sqlx::query_scalar("SELECT bool_and(lane='token_count' AND admitted_count=1 AND reserved_tokens=0 AND settled_tokens=0) FROM gw_rate_windows").fetch_one(db.store.pool()).await.unwrap();
                db.close().await;
                assert!(totals && windows);
                assert_eq!(
                    row.get::<i64, _>("scopes"),
                    if key.is_some() { 2 } else { 1 }
                );
                actual.push((
                    mode,
                    scope,
                    free,
                    error,
                    row.get::<bool, _>("not_dispatched"),
                    row.get::<String, _>("source"),
                    row.get::<Option<String>, _>("counted"),
                ));
                expected.push((
                    mode,
                    scope,
                    free,
                    if free {
                        None
                    } else {
                        Some(LedgerError::BudgetExhausted)
                    },
                    !free,
                    if free {
                        "profile_no_charge"
                    } else {
                        "gateway_no_dispatch"
                    }
                    .to_owned(),
                    if free { Some("10".to_owned()) } else { None },
                ));
            }
        }
    }
    assert_eq!(
        actual, expected,
        "limit0 is a dispatch stop, not a zero-rate allowance"
    );
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_zero_budget_count_native_wire_and_free_rate_access_checks() {
    const COUNT: &str =
        r#"{"model":"synthetic-text","messages":[{"role":"user","content":"synthetic count"}]}"#;
    let mut actual = Vec::new();
    let mut expected = Vec::new();
    for mode in ["reserved", "actual"] {
        for ceiling in [false, true] {
            for free in [false, true] {
                let mut yaml = zero_rate_count_yaml(free);
                if ceiling {
                    yaml.push_str(&format!("execution_ceiling: {{budget: {{mode: {mode}, limit_usd: '0.000000000000', period: {{kind: calendar_month, timezone: UTC}}}}, count_rpm: 1}}\n"));
                }
                let h = Harness::with_clock(configuration_from_yaml(&yaml)).await;
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
                sqlx::query("UPDATE gw_policies SET budget_mode=$2,budget_limit_usd=gw_amount($3::numeric),count_rpm_limit=1 WHERE principal_id=$1")
                    .bind(h.principal).bind(mode).bind(if ceiling {"1.000000000000"} else {"0.000000000000"}).execute(h.db.store.pool()).await.unwrap();
                let response = h
                    .request("/v1/messages/count_tokens", COUNT)
                    .send()
                    .await
                    .unwrap();
                let status = response.status();
                let headers = response.headers().clone();
                let body: serde_json::Value =
                    serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
                let row = sqlx::query("SELECT r.phase,r.accounting_state,r.cost_usd::text AS cost,r.dispatch_authorized_at IS NULL AS not_dispatched,e.normalized_usage->>'source' AS source,e.normalized_usage->>'counted_input_tokens' AS counted FROM gw_requests r JOIN gw_usage_events e ON e.request_id=r.id AND e.kind='final'").fetch_one(h.db.store.pool()).await.unwrap();
                if free {
                    assert_eq!(status, StatusCode::OK);
                    assert_eq!(body["input_tokens"], 321);
                    assert_eq!(row.get::<String, _>("source"), "profile_no_charge");
                    assert_eq!(
                        row.get::<Option<String>, _>("counted"),
                        Some("321".to_owned())
                    );
                    let fixed:bool=sqlx::query_scalar("SELECT bool_and(r.admitted_at=c.observed AND b.rate_lane='token_count' AND b.rate_start=to_timestamp(floor(extract(epoch FROM c.observed)/60)*60)) FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id CROSS JOIN gw_fixture_clock c").fetch_one(h.database.pool()).await.unwrap();
                    assert!(fixed, "두alias는같은권위DB시각/60초창을사용해야한다");
                    let rate = h
                        .request("/anthropic/v1/messages/count_tokens", COUNT)
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(rate.status(), StatusCode::TOO_MANY_REQUESTS);
                    assert_eq!(rate.headers()["x-gateway-error-code"], "rate_limited");
                    assert!(rate.headers().contains_key("retry-after"));
                    let _ = rate.bytes().await.unwrap();
                    let original_window:bool=sqlx::query_scalar("SELECT count(*)=1 AND bool_and(lane='token_count' AND admitted_count=1 AND window_start=to_timestamp(floor(extract(epoch FROM c.observed)/60)*60)) FROM gw_rate_windows CROSS JOIN gw_fixture_clock c").fetch_one(h.database.pool()).await.unwrap();
                    assert!(original_window, "첫count의원래창/RPM1을유지해야한다");
                    sqlx::query(
                        "UPDATE gw_policies SET allow_rules='[]'::jsonb WHERE principal_id=$1",
                    )
                    .bind(h.principal)
                    .execute(h.db.store.pool())
                    .await
                    .unwrap();
                    let denied = h
                        .request("/anthropic/v1/messages/count_tokens", COUNT)
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
                    assert_eq!(denied.headers()["x-gateway-error-code"], "access_denied");
                    let _ = denied.bytes().await.unwrap();
                }
                let totals: bool = sqlx::query_scalar("SELECT bool_and(cost_usd=0 AND reserved_usd=0 AND unknown_count=0) FROM gw_usage_totals").fetch_one(h.db.store.pool()).await.unwrap();
                let budgets: bool = sqlx::query_scalar("SELECT bool_and(state='settled' AND remaining_reservation_usd=0 AND remaining_reserved_tokens=0 AND applied_cost_usd=0 AND applied_input_tokens=0 AND applied_output_tokens=0) FROM gw_request_budgets").fetch_one(h.db.store.pool()).await.unwrap();
                let windows: bool = sqlx::query_scalar("SELECT bool_and(lane='token_count' AND admitted_count=1 AND reserved_tokens=0 AND settled_tokens=0) FROM gw_rate_windows").fetch_one(h.db.store.pool()).await.unwrap();
                let calls = h.provider.calls.lock().await.len();
                let requests: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_requests")
                    .fetch_one(h.db.store.pool())
                    .await
                    .unwrap();
                h.close().await;
                assert_eq!(requests, 1);
                assert_eq!(row.get::<String, _>("phase"), "finished");
                assert_eq!(row.get::<String, _>("accounting_state"), "settled");
                assert_eq!(row.get::<String, _>("cost"), "0.000000000000");
                assert_eq!(headers["cache-control"], "no-store");
                assert!(
                    uuid::Uuid::parse_str(headers["x-gateway-request-id"].to_str().unwrap())
                        .is_ok()
                );
                assert!(!headers.contains_key("retry-after"));
                if !free {
                    actual.push((
                        mode,
                        ceiling,
                        status.as_u16(),
                        headers
                            .get("x-gateway-error-code")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned),
                        headers
                            .get("x-gateway-dispatch-state")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned),
                        body["error"]["type"].as_str().map(str::to_owned),
                        row.get::<bool, _>("not_dispatched"),
                        calls,
                        totals,
                        budgets,
                        windows,
                    ));
                    expected.push((
                        mode,
                        ceiling,
                        429,
                        Some("budget_exhausted".to_owned()),
                        Some("not_dispatched".to_owned()),
                        Some("rate_limit_error".to_owned()),
                        true,
                        0,
                        true,
                        true,
                        true,
                    ));
                } else {
                    assert!(totals && budgets && windows);
                    assert_eq!(calls, 1);
                }
            }
        }
    }
    assert_eq!(
        actual, expected,
        "zero-rate chargeable count must be rejected on the native wire"
    );
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_positive_reserved_count_exact_budget_is_allowed_in_every_scope() {
    for scope in ["principal", "key", "ceiling"] {
        let db = postgres::TestDb::new().await;
        let id = finance::human(db.store.pool()).await;
        let cfg = chargeable_configuration(false);
        let spec =
            RequestSpec::from_profile(ProviderId::Anthropic, &cfg.providers()[0].profiles[1], None);
        let bound = spec.cost_bound.unwrap().to_string();
        sqlx::query("UPDATE gw_policies SET budget_mode='reserved',budget_limit_usd=gw_amount($2::numeric) WHERE principal_id=$1").bind(id).bind(if scope == "principal" {&bound} else {"1.000000000000"}).execute(db.store.pool()).await.unwrap();
        let key = if scope == "key" {
            Some(key(db.store.pool(), id, Some(&bound), None, None).await)
        } else {
            None
        };
        let current = finance::current(db.store.pool(), id).await;
        let ledger = PgLedger::new(
            db.store.clone(),
            AccessSettings::from_config(&configuration()),
            [1; 32],
        )
        .with_ceiling(if scope == "ceiling" {
            limits(budget("reserved", &bound, "UTC"))
        } else {
            PolicyLimits::default()
        });
        ledger
            .admit_scoped(&current, &spec, key, None)
            .await
            .unwrap();
        let permit = ledger.authorize_dispatch(spec.id).await;
        let reservations: Vec<String> =
            sqlx::query_scalar("SELECT reserved_usd::text FROM gw_usage_totals ORDER BY scope_id")
                .fetch_all(db.store.pool())
                .await
                .unwrap();
        if permit.is_ok() {
            ledger
                .finalize_request(spec.id, &count_observation(false))
                .await
                .unwrap();
        }
        db.close().await;
        assert!(
            permit.is_ok(),
            "positive exact reservation rejected: {scope} {:?}",
            permit.err()
        );
        assert_eq!(reservations, vec![bound; if key.is_some() { 2 } else { 1 }]);
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s4_count_alias_same_window_rejects_and_explicit_next_window_allows() {
    let h = Harness::with_clock(zero_rate_count_configuration(true)).await;
    sqlx::query("UPDATE gw_policies SET count_rpm_limit=1 WHERE principal_id=$1")
        .bind(h.principal)
        .execute(h.db.store.pool())
        .await
        .unwrap();
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
    let body = r#"{"model":"synthetic-text","messages":[{"role":"user","content":"synthetic count window"}]}"#;
    for (path, status) in [
        ("/v1/messages/count_tokens", 200),
        ("/anthropic/v1/messages/count_tokens", 429),
    ] {
        let r = h.request(path, body).send().await.unwrap();
        assert_eq!(r.status().as_u16(), status);
        r.bytes().await.unwrap();
    }
    let same:bool=sqlx::query_scalar("SELECT count(*)=1 AND bool_and(lane='token_count' AND admitted_count=1) FROM gw_rate_windows").fetch_one(h.database.pool()).await.unwrap();
    assert!(same);
    sqlx::query("UPDATE gw_fixture_clock SET observed=observed+interval '60 seconds'")
        .execute(h.database.pool())
        .await
        .unwrap();
    let r = h
        .request("/anthropic/v1/messages/count_tokens", body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.bytes().await.unwrap();
    let rows=sqlx::query("SELECT lane,admitted_count,extract(epoch FROM window_start-lag(window_start) OVER(ORDER BY window_start))::bigint AS delta FROM gw_rate_windows ORDER BY window_start").fetch_all(h.database.pool()).await.unwrap();
    assert_eq!(rows.len(), 2);
    for r in &rows {
        assert_eq!(r.get::<String, _>("lane"), "token_count");
        assert_eq!(r.get::<i64, _>("admitted_count"), 1);
    }
    assert_eq!(rows[1].get::<i64, _>("delta"), 60);
    assert_eq!(h.provider.calls.lock().await.len(), 2);
    h.close().await;
}
