use super::{ApiState, query_error, success};
use crate::{
    auth::{AuthRoute, csrf},
    ledger::{
        commands::CommandContext,
        management::{Command, ManagementError},
        management_query::{self, QueryContext},
    },
    runtime::{HttpAdmission, RouteClass},
};
use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use uuid::Uuid;

const CREDENTIALS: &str = "/gateway/admin/provider-credentials";
const CATALOG: &str = "/gateway/admin/model-catalog";
fn failure(status: StatusCode, code: &'static str) -> Response {
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
        PolicyPeriod => (409, "policy_period_conflict"),
        ConfigurationConflict => (409, "configuration_conflict"),
        SecurityFence => (409, "security_fence_conflict"),
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
        CredentialConflict(current) => {
            return details(
                StatusCode::CONFLICT,
                "credential_conflict",
                json!({"current_credential_id":current}),
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
pub(super) async fn handle(State(state): State<ApiState>, request: Request) -> Response {
    let path = request.uri().path();
    let allowed = if path == "/gateway/csrf" || path == CREDENTIALS || path == CATALOG {
        vec![Method::GET]
    } else if path == "/gateway/admin/model-catalog/sync"
        || path == "/gateway/admin/model-catalog/approve"
    {
        vec![Method::POST]
    } else if path.starts_with(CREDENTIALS) {
        vec![Method::PUT, Method::DELETE]
    } else if path == "/gateway/keys" || path == "/gateway/admin/principals" {
        vec![Method::GET, Method::POST]
    } else if path.starts_with("/gateway/keys/") {
        vec![Method::DELETE]
    } else if path.ends_with("/policy") || path.ends_with("/settlement") {
        vec![Method::PUT]
    } else if path.ends_with("/inference-pause/release") {
        vec![Method::POST]
    } else if path.starts_with("/gateway/admin/principals/") {
        vec![Method::GET, Method::PATCH]
    } else {
        vec![Method::GET]
    };
    if !allowed.contains(request.method()) {
        let mut r = crate::http::method_not_allowed();
        r.headers_mut().insert(
            header::ALLOW,
            HeaderValue::from_str(
                &allowed
                    .iter()
                    .map(Method::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
            )
            .expect("method"),
        );
        return r;
    }
    let write = request.method() != Method::GET;
    let Some(http) = request.extensions().get::<HttpAdmission>().cloned() else {
        return error(ManagementError::Invariant);
    };
    let mut lease = match state.runtime.try_admit_with_http(
        if write {
            RouteClass::ManagementWrite
        } else {
            RouteClass::ManagementRead
        },
        http,
    ) {
        Ok(l) => l,
        Err(e) => return crate::http::admission_error(e),
    };
    if let Err(e) = lease.begin_authentication() {
        return crate::http::admission_error(e);
    }
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(state.runtime.limits().management_timeout_ms),
        execute(&state, request, &mut lease),
    )
    .await;
    lease.bind_response(result.unwrap_or_else(|_| error(ManagementError::Database)))
}
async fn execute(
    state: &ApiState,
    request: Request,
    lease: &mut crate::runtime::RequestLease,
) -> Response {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let (head, body) = request.into_parts();
    let path = head.uri.path();
    let write = head.method != Method::GET;
    let human = match state
        .api
        .auth
        .authenticate(
            &head.headers,
            if path == "/gateway/csrf" {
                AuthRoute::CookieOnly
            } else {
                AuthRoute::Management
            },
            now,
        )
        .await
    {
        Ok(h) => h,
        Err(e) => return crate::http::authentication_error(e),
    };
    if let Err(e) = lease.identify(human.current.id) {
        return crate::http::admission_error(e);
    }
    let admin = path.starts_with("/gateway/admin/");
    if admin && !human.current.admin {
        return error(ManagementError::Forbidden);
    }
    if head.method == Method::DELETE && path.starts_with("/gateway/keys/") {
        let Some(id) = path
            .rsplit('/')
            .next()
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
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
        .credential(&head.headers, AuthRoute::Management)
    {
        Ok(c) => c,
        Err(e) => return crate::http::authentication_error(e),
    };
    if write && csrf::required(human.source) {
        let origins: Vec<_> = head.headers.get_all("origin").iter().collect();
        if origins.len() != 1 || origins[0].as_bytes() != state.api.origin.as_bytes() {
            return failure(StatusCode::FORBIDDEN, "origin_denied");
        }
        let tokens: Vec<_> = head.headers.get_all("x-csrf-token").iter().collect();
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
            return failure(StatusCode::FORBIDDEN, "csrf_invalid");
        }
    }
    if path == "/gateway/csrf" {
        if head.uri.query().is_some() {
            return error(ManagementError::Invalid);
        }
        return match csrf::issue(
            &state.api.signing_key,
            &credential.token,
            now,
            human.identity.expires_at(),
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
        };
    }
    if path == CREDENTIALS || path.starts_with(CREDENTIALS) {
        return provider_credentials(state, &head, body, human.current.id).await;
    }
    if path == CATALOG || path.starts_with(CATALOG) {
        return model_catalog(state, &head, body, human.current.id).await;
    }
    let grants = state.api.grants();
    let ctx = QueryContext {
        actor: human.current.id,
        admin,
        settings: state.api.auth.settings(),
        grants: &grants,
        digest: &state.api.digest,
        signing_key: &state.api.signing_key,
    };
    if !write {
        let result = if path == "/gateway/keys" || path == "/gateway/admin/keys" {
            state
                .api
                .store
                .management_list(&ctx, "keys", head.uri.query())
                .await
        } else if path == "/gateway/admin/principals" {
            state
                .api
                .store
                .management_list(&ctx, "principals", head.uri.query())
                .await
        } else if path == "/gateway/admin/requests" {
            state
                .api
                .store
                .management_list(&ctx, "requests", head.uri.query())
                .await
        } else if path == "/gateway/admin/usage" {
            state
                .api
                .store
                .management_usage(&ctx, head.uri.query(), &state.api.ceiling)
                .await
        } else {
            if head.uri.query().is_some() {
                return error(ManagementError::Invalid);
            }
            let Some(raw) = path.rsplit('/').next() else {
                return error(ManagementError::Invalid);
            };
            let Ok(id) = Uuid::parse_str(raw) else {
                return error(ManagementError::Invalid);
            };
            state
                .api
                .store
                .management_detail(
                    &ctx,
                    if path.starts_with("/gateway/admin/principals/") {
                        "principals"
                    } else {
                        "requests"
                    },
                    id,
                    &state.api.ceiling,
                )
                .await
        };
        return match result {
            Ok(v) => success(v),
            Err(e) => error(e),
        };
    }
    let commands: Vec<_> = head
        .headers
        .get_all("x-gateway-command-id")
        .iter()
        .collect();
    let command_id = if commands.len() == 1 {
        commands[0]
            .to_str()
            .ok()
            .and_then(|s| Uuid::parse_str(s).ok())
    } else {
        None
    };
    let Some(command_id) = command_id else {
        return error(ManagementError::Invalid);
    };
    if head
        .headers
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .any(|v| v.as_bytes() != b"identity")
    {
        return failure(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type");
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
    let bytes = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        to_bytes(body, body_limit),
    )
    .await
    {
        Ok(Ok(b)) => b,
        Ok(Err(_)) => return failure(StatusCode::PAYLOAD_TOO_LARGE, "body_too_large"),
        Err(_) => return failure(StatusCode::REQUEST_TIMEOUT, "request_body_timeout"),
    };
    let input = if head.method == Method::DELETE {
        if !bytes.is_empty() {
            return error(ManagementError::Invalid);
        }
        Value::Null
    } else {
        if !crate::providers::anthropic::json_content_type(&head.headers) {
            return failure(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type");
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
    let segments: Vec<_> = path.split('/').collect();
    let id = segments.get(4).and_then(|s| Uuid::parse_str(s).ok());
    let parse = || -> Result<Command, ManagementError> {
        if path == "/gateway/keys" {
            Ok(Command::Issue(
                serde_json::from_value(input.clone()).map_err(|_| ManagementError::Invalid)?,
            ))
        } else if path.starts_with("/gateway/keys/") {
            let id = segments
                .get(3)
                .and_then(|s| Uuid::parse_str(s).ok())
                .ok_or(ManagementError::Invalid)?;
            let mut f = management_query::fields(head.uri.query())?;
            let expected = f
                .remove("expected_version")
                .ok_or(ManagementError::Invalid)?;
            if !f.is_empty() {
                return Err(ManagementError::Invalid);
            }
            Ok(Command::Revoke { id, expected })
        } else if path == "/gateway/admin/principals" {
            Ok(Command::Create(
                serde_json::from_value(input.clone()).map_err(|_| ManagementError::Invalid)?,
            ))
        } else {
            let id = id.ok_or(ManagementError::Invalid)?;
            if path.ends_with("/policy") {
                Ok(Command::Policy(
                    id,
                    serde_json::from_value(input.clone()).map_err(|_| ManagementError::Invalid)?,
                ))
            } else if path.ends_with("/settlement") {
                Ok(Command::Settle(
                    id,
                    serde_json::from_value(input.clone()).map_err(|_| ManagementError::Invalid)?,
                ))
            } else if path.ends_with("/inference-pause/release") {
                Ok(Command::Release(
                    id,
                    serde_json::from_value(input.clone()).map_err(|_| ManagementError::Invalid)?,
                ))
            } else {
                Ok(Command::Update(
                    id,
                    serde_json::from_value(input.clone()).map_err(|_| ManagementError::Invalid)?,
                ))
            }
        }
    };
    if head.method != Method::DELETE && head.uri.query().is_some() {
        return error(ManagementError::Invalid);
    }
    let command = match parse() {
        Ok(c) => c,
        Err(e) => return error(e),
    };
    let normalized_path = if head.method == Method::DELETE {
        format!(
            "/gateway/keys/{}",
            segments
                .get(3)
                .and_then(|s| Uuid::parse_str(s).ok())
                .unwrap_or_default()
        )
    } else if let Some(id) = id {
        path.replace(segments[4], &id.to_string())
    } else {
        path.to_owned()
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
    let ctx = CommandContext {
        actor: human.current.id,
        command_id,
        method: head.method.as_str(),
        path: &normalized_path,
        settings: state.api.auth.settings(),
        grants: &grants,
        digest: state.api.digest_bytes,
        ceiling: &state.api.ceiling,
        execution_running: running,
    };
    match state.api.ledger.management_command(&ctx, command).await {
        Ok(result) => {
            let mut r = if let Some(v) = result.body {
                success(v)
            } else {
                StatusCode::NO_CONTENT.into_response()
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

/// Provider 회사 키 조회·등록·해제. 원문은 요청 본문에서만 받고 어떤 응답에도 담지 않는다.
async fn provider_credentials(
    state: &ApiState,
    head: &axum::http::request::Parts,
    body: axum::body::Body,
    actor: Uuid,
) -> Response {
    use crate::{config::ProviderId, ledger::credentials::CredentialCommand};
    let Some(service) = state.api.credentials.clone() else {
        return error(ManagementError::NotFound);
    };
    let path = head.uri.path();
    if head.method == Method::GET {
        if path != CREDENTIALS || head.uri.query().is_some() {
            return error(ManagementError::Invalid);
        }
        return match service.list().await {
            Ok(v) => success(v),
            Err(e) => error(e),
        };
    }
    let Some(provider) = path
        .strip_prefix(CREDENTIALS)
        .and_then(|p| p.strip_prefix('/'))
        .and_then(ProviderId::from_name)
    else {
        return error(ManagementError::NotFound);
    };
    let commands: Vec<_> = head
        .headers
        .get_all("x-gateway-command-id")
        .iter()
        .collect();
    let Some(command_id) = (commands.len() == 1)
        .then(|| {
            commands[0]
                .to_str()
                .ok()
                .and_then(|s| Uuid::parse_str(s).ok())
        })
        .flatten()
    else {
        return error(ManagementError::Invalid);
    };
    if head
        .headers
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .any(|v| v.as_bytes() != b"identity")
    {
        return failure(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type");
    }
    let body_limit = state.runtime.limits().management_body_bytes;
    let bytes = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        to_bytes(body, body_limit),
    )
    .await
    {
        Ok(Ok(b)) => b,
        Ok(Err(_)) => return failure(StatusCode::PAYLOAD_TOO_LARGE, "body_too_large"),
        Err(_) => return failure(StatusCode::REQUEST_TIMEOUT, "request_body_timeout"),
    };
    let command = if head.method == Method::PUT {
        if head.uri.query().is_some() {
            return error(ManagementError::Invalid);
        }
        if !crate::providers::anthropic::json_content_type(&head.headers) {
            return failure(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type");
        }
        let Ok(mut input) = serde_json::from_slice::<serde_json::Map<String, Value>>(&bytes) else {
            return error(ManagementError::Invalid);
        };
        if input.len() != 2 {
            return error(ManagementError::Invalid);
        }
        let expected = match input.remove("expected_credential_id") {
            Some(Value::Null) => None,
            Some(Value::String(s)) => match Uuid::parse_str(&s) {
                Ok(id) => Some(id),
                Err(_) => return error(ManagementError::Invalid),
            },
            _ => return error(ManagementError::Invalid),
        };
        let Some(Value::String(secret)) = input.remove("secret") else {
            return error(ManagementError::Invalid);
        };
        CredentialCommand::Set {
            provider,
            secret: zeroize::Zeroizing::new(secret),
            expected,
        }
    } else {
        if !bytes.is_empty() {
            return error(ManagementError::Invalid);
        }
        let mut fields = match management_query::fields(head.uri.query()) {
            Ok(f) => f,
            Err(e) => return error(e),
        };
        let Some(expected) = fields
            .remove("expected_credential_id")
            .and_then(|s| Uuid::parse_str(&s).ok())
        else {
            return error(ManagementError::Invalid);
        };
        if !fields.is_empty() {
            return error(ManagementError::Invalid);
        }
        CredentialCommand::Retire { provider, expected }
    };
    let grants = state.api.grants();
    let ctx = CommandContext {
        actor,
        command_id,
        method: head.method.as_str(),
        path,
        settings: state.api.auth.settings(),
        grants: &grants,
        digest: state.api.digest_bytes,
        ceiling: &state.api.ceiling,
        execution_running: false,
    };
    match service.execute(&ctx, command).await {
        Ok(result) => {
            // 이 프로세스는 즉시, 다른 프로세스는 상태 점검 주기에 맞춰 반영한다.
            let _ = service.refresh().await;
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
        Err(e) => error(e),
    }
}

/// 모델·가격 카탈로그 조회, 공개 가격 목록 동기화(변경안 만들기), 변경안 승인.
async fn model_catalog(
    state: &ApiState,
    head: &axum::http::request::Parts,
    body: axum::body::Body,
    actor: Uuid,
) -> Response {
    let Some(service) = state.api.catalog.clone().filter(|c| c.enabled()) else {
        return error(ManagementError::NotFound);
    };
    let path = head.uri.path();
    if head.uri.query().is_some() {
        return error(ManagementError::Invalid);
    }
    if head.method == Method::GET {
        return match service.view().await {
            Ok(v) => success(v),
            Err(e) => error(e),
        };
    }
    let commands: Vec<_> = head
        .headers
        .get_all("x-gateway-command-id")
        .iter()
        .collect();
    let Some(command_id) = (commands.len() == 1)
        .then(|| {
            commands[0]
                .to_str()
                .ok()
                .and_then(|s| Uuid::parse_str(s).ok())
        })
        .flatten()
    else {
        return error(ManagementError::Invalid);
    };
    if head
        .headers
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .any(|v| v.as_bytes() != b"identity")
    {
        return failure(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type");
    }
    let body_limit = state.runtime.limits().management_body_bytes;
    let bytes = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        to_bytes(body, body_limit),
    )
    .await
    {
        Ok(Ok(b)) => b,
        Ok(Err(_)) => return failure(StatusCode::PAYLOAD_TOO_LARGE, "body_too_large"),
        Err(_) => return failure(StatusCode::REQUEST_TIMEOUT, "request_body_timeout"),
    };
    let input: Value = if bytes.is_empty() {
        json!({})
    } else if !crate::providers::anthropic::json_content_type(&head.headers) {
        return failure(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type");
    } else {
        match serde_json::from_slice(&bytes) {
            Ok(v @ Value::Object(_)) => v,
            _ => return error(ManagementError::Invalid),
        }
    };
    let grants = state.api.grants();
    let ctx = CommandContext {
        actor,
        command_id,
        method: head.method.as_str(),
        path,
        settings: state.api.auth.settings(),
        grants: &grants,
        digest: state.api.digest_bytes,
        ceiling: &state.api.ceiling,
        execution_running: false,
    };
    let result = if path == "/gateway/admin/model-catalog/sync" {
        if input.as_object().is_some_and(|o| !o.is_empty()) {
            return error(ManagementError::Invalid);
        }
        service.sync(&ctx).await
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
        Err(e) => error(e),
    }
}

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
