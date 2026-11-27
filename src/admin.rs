mod guard;
mod management;
use crate::{
    auth::{AuthRoute, SsoAuthenticator},
    config::ValidatedConfig,
    ledger::{
        PgIdentityStore,
        dashboard::{QueryError, UsageQuery},
    },
    runtime::Runtime,
};
use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::sync::Arc;

pub struct DashboardApi {
    auth: Arc<SsoAuthenticator>,
    store: Arc<PgIdentityStore>,
    static_grants: Vec<Value>,
    catalog: Option<Arc<crate::catalog::CatalogService>>,
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
    /// 이 서버에 Admin API 키가 있어 토큰 대조와 사후 보정을 쓸 수 있는지.
    token_check_enabled: bool,
    /// 설정 파일 profile의 Anthropic 모델 가격. 토큰 대조의 차이 금액에 쓴다.
    static_prices: crate::ledger::token_check::Prices,
}
/// 사용량 보고서는 Provider가 응답에 알린 모델 이름을 쓴다. profile 이름과 그 이름들 모두에 같은 가격을 둔다.
fn model_prices(
    profile: &crate::config::ModelProfile,
) -> impl Iterator<Item = (String, std::collections::BTreeMap<String, String>)> + '_ {
    std::iter::once(&profile.model)
        .chain(profile.actual_models.iter())
        .map(|name| (name.clone(), profile.rates.clone()))
}
impl DashboardApi {
    /// Provider 회사 키 관리 API를 붙인다. 없으면 해당 경로는 404다.
    /// 모델·가격 카탈로그 관리 API를 붙이고, 승인된 카탈로그 모델을 권한 후보에 넣는다.
    pub fn with_catalog(mut self, service: Arc<crate::catalog::CatalogService>) -> Self {
        self.catalog = Some(service);
        self
    }
    /// Anthropic 모델의 지금 가격. 설정 파일 profile과 승인된 카탈로그를 합치고, 카탈로그가 우선한다.
    fn prices(&self) -> crate::ledger::token_check::Prices {
        let mut prices = self.static_prices.clone();
        if let Some(catalog) = &self.catalog {
            for (provider, profile) in &catalog.cache().snapshot().profiles {
                if *provider == crate::config::ProviderId::Anthropic {
                    prices.extend(model_prices(profile));
                }
            }
        }
        prices
    }
    /// 정책에 줄 수 있는 (Provider, 모델, 기능) 목록. 설정 파일 profile 과 승인된 카탈로그를 합친다.
    fn grants(&self) -> Value {
        let mut grants = self.static_grants.clone();
        if let Some(catalog) = &self.catalog {
            for (provider, profile) in &catalog.cache().snapshot().profiles {
                grants.push(json!({"provider":crate::ledger::finance::provider_name(*provider),"model":profile.model,"capabilities":[crate::ledger::finance::api_name(profile.api)]}));
            }
        }
        Value::Array(grants)
    }
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
            // 조회 서비스와 같은 조건이다: Admin API 키와 Anthropic Provider가 모두 있어야 한다.
            token_check_enabled: config.anthropic_admin_api_key().is_some()
                && config
                    .providers()
                    .iter()
                    .any(|p| p.provider == crate::config::ProviderId::Anthropic),
            static_prices: config
                .providers()
                .iter()
                .filter(|p| p.provider == crate::config::ProviderId::Anthropic)
                .flat_map(|p| p.profiles.iter())
                .filter(|p| p.api == crate::config::ApiKind::Messages)
                .flat_map(model_prices)
                .collect(),
            static_grants: grants,
            catalog: None,
            digest,
            digest_bytes: crate::gateway::auth_config_digest(config),
            signing_key: config.csrf_signing_key_bytes().to_vec(),
            ceiling: config.execution_ceiling().clone(),
            login: config.login_redirect(),
            refresh: config.auth().refresh_url.to_string(),
            logout: config.logout_redirect(),
            origin: config
                .public_origin()
                .to_string()
                .trim_end_matches('/')
                .to_owned(),
        }
    }
    /// 관리 API의 경로 표(결정 0010). 경로마다 한 번만 등록한다.
    /// 세 번째 값은 허용하지 않는 메서드에 답하는 405 응답의 `Allow` 헤더다.
    pub fn routes(self: Arc<Self>, runtime: Arc<Runtime>) -> Router {
        use axum::routing::{delete, get, post, put};
        use guard::{Checks, route};
        use management as m;
        let s = ApiState { api: self, runtime };
        let (user, admin) = (Checks::USER, Checks::ADMIN);
        let csrf = Checks::Management {
            auth: AuthRoute::CookieOnly,
            admin: false,
            key_owner: false,
        };
        let key = Checks::Management {
            auth: AuthRoute::Management,
            admin: false,
            key_owner: true,
        };
        Router::new()
            .route("/auth/login", route(&s, Checks::Login, "GET", get(login)))
            .route("/api/me", route(&s, Checks::Dashboard, "GET", get(me)))
            .route(
                "/api/usage",
                route(&s, Checks::Dashboard, "GET", get(usage)),
            )
            .route(
                "/api/usage/models",
                route(&s, Checks::Dashboard, "GET", get(usage_models)),
            )
            .route("/api/csrf", route(&s, csrf, "GET", get(m::csrf_token)))
            .route(
                "/api/keys",
                route(&s, user, "GET, POST", get(m::list_keys).post(m::issue_key)),
            )
            .route(
                "/api/keys/{id}",
                route(
                    &s,
                    key,
                    "DELETE, PATCH",
                    delete(m::revoke_key).patch(m::edit_key),
                ),
            )
            .route(
                "/api/admin/principals",
                route(
                    &s,
                    admin,
                    "GET, POST",
                    get(m::list_principals).post(m::create_principal),
                ),
            )
            .route(
                "/api/admin/principals/{id}",
                route(
                    &s,
                    admin,
                    "GET, PATCH",
                    get(m::principal_detail).patch(m::update_principal),
                ),
            )
            .route(
                "/api/admin/principals/{id}/policy",
                route(&s, admin, "PUT", put(m::set_policy)),
            )
            .route(
                "/api/admin/principals/{id}/inference-pause/release",
                route(&s, admin, "POST", post(m::release_pause)),
            )
            .route(
                "/api/admin/principals/{id}/provider-credentials",
                route(&s, admin, "PUT", put(m::authorize_credentials)),
            )
            .route(
                "/api/admin/keys",
                route(&s, admin, "GET", get(m::list_keys)),
            )
            .route(
                "/api/admin/keys/{id}/provider-credential",
                route(&s, admin, "PUT", put(m::bind_key_credential)),
            )
            .route(
                "/api/admin/provider-credentials",
                route(&s, admin, "GET", get(m::list_credentials)),
            )
            .route(
                "/api/admin/provider-credentials/{provider}",
                route(&s, admin, "POST", post(m::add_credential)),
            )
            .route(
                "/api/admin/provider-credentials/{provider}/{id}",
                route(
                    &s,
                    admin,
                    "POST, PUT, PATCH, DELETE",
                    post(m::credential_item)
                        .put(m::credential_item)
                        .patch(m::credential_item)
                        .delete(m::credential_item),
                ),
            )
            .route(
                "/api/admin/settings",
                route(&s, admin, "GET", get(m::settings_view)),
            )
            .route(
                "/api/admin/settings/budget-timezone",
                route(&s, admin, "PUT", put(m::set_budget_timezone)),
            )
            .route(
                "/api/admin/settings/cutoff-rule",
                route(&s, admin, "PUT", put(m::set_cutoff_rule)),
            )
            .route(
                "/api/admin/token-check",
                route(&s, admin, "GET", get(m::token_check)),
            )
            .route(
                "/api/admin/model-catalog",
                route(&s, admin, "GET", get(m::catalog_view)),
            )
            .route(
                "/api/admin/model-catalog/sync",
                route(&s, admin, "POST", post(m::catalog_sync)),
            )
            .route(
                "/api/admin/model-catalog/approve",
                route(&s, admin, "POST", post(m::catalog_approve)),
            )
            .route(
                "/api/admin/usage",
                route(&s, admin, "GET", get(m::admin_usage)),
            )
            .route(
                "/api/admin/requests",
                route(&s, admin, "GET", get(m::list_requests)),
            )
            .route(
                "/api/admin/requests/models",
                route(&s, admin, "GET", get(m::request_models)),
            )
            .route(
                "/api/admin/requests/{id}",
                route(&s, admin, "GET", get(m::request_detail)),
            )
            .route(
                "/api/admin/requests/{id}/settlement",
                route(&s, admin, "PUT", put(m::settle)),
            )
            .with_state(s)
    }
}
#[derive(Clone)]
struct ApiState {
    api: Arc<DashboardApi>,
    runtime: Arc<Runtime>,
}
/// `GET /auth/login`. 로그인한 사용자를 대시보드로 보낸다. 로그인하지 않았으면 `guard`가 SSO로 보낸다.
async fn login() -> Response {
    redirect(StatusCode::SEE_OTHER, "/dashboard/")
}
/// `GET /api/me`. 대시보드가 처음 읽는 내 정보.
async fn me(State(state): State<ApiState>, actor: guard::Actor, request: Request) -> Response {
    if request.uri().query().is_some() {
        return query_error(QueryError::InvalidFilter);
    }
    let human = &actor.human;
    let grants = state.api.grants();
    match state
        .api
        .store
        .dashboard_me(human.current.id, &grants, &state.api.digest)
        .await
    {
        Ok(mut principal) => {
            let object = principal.as_object_mut().expect("principal object");
            let grants = object.remove("effective_grants").unwrap_or(json!([]));
            let budget_timezone = object.remove("budget_timezone").unwrap_or(Value::Null);
            let provider_credentials = object.remove("provider_credentials").unwrap_or(json!([]));
            let Some(expires) =
                time::OffsetDateTime::from_unix_timestamp(human.identity.expires_at())
                    .ok()
                    .and_then(|t| {
                        t.format(&time::format_description::well_known::Rfc3339)
                            .ok()
                    })
            else {
                return query_error(QueryError::Invariant);
            };
            let key_limit_maximum = crate::ledger::policy::PolicyLimits::key_maximum_view(
                &principal["policy"]["limits"],
                &state.api.ceiling,
            );
            success(
                json!({"key_limit_maximum":key_limit_maximum,"principal":principal,"role":if human.current.admin{"admin"}else{"user"},"grants":grants,"budget_timezone":budget_timezone,"provider_credentials":provider_credentials,"auth_expires_at":expires,"sso":{"refresh_url":state.api.refresh,"logout_url":state.api.logout}}),
            )
        }
        Err(error) => query_error(error),
    }
}
/// `GET /api/usage/models`. 내 사용량 화면의 모델 목록.
async fn usage_models(
    State(state): State<ApiState>,
    actor: guard::Actor,
    request: Request,
) -> Response {
    if request.uri().query().is_some() {
        return query_error(QueryError::InvalidFilter);
    }
    match state
        .api
        .store
        .dashboard_usage_models(actor.human.current.id)
        .await
    {
        Ok(value) => success(value),
        Err(error) => query_error(error),
    }
}
/// `GET /api/usage`. 내 사용량.
async fn usage(State(state): State<ApiState>, actor: guard::Actor, request: Request) -> Response {
    let query = match UsageQuery::parse(request.uri().query()) {
        Ok(q) => q,
        Err(e) => return query_error(e),
    };
    let grants = state.api.grants();
    match state
        .api
        .store
        .dashboard_usage_with_ceiling(
            actor.human.current.id,
            query,
            &grants,
            &state.api.digest,
            &state.api.signing_key,
            &state.api.ceiling,
        )
        .await
    {
        Ok(value) => success(value),
        Err(error) => query_error(error),
    }
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
