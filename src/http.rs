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

/// `content-type`이 하나뿐이고 `application/json`(선택 `charset=utf-8`)인지.
pub fn json_content_type(headers: &axum::http::HeaderMap) -> bool {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let Some(v) = values.next().and_then(|v| v.to_str().ok()) else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    let mut parts = v.split(';');
    if !parts
        .next()
        .is_some_and(|s| s.trim().eq_ignore_ascii_case("application/json"))
    {
        return false;
    }
    let parameters: Vec<_> = parts.collect();
    parameters.is_empty()
        || (parameters.len() == 1 && parameters[0].trim().eq_ignore_ascii_case("charset=utf-8"))
}
/// 요청 본문의 `Content-Encoding`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyEncoding {
    /// 헤더가 없거나 모두 `identity`다.
    Identity,
    /// `zstd` 한 겹.
    Zstd,
    /// 그 밖의 인코딩, 또는 두 겹 이상.
    Other,
}
pub fn body_encoding(headers: &axum::http::HeaderMap) -> BodyEncoding {
    let values: Vec<_> = headers
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .flat_map(|v| v.as_bytes().split(|b| *b == b','))
        .map(|v| v.trim_ascii())
        .filter(|v| !v.eq_ignore_ascii_case(b"identity"))
        .collect();
    match values.as_slice() {
        [] => BodyEncoding::Identity,
        [v] if v.eq_ignore_ascii_case(b"zstd") => BodyEncoding::Zstd,
        _ => BodyEncoding::Other,
    }
}
/// zstd 압축 창의 상한. 이보다 큰 창을 선언한 frame은 메모리를 잡기 전에 거절한다.
pub const ZSTD_MAX_WINDOW_BYTES: u64 = 8 * 1024 * 1024;
/// zstd 한 frame을 푼다. 푼 크기가 `limit`을 넘으면 413, 형식이 틀리거나 frame 뒤에 바이트가 남으면 400이다.
pub async fn decode_zstd(
    body: axum::body::Bytes,
    limit: usize,
) -> Result<axum::body::Bytes, (StatusCode, &'static str)> {
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let invalid = (StatusCode::BAD_REQUEST, "invalid_request");
        let mut source: &[u8] = &body;
        let decoder = ruzstd::decoding::StreamingDecoder::new_with_max_window_size(
            &mut source,
            ZSTD_MAX_WINDOW_BYTES,
        )
        .map_err(|_| invalid)?;
        let mut out = Vec::new();
        decoder
            .take(limit as u64 + 1)
            .read_to_end(&mut out)
            .map_err(|_| invalid)?;
        if out.len() > limit {
            return Err((StatusCode::PAYLOAD_TOO_LARGE, "body_too_large"));
        }
        if !source.is_empty() {
            return Err(invalid);
        }
        Ok(axum::body::Bytes::from(out))
    })
    .await
    .unwrap_or(Err((StatusCode::INTERNAL_SERVER_ERROR, "internal_error")))
}
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
        .route("/", any(crate::dashboard::redirect))
        .route("/dashboard", any(crate::dashboard::redirect))
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
                return crate::gateway::project_error(request.uri().path(), response);
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
        AuthError::VirtualKeyRequired => (
            StatusCode::FORBIDDEN,
            "virtual_key_required",
            "추론 요청은 가상 키로만 보낼 수 있습니다.",
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

#[cfg(test)]
mod encoding_tests {
    use super::*;
    use axum::body::Bytes;
    use axum::http::{HeaderMap, HeaderValue};
    fn zstd(data: &[u8]) -> Bytes {
        Bytes::from(ruzstd::encoding::compress_to_vec(
            data,
            ruzstd::encoding::CompressionLevel::Fastest,
        ))
    }
    #[test]
    fn only_one_layer_of_zstd_is_recognized() {
        let encoding = |values: &[&'static str]| {
            let mut h = HeaderMap::new();
            for v in values {
                h.append(header::CONTENT_ENCODING, HeaderValue::from_static(v));
            }
            body_encoding(&h)
        };
        assert_eq!(encoding(&[]), BodyEncoding::Identity);
        assert_eq!(encoding(&["identity"]), BodyEncoding::Identity);
        assert_eq!(encoding(&["zstd"]), BodyEncoding::Zstd);
        assert_eq!(encoding(&["ZSTD"]), BodyEncoding::Zstd);
        assert_eq!(encoding(&["identity, zstd"]), BodyEncoding::Zstd);
        for other in [
            &["gzip"][..],
            &["zstd, zstd"],
            &["zstd", "zstd"],
            &["zstd", "gzip"],
            &["br"],
        ] {
            assert_eq!(encoding(other), BodyEncoding::Other, "{other:?}");
        }
    }
    #[tokio::test]
    async fn zstd_bodies_are_decoded_within_the_limit() {
        let data =
            br#"{"model":"jev-latest","state":"x","questions":{"q":{"type":"noul"}}}"#.repeat(100);
        assert_eq!(decode_zstd(zstd(&data), data.len()).await.unwrap(), data);
        // 푼 크기가 상한을 넘으면 압축된 크기와 무관하게 413이다.
        let big = vec![b'a'; 1024 * 1024];
        let compressed = zstd(&big);
        assert!(compressed.len() < 64 * 1024);
        assert_eq!(
            decode_zstd(compressed, big.len() - 1).await.err(),
            Some((StatusCode::PAYLOAD_TOO_LARGE, "body_too_large"))
        );
    }
    #[tokio::test]
    async fn malformed_frames_and_trailing_bytes_are_rejected() {
        let invalid = Some((StatusCode::BAD_REQUEST, "invalid_request"));
        assert_eq!(
            decode_zstd(Bytes::from_static(b"{}"), 1024).await.err(),
            invalid
        );
        let mut two = zstd(b"{}").to_vec();
        two.extend_from_slice(&zstd(b"{}"));
        assert_eq!(decode_zstd(Bytes::from(two), 1024).await.err(), invalid);
        let mut cut = zstd(&vec![b'a'; 4096]).to_vec();
        cut.truncate(cut.len() - 1);
        assert_eq!(decode_zstd(Bytes::from(cut), 8192).await.err(), invalid);
        // 16 MiB 창을 선언한 frame은 메모리를 잡기 전에 거절한다.
        let huge_window = Bytes::from_static(&[0x28, 0xb5, 0x2f, 0xfd, 0x00, 14 << 3]);
        assert_eq!(decode_zstd(huge_window, 1024).await.err(), invalid);
    }
}
