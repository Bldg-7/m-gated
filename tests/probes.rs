mod support;
use std::{collections::BTreeMap, net::SocketAddr, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::timeout,
};

struct TestServer {
    address: SocketAddr,
    task: JoinHandle<()>,
}

impl TestServer {
    async fn start() -> Self {
        let config = m_gated::config::load(m_gated::config::ConfigInputs {
            yaml: support::YAML,
            file_environment: None,
            environment: &support::environment(),
        })
        .unwrap();
        let app =
            m_gated::bootstrap::build(config, m_gated::bootstrap::Dependencies::unassembled())
                .unwrap();
        app.runtime().start_http().unwrap();
        // 미조립 bootstrap을 성공시킨 시험이 아니라 동일 조립의 HTTP 상태 reducer 시험이다.
        Self::with_router(app.runtime().clone(), app.router()).await
    }

    async fn with_runtime(runtime: std::sync::Arc<m_gated::runtime::Runtime>) -> Self {
        let router = m_gated::http::router(m_gated::http::AppState {
            runtime: runtime.clone(),
        });
        Self::with_router(runtime, router).await
    }
    async fn with_router(
        runtime: std::sync::Arc<m_gated::runtime::Runtime>,
        router: axum::Router,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            m_gated::server::serve(listener, router, runtime, std::future::pending())
                .await
                .unwrap();
        });
        Self { address, task }
    }

    async fn request(&self, method: &str, path: &str) -> WireResponse {
        timeout(Duration::from_secs(3), async {
            let mut stream = TcpStream::connect(self.address).await.unwrap();
            stream
                .write_all(
                    format!(
                        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nX-Gateway-Request-ID: 11111111-1111-4111-8111-111111111111\r\nX-Request-ID: header-sentinel\r\nAuthorization: Bearer synthetic-auth-sentinel\r\nCookie: synthetic-cookie=synthetic-cookie-sentinel\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut bytes = Vec::new();
            stream.take(16 * 1024).read_to_end(&mut bytes).await.unwrap();
            assert!(bytes.len() < 16 * 1024, "고정 응답의 wire 상한을 넘었다");
            let response = WireResponse::parse(bytes);
            let request_id = uuid::Uuid::parse_str(&response.headers["x-gateway-request-id"]).unwrap();
            assert_eq!(request_id.get_version_num(), 4);
            assert_ne!(request_id.to_string(), "11111111-1111-4111-8111-111111111111");
            for sentinel in ["header-sentinel", "synthetic-auth-sentinel", "synthetic-cookie-sentinel", "url-sentinel", "path-sentinel"] {
                assert!(!response.body.contains(sentinel));
                assert!(response.headers.values().all(|value| !value.contains(sentinel)));
            }
            response
        })
        .await
        .expect("loopback HTTP 응답 기한 초과")
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct WireResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: String,
}

impl WireResponse {
    fn parse(bytes: Vec<u8>) -> Self {
        let wire = String::from_utf8(bytes).unwrap();
        let (head, body) = wire.split_once("\r\n\r\n").unwrap();
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let mut headers = BTreeMap::new();
        for line in lines {
            let (name, value) = line.split_once(':').unwrap();
            assert!(
                headers
                    .insert(name.to_ascii_lowercase(), value.trim().to_owned())
                    .is_none(),
                "고정 응답은 중복 헤더를 보내지 않는다"
            );
        }
        Self {
            status,
            headers,
            body: body.to_owned(),
        }
    }
}

#[tokio::test]
async fn livez_independent_readyz_fails_closed() {
    let server = TestServer::start().await;
    let live = server.request("GET", "/livez").await;
    assert_eq!(
        live.status, 200,
        "조립된 Router는 외부 의존성 없이 live200이어야 한다"
    );
    assert_eq!(live.body, "ok");
    let ready = server.request("GET", "/readyz").await;
    assert_eq!(ready.status, 503, "필수 bootstrap 미구현은 Ready가 아니다");
    assert_eq!(ready.body, "not_ready");
    assert_eq!(
        ready
            .headers
            .get("x-gateway-error-code")
            .map(String::as_str),
        Some("not_ready")
    );
    assert_ne!(
        live.headers["x-gateway-request-id"],
        ready.headers["x-gateway-request-id"]
    );
    for response in [live, ready] {
        assert_eq!(response.headers["cache-control"], "no-store");
        let id = uuid::Uuid::parse_str(&response.headers["x-gateway-request-id"]).unwrap();
        assert_eq!(id.get_version_num(), 4);
    }
}

#[tokio::test]
async fn probes_reject_implicit_head_and_options() {
    let server = TestServer::start().await;
    for path in ["/livez", "/readyz"] {
        for method in [
            "HEAD", "OPTIONS", "POST", "PUT", "PATCH", "DELETE", "TRACE", "CONNECT",
        ] {
            let response = server.request(method, path).await;
            assert_eq!(
                response.status, 405,
                "{method} {path}는 등록된 GET이 아니다"
            );
            assert_eq!(response.headers["allow"], "GET");
            assert_eq!(response.headers["cache-control"], "no-store");
            assert_eq!(
                response.headers["x-gateway-error-code"],
                "method_not_allowed"
            );
            assert_eq!(
                response.headers["x-gateway-dispatch-state"],
                "not_dispatched"
            );
            assert!(!response.headers.contains_key("access-control-allow-origin"));
            let request_id =
                uuid::Uuid::parse_str(&response.headers["x-gateway-request-id"]).unwrap();
            assert_eq!(request_id.get_version_num(), 4);
            if method == "HEAD" {
                assert!(
                    response.body.is_empty(),
                    "HEAD 응답은 오류여도 본문을 보내지 않는다"
                );
            } else {
                let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
                assert_eq!(body["error"]["code"], "method_not_allowed");
                assert_eq!(body["error"]["request_id"], request_id.to_string());
                assert_eq!(body["error"]["details"], serde_json::json!({}));
            }
        }
    }
}

#[tokio::test]
async fn unknown_routes_never_become_spa_or_proxy() {
    let server = TestServer::start().await;
    for path in [
        "/unknown-sentinel?token=url-sentinel",
        "/openai/v1/responses",
        "/anthropic/v1/messages",
        "/v1/messages",
        "/deepseek/v1/chat/completions",
        "/bedrock/v1/messages",
        "/gateway/me",
        "/auth/login",
        "/ui/",
        "/ui/assets/missing.js",
        "/ui/%2e%2e/path-sentinel",
        "/livez/",
    ] {
        for method in ["GET", "POST", "HEAD", "OPTIONS"] {
            let response = server.request(method, path).await;
            assert_eq!(response.status, 404, "{method} {path}는 미등록 경로다");
            assert_eq!(
                response
                    .headers
                    .get("x-gateway-error-code")
                    .map(String::as_str),
                Some("route_not_found")
            );
            assert_eq!(response.headers["cache-control"], "no-store");
            assert_eq!(
                response.headers["x-gateway-dispatch-state"],
                "not_dispatched"
            );
            assert!(!response.headers.contains_key("allow"));
            assert!(!response.headers.contains_key("location"));
            assert!(!response.headers.contains_key("access-control-allow-origin"));
            let request_id =
                uuid::Uuid::parse_str(&response.headers["x-gateway-request-id"]).unwrap();
            assert_eq!(request_id.get_version_num(), 4);
            if method == "HEAD" {
                assert!(response.body.is_empty());
            } else {
                assert_eq!(response.headers["content-type"], "application/json");
                let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
                assert_eq!(
                    body,
                    serde_json::json!({"error": {
                        "code": "route_not_found",
                        "message": "등록되지 않은 경로입니다.",
                        "request_id": request_id.to_string(),
                        "details": {}
                    }})
                );
            }
        }
    }
}

#[tokio::test]
async fn recent_health_failures_latch_and_drain_are_visible_on_same_router() {
    use m_gated::config::Limits;
    use m_gated::runtime::{Dependency, RouteClass, Runtime};
    let runtime = Runtime::new(Limits::default()).unwrap();
    runtime.start_http().unwrap();
    let server = TestServer::with_runtime(runtime.clone()).await;
    assert_eq!(server.request("GET", "/readyz").await.status, 503);
    for dependency in [
        Dependency::Database,
        Dependency::Jwks,
        Dependency::Schema,
        Dependency::Ui,
    ] {
        runtime.record_health(dependency, true, tokio::time::Instant::now());
    }
    let ready = server.request("GET", "/readyz").await;
    assert_eq!(ready.status, 200);
    assert_eq!(ready.body, "ready");
    assert!(!ready.headers.contains_key("x-gateway-error-code"));
    for dependency in [
        Dependency::Database,
        Dependency::Jwks,
        Dependency::Schema,
        Dependency::Ui,
    ] {
        runtime.record_health(dependency, false, tokio::time::Instant::now());
        assert_eq!(server.request("GET", "/livez").await.status, 200);
        assert_eq!(server.request("GET", "/readyz").await.status, 503);
        assert!(runtime.try_admit(RouteClass::Inference).is_err());
        runtime.record_health(dependency, true, tokio::time::Instant::now());
        assert_eq!(server.request("GET", "/readyz").await.status, 200);
    }
    // 추론 전체가 점유되어도 프로브는 업무 semaphore를 기다리지 않는다.
    let mut leases = Vec::new();
    for _ in 0..16 {
        let mut lease = runtime.try_admit(RouteClass::Inference).unwrap();
        lease.begin_authentication().unwrap();
        lease.identify(uuid::Uuid::new_v4()).unwrap();
        leases.push(lease);
    }
    assert!(runtime.try_admit(RouteClass::Inference).is_err());
    assert_eq!(server.request("GET", "/livez").await.status, 200);
    assert_eq!(server.request("GET", "/readyz").await.status, 200);
    drop(leases);
    runtime.record_finalization_failure();
    assert_eq!(server.request("GET", "/readyz").await.status, 503);
    assert!(runtime.try_admit(RouteClass::ManagementWrite).is_ok());
    runtime.begin_draining();
    assert_eq!(server.request("GET", "/livez").await.status, 200);
    assert_eq!(server.request("GET", "/readyz").await.status, 503);
    assert!(runtime.try_admit(RouteClass::ManagementWrite).is_err());
}

#[tokio::test]
async fn http_gate_covers_unknown_routes_but_probes_keep_their_own_capacity() {
    let runtime = m_gated::runtime::Runtime::new(m_gated::config::Limits::default()).unwrap();
    runtime.start_http().unwrap();
    let server = TestServer::with_runtime(runtime.clone()).await;
    let permits: Vec<_> = (0..40).map(|_| runtime.try_http().unwrap()).collect();
    let blocked = server.request("GET", "/unsupported").await;
    assert_eq!(
        blocked.status, 503,
        "전체 HTTP gate가 route 오류 작업에도 적용된다"
    );
    assert_eq!(
        blocked.headers["x-gateway-dispatch-state"],
        "not_dispatched"
    );
    assert_eq!(server.request("GET", "/livez").await.status, 200);
    assert_eq!(server.request("GET", "/readyz").await.status, 503);
    // inference 미조립 Router는 같은 문자열 경로도 등록된 native API로 오인하지 않는다.
    let inactive = server.request("POST", "/v1/messages").await;
    assert_eq!(inactive.status, 503);
    let body: serde_json::Value = serde_json::from_str(&inactive.body).unwrap();
    assert_eq!(body["error"]["code"], "not_ready");
    assert!(body.get("type").is_none());
    drop(permits);
    assert_eq!(server.request("GET", "/unsupported").await.status, 404);
}

#[derive(Clone, Copy, Debug)]
enum SaturatedGate {
    PreAuth,
    Authentication,
    Inference,
    Principal,
    Nonstream,
    ManagementRead,
    ManagementWrite,
    Asset,
}
impl SaturatedGate {
    fn acquire(
        self,
        runtime: &std::sync::Arc<m_gated::runtime::Runtime>,
        principal: uuid::Uuid,
    ) -> Result<m_gated::runtime::RequestLease, m_gated::runtime::AdmissionError> {
        use m_gated::runtime::RouteClass;
        let class = match self {
            Self::ManagementRead => RouteClass::ManagementRead,
            Self::ManagementWrite => RouteClass::ManagementWrite,
            Self::Asset => RouteClass::Asset,
            _ => RouteClass::Inference,
        };
        let mut lease = runtime.try_admit(class)?;
        if matches!(
            self,
            Self::Authentication | Self::Inference | Self::Principal | Self::Nonstream
        ) {
            lease.begin_authentication()?;
        }
        if matches!(self, Self::Inference | Self::Principal | Self::Nonstream) {
            lease.identify(principal)?;
        }
        if matches!(self, Self::Nonstream) {
            lease.acquire_nonstream()?;
        }
        Ok(lease)
    }
}
async fn assert_saturated_gate_wire(gate: SaturatedGate, count: usize, status: u16, code: &str) {
    use axum::response::IntoResponse;
    use m_gated::{
        config::Limits,
        runtime::{Dependency, Runtime},
    };
    // auth 상한을 pre-auth보다 줄여 인증조회 자체 포화를 구분한다.
    let runtime = Runtime::new(Limits {
        authentication: 2,
        ..Limits::default()
    })
    .unwrap();
    runtime.start_http().unwrap();
    for dependency in [
        Dependency::Database,
        Dependency::Jwks,
        Dependency::Schema,
        Dependency::Ui,
    ] {
        runtime.record_health(dependency, true, tokio::time::Instant::now());
    }
    let shared_principal = uuid::Uuid::new_v4();
    let leases: Vec<_> = (0..count)
        .map(|_| {
            let principal = if matches!(gate, SaturatedGate::Principal) {
                shared_principal
            } else {
                uuid::Uuid::new_v4()
            };
            gate.acquire(&runtime, principal).unwrap()
        })
        .collect();
    let handler_runtime = runtime.clone();
    // 실제 인증 성공 경로가 아니라 S0 admission→응답 mapping만 관찰하는 test 전용 route다.
    let extra = axum::Router::new().route(
        "/admission-test",
        axum::routing::any(move || {
            let runtime = handler_runtime.clone();
            async move {
                match gate.acquire(&runtime, shared_principal) {
                    Err(error) => m_gated::http::admission_error(error),
                    Ok(lease) => lease.bind_response(axum::http::StatusCode::OK.into_response()),
                }
            }
        }),
    );
    let router = m_gated::http::router(m_gated::http::AppState {
        runtime: runtime.clone(),
    })
    .merge(extra);
    let server = TestServer::with_router(runtime.clone(), router).await;
    let before = runtime.available_inference();
    let response = server.request("GET", "/admission-test").await;
    assert_eq!(response.status, status, "{gate:?} 포화의 실제 HTTP 분류");
    assert_eq!(response.headers["x-gateway-error-code"], code);
    assert_eq!(
        response.headers["x-gateway-dispatch-state"],
        "not_dispatched"
    );
    let retry: u16 = response.headers["retry-after"].parse().unwrap();
    assert!((1..=120).contains(&retry));
    let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(body["error"]["code"], code);
    assert_eq!(
        body["error"]["request_id"],
        response.headers["x-gateway-request-id"]
    );
    assert_eq!(
        runtime.available_inference(),
        before,
        "부분 획득 permit rollback"
    );
    drop(leases);
    assert_eq!(runtime.available_inference(), 16);
    assert_eq!(runtime.active_principals(), 0);
}

#[tokio::test]
async fn pre_auth_saturation_returns_503_on_wire() {
    assert_saturated_gate_wire(
        SaturatedGate::PreAuth,
        4,
        503,
        "auth_dependency_unavailable",
    )
    .await;
}

#[tokio::test]
async fn authentication_saturation_returns_503_on_wire() {
    assert_saturated_gate_wire(
        SaturatedGate::Authentication,
        2,
        503,
        "auth_dependency_unavailable",
    )
    .await;
}

#[tokio::test]
async fn business_concurrency_saturation_remains_429_on_wire() {
    for (gate, count) in [
        (SaturatedGate::Inference, 16),
        (SaturatedGate::Principal, 4),
        (SaturatedGate::Nonstream, 4),
        (SaturatedGate::ManagementRead, 4),
        (SaturatedGate::ManagementWrite, 1),
        (SaturatedGate::Asset, 8),
    ] {
        assert_saturated_gate_wire(gate, count, 429, "concurrency_limited").await;
    }
}
