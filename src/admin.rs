mod management;
use crate::{
    auth::{AuthRoute, SsoAuthenticator},
    config::ValidatedConfig,
    ledger::{
        PgIdentityStore,
        dashboard::{QueryError, UsageQuery},
    },
    runtime::{HttpAdmission, RouteClass, Runtime},
};
use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub struct DashboardApi {
    auth: Arc<SsoAuthenticator>,
    store: Arc<PgIdentityStore>,
    grants: Value,
    digest: String,
    digest_bytes: [u8; 32],
    signing_key: Vec<u8>,
    ceiling: crate::ledger::policy::PolicyLimits,
    login: String,
    refresh: String,
    logout: String,
    origin: String,
    ledger: Arc<crate::ledger::PgLedger>,
    credentials: Option<Arc<crate::credentials::CredentialService>>,
}
impl DashboardApi {
    /// Provider 회사 키 관리 API를 붙인다. 없으면 해당 경로는 404다.
    pub fn with_credentials(mut self, service: Arc<crate::credentials::CredentialService>) -> Self {
        self.credentials = Some(service);
        self
    }
    pub fn new(
        config: &ValidatedConfig,
        auth: Arc<SsoAuthenticator>,
        store: Arc<PgIdentityStore>,
    ) -> Self {
        let grants:Vec<Value>=config.providers().iter().flat_map(|p|p.profiles.iter().map(|profile|json!({"provider":crate::ledger::finance::provider_name(p.provider),"model":profile.model,"capabilities":[crate::ledger::finance::api_name(profile.api)]}))).collect();
        let digest = crate::gateway::auth_config_digest(config)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Self {
            ledger: Arc::new(
                crate::ledger::PgLedger::new(
                    store.clone(),
                    auth.settings().clone(),
                    crate::gateway::auth_config_digest(config),
                )
                .with_ceiling(config.execution_ceiling().clone()),
            ),
            auth,
            store,
            credentials: None,
            grants: json!(grants),
            digest,
            digest_bytes: crate::gateway::auth_config_digest(config),
            signing_key: config.csrf_signing_key_bytes().to_vec(),
            ceiling: config.execution_ceiling().clone(),
            login: config.auth().login_url.to_string(),
            refresh: config.auth().refresh_url.to_string(),
            logout: config.auth().logout_url.to_string(),
            origin: config
                .public_origin()
                .to_string()
                .trim_end_matches('/')
                .to_owned(),
        }
    }
    pub fn routes(self: Arc<Self>, runtime: Arc<Runtime>) -> Router {
        Router::new()
            .route("/gateway/me", any(read))
            .route("/gateway/usage", any(read))
            .route("/auth/login", any(read))
            .route("/gateway/csrf", any(management::handle))
            .route("/gateway/keys", any(management::handle))
            .route("/gateway/keys/{id}", any(management::handle))
            .route("/gateway/admin/principals", any(management::handle))
            .route("/gateway/admin/principals/{id}", any(management::handle))
            .route(
                "/gateway/admin/principals/{id}/policy",
                any(management::handle),
            )
            .route(
                "/gateway/admin/principals/{id}/inference-pause/release",
                any(management::handle),
            )
            .route("/gateway/admin/keys", any(management::handle))
            .route(
                "/gateway/admin/provider-credentials",
                any(management::handle),
            )
            .route(
                "/gateway/admin/provider-credentials/{provider}",
                any(management::handle),
            )
            .route("/gateway/admin/usage", any(management::handle))
            .route("/gateway/admin/requests", any(management::handle))
            .route("/gateway/admin/requests/{id}", any(management::handle))
            .route(
                "/gateway/admin/requests/{id}/settlement",
                any(management::handle),
            )
            .with_state(ApiState { api: self, runtime })
    }
}
#[derive(Clone)]
struct ApiState {
    api: Arc<DashboardApi>,
    runtime: Arc<Runtime>,
}
async fn read(State(state): State<ApiState>, request: Request) -> Response {
    if request.method() != Method::GET {
        return crate::http::method_not_allowed();
    }
    let (request, body) = request.into_parts();
    drop(body);
    let Some(http) = request.extensions.get::<HttpAdmission>() else {
        return crate::http::admission_error(crate::runtime::AdmissionError::InvalidTransition);
    };
    let mut lease = match state
        .runtime
        .try_admit_with_http(RouteClass::ManagementRead, http.clone())
    {
        Ok(lease) => lease,
        Err(error) => return crate::http::admission_error(error),
    };
    let result=tokio::time::timeout(Duration::from_millis(state.runtime.limits().management_timeout_ms),async {
        if let Err(error)=lease.begin_authentication() { return crate::http::admission_error(error); }
        let login=request.uri.path()=="/auth/login";
        let explicit=request.headers.contains_key("authorization")||request.headers.contains_key("x-api-key");
        let now=SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
        let human=match state.api.auth.authenticate(&request.headers,if login{AuthRoute::Login}else{AuthRoute::Management},now).await {
            Ok(human)=>human,
            Err(crate::auth::AuthError::InvalidCredentials) if login&&!explicit=>return redirect(StatusCode::FOUND,&state.api.login),
            Err(error)=>return crate::http::authentication_error(error),
        };
        if let Err(error)=lease.identify(human.current.id) { return crate::http::admission_error(error); }
        if login { return redirect(StatusCode::SEE_OTHER,"/ui/"); }
        if request.uri.path()=="/gateway/me" {
            if request.uri.query().is_some() { return query_error(QueryError::InvalidFilter); }
            match state.api.store.dashboard_me(human.current.id,&state.api.grants,&state.api.digest).await {
                Ok(mut principal)=>{
                    let grants=principal.as_object_mut().expect("principal object").remove("effective_grants").unwrap_or(json!([]));
                    let Some(expires)=time::OffsetDateTime::from_unix_timestamp(human.identity.expires_at()).ok().and_then(|t|t.format(&time::format_description::well_known::Rfc3339).ok()) else { return query_error(QueryError::Invariant); };
                    success(json!({"principal":principal,"role":if human.current.admin{"admin"}else{"user"},"grants":grants,"auth_expires_at":expires,"sso":{"refresh_url":state.api.refresh,"logout_url":state.api.logout}}))
                },Err(error)=>query_error(error),
            }
        } else {
            let query=match UsageQuery::parse(request.uri.query()) { Ok(q)=>q,Err(e)=>return query_error(e) };
            match state.api.store.dashboard_usage_with_ceiling(human.current.id,query,&state.api.grants,&state.api.digest,&state.api.signing_key,&state.api.ceiling).await { Ok(value)=>success(value),Err(error)=>query_error(error) }
        }
    }).await;
    lease.bind_response(result.unwrap_or_else(|_| query_error(QueryError::Database)))
}
fn redirect(status: StatusCode, to: &str) -> Response {
    let mut r = status.into_response();
    r.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(to).expect("검증된 고정 redirect"),
    );
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}
fn success(value: Value) -> Response {
    let mut r = Json(value).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r.headers_mut().insert(
        "x-gateway-request-id",
        HeaderValue::from_str(&uuid::Uuid::new_v4().to_string()).expect("UUID"),
    );
    r
}
fn query_error(error: QueryError) -> Response {
    let (status, code, message) = match &error {
        QueryError::InvalidFilter => (
            StatusCode::BAD_REQUEST,
            "invalid_filter",
            "조회 조건이 유효하지 않습니다.",
        ),
        QueryError::InvalidCursor => (
            StatusCode::BAD_REQUEST,
            "invalid_cursor",
            "조회 cursor가 유효하지 않습니다.",
        ),
        QueryError::NotFound => (
            StatusCode::NOT_FOUND,
            "resource_not_found",
            "리소스를 찾을 수 없습니다.",
        ),
        QueryError::AccessDenied => (
            StatusCode::FORBIDDEN,
            "access_denied",
            "현재 접근이 허용되지 않습니다.",
        ),
        QueryError::Database => (
            StatusCode::SERVICE_UNAVAILABLE,
            "db_unavailable",
            "데이터베이스가 준비되지 않았습니다.",
        ),
        QueryError::Invariant => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "invariant_violation",
            "조회 상태가 유효하지 않습니다.",
        ),
        QueryError::DetailUnavailable(_) => (
            StatusCode::CONFLICT,
            "usage_detail_unavailable",
            "해당 기간의 상세 사용량을 제공할 수 없습니다.",
        ),
    };
    if let QueryError::DetailUnavailable(boundary) = error {
        let id = uuid::Uuid::new_v4();
        let mut r=Json(json!({"error":{"code":code,"message":message,"request_id":id.to_string(),"details":{"detail_available_from":boundary}}})).into_response();
        *r.status_mut() = status;
        for (name, value) in [
            ("cache-control", "no-store".to_owned()),
            ("x-gateway-request-id", id.to_string()),
            ("x-gateway-error-code", code.to_owned()),
            ("x-gateway-dispatch-state", "not_dispatched".to_owned()),
        ] {
            r.headers_mut()
                .insert(name, HeaderValue::from_str(&value).expect("고정값"));
        }
        r
    } else {
        let mut r = crate::http::route_error(status, code, message);
        if status == StatusCode::SERVICE_UNAVAILABLE {
            r.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        }
        r
    }
}
