pub mod panic;
pub mod request;
use crate::config::Limits;
pub use request::{RequestControl, RequestOwner};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, watch},
    time::Instant,
};
use uuid::Uuid;

/// 클라이언트 입력과 무관한 입장 기준 절대 기한. DB에는 같은 측정점의 잔여만 매핑한다.
#[derive(Clone, Copy)]
pub struct RequestDeadline {
    transport: Instant,
    execution: Instant,
}
impl RequestDeadline {
    pub fn new(transport: Instant, execution: Instant) -> Self {
        Self {
            transport,
            execution,
        }
    }
    pub fn remaining_at(self, observed: Instant) -> Option<(u64, u64)> {
        let transport = self.transport.checked_duration_since(observed)?.as_millis();
        let execution = self.execution.checked_duration_since(observed)?.as_millis();
        if transport == 0 || transport >= execution || execution > 900_000 {
            return None;
        }
        Some((transport as u64, execution as u64))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    Starting,
    NotReady,
    Ready,
    Draining,
    Stopping,
    Stopped,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Dependency {
    Database,
    Jwks,
    Schema,
    Ui,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteClass {
    Inference,
    ManagementRead,
    ManagementWrite,
    Asset,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionError {
    NotReady,
    Draining,
    HttpCapacity,
    AuthenticationCapacity,
    Concurrency,
    InvalidTransition,
}
#[derive(Clone, Copy)]
struct Observation {
    healthy: bool,
    observed_at: Instant,
}
struct PrincipalGate {
    semaphore: Arc<Semaphore>,
    leases: usize,
}
struct State {
    phase: Lifecycle,
    health: BTreeMap<Dependency, Observation>,
    finalization_latch: bool,
    unidentified_finalization_failure: bool,
    failed_requests: BTreeMap<Uuid, std::collections::BTreeSet<Uuid>>,
    principals: BTreeMap<Uuid, PrincipalGate>,
    not_ready_since: Option<Instant>,
    ready_alarm_sent: bool,
}
pub struct Runtime {
    limits: Limits,
    state: Mutex<State>,
    stop: watch::Sender<bool>,
    monitor_running: AtomicBool,
    reaper_running: AtomicBool,
    http: Arc<Semaphore>,
    inference: Arc<Semaphore>,
    pre_auth: Arc<Semaphore>,
    authentication: Arc<Semaphore>,
    nonstream: Arc<Semaphore>,
    management: Arc<Semaphore>,
    writes: Arc<Semaphore>,
    assets: Arc<Semaphore>,
    requests: Mutex<BTreeMap<Uuid, Arc<RequestControl>>>,
    request_changed: tokio::sync::Notify,
    resources: Arc<request::ResourceCounters>,
}
impl Runtime {
    pub fn new(limits: Limits) -> Result<Arc<Self>, crate::config::StartupError> {
        limits.validate()?;
        let (stop, _) = watch::channel(false);
        Ok(Arc::new(Self {
            state: Mutex::new(State {
                phase: Lifecycle::Starting,
                health: BTreeMap::new(),
                finalization_latch: false,
                unidentified_finalization_failure: false,
                failed_requests: BTreeMap::new(),
                principals: BTreeMap::new(),
                not_ready_since: None,
                ready_alarm_sent: false,
            }),
            stop,
            monitor_running: AtomicBool::new(false),
            reaper_running: AtomicBool::new(false),
            http: Arc::new(Semaphore::new(limits.http_requests)),
            inference: Arc::new(Semaphore::new(limits.inference)),
            pre_auth: Arc::new(Semaphore::new(limits.pre_auth)),
            authentication: Arc::new(Semaphore::new(limits.authentication)),
            nonstream: Arc::new(Semaphore::new(limits.nonstream)),
            management: Arc::new(Semaphore::new(limits.management)),
            writes: Arc::new(Semaphore::new(limits.management_writes)),
            assets: Arc::new(Semaphore::new(limits.assets)),
            requests: Mutex::new(BTreeMap::new()),
            request_changed: tokio::sync::Notify::new(),
            resources: Arc::new(request::ResourceCounters::default()),
            limits,
        }))
    }
    pub fn start_http(&self) -> Result<(), AdmissionError> {
        let mut state = self.state.lock().expect("런타임 상태 잠금");
        if state.phase != Lifecycle::Starting {
            return Err(AdmissionError::InvalidTransition);
        }
        state.phase = Lifecycle::NotReady;
        Ok(())
    }
    /// 의존성 작업의 관찰값만 기록한다. 이 함수는 인증/DB 검증이나 bootstrap 성공을 대신하지 않는다.
    pub fn record_health(&self, dependency: Dependency, healthy: bool, observed_at: Instant) {
        let mut state = self.state.lock().expect("런타임 상태 잠금");
        if observed_at > Instant::now()
            || state
                .health
                .get(&dependency)
                .is_some_and(|old| old.observed_at > observed_at)
        {
            return;
        }
        state.health.insert(
            dependency,
            Observation {
                healthy,
                observed_at,
            },
        );
    }
    pub fn lifecycle(&self) -> Lifecycle {
        let state = self.state.lock().expect("런타임 상태 잠금");
        match state.phase {
            Lifecycle::Ready | Lifecycle::NotReady => {
                if !state.finalization_latch
                    && [
                        Dependency::Database,
                        Dependency::Jwks,
                        Dependency::Schema,
                        Dependency::Ui,
                    ]
                    .into_iter()
                    .all(|d| self.healthy(&state, d, Instant::now()))
                {
                    Lifecycle::Ready
                } else {
                    Lifecycle::NotReady
                }
            }
            other => other,
        }
    }
    pub fn record_finalization_failure(&self) {
        let mut state = self.state.lock().expect("런타임 상태 잠금");
        if !state.finalization_latch {
            crate::telemetry::add(crate::telemetry::Metric::AlarmLatch, 1);
        }
        state.finalization_latch = true;
        state.unidentified_finalization_failure = true;
    }
    pub fn record_request_finalization_failure(&self, id: Uuid) {
        self.record_family_finalization_failure(id, None);
    }
    pub fn record_family_finalization_failure(&self, root: Uuid, child: Option<Uuid>) {
        let mut state = self.state.lock().expect("런타임 상태 잠금");
        if !state.finalization_latch {
            crate::telemetry::add(crate::telemetry::Metric::AlarmLatch, 1);
        }
        state.finalization_latch = true;
        if state.failed_requests.contains_key(&root)
            || state.failed_requests.len() < self.limits.inference
        {
            let family = state.failed_requests.entry(root).or_default();
            family.insert(root);
            if let Some(child) = child
                && family.len() < 2
            {
                family.insert(child);
            }
        } else {
            // 보관 상한을 넘는 예상 밖 실패는 ID를 버린 뒤 자동 복구했다고 주장하지 않는다.
            state.unidentified_finalization_failure = true;
        }
    }
    pub fn failed_request_ids(&self) -> Vec<Uuid> {
        self.state
            .lock()
            .expect("런타임 상태 잠금")
            .failed_requests
            .values()
            .flat_map(|ids| ids.iter().copied())
            .collect()
    }
    /// 장부가 가족 각 행의 마감/전역 차단을 확인한 ID만 제거한다.
    pub(crate) fn confirm_durable_finalizations(&self, ids: &[Uuid]) {
        let mut state = self.state.lock().expect("런타임 상태 잠금");
        state
            .failed_requests
            .retain(|_, family| !family.iter().all(|id| ids.contains(id)));
        state.finalization_latch =
            state.unidentified_finalization_failure || !state.failed_requests.is_empty();
        self.resources.notify();
    }
    pub(crate) fn reaper_guard(self: &Arc<Self>) -> Option<ReaperGuard> {
        if self.reaper_running.swap(true, Ordering::AcqRel) {
            None
        } else {
            Some(ReaperGuard(self.clone()))
        }
    }
    pub(crate) fn shutdown_signal(&self) -> watch::Receiver<bool> {
        self.stop.subscribe()
    }
    pub fn begin_draining(&self) {
        let mut state = self.state.lock().expect("런타임 상태 잠금");
        if matches!(
            state.phase,
            Lifecycle::Starting | Lifecycle::Ready | Lifecycle::NotReady
        ) {
            state.phase = Lifecycle::Draining;
            self.stop.send_replace(true);
        }
    }
    pub fn begin_stopping(&self) -> Result<(), AdmissionError> {
        let mut state = self.state.lock().expect("런타임 상태 잠금");
        if state.phase != Lifecycle::Draining {
            return Err(AdmissionError::InvalidTransition);
        }
        state.phase = Lifecycle::Stopping;
        Ok(())
    }
    pub fn stop(&self) {
        let mut state = self.state.lock().expect("런타임 상태 잠금");
        state.phase = Lifecycle::Stopped;
        self.stop.send_replace(true);
    }
    fn healthy(&self, state: &State, d: Dependency, now: Instant) -> bool {
        state.health.get(&d).is_some_and(|o| {
            o.healthy
                && now.saturating_duration_since(o.observed_at)
                    < Duration::from_millis(self.limits.health_valid_ms)
        })
    }
    pub async fn monitor<F, Fut>(self: Arc<Self>, mut check: F) -> Result<(), AdmissionError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = HealthReport>,
    {
        if self.monitor_running.swap(true, Ordering::AcqRel) {
            return Err(AdmissionError::InvalidTransition);
        }
        let _guard = MonitorGuard(self.clone());
        let mut stop = self.stop.subscribe();
        let mut tick = tokio::time::interval(Duration::from_millis(self.limits.health_interval_ms));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if *stop.borrow() {
                return Ok(());
            }
            tokio::select! {
                biased;
                _=stop.changed()=>return Ok(()),
                _=tick.tick()=>{},
            }
            let report = tokio::select! {
                biased;
                _=stop.changed()=>return Ok(()),
                report=tokio::time::timeout(Duration::from_secs(5),check())=>report.unwrap_or_default(),
            };
            let now = Instant::now();
            for (dependency, healthy) in [
                (Dependency::Database, report.database),
                (Dependency::Jwks, report.jwks),
                (Dependency::Schema, report.schema),
                (Dependency::Ui, report.ui),
            ] {
                self.record_health(dependency, healthy, now);
            }
            self.observe_telemetry(now);
            crate::telemetry::sample_memory();
            crate::telemetry::publish();
        }
    }
    pub fn observe_telemetry(&self, now: Instant) {
        use crate::telemetry::{self, Metric};
        let ready = self.lifecycle() == Lifecycle::Ready;
        telemetry::set(Metric::Ready, u64::from(ready));
        let mut state = self.state.lock().expect("런타임 상태 잠금");
        if ready {
            state.not_ready_since = None;
            state.ready_alarm_sent = false;
        } else if matches!(state.phase, Lifecycle::Ready | Lifecycle::NotReady) {
            let since = *state.not_ready_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= Duration::from_secs(30)
                && !state.ready_alarm_sent
            {
                telemetry::add(Metric::AlarmReady, 1);
                state.ready_alarm_sent = true;
            }
        }
        drop(state);
        telemetry::set(Metric::ActiveTasks, self.active_requests() as u64);
        telemetry::set(
            Metric::ActivePermits,
            (self.limits.http_requests - self.available_http()) as u64,
        );
        let resources = self.request_resources();
        telemetry::set(Metric::BodyBytes, resources.body_bytes as u64);
        telemetry::set(Metric::QueueBytes, resources.queue_bytes as u64);
        telemetry::set(Metric::QueueWaiters, resources.queue_waiters as u64);
        telemetry::max(Metric::QueuePeak, resources.queue_peak as u64);
        telemetry::max(Metric::ParserPeak, resources.parser_peak as u64);
    }
    pub fn limits(&self) -> &Limits {
        &self.limits
    }
    pub fn active_principals(&self) -> usize {
        self.state
            .lock()
            .expect("런타임 상태 잠금")
            .principals
            .len()
    }
    pub fn available_assets(&self) -> usize {
        self.assets.available_permits()
    }
    pub fn available_management(&self) -> usize {
        self.management.available_permits()
    }
    pub fn available_nonstream(&self) -> usize {
        self.nonstream.available_permits()
    }
    pub fn available_http(&self) -> usize {
        self.http.available_permits()
    }
    pub fn available_inference(&self) -> usize {
        self.inference.available_permits()
    }
    pub fn try_http(self: &Arc<Self>) -> Result<HttpAdmission, AdmissionError> {
        let permit = self.http.clone().try_acquire_owned().map_err(|_| {
            crate::telemetry::admission(AdmissionError::HttpCapacity);
            AdmissionError::HttpCapacity
        })?;
        Ok(HttpAdmission {
            entry: Arc::new(HttpEntry {
                _permit: permit,
                admitted: AtomicBool::new(false),
                started: Instant::now(),
            }),
            runtime: self.clone(),
        })
    }
    pub fn try_admit(self: &Arc<Self>, class: RouteClass) -> Result<RequestLease, AdmissionError> {
        self.try_admit_with_http(class, self.try_http()?)
    }
    pub fn try_admit_with_http(
        self: &Arc<Self>,
        class: RouteClass,
        http: HttpAdmission,
    ) -> Result<RequestLease, AdmissionError> {
        self.admit_with_http(class, http)
            .inspect_err(|error| crate::telemetry::admission(*error))
    }
    fn admit_with_http(
        self: &Arc<Self>,
        class: RouteClass,
        http: HttpAdmission,
    ) -> Result<RequestLease, AdmissionError> {
        if !Arc::ptr_eq(self, &http.runtime) || http.entry.admitted.swap(true, Ordering::AcqRel) {
            return Err(AdmissionError::InvalidTransition);
        }
        let state = self.state.lock().expect("런타임 상태 잠금");
        if matches!(
            state.phase,
            Lifecycle::Draining | Lifecycle::Stopping | Lifecycle::Stopped
        ) {
            return Err(AdmissionError::Draining);
        }
        if state.phase == Lifecycle::Starting {
            return Err(AdmissionError::NotReady);
        }
        let now = Instant::now();
        let healthy = |d| self.healthy(&state, d, now);
        let dependencies = match class {
            RouteClass::Inference => {
                !state.finalization_latch
                    && [
                        Dependency::Database,
                        Dependency::Jwks,
                        Dependency::Schema,
                        Dependency::Ui,
                    ]
                    .into_iter()
                    .all(healthy)
            }
            RouteClass::ManagementRead | RouteClass::ManagementWrite => {
                [Dependency::Database, Dependency::Jwks, Dependency::Schema]
                    .into_iter()
                    .all(healthy)
            }
            RouteClass::Asset => healthy(Dependency::Ui),
        };
        if !dependencies {
            return Err(AdmissionError::NotReady);
        }
        let mut permits = Vec::new();
        let mut pre_auth = None;
        match class {
            RouteClass::Inference => {
                permits.push(acquire(&self.inference)?);
                pre_auth = Some(
                    acquire(&self.pre_auth).map_err(|_| AdmissionError::AuthenticationCapacity)?,
                );
            }
            RouteClass::ManagementRead | RouteClass::ManagementWrite => {
                permits.push(acquire(&self.management)?);
                if class == RouteClass::ManagementWrite {
                    permits.push(acquire(&self.writes)?);
                }
                pre_auth = Some(
                    acquire(&self.pre_auth).map_err(|_| AdmissionError::AuthenticationCapacity)?,
                );
            }
            RouteClass::Asset => permits.push(acquire(&self.assets)?),
        }
        Ok(RequestLease {
            runtime: self.clone(),
            class,
            _http: http,
            permits,
            pre_auth,
            authentication: None,
            principal: None,
            nonstream: false,
        })
    }
}
pub(crate) struct ReaperGuard(Arc<Runtime>);
impl Drop for ReaperGuard {
    fn drop(&mut self) {
        self.0.reaper_running.store(false, Ordering::Release);
    }
}
fn acquire(semaphore: &Arc<Semaphore>) -> Result<OwnedSemaphorePermit, AdmissionError> {
    semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| AdmissionError::Concurrency)
}

pub struct RequestLease {
    runtime: Arc<Runtime>,
    class: RouteClass,
    _http: HttpAdmission,
    permits: Vec<OwnedSemaphorePermit>,
    pre_auth: Option<OwnedSemaphorePermit>,
    authentication: Option<OwnedSemaphorePermit>,
    principal: Option<Uuid>,
    nonstream: bool,
}
impl RequestLease {
    pub fn started(&self) -> Instant {
        self._http.started()
    }
    pub fn bind_response(self, response: axum::response::Response) -> axum::response::Response {
        retain_response(response, Arc::new(self))
    }
    pub fn begin_authentication(&mut self) -> Result<(), AdmissionError> {
        if self.pre_auth.is_none() || self.authentication.is_some() {
            return Err(AdmissionError::InvalidTransition);
        }
        self.authentication = Some(acquire(&self.runtime.authentication).map_err(|_| {
            crate::telemetry::admission(AdmissionError::AuthenticationCapacity);
            crate::telemetry::add(crate::telemetry::Metric::AdmissionAuthentication, 1);
            AdmissionError::AuthenticationCapacity
        })?);
        Ok(())
    }
    pub fn identify(&mut self, principal: Uuid) -> Result<(), AdmissionError> {
        if self.authentication.is_none() || self.principal.is_some() {
            return Err(AdmissionError::InvalidTransition);
        }
        if self.class == RouteClass::Inference {
            let mut state = self.runtime.state.lock().expect("런타임 상태 잠금");
            let gate = state
                .principals
                .entry(principal)
                .or_insert_with(|| PrincipalGate {
                    semaphore: Arc::new(Semaphore::new(self.runtime.limits.per_principal)),
                    leases: 0,
                });
            let permit = acquire(&gate.semaphore).inspect_err(|error| {
                crate::telemetry::admission(*error);
                crate::telemetry::add(crate::telemetry::Metric::AdmissionPrincipal, 1);
            })?;
            gate.leases += 1;
            self.permits.push(permit);
            self.principal = Some(principal);
        }
        self.authentication.take();
        self.pre_auth.take();
        Ok(())
    }
    pub fn acquire_nonstream(&mut self) -> Result<(), AdmissionError> {
        if self.class != RouteClass::Inference || self.principal.is_none() || self.nonstream {
            return Err(AdmissionError::InvalidTransition);
        }
        self.permits
            .push(acquire(&self.runtime.nonstream).inspect_err(|error| {
                crate::telemetry::admission(*error);
                crate::telemetry::add(crate::telemetry::Metric::AdmissionNonstream, 1);
            })?);
        self.nonstream = true;
        Ok(())
    }
}
impl Drop for RequestLease {
    fn drop(&mut self) {
        self.permits.clear();
        if let Some(id) = self.principal {
            let mut state = self.runtime.state.lock().expect("런타임 상태 잠금");
            if let Some(gate) = state.principals.get_mut(&id) {
                gate.leases -= 1;
                if gate.leases == 0 {
                    state.principals.remove(&id);
                }
            }
        }
        self.runtime.resources.notify();
    }
}

#[derive(Clone)]
pub struct HttpAdmission {
    entry: Arc<HttpEntry>,
    runtime: Arc<Runtime>,
}
struct HttpEntry {
    _permit: OwnedSemaphorePermit,
    admitted: AtomicBool,
    started: Instant,
}
impl HttpAdmission {
    pub fn started(&self) -> Instant {
        self.entry.started
    }
    pub fn bind_response(self, response: axum::response::Response) -> axum::response::Response {
        retain_response(response, Arc::new(self))
    }
}
pub(crate) fn retain_response<T: Send + Sync + 'static>(
    response: axum::response::Response,
    owner: Arc<T>,
) -> axum::response::Response {
    let (mut parts, inner) = response.into_parts();
    if let Some(held) = parts.extensions.get_mut::<TransportOwner>() {
        held._owners.push(owner.clone());
    } else {
        parts.extensions.insert(TransportOwner {
            _owners: vec![owner.clone()],
        });
    }
    axum::response::Response::from_parts(parts, axum::body::Body::new(HeldBody { inner, owner }))
}
#[derive(Clone)]
pub(crate) struct TransportOwner {
    _owners: Vec<Arc<dyn Send + Sync>>,
}
struct HeldBody<T> {
    inner: axum::body::Body,
    owner: Arc<T>,
}
struct HeldBytes<T> {
    bytes: axum::body::Bytes,
    _owner: Arc<T>,
}
impl<T> AsRef<[u8]> for HeldBytes<T> {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}
impl<T: Send + Sync + 'static> http_body::Body for HeldBody<T> {
    type Data = axum::body::Bytes;
    type Error = axum::Error;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let owner = self.owner.clone();
        std::pin::Pin::new(&mut self.inner)
            .poll_frame(cx)
            .map(|result| {
                result.map(|frame| {
                    frame.map(|f| {
                        f.map_data(|bytes| {
                            axum::body::Bytes::from_owner(HeldBytes {
                                bytes,
                                _owner: owner,
                            })
                        })
                    })
                })
            })
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

#[derive(Default)]
pub struct HealthReport {
    pub database: bool,
    pub jwks: bool,
    pub schema: bool,
    pub ui: bool,
}
struct MonitorGuard(Arc<Runtime>);
impl Drop for MonitorGuard {
    fn drop(&mut self) {
        self.0.monitor_running.store(false, Ordering::Release);
    }
}

/// 길이뿐 아니라 원본 backing allocation도 같은 byte 상한 안에 유지한다.
pub(crate) struct BoundedBuffer {
    bytes: Vec<u8>,
    limit: usize,
    input: bool,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct BufferLimit;
impl BoundedBuffer {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            input: false,
        }
    }
    pub(crate) fn input(limit: usize) -> Self {
        let mut buffer = Self::new(limit);
        buffer.input = true;
        buffer
    }
    pub(crate) fn extend(&mut self, chunk: &[u8]) -> Result<(), BufferLimit> {
        let needed = self
            .bytes
            .len()
            .checked_add(chunk.len())
            .ok_or(BufferLimit)?;
        if needed > self.limit {
            return Err(BufferLimit);
        }
        if needed > self.bytes.capacity() {
            let target = self
                .bytes
                .capacity()
                .saturating_mul(2)
                .max(needed)
                .min(self.limit);
            self.bytes.reserve_exact(target - self.bytes.len());
        }
        self.bytes.extend_from_slice(chunk);
        if self.input {
            crate::telemetry::add(crate::telemetry::Metric::InputBytes, chunk.len() as u64);
        }
        Ok(())
    }
    pub(crate) fn freeze(mut self) -> axum::body::Bytes {
        let input = self.input;
        self.input = false;
        axum::body::Bytes::from_owner(InputAllocation {
            bytes: std::mem::take(&mut self.bytes),
            input,
        })
    }
}
impl Drop for BoundedBuffer {
    fn drop(&mut self) {
        if self.input {
            crate::telemetry::subtract(
                crate::telemetry::Metric::InputBytes,
                self.bytes.len() as u64,
            );
        }
    }
}
struct InputAllocation {
    bytes: Vec<u8>,
    input: bool,
}
impl AsRef<[u8]> for InputAllocation {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}
impl Drop for InputAllocation {
    fn drop(&mut self) {
        if self.input {
            crate::telemetry::subtract(
                crate::telemetry::Metric::InputBytes,
                self.bytes.len() as u64,
            );
        }
    }
}
#[cfg(test)]
mod buffer_tests {
    use super::*;
    #[test]
    fn bounded_buffer_never_keeps_an_oversized_backing_allocation() {
        let mut buffer = BoundedBuffer::new(10_000);
        buffer.extend(&vec![0; 6000]).unwrap();
        buffer.extend(&vec![0; 4000]).unwrap();
        assert!(
            buffer.bytes.capacity() <= 10_000,
            "Vec growth retained allocation over the byte bound"
        );
        assert!(buffer.extend(&[0]).is_err());
        assert_eq!(buffer.freeze().len(), 10_000);
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    #[tokio::test]
    async fn reaper_is_singleton_and_partial_family_never_clears_latch() {
        let runtime = Runtime::new(Limits::default()).unwrap();
        let guard = runtime.reaper_guard().unwrap();
        assert!(runtime.reaper_guard().is_none());
        drop(guard);
        assert!(runtime.reaper_guard().is_some());
        runtime.start_http().unwrap();
        for d in [
            Dependency::Database,
            Dependency::Jwks,
            Dependency::Schema,
            Dependency::Ui,
        ] {
            runtime.record_health(d, true, Instant::now());
        }
        let root = Uuid::new_v4();
        let child = Uuid::new_v4();
        runtime.record_family_finalization_failure(root, Some(child));
        runtime.confirm_durable_finalizations(&[root]);
        assert_eq!(runtime.failed_request_ids().len(), 2);
        assert_eq!(runtime.lifecycle(), Lifecycle::NotReady);
        runtime.confirm_durable_finalizations(&[root, child]);
        assert_eq!(runtime.lifecycle(), Lifecycle::Ready);
        runtime.record_finalization_failure();
        runtime.confirm_durable_finalizations(&[]);
        assert_eq!(runtime.lifecycle(), Lifecycle::NotReady);
    }
}
