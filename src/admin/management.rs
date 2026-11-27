//! 관리 API의 처리 함수. 경로와 메서드마다 함수가 하나다(결정 0010).
//! 공통 검사(자리, 인증, 관리자, 키 주인, CSRF)는 `guard`가 먼저 한다.
//! 각 함수는 입력을 지금과 같은 순서로 검사한다. 순서를 바꾸면 응답이 바뀐다.
use super::{
    ApiState,
    guard::{Actor, CommandId, Params},
    query_error, success,
};
use crate::{
    auth::csrf,
    config::ProviderId,
    ledger::{
        commands::CommandContext,
        credentials::{BoundAction, CredentialCommand},
        management::{Command, CommandResult, ManagementError},
        management_query::{self, QueryContext},
    },
};
use axum::{
    body::{Body, Bytes, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header, request::Parts},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub(super) fn failure(status: StatusCode, code: &'static str) -> Response {
    crate::http::route_error(status, code, "관리 요청을 처리할 수 없습니다.")
}
fn details(status: StatusCode, code: &'static str, details: Value) -> Response {
    let mut r = failure(status, code);
    let id = r.headers()["x-gateway-request-id"]
        .to_str()
        .unwrap_or_default()
        .to_owned();
    *r.body_mut()=axum::body::Body::from(serde_json::to_vec(&json!({"error":{"code":code,"message":"관리 요청을 처리할 수 없습니다.","request_id":id,"details":details}})).expect("오류 DTO"));
    r
}
pub(super) fn error(e: ManagementError) -> Response {
    use ManagementError::*;
    let (status, code) = match e {
        Invalid => (400, "invalid_request"),
        Forbidden => (403, "access_denied"),
        NotFound => (404, "resource_not_found"),
        Database => (503, "db_unavailable"),
        Busy => (503, "command_busy"),
        Invariant => (500, "invariant_violation"),
        CommandConflict => (409, "command_conflict"),
        Quota => (409, "key_quota_exceeded"),
        LimitExceeded => (422, "limit_exceeds_maximum"),
        PolicyPeriod => (409, "policy_period_conflict"),
        ConfigurationConflict => (409, "configuration_conflict"),
        SecurityFence => (409, "security_fence_conflict"),
        AccessDuplicate => (409, "access_exists"),
        AccessProtected => (409, "access_protected"),
        SettlementConflict => (409, "settlement_conflict"),
        ExecutionNotFinal => (409, "execution_not_final"),
        Version(v) => {
            return details(
                StatusCode::CONFLICT,
                "version_conflict",
                json!({"current_version":v.to_string()}),
            );
        }
        CredentialRejected => (422, "provider_credential_rejected"),
        CredentialUnverified => (502, "provider_credential_unverified"),
        CatalogSourcesUnavailable => (502, "catalog_sources_unavailable"),
        CatalogProposalStale => (409, "catalog_proposal_stale"),
        CredentialLabelTaken => (409, "credential_label_taken"),
        BindingConflict(current) => {
            return details(
                StatusCode::CONFLICT,
                "credential_binding_conflict",
                json!({"current_credential_id":current}),
            );
        }
        AuthorizationConflict(ids) => {
            return details(
                StatusCode::CONFLICT,
                "credential_authorization_conflict",
                json!({"credential_ids":ids}),
            );
        }
        AuthorizationInUse(n) => {
            return details(
                StatusCode::CONFLICT,
                "credential_authorization_in_use",
                json!({"bound_keys":n.to_string()}),
            );
        }
        CredentialNotAuthorized => (403, "provider_credential_not_authorized"),
        CredentialRequired => (400, "provider_credential_required"),
        CredentialDuplicate(id) => {
            return details(
                StatusCode::CONFLICT,
                "credential_duplicate",
                json!({"credential_id":id}),
            );
        }
        CredentialInUse { bound_keys } => {
            return details(
                StatusCode::CONFLICT,
                "credential_in_use",
                json!({"bound_keys":bound_keys.to_string()}),
            );
        }
        SecretUnavailable(id) => {
            return details(
                StatusCode::CONFLICT,
                "key_secret_unavailable",
                json!({"key_id":id}),
            );
        }
        Query(q) => return query_error(q),
    };
    let mut r = failure(StatusCode::from_u16(status).expect("status"), code);
    if status == 503 {
        r.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    r
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
pub(super) fn identity_encoding(headers: &HeaderMap) -> bool {
    !headers
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .any(|v| v.as_bytes() != b"identity")
}
fn typed<T: serde::de::DeserializeOwned>(input: &Value) -> Result<T, ManagementError> {
    serde_json::from_value(input.clone()).map_err(|_| ManagementError::Invalid)
}
pub(super) fn unsupported_media_type() -> Response {
    failure(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type")
}
/// 본문을 크기 상한과 5초 안에 읽는다.
pub(super) async fn read_body(state: &ApiState, body: Body) -> Result<Bytes, Response> {
    match tokio::time::timeout(
        Duration::from_secs(5),
        to_bytes(body, state.runtime.limits().management_body_bytes),
    )
    .await
    {
        Ok(Ok(b)) => Ok(b),
        Ok(Err(_)) => Err(failure(StatusCode::PAYLOAD_TOO_LARGE, "body_too_large")),
        Err(_) => Err(failure(StatusCode::REQUEST_TIMEOUT, "request_body_timeout")),
    }
}
/// 명령을 처리한 응답. 명령 ID와 재전송 여부를 헤더에 담는다.
pub(super) fn acknowledged(result: CommandResult, command_id: Uuid) -> Response {
    let mut r = match result.body {
        Some(v) => success(v),
        None => StatusCode::NO_CONTENT.into_response(),
    };
    *r.status_mut() = StatusCode::from_u16(result.status).expect("command status");
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r.headers_mut().insert(
        "x-gateway-command-id",
        HeaderValue::from_str(&command_id.to_string()).expect("UUID"),
    );
    r.headers_mut().insert(
        "x-gateway-command-replayed",
        HeaderValue::from_static(if result.replayed { "true" } else { "false" }),
    );
    r
}
pub(super) fn command_context<'a>(
    state: &'a ApiState,
    actor: Uuid,
    command_id: Uuid,
    method: &'a Method,
    path: &'a str,
    grants: &'a Value,
    execution_running: bool,
) -> CommandContext<'a> {
    CommandContext {
        actor,
        command_id,
        method: method.as_str(),
        path,
        settings: state.api.auth.settings(),
        grants,
        digest: state.api.digest_bytes,
        ceiling: &state.api.ceiling,
        execution_running,
    }
}

// ---- 조회 ----

/// `GET /api/csrf`. 쿠키로 쓰기 요청을 보낼 때 쓸 CSRF 토큰을 준다.
pub(super) async fn csrf_token(
    State(state): State<ApiState>,
    actor: Actor,
    request: Request,
) -> Response {
    if request.uri().query().is_some() {
        return error(ManagementError::Invalid);
    }
    let Some(credential) = &actor.credential else {
        return error(ManagementError::Invariant);
    };
    match csrf::issue(
        &state.api.signing_key,
        &credential.token,
        now(),
        actor.human.identity.expires_at(),
    ) {
        Ok((token, exp)) => {
            let expires = time::OffsetDateTime::from_unix_timestamp(exp)
                .ok()
                .and_then(|t| {
                    t.format(&time::format_description::well_known::Rfc3339)
                        .ok()
                });
            success(json!({"token":token,"expires_at":expires}))
        }
        Err(e) => crate::http::authentication_error(e),
    }
}

fn query_context<'a>(state: &'a ApiState, actor: &Actor, grants: &'a Value) -> QueryContext<'a> {
    QueryContext {
        actor: actor.human.current.id,
        admin: actor.admin_route,
        settings: state.api.auth.settings(),
        grants,
        digest: &state.api.digest,
        signing_key: &state.api.signing_key,
    }
}
fn respond(result: Result<Value, ManagementError>) -> Response {
    match result {
        Ok(v) => success(v),
        Err(e) => error(e),
    }
}
async fn list(state: &ApiState, actor: &Actor, uri: &Uri, kind: &str) -> Response {
    let grants = state.api.grants();
    let ctx = query_context(state, actor, &grants);
    respond(
        state
            .api
            .store
            .management_list(&ctx, kind, uri.query())
            .await,
    )
}
/// `GET /api/keys`, `GET /api/admin/keys`.
pub(super) async fn list_keys(
    State(state): State<ApiState>,
    actor: Actor,
    request: Request,
) -> Response {
    list(&state, &actor, request.uri(), "keys").await
}
/// `GET /api/admin/principals`.
pub(super) async fn list_principals(
    State(state): State<ApiState>,
    actor: Actor,
    request: Request,
) -> Response {
    list(&state, &actor, request.uri(), "principals").await
}
/// `GET /api/admin/requests`.
pub(super) async fn list_requests(
    State(state): State<ApiState>,
    actor: Actor,
    request: Request,
) -> Response {
    list(&state, &actor, request.uri(), "requests").await
}
/// `GET /api/admin/requests/models`.
pub(super) async fn request_models(
    State(state): State<ApiState>,
    actor: Actor,
    request: Request,
) -> Response {
    if request.uri().query().is_some() {
        return error(ManagementError::Invalid);
    }
    let grants = state.api.grants();
    let ctx = query_context(&state, &actor, &grants);
    respond(state.api.store.management_request_models(&ctx).await)
}
/// `GET /api/admin/usage`.
pub(super) async fn admin_usage(
    State(state): State<ApiState>,
    actor: Actor,
    request: Request,
) -> Response {
    let grants = state.api.grants();
    let ctx = query_context(&state, &actor, &grants);
    respond(
        state
            .api
            .store
            .management_usage(&ctx, request.uri().query(), &state.api.ceiling)
            .await,
    )
}
async fn detail(
    state: &ApiState,
    actor: &Actor,
    params: &Params,
    uri: &Uri,
    kind: &str,
) -> Response {
    if uri.query().is_some() {
        return error(ManagementError::Invalid);
    }
    let Some(id) = params.uuid("id") else {
        return error(ManagementError::Invalid);
    };
    let grants = state.api.grants();
    let ctx = query_context(state, actor, &grants);
    respond(
        state
            .api
            .store
            .management_detail(&ctx, kind, id, &state.api.ceiling)
            .await,
    )
}
/// `GET /api/admin/principals/{id}`.
pub(super) async fn principal_detail(
    State(state): State<ApiState>,
    actor: Actor,
    params: Params,
    request: Request,
) -> Response {
    detail(&state, &actor, &params, request.uri(), "principals").await
}
/// `GET /api/admin/requests/{id}`.
pub(super) async fn request_detail(
    State(state): State<ApiState>,
    actor: Actor,
    params: Params,
    request: Request,
) -> Response {
    detail(&state, &actor, &params, request.uri(), "requests").await
}

// ---- 가상 키, principal, 정책, 정산 명령 ----

/// 장부 명령의 종류. 경로와 메서드가 정한다.
#[derive(Clone, Copy)]
enum Write {
    Issue,
    EditKey,
    Revoke,
    Create,
    Update,
    Policy,
    Release,
    Settle,
}

async fn command(
    state: &ApiState,
    actor: &Actor,
    params: &Params,
    CommandId(command_id): CommandId,
    request: Request,
    write: Write,
) -> Response {
    let Some(command_id) = command_id else {
        return error(ManagementError::Invalid);
    };
    let (head, body) = request.into_parts();
    if !identity_encoding(&head.headers) {
        return unsupported_media_type();
    }
    let body_limit = state.runtime.limits().management_body_bytes;
    if head
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
        .is_some_and(|n| n > body_limit)
    {
        return failure(StatusCode::PAYLOAD_TOO_LARGE, "body_too_large");
    }
    let bytes = match read_body(state, body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let input = if head.method == Method::DELETE {
        if !bytes.is_empty() {
            return error(ManagementError::Invalid);
        }
        Value::Null
    } else {
        if !crate::http::json_content_type(&head.headers) {
            return unsupported_media_type();
        }
        struct Check;
        impl crate::json::Visitor for Check {
            fn value(
                &mut self,
                _: &[String],
                _: crate::json::Kind,
                _: &str,
            ) -> Result<(), crate::json::JsonError> {
                Ok(())
            }
        }
        if crate::json::scan(&bytes, &mut Check).await.is_err() {
            return error(ManagementError::Invalid);
        }
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
            return error(ManagementError::Invalid);
        };
        if !v.is_object() {
            return error(ManagementError::Invalid);
        }
        v
    };
    // nullable가 아닌 선택 필드도 명시 null을 거부하고 누락과 구분한다.
    if [
        "service_principal_id",
        "display_name",
        "management_owner_id",
        "fence_release",
        "execution_stopped_confirmed",
    ]
    .iter()
    .any(|k| input.get(*k) == Some(&Value::Null))
    {
        return error(ManagementError::Invalid);
    }
    if head.method != Method::DELETE && head.uri.query().is_some() {
        return error(ManagementError::Invalid);
    }
    let id = params.uuid("id");
    let parsed = (|| -> Result<Command, ManagementError> {
        let id = || id.ok_or(ManagementError::Invalid);
        Ok(match write {
            Write::Issue => Command::Issue(typed(&input)?),
            Write::EditKey => Command::EditKey(id()?, typed(&input)?),
            Write::Revoke => {
                let id = id()?;
                let mut f = management_query::fields(head.uri.query())?;
                let expected = f
                    .remove("expected_version")
                    .ok_or(ManagementError::Invalid)?;
                if !f.is_empty() {
                    return Err(ManagementError::Invalid);
                }
                Command::Revoke { id, expected }
            }
            Write::Create => Command::Create(typed(&input)?),
            Write::Update => Command::Update(id()?, typed(&input)?),
            Write::Policy => Command::Policy(id()?, typed(&input)?),
            Write::Release => Command::Release(id()?, typed(&input)?),
            Write::Settle => Command::Settle(id()?, typed(&input)?),
        })
    })();
    let command = match parsed {
        Ok(c) => c,
        Err(e) => return error(e),
    };
    // 명령 기록에 남는 경로. 같은 명령이면 클라이언트가 ID를 어떻게 썼든 같은 경로가 된다.
    // 가상 키 수정만 요청의 경로를 그대로 남긴다.
    let id = id.unwrap_or_default();
    let path = match write {
        Write::Issue | Write::Create | Write::EditKey => head.uri.path().to_owned(),
        Write::Revoke => format!("/api/keys/{id}"),
        Write::Update => format!("/api/admin/principals/{id}"),
        Write::Policy => format!("/api/admin/principals/{id}/policy"),
        Write::Release => format!("/api/admin/principals/{id}/inference-pause/release"),
        Write::Settle => format!("/api/admin/requests/{id}/settlement"),
    };
    let mut running = false;
    if let Command::Settle(id, _) = &command {
        let root = match state.api.store.settlement_root(*id).await {
            Ok(root) => root,
            Err(e) => return error(e),
        };
        running = state
            .runtime
            .request_phases()
            .iter()
            .any(|(current, phase)| {
                (*current == *id || Some(*current) == root)
                    && !matches!(
                        phase,
                        crate::runtime::request::RequestPhase::AwaitBodyRelease
                            | crate::runtime::request::RequestPhase::Done
                    )
            });
    }
    let grants = state.api.grants();
    let ctx = command_context(
        state,
        actor.human.current.id,
        command_id,
        &head.method,
        &path,
        &grants,
        running,
    );
    match state.api.ledger.management_command(&ctx, command).await {
        Ok(result) => acknowledged(result, command_id),
        Err(e) => {
            let replayed = matches!(e, ManagementError::SecretUnavailable(_));
            let mut response = error(e);
            if replayed {
                response.headers_mut().insert(
                    "x-gateway-command-id",
                    HeaderValue::from_str(&command_id.to_string()).expect("UUID"),
                );
                response.headers_mut().insert(
                    "x-gateway-command-replayed",
                    HeaderValue::from_static("true"),
                );
            }
            response
        }
    }
}

macro_rules! write_handler {
    ($(#[$doc:meta])* $name:ident, $write:expr) => {
        $(#[$doc])*
        pub(super) async fn $name(
            State(state): State<ApiState>,
            actor: Actor,
            params: Params,
            command_id: CommandId,
            request: Request,
        ) -> Response {
            command(&state, &actor, &params, command_id, request, $write).await
        }
    };
}
write_handler!(
    /// `POST /api/keys`. 가상 키를 발급한다.
    issue_key,
    Write::Issue
);
write_handler!(
    /// `PATCH /api/keys/{id}`. 가상 키의 이름과 한도를 바꾼다.
    edit_key,
    Write::EditKey
);
write_handler!(
    /// `DELETE /api/keys/{id}?expected_version=`. 가상 키를 폐기한다.
    revoke_key,
    Write::Revoke
);
write_handler!(
    /// `POST /api/admin/principals`. 서비스 계정을 만든다.
    create_principal,
    Write::Create
);
write_handler!(
    /// `PATCH /api/admin/principals/{id}`.
    update_principal,
    Write::Update
);
write_handler!(
    /// `PUT /api/admin/principals/{id}/policy`.
    set_policy,
    Write::Policy
);
write_handler!(
    /// `POST /api/admin/principals/{id}/inference-pause/release`.
    release_pause,
    Write::Release
);
write_handler!(
    /// `PUT /api/admin/requests/{id}/settlement`. 미정산 요청을 정산한다.
    settle,
    Write::Settle
);

// ---- Provider 회사 키 ----

/// `GET /api/admin/provider-credentials`.
pub(super) async fn list_credentials(State(state): State<ApiState>, request: Request) -> Response {
    let Some(service) = state.api.credentials.clone() else {
        return error(ManagementError::NotFound);
    };
    if request.uri().query().is_some() {
        return error(ManagementError::Invalid);
    }
    respond(service.list().await)
}

/// Provider 회사 키 명령이 오는 경로.
#[derive(Clone, Copy)]
enum CredentialRoute {
    /// `/api/admin/provider-credentials/{provider}`
    Provider,
    /// `/api/admin/provider-credentials/{provider}/{id}`
    Item,
    /// `/api/admin/keys/{id}/provider-credential`
    KeyBinding,
    /// `/api/admin/principals/{id}/provider-credentials`
    Principal,
}

/// Provider 회사 키 등록·교체·해제와 인가. 원문은 요청 본문에서만 받고 어떤 응답에도 담지 않는다.
/// 경로 변수가 틀리면 404다.
async fn credential_command(
    state: &ApiState,
    actor: &Actor,
    params: &Params,
    CommandId(command_id): CommandId,
    request: Request,
    route: CredentialRoute,
) -> Response {
    let Some(service) = state.api.credentials.clone() else {
        return error(ManagementError::NotFound);
    };
    let not_found = || error(ManagementError::NotFound);
    let principal = match route {
        CredentialRoute::Principal => match params.uuid("id") {
            Some(id) => Some(id),
            None => return not_found(),
        },
        _ => None,
    };
    let (provider, id, key) = match route {
        CredentialRoute::Principal => (None, None, None),
        CredentialRoute::KeyBinding => match params.uuid("id") {
            Some(key) => (None, None, Some(key)),
            None => return not_found(),
        },
        CredentialRoute::Provider => match params.provider() {
            Some(provider) => (Some(provider), None, None),
            None => return not_found(),
        },
        CredentialRoute::Item => match (params.provider(), params.uuid("id")) {
            (Some(provider), Some(id)) => (Some(provider), Some(id), None),
            _ => return not_found(),
        },
    };
    let Some(command_id) = command_id else {
        return error(ManagementError::Invalid);
    };
    let (head, body) = request.into_parts();
    if !identity_encoding(&head.headers) {
        return unsupported_media_type();
    }
    let bytes = match read_body(state, body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let path = head.uri.path();
    let command = if head.method == Method::DELETE {
        let (Some(provider), Some(id)) = (provider, id) else {
            return error(ManagementError::Invalid);
        };
        if !bytes.is_empty() {
            return error(ManagementError::Invalid);
        }
        let mut fields = match management_query::fields(head.uri.query()) {
            Ok(f) => f,
            Err(e) => return error(e),
        };
        let Some(expected) = fields
            .remove("expected_version")
            .and_then(|s| s.parse::<i64>().ok())
        else {
            return error(ManagementError::Invalid);
        };
        let move_to = match fields.remove("move_to").map(|s| Uuid::parse_str(&s)) {
            None => None,
            Some(Ok(id)) => Some(id),
            Some(Err(_)) => return error(ManagementError::Invalid),
        };
        if !fields.is_empty() {
            return error(ManagementError::Invalid);
        }
        CredentialCommand::Retire {
            provider,
            id,
            expected,
            move_to,
        }
    } else {
        if head.uri.query().is_some() {
            return error(ManagementError::Invalid);
        }
        if !crate::http::json_content_type(&head.headers) {
            return unsupported_media_type();
        }
        let Ok(mut input) = serde_json::from_slice::<serde_json::Map<String, Value>>(&bytes) else {
            return error(ManagementError::Invalid);
        };
        let mut take_string = |name: &str| match input.remove(name) {
            Some(Value::String(s)) => Some(s),
            _ => None,
        };
        let version = |s: Option<String>| s.and_then(|s| s.parse::<i64>().ok());
        let ids = |v: Option<Value>| -> Option<Vec<Uuid>> {
            v?.as_array()?
                .iter()
                .map(|x| x.as_str().and_then(|s| Uuid::parse_str(s).ok()))
                .collect()
        };
        if let Some(principal) = principal {
            let provider = take_string("provider").and_then(|p| ProviderId::from_name(&p));
            let credentials = ids(input.remove("credential_ids"));
            let expected = match input.remove("expected") {
                Some(Value::Object(mut e)) => {
                    let r = ids(e.remove("credential_ids"));
                    if e.is_empty() { r } else { None }
                }
                _ => None,
            };
            let bound = match input.remove("bound_keys") {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) if s == "block" => Ok(Some(BoundAction::Block)),
                Some(Value::Object(mut o)) => match (o.remove("move_to"), o.is_empty()) {
                    (Some(Value::String(s)), true) => Uuid::parse_str(&s)
                        .map(|id| Some(BoundAction::Move(id)))
                        .map_err(|_| ()),
                    _ => Err(()),
                },
                Some(_) => Err(()),
            };
            let command = match (provider, credentials, expected, bound) {
                (Some(provider), Some(credentials), Some(expected), Ok(bound))
                    if head.method == Method::PUT && input.is_empty() =>
                {
                    CredentialCommand::Authorize {
                        principal,
                        provider,
                        credentials,
                        expected,
                        bound,
                    }
                }
                _ => return error(ManagementError::Invalid),
            };
            return run_credential_command(state, &head, path, actor, command_id, service, command)
                .await;
        }
        let parsed = match (head.method.as_str(), provider, id, key) {
            ("POST", Some(provider), None, None) => {
                match (take_string("label"), take_string("secret")) {
                    (Some(label), Some(secret)) => Some(CredentialCommand::Add {
                        provider,
                        label,
                        secret: zeroize::Zeroizing::new(secret),
                    }),
                    _ => None,
                }
            }
            // 이 Provider 키를 여러 사용자에게 한 번에 인가한다.
            ("POST", Some(provider), Some(id), None) => {
                ids(input.remove("principal_ids")).map(|principals| CredentialCommand::Grant {
                    provider,
                    id,
                    principals,
                })
            }
            ("PUT", Some(provider), Some(id), None) => {
                match (
                    version(take_string("expected_version")),
                    take_string("secret"),
                ) {
                    (Some(expected), Some(secret)) => Some(CredentialCommand::Replace {
                        provider,
                        id,
                        expected,
                        secret: zeroize::Zeroizing::new(secret),
                    }),
                    _ => None,
                }
            }
            ("PATCH", Some(provider), Some(id), None) => {
                match (
                    version(take_string("expected_version")),
                    take_string("label"),
                ) {
                    (Some(expected), Some(label)) => Some(CredentialCommand::Update {
                        provider,
                        id,
                        expected,
                        label,
                    }),
                    _ => None,
                }
            }
            ("PUT", None, None, Some(key)) => {
                let provider = take_string("provider").and_then(|p| ProviderId::from_name(&p));
                let credential =
                    take_string("credential_id").and_then(|s| Uuid::parse_str(&s).ok());
                let expected = match input.remove("expected_credential_id") {
                    Some(Value::Null) => Ok(None),
                    Some(Value::String(s)) => Uuid::parse_str(&s).map(Some).map_err(|_| ()),
                    _ => Err(()),
                };
                match (provider, credential, expected) {
                    (Some(provider), Some(credential), Ok(expected)) => {
                        Some(CredentialCommand::BindKey {
                            key,
                            provider,
                            credential,
                            expected,
                        })
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        match parsed {
            Some(command) if input.is_empty() => command,
            _ => return error(ManagementError::Invalid),
        }
    };
    run_credential_command(state, &head, path, actor, command_id, service, command).await
}

async fn run_credential_command(
    state: &ApiState,
    head: &Parts,
    path: &str,
    actor: &Actor,
    command_id: Uuid,
    service: std::sync::Arc<crate::credentials::CredentialService>,
    command: CredentialCommand,
) -> Response {
    let grants = state.api.grants();
    let ctx = command_context(
        state,
        actor.human.current.id,
        command_id,
        &head.method,
        path,
        &grants,
        false,
    );
    match service.execute(&ctx, command).await {
        Ok(result) => {
            // 이 프로세스는 즉시, 다른 프로세스는 상태 점검 주기에 맞춰 반영한다.
            let _ = service.refresh().await;
            acknowledged(result, command_id)
        }
        Err(e) => error(e),
    }
}

macro_rules! credential_handler {
    ($(#[$doc:meta])* $name:ident, $route:expr) => {
        $(#[$doc])*
        pub(super) async fn $name(
            State(state): State<ApiState>,
            actor: Actor,
            params: Params,
            command_id: CommandId,
            request: Request,
        ) -> Response {
            credential_command(&state, &actor, &params, command_id, request, $route).await
        }
    };
}
credential_handler!(
    /// `POST /api/admin/provider-credentials/{provider}`. 회사 키를 등록한다.
    add_credential,
    CredentialRoute::Provider
);
credential_handler!(
    /// `POST|PUT|PATCH|DELETE /api/admin/provider-credentials/{provider}/{id}`.
    /// 인가, 원문 교체, 이름 변경, 해제.
    credential_item,
    CredentialRoute::Item
);
credential_handler!(
    /// `PUT /api/admin/keys/{id}/provider-credential`. 가상 키에 회사 키를 연결한다.
    bind_key_credential,
    CredentialRoute::KeyBinding
);
credential_handler!(
    /// `PUT /api/admin/principals/{id}/provider-credentials`. 사용자에게 회사 키를 인가한다.
    authorize_credentials,
    CredentialRoute::Principal
);

// ---- 토큰 대조 ----

/// `GET /api/admin/token-check`. 토큰 대조 화면 값(결정 0005 23~27번).
/// `range`는 24h, 7d, 30d 가운데 하나다(없으면 24h).
pub(super) async fn token_check(
    State(state): State<ApiState>,
    actor: Actor,
    request: Request,
) -> Response {
    let hours = match request.uri().query() {
        None | Some("range=24h") => 24,
        Some("range=7d") => 24 * 7,
        Some("range=30d") => 24 * 30,
        _ => return error(ManagementError::Invalid),
    };
    let grants = state.api.grants();
    let ctx = command_context(
        &state,
        actor.human.current.id,
        Uuid::nil(),
        request.method(),
        request.uri().path(),
        &grants,
        false,
    );
    match state
        .api
        .store
        .token_check_view(&ctx, &state.api.ledger, hours, &state.api.prices())
        .await
    {
        Ok(mut v) => {
            v["enabled"] = json!(state.api.token_check_enabled);
            success(v)
        }
        Err(e) => error(e),
    }
}

// ---- 게이트웨이 설정 ----

/// 게이트웨이 전체 설정(예산 기간의 시간대와 끊긴 요청의 정산 규칙). 조회와 변경 모두 관리자만 한다.
#[derive(Clone, Copy, PartialEq)]
enum Setting {
    View,
    BudgetTimezone,
    CutoffRule,
}

async fn setting(
    state: &ApiState,
    actor: &Actor,
    CommandId(command_id): CommandId,
    request: Request,
    setting: Setting,
) -> Response {
    let (head, body) = request.into_parts();
    if head.uri.query().is_some() {
        return error(ManagementError::Invalid);
    }
    let grants = state.api.grants();
    let ctx = command_context(
        state,
        actor.human.current.id,
        command_id.unwrap_or_else(Uuid::nil),
        &head.method,
        head.uri.path(),
        &grants,
        false,
    );
    if setting == Setting::View {
        return match state.api.store.settings_view(&ctx).await {
            Ok(mut v) => {
                // 사후 보정은 이 서버에 Admin API 키가 있을 때만 켤 수 있다.
                v["cutoff_rule"]["correction_available"] = json!(state.api.token_check_enabled);
                success(v)
            }
            Err(e) => error(e),
        };
    }
    let Some(command_id) = command_id else {
        return error(ManagementError::Invalid);
    };
    if !identity_encoding(&head.headers) || !crate::http::json_content_type(&head.headers) {
        return unsupported_media_type();
    }
    let bytes = match read_body(state, body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SetTimezone {
        timezone: String,
        expected_version: String,
    }
    /// 끊긴 요청의 정산 규칙. 이번 단계에서는 Anthropic 규칙만 바꿀 수 있다.
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SetCutoffRule {
        provider: String,
        cutoff_rule: String,
        /// 사후 보정 스위치. 없으면 지금 값을 그대로 둔다.
        #[serde(default)]
        correction: Option<bool>,
        expected_version: String,
    }
    let result = if setting == Setting::CutoffRule {
        let Ok(input) = serde_json::from_slice::<SetCutoffRule>(&bytes) else {
            return error(ManagementError::Invalid);
        };
        let Ok(expected) = input.expected_version.parse::<i64>() else {
            return error(ManagementError::Invalid);
        };
        // Admin API 키가 없는 서버에서는 사후 보정을 켤 수 없다.
        if input.correction == Some(true) && !state.api.token_check_enabled {
            return error(ManagementError::Invalid);
        }
        state
            .api
            .store
            .set_cutoff_rule(
                &ctx,
                &input.provider,
                &input.cutoff_rule,
                input.correction,
                expected,
            )
            .await
    } else {
        let Ok(input) = serde_json::from_slice::<SetTimezone>(&bytes) else {
            return error(ManagementError::Invalid);
        };
        let Ok(expected) = input.expected_version.parse::<i64>() else {
            return error(ManagementError::Invalid);
        };
        state
            .api
            .store
            .set_budget_timezone(&ctx, &input.timezone, expected)
            .await
    };
    match result {
        Ok(result) => acknowledged(result, command_id),
        Err(e) => error(e),
    }
}

macro_rules! setting_handler {
    ($(#[$doc:meta])* $name:ident, $setting:expr) => {
        $(#[$doc])*
        pub(super) async fn $name(
            State(state): State<ApiState>,
            actor: Actor,
            command_id: CommandId,
            request: Request,
        ) -> Response {
            setting(&state, &actor, command_id, request, $setting).await
        }
    };
}
setting_handler!(
    /// `GET /api/admin/settings`.
    settings_view,
    Setting::View
);
setting_handler!(
    /// `PUT /api/admin/settings/budget-timezone`.
    set_budget_timezone,
    Setting::BudgetTimezone
);
setting_handler!(
    /// `PUT /api/admin/settings/cutoff-rule`.
    set_cutoff_rule,
    Setting::CutoffRule
);

// ---- 접근 목록(결정 0001) ----

#[derive(Clone, Copy, PartialEq)]
enum Access {
    List,
    Add,
    Update,
    Remove,
}

async fn access(
    state: &ApiState,
    actor: &Actor,
    params: &Params,
    CommandId(command_id): CommandId,
    request: Request,
    route: Access,
) -> Response {
    let (head, body) = request.into_parts();
    let id = params.uuid("id");
    let grants = state.api.grants();
    // 명령 기록의 경로는 등록한 경로 모양과 표준 UUID로 만든다(결정 0010 규칙 5).
    let path = match (route, id) {
        (Access::Update | Access::Remove, Some(id)) => format!("/api/admin/access/{id}"),
        _ => head.uri.path().to_owned(),
    };
    let ctx = command_context(
        state,
        actor.human.current.id,
        command_id.unwrap_or_else(Uuid::nil),
        &head.method,
        &path,
        &grants,
        false,
    );
    if route == Access::List {
        if head.uri.query().is_some() {
            return error(ManagementError::Invalid);
        }
        return match state.api.store.access_list(&ctx).await {
            Ok(v) => success(v),
            Err(e) => error(e),
        };
    }
    let Some(command_id) = command_id else {
        return error(ManagementError::Invalid);
    };
    if !identity_encoding(&head.headers) {
        return unsupported_media_type();
    }
    let bytes = match read_body(state, body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Add {
        email: String,
        role: String,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Update {
        role: String,
        expected_version: String,
    }
    use crate::ledger::access::Role;
    let result = match route {
        Access::Remove => {
            if !bytes.is_empty() {
                return error(ManagementError::Invalid);
            }
            let mut fields = match management_query::fields(head.uri.query()) {
                Ok(f) => f,
                Err(e) => return error(e),
            };
            let Some(expected) = fields
                .remove("expected_version")
                .and_then(|s| s.parse::<i64>().ok())
            else {
                return error(ManagementError::Invalid);
            };
            let Some(id) = id.filter(|_| fields.is_empty()) else {
                return error(ManagementError::Invalid);
            };
            state.api.store.remove_access(&ctx, id, expected).await
        }
        _ => {
            if head.uri.query().is_some() {
                return error(ManagementError::Invalid);
            }
            if !crate::http::json_content_type(&head.headers) {
                return unsupported_media_type();
            }
            if route == Access::Add {
                let Ok(input) = serde_json::from_slice::<Add>(&bytes) else {
                    return error(ManagementError::Invalid);
                };
                let Some(role) = Role::parse(&input.role) else {
                    return error(ManagementError::Invalid);
                };
                state.api.store.add_access(&ctx, &input.email, role).await
            } else {
                let Ok(input) = serde_json::from_slice::<Update>(&bytes) else {
                    return error(ManagementError::Invalid);
                };
                let (Some(role), Ok(expected), Some(id)) = (
                    Role::parse(&input.role),
                    input.expected_version.parse::<i64>(),
                    id,
                ) else {
                    return error(ManagementError::Invalid);
                };
                state
                    .api
                    .store
                    .update_access(&ctx, id, role, expected)
                    .await
            }
        }
    };
    match result {
        Ok(mut result) => {
            // 비밀번호 방식에서는 사람을 더하면 초대 메일도 보낸다(결정 0011 규칙 2).
            if route == Access::Add
                && !result.replayed
                && state.api.mode == crate::config::AuthMode::Password
                && let Some(body) = result.body.as_mut()
            {
                super::password::invite_added(state, actor.human.current.id, body).await;
            }
            acknowledged(result, command_id)
        }
        Err(e) => error(e),
    }
}

macro_rules! access_handler {
    ($(#[$doc:meta])* $name:ident, $route:expr) => {
        $(#[$doc])*
        pub(super) async fn $name(
            State(state): State<ApiState>,
            actor: Actor,
            params: Params,
            command_id: CommandId,
            request: Request,
        ) -> Response {
            access(&state, &actor, &params, command_id, request, $route).await
        }
    };
}
access_handler!(
    /// `GET /api/admin/access`.
    access_list,
    Access::List
);
access_handler!(
    /// `POST /api/admin/access`.
    add_access,
    Access::Add
);
access_handler!(
    /// `PATCH /api/admin/access/{id}`.
    update_access,
    Access::Update
);
access_handler!(
    /// `DELETE /api/admin/access/{id}`.
    remove_access,
    Access::Remove
);

// ---- 모델·가격 카탈로그 ----

/// 모델·가격 카탈로그 조회, 공개 가격 목록 동기화(변경안 만들기), 변경안 승인, 단가 고치기(결정 0012).
#[derive(Clone, Copy, PartialEq)]
enum Catalog {
    View,
    Sync,
    Approve,
    Rates,
}

async fn catalog(
    state: &ApiState,
    actor: &Actor,
    CommandId(command_id): CommandId,
    request: Request,
    action: Catalog,
) -> Response {
    let Some(service) = state.api.catalog.clone().filter(|c| c.enabled()) else {
        return error(ManagementError::NotFound);
    };
    let (head, body) = request.into_parts();
    if head.uri.query().is_some() {
        return error(ManagementError::Invalid);
    }
    if action == Catalog::View {
        return respond(service.view().await);
    }
    let Some(command_id) = command_id else {
        return error(ManagementError::Invalid);
    };
    if !identity_encoding(&head.headers) {
        return unsupported_media_type();
    }
    let bytes = match read_body(state, body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let input: Value = if bytes.is_empty() {
        json!({})
    } else if !crate::http::json_content_type(&head.headers) {
        return unsupported_media_type();
    } else {
        match serde_json::from_slice(&bytes) {
            Ok(v @ Value::Object(_)) => v,
            _ => return error(ManagementError::Invalid),
        }
    };
    let grants = state.api.grants();
    let ctx = command_context(
        state,
        actor.human.current.id,
        command_id,
        &head.method,
        head.uri.path(),
        &grants,
        false,
    );
    let result = if action == Catalog::Sync {
        if input.as_object().is_some_and(|o| !o.is_empty()) {
            return error(ManagementError::Invalid);
        }
        service.sync(&ctx).await
    } else if action == Catalog::Rates {
        /// 칸 이름 → 단가(100만 토큰당 USD 문자열). null은 단가를 지운다.
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Rates {
            provider: String,
            model: String,
            rates: std::collections::BTreeMap<String, Option<String>>,
            expected_version: String,
        }
        let Ok(edit) = serde_json::from_value::<Rates>(input) else {
            return error(ManagementError::Invalid);
        };
        let Ok(expected) = edit.expected_version.parse::<i64>() else {
            return error(ManagementError::Invalid);
        };
        service
            .edit_rates(&ctx, &edit.provider, &edit.model, edit.rates, expected)
            .await
    } else {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Item {
            provider: String,
            model: String,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Approve {
            proposal_id: Uuid,
            expected_version: String,
            items: Vec<Item>,
        }
        let Ok(approve) = serde_json::from_value::<Approve>(input) else {
            return error(ManagementError::Invalid);
        };
        let Ok(expected) = approve.expected_version.parse::<i64>() else {
            return error(ManagementError::Invalid);
        };
        let selected: std::collections::BTreeSet<_> = approve
            .items
            .into_iter()
            .map(|i| (i.provider, i.model))
            .collect();
        service
            .approve(&ctx, approve.proposal_id, expected, selected)
            .await
    };
    match result {
        Ok(result) => {
            // 이 프로세스는 즉시, 다른 프로세스는 상태 점검 주기에 맞춰 새 버전을 쓴다.
            let _ = service.refresh().await;
            acknowledged(result, command_id)
        }
        Err(e) => error(e),
    }
}

macro_rules! catalog_handler {
    ($(#[$doc:meta])* $name:ident, $action:expr) => {
        $(#[$doc])*
        pub(super) async fn $name(
            State(state): State<ApiState>,
            actor: Actor,
            command_id: CommandId,
            request: Request,
        ) -> Response {
            catalog(&state, &actor, command_id, request, $action).await
        }
    };
}
catalog_handler!(
    /// `GET /api/admin/model-catalog`.
    catalog_view,
    Catalog::View
);
catalog_handler!(
    /// `POST /api/admin/model-catalog/sync`.
    catalog_sync,
    Catalog::Sync
);
catalog_handler!(
    /// `POST /api/admin/model-catalog/approve`.
    catalog_approve,
    Catalog::Approve
);
catalog_handler!(
    /// `PUT /api/admin/model-catalog/rates`.
    catalog_rates,
    Catalog::Rates
);

#[cfg(test)]
mod error_matrix_tests {
    use super::*;
    #[tokio::test]
    async fn management_error_matrix_has_only_explicit_details_and_no_retry_for_cas() {
        let key = Uuid::new_v4();
        for (failure, status, code, expected_details) in [
            (ManagementError::Invalid, 400, "invalid_request", json!({})),
            (ManagementError::Forbidden, 403, "access_denied", json!({})),
            (
                ManagementError::NotFound,
                404,
                "resource_not_found",
                json!({}),
            ),
            (ManagementError::Database, 503, "db_unavailable", json!({})),
            (ManagementError::Busy, 503, "command_busy", json!({})),
            (
                ManagementError::Invariant,
                500,
                "invariant_violation",
                json!({}),
            ),
            (
                ManagementError::CommandConflict,
                409,
                "command_conflict",
                json!({}),
            ),
            (ManagementError::Quota, 409, "key_quota_exceeded", json!({})),
            (
                ManagementError::PolicyPeriod,
                409,
                "policy_period_conflict",
                json!({}),
            ),
            (
                ManagementError::ConfigurationConflict,
                409,
                "configuration_conflict",
                json!({}),
            ),
            (
                ManagementError::SecurityFence,
                409,
                "security_fence_conflict",
                json!({}),
            ),
            (
                ManagementError::SettlementConflict,
                409,
                "settlement_conflict",
                json!({}),
            ),
            (
                ManagementError::ExecutionNotFinal,
                409,
                "execution_not_final",
                json!({}),
            ),
            (
                ManagementError::Version(17),
                409,
                "version_conflict",
                json!({"current_version":"17"}),
            ),
            (
                ManagementError::SecretUnavailable(key),
                409,
                "key_secret_unavailable",
                json!({"key_id":key}),
            ),
            (
                ManagementError::Query(crate::ledger::dashboard::QueryError::InvalidCursor),
                400,
                "invalid_cursor",
                json!({}),
            ),
            (
                ManagementError::Query(crate::ledger::dashboard::QueryError::InvalidFilter),
                400,
                "invalid_filter",
                json!({}),
            ),
            (
                ManagementError::Query(crate::ledger::dashboard::QueryError::DetailUnavailable(
                    "2026-01-01T00:00:00Z".into(),
                )),
                409,
                "usage_detail_unavailable",
                json!({"detail_available_from":"2026-01-01T00:00:00Z"}),
            ),
        ] {
            let r = error(failure);
            assert_eq!(r.status().as_u16(), status, "{code}");
            assert_eq!(r.headers()["x-gateway-error-code"], code);
            assert_eq!(r.headers()["cache-control"], "no-store");
            assert_eq!(r.headers()["x-gateway-dispatch-state"], "not_dispatched");
            assert_eq!(
                r.headers().contains_key("retry-after"),
                status == 503,
                "{code}"
            );
            assert!(!r.headers().contains_key("x-should-retry"));
            let id = r.headers()["x-gateway-request-id"]
                .to_str()
                .unwrap()
                .to_owned();
            Uuid::parse_str(&id).unwrap();
            let body = axum::body::to_bytes(r.into_body(), 2048).await.unwrap();
            let v: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(v["error"]["code"], code);
            assert_eq!(v["error"]["request_id"], id);
            assert_eq!(v["error"]["details"], expected_details);
            assert_eq!(v["error"].as_object().unwrap().len(), 4);
        }
    }
}
