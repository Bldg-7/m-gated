use crate::{config::Limits, runtime::Runtime};
use axum::Router;
use hyper_util::rt::{TokioIo, TokioTimer};
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
    task::JoinSet,
    time::{Instant, sleep_until, timeout},
};
use tower::ServiceExt;

/// listener는 bootstrap 검증 뒤에만 생성한다. HTTP/1은 raw framing 검증을 위해 한 연결 한 요청이다.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    runtime: Arc<Runtime>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let connections = Arc::new(Semaphore::new(runtime.limits().connections));
    let mut tasks = JoinSet::new();
    let (stop, _) = watch::channel(false);
    tokio::pin!(shutdown);
    let result = loop {
        tokio::select! {
            biased;
            _=&mut shutdown=>break Ok(()),
            Some(_)=tasks.join_next(),if !tasks.is_empty()=>{},
            accepted=listener.accept()=>{
                let (stream,_)=match accepted {Ok(v)=>v,Err(error)=>break Err(error)};
                let Ok(permit)=connections.clone().try_acquire_owned() else {drop(stream);continue;};
                let router=router.clone();let limits=runtime.limits().clone();let mut stopped=stop.subscribe();
                tasks.spawn(async move {
                    let _permit=permit;
                    let prepared=tokio::select! {
                        biased;
                        _=stopped.changed()=>return,
                        prepared=prepare(stream,&limits)=>prepared,
                    };
                    let Some(io)=prepared else {return;};
                    let header_deadline=io.http2_headers.as_ref().map(|headers|headers.deadline.subscribe());
                    if io.http2_headers.is_some() {
                        tokio::select!{biased;_=http2_header_expired(header_deadline)=>{},_=serve_h2(io,router,limits.clone(),stopped)=>{}}
                    } else {
                        let (deadline,rx)=watch::channel(None);
                        let head_signal=io.head_signal.clone();
                        let owners=Arc::new(std::sync::Mutex::new(Vec::new()));
                        let held=owners.clone();let service_limits=limits.clone();
                        let service=hyper::service::service_fn(move |request:hyper::Request<hyper::body::Incoming>|{
                            deadline.send_replace(Some(response_deadline(request.uri().path(),&service_limits)));
                            let router=router.clone();let held=held.clone();let signal=Arc::new(HeadSignal::new());*head_signal.lock().expect("헤더 전송 신호 잠금")=Some(signal.clone());
                            async move {
                                let mut response=router.oneshot(request.map(axum::body::Body::new)).await?;
                                if let Some(owner)=response.extensions_mut().remove::<crate::runtime::TransportOwner>(){held.lock().expect("전송 owner 잠금").push(owner);}
                                let(parts,body)=response.into_parts();let response=axum::response::Response::from_parts(parts,axum::body::Body::new(HeadGatedBody::new(body,signal)));
                                Ok::<_,std::convert::Infallible>(response)
                            }
                        });
                        let mut builder=hyper::server::conn::http1::Builder::new();
                        builder.keep_alive(false).max_headers(limits.header_count).max_buf_size(limits.header_bytes).timer(TokioTimer::new()).header_read_timeout(Duration::from_millis(limits.header_timeout_ms));
                        let connection=builder.serve_connection(TokioIo::new(io),service);
                        tokio::pin!(connection);
                        tokio::select!{biased;_=http2_header_expired(Some(rx.clone()))=>{},_=&mut connection=>{},_=stopped.changed()=>{connection.as_mut().graceful_shutdown();tokio::select!{_=http2_header_expired(Some(rx.clone()))=>{},_=timeout(Duration::from_secs(25),&mut connection)=>{}}}}
                        drop(owners);
                    }
                });
            },
        }
    };
    runtime.begin_draining();
    stop.send_replace(true);
    drop(listener);
    let shutdown_deadline = Instant::now() + Duration::from_secs(30);
    crate::telemetry::set(crate::telemetry::Metric::ShutdownRemaining, 30_000);
    if timeout(Duration::from_secs(25), async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        runtime.cancel_requests();
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    runtime.cancel_requests();
    let _ = tokio::time::timeout_at(shutdown_deadline, runtime.wait_requests()).await;
    let _ = runtime.begin_stopping();
    crate::telemetry::set(
        crate::telemetry::Metric::ShutdownRemaining,
        shutdown_deadline
            .saturating_duration_since(Instant::now())
            .as_millis() as u64,
    );
    runtime.observe_telemetry(Instant::now());
    crate::telemetry::sample_memory();
    crate::telemetry::publish();
    runtime.stop();
    result
}

struct HeadSignal {
    sent: std::sync::atomic::AtomicBool,
    ready: Arc<tokio::sync::Notify>,
}
impl HeadSignal {
    fn new() -> Self {
        Self {
            sent: std::sync::atomic::AtomicBool::new(false),
            ready: Arc::new(tokio::sync::Notify::new()),
        }
    }
}
struct HeadGatedBody {
    body: axum::body::Body,
    signal: Arc<HeadSignal>,
    ready: Pin<Box<tokio::sync::futures::OwnedNotified>>,
}
impl HeadGatedBody {
    fn new(body: axum::body::Body, signal: Arc<HeadSignal>) -> Self {
        let ready = Box::pin(signal.ready.clone().notified_owned());
        Self {
            body,
            signal,
            ready,
        }
    }
}
impl http_body::Body for HeadGatedBody {
    type Data = axum::body::Bytes;
    type Error = axum::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        if !self.signal.sent.load(std::sync::atomic::Ordering::Acquire) {
            let _ = self.ready.as_mut().poll(cx);
            if !self.signal.sent.load(std::sync::atomic::Ordering::Acquire) {
                return Poll::Pending;
            }
        }
        Pin::new(&mut self.body).poll_frame(cx)
    }
    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
}

fn response_deadline(path: &str, limits: &Limits) -> Instant {
    let ms = if path.starts_with("/ui") {
        limits.asset_timeout_ms
    } else if path.starts_with("/anthropic/")
        || path == "/v1/messages"
        || path == "/v1/messages/count_tokens"
        || path.starts_with("/openai/")
        || path.starts_with("/deepseek/")
    {
        limits.transport_timeout_ms
    } else {
        limits.management_timeout_ms
    };
    Instant::now() + Duration::from_millis(ms)
}
async fn serve_h2(
    io: PrefixedIo,
    router: Router,
    limits: Limits,
    mut stopped: watch::Receiver<bool>,
) {
    let flushed = io.flushed.subscribe();
    let mut builder = h2::server::Builder::new();
    builder
        .max_concurrent_streams(limits.http2_streams)
        .max_header_list_size(limits.header_bytes as u32)
        .max_send_buffer_size(64 * 1024)
        .max_pending_accept_reset_streams(16)
        .max_concurrent_reset_streams(16)
        .initial_window_size(64 * 1024)
        .initial_connection_window_size(1024 * 1024);
    let Ok(mut connection) = builder.handshake::<_, axum::body::Bytes>(io).await else {
        return;
    };
    let mut tasks = JoinSet::new();
    let mut drain_deadline = None;
    loop {
        tokio::select! {biased;
            _=stopped.changed(),if drain_deadline.is_none()=>{connection.graceful_shutdown();drain_deadline=Some(Instant::now()+Duration::from_secs(25));},
            _=sleep_until(drain_deadline.unwrap_or_else(||Instant::now()+Duration::from_secs(25))),if drain_deadline.is_some()=>break,
            Some(_)=tasks.join_next(),if !tasks.is_empty()=>{},
            accepted=connection.accept()=>{
                let Some(Ok((request,mut respond)))=accepted else{break;};
                if drain_deadline.is_some()||tasks.len()>=limits.http2_streams as usize {respond.send_reset(h2::Reason::REFUSED_STREAM);continue;}
                let deadline=response_deadline(request.uri().path(),&limits);
                if request.headers().len()>limits.header_count || request.headers().get_all(hyper::header::CONTENT_LENGTH).iter().count()>1 {respond.send_reset(h2::Reason::PROTOCOL_ERROR);continue;}
                let length=match request.headers().get(hyper::header::CONTENT_LENGTH){Some(value)=>match std::str::from_utf8(value.as_bytes()).ok().filter(|s|!s.is_empty()&&s.bytes().all(|b|b.is_ascii_digit())).and_then(|s|s.parse::<u64>().ok()){Some(n)=>Some(n),None=>{respond.send_reset(h2::Reason::PROTOCOL_ERROR);continue;}},None=>None};
                if request.headers().contains_key(hyper::header::TRANSFER_ENCODING)||request.headers().get_all(hyper::header::TE).iter().any(|v|v.as_bytes()!=b"trailers"){respond.send_reset(h2::Reason::PROTOCOL_ERROR);continue;}
                let router=router.clone();let flushed=flushed.clone();
                tasks.spawn(async move {
                    let work=router.oneshot(request.map(|inner|axum::body::Body::new(H2Receive{inner,remaining:length})));
                    let response=tokio::select!{biased;_=tokio::time::sleep_until(deadline)=>{respond.send_reset(h2::Reason::CANCEL);return;},_=std::future::poll_fn(|cx|respond.poll_reset(cx))=>return,response=work=>match response{Ok(r)=>r,Err(never)=>match never{}}};
                    send_h2_response(response,respond,flushed,deadline).await;
                });
            }
        }
    }
    // connection 종료는 body/receiver를 해제한다. 추론 장부 마감은 별도 RequestTask가 소유한다.
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}
struct H2Receive {
    inner: h2::RecvStream,
    remaining: Option<u64>,
}
struct H2Received {
    bytes: axum::body::Bytes,
    flow: h2::FlowControl,
}
impl AsRef<[u8]> for H2Received {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}
impl Drop for H2Received {
    fn drop(&mut self) {
        let _ = self.flow.release_capacity(self.bytes.len());
    }
}
impl http_body::Body for H2Receive {
    type Data = axum::body::Bytes;
    type Error = h2::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        match self.inner.poll_data(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                if let Some(remaining) = &mut self.remaining {
                    if bytes.len() as u64 > *remaining {
                        return Poll::Ready(Some(Err(h2::Reason::PROTOCOL_ERROR.into())));
                    }
                    *remaining -= bytes.len() as u64;
                }
                let bytes = axum::body::Bytes::from_owner(H2Received {
                    bytes,
                    flow: self.inner.flow_control().clone(),
                });
                Poll::Ready(Some(Ok(http_body::Frame::data(bytes))))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) if self.remaining.is_some_and(|n| n != 0) => {
                self.remaining = None;
                Poll::Ready(Some(Err(h2::Reason::PROTOCOL_ERROR.into())))
            }
            Poll::Ready(None) => self.inner.poll_trailers(cx).map(|r| match r {
                Ok(Some(t)) => Some(Ok(http_body::Frame::trailers(t))),
                Ok(None) => None,
                Err(e) => Some(Err(e)),
            }),
            Poll::Pending => Poll::Pending,
        }
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream() && self.remaining.is_none_or(|n| n == 0)
    }
}
async fn send_h2_response(
    response: axum::response::Response,
    mut respond: h2::server::SendResponse<axum::body::Bytes>,
    mut flushed: watch::Receiver<u64>,
    deadline: Instant,
) {
    use http_body::Body as _;
    let (parts, mut body) = response.into_parts();
    let _owner = parts
        .extensions
        .get::<crate::runtime::TransportOwner>()
        .cloned();
    let head_flush = *flushed.borrow_and_update();
    let Ok(mut send) = respond.send_response(hyper::Response::from_parts(parts, ()), false) else {
        return;
    };
    let write = async {
        while *flushed.borrow_and_update() == head_flush {
            tokio::select! {biased;_=std::future::poll_fn(|cx|send.poll_reset(cx))=>return Err(()),r=flushed.changed()=>r.map_err(|_|())?};
        }
        loop {
            let frame = tokio::select! {biased;_=std::future::poll_fn(|cx|send.poll_reset(cx))=>return Err(()),frame=std::future::poll_fn(|cx|Pin::new(&mut body).poll_frame(cx))=>frame};
            match frame {
                Some(Ok(frame)) => {
                    if let Ok(bytes) = frame.into_data() {
                        // h2에는 bounded copy만 전달한다. 작은 slice로 거대한 backing을 잡지 않는다.
                        for chunk in bytes.chunks(crate::stream::CHUNK_BYTES) {
                            let mut offset = 0;
                            while offset < chunk.len() {
                                send.reserve_capacity(chunk.len() - offset);
                                let capacity = std::future::poll_fn(|cx| send.poll_capacity(cx))
                                    .await
                                    .ok_or(())?
                                    .map_err(|_| ())?;
                                let n = capacity.min(chunk.len() - offset);
                                let before = *flushed.borrow_and_update();
                                send.send_data(
                                    axum::body::Bytes::copy_from_slice(&chunk[offset..offset + n]),
                                    false,
                                )
                                .map_err(|_| ())?;
                                // 원본 frame/queue owner는 codec의 실제 flush까지 보유한다.
                                while *flushed.borrow_and_update() == before {
                                    tokio::select! {biased;_=std::future::poll_fn(|cx|send.poll_reset(cx))=>return Err(()),r=flushed.changed()=>r.map_err(|_|())?};
                                }
                                offset += n;
                            }
                        }
                    }
                }
                Some(Err(_)) => return Err(()),
                None => {
                    send.send_data(axum::body::Bytes::new(), true)
                        .map_err(|_| ())?;
                    return Ok(());
                }
            }
        }
    };
    if timeout(deadline.saturating_duration_since(Instant::now()), write)
        .await
        .map_or(true, |r| r.is_err())
    {
        send.send_reset(h2::Reason::CANCEL);
    }
}

const HTTP2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

async fn prepare(mut stream: TcpStream, limits: &Limits) -> Option<PrefixedIo> {
    let header_timeout = Duration::from_millis(limits.header_timeout_ms);
    let started_at = Instant::now();
    let result = timeout(header_timeout, read_head(&mut stream, limits)).await;
    match result {
        Ok(Ok(prefix)) => {
            let http2_headers = prefix
                .starts_with(HTTP2_PREFACE)
                .then(|| Http2HeaderDeadline::new(started_at, header_timeout));
            Some(PrefixedIo {
                stream,
                prefix,
                position: 0,
                http2_headers,
                flushed: watch::channel(0_u64).0,
                head_signal: Arc::new(std::sync::Mutex::new(None)),
                head_tail: Vec::new(),
                head_written: false,
            })
        }
        other => {
            let status = match other {
                Err(_) => "408 Request Timeout",
                Ok(Err(HeadError::TooLarge)) => "431 Request Header Fields Too Large",
                _ => "400 Bad Request",
            };
            let response =
                format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
            let _ = timeout(
                Duration::from_millis(limits.header_timeout_ms),
                stream.write_all(response.as_bytes()),
            )
            .await;
            None
        }
    }
}
enum HeadError {
    Invalid,
    TooLarge,
}
async fn read_head(stream: &mut TcpStream, limits: &Limits) -> Result<Vec<u8>, HeadError> {
    let mut bytes = Vec::with_capacity(limits.header_bytes);
    let mut buffer = [0_u8; 8192];
    loop {
        let old_len = bytes.len();
        let available = (limits.header_bytes - old_len).min(buffer.len());
        if available == 0 {
            return Err(HeadError::TooLarge);
        }
        let received = stream
            .read(&mut buffer[..available])
            .await
            .map_err(|_| HeadError::Invalid)?;
        if received == 0 {
            return Err(HeadError::Invalid);
        }
        bytes.extend_from_slice(&buffer[..received]);
        if bytes.len() >= HTTP2_PREFACE.len() && bytes.starts_with(HTTP2_PREFACE) {
            return Ok(bytes);
        }
        if HTTP2_PREFACE.starts_with(&bytes) {
            continue;
        }
        let start = old_len.saturating_sub(3);
        if bytes[start..].windows(4).any(|w| w == b"\r\n\r\n") {
            validate_head(&bytes, limits)?;
            return Ok(bytes);
        }
    }
}
fn validate_head(bytes: &[u8], limits: &Limits) -> Result<(), HeadError> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut request = httparse::Request::new(&mut headers[..limits.header_count]);
    match request.parse(bytes) {
        Ok(httparse::Status::Complete(_)) => {}
        Err(httparse::Error::TooManyHeaders) => return Err(HeadError::TooLarge),
        _ => return Err(HeadError::Invalid),
    }
    let mut lengths = 0;
    let mut transfers = 0;
    for header in request.headers {
        if header.name.eq_ignore_ascii_case("content-length") {
            lengths += 1;
            if header.value.is_empty()
                || !header.value.iter().all(u8::is_ascii_digit)
                || std::str::from_utf8(header.value)
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .is_none()
            {
                return Err(HeadError::Invalid);
            }
        }
        if header.name.eq_ignore_ascii_case("transfer-encoding") {
            transfers += 1;
            if !header.value.eq_ignore_ascii_case(b"chunked") {
                return Err(HeadError::Invalid);
            }
        }
    }
    if lengths > 1 || transfers > 1 || (lengths > 0 && transfers > 0) {
        return Err(HeadError::Invalid);
    }
    Ok(())
}

struct PrefixedIo {
    stream: TcpStream,
    prefix: Vec<u8>,
    position: usize,
    http2_headers: Option<Http2HeaderDeadline>,
    flushed: watch::Sender<u64>,
    head_signal: Arc<std::sync::Mutex<Option<Arc<HeadSignal>>>>,
    head_tail: Vec<u8>,
    head_written: bool,
}
impl AsyncRead for PrefixedIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.position < this.prefix.len() {
            let count = out.remaining().min(this.prefix.len() - this.position);
            let bytes = &this.prefix[this.position..this.position + count];
            if let Some(headers) = &mut this.http2_headers {
                headers.observe(bytes)?;
            }
            out.put_slice(bytes);
            this.position += count;
            if this.position == this.prefix.len() {
                this.prefix = Vec::new();
            }
            return Poll::Ready(Ok(()));
        }
        let before = out.filled().len();
        match Pin::new(&mut this.stream).poll_read(cx, out) {
            Poll::Ready(Ok(())) => {
                if let Some(headers) = &mut this.http2_headers {
                    headers.observe(&out.filled()[before..])?;
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}
impl AsyncWrite for PrefixedIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write(cx, bytes);
        if !self.head_written
            && let Poll::Ready(Ok(n)) = &result
        {
            for &b in &bytes[..*n] {
                self.head_tail.push(b);
                if self.head_tail.ends_with(b"\r\n\r\n") {
                    self.head_written = true;
                    self.head_tail.clear();
                    if let Some(signal) = self
                        .head_signal
                        .lock()
                        .expect("헤더 전송 신호 잠금")
                        .as_ref()
                    {
                        signal
                            .sent
                            .store(true, std::sync::atomic::Ordering::Release);
                        signal.ready.notify_waiters();
                    }
                    break;
                }
                if self.head_tail.len() > 4 {
                    self.head_tail.remove(0);
                }
            }
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.stream).poll_flush(cx);
        if result.is_ready() {
            self.flushed.send_modify(|n| *n = n.wrapping_add(1));
        }
        result
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

/// HPACK/body는 Hyper에 그대로 전달하고 9-byte frame 경계만 관찰한다.
/// 최초 HEADERS 완료 전과 이후 각 HEADERS/CONTINUATION block의 기한은 bytes로 연장하지 않는다.
/// HTTP2 header block은 서로 interleave되지 않으며 해당 프로토콜 유효성 검사는 Hyper가 맡는다.
struct Http2HeaderDeadline {
    deadline: watch::Sender<Option<Instant>>,
    timeout: Duration,
    preface_left: usize,
    frame_header: [u8; 9],
    header_len: usize,
    payload_left: usize,
    ends_headers: bool,
    block_deadline: Option<Instant>,
    frame_deadline: Option<Instant>,
}
impl Http2HeaderDeadline {
    fn new(started_at: Instant, timeout: Duration) -> Self {
        let initial = Some(started_at + timeout);
        let (deadline, _) = watch::channel(initial);
        Self {
            deadline,
            timeout,
            preface_left: HTTP2_PREFACE.len(),
            frame_header: [0; 9],
            header_len: 0,
            payload_left: 0,
            ends_headers: false,
            block_deadline: initial,
            frame_deadline: None,
        }
    }
    fn observe(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        if self
            .deadline
            .borrow()
            .is_some_and(|at| Instant::now() >= at)
        {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let skip = self.preface_left.min(bytes.len());
        self.preface_left -= skip;
        bytes = &bytes[skip..];
        while !bytes.is_empty() {
            if self.header_len < 9 {
                if self.header_len == 0 && self.block_deadline.is_none() {
                    self.frame_deadline = Some(Instant::now() + self.timeout);
                }
                let count = (9 - self.header_len).min(bytes.len());
                self.frame_header[self.header_len..self.header_len + count]
                    .copy_from_slice(&bytes[..count]);
                self.header_len += count;
                bytes = &bytes[count..];
                if self.header_len < 9 {
                    break;
                }
                self.payload_left = ((self.frame_header[0] as usize) << 16)
                    | ((self.frame_header[1] as usize) << 8)
                    | self.frame_header[2] as usize;
                let is_headers = matches!(self.frame_header[3], 1 | 9);
                if is_headers && self.block_deadline.is_none() {
                    self.block_deadline = self.frame_deadline;
                }
                self.frame_deadline = None;
                self.ends_headers = is_headers && self.frame_header[4] & 4 != 0;
            }
            let count = self.payload_left.min(bytes.len());
            self.payload_left -= count;
            bytes = &bytes[count..];
            if self.payload_left == 0 {
                if self.ends_headers {
                    self.block_deadline = None;
                }
                self.header_len = 0;
            }
        }
        let next = self.block_deadline.or(self.frame_deadline);
        self.deadline.send_if_modified(|current| {
            if *current == next {
                false
            } else {
                *current = next;
                true
            }
        });
        Ok(())
    }
}

async fn http2_header_expired(mut deadline: Option<watch::Receiver<Option<Instant>>>) {
    let Some(deadline) = deadline.as_mut() else {
        return std::future::pending().await;
    };
    loop {
        let at = *deadline.borrow_and_update();
        match at {
            Some(at) => tokio::select! {
                biased;
                _=sleep_until(at)=>{
                    if deadline.borrow().is_some_and(|current| Instant::now() >= current) {
                        return;
                    }
                },
                result=deadline.changed()=>if result.is_err(){return;},
            },
            None => {
                if deadline.changed().await.is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::advance;

    #[tokio::test(start_paused = true)]
    async fn http2_completed_headers_do_not_expire_from_an_obsolete_watchdog_timer() {
        let mut headers = Http2HeaderDeadline::new(Instant::now(), Duration::from_millis(100));
        let mut watchdog = Box::pin(http2_header_expired(Some(headers.deadline.subscribe())));
        let pending =
            std::future::poll_fn(|cx| Poll::Ready(watchdog.as_mut().poll(cx).is_pending()));
        assert!(pending.await);
        advance(Duration::from_millis(99)).await;
        headers.observe(HTTP2_PREFACE).unwrap();
        headers.observe(b"\0\0\x01\x01\x05\0\0\0\x01\x82").unwrap();
        advance(Duration::from_millis(1)).await;
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(watchdog.as_mut().poll(cx).is_pending())).await,
            "이전 timer를 늦게 poll해도 기한 전 완료된 헤더를 timeout으로 바꾸면 안 된다"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn http2_initial_header_deadline_includes_preface_settings_ping_and_progress() {
        let started = Instant::now();
        let mut headers = Http2HeaderDeadline::new(started, Duration::from_millis(100));
        headers.observe(HTTP2_PREFACE).unwrap();
        headers.observe(b"\0\0\0\x04\0\0\0\0\0").unwrap();
        advance(Duration::from_millis(40)).await;
        headers.observe(b"\0\0\x08\x06\0\0\0\0\0abcdefgh").unwrap();
        advance(Duration::from_millis(59)).await;
        headers.observe(b"\0\0\x01\x01\x01\0\0\0\x01\x82").unwrap();
        assert_eq!(
            *headers.deadline.borrow(),
            Some(started + Duration::from_millis(100))
        );
        advance(Duration::from_millis(1)).await;
        assert_eq!(
            headers
                .observe(b"\0\0\0\x09\x04\0\0\0\x01")
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[tokio::test(start_paused = true)]
    async fn http2_later_header_deadline_starts_at_first_frame_byte_and_never_extends() {
        let mut headers = Http2HeaderDeadline::new(Instant::now(), Duration::from_millis(100));
        headers.observe(HTTP2_PREFACE).unwrap();
        headers.observe(b"\0\0\x01\x01\x05\0\0\0\x01\x82").unwrap();
        assert_eq!(*headers.deadline.borrow(), None);
        advance(Duration::from_secs(1)).await;
        headers.observe(b"\0").unwrap();
        let fixed = *headers.deadline.borrow();
        assert_eq!(fixed, Some(Instant::now() + Duration::from_millis(100)));
        advance(Duration::from_millis(40)).await;
        headers.observe(b"\0\x01\x01\x01\0\0\0\x03\x82").unwrap();
        advance(Duration::from_millis(59)).await;
        headers.observe(b"\0\0\x01\x09\0\0\0\0\x03\x86").unwrap();
        assert_eq!(*headers.deadline.borrow(), fixed);
        advance(Duration::from_millis(1)).await;
        assert_eq!(
            headers
                .observe(b"\0\0\0\x09\x04\0\0\0\x03")
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
    }
}
