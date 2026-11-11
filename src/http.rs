use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::any,
};
use uuid::Uuid;

use crate::runtime::{AdmissionError, Lifecycle, Runtime};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub runtime: Arc<Runtime>,
}

pub fn router(state: AppState) -> Router {
    assemble_router(state, Router::new(), Vec::new())
}
pub fn router_with_inference(
    state: AppState,
    gateway: Arc<crate::gateway::InferenceGateway>,
) -> Router {
    let routes = gateway.routes(state.runtime.clone());
    assemble_router(state, routes, gateway.paths())
}
pub fn router_with_components(
    state: AppState,
    gateway: Option<Arc<crate::gateway::InferenceGateway>>,
    mut routes: Router,
) -> Router {
    let paths = if let Some(gateway) = gateway {
        routes = routes.merge(gateway.routes(state.runtime.clone()));
        gateway.paths()
    } else {
        Vec::new()
    };
    assemble_router(state, routes, paths)
}
#[derive(Clone)]
struct HeaderState {
    runtime: Arc<Runtime>,
    native_paths: Vec<&'static str>,
}
fn assemble_router(state: AppState, routes: Router, native_paths: Vec<&'static str>) -> Router {
    // get의 HEAD 자동 성공을 피하고 GET 외 method를 명시적으로 거부한다.
    Router::new()
        .route("/ui", any(crate::dashboard::redirect))
        .route("/livez", any(livez))
        .route("/readyz", any(readyz))
        .fallback(route_not_found)
        .with_state(state.clone())
        .merge(routes)
        .layer(middleware::from_fn_with_state(
            HeaderState {
                runtime: state.runtime,
                native_paths,
            },
            check_headers,
        ))
        .layer(middleware::from_fn(observe_status))
}

async fn observe_status(request: Request, next: Next) -> Response {
    let response = next.run(request).await;
    crate::telemetry::http_status(response.status().as_u16());
    response
}

async fn check_headers(
    State(state): State<HeaderState>,
    mut request: Request,
    next: Next,
) -> Response {
    let limits = state.runtime.limits();
    let mut bytes = 0_usize;
    let mut count = 0;
    for (name, value) in request.headers() {
        count += 1;
        bytes = bytes.saturating_add(name.as_str().len() + value.as_bytes().len() + 4);
    }
    if count > limits.header_count || bytes > limits.header_bytes {
        return route_error(
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            "header_limit",
            "요청 헤더 상한을 초과했습니다.",
        );
    }
    if matches!(request.uri().path(), "/livez" | "/readyz") {
        return next.run(request).await;
    }
    let http = match state.runtime.try_http() {
        Ok(http) => http,
        Err(error) => {
            if state.native_paths.contains(&request.uri().path()) {
                let response = crate::gateway::native_admission(Uuid::new_v4(), error);
                return if request.uri().path().starts_with("/openai/v1/")
                    || request.uri().path().starts_with("/deepseek/v1/")
                {
                    crate::gateway::openai_error(response)
                } else {
                    response
                };
            }
            return admission_error(error);
        }
    };
    request.extensions_mut().insert(http.clone());
    let mut response = next.run(request).await;
    if !response.headers().contains_key("x-gateway-request-id") {
        response.headers_mut().insert(
            "x-gateway-request-id",
            HeaderValue::from_str(&Uuid::new_v4().to_string()).expect("서버 UUID"),
        );
    }
    http.bind_response(response)
}

pub fn admission_error(error: AdmissionError) -> Response {
    let (status, code, message) = match error {
        AdmissionError::Concurrency => (
            StatusCode::TOO_MANY_REQUESTS,
            "concurrency_limited",
            "처리 슬롯이 가득 찼습니다.",
        ),
        AdmissionError::AuthenticationCapacity => (
            StatusCode::SERVICE_UNAVAILABLE,
            "auth_dependency_unavailable",
            "인증 처리 슬롯이 가득 찼습니다.",
        ),
        AdmissionError::Draining => (
            StatusCode::SERVICE_UNAVAILABLE,
            "draining",
            "서버가 종료 중입니다.",
        ),
        AdmissionError::NotReady | AdmissionError::HttpCapacity => (
            StatusCode::SERVICE_UNAVAILABLE,
            "not_ready",
            "필수 의존성이 준비되지 않았습니다.",
        ),
        AdmissionError::InvalidTransition => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "invariant_violation",
            "요청 상태가 올바르지 않습니다.",
        ),
    };
    let mut response = route_error(status, code, message);
    if error != AdmissionError::InvalidTransition {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    response
}

async fn livez(method: Method) -> Response {
    probe(method, StatusCode::OK, "ok")
}

async fn readyz(State(state): State<AppState>, method: Method) -> Response {
    if state.runtime.lifecycle() == Lifecycle::Ready {
        probe(method, StatusCode::OK, "ready")
    } else {
        probe(method, StatusCode::SERVICE_UNAVAILABLE, "not_ready")
    }
}

fn probe(method: Method, status: StatusCode, body: &'static str) -> Response {
    if method != Method::GET {
        return method_not_allowed();
    }
    let mut response = response(status, body, Uuid::new_v4());
    if status == StatusCode::SERVICE_UNAVAILABLE {
        response.headers_mut().insert(
            "x-gateway-error-code",
            HeaderValue::from_static("not_ready"),
        );
    }
    response
}

async fn route_not_found() -> Response {
    route_error(
        StatusCode::NOT_FOUND,
        "route_not_found",
        "등록되지 않은 경로입니다.",
    )
}

pub(crate) fn method_not_allowed() -> Response {
    let mut response = route_error(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "허용되지 않은 HTTP method입니다.",
    );
    response
        .headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static("GET"));
    response
}

pub(crate) fn route_error(
    status: StatusCode,
    code: &'static str,
    message: &'static str,
) -> Response {
    let request_id = Uuid::new_v4();
    let mut response = response(
        status,
        Json(serde_json::json!({"error": {
            "code": code,
            "message": message,
            "request_id": request_id.to_string(),
            "details": {}
        }})),
        request_id,
    );
    response
        .headers_mut()
        .insert("x-gateway-error-code", HeaderValue::from_static(code));
    response.headers_mut().insert(
        "x-gateway-dispatch-state",
        HeaderValue::from_static("not_dispatched"),
    );
    response
}

fn response(status: StatusCode, body: impl IntoResponse, request_id: Uuid) -> Response {
    let mut response = (status, body).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-gateway-request-id",
        HeaderValue::from_str(&request_id.to_string()).expect("서버 UUID는 유효한 헤더다"),
    );
    response
}

pub fn authentication_error(error: crate::auth::AuthError) -> Response {
    use crate::auth::AuthError;
    let (status, code, message) = match error {
        AuthError::InvalidCredentials => (
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "인증 정보가 유효하지 않습니다.",
        ),
        AuthError::AuthSourceForbidden => (
            StatusCode::FORBIDDEN,
            "auth_source_forbidden",
            "허용되지 않은 인증 소스입니다.",
        ),
        AuthError::AccessDenied => (
            StatusCode::FORBIDDEN,
            "access_denied",
            "현재 접근이 허용되지 않습니다.",
        ),
        AuthError::OwnerInactive => (
            StatusCode::FORBIDDEN,
            "owner_inactive",
            "현재 서비스 소유자가 활성 상태가 아닙니다.",
        ),
        AuthError::DependencyUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "auth_dependency_unavailable",
            "인증 의존성이 준비되지 않았습니다.",
        ),
        AuthError::DatabaseUnavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "db_unavailable",
            "데이터베이스가 준비되지 않았습니다.",
        ),
    };
    let mut response = route_error(status, code, message);
    if status == StatusCode::SERVICE_UNAVAILABLE {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    response
}
