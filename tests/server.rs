use m_gated::{
    config::Limits,
    http::{AppState, router},
    runtime::Runtime,
    server,
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};

struct Server {
    address: SocketAddr,
    runtime: Arc<Runtime>,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<std::io::Result<()>>,
}
impl Server {
    async fn start(limits: Limits) -> Self {
        Self::start_with(limits, axum::Router::new()).await
    }
    async fn start_with(limits: Limits, extra: axum::Router) -> Self {
        let runtime = Runtime::new(limits).unwrap();
        runtime.start_http().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, receive) = oneshot::channel();
        let task = tokio::spawn(server::serve(
            listener,
            router(AppState {
                runtime: runtime.clone(),
            })
            .merge(extra),
            runtime.clone(),
            async move {
                let _ = receive.await;
            },
        ));
        Self {
            address,
            runtime,
            stop: Some(stop),
            task,
        }
    }
    async fn connect(&self) -> TcpStream {
        TcpStream::connect(self.address).await.unwrap()
    }
    async fn shutdown(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        timeout(Duration::from_secs(2), &mut self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            self.runtime.lifecycle(),
            m_gated::runtime::Lifecycle::Stopped
        );
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn read_head(stream: &mut TcpStream) -> Vec<u8> {
    timeout(Duration::from_secs(2), async {
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") && head.len() < 65536 {
            let mut b = [0];
            if stream.read(&mut b).await.unwrap() == 0 {
                break;
            }
            head.push(b[0]);
        }
        head
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn tcp_connection_cap_rejects_new_socket_and_recovers_on_release() {
    let (entered, mut started) = tokio::sync::mpsc::channel(1);
    let release = Arc::new(tokio::sync::Notify::new());
    let release_handler = release.clone();
    let extra = axum::Router::new().route(
        "/held",
        axum::routing::get(move || {
            let entered = entered.clone();
            let release = release_handler.clone();
            async move {
                entered.send(()).await.unwrap();
                release.notified().await;
                "held"
            }
        }),
    );
    let server = Server::start_with(
        Limits {
            connections: 1,
            ..Limits::default()
        },
        extra,
    )
    .await;
    let mut held = server.connect().await;
    held.write_all(b"GET /held HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    timeout(Duration::from_secs(2), started.recv())
        .await
        .unwrap()
        .unwrap();
    let mut rejected = server.connect().await;
    let _ = rejected
        .write_all(b"GET /livez HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await;
    let mut response = Vec::new();
    let _ = timeout(Duration::from_secs(2), rejected.read_to_end(&mut response))
        .await
        .unwrap();
    assert!(
        response.is_empty(),
        "연결 permit이 없으면 HTTP 업무를 시작하지 않는다"
    );
    release.notify_one();
    let mut closed = Vec::new();
    timeout(Duration::from_secs(2), held.read_to_end(&mut closed))
        .await
        .unwrap()
        .unwrap();
    assert!(closed.starts_with(b"HTTP/1.1 200"));
    let mut recovered = server.connect().await;
    recovered
        .write_all(b"GET /livez HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    assert!(read_head(&mut recovered).await.starts_with(b"HTTP/1.1 200"));
    drop(recovered);
    drop(held);
    drop(rejected);
    server.shutdown().await;
}

#[tokio::test]
async fn slow_header_is_closed_at_configured_total_deadline() {
    let server = Server::start(Limits {
        header_timeout_ms: 50,
        ..Limits::default()
    })
    .await;
    let mut stream = server.connect().await;
    stream
        .write_all(b"GET /livez HTTP/1.1\r\nHost: ")
        .await
        .unwrap();
    let mut response = Vec::new();
    let read = timeout(
        Duration::from_millis(500),
        stream.read_to_end(&mut response),
    )
    .await;
    assert!(
        read.is_ok(),
        "부분 헤더는 진행과 무관하게 50ms 총기한으로 종료해야 한다"
    );
    assert!(response.is_empty() || response.starts_with(b"HTTP/1.1 408"));
}

#[tokio::test]
async fn http2_settings_advertise_the_fixed_stream_limit() {
    let server = Server::start(Limits::default()).await;
    let mut stream = server.connect().await;
    stream
        .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\0\0\0\x04\0\0\0\0\0")
        .await
        .unwrap();
    let advertised = timeout(Duration::from_secs(2), async {
        let mut frame = [0; 9];
        stream.read_exact(&mut frame).await.unwrap();
        assert_eq!(frame[3], 4, "서버의 첫 frame은 SETTINGS다");
        let size = ((frame[0] as usize) << 16) | ((frame[1] as usize) << 8) | frame[2] as usize;
        assert!(size <= 16384);
        let mut payload = vec![0; size];
        stream.read_exact(&mut payload).await.unwrap();
        payload.chunks_exact(6).find_map(|p| {
            if p[..2] == [0, 3] {
                Some(u32::from_be_bytes(p[2..].try_into().unwrap()))
            } else {
                None
            }
        })
    })
    .await
    .unwrap();
    assert_eq!(
        advertised,
        Some(16),
        "연결당 HTTP2 stream은 16개로 제한한다"
    );
}

#[tokio::test]
async fn framing_header_count_and_bytes_are_rejected_before_route_work() {
    let server = Server::start(Limits::default()).await;
    for head in [
        format!("GET /livez HTTP/1.1\r\nHost: localhost\r\n{}\r\n",(0..65).map(|n|format!("x-{n}: value\r\n")).collect::<String>()),
        format!("GET /livez HTTP/1.1\r\nHost: localhost\r\nX-Large: {}\r\n\r\n","a".repeat(32768)),
        "POST /unsupported HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n".into(),
    ] {
        let mut stream=server.connect().await;stream.write_all(head.as_bytes()).await.unwrap();
        let response=read_head(&mut stream).await;
        assert!(response.is_empty() || response.starts_with(b"HTTP/1.1 431") || response.starts_with(b"HTTP/1.1 400"),"framing/헤더 초과는 route 성공이 아니다: {}",String::from_utf8_lossy(&response));
    }
}

#[tokio::test]
async fn raw_framing_conflicts_are_rejected_before_any_handler() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = calls.clone();
    let extra = axum::Router::new().route(
        "/echo",
        axum::routing::any(move || {
            handler_calls.fetch_add(1, Ordering::SeqCst);
            async { "unexpected" }
        }),
    );
    let server = Server::start_with(Limits::default(), extra).await;
    for headers in [
        "Content-Length: 0\r\nTransfer-Encoding: chunked",
        "transfer-encoding:\tchunked\r\nCONTENT-LENGTH:\t0",
        "Content-Length : 0",
        "Content-Length: +0",
        "Content-Length: 0,0",
        "Content-Length: 0\r\nContent-Length: 0",
        "Transfer-Encoding: gzip, chunked",
        "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked",
    ] {
        let mut stream = server.connect().await;
        stream.write_all(format!("POST /echo HTTP/1.1\r\nHost: localhost\r\nX-Sentinel: do-not-reflect-sentinel\r\n{headers}\r\n\r\n0\r\n\r\n").as_bytes()).await.unwrap();
        let mut response = Vec::new();
        timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.starts_with(b"HTTP/1.1 400"));
        assert!(!String::from_utf8_lossy(&response).contains("do-not-reflect-sentinel"));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn prefetched_content_length_chunked_bodies_and_header_boundaries_preserve_bytes() {
    use axum::response::IntoResponse;
    let extra = axum::Router::new().route(
        "/echo",
        axum::routing::any(|request: axum::extract::Request| async {
            axum::body::Body::from(
                axum::body::to_bytes(request.into_body(), 64 * 1024)
                    .await
                    .unwrap(),
            )
            .into_response()
        }),
    );
    let server = Server::start_with(Limits::default(), extra).await;
    for (framing, body, expected) in [
        (
            "Content-Length: 15",
            b"{\"test\": true}\n".as_slice(),
            b"{\"test\": true}\n".as_slice(),
        ),
        (
            "Transfer-Encoding: chunked",
            b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n".as_slice(),
            b"hello world".as_slice(),
        ),
    ] {
        let mut stream = server.connect().await;
        let mut request =
            format!("POST /echo HTTP/1.1\r\nHost: localhost\r\n{framing}\r\n\r\n").into_bytes();
        request.extend_from_slice(body);
        stream.write_all(&request).await.unwrap();
        let mut response = Vec::new();
        timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200"));
        assert!(response.ends_with(expected));
        assert!(String::from_utf8_lossy(&response).contains("connection: close"));
    }
    let mut stream = server.connect().await;
    // Host+63개로 정확히64 headers다. 상한을 완화하지 않고 경계를 통과한다.
    let request = format!(
        "GET /livez HTTP/1.1\r\nHost: localhost\r\n{}\r\n",
        (0..63).map(|i| format!("X-{i}: v\r\n")).collect::<String>()
    );
    for chunk in request.as_bytes().chunks(3) {
        stream.write_all(chunk).await.unwrap();
    }
    let mut response = Vec::new();
    timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(response.starts_with(b"HTTP/1.1 200"));
}

#[tokio::test]
async fn inbound_http1_pipeline_dispatches_only_first_request_without_truncating_response() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = calls.clone();
    let extra = axum::Router::new().route(
        "/large",
        axum::routing::any(move || {
            handler_calls.fetch_add(1, Ordering::SeqCst);
            async { vec![b'x'; 64 * 1024] }
        }),
    );
    let server = Server::start_with(Limits::default(), extra).await;
    let mut stream = server.connect().await;
    stream.write_all(b"GET /large HTTP/1.1\r\nHost: localhost\r\n\r\nGET /large HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
    let head = read_head(&mut stream).await;
    assert!(head.starts_with(b"HTTP/1.1 200"));
    let mut body = Vec::new();
    timeout(Duration::from_secs(2), stream.read_to_end(&mut body))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(body, vec![b'x'; 64 * 1024]);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

async fn h2_frame(stream: &mut TcpStream) -> (u8, u8, u32, Vec<u8>) {
    let mut head = [0_u8; 9];
    stream.read_exact(&mut head).await.unwrap();
    let length = ((head[0] as usize) << 16) | ((head[1] as usize) << 8) | (head[2] as usize);
    assert!(length <= 16384);
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).await.unwrap();
    (
        head[3],
        head[4],
        u32::from_be_bytes(head[5..9].try_into().unwrap()) & 0x7fff_ffff,
        payload,
    )
}
async fn send_h2_headers(stream: &mut TcpStream, id: u32) {
    let block = b"\x82\x86\x04\x05/held\x01\x09localhost";
    let mut frame = vec![0, 0, block.len() as u8, 1, 5];
    frame.extend_from_slice(&id.to_be_bytes());
    frame.extend_from_slice(block);
    stream.write_all(&frame).await.unwrap();
}

#[tokio::test]
async fn http2_rejects_seventeenth_concurrent_stream_and_keeps_first_sixteen() {
    let (entered, mut started) = tokio::sync::mpsc::channel(32);
    let release = Arc::new(tokio::sync::Notify::new());
    let release_handler = release.clone();
    let extra = axum::Router::new().route(
        "/held",
        axum::routing::any(move || {
            let entered = entered.clone();
            let release = release_handler.clone();
            async move {
                entered.send(()).await.unwrap();
                release.notified().await;
                "held"
            }
        }),
    );
    let server = Server::start_with(Limits::default(), extra).await;
    let mut stream = server.connect().await;
    // preface를 두 부분으로 보내며 다른 연결의 프로브를 사이에 관찰한다.
    stream.write_all(b"PRI * HTTP/2.0\r\n").await.unwrap();
    let mut probe = server.connect().await;
    probe
        .write_all(b"GET /livez HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    assert!(read_head(&mut probe).await.starts_with(b"HTTP/1.1 200"));
    drop(probe);
    stream
        .write_all(b"\r\nSM\r\n\r\n\0\0\0\x04\0\0\0\0\0")
        .await
        .unwrap();
    let (kind, _, _, _) = timeout(Duration::from_secs(2), h2_frame(&mut stream))
        .await
        .unwrap();
    assert_eq!(kind, 4);
    stream.write_all(b"\0\0\0\x04\x01\0\0\0\0").await.unwrap();
    for id in (1..=33).step_by(2) {
        send_h2_headers(&mut stream, id).await;
    }
    for _ in 0..16 {
        timeout(Duration::from_secs(2), started.recv())
            .await
            .unwrap()
            .unwrap();
    }
    timeout(Duration::from_secs(2), async {
        for _ in 0..40 {
            let (kind, _, id, payload) = h2_frame(&mut stream).await;
            if kind == 3 && id == 33 {
                assert_eq!(
                    payload,
                    7_u32.to_be_bytes(),
                    "초과 stream은 REFUSED_STREAM이다"
                );
                return;
            }
        }
        panic!("초과 stream에 대한 RST_STREAM이 없다");
    })
    .await
    .unwrap();
    assert!(
        started.try_recv().is_err(),
        "17번째 handler는 실행하지 않는다"
    );
    release.notify_waiters();
    timeout(Duration::from_secs(2), async {
        let mut complete = std::collections::BTreeSet::new();
        while complete.len() < 16 {
            let (kind, flags, id, _) = h2_frame(&mut stream).await;
            if (kind == 0 || kind == 1) && flags & 1 == 1 {
                complete.insert(id);
            }
        }
        assert!(!complete.contains(&33));
    })
    .await
    .unwrap();
}

struct EventBody(tokio::sync::mpsc::Receiver<axum::body::Bytes>);
impl http_body::Body for EventBody {
    type Data = axum::body::Bytes;
    type Error = std::convert::Infallible;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        self.0
            .poll_recv(cx)
            .map(|event| event.map(|bytes| Ok(http_body::Frame::data(bytes))))
    }
}

#[tokio::test]
async fn http1_close_and_graceful_shutdown_keep_stream_until_body_end() {
    use axum::response::IntoResponse;
    let (send, receive) = tokio::sync::mpsc::channel(2);
    let receiver = Arc::new(std::sync::Mutex::new(Some(receive)));
    let extra = axum::Router::new().route(
        "/events",
        axum::routing::get(move || {
            let receive = receiver.lock().unwrap().take().unwrap();
            async {
                (
                    [("content-type", "text/event-stream")],
                    axum::body::Body::new(EventBody(receive)),
                )
                    .into_response()
            }
        }),
    );
    let mut server = Server::start_with(Limits::default(), extra).await;
    let mut stream = server.connect().await;
    send.send(axum::body::Bytes::from_static(b"data: first\n\n"))
        .await
        .unwrap();
    stream
        .write_all(b"GET /events HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let head = read_head(&mut stream).await;
    assert!(head.starts_with(b"HTTP/1.1 200"));
    let mut first = vec![0; 18];
    timeout(Duration::from_secs(2), stream.read_exact(&mut first))
        .await
        .unwrap()
        .unwrap();
    assert!(first.windows(13).any(|w| w == b"data: first\n\n"));
    server.stop.take().unwrap().send(()).unwrap();
    send.send(axum::body::Bytes::from_static(b"data: last\n\n"))
        .await
        .unwrap();
    drop(send);
    let mut remaining = Vec::new();
    timeout(Duration::from_secs(2), stream.read_to_end(&mut remaining))
        .await
        .unwrap()
        .unwrap();
    assert!(remaining.windows(12).any(|w| w == b"data: last\n\n"));
    assert!(remaining.ends_with(b"0\r\n\r\n"));
    timeout(Duration::from_secs(2), &mut server.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        server.runtime.lifecycle(),
        m_gated::runtime::Lifecycle::Stopped
    );
}

async fn assert_connection_closed_and_probe_recovers(server: &Server, stream: &mut TcpStream) {
    let mut remaining = Vec::new();
    assert!(
        timeout(
            Duration::from_millis(700),
            stream.read_to_end(&mut remaining)
        )
        .await
        .is_ok(),
        "불완전 HTTP2 헤더는 고정 총기한에 닫혀야 한다"
    );
    let mut probe = server.connect().await;
    probe
        .write_all(b"GET /livez HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    assert!(
        read_head(&mut probe).await.starts_with(b"HTTP/1.1 200"),
        "헤더 timeout 뒤 connection permit을 반환해야 한다"
    );
}

#[tokio::test]
async fn http2_incomplete_handshake_and_headers_expire_and_release_connection_cap() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = Arc::new(AtomicUsize::new(0));
    for suffix in [
        b"".as_slice(),
        b"\0\0\0\x04\0\0\0\0\0",
        b"\0\0\0\x04\0\0\0\0\0\0\0\x01\x01\x01\0\0\0\x01\x82",
        b"\0\0\0\x04\0\0\0\0\0\0\0\x01\x01\x01\0\0\0\x01\x82\0\0\x01\x09\0\0\0\0\x01\x86",
        b"\0\0\0\x04\0\0\0\0\0\0\0\x08\x01\x05\0\0\0\x01\x82",
        b"\0\0\0\x04\0\0\0\0\0\0\0",
    ] {
        let handler_calls = calls.clone();
        let extra = axum::Router::new().route(
            "/held",
            axum::routing::any(move || {
                handler_calls.fetch_add(1, Ordering::SeqCst);
                async { "unexpected" }
            }),
        );
        let server = Server::start_with(
            Limits {
                connections: 1,
                header_timeout_ms: 100,
                ..Limits::default()
            },
            extra,
        )
        .await;
        let mut stream = server.connect().await;
        let mut request = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
        request.extend_from_slice(suffix);
        stream.write_all(&request).await.unwrap();
        assert_connection_closed_and_probe_recovers(&server, &mut stream).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "미완성 HEADERS는 dispatch하지 않는다"
        );
        server.shutdown().await;
    }
}

#[tokio::test]
async fn http2_later_incomplete_header_block_has_its_own_deadline() {
    let (entered, mut started) = tokio::sync::mpsc::channel(1);
    let extra = axum::Router::new().route(
        "/held",
        axum::routing::any(move || {
            let entered = entered.clone();
            async move {
                entered.send(()).await.unwrap();
                "held"
            }
        }),
    );
    let server = Server::start_with(
        Limits {
            connections: 1,
            header_timeout_ms: 100,
            ..Limits::default()
        },
        extra,
    )
    .await;
    let mut stream = server.connect().await;
    stream
        .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\0\0\0\x04\0\0\0\0\0")
        .await
        .unwrap();
    send_h2_headers(&mut stream, 1).await;
    timeout(Duration::from_secs(2), started.recv())
        .await
        .unwrap()
        .unwrap();
    stream
        .write_all(b"\0\0\x01\x01\x01\0\0\0\x03\x82")
        .await
        .unwrap();
    assert_connection_closed_and_probe_recovers(&server, &mut stream).await;
    assert!(
        started.try_recv().is_err(),
        "두 번째 불완전 stream의 handler는 실행하지 않는다"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn http2_complete_headers_leave_multiplexed_sse_alive_past_header_deadline() {
    use axum::response::IntoResponse;
    let (send, receive) = tokio::sync::mpsc::channel(2);
    let receiver = Arc::new(std::sync::Mutex::new(Some(receive)));
    let extra = axum::Router::new().route(
        "/held",
        axum::routing::any(move || {
            let receive = receiver.lock().unwrap().take().unwrap();
            async {
                (
                    [("content-type", "text/event-stream")],
                    axum::body::Body::new(EventBody(receive)),
                )
                    .into_response()
            }
        }),
    );
    let server = Server::start_with(
        Limits {
            header_timeout_ms: 100,
            ..Limits::default()
        },
        extra,
    )
    .await;
    let mut stream = server.connect().await;
    stream
        .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\0\0\0\x04\0\0\0\0\0")
        .await
        .unwrap();
    send.send(axum::body::Bytes::from_static(b"data: first\n\n"))
        .await
        .unwrap();
    send_h2_headers(&mut stream, 1).await;
    // 같은 연결에서 독립 probe stream도 완료되어야 한다.
    let block = b"\x82\x86\x04\x06/livez\x01\x09localhost";
    let mut frame = vec![0, 0, block.len() as u8, 1, 5, 0, 0, 0, 3];
    frame.extend_from_slice(block);
    stream.write_all(&frame).await.unwrap();
    timeout(Duration::from_secs(2), async {
        let mut first = false;
        let mut probe = false;
        while !first || !probe {
            let (kind, flags, id, payload) = h2_frame(&mut stream).await;
            if kind == 4 && flags == 0 {
                stream.write_all(b"\0\0\0\x04\x01\0\0\0\0").await.unwrap();
            }
            if kind == 0 && id == 1 {
                assert_eq!(payload, b"data: first\n\n");
                assert_eq!(flags & 1, 0);
                first = true;
            }
            if (kind == 0 || kind == 1) && id == 3 && flags & 1 != 0 {
                probe = true;
            }
        }
    })
    .await
    .unwrap();
    let idle = timeout(Duration::from_millis(250), async {
        loop {
            let (kind, flags, id, _) = h2_frame(&mut stream).await;
            assert_ne!(
                kind, 7,
                "완료된 헤더 뒤 SSE를 연결 전체 timeout으로 닫으면 안 된다"
            );
            assert!(!(id == 1 && flags & 1 != 0), "SSE가 조기 종료되었다");
        }
    })
    .await;
    assert!(idle.is_err(), "헤더 기한 이후에도 SSE는 열린 상태다");
    send.send(axum::body::Bytes::from_static(b"data: last\n\n"))
        .await
        .unwrap();
    drop(send);
    timeout(Duration::from_secs(2), async {
        let mut last = false;
        loop {
            let (kind, flags, id, payload) = h2_frame(&mut stream).await;
            if kind == 0 && id == 1 {
                if !payload.is_empty() {
                    assert_eq!(payload, b"data: last\n\n");
                    last = true;
                }
                if flags & 1 != 0 {
                    assert!(last);
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn asset_deadline_releases_hyper_owned_frame_on_stalled_http1_socket() {
    use axum::response::IntoResponse;
    struct Owned {
        bytes: Vec<u8>,
        released: Option<oneshot::Sender<()>>,
    }
    impl AsRef<[u8]> for Owned {
        fn as_ref(&self) -> &[u8] {
            &self.bytes
        }
    }
    impl Drop for Owned {
        fn drop(&mut self) {
            if let Some(tx) = self.released.take() {
                let _ = tx.send(());
            }
        }
    }
    let (tx, released) = oneshot::channel();
    let flag = Arc::new(std::sync::Mutex::new(Some(tx)));
    let extra = axum::Router::new().route(
        "/ui/assets/slow",
        axum::routing::get(move || {
            let flag = flag.clone();
            async move {
                axum::body::Bytes::from_owner(Owned {
                    bytes: vec![b'x'; 16 * 1024 * 1024],
                    released: flag.lock().unwrap().take(),
                })
                .into_response()
            }
        }),
    );
    let server = Server::start_with(
        Limits {
            asset_timeout_ms: 100,
            ..Limits::default()
        },
        extra,
    )
    .await;
    let mut socket = server.connect().await;
    socket
        .write_all(b"GET /ui/assets/slow HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    assert!(read_head(&mut socket).await.starts_with(b"HTTP/1.1 200"));
    // Body frame Drop만으로 socket 전송 완료를 추정하지 않는다.
    tokio::time::timeout(Duration::from_millis(700), released)
        .await
        .expect("owner bytes 해제 신호")
        .unwrap();
    server.shutdown().await;
}

#[tokio::test]
async fn http2_flow_control_deadline_releases_asset_permit_and_isolates_other_streams() {
    use axum::{extract::Request, response::IntoResponse};
    use m_gated::runtime::{Dependency, RouteClass};
    let runtime_slot = Arc::new(std::sync::OnceLock::<Arc<Runtime>>::new());
    let slot = runtime_slot.clone();
    let extra = axum::Router::new().route(
        "/ui/assets/slow",
        axum::routing::any(move |_: Request| {
            let runtime = slot.get().unwrap().clone();
            async move {
                match runtime.try_admit(RouteClass::Asset) {
                    Ok(lease) => lease.bind_response(vec![b'x'; 16 * 1024 * 1024].into_response()),
                    Err(_) => (axum::http::StatusCode::TOO_MANY_REQUESTS, "held").into_response(),
                }
            }
        }),
    );
    let server = Server::start_with(
        Limits {
            assets: 1,
            asset_timeout_ms: 150,
            ..Limits::default()
        },
        extra,
    )
    .await;
    runtime_slot.set(server.runtime.clone()).ok().unwrap();
    server
        .runtime
        .record_health(Dependency::Ui, true, tokio::time::Instant::now());
    let mut builder = h2::client::Builder::new();
    builder.initial_window_size(16 * 1024);
    let (mut sender, connection) = builder
        .handshake::<_, axum::body::Bytes>(server.connect().await)
        .await
        .unwrap();
    let connection = tokio::spawn(connection);
    let request = axum::http::Request::builder()
        .uri("http://localhost/ui/assets/slow")
        .body(())
        .unwrap();
    let (response, _) = sender.send_request(request, true).unwrap();
    let mut slow = response.await.unwrap().into_body();
    let held = slow.data().await.unwrap().unwrap();
    assert_eq!(held.len(), 16 * 1024);
    assert!(
        server.runtime.try_admit(RouteClass::Asset).is_err(),
        "느린 body 전송 동안 asset permit을 보유한다"
    );
    let (response, _) = sender
        .send_request(
            axum::http::Request::builder()
                .uri("http://localhost/livez")
                .body(())
                .unwrap(),
            true,
        )
        .unwrap();
    let mut probe = response.await.unwrap();
    assert_eq!(probe.status(), 200);
    assert_eq!(probe.body_mut().data().await.unwrap().unwrap(), "ok");
    let reset = timeout(Duration::from_secs(1), slow.data())
        .await
        .expect("개별 stream 총기한")
        .expect("RST_STREAM")
        .expect_err("성공 EOF가 아닌 reset이어야 한다");
    assert!(reset.is_reset());
    assert!(
        server.runtime.try_admit(RouteClass::Asset).is_ok(),
        "reset 뒤 body/transport owner와 permit을 반환한다"
    );
    let (response, _) = sender
        .send_request(
            axum::http::Request::builder()
                .uri("http://localhost/livez")
                .body(())
                .unwrap(),
            true,
        )
        .unwrap();
    assert_eq!(response.await.unwrap().status(), 200);
    drop(held);
    drop(slow);
    drop(sender);
    connection.abort();
    let _ = connection.await;
    server.shutdown().await;
}

async fn response_class_deadline_case(h2: bool, asset: bool, full_deadline: bool) {
    use axum::response::IntoResponse;
    use m_gated::runtime::{Dependency, RouteClass};
    let class = if asset {
        RouteClass::Asset
    } else {
        RouteClass::ManagementRead
    };
    let path = if asset {
        "/ui/assets/slow"
    } else {
        "/gateway/slow"
    };
    let mut limits = Limits {
        assets: 1,
        management: 1,
        ..Limits::default()
    };
    if !full_deadline {
        limits.asset_timeout_ms = 120;
        limits.management_timeout_ms = 120;
    }
    let expected = Duration::from_millis(if asset {
        limits.asset_timeout_ms
    } else {
        limits.management_timeout_ms
    });
    let runtime = Runtime::new(limits).unwrap();
    runtime.start_http().unwrap();
    for d in [
        Dependency::Database,
        Dependency::Jwks,
        Dependency::Schema,
        Dependency::Ui,
    ] {
        runtime.record_health(d, true, tokio::time::Instant::now());
    }
    let (rt, bytes) = (
        runtime.clone(),
        if asset { 16 * 1024 * 1024 } else { 1024 * 1024 },
    );
    let (tx, rx) = tokio::sync::mpsc::channel::<axum::body::Bytes>(2);
    let receiver = Arc::new(tokio::sync::Mutex::new(Some(rx)));
    let received = receiver.clone();
    let routes = axum::Router::new()
        .route(
            path,
            axum::routing::any(move |request: axum::extract::Request| {
                let rt = rt.clone();
                async move {
                    let http = request
                        .extensions()
                        .get::<m_gated::runtime::HttpAdmission>()
                        .unwrap()
                        .clone();
                    rt.try_admit_with_http(class, http)
                        .unwrap()
                        .bind_response(vec![b'x'; bytes].into_response())
                }
            }),
        )
        .route(
            "/anthropic/v1/messages",
            axum::routing::get(move || {
                let receiver = received.clone();
                async move {
                    let rx = receiver.lock().await.take().unwrap();
                    axum::body::Body::from_stream(futures_util::stream::unfold(
                        rx,
                        |mut rx| async {
                            rx.recv().await.map(|b| (Ok::<_, std::io::Error>(b), rx))
                        },
                    ))
                }
            }),
        );
    let app = m_gated::http::router_with_components(
        AppState {
            runtime: runtime.clone(),
        },
        None,
        routes,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    socket2::SockRef::from(&listener)
        .set_send_buffer_size(4096)
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, shutdown) = oneshot::channel();
    let r = runtime.clone();
    let task = tokio::spawn(server::serve(listener, app, r, async {
        let _ = shutdown.await;
    }));
    let socket = TcpStream::connect(addr).await.unwrap();
    socket2::SockRef::from(&socket)
        .set_recv_buffer_size(4096)
        .unwrap();
    let started = tokio::time::Instant::now();
    let mut changes = runtime.resource_changes();
    if h2 {
        let mut builder = h2::client::Builder::new();
        builder.initial_window_size(16 * 1024);
        let (mut client, connection) = builder
            .handshake::<_, axum::body::Bytes>(socket)
            .await
            .unwrap();
        let connection = tokio::spawn(connection);
        let (response, _) = client
            .send_request(
                axum::http::Request::builder()
                    .uri(format!("http://localhost{path}"))
                    .body(())
                    .unwrap(),
                true,
            )
            .unwrap();
        let mut slow = response.await.unwrap().into_body();
        let held = slow.data().await.unwrap().unwrap();
        assert!(runtime.try_admit(class).is_err());
        tx.send(axum::body::Bytes::from_static(b"data: first\n\n"))
            .await
            .unwrap();
        let (events, _) = client
            .send_request(
                axum::http::Request::builder()
                    .uri("http://localhost/anthropic/v1/messages")
                    .body(())
                    .unwrap(),
                true,
            )
            .unwrap();
        let mut events = events.await.unwrap().into_body();
        assert_eq!(events.data().await.unwrap().unwrap(), "data: first\n\n");
        let (result, _) = client
            .send_request(
                axum::http::Request::builder()
                    .uri("http://localhost/livez")
                    .body(())
                    .unwrap(),
                true,
            )
            .unwrap();
        assert_eq!(result.await.unwrap().status(), 200);
        let result = timeout(expected + Duration::from_secs(3), slow.data())
            .await
            .unwrap()
            .unwrap();
        assert!(result.unwrap_err().is_reset());
        tx.send(axum::body::Bytes::from_static(b"data: final\n\n"))
            .await
            .unwrap();
        drop(tx);
        assert_eq!(events.data().await.unwrap().unwrap(), "data: final\n\n");
        while let Some(chunk) = events.data().await {
            assert!(chunk.unwrap().is_empty());
        }
        drop(held);
        drop(slow);
        drop(client);
        connection.abort();
        let _ = connection.await;
    } else {
        let mut socket = socket;
        socket
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .unwrap();
        assert!(read_head(&mut socket).await.starts_with(b"HTTP/1.1 200"));
        assert!(runtime.try_admit(class).is_err());
        timeout(expected + Duration::from_secs(3), async {
            loop {
                let available = if asset {
                    runtime.available_assets()
                } else {
                    runtime.available_management()
                };
                if available == 1 {
                    break;
                }
                changes.changed().await.unwrap();
            }
        })
        .await
        .expect("H1 kernel write 정지 중 transport owner와 permit을 총기한에 반환");
        drop(socket);
    }
    assert!(started.elapsed() >= expected.saturating_sub(Duration::from_millis(20)));
    assert!(started.elapsed() < expected + Duration::from_secs(3));
    assert_eq!(runtime.available_assets(), 1);
    assert_eq!(runtime.available_management(), 1);
    assert_eq!(runtime.active_requests(), 0);
    stop.send(()).unwrap();
    timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
#[tokio::test]
async fn management_and_asset_deadlines_cover_stalled_h1_and_h2_with_live_sse() {
    for h2 in [false, true] {
        for asset in [false, true] {
            response_class_deadline_case(h2, asset, false).await;
        }
    }
}
#[tokio::test]
#[ignore = "explicit 10s management/30s asset wall-clock transport validation"]
async fn default_management_ten_seconds_and_asset_thirty_seconds_are_wire_deadlines() {
    tokio::join!(
        response_class_deadline_case(false, false, true),
        response_class_deadline_case(true, false, true),
        response_class_deadline_case(false, true, true),
        response_class_deadline_case(true, true, true)
    );
}

#[tokio::test]
async fn http2_graceful_shutdown_preserves_existing_stream_until_body_end() {
    let (tx, rx) = tokio::sync::mpsc::channel::<axum::body::Bytes>(2);
    let rx = Arc::new(tokio::sync::Mutex::new(Some(rx)));
    let extra = axum::Router::new().route(
        "/events",
        axum::routing::get(move || {
            let rx = rx.clone();
            async move {
                let receiver = rx.lock().await.take().unwrap();
                axum::body::Body::from_stream(futures_util::stream::unfold(
                    receiver,
                    |mut rx| async { rx.recv().await.map(|b| (Ok::<_, std::io::Error>(b), rx)) },
                ))
            }
        }),
    );
    let mut server = Server::start_with(Limits::default(), extra).await;
    let (mut client, connection) = h2::client::handshake(server.connect().await).await.unwrap();
    let connection = tokio::spawn(connection);
    tx.send(axum::body::Bytes::from_static(b"data: first\n\n"))
        .await
        .unwrap();
    let (response, _) = client
        .send_request(
            axum::http::Request::builder()
                .uri("http://localhost/events")
                .body(())
                .unwrap(),
            true,
        )
        .unwrap();
    let mut body = response.await.unwrap().into_body();
    assert_eq!(body.data().await.unwrap().unwrap(), "data: first\n\n");
    server.stop.take().unwrap().send(()).unwrap();
    // GOAWAY는 기존 stream의 body를 취소하지 않는다. 마지막 body가 종료를 해제한다.
    tx.send(axum::body::Bytes::from_static(b"data: final\n\n"))
        .await
        .unwrap();
    drop(tx);
    assert_eq!(body.data().await.unwrap().unwrap(), "data: final\n\n");
    while let Some(chunk) = body.data().await {
        assert!(chunk.unwrap().is_empty());
    }
    timeout(Duration::from_secs(2), &mut server.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(body);
    drop(client);
    connection.abort();
    let _ = connection.await;
}

#[tokio::test]
async fn large_incremental_sse_cpu_work_yields_to_real_probe_without_finishing_first() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let server = Server::start(Limits::default()).await;
    let done = Arc::new(AtomicBool::new(false));
    let finished = done.clone();
    let (started, ready) = oneshot::channel();
    let parser = tokio::spawn(async move {
        let mut parser = m_gated::stream::SseDecoder::new(&["model"]);
        parser
            .feed(b"data: {\"skip\":\"", |_| Ok(()))
            .await
            .unwrap();
        started.send(()).unwrap();
        for _ in 0..200 {
            parser.feed(&[b'x'; 64 * 1024], |_| Ok(())).await.unwrap();
        }
        parser
            .feed(b"\",\"model\":\"synthetic\"}\n\n", |_| Ok(()))
            .await
            .unwrap();
        parser.finish().unwrap();
        finished.store(true, Ordering::Release);
        assert!(parser.retained_peak() <= 256 * 1024);
    });
    ready.await.unwrap();
    let started = tokio::time::Instant::now();
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{}/livez", server.address))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(
        !done.load(Ordering::Acquire),
        "13MiB skip 작업이 probe를 끝까지 굶기지 않는다"
    );
    println!(
        "S3_COOPERATIVE_PROBE latency_ms={}",
        started.elapsed().as_millis()
    );
    parser.await.unwrap();
    server.shutdown().await;
}
