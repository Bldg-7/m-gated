use m_gated::{
    config::Limits,
    runtime::{AdmissionError, Dependency, Lifecycle, RequestLease, RouteClass, Runtime},
};
use tokio::time::{Duration, Instant, advance};
use uuid::Uuid;

fn observe_all(runtime: &Runtime, healthy: bool) {
    for d in [
        Dependency::Database,
        Dependency::Jwks,
        Dependency::Schema,
        Dependency::Ui,
    ] {
        runtime.record_health(d, healthy, Instant::now());
    }
}
fn serving() -> std::sync::Arc<Runtime> {
    let runtime = Runtime::new(Limits::default()).unwrap();
    runtime.start_http().unwrap();
    observe_all(&runtime, true);
    runtime
}
fn identified(runtime: &std::sync::Arc<Runtime>, id: Uuid) -> RequestLease {
    let mut lease = runtime.try_admit(RouteClass::Inference).unwrap();
    lease.begin_authentication().unwrap();
    lease.identify(id).unwrap();
    lease
}

#[tokio::test(start_paused = true)]
async fn one_bounded_health_monitor_times_out_recovers_and_stops_on_drain() {
    use m_gated::runtime::HealthReport;
    use tokio::sync::{mpsc, oneshot};
    let runtime = Runtime::new(Limits::default()).unwrap();
    runtime.start_http().unwrap();
    let (checks, mut entered) = mpsc::channel::<oneshot::Sender<HealthReport>>(1);
    let task = tokio::spawn(runtime.clone().monitor(move || {
        let checks = checks.clone();
        async move {
            let (report, receive) = oneshot::channel();
            checks.send(report).await.unwrap();
            receive.await.unwrap_or_default()
        }
    }));
    let first = entered.recv().await.unwrap();
    assert_eq!(
        runtime
            .clone()
            .monitor(|| async { HealthReport::default() })
            .await,
        Err(AdmissionError::InvalidTransition)
    );
    assert_eq!(runtime.lifecycle(), Lifecycle::NotReady);
    assert!(
        first
            .send(HealthReport {
                database: true,
                jwks: true,
                schema: true,
                ui: true
            })
            .is_ok()
    );
    advance(Duration::from_secs(5)).await;
    let stalled = entered.recv().await.unwrap();
    assert_eq!(runtime.lifecycle(), Lifecycle::Ready);
    advance(Duration::from_secs(26)).await;
    let recovery = entered.recv().await.unwrap();
    assert_eq!(
        runtime.lifecycle(),
        Lifecycle::NotReady,
        "5초 health timeout은 실패 관찰을 기록한다"
    );
    assert!(
        stalled.send(HealthReport::default()).is_err(),
        "만료된 작업은 취소되며 늦은 성공을 반영하지 않는다"
    );
    assert!(
        entered.try_recv().is_err(),
        "missed tick을 task queue로 늘리지 않는다"
    );
    assert!(
        recovery
            .send(HealthReport {
                database: true,
                jwks: true,
                schema: true,
                ui: true
            })
            .is_ok()
    );
    advance(Duration::from_secs(5)).await;
    let in_progress = entered.recv().await.unwrap();
    assert_eq!(runtime.lifecycle(), Lifecycle::Ready);
    runtime.begin_draining();
    assert_eq!(task.await.unwrap(), Ok(()));
    assert!(in_progress.send(HealthReport::default()).is_err());
    assert!(entered.recv().await.is_none());
}

#[tokio::test]
async fn one_http_admission_cannot_be_reused_for_two_business_leases_or_other_runtime() {
    let runtime = serving();
    let other = serving();
    let http = runtime.try_http().unwrap();
    assert_eq!(
        other
            .try_admit_with_http(RouteClass::Inference, http.clone())
            .err(),
        Some(AdmissionError::InvalidTransition)
    );
    let lease = runtime
        .try_admit_with_http(RouteClass::Inference, http.clone())
        .unwrap();
    assert_eq!(
        runtime
            .try_admit_with_http(RouteClass::Inference, http.clone())
            .err(),
        Some(AdmissionError::InvalidTransition)
    );
    drop(lease);
    assert_eq!(
        runtime
            .try_admit_with_http(RouteClass::Inference, http)
            .err(),
        Some(AdmissionError::InvalidTransition)
    );
}

#[tokio::test]
async fn response_body_and_each_retained_frame_keep_admission_permits() {
    use axum::{body::Body, response::IntoResponse};
    use http_body::Body as _;
    let runtime = serving();
    let lease = identified(&runtime, Uuid::new_v4());
    let response = lease.bind_response(Body::from(vec![42_u8; 1024]).into_response());
    assert_eq!(
        runtime.available_inference(),
        15,
        "본문이 살아 있는 동안 permit을 반환하지 않는다"
    );
    let mut body = response.into_body();
    let frame = std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx))
        .await
        .unwrap()
        .unwrap();
    drop(body);
    assert_eq!(
        runtime.available_inference(),
        15,
        "frame의 bytes가 살아 있어도 permit을 보유한다"
    );
    drop(frame);
    assert_eq!(runtime.available_inference(), 16);
    assert_eq!(runtime.active_principals(), 0);
}

#[tokio::test(start_paused = true)]
async fn recent_health_drives_ready_expiry_failure_and_recovery() {
    let runtime = Runtime::new(Limits::default()).unwrap();
    assert_eq!(runtime.lifecycle(), Lifecycle::Starting);
    assert_eq!(
        runtime.try_admit(RouteClass::Inference).err(),
        Some(AdmissionError::NotReady)
    );
    runtime.start_http().unwrap();
    assert_eq!(runtime.lifecycle(), Lifecycle::NotReady);
    observe_all(&runtime, true);
    assert_eq!(
        runtime.lifecycle(),
        Lifecycle::Ready,
        "완전하고 유효한 최근 관찰은 Ready가 된다"
    );
    advance(Duration::from_millis(9999)).await;
    assert_eq!(runtime.lifecycle(), Lifecycle::Ready);
    advance(Duration::from_millis(1)).await;
    assert_eq!(
        runtime.lifecycle(),
        Lifecycle::NotReady,
        "정각부터 오래된 health는 사용할 수 없다"
    );
    assert_eq!(
        runtime.try_admit(RouteClass::Inference).err(),
        Some(AdmissionError::NotReady)
    );
    observe_all(&runtime, true);
    for dependency in [
        Dependency::Database,
        Dependency::Jwks,
        Dependency::Schema,
        Dependency::Ui,
    ] {
        runtime.record_health(dependency, false, Instant::now());
        assert_eq!(runtime.lifecycle(), Lifecycle::NotReady);
        assert_eq!(
            runtime.try_admit(RouteClass::Inference).err(),
            Some(AdmissionError::NotReady)
        );
        runtime.record_health(dependency, true, Instant::now());
        assert_eq!(runtime.lifecycle(), Lifecycle::Ready);
    }
    runtime.record_health(
        Dependency::Database,
        false,
        Instant::now() - Duration::from_secs(1),
    );
    runtime.record_health(
        Dependency::Database,
        false,
        Instant::now() + Duration::from_secs(1),
    );
    assert_eq!(
        runtime.lifecycle(),
        Lifecycle::Ready,
        "역순/미래 관찰은 최신 결과를 오염시키지 않는다"
    );
}

#[tokio::test]
async fn latch_and_ui_failure_do_not_lock_out_valid_management_recovery() {
    let runtime = serving();
    runtime.record_finalization_failure();
    assert_eq!(runtime.lifecycle(), Lifecycle::NotReady);
    assert_eq!(
        runtime.try_admit(RouteClass::Inference).err(),
        Some(AdmissionError::NotReady)
    );
    runtime.record_health(Dependency::Ui, false, Instant::now());
    for class in [RouteClass::ManagementRead, RouteClass::ManagementWrite] {
        assert!(runtime.try_admit(class).is_ok());
    }
    runtime.record_health(Dependency::Database, false, Instant::now());
    assert_eq!(
        runtime.try_admit(RouteClass::ManagementRead).err(),
        Some(AdmissionError::NotReady)
    );
    observe_all(&runtime, true);
    assert_eq!(
        runtime.lifecycle(),
        Lifecycle::NotReady,
        "DB 복구만으로 마감 latch를 해제하지 않는다"
    );
}

#[tokio::test]
async fn drain_is_monotonic_and_never_reopens_from_health_or_start() {
    let runtime = serving();
    let lease = runtime.try_admit(RouteClass::Inference).unwrap();
    runtime.begin_draining();
    assert_eq!(runtime.lifecycle(), Lifecycle::Draining);
    assert_eq!(
        runtime.available_inference(),
        15,
        "진행 요청 permit은 drain 시작 때 반환하지 않는다"
    );
    observe_all(&runtime, true);
    for class in [
        RouteClass::Inference,
        RouteClass::ManagementRead,
        RouteClass::ManagementWrite,
        RouteClass::Asset,
    ] {
        assert_eq!(
            runtime.try_admit(class).err(),
            Some(AdmissionError::Draining)
        );
    }
    assert_eq!(runtime.start_http(), Err(AdmissionError::InvalidTransition));
    drop(lease);
    assert_eq!(runtime.available_inference(), 16);
    runtime.begin_stopping().unwrap();
    assert_eq!(runtime.lifecycle(), Lifecycle::Stopping);
    assert_eq!(runtime.start_http(), Err(AdmissionError::InvalidTransition));
    runtime.stop();
    runtime.begin_draining();
    assert_eq!(runtime.lifecycle(), Lifecycle::Stopped);
}

#[tokio::test]
async fn admission_is_immediate_bounded_and_rolls_back_partial_acquisition() {
    let runtime = serving();
    let mut leases = Vec::new();
    for _ in 0..4 {
        leases.push(runtime.try_admit(RouteClass::Inference).unwrap());
    }
    assert_eq!(
        runtime.try_admit(RouteClass::Inference).err(),
        Some(AdmissionError::AuthenticationCapacity)
    );
    assert_eq!(
        runtime.available_inference(),
        12,
        "실패 시 부분 획득한 permit을 남기지 않는다"
    );
    drop(leases);
    assert_eq!(runtime.available_inference(), 16);
    let principal = Uuid::new_v4();
    let leases: Vec<_> = (0..4).map(|_| identified(&runtime, principal)).collect();
    let mut excess = runtime.try_admit(RouteClass::Inference).unwrap();
    excess.begin_authentication().unwrap();
    assert_eq!(excess.identify(principal), Err(AdmissionError::Concurrency));
    assert_eq!(runtime.active_principals(), 1);
    drop(excess);
    drop(leases);
    assert_eq!(
        runtime.active_principals(),
        0,
        "마지막 lease 뒤 동적 주체 map은 사라진다"
    );
    let mut all = Vec::new();
    for _ in 0..16 {
        all.push(identified(&runtime, Uuid::new_v4()));
    }
    assert_eq!(
        runtime.try_admit(RouteClass::Inference).err(),
        Some(AdmissionError::Concurrency)
    );
    assert!(
        runtime.try_admit(RouteClass::ManagementRead).is_ok(),
        "관리 permit은 추론 슬롯을 빌리지 않는다"
    );
    assert!(runtime.try_admit(RouteClass::Asset).is_ok());
    drop(all);
    assert_eq!(runtime.available_inference(), 16);
    assert_eq!(runtime.active_principals(), 0);
}

#[tokio::test]
async fn management_write_assets_nonstream_and_global_http_gates_are_independent() {
    let runtime = serving();
    let write = runtime.try_admit(RouteClass::ManagementWrite).unwrap();
    assert_eq!(
        runtime.try_admit(RouteClass::ManagementWrite).err(),
        Some(AdmissionError::Concurrency)
    );
    let reads: Vec<_> = (0..3)
        .map(|_| runtime.try_admit(RouteClass::ManagementRead).unwrap())
        .collect();
    assert_eq!(
        runtime.try_admit(RouteClass::ManagementRead).err(),
        Some(AdmissionError::Concurrency)
    );
    drop(write);
    drop(reads);
    let assets: Vec<_> = (0..8)
        .map(|_| runtime.try_admit(RouteClass::Asset).unwrap())
        .collect();
    assert_eq!(
        runtime.try_admit(RouteClass::Asset).err(),
        Some(AdmissionError::Concurrency)
    );
    drop(assets);
    let mut leases: Vec<_> = (0..4)
        .map(|_| identified(&runtime, Uuid::new_v4()))
        .collect();
    for lease in &mut leases {
        lease.acquire_nonstream().unwrap();
    }
    let mut fifth = identified(&runtime, Uuid::new_v4());
    assert_eq!(fifth.acquire_nonstream(), Err(AdmissionError::Concurrency));
    drop(leases);
    fifth.acquire_nonstream().unwrap();
    drop(fifth);
    let tiny = Limits {
        http_requests: 4,
        inference: 4,
        management: 4,
        assets: 4,
        ..Limits::default()
    };
    let runtime = Runtime::new(tiny).unwrap();
    runtime.start_http().unwrap();
    observe_all(&runtime, true);
    let leases: Vec<_> = (0..4)
        .map(|_| runtime.try_admit(RouteClass::Asset).unwrap())
        .collect();
    assert_eq!(
        runtime.try_admit(RouteClass::ManagementRead).err(),
        Some(AdmissionError::HttpCapacity)
    );
    drop(leases);
    assert!(runtime.try_admit(RouteClass::ManagementRead).is_ok());
}

#[test]
fn failed_root_families_keep_each_child_without_spending_another_root_slot() {
    let runtime = Runtime::new(Limits::default()).unwrap();
    let mut expected = std::collections::BTreeSet::new();
    for _ in 0..16 {
        let root = uuid::Uuid::new_v4();
        let child = uuid::Uuid::new_v4();
        runtime.record_family_finalization_failure(root, Some(child));
        runtime.record_family_finalization_failure(root, Some(child));
        expected.extend([root, child]);
    }
    assert_eq!(
        runtime
            .failed_request_ids()
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        expected
    );
    runtime.record_family_finalization_failure(uuid::Uuid::new_v4(), Some(uuid::Uuid::new_v4()));
    assert_eq!(runtime.failed_request_ids().len(), 32);
}
