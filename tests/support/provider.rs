use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use std::sync::Arc;
use tokio::{sync::Mutex, task::JoinHandle};
#[derive(Clone)]
pub struct Captured {
    pub method: String,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}
pub struct Provider {
    pub calls: Mutex<Vec<Captured>>,
    input_hardcap: Mutex<Option<usize>>,
    response_wait: Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    >,
    responses_contract: Mutex<Option<(u64, u64)>>,
    count_wait: Mutex<
        Option<(
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    >,
    count_response: Mutex<Option<(StatusCode, HeaderMap, Bytes)>>,
    response: Mutex<(StatusCode, HeaderMap, Bytes, bool)>,
    streaming: Mutex<Option<StreamingResponse>>,
}
struct StreamingResponse {
    messages_only: bool,
    receiver: tokio::sync::mpsc::Receiver<Bytes>,
    stopped: tokio::sync::oneshot::Sender<()>,
    content_type: &'static str,
}
pub struct Server {
    pub base: url::Url,
    task: JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Provider {
    pub fn new() -> Arc<Self> {
        Arc::new(Self{response_wait:Mutex::new(None),responses_contract:Mutex::new(None),input_hardcap:Mutex::new(None),count_wait:Mutex::new(None),count_response:Mutex::new(None),streaming:Mutex::new(None),calls:Mutex::new(Vec::new()),response:Mutex::new((StatusCode::OK,HeaderMap::from_iter([("content-type".parse().unwrap(),"application/json".parse().unwrap()),("request-id".parse().unwrap(),"provider-synthetic-id".parse().unwrap())]),Bytes::from_static(br#"{"id":"msg_fixture","type":"message","role":"assistant","model":"synthetic-text","content":[{"type":"text","text":"synthetic response"}],"stop_reason":"end_turn","usage":{"input_tokens":100,"output_tokens":20}}"#),false))})
    }
    /// 합성 모델은 입력 bytes 전체(옵션/이미지/tool overhead 포함)를 context token 상한 안으로 강제한다.
    #[allow(dead_code)]
    pub async fn responses_contract(&self, context: u64, output: u64) {
        *self.responses_contract.lock().await = Some((context, output));
    }
    #[allow(dead_code)]
    pub async fn hold_response(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (ready, started) = tokio::sync::oneshot::channel();
        let (release, wait) = tokio::sync::oneshot::channel();
        *self.response_wait.lock().await = Some((ready, wait));
        (started, release)
    }
    pub async fn hardcap(&self, bytes: usize) {
        *self.input_hardcap.lock().await = Some(bytes);
    }
    pub async fn hold_count(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (ready, started) = tokio::sync::oneshot::channel();
        let (release, wait) = tokio::sync::oneshot::channel();
        *self.count_wait.lock().await = Some((ready, wait));
        (started, release)
    }
    pub async fn set_count(&self, status: StatusCode, headers: HeaderMap, body: Bytes) {
        *self.count_response.lock().await = Some((status, headers, body));
    }
    pub async fn set(&self, status: StatusCode, headers: HeaderMap, body: Bytes) {
        *self.response.lock().await = (status, headers, body, false);
    }
    pub async fn set_chunked(&self, status: StatusCode, headers: HeaderMap, body: Bytes) {
        *self.response.lock().await = (status, headers, body, true);
    }
    pub async fn stream(
        &self,
    ) -> (
        tokio::sync::mpsc::Sender<Bytes>,
        tokio::sync::oneshot::Receiver<()>,
    ) {
        self.stream_with_type("text/event-stream").await
    }
    #[allow(dead_code)]
    pub async fn stream_json(
        &self,
    ) -> (
        tokio::sync::mpsc::Sender<Bytes>,
        tokio::sync::oneshot::Receiver<()>,
    ) {
        self.stream_with_type("application/json").await
    }
    async fn stream_with_type(
        &self,
        content_type: &'static str,
    ) -> (
        tokio::sync::mpsc::Sender<Bytes>,
        tokio::sync::oneshot::Receiver<()>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let (stopped, closed) = tokio::sync::oneshot::channel();
        *self.streaming.lock().await = Some(StreamingResponse {
            messages_only: false,
            receiver: rx,
            stopped,
            content_type,
        });
        (tx, closed)
    }
    /// 독립 내부 count의 JSON 응답은 유지하고 부모 Messages만 제어한다.
    #[allow(dead_code)]
    pub async fn stream_messages(
        &self,
    ) -> (
        tokio::sync::mpsc::Sender<Bytes>,
        tokio::sync::oneshot::Receiver<()>,
    ) {
        let result = self.stream().await;
        self.streaming.lock().await.as_mut().unwrap().messages_only = true;
        result
    }
    pub async fn serve(self: &Arc<Self>) -> Server {
        async fn handler(State(p): State<Arc<Provider>>, r: Request) -> Response {
            let (parts, body) = r.into_parts();
            let count = parts.uri.path().ends_with("/count_tokens");
            let body = axum::body::to_bytes(body, 8 * 1024 * 1024).await.unwrap();
            let input_bytes = body.len();
            let contract_valid = if let Some((context, output)) = *p.responses_contract.lock().await
            {
                let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
                input_bytes as u64 <= context
                    && v["max_output_tokens"]
                        .as_u64()
                        .is_some_and(|n| n > 0 && n <= output)
            } else {
                true
            };
            p.calls.lock().await.push(Captured {
                method: parts.method.to_string(),
                path: parts.uri.path().to_owned(),
                headers: parts.headers,
                body,
            });
            if let Some((ready, wait)) = p.response_wait.lock().await.take() {
                let _ = ready.send(());
                let _ = wait.await;
            }
            if !contract_valid {
                return Response::builder().status(400).header("content-type","application/json").body(Body::from(r#"{"error":{"message":"Synthetic native context contract exceeded","type":"invalid_request_error","param":null,"code":"context_length_exceeded"}}"#)).unwrap();
            }
            if count && let Some((ready, wait)) = p.count_wait.lock().await.take() {
                let _ = ready.send(());
                let _ = wait.await;
            }
            if p.input_hardcap
                .lock()
                .await
                .is_some_and(|max| input_bytes > max)
            {
                return Response::builder().status(400).header("content-type","application/json").body(Body::from(r#"{"type":"error","error":{"type":"invalid_request_error","message":"Fixture context bound exceeded"}}"#)).unwrap();
            }
            let (mut status, mut headers, mut body, chunked) = p.response.lock().await.clone();
            if count && let Some((s, h, b)) = p.count_response.lock().await.clone() {
                status = s;
                headers = h;
                body = b;
            }
            let streaming = {
                let mut pending = p.streaming.lock().await;
                if count && pending.as_ref().is_some_and(|s| s.messages_only) {
                    None
                } else {
                    pending.take()
                }
            };
            if let Some(StreamingResponse {
                receiver,
                stopped,
                content_type,
                ..
            }) = streaming
            {
                return Response::builder()
                    .header("content-type", content_type)
                    .body(Body::new(ControlledStream {
                        receiver,
                        stopped: Some(stopped),
                    }))
                    .unwrap();
            }
            let mut r = Response::new(if chunked {
                Body::new(Chunked(body))
            } else {
                Body::from(body)
            });
            *r.status_mut() = status;
            *r.headers_mut() = headers;
            r
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().fallback(handler).with_state(self.clone());
        Server {
            base: format!("http://{address}/").parse().unwrap(),
            task: tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }),
        }
    }
}

impl Provider {
    /// POST 본문을 실제로 수신한 뒤 응답 헤더 없이 연결을 끊는다.
    pub async fn serve_reset(self: &Arc<Self>) -> Server {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let provider = self.clone();
        let task = tokio::spawn(async move {
            let mut jobs = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted=listener.accept()=>{let (mut socket,_)=accepted.unwrap();if jobs.len()>=8{drop(socket);continue;}let provider=provider.clone();jobs.spawn(async move {
                        let job=async {
                            let mut bytes=Vec::new();let mut chunk=[0u8;4096];
                            let head=loop{let n=socket.read(&mut chunk).await.unwrap();if n==0{return;}bytes.extend_from_slice(&chunk[..n]);if let Some(i)=bytes.windows(4).position(|b|b==b"\r\n\r\n"){break i+4;}assert!(bytes.len()<=32768);};
                            let mut raw_headers=[httparse::EMPTY_HEADER;64];let mut request=httparse::Request::new(&mut raw_headers);request.parse(&bytes[..head]).unwrap();let method=request.method.unwrap().to_owned();let path=request.path.unwrap().to_owned();let mut headers=HeaderMap::new();for h in request.headers.iter(){headers.append(axum::http::HeaderName::from_bytes(h.name.as_bytes()).unwrap(),axum::http::HeaderValue::from_bytes(h.value).unwrap());}
                            let length=headers["content-length"].to_str().unwrap().parse::<usize>().unwrap();assert!(length<=8*1024*1024);
                            while bytes.len()-head<length{let n=socket.read(&mut chunk).await.unwrap();if n==0{return;}bytes.extend_from_slice(&chunk[..n]);assert!(bytes.len()<=head+8*1024*1024);}
                            provider.calls.lock().await.push(Captured{method,path,headers,body:Bytes::copy_from_slice(&bytes[head..head+length])});let _=socket.shutdown().await;
                        };
                        let _=tokio::time::timeout(std::time::Duration::from_secs(3),job).await;
                    });},
                    _=jobs.join_next(),if !jobs.is_empty()=>{},
                }
            }
        });
        Server {
            base: format!("http://{address}/").parse().unwrap(),
            task,
        }
    }
}

// size_hint 없이 여러 frame을 보내 실제 HTTP/1 chunked 누계 검사를 거친다.
struct Chunked(Bytes);
impl http_body::Body for Chunked {
    type Data = Bytes;
    type Error = std::convert::Infallible;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        let n = self.0.len().min(16);
        std::task::Poll::Ready(if n == 0 {
            None
        } else {
            Some(Ok(http_body::Frame::data(self.0.split_to(n))))
        })
    }
}

struct ControlledStream {
    receiver: tokio::sync::mpsc::Receiver<Bytes>,
    stopped: Option<tokio::sync::oneshot::Sender<()>>,
}
impl Drop for ControlledStream {
    fn drop(&mut self) {
        if let Some(tx) = self.stopped.take() {
            let _ = tx.send(());
        }
    }
}
impl http_body::Body for ControlledStream {
    type Data = Bytes;
    type Error = std::convert::Infallible;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        self.receiver
            .poll_recv(cx)
            .map(|chunk| chunk.map(|bytes| Ok(http_body::Frame::data(bytes))))
    }
}
