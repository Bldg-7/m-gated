//! 관리 API의 경로 등록과 공통 검사(결정 0010).
//!
//! 메서드가 틀린 요청은 공통 검사보다 먼저 405로 답한다. 그래서 공통 검사는
//! `MethodRouter::route_layer`로 허용 메서드의 처리 함수만 감싸고, `HEAD`와 나머지 메서드는
//! 그 뒤에 더한 처리 함수가 우리 `Allow` 헤더로 거절한다.
use super::{ApiState, management::error, query_error, redirect};
use crate::{
    auth::{AuthError, AuthRoute, AuthenticatedHuman, Credential, TokenScope, csrf},
    config::ProviderId,
    ledger::{dashboard::QueryError, management::ManagementError},
    runtime::{HttpAdmission, RequestLease, RouteClass},
};
use axum::{
    extract::{FromRequestParts, RawPathParams, Request, State},
    http::{HeaderValue, Method, StatusCode, header, request::Parts},
    middleware::{self, Next},
    response::Response,
    routing::MethodRouter,
};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

/// 경로가 공통 검사에 주는 선택.
#[derive(Clone, Copy)]
pub(super) enum Checks {
    /// 대시보드 화면의 조회(`/api/me`, `/api/usage`, `/api/usage/models`). 관리자와 CSRF 검사가 없다.
    Dashboard,
    /// `/auth/login`. 유효한 자격 증명이 없으면 SSO 로그인 화면으로 보낸다.
    Login,
    /// 그 밖의 관리 API.
    Management {
        /// 자격 증명을 찾는 방법. `/api/csrf`만 세션 쿠키만 받는다.
        auth: AuthRoute,
        /// `/api/admin/` 아래 경로. 관리자만 부를 수 있다.
        admin: bool,
        /// `DELETE /api/keys/{id}`에서 키 주인을 CSRF 검사보다 먼저 확인한다.
        key_owner: bool,
        /// 관리 토큰으로 부를 수 있는지(결정 0014 규칙 2).
        tokens: bool,
    },
}
impl Checks {
    pub(super) const USER: Self = Self::Management {
        auth: AuthRoute::Management,
        admin: false,
        key_owner: false,
        tokens: true,
    };
    pub(super) const ADMIN: Self = Self::Management {
        auth: AuthRoute::Management,
        admin: true,
        key_owner: false,
        tokens: true,
    };
    /// 관리 토큰으로 부를 수 없는 관리자 경로. 토큰을 관리하는 경로다.
    pub(super) const ADMIN_NO_TOKEN: Self = Self::Management {
        auth: AuthRoute::Management,
        admin: true,
        key_owner: false,
        tokens: false,
    };
    /// 관리 토큰으로 부를 수 없는 사용자 경로. 브라우저 앞의 사람을 위한 경로다.
    pub(super) const USER_NO_TOKEN: Self = Self::Management {
        auth: AuthRoute::Management,
        admin: false,
        key_owner: false,
        tokens: false,
    };
}

/// 경로 하나의 메서드 라우터를 만든다. `allow`는 405 응답의 `Allow` 헤더 값이다.
pub(super) fn route(
    state: &ApiState,
    checks: Checks,
    allow: &'static str,
    methods: MethodRouter<ApiState>,
) -> MethodRouter<ApiState> {
    let rejected = move || async move {
        let mut response = crate::http::method_not_allowed();
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static(allow));
        response
    };
    methods
        .route_layer(middleware::from_fn_with_state(
            Guard {
                state: state.clone(),
                checks,
            },
            guard,
        ))
        .head(rejected)
        .fallback(rejected)
}

#[derive(Clone)]
struct Guard {
    state: ApiState,
    checks: Checks,
}

/// 공통 검사를 통과한 요청의 주체. 처리 함수는 이것을 추출기로 받는다.
#[derive(Clone)]
pub(super) struct Actor {
    pub human: Arc<AuthenticatedHuman>,
    /// 관리 API에서 읽은 자격 증명. 대시보드 조회에는 없다.
    pub credential: Option<Arc<Credential>>,
    /// `/api/admin/` 아래 경로인지.
    pub admin_route: bool,
}
impl<S: Send + Sync> FromRequestParts<S> for Actor {
    type Rejection = Response;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Response> {
        parts
            .extensions
            .get::<Actor>()
            .cloned()
            .ok_or_else(|| error(ManagementError::Invariant))
    }
}

/// 경로 변수. 해석 결과를 그대로 두고, 처리 함수가 지금과 같은 자리에서 같은 오류로 거절한다.
pub(super) struct Params(BTreeMap<String, String>);
impl Params {
    pub fn uuid(&self, name: &str) -> Option<Uuid> {
        self.0.get(name).and_then(|s| Uuid::parse_str(s).ok())
    }
    pub fn provider(&self) -> Option<ProviderId> {
        self.0
            .get("provider")
            .and_then(|s| ProviderId::from_name(s))
    }
}
impl<S: Send + Sync> FromRequestParts<S> for Params {
    type Rejection = Response;
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Response> {
        Ok(Self(path_params(parts, state).await))
    }
}
async fn path_params<S: Send + Sync>(parts: &mut Parts, state: &S) -> BTreeMap<String, String> {
    RawPathParams::from_request_parts(parts, state)
        .await
        .map(|params| {
            params
                .iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect()
        })
        .unwrap_or_default()
}

/// `x-gateway-command-id` 헤더. 정확히 하나이고 UUID일 때만 값이 있다.
pub(super) struct CommandId(pub Option<Uuid>);
impl<S: Send + Sync> FromRequestParts<S> for CommandId {
    type Rejection = Response;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Response> {
        let values: Vec<_> = parts
            .headers
            .get_all("x-gateway-command-id")
            .iter()
            .collect();
        Ok(Self(if values.len() == 1 {
            values[0]
                .to_str()
                .ok()
                .and_then(|s| Uuid::parse_str(s).ok())
        } else {
            None
        }))
    }
}

async fn guard(State(guard): State<Guard>, request: Request, next: Next) -> Response {
    match guard.checks {
        Checks::Dashboard | Checks::Login => dashboard(guard, request, next).await,
        Checks::Management {
            auth,
            admin,
            key_owner,
            tokens,
        } => {
            let write = request.method() != Method::GET;
            let Some(http) = request.extensions().get::<HttpAdmission>().cloned() else {
                return error(ManagementError::Invariant);
            };
            let mut lease = match guard.state.runtime.try_admit_with_http(
                if write {
                    RouteClass::ManagementWrite
                } else {
                    RouteClass::ManagementRead
                },
                http,
            ) {
                Ok(lease) => lease,
                Err(e) => return crate::http::admission_error(e),
            };
            if let Err(e) = lease.begin_authentication() {
                return crate::http::admission_error(e);
            }
            let result = tokio::time::timeout(
                Duration::from_millis(guard.state.runtime.limits().management_timeout_ms),
                management(
                    &guard.state,
                    request,
                    next,
                    &mut lease,
                    auth,
                    admin,
                    key_owner,
                    tokens,
                ),
            )
            .await;
            lease.bind_response(result.unwrap_or_else(|_| error(ManagementError::Database)))
        }
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// 대시보드 조회와 로그인. 조회 자리를 잡고, 시간 제한 안에서 인증한다.
async fn dashboard(guard: Guard, mut request: Request, next: Next) -> Response {
    let state = &guard.state;
    let Some(http) = request.extensions().get::<HttpAdmission>().cloned() else {
        return crate::http::admission_error(crate::runtime::AdmissionError::InvalidTransition);
    };
    let mut lease = match state
        .runtime
        .try_admit_with_http(RouteClass::ManagementRead, http)
    {
        Ok(lease) => lease,
        Err(error) => return crate::http::admission_error(error),
    };
    let login = matches!(guard.checks, Checks::Login);
    let result = tokio::time::timeout(
        Duration::from_millis(state.runtime.limits().management_timeout_ms),
        async {
            if let Err(error) = lease.begin_authentication() {
                return crate::http::admission_error(error);
            }
            let explicit = request.headers().contains_key("authorization")
                || request.headers().contains_key("x-api-key");
            let human = match state
                .api
                .auth
                .authenticate(
                    request.headers(),
                    if login {
                        AuthRoute::Login
                    } else {
                        AuthRoute::Management
                    },
                    now(),
                )
                .await
            {
                Ok(human) => human,
                Err(crate::auth::AuthError::InvalidCredentials) if login && !explicit => {
                    return redirect(StatusCode::FOUND, &state.api.login);
                }
                Err(error) => return crate::http::authentication_error(error),
            };
            if let Err(error) = lease.identify(human.current.id) {
                return crate::http::admission_error(error);
            }
            request.extensions_mut().insert(Actor {
                human: Arc::new(human),
                credential: None,
                admin_route: false,
            });
            next.run(request).await
        },
    )
    .await;
    lease.bind_response(result.unwrap_or_else(|_| query_error(QueryError::Database)))
}

/// 관리 API의 인증, 관리자 권한, 관리 토큰, 키 주인, CSRF 검사. 이 순서를 바꾸면 응답이 바뀐다.
#[allow(clippy::too_many_arguments)]
async fn management(
    state: &ApiState,
    request: Request,
    next: Next,
    lease: &mut RequestLease,
    auth: AuthRoute,
    admin: bool,
    key_owner: bool,
    tokens: bool,
) -> Response {
    let now = now();
    let (mut parts, body) = request.into_parts();
    let human = match state.api.auth.authenticate(&parts.headers, auth, now).await {
        Ok(h) => h,
        Err(e) => return crate::http::authentication_error(e),
    };
    if let Err(e) = lease.identify(human.current.id) {
        return crate::http::admission_error(e);
    }
    if admin && !human.current.admin {
        return error(ManagementError::Forbidden);
    }
    // 관리 토큰은 막힌 경로를 부를 수 없고, `read` 토큰은 `GET`만 부를 수 있다(결정 0014).
    if let Some(token) = &human.token
        && (!tokens || (token.scope == TokenScope::Read && parts.method != Method::GET))
    {
        return crate::http::authentication_error(AuthError::ManagementTokenNotAllowed);
    }
    if key_owner && parts.method == Method::DELETE {
        let params = path_params(&mut parts, &()).await;
        let Some(id) = params.get("id").and_then(|s| Uuid::parse_str(s).ok()) else {
            return error(ManagementError::Invalid);
        };
        if let Err(e) = state
            .api
            .store
            .management_key_access(human.current.id, id, state.api.auth.settings())
            .await
        {
            return error(e);
        }
    }
    let credential = match state
        .api
        .auth
        .credential(&parts.headers, AuthRoute::Management)
    {
        Ok(c) => c,
        Err(e) => return crate::http::authentication_error(e),
    };
    if parts.method != Method::GET && csrf::required(human.source) {
        let origins: Vec<_> = parts.headers.get_all("origin").iter().collect();
        if origins.len() != 1 || origins[0].as_bytes() != state.api.origin.as_bytes() {
            return super::management::failure(StatusCode::FORBIDDEN, "origin_denied");
        }
        let tokens: Vec<_> = parts.headers.get_all("x-csrf-token").iter().collect();
        if tokens.len() != 1
            || !tokens[0].to_str().is_ok_and(|s| {
                csrf::verify(
                    &state.api.signing_key,
                    s,
                    &credential.token,
                    now,
                    human.identity.expires_at(),
                )
            })
        {
            return super::management::failure(StatusCode::FORBIDDEN, "csrf_invalid");
        }
    }
    parts.extensions.insert(Actor {
        human: Arc::new(human),
        credential: Some(Arc::new(credential)),
        admin_route: admin,
    });
    next.run(Request::from_parts(parts, body)).await
}
