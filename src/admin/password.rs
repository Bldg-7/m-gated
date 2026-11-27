//! 이메일/비밀번호 로그인의 HTTP 경로(결정 0011). 비밀번호 방식에서만 등록한다.
use super::{
    ApiState,
    guard::{Actor, CommandId, Params},
    management::{
        acknowledged, command_context, error, failure, identity_encoding, read_body,
        unsupported_media_type,
    },
};
use crate::{
    auth::{
        AuthError, VerifiedIdentity,
        password::{self, LinkKind},
    },
    ledger::{access::LinkStart, management::ManagementError},
    runtime::{HttpAdmission, RouteClass},
};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    response::Response,
};
use serde_json::{Value, json};
use uuid::Uuid;

/// 인증 전 경로의 처리 자리. 관리 쓰기 자리를 잡고, 인증 작업 한도 안에서 `work`를 실행한다.
/// 비밀번호 해시 검사도 이 한도에 들어간다(결정 0011 규칙 7).
async fn admitted<F, Fut>(state: &ApiState, request: Request, work: F) -> Response
where
    F: FnOnce(Request) -> Fut,
    Fut: std::future::Future<Output = Response>,
{
    let Some(http) = request.extensions().get::<HttpAdmission>().cloned() else {
        return error(ManagementError::Invariant);
    };
    let mut lease = match state
        .runtime
        .try_admit_with_http(RouteClass::ManagementWrite, http)
    {
        Ok(lease) => lease,
        Err(e) => return crate::http::admission_error(e),
    };
    if let Err(e) = lease.begin_authentication() {
        return crate::http::admission_error(e);
    }
    let timeout = std::time::Duration::from_millis(state.runtime.limits().management_timeout_ms);
    let response = tokio::time::timeout(timeout, work(request))
        .await
        .unwrap_or_else(|_| error(ManagementError::Database));
    lease.bind_response(response)
}

/// JSON 본문을 읽는다. 인코딩·형식·크기가 틀리면 관리 API와 같은 오류로 답한다.
async fn json_body<T: serde::de::DeserializeOwned>(
    state: &ApiState,
    request: Request,
) -> Result<T, Response> {
    let (head, body) = request.into_parts();
    if head.uri.query().is_some() {
        return Err(error(ManagementError::Invalid));
    }
    if !identity_encoding(&head.headers) || !crate::http::json_content_type(&head.headers) {
        return Err(unsupported_media_type());
    }
    let bytes = read_body(state, body).await?;
    serde_json::from_slice(&bytes).map_err(|_| error(ManagementError::Invalid))
}

/// 세션 쿠키. `HttpOnly`, `Secure`, `SameSite=Lax`, 12시간.
fn session_cookie(token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{}={token}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={}",
        password::SESSION_COOKIE,
        password::SESSION_SECONDS
    ))
    .expect("세션 토큰은 base64url")
}
fn cleared_cookie() -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0",
        password::SESSION_COOKIE
    ))
    .expect("고정 쿠키")
}
fn no_content(cookie: HeaderValue) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    response.headers_mut().insert(header::SET_COOKIE, cookie);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// 세션을 만든다. 정지된 계정이나 접근 목록에서 빠진 사람이면 만들지 않는다.
async fn start_session(state: &ApiState, email: &str, display_name: &str) -> Response {
    let identity = VerifiedIdentity::for_password(
        state.api.origin.clone(),
        email.to_owned(),
        display_name.to_owned(),
    );
    let store = state.api.auth.store();
    if let Err(e) = store.current_human(&identity).await {
        return crate::http::authentication_error(e);
    }
    let Ok(token) = password::session_token() else {
        return error(ManagementError::Invariant);
    };
    match store
        .start_session(
            email,
            &password::token_hash(&token),
            password::SESSION_SECONDS,
        )
        .await
    {
        Ok(()) => no_content(session_cookie(&token)),
        Err(e) => crate::http::authentication_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SignIn {
    email: String,
    password: String,
}
/// `POST /auth/password`. 틀린 비밀번호와 없는 이메일은 같은 답을 같은 시간에 받는다.
pub(super) async fn sign_in(State(state): State<ApiState>, request: Request) -> Response {
    admitted(&state, request, |request| async {
        let input: SignIn = match json_body(&state, request).await {
            Ok(v) => v,
            Err(r) => return r,
        };
        // 모양이 틀린 이메일도 없는 이메일과 같게 다룬다.
        let email = crate::config::email_value(&input.email)
            .unwrap_or_else(|_| input.email.trim().to_lowercase());
        if email.is_empty() || email.len() > 320 || input.password.len() > 4 * 128 {
            return failure(StatusCode::UNAUTHORIZED, "invalid_credentials");
        }
        let store = state.api.auth.store();
        let record = match store.sign_in_record(&email).await {
            Ok(r) => r,
            Err(e) => return crate::http::authentication_error(e),
        };
        if let Some(seconds) = record.locked_seconds {
            let mut response = failure(StatusCode::TOO_MANY_REQUESTS, "sign_in_locked");
            let wait = seconds.max(1);
            response.headers_mut().insert(
                header::RETRY_AFTER,
                HeaderValue::from_str(&wait.to_string()).expect("숫자"),
            );
            return response;
        }
        // 없는 이메일도 같은 시간이 걸리도록 고정된 가짜 해시와 비교한다.
        let known = record.hash.is_some();
        let hash = match record.hash {
            Some(h) => h,
            None => state.api.dummy_hash().await,
        };
        let matched = password::verify_password(input.password, hash)
            .await
            .unwrap_or(false);
        if !(known && matched) {
            if let Err(e) = store.record_sign_in_failure(&email).await {
                return crate::http::authentication_error(e);
            }
            return failure(StatusCode::UNAUTHORIZED, "invalid_credentials");
        }
        let name = match store.session_name(&email).await {
            Ok(Some(n)) => n,
            Ok(None) => return failure(StatusCode::UNAUTHORIZED, "invalid_credentials"),
            Err(e) => return crate::http::authentication_error(e),
        };
        start_session(&state, &email, &name).await
    })
    .await
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkInput {
    token: String,
}
/// `POST /auth/password/link`. 비밀번호 설정 화면이 링크의 이메일과 종류를 묻는다.
pub(super) async fn link_info(State(state): State<ApiState>, request: Request) -> Response {
    admitted(&state, request, |request| async {
        let input: LinkInput = match json_body(&state, request).await {
            Ok(v) => v,
            Err(r) => return r,
        };
        if !password::plausible_token(&input.token) || password::is_session_token(&input.token) {
            return failure(StatusCode::NOT_FOUND, "link_invalid");
        }
        match state
            .api
            .auth
            .store()
            .link(&password::token_hash(&input.token))
            .await
        {
            Ok(Some(link)) => super::success(json!({
                "email": link.email,
                "kind": link.kind.name(),
                "password_set": link.password_set,
                "expires_at": time::OffsetDateTime::from_unix_timestamp(link.expires_at)
                    .ok()
                    .and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()),
            })),
            Ok(None) => failure(StatusCode::NOT_FOUND, "link_invalid"),
            Err(e) => crate::http::authentication_error(e),
        }
    })
    .await
}

/// 표시 이름 규칙. 계정의 이름 규칙과 같다.
fn display_name_allowed(name: &str) -> bool {
    let n = name.chars().count();
    (1..=100).contains(&n) && name.len() <= 400 && !name.chars().any(char::is_control)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SetPassword {
    token: String,
    password: String,
    #[serde(default)]
    display_name: Option<String>,
}
/// `POST /auth/password/set`. 링크로 비밀번호를 정하고 바로 로그인한다. 그 사람의 링크와
/// 세션은 모두 끝난다.
pub(super) async fn set_password(State(state): State<ApiState>, request: Request) -> Response {
    admitted(&state, request, |request| async {
        let input: SetPassword = match json_body(&state, request).await {
            Ok(v) => v,
            Err(r) => return r,
        };
        if !password::plausible_token(&input.token) || password::is_session_token(&input.token) {
            return failure(StatusCode::NOT_FOUND, "link_invalid");
        }
        if !password::password_allowed(&input.password) {
            return failure(StatusCode::BAD_REQUEST, "password_invalid");
        }
        let name = input.display_name.as_deref().map(str::trim);
        if name.is_some_and(|n| !display_name_allowed(n)) {
            return error(ManagementError::Invalid);
        }
        let store = state.api.auth.store();
        let token_hash = password::token_hash(&input.token);
        // 처음 정하는 사람은 표시 이름이 있어야 한다.
        match store.link(&token_hash).await {
            Ok(Some(link)) if !link.password_set && name.is_none() => {
                return error(ManagementError::Invalid);
            }
            Ok(Some(_)) => {}
            Ok(None) => return failure(StatusCode::NOT_FOUND, "link_invalid"),
            Err(e) => return crate::http::authentication_error(e),
        }
        let hash = match password::hash_password(input.password).await {
            Ok(h) => h,
            Err(e) => return crate::http::authentication_error(e),
        };
        let email = match store.set_password_with_link(&token_hash, &hash, name).await {
            Ok(Some(email)) => email,
            Ok(None) => return failure(StatusCode::NOT_FOUND, "link_invalid"),
            Err(AuthError::InvalidCredentials) => return error(ManagementError::Invalid),
            Err(e) => return crate::http::authentication_error(e),
        };
        let display = match store.session_name(&email).await {
            Ok(Some(n)) => n,
            _ => return error(ManagementError::Invariant),
        };
        start_session(&state, &email, &display).await
    })
    .await
}

/// `POST /auth/logout`. 이 세션을 끝내고 쿠키를 지운다. 세션이 없어도 같은 답이다.
pub(super) async fn sign_out(State(state): State<ApiState>, request: Request) -> Response {
    let state = &state;
    admitted(state, request, |request| async move {
        if let Ok(credential) = state
            .api
            .auth
            .credential(request.headers(), crate::auth::AuthRoute::CookieOnly)
            && password::is_session_token(&credential.token)
            && let Err(e) = state
                .api
                .auth
                .store()
                .end_session(&password::token_hash(&credential.token))
                .await
        {
            return crate::http::authentication_error(e);
        }
        no_content(cleared_cookie())
    })
    .await
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangePassword {
    current_password: String,
    new_password: String,
}
/// `POST /api/me/password`. 자기 비밀번호를 바꾼다. 그 사람의 세션은 모두 끝나므로 다시 로그인한다.
pub(super) async fn change_password(
    State(state): State<ApiState>,
    actor: Actor,
    request: Request,
) -> Response {
    let input: ChangePassword = match json_body(&state, request).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    if !password::password_allowed(&input.new_password) {
        return failure(StatusCode::BAD_REQUEST, "password_invalid");
    }
    let email = actor.human.identity.email().to_owned();
    let store = state.api.auth.store();
    let current = match store.password_hash(&email).await {
        Ok(Some(h)) => h,
        Ok(None) => return error(ManagementError::Forbidden),
        Err(e) => return crate::http::authentication_error(e),
    };
    if !password::verify_password(input.current_password, current)
        .await
        .unwrap_or(false)
    {
        return failure(StatusCode::BAD_REQUEST, "current_password_invalid");
    }
    let hash = match password::hash_password(input.new_password).await {
        Ok(h) => h,
        Err(e) => return crate::http::authentication_error(e),
    };
    match store.change_password(&email, &hash).await {
        Ok(()) => no_content(cleared_cookie()),
        Err(e) => crate::http::authentication_error(e),
    }
}

/// 링크를 담은 메일을 보낸다. 실패하면 저장한 링크를 지운다(결정 0011 규칙 6).
async fn deliver(state: &ApiState, email: &str, kind: LinkKind, token: &str) -> bool {
    let Some(mailer) = &state.api.mailer else {
        return false;
    };
    let mail = password::link_mail(kind, email, &state.api.origin, token);
    if mailer.send(mail).await.is_ok() {
        return true;
    }
    let _ = state
        .api
        .auth
        .store()
        .delete_link(&password::token_hash(token))
        .await;
    false
}

/// `POST /api/admin/access/{id}/link`. 비밀번호가 없는 사람에게는 초대, 있는 사람에게는 재설정
/// 링크를 메일로 보낸다. 메일을 보내지 못하면 `502 mail_failed`이고, 같은 명령 ID로 다시 보낼 수 있다.
pub(super) async fn send_link(
    State(state): State<ApiState>,
    actor: Actor,
    params: Params,
    CommandId(command_id): CommandId,
    request: Request,
) -> Response {
    let Some(command_id) = command_id else {
        return error(ManagementError::Invalid);
    };
    let (head, body) = request.into_parts();
    if head.uri.query().is_some() || !identity_encoding(&head.headers) {
        return error(ManagementError::Invalid);
    }
    match read_body(&state, body).await {
        Ok(bytes) if bytes.is_empty() => {}
        Ok(_) => return error(ManagementError::Invalid),
        Err(r) => return r,
    }
    let Some(id) = params.uuid("id") else {
        return error(ManagementError::Invalid);
    };
    let grants = state.api.grants();
    let path = format!("/api/admin/access/{id}/link");
    let ctx = command_context(
        &state,
        &actor,
        command_id,
        &head.method,
        &path,
        &grants,
        false,
    );
    let Ok(token) = password::link_token() else {
        return error(ManagementError::Invariant);
    };
    let store = &state.api.store;
    let (email, kind) = match store
        .start_access_link(&ctx, id, &password::token_hash(&token))
        .await
    {
        Ok(LinkStart::Replayed(result)) => return acknowledged(result, command_id),
        Ok(LinkStart::Ready { email, kind }) => (email, kind),
        Err(e) => return error(e),
    };
    if !deliver(&state, &email, kind, &token).await {
        return failure(StatusCode::BAD_GATEWAY, "mail_failed");
    }
    match store.finish_access_link(&ctx, id, &email, kind).await {
        Ok(result) => acknowledged(result, command_id),
        Err(e) => error(e),
    }
}

/// 비밀번호 방식에서 사람을 접근 목록에 더한 뒤 초대 메일을 보낸다. 결과를 ack에 `invitation`으로 적는다.
pub(super) async fn invite_added(state: &ApiState, actor: Uuid, ack: &mut Value) {
    let Some(email) = ack["access"]["email"].as_str().map(str::to_owned) else {
        return;
    };
    let Ok(token) = password::link_token() else {
        ack["invitation"] = json!("failed");
        return;
    };
    let stored = state
        .api
        .auth
        .store()
        .create_link(
            &email,
            LinkKind::Invitation,
            &password::token_hash(&token),
            Some(actor),
        )
        .await
        .is_ok();
    let sent = stored && deliver(state, &email, LinkKind::Invitation, &token).await;
    ack["invitation"] = json!(if sent { "sent" } else { "failed" });
}
