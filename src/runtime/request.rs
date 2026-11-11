//! body Drop은 동기 취소만 수행한다. 감독 task가 실제 송신 종료와 장부 마감을 소유한다.
use super::*;
use axum::{
    body::{Body, Bytes},
    response::Response,
};
use std::sync::atomic::{AtomicU8, AtomicUsize};
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{
    sync::{mpsc, oneshot},
    task::AbortHandle,
};

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestPhase {
    Admitting,
    Receiving,
    Preparing,
    Dispatching,
    Relaying,
    Finalizing,
    AwaitBodyRelease,
    Done,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestInterruption {
    ClientCancelled,
    ShutdownCancelled,
    TransportDeadline,
}
pub struct RequestControl {
    cancel: watch::Sender<Option<(Instant, RequestInterruption)>>,
    resources: Arc<ResourceCounters>,
    handle: Mutex<Option<AbortHandle>>,
    finalizing: AtomicBool,
    started: Instant,
    parser_bytes: AtomicUsize,
    phase: AtomicU8,
    finalization_budget: Duration,
    cleanup_deadline: Mutex<Option<Instant>>,
    pub transport_deadline: Instant,
    pub execution_deadline: Instant,
}
impl RequestControl {
    pub fn cancel(&self) {
        self.cancel_with(RequestInterruption::ClientCancelled);
    }
    fn cancel_with(&self, cause: RequestInterruption) {
        self.cancel.send_if_modified(|first| {
            if first.is_none() {
                *first = Some((Instant::now(), cause));
                if !self.finalizing.load(Ordering::Acquire) {
                    crate::telemetry::add(crate::telemetry::Metric::Cancelled, 1);
                }
                true
            } else {
                false
            }
        });
    }
    pub fn phase(&self) -> RequestPhase {
        match self.phase.load(Ordering::Acquire) {
            0 => RequestPhase::Admitting,
            1 => RequestPhase::Receiving,
            2 => RequestPhase::Preparing,
            3 => RequestPhase::Dispatching,
            4 => RequestPhase::Relaying,
            5 => RequestPhase::Finalizing,
            6 => RequestPhase::AwaitBodyRelease,
            _ => RequestPhase::Done,
        }
    }
    pub fn set_phase(&self, phase: RequestPhase) {
        if self.phase.swap(phase as u8, Ordering::AcqRel) != phase as u8 {
            self.resources.notify();
        }
    }
    pub fn begin_finalizing(&self) -> Instant {
        self.set_phase(RequestPhase::Finalizing);
        self.finalizing.store(true, Ordering::Release);
        let mut at = self.cleanup_deadline.lock().expect("마감 기한 잠금");
        *at.get_or_insert_with(|| {
            self.execution_deadline
                .min(Instant::now() + self.finalization_budget)
        })
    }
    pub async fn interrupted(&self) -> RequestInterruption {
        tokio::select! {biased;_=self.cancelled()=>{},_=tokio::time::sleep_until(self.transport_deadline)=>{}}
        if self.finalizing.load(Ordering::Acquire) {
            std::future::pending::<()>().await;
        }
        // transport 종료 뒤 ResponseWait/body Drop이 늦게 cancel해도 최초 기한 원인을 덮지 않는다.
        // 기한 전 취소는 이 future의 처리가 기한 뒤로 늦어져도 그대로 취소다. 정각은 기한 만료다.
        self.cancel
            .borrow()
            .filter(|(at, _)| *at < self.transport_deadline)
            .map_or(RequestInterruption::TransportDeadline, |(_, cause)| cause)
    }
    pub async fn cancelled(&self) {
        let mut rx = self.cancel.subscribe();
        if rx.borrow().is_some() {
            return;
        }
        let _ = rx.changed().await;
    }
}
impl Drop for RequestControl {
    fn drop(&mut self) {
        crate::telemetry::subtract(
            crate::telemetry::Metric::ParserBytes,
            self.parser_bytes.load(Ordering::Acquire) as u64,
        );
    }
}
pub(super) struct ResourceCounters {
    queue: AtomicUsize,
    queue_waiters: AtomicUsize,
    queue_peak: AtomicUsize,
    body: AtomicUsize,
    body_peak: AtomicUsize,
    parser_peak: AtomicUsize,
    upstream_chunk_peak: AtomicUsize,
    finished: AtomicUsize,
    changed: watch::Sender<u64>,
}
impl Default for ResourceCounters {
    fn default() -> Self {
        Self {
            queue: AtomicUsize::new(0),
            queue_waiters: AtomicUsize::new(0),
            queue_peak: AtomicUsize::new(0),
            body: AtomicUsize::new(0),
            body_peak: AtomicUsize::new(0),
            parser_peak: AtomicUsize::new(0),
            upstream_chunk_peak: AtomicUsize::new(0),
            finished: AtomicUsize::new(0),
            changed: watch::channel(0).0,
        }
    }
}
impl ResourceCounters {
    pub(super) fn notify(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }
}
#[derive(Debug, Clone, Copy)]
pub struct RequestResources {
    pub queue_bytes: usize,
    pub queue_waiters: usize,
    pub queue_peak: usize,
    pub body_bytes: usize,
    pub body_peak: usize,
    pub parser_peak: usize,
    pub upstream_chunk_peak: usize,
    pub finished: usize,
}
impl Runtime {
    pub fn resource_changes(&self) -> watch::Receiver<u64> {
        self.resources.changed.subscribe()
    }
    pub fn request_resources(&self) -> RequestResources {
        let r = &self.resources;
        RequestResources {
            queue_bytes: r.queue.load(Ordering::Acquire),
            queue_waiters: r.queue_waiters.load(Ordering::Acquire),
            queue_peak: r.queue_peak.load(Ordering::Acquire),
            body_bytes: r.body.load(Ordering::Acquire),
            body_peak: r.body_peak.load(Ordering::Acquire),
            parser_peak: r.parser_peak.load(Ordering::Acquire),
            upstream_chunk_peak: r.upstream_chunk_peak.load(Ordering::Acquire),
            finished: r.finished.load(Ordering::Acquire),
        }
    }
}
impl RequestControl {
    pub fn observe_parser(&self, bytes: usize, chunk: usize) {
        let old = self.parser_bytes.swap(bytes, Ordering::AcqRel);
        crate::telemetry::subtract(crate::telemetry::Metric::ParserBytes, old as u64);
        crate::telemetry::add(crate::telemetry::Metric::ParserBytes, bytes as u64);
        self.resources
            .parser_peak
            .fetch_max(bytes, Ordering::AcqRel);
        self.resources
            .upstream_chunk_peak
            .fetch_max(chunk, Ordering::AcqRel);
    }
    pub fn retain_body(&self, bytes: Bytes) -> Bytes {
        let n = bytes.len();
        let now = self.resources.body.fetch_add(n, Ordering::AcqRel) + n;
        self.resources.body_peak.fetch_max(now, Ordering::AcqRel);
        self.resources.notify();
        Bytes::from_owner(MeteredBody {
            bytes,
            resources: self.resources.clone(),
        })
    }
}
struct MeteredBody {
    bytes: Bytes,
    resources: Arc<ResourceCounters>,
}
impl AsRef<[u8]> for MeteredBody {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}
impl Drop for MeteredBody {
    fn drop(&mut self) {
        self.resources
            .body
            .fetch_sub(self.bytes.len(), Ordering::AcqRel);
        self.resources.notify();
    }
}
pub struct RequestRegistration {
    runtime: Arc<Runtime>,
    id: Uuid,
}
impl Drop for RequestRegistration {
    fn drop(&mut self) {
        self.runtime
            .requests
            .lock()
            .expect("요청 감독 잠금")
            .remove(&self.id);
        self.runtime
            .resources
            .finished
            .fetch_add(1, Ordering::AcqRel);
        self.runtime.request_changed.notify_waiters();
    }
}
impl Runtime {
    pub fn register_request(
        self: &Arc<Self>,
        id: Uuid,
        started: Instant,
    ) -> (Arc<RequestControl>, RequestRegistration) {
        let (cancel, _) = watch::channel(None);
        let control = Arc::new(RequestControl {
            cancel,
            resources: self.resources.clone(),
            finalizing: AtomicBool::new(false),
            started,
            parser_bytes: AtomicUsize::new(0),
            phase: AtomicU8::new(RequestPhase::Admitting as u8),
            finalization_budget: Duration::from_millis(self.limits.finalization_timeout_ms),
            cleanup_deadline: Mutex::new(None),
            handle: Mutex::new(None),
            transport_deadline: started + Duration::from_millis(self.limits.transport_timeout_ms),
            execution_deadline: started + Duration::from_millis(self.limits.request_timeout_ms),
        });
        self.requests
            .lock()
            .expect("요청 감독 잠금")
            .insert(id, control.clone());
        (
            control,
            RequestRegistration {
                runtime: self.clone(),
                id,
            },
        )
    }
    pub fn attach_request(&self, id: Uuid, handle: AbortHandle) {
        if let Some(c) = self.requests.lock().expect("요청 감독 잠금").get(&id) {
            *c.handle.lock().expect("요청 handle 잠금") = Some(handle);
        }
    }
    pub async fn wait_requests(&self) {
        loop {
            let changed = self.request_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.active_requests() == 0 {
                return;
            }
            changed.await;
        }
    }
    pub fn request_phases(&self) -> Vec<(Uuid, RequestPhase)> {
        self.requests
            .lock()
            .expect("요청 감독 잠금")
            .iter()
            .map(|(id, c)| (*id, c.phase()))
            .collect()
    }
    pub fn active_requests(&self) -> usize {
        self.requests.lock().expect("요청 감독 잠금").len()
    }
    pub fn cancel_requests(&self) {
        for c in self.requests.lock().expect("요청 감독 잠금").values() {
            c.cancel_with(RequestInterruption::ShutdownCancelled);
        }
    }
}
/// worker와 body/frame 모두 참조를 해제한 뒤 permit과 감독 entry를 함께 반환한다.
pub struct RequestOwner {
    pub lease: tokio::sync::Mutex<RequestLease>,
    control: Arc<RequestControl>,
    _registration: RequestRegistration,
}
impl RequestOwner {
    pub fn new(
        lease: RequestLease,
        control: Arc<RequestControl>,
        registration: RequestRegistration,
    ) -> Arc<Self> {
        Arc::new(Self {
            lease: tokio::sync::Mutex::new(lease),
            control,
            _registration: registration,
        })
    }
    pub fn bind(self: &Arc<Self>, response: Response) -> Response {
        let response = super::retain_response(response, self.clone());
        let (parts, inner) = response.into_parts();
        Response::from_parts(
            parts,
            Body::new(CancelBody {
                inner,
                control: self.control.clone(),
                ended: false,
            }),
        )
    }
}
impl Drop for RequestOwner {
    fn drop(&mut self) {
        self.control.set_phase(RequestPhase::Done);
    }
}
struct CancelBody {
    inner: Body,
    control: Arc<RequestControl>,
    ended: bool,
}
impl Drop for CancelBody {
    fn drop(&mut self) {
        if !self.ended {
            self.control.cancel();
        }
    }
}
impl http_body::Body for CancelBody {
    type Data = Bytes;
    type Error = axum::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, axum::Error>>> {
        let result = Pin::new(&mut self.inner).poll_frame(cx);
        if matches!(result, Poll::Ready(None)) {
            self.ended = true;
        }
        result
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}
pub struct ResponseWait {
    pub receiver: oneshot::Receiver<Response>,
    control: Arc<RequestControl>,
    armed: bool,
}
impl ResponseWait {
    pub fn new(receiver: oneshot::Receiver<Response>, control: Arc<RequestControl>) -> Self {
        Self {
            receiver,
            control,
            armed: true,
        }
    }
    pub fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for ResponseWait {
    fn drop(&mut self) {
        if self.armed {
            self.control.cancel();
        }
    }
}

#[derive(Debug)]
pub struct QueueSendError;
pub struct ByteQueue {
    first_sent: AtomicBool,
    started: Instant,
    sender: mpsc::Sender<Bytes>,
    bytes: Arc<Semaphore>,
    control: Arc<RequestControl>,
    success: Arc<AtomicBool>,
}
struct QueueBytes {
    bytes: Bytes,
    _permit: OwnedSemaphorePermit,
    resources: Arc<ResourceCounters>,
}
impl Drop for QueueBytes {
    fn drop(&mut self) {
        self.resources
            .queue
            .fetch_sub(self.bytes.len(), Ordering::AcqRel);
        self.resources.notify();
    }
}
impl AsRef<[u8]> for QueueBytes {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}
pub fn byte_queue(control: Arc<RequestControl>) -> (ByteQueue, Body) {
    let (sender, receiver) = mpsc::channel(4);
    let success = Arc::new(AtomicBool::new(false));
    crate::telemetry::add(crate::telemetry::Metric::Streams, 1);
    let queue = ByteQueue {
        first_sent: AtomicBool::new(false),
        started: Instant::now(),
        sender,
        bytes: Arc::new(Semaphore::new(crate::stream::QUEUE_BYTES)),
        control,
        success: success.clone(),
    };
    (
        queue,
        Body::new(QueueBody {
            receiver,
            success,
            ended: false,
        }),
    )
}
struct QueueBody {
    receiver: mpsc::Receiver<Bytes>,
    success: Arc<AtomicBool>,
    ended: bool,
}
impl http_body::Body for QueueBody {
    type Data = Bytes;
    type Error = std::io::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, std::io::Error>>> {
        if self.ended {
            return Poll::Ready(None);
        }
        match self.receiver.poll_recv(cx) {
            Poll::Ready(Some(bytes)) => Poll::Ready(Some(Ok(http_body::Frame::data(bytes)))),
            Poll::Ready(None) => {
                self.ended = true;
                if self.success.load(Ordering::Acquire) {
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(Err(std::io::Error::other(
                        "Gateway stream interrupted",
                    ))))
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for ByteQueue {
    fn drop(&mut self) {
        crate::telemetry::add(
            crate::telemetry::Metric::StreamMicros,
            self.started.elapsed().as_micros().min(u64::MAX as u128) as u64,
        );
    }
}
impl ByteQueue {
    pub fn finish(self) {
        self.success.store(true, Ordering::Release);
    }
    pub async fn send(&self, bytes: &[u8]) -> Result<(), QueueSendError> {
        for part in bytes.chunks(crate::stream::CHUNK_BYTES) {
            let _blocked =
                if self.bytes.available_permits() < part.len() || self.sender.capacity() == 0 {
                    self.control
                        .resources
                        .queue_waiters
                        .fetch_add(1, Ordering::AcqRel);
                    self.control.resources.notify();
                    Some(QueueWait(self.control.resources.clone()))
                } else {
                    None
                };
            tokio::select! {biased;_=self.control.cancelled()=>return Err(QueueSendError),_=tokio::time::sleep_until(self.control.transport_deadline)=>return Err(QueueSendError),result=async{
                let permit=self.bytes.clone().acquire_many_owned(part.len()as u32).await.map_err(|_|QueueSendError)?;
                let resources=self.control.resources.clone();let n=resources.queue.fetch_add(part.len(),Ordering::AcqRel)+part.len();resources.queue_peak.fetch_max(n,Ordering::AcqRel);resources.notify();let bytes=Bytes::from_owner(QueueBytes{bytes:Bytes::copy_from_slice(part),_permit:permit,resources});self.sender.send(bytes).await.map_err(|_|QueueSendError)
            }=>result?}

            if !self.first_sent.swap(true, Ordering::AcqRel) {
                crate::telemetry::add(crate::telemetry::Metric::Ttft, 1);
                crate::telemetry::add(
                    crate::telemetry::Metric::TtftMicros,
                    self.control
                        .started
                        .elapsed()
                        .as_micros()
                        .min(u64::MAX as u128) as u64,
                );
            }
        }
        Ok(())
    }
    pub fn available_bytes(&self) -> usize {
        self.bytes.available_permits()
    }
}

struct QueueWait(Arc<ResourceCounters>);
impl Drop for QueueWait {
    fn drop(&mut self) {
        self.0.queue_waiters.fetch_sub(1, Ordering::AcqRel);
        self.0.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn request_interruption_preserves_first_cancel_time_across_transport_deadline() {
        let runtime = Runtime::new(crate::config::Limits {
            transport_timeout_ms: 1500,
            request_timeout_ms: 6500,
            body_timeout_ms: 1500,
            body_idle_timeout_ms: 1500,
            ..Default::default()
        })
        .unwrap();
        // 가상 단조 시각의 순서를 검증한다. 실제 socket/DB의 정각 정밀도 주장은 아니다.
        for (cancel_ms, expected) in [
            (Some(1499), RequestInterruption::ClientCancelled),
            (Some(1500), RequestInterruption::TransportDeadline),
            (Some(1501), RequestInterruption::TransportDeadline),
            (None, RequestInterruption::TransportDeadline),
        ] {
            let (control, registration) = runtime.register_request(Uuid::new_v4(), Instant::now());
            if let Some(ms) = cancel_ms {
                tokio::time::advance(Duration::from_millis(ms)).await;
                control.cancel();
                tokio::time::advance(Duration::from_millis(2000 - ms)).await;
                control.cancel(); // 늦은 두 번째 Drop이 최초 취소를 덮지 않는다.
            } else {
                tokio::time::advance(Duration::from_millis(2000)).await;
            }
            assert_eq!(control.interrupted().await, expected);
            assert_eq!(
                control.interrupted().await,
                expected,
                "이미 관측한 원인은 불변"
            );
            drop(registration);
        }
        assert_eq!(runtime.active_requests(), 0);
    }
    #[tokio::test(start_paused = true)]
    async fn shutdown_and_body_drop_keep_first_typed_cancel_cause() {
        for shutdown_first in [false, true] {
            let runtime = Runtime::new(Limits::default()).unwrap();
            let (control, registration) = runtime.register_request(Uuid::new_v4(), Instant::now());
            if !shutdown_first {
                control.cancel();
            }
            runtime.cancel_requests();
            control.cancel();
            assert_eq!(
                control.interrupted().await,
                if shutdown_first {
                    RequestInterruption::ShutdownCancelled
                } else {
                    RequestInterruption::ClientCancelled
                }
            );
            drop(registration);
            assert_eq!(runtime.active_requests(), 0);
        }
    }
}

#[cfg(test)]
mod ttft_tests {
    use super::*;
    #[tokio::test]
    async fn ttft_counts_first_successful_enqueue_not_zero_byte_failures() {
        use crate::telemetry::{self, Metric};
        let runtime = Runtime::new(Limits::default()).unwrap();
        for failure in ["pre_cancel", "receiver_drop", "success"] {
            let (control, registration) = runtime.register_request(Uuid::new_v4(), Instant::now());
            let (queue, body) = byte_queue(control.clone());
            let mut body = Some(body);
            if failure == "pre_cancel" {
                control.cancel();
            }
            if failure == "receiver_drop" {
                drop(body.take());
            }
            let before = telemetry::get(Metric::Ttft);
            let result = queue.send(b"first").await;
            assert_eq!(result.is_ok(), failure == "success");
            assert_eq!(
                telemetry::get(Metric::Ttft),
                before + u64::from(failure == "success"),
                "{failure}: 전달0byte는 TTFT표본이 아니다"
            );
            if failure == "success" {
                queue.send(b"second").await.unwrap();
                assert_eq!(telemetry::get(Metric::Ttft), before + 1);
            }
            drop(queue);
            drop(body);
            drop(registration);
        }
        assert_eq!(runtime.active_requests(), 0);
        assert_eq!(runtime.request_resources().queue_bytes, 0);
    }
}
