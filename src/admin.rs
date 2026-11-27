mod guard;
mod management;
mod password;
use crate::{
    auth::{AuthRoute, Authenticator},
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
    auth: Arc<Authenticator>,
    store: Arc<PgIdentityStore>,
    static_grants: Vec<Value>,
    catalog: Option<Arc<crate::catalog::CatalogService>>,
    digest: String,
    digest_bytes: [u8; 32],
    signing_key: Vec<u8>,
    ceiling: crate::ledger::policy::PolicyLimits,
    login: String,
    /// SSO 토큰 갱신 주소. 비밀번호 방식에는 없다.
    refresh: Option<String>,
    mode: crate::config::AuthMode,
    logout: String,
    origin: String,
    ledger: Arc<crate::ledger::PgLedger>,
    credentials: Option<Arc<crate::credentials::CredentialService>>,
    /// 이 서버에 Admin API 키가 있어 토큰 대조와 사후 보정을 쓸 수 있는지.
    token_check_enabled: bool,
    /// 설정 파일 profile의 Anthropic 모델 가격. 토큰 대조의 차이 금액에 쓴다.
    static_prices: crate::ledger::token_check::Prices,
    /// 초대·재설정 메일을 보낸다. 비밀번호 방식에서만 있다.
    mailer: Option<Arc<dyn crate::auth::password::Mailer>>,
    /// 없는 이메일로 로그인할 때 비교할 가짜 해시. 처음 쓸 때 한 번 만든다.
    dummy: tokio::sync::OnceCell<String>,
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
    /// 초대·재설정 메일을 보낼 전송기를 붙인다. 비밀번호 방식에서만 쓴다.
    pub fn with_mailer(mut self, mailer: Arc<dyn crate::auth::password::Mailer>) -> Self {
        self.mailer = Some(mailer);
        self
    }
    /// 없는 이메일의 로그인 시도가 같은 시간이 걸리도록 비교하는 가짜 argon2id 해시.
    async fn dummy_hash(&self) -> String {
        self.dummy
            .get_or_init(|| async {
                let mut seed = [0u8; 16];
                let _ = getrandom::getrandom(&mut seed);
                crate::auth::password::hash_password(format!("{seed:?}"))
                    .await
                    .unwrap_or_default()
            })
            .await
            .clone()
    }
    pub fn with_credentials(mut self, service: Arc<crate::credentials::CredentialService>) -> Self {
        self.credentials = Some(service);
        self
    }
    pub fn new(
        config: &ValidatedConfig,
        auth: Arc<Authenticator>,
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
                    crate::gateway::auth_config_digest(config),
                )
                .with_ceiling(config.execution_ceiling().clone()),
            ),
            auth,
            store,
            credentials: None,
            mailer: None,
            dummy: tokio::sync::OnceCell::new(),
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
            refresh: config.sso().map(|a| a.refresh_url.to_string()),
            mode: config.auth_mode(),
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
            tokens: false,
        };
        let key = Checks::Management {
            auth: AuthRoute::Management,
            admin: false,
            key_owner: true,
            tokens: true,
        };
        let table = Table {
            router: Router::new().route("/auth/login", route(&s, Checks::Login, "GET", get(login))),
            state: s.clone(),
            added: Vec::new(),
        }
        .at("/api/me", Checks::Dashboard, "GET", get(me))
        .at("/api/openapi.json", Checks::Dashboard, "GET", get(openapi))
        .at("/api/usage", Checks::Dashboard, "GET", get(usage))
        .at(
            "/api/usage/models",
            Checks::Dashboard,
            "GET",
            get(usage_models),
        )
        .at("/api/csrf", csrf, "GET", get(m::csrf_token))
        .at(
            "/api/keys",
            user,
            "GET, POST",
            get(m::list_keys).post(m::issue_key),
        )
        .at(
            "/api/keys/{id}",
            key,
            "DELETE, PATCH",
            delete(m::revoke_key).patch(m::edit_key),
        )
        .at(
            "/api/admin/principals",
            admin,
            "GET, POST",
            get(m::list_principals).post(m::create_principal),
        )
        .at(
            "/api/admin/principals/{id}",
            admin,
            "GET, PATCH",
            get(m::principal_detail).patch(m::update_principal),
        )
        .at(
            "/api/admin/principals/{id}/policy",
            admin,
            "PUT",
            put(m::set_policy),
        )
        .at(
            "/api/admin/principals/{id}/inference-pause/release",
            admin,
            "POST",
            post(m::release_pause),
        )
        .at(
            "/api/admin/principals/{id}/provider-credentials",
            admin,
            "PUT",
            put(m::authorize_credentials),
        )
        .at("/api/admin/keys", admin, "GET", get(m::list_keys))
        .at(
            "/api/admin/keys/{id}/provider-credential",
            admin,
            "PUT",
            put(m::bind_key_credential),
        )
        .at(
            "/api/admin/provider-credentials",
            admin,
            "GET",
            get(m::list_credentials),
        )
        .at(
            "/api/admin/provider-credentials/{provider}",
            admin,
            "POST",
            post(m::add_credential),
        )
        .at(
            "/api/admin/provider-credentials/{provider}/{id}",
            admin,
            "POST, PUT, PATCH, DELETE",
            post(m::credential_item)
                .put(m::credential_item)
                .patch(m::credential_item)
                .delete(m::credential_item),
        )
        .at(
            "/api/admin/access",
            admin,
            "GET, POST",
            get(m::access_list).post(m::add_access),
        )
        .at(
            "/api/admin/access/{id}",
            admin,
            "DELETE, PATCH",
            delete(m::remove_access).patch(m::update_access),
        )
        .at(
            "/api/admin/management-tokens",
            Checks::ADMIN_NO_TOKEN,
            "GET, POST",
            get(m::list_management_tokens).post(m::create_management_token),
        )
        .at(
            "/api/admin/management-tokens/{id}",
            Checks::ADMIN_NO_TOKEN,
            "DELETE",
            delete(m::revoke_management_token),
        )
        .at("/api/admin/settings", admin, "GET", get(m::settings_view))
        .at(
            "/api/admin/settings/budget-timezone",
            admin,
            "PUT",
            put(m::set_budget_timezone),
        )
        .at(
            "/api/admin/settings/cutoff-rule",
            admin,
            "PUT",
            put(m::set_cutoff_rule),
        )
        .at("/api/admin/token-check", admin, "GET", get(m::token_check))
        .at(
            "/api/admin/model-catalog",
            admin,
            "GET",
            get(m::catalog_view),
        )
        .at(
            "/api/admin/model-catalog/sync",
            admin,
            "POST",
            post(m::catalog_sync),
        )
        .at(
            "/api/admin/model-catalog/approve",
            admin,
            "POST",
            post(m::catalog_approve),
        )
        .at(
            "/api/admin/model-catalog/rates",
            admin,
            "PUT",
            put(m::catalog_rates),
        )
        .at("/api/admin/usage", admin, "GET", get(m::admin_usage))
        .at("/api/admin/requests", admin, "GET", get(m::list_requests))
        .at(
            "/api/admin/requests/models",
            admin,
            "GET",
            get(m::request_models),
        )
        .at(
            "/api/admin/requests/{id}",
            admin,
            "GET",
            get(m::request_detail),
        )
        .at(
            "/api/admin/requests/{id}/settlement",
            admin,
            "PUT",
            put(m::settle),
        );
        let password_mode = s.api.mode == crate::config::AuthMode::Password;
        // 비밀번호 방식의 경로(결정 0011). SSO 방식에는 없다.
        let table = if password_mode {
            use password as pw;
            table
                .at(
                    "/api/me/password",
                    Checks::USER_NO_TOKEN,
                    "POST",
                    post(pw::change_password),
                )
                .at(
                    "/api/admin/access/{id}/link",
                    admin,
                    "POST",
                    post(pw::send_link),
                )
        } else {
            table
        };
        debug_assert_eq!(
            {
                let mut added = table.added.clone();
                added.sort();
                added
            },
            route_table(password_mode),
            "관리 API 경로 표(ROUTES)와 등록한 경로가 다르다"
        );
        let router = if password_mode {
            use password as pw;
            let open = |allow: &'static str, handler: axum::routing::MethodRouter<ApiState>| {
                handler.fallback(move || async move {
                    let mut response = crate::http::method_not_allowed();
                    response.headers_mut().insert(
                        axum::http::header::ALLOW,
                        axum::http::HeaderValue::from_static(allow),
                    );
                    response
                })
            };
            table
                .router
                .route("/auth/password", open("POST", post(pw::sign_in)))
                .route("/auth/password/link", open("POST", post(pw::link_info)))
                .route("/auth/password/set", open("POST", post(pw::set_password)))
                .route("/auth/logout", open("POST", post(pw::sign_out)))
        } else {
            table.router
        };
        router.with_state(s)
    }
}
/// 관리 API의 경로 표(결정 0014 규칙 4): 경로, 허용 메서드, 비밀번호 방식에만 있는지.
/// `/api/` 아래 경로만 둔다. `routes`가 등록하는 경로와 OpenAPI 문서의 경로는 이 표와 같아야 한다.
pub const ROUTES: &[(&str, &str, bool)] = &[
    ("/api/me", "GET", false),
    ("/api/me/password", "POST", true),
    ("/api/openapi.json", "GET", false),
    ("/api/usage", "GET", false),
    ("/api/usage/models", "GET", false),
    ("/api/csrf", "GET", false),
    ("/api/keys", "GET, POST", false),
    ("/api/keys/{id}", "DELETE, PATCH", false),
    ("/api/admin/principals", "GET, POST", false),
    ("/api/admin/principals/{id}", "GET, PATCH", false),
    ("/api/admin/principals/{id}/policy", "PUT", false),
    (
        "/api/admin/principals/{id}/inference-pause/release",
        "POST",
        false,
    ),
    (
        "/api/admin/principals/{id}/provider-credentials",
        "PUT",
        false,
    ),
    ("/api/admin/keys", "GET", false),
    ("/api/admin/keys/{id}/provider-credential", "PUT", false),
    ("/api/admin/provider-credentials", "GET", false),
    ("/api/admin/provider-credentials/{provider}", "POST", false),
    (
        "/api/admin/provider-credentials/{provider}/{id}",
        "POST, PUT, PATCH, DELETE",
        false,
    ),
    ("/api/admin/access", "GET, POST", false),
    ("/api/admin/access/{id}", "DELETE, PATCH", false),
    ("/api/admin/access/{id}/link", "POST", true),
    ("/api/admin/management-tokens", "GET, POST", false),
    ("/api/admin/management-tokens/{id}", "DELETE", false),
    ("/api/admin/settings", "GET", false),
    ("/api/admin/settings/budget-timezone", "PUT", false),
    ("/api/admin/settings/cutoff-rule", "PUT", false),
    ("/api/admin/token-check", "GET", false),
    ("/api/admin/model-catalog", "GET", false),
    ("/api/admin/model-catalog/sync", "POST", false),
    ("/api/admin/model-catalog/approve", "POST", false),
    ("/api/admin/model-catalog/rates", "PUT", false),
    ("/api/admin/usage", "GET", false),
    ("/api/admin/requests", "GET", false),
    ("/api/admin/requests/models", "GET", false),
    ("/api/admin/requests/{id}", "GET", false),
    ("/api/admin/requests/{id}/settlement", "PUT", false),
];
/// 한 로그인 방식에서 등록하는 경로와 허용 메서드. 경로 순서로 정렬한다.
pub fn route_table(password: bool) -> Vec<(&'static str, &'static str)> {
    let mut table: Vec<_> = ROUTES
        .iter()
        .filter(|(_, _, only)| password || !only)
        .map(|(path, allow, _)| (*path, *allow))
        .collect();
    table.sort();
    table
}
/// 경로를 등록하면서 등록한 경로와 허용 메서드를 모은다.
struct Table {
    router: Router<ApiState>,
    state: ApiState,
    added: Vec<(&'static str, &'static str)>,
}
impl Table {
    fn at(
        mut self,
        path: &'static str,
        checks: guard::Checks,
        allow: &'static str,
        methods: axum::routing::MethodRouter<ApiState>,
    ) -> Self {
        self.router = self
            .router
            .route(path, guard::route(&self.state, checks, allow, methods));
        self.added.push((path, allow));
        self
    }
}
/// OpenAPI 문서(결정 0014 규칙 4). 바이너리에 넣어 두고 그대로 준다.
pub const OPENAPI: &str = include_str!("../api/management.openapi.json");
/// `GET /api/openapi.json`. 로그인한 사람과 관리 토큰에 관리 API 문서를 준다.
async fn openapi(request: Request) -> Response {
    if request.uri().query().is_some() {
        return query_error(QueryError::InvalidFilter);
    }
    let mut r = (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        OPENAPI,
    )
        .into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r.headers_mut().insert(
        "x-gateway-request-id",
        HeaderValue::from_str(&uuid::Uuid::new_v4().to_string()).expect("UUID"),
    );
    r
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
                json!({"key_limit_maximum":key_limit_maximum,"principal":principal,"role":if human.current.admin{"admin"}else{"user"},"grants":grants,"budget_timezone":budget_timezone,"provider_credentials":provider_credentials,"auth_expires_at":expires,"auth":{"mode":match state.api.mode{crate::config::AuthMode::Sso=>"sso",crate::config::AuthMode::Password=>"password"},"refresh_url":state.api.refresh,"logout_url":state.api.logout}}),
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

/// 관리 API가 낼 수 있는 오류 코드가 모두 OpenAPI 문서에 있는지 본다(결정 0014 규칙 4).
/// 오류를 만드는 함수는 이 crate 밖에서 부를 수 없으므로 여기서 시험한다.
#[cfg(test)]
mod openapi_tests {
    use super::{management::error, query_error};
    use crate::{auth::AuthError, ledger::management::ManagementError, runtime::AdmissionError};
    use axum::{http::StatusCode, response::Response};
    use serde_json::Value;
    use std::collections::BTreeSet;
    use uuid::Uuid;

    /// `/auth/` 아래 비밀번호 로그인 경로만 내는 코드. 문서는 `/auth/`를 설명하지 않는다(결정 0014 규칙 4).
    /// - `sign_in_locked`: `POST /auth/password`가 실패를 거듭한 이메일을 잠깐 막는다.
    /// - `link_invalid`: `POST /auth/password/link`, `POST /auth/password/set`이 모르는 링크를 거절한다.
    const AUTH_ONLY: &[&str] = &["sign_in_locked", "link_invalid"];

    /// 원문에서 오류 코드를 직접 넘기는 곳을 찾을 파일. 시험 모듈은 빼고 읽는다.
    const SOURCES: &[(&str, &str)] = &[
        ("src/admin.rs", include_str!("admin.rs")),
        ("src/admin/guard.rs", include_str!("admin/guard.rs")),
        (
            "src/admin/management.rs",
            include_str!("admin/management.rs"),
        ),
        ("src/admin/password.rs", include_str!("admin/password.rs")),
        ("src/http.rs", include_str!("http.rs")),
    ];

    fn code(response: &Response) -> (u16, String) {
        let code = response
            .headers()
            .get("x-gateway-error-code")
            .and_then(|v| v.to_str().ok())
            .expect("오류 응답에는 x-gateway-error-code가 있다");
        (response.status().as_u16(), code.to_owned())
    }

    /// 모든 `AuthError`. 값이 늘면 아래 `match`가 컴파일되지 않으므로 목록도 함께 고친다.
    fn auth_errors() -> Vec<AuthError> {
        use AuthError::*;
        let all = vec![
            InvalidCredentials,
            AuthSourceForbidden,
            AccessDenied,
            OwnerInactive,
            DependencyUnavailable,
            DatabaseUnavailable,
            VirtualKeyRequired,
            ManagementTokenNotAllowed,
        ];
        for e in &all {
            match e {
                InvalidCredentials
                | AuthSourceForbidden
                | AccessDenied
                | OwnerInactive
                | DependencyUnavailable
                | DatabaseUnavailable
                | VirtualKeyRequired
                | ManagementTokenNotAllowed => {}
            }
        }
        all
    }

    /// 모든 `QueryError`. 값이 늘면 `match`가 컴파일되지 않는다.
    fn query_errors() -> Vec<crate::ledger::dashboard::QueryError> {
        use crate::ledger::dashboard::QueryError::{self, *};
        let all = vec![
            InvalidFilter,
            InvalidCursor,
            NotFound,
            AccessDenied,
            Database,
            Invariant,
            DetailUnavailable("2026-01-01T00:00:00.000000Z".into()),
        ];
        for e in &all {
            match e {
                QueryError::InvalidFilter
                | InvalidCursor
                | NotFound
                | AccessDenied
                | Database
                | Invariant
                | DetailUnavailable(_) => {}
            }
        }
        all
    }

    /// 모든 `ManagementError`. 값이 늘면 `match`가 컴파일되지 않는다.
    fn management_errors() -> Vec<ManagementError> {
        use ManagementError::*;
        let id = Uuid::nil();
        let mut all = vec![
            Invalid,
            Forbidden,
            NotFound,
            Database,
            Busy,
            Invariant,
            CommandConflict,
            Version(1),
            SecretUnavailable(id),
            Quota,
            LimitExceeded,
            PolicyPeriod,
            ConfigurationConflict,
            SecurityFence,
            AccessDuplicate,
            AccessProtected,
            SettlementConflict,
            ExecutionNotFinal,
            CredentialRejected,
            CredentialUnverified,
            CredentialDuplicate(id),
            CredentialLabelTaken,
            CredentialInUse { bound_keys: 1 },
            BindingConflict(None),
            AuthorizationConflict(vec![id]),
            AuthorizationInUse(1),
            CredentialNotAuthorized,
            CredentialRequired,
            CatalogSourcesUnavailable,
            CatalogProposalStale,
            TokenQuota,
            TokenSecretUnavailable(id),
        ];
        all.extend(query_errors().into_iter().map(Query));
        for e in &all {
            match e {
                Invalid
                | Forbidden
                | NotFound
                | Database
                | Busy
                | Invariant
                | CommandConflict
                | Version(_)
                | SecretUnavailable(_)
                | Quota
                | LimitExceeded
                | PolicyPeriod
                | ConfigurationConflict
                | SecurityFence
                | AccessDuplicate
                | AccessProtected
                | SettlementConflict
                | ExecutionNotFinal
                | Query(_)
                | CredentialRejected
                | CredentialUnverified
                | CredentialDuplicate(_)
                | CredentialLabelTaken
                | CredentialInUse { .. }
                | BindingConflict(_)
                | AuthorizationConflict(_)
                | AuthorizationInUse(_)
                | CredentialNotAuthorized
                | CredentialRequired
                | CatalogSourcesUnavailable
                | CatalogProposalStale
                | TokenQuota
                | TokenSecretUnavailable(_) => {}
            }
        }
        all
    }

    /// 모든 `AdmissionError`. 값이 늘면 `match`가 컴파일되지 않는다.
    fn admission_errors() -> Vec<AdmissionError> {
        use AdmissionError::*;
        let all = vec![
            NotReady,
            Draining,
            HttpCapacity,
            AuthenticationCapacity,
            Concurrency,
            InvalidTransition,
        ];
        for e in &all {
            match e {
                NotReady
                | Draining
                | HttpCapacity
                | AuthenticationCapacity
                | Concurrency
                | InvalidTransition => {}
            }
        }
        all
    }

    /// `failure(상태, "코드")`, `route_error(상태, "코드", …)`처럼 코드를 글자로 넘기는 곳을 원문에서 찾는다.
    /// 둘째 인자가 문자열일 때만 센다. 변수로 넘기는 곳은 위의 열거형 시험이 다룬다.
    fn literal_codes() -> Vec<(String, u16, String)> {
        let statuses = [
            ("BAD_REQUEST", StatusCode::BAD_REQUEST),
            ("UNAUTHORIZED", StatusCode::UNAUTHORIZED),
            ("FORBIDDEN", StatusCode::FORBIDDEN),
            ("NOT_FOUND", StatusCode::NOT_FOUND),
            ("METHOD_NOT_ALLOWED", StatusCode::METHOD_NOT_ALLOWED),
            ("REQUEST_TIMEOUT", StatusCode::REQUEST_TIMEOUT),
            ("CONFLICT", StatusCode::CONFLICT),
            ("PAYLOAD_TOO_LARGE", StatusCode::PAYLOAD_TOO_LARGE),
            ("UNSUPPORTED_MEDIA_TYPE", StatusCode::UNSUPPORTED_MEDIA_TYPE),
            ("UNPROCESSABLE_ENTITY", StatusCode::UNPROCESSABLE_ENTITY),
            ("TOO_MANY_REQUESTS", StatusCode::TOO_MANY_REQUESTS),
            (
                "REQUEST_HEADER_FIELDS_TOO_LARGE",
                StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            ),
            ("INTERNAL_SERVER_ERROR", StatusCode::INTERNAL_SERVER_ERROR),
            ("BAD_GATEWAY", StatusCode::BAD_GATEWAY),
            ("SERVICE_UNAVAILABLE", StatusCode::SERVICE_UNAVAILABLE),
        ];
        let mut found = Vec::new();
        for (file, text) in SOURCES {
            let text = text.split("#[cfg(test)]").next().unwrap_or_default();
            for call in ["failure(", "route_error("] {
                for (at, _) in text.match_indices(call) {
                    let before = text[..at].chars().next_back();
                    if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                        continue;
                    }
                    let args = arguments(&text[at + call.len()..]);
                    let Some(code) = args
                        .get(1)
                        .and_then(|a| a.strip_prefix('"'))
                        .and_then(|a| a.strip_suffix('"'))
                    else {
                        continue;
                    };
                    let name = args[0].trim_start_matches("StatusCode::");
                    let status = statuses
                        .iter()
                        .find(|(n, _)| *n == name)
                        .unwrap_or_else(|| panic!("{file}: 상태 `{}`를 표에 더한다", args[0]))
                        .1;
                    found.push((file.to_string(), status.as_u16(), code.to_owned()));
                }
            }
        }
        found
    }

    /// 여는 괄호 뒤의 글에서 맨 바깥 인자들을 잘라 낸다. 문자열 안의 괄호와 쉼표는 세지 않는다.
    fn arguments(text: &str) -> Vec<String> {
        let mut args = Vec::new();
        let mut current = String::new();
        let mut depth = 0;
        let mut quoted = false;
        let mut escaped = false;
        for c in text.chars() {
            if quoted {
                current.push(c);
                match (escaped, c) {
                    (true, _) => escaped = false,
                    (false, '\\') => escaped = true,
                    (false, '"') => quoted = false,
                    _ => {}
                }
                continue;
            }
            match c {
                '"' => {
                    quoted = true;
                    current.push(c);
                }
                '(' | '[' | '{' => {
                    depth += 1;
                    current.push(c);
                }
                ')' | ']' | '}' if depth > 0 => {
                    depth -= 1;
                    current.push(c);
                }
                ')' => break,
                ',' if depth == 0 => args.push(std::mem::take(&mut current).trim().to_owned()),
                _ => current.push(c),
            }
        }
        if !current.trim().is_empty() {
            args.push(current.trim().to_owned());
        }
        args
    }

    /// 문서가 이 상태로 선언한 오류 코드: 어느 operation의 `x-error-codes`이거나 `x-common-errors`.
    fn documented() -> BTreeSet<(u16, String)> {
        let doc: Value = serde_json::from_str(super::OPENAPI).expect("OpenAPI 문서는 JSON이다");
        let mut codes = BTreeSet::new();
        let mut add = |status: &str, list: &Value| {
            for c in list.as_array().into_iter().flatten() {
                codes.insert((
                    status.parse().expect("상태 코드"),
                    c.as_str().expect("코드").to_owned(),
                ));
            }
        };
        for (status, list) in doc["x-common-errors"].as_object().expect("x-common-errors") {
            add(status, list);
        }
        for item in doc["paths"].as_object().expect("paths").values() {
            for operation in item.as_object().expect("path item").values() {
                for (status, response) in operation["responses"].as_object().into_iter().flatten() {
                    add(status, &response["x-error-codes"]);
                }
            }
        }
        codes
    }

    #[tokio::test]
    async fn every_management_error_code_is_in_the_openapi_document() {
        let mut produced: Vec<(String, u16, String)> = Vec::new();
        for e in auth_errors() {
            // 가상 키가 필요하다는 답은 추론 경로만 낸다. 관리 API의 인증은 이 값을 내지 않는다.
            if e == AuthError::VirtualKeyRequired {
                continue;
            }
            let (s, c) = code(&crate::http::authentication_error(e));
            produced.push((format!("authentication_error({e:?})"), s, c));
        }
        for e in management_errors() {
            let label = format!("error({e:?})");
            let (s, c) = code(&error(e));
            produced.push((label, s, c));
        }
        for e in query_errors() {
            let label = format!("query_error({e:?})");
            let (s, c) = code(&query_error(e));
            produced.push((label, s, c));
        }
        for e in admission_errors() {
            let (s, c) = code(&crate::http::admission_error(e));
            produced.push((format!("admission_error({e:?})"), s, c));
        }
        let (s, c) = code(&crate::http::method_not_allowed());
        produced.push(("method_not_allowed()".into(), s, c));
        let (s, c) = code(&crate::http::route_not_found_for_test().await);
        produced.push(("route_not_found()".into(), s, c));
        let literals = literal_codes();
        for expected in [
            "origin_denied",
            "csrf_invalid",
            "body_too_large",
            "request_body_timeout",
            "unsupported_media_type",
            "header_limit",
            "mail_failed",
            "password_invalid",
            "current_password_invalid",
        ] {
            assert!(
                literals.iter().any(|(_, _, c)| c == expected),
                "원문 검색이 `{expected}`를 찾지 못했다"
            );
        }
        for (file, s, c) in literals {
            if AUTH_ONLY.contains(&c.as_str()) {
                continue;
            }
            produced.push((file, s, c));
        }
        let documented = documented();
        let missing: Vec<String> = produced
            .iter()
            .filter(|(_, s, c)| !documented.contains(&(*s, c.clone())))
            .map(|(from, s, c)| format!("{s} {c} ({from})"))
            .collect();
        assert!(
            missing.is_empty(),
            "OpenAPI 문서에 없는 오류 코드:\n{}",
            missing.join("\n")
        );
    }
}
