//! env_clear인 별도 시험 OS worker만 사용한다. 제품 binary/기본빌드에는 포함되지 않는다.
use super::*;
use m_gated::{ledger::PgIdentityStore, runtime::Runtime};
use serde_json::{Value, json};
use std::{
    io::{BufRead, Write},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

struct Worker {
    child: Child,
    input: ChildStdin,
    output: tokio::io::Lines<BufReader<ChildStdout>>,
}
impl Worker {
    async fn spawn(db: &postgres::TestDb, job: Value) -> Self {
        let raw = std::env::var("TEST_DATABASE_URL").unwrap();
        postgres::test_options(&raw).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "s10_process::recovery_process_worker",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("TEST_DATABASE_URL", raw)
            .env("MGATED_TEST_RECOVERY_SCHEMA", &db.schema)
            .env("MGATED_TEST_RECOVERY_JOB", job.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap()).lines();
        Self {
            child,
            input,
            output,
        }
    }
    async fn event(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(8), async {
            while let Some(line) = self.output.next_line().await.unwrap() {
                if let Some(event) = line.strip_prefix("S10_RECOVERY_EVENT ") {
                    return serde_json::from_str(event).unwrap();
                }
            }
            panic!("OS worker event 누락")
        })
        .await
        .unwrap()
    }
    async fn proceed(&mut self) {
        self.input.write_all(b"continue\n").await.unwrap();
        self.input.flush().await.unwrap();
    }
    async fn kill(mut self) {
        self.child.kill().await.unwrap();
        assert!(!self.child.wait().await.unwrap().success());
    }
    async fn finish(mut self) {
        assert!(
            tokio::time::timeout(Duration::from_secs(5), self.child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}
fn emit(value: Value) {
    println!("S10_RECOVERY_EVENT {value}");
    std::io::stdout().flush().unwrap();
}
fn barrier() {
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).unwrap();
    assert_eq!(line, "continue\n");
}

#[tokio::test]
#[ignore = "test-only child entry; run only through owning process matrix, exclude from bulk PG"]
async fn recovery_process_worker() {
    let raw = std::env::var("TEST_DATABASE_URL").unwrap();
    let options = postgres::test_options(&raw).unwrap();
    let schema = std::env::var("MGATED_TEST_RECOVERY_SCHEMA").unwrap();
    assert!(
        schema.starts_with("gw_test_")
            && schema.len() == 40
            && schema[8..].bytes().all(|b| b.is_ascii_hexdigit())
    );
    let job: Value =
        serde_json::from_str(&std::env::var("MGATED_TEST_RECOVERY_JOB").unwrap()).unwrap();
    let path = format!("{schema},pg_catalog");
    let pool = PgIdentityStore::pool_options()
        .connect_with(
            options
                .port(job["port"].as_u64().unwrap_or(64603) as u16)
                .options([("search_path", path.as_str())]),
        )
        .await
        .unwrap();
    let store = Arc::new(PgIdentityStore::new(pool));
    let free = job["free"].as_bool().unwrap_or(true);
    let cfg = if free {
        configuration()
    } else {
        s4::chargeable_configuration(false)
    };
    let ledger = PgLedger::new(store.clone(), AccessSettings::from_config(&cfg), [1; 32]);
    match job["action"].as_str().unwrap() {
        "prepare" => {
            let user = uuid::Uuid::parse_str(job["user"].as_str().unwrap()).unwrap();
            let current = finance::current(store.pool(), user).await;
            let root = s4::strict_specification();
            let internal = job["internal"].as_bool().unwrap();
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
            emit(json!({"event":"admitted","id":spec.id,"root":internal.then_some(root.id)}));
            barrier();
            let dispatch = ledger.authorize_dispatch(spec.id).await;
            emit(
                json!({"event":"dispatch","authorized":dispatch.is_ok(),"uncertain":matches!(dispatch,Err(LedgerError::DispatchUncertain))}),
            );
            barrier();
        }
        action => {
            let id = uuid::Uuid::parse_str(job["id"].as_str().unwrap()).unwrap();
            let result = match action {
                "reap" => ledger.recover_expired(id).await.unwrap(),
                "final" => Some(
                    ledger
                        .finalize_request(id, &s4::count_observation(free))
                        .await
                        .unwrap(),
                ),
                "lost_final" => match ledger
                    .finalize_request(
                        id,
                        &FinalObservation {
                            usage: None,
                            counted_input_tokens: None,
                            ..s4::count_observation(free)
                        },
                    )
                    .await
                {
                    Ok(value) => Some(value),
                    Err(LedgerError::Conflict) => None,
                    Err(_) => panic!("test finalization failed"),
                },
                _ => panic!("unknown test worker action"),
            };
            let no_permit = ledger.authorize_dispatch(id).await.is_err();
            emit(json!({"event":"recovered","changed":result.is_some(),"no_permit":no_permit}));
        }
    }
    store.pool().close().await;
}

async fn perform(db: &postgres::TestDb, id: uuid::Uuid, free: bool, action: &str) -> Value {
    let mut worker = Worker::spawn(db, json!({"action":action,"id":id,"free":free})).await;
    let result = worker.event().await;
    assert_eq!(result["event"], "recovered");
    assert_eq!(result["no_permit"], true, "복구 뒤 추가 SendPermit 금지");
    worker.finish().await;
    result
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL; OS kill and multi-process count recovery matrix"]
async fn killed_admit_dispatch_and_free_paid_count_edges_recover_in_other_process_without_stopped_or_repermit()
 {
    for internal in [false, true] {
        for free in [false, true] {
            for offset in [-1i64, 0, 1] {
                for reaper_first in [false, true] {
                    let db = postgres::TestDb::new().await;
                    let clock = db.clock_store(None).await;
                    let user = finance::human(clock.pool()).await;
                    sqlx::query("UPDATE gw_policies SET budget_mode='reserved',budget_limit_usd=1 WHERE principal_id=$1").bind(user).execute(clock.pool()).await.unwrap();
                    let mut writer = Worker::spawn(
                        &db,
                        json!({"action":"prepare","user":user,"free":free,"internal":internal}),
                    )
                    .await;
                    let admitted = writer.event().await;
                    let id = uuid::Uuid::parse_str(admitted["id"].as_str().unwrap()).unwrap();
                    writer.proceed().await;
                    assert!(writer.event().await["authorized"].as_bool().unwrap());
                    writer.kill().await;
                    sqlx::query("UPDATE gw_fixture_clock SET observed=(SELECT lease_expires_at+make_interval(secs=>$2) FROM gw_requests WHERE id=$1)").bind(id).bind(offset as f64).execute(clock.pool()).await.unwrap();
                    if reaper_first {
                        let reaped = perform(&db, id, free, "reap").await;
                        assert_eq!(reaped["changed"], offset >= 0);
                    }
                    perform(&db, id, free, "lost_final").await;
                    if !reaper_first {
                        perform(&db, id, free, "reap").await;
                    }
                    let row=sqlx::query("SELECT r.accounting_state,r.cost_usd::text AS cost,r.execution_stopped_at IS NULL AS stopped_unproven,(SELECT normalized_usage->>'source' FROM gw_usage_events WHERE request_id=r.id ORDER BY id LIMIT 1) AS source,(SELECT normalized_usage->>'counted_input_tokens' FROM gw_usage_events WHERE request_id=r.id ORDER BY id LIMIT 1) AS counted,b.remaining_reservation_usd::text AS reserved,t.unknown_count,t.settled_count FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id JOIN gw_usage_totals t ON t.scope_id=b.scope_id AND t.period_start=b.period_start WHERE r.id=$1").bind(id).fetch_one(clock.pool()).await.unwrap();
                    assert_eq!(
                        row.get::<String, _>("accounting_state"),
                        if free { "settled" } else { "unknown" }
                    );
                    assert!(row.get::<bool, _>("stopped_unproven"));
                    assert!(row.get::<Option<String>, _>("counted").is_none());
                    assert_eq!(row.get::<i64, _>("unknown_count"), i64::from(!free));
                    if free {
                        assert_eq!(row.get::<String, _>("cost"), "0.000000000000");
                        assert_eq!(row.get::<String, _>("source"), "profile_no_charge");
                        assert_eq!(row.get::<String, _>("reserved"), "0.000000000000");
                        assert_eq!(row.get::<i64, _>("settled_count"), 1);
                    } else {
                        assert!(row.get::<Option<String>, _>("cost").is_none());
                        assert_ne!(row.get::<String, _>("reserved"), "0.000000000000");
                    }
                    if let Some(root) = admitted["root"].as_str() {
                        let root = uuid::Uuid::parse_str(root).unwrap();
                        perform(&db, root, free, "reap").await;
                        let parent:bool=sqlx::query_scalar("SELECT phase='finished' AND accounting_state='settled' AND cost_usd=0 AND execution_stopped_at IS NULL FROM gw_requests WHERE id=$1").bind(root).fetch_one(clock.pool()).await.unwrap();
                        assert!(parent);
                    }
                    clock.pool().close().await;
                    db.close().await;
                }
            }
        }
    }
    // admitted barrier에서 실제 kill한 경우에는 승인 없이 자기 미송신0으로 복구한다.
    let db = postgres::TestDb::new().await;
    let clock = db.clock_store(None).await;
    let user = finance::human(clock.pool()).await;
    let mut writer = Worker::spawn(
        &db,
        json!({"action":"prepare","user":user,"free":false,"internal":false}),
    )
    .await;
    let event = writer.event().await;
    let id = uuid::Uuid::parse_str(event["id"].as_str().unwrap()).unwrap();
    writer.kill().await;
    sqlx::query("UPDATE gw_fixture_clock SET observed=(SELECT admit_expires_at FROM gw_requests WHERE id=$1)").bind(id).execute(clock.pool()).await.unwrap();
    perform(&db, id, false, "reap").await;
    let zero:bool=sqlx::query_scalar("SELECT accounting_state='settled' AND cost_usd=0 AND dispatch_authorized_at IS NULL AND execution_stopped_at IS NULL FROM gw_requests WHERE id=$1").bind(id).fetch_one(clock.pool()).await.unwrap();
    assert!(zero);
    clock.pool().close().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL; real COMMIT ACK loss and OS recovery"]
async fn free_paid_external_internal_uncertain_dispatch_recovers_in_other_process() {
    for internal in [false, true] {
        for free in [false, true] {
            let db = postgres::TestDb::new().await;
            let clock = db.clock_store(None).await;
            let user = finance::human(clock.pool()).await;
            sqlx::query("UPDATE gw_policies SET budget_mode='reserved',budget_limit_usd=1 WHERE principal_id=$1").bind(user).execute(clock.pool()).await.unwrap();
            let proxy = pg_proxy::CommitProxy::new().await;
            let mut writer=Worker::spawn(&db,json!({"action":"prepare","user":user,"free":free,"internal":internal,"port":proxy.port})).await;
            let event = writer.event().await;
            let id = uuid::Uuid::parse_str(event["id"].as_str().unwrap()).unwrap();
            proxy.armed.store(true, std::sync::atomic::Ordering::SeqCst);
            writer.proceed().await;
            let dispatched = writer.event().await;
            assert_eq!(dispatched["uncertain"], true);
            assert_eq!(dispatched["authorized"], false);
            writer.kill().await;
            tokio::time::timeout(Duration::from_secs(2), proxy.committed.notified())
                .await
                .unwrap();
            drop(proxy);
            sqlx::query("UPDATE gw_fixture_clock SET observed=(SELECT lease_expires_at FROM gw_requests WHERE id=$1)").bind(id).execute(clock.pool()).await.unwrap();
            perform(&db, id, free, "reap").await;
            let correct:bool=sqlx::query_scalar("SELECT execution_stopped_at IS NULL AND CASE WHEN $2 THEN accounting_state='settled' AND cost_usd=0 ELSE accounting_state='unknown' AND cost_usd IS NULL END FROM gw_requests WHERE id=$1").bind(id).bind(free).fetch_one(clock.pool()).await.unwrap();
            assert!(correct);
            clock.pool().close().await;
            db.close().await;
        }
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL; partial durable family latch"]
async fn partially_finished_family_keeps_latch_until_other_os_worker_confirms_child() {
    let db = postgres::TestDb::new().await;
    let clock = db.clock_store(None).await;
    let cfg = configuration();
    let user = finance::human(clock.pool()).await;
    let current = finance::current(clock.pool(), user).await;
    let ledger = PgLedger::new(clock.clone(), AccessSettings::from_config(&cfg), [1; 32]);
    let root = s4::strict_specification();
    ledger.admit_request(&current, &root).await.unwrap();
    let mut child =
        RequestSpec::from_profile(ProviderId::Anthropic, &cfg.providers()[0].profiles[1], None);
    child.deadline = root.deadline;
    ledger
        .admit_scoped(&current, &child, None, Some(root.id))
        .await
        .unwrap();
    ledger.authorize_dispatch(child.id).await.unwrap();
    let runtime = Runtime::new(cfg.limits().clone()).unwrap();
    runtime.record_family_finalization_failure(root.id, Some(child.id));
    ledger
        .finalize_request(
            root.id,
            &FinalObservation {
                usage: None,
                ..observation()
            },
        )
        .await
        .unwrap();
    ledger.recovery_tick(&runtime).await.unwrap();
    assert_eq!(
        runtime.failed_request_ids().len(),
        2,
        "부분 P 가족을 조기 해제하면 안 된다"
    );
    sqlx::query("UPDATE gw_fixture_clock SET observed=(SELECT lease_expires_at FROM gw_requests WHERE id=$1)").bind(child.id).execute(clock.pool()).await.unwrap();
    perform(&db, child.id, true, "reap").await;
    assert_eq!(
        runtime.failed_request_ids().len(),
        2,
        "DB 연결 회복/다른 프로세스 마감만으로 로컬latch를 추측하지 않는다"
    );
    ledger.recovery_tick(&runtime).await.unwrap();
    assert!(runtime.failed_request_ids().is_empty());
    clock.pool().close().await;
    db.close().await;
}
