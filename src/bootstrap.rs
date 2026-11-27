use crate::{
    config::{StartupError, ValidatedConfig},
    http::{self, AppState},
    runtime::Runtime,
};
use axum::Router;
use std::sync::Arc;

#[derive(Clone, Copy)]
enum RequiredComponent {
    UiBundle,
    MigratedSchema,
    Authentication,
}

/// S1/S2/U0의 실제 조립 전에는 필수 구성요소의 누락을 그대로 유지한다.
/// 환경변수나 테스트 플래그로 이 목록을 지우는 경로는 없다.
pub struct Dependencies {
    missing: Vec<RequiredComponent>,
    authentication: Option<Arc<crate::auth::Authenticator>>,
    inference: Option<Arc<crate::gateway::InferenceGateway>>,
    dashboard: Option<Arc<crate::admin::DashboardApi>>,
    ui: Option<Arc<crate::dashboard::UiBundle>>,
    credentials: Option<Arc<crate::credentials::CredentialService>>,
    catalog: Option<Arc<crate::catalog::CatalogService>>,
    /// 토큰 대조와 사후 보정. Admin API 키가 있을 때만 있다.
    token_check: Option<Arc<crate::token_check::TokenCheck>>,
}
impl Dependencies {
    pub fn unassembled() -> Self {
        Self {
            missing: vec![
                RequiredComponent::UiBundle,
                RequiredComponent::MigratedSchema,
                RequiredComponent::Authentication,
            ],
            authentication: None,
            inference: None,
            dashboard: None,
            ui: None,
            credentials: None,
            catalog: None,
            token_check: None,
        }
    }
    pub fn with_token_check(mut self, token_check: Arc<crate::token_check::TokenCheck>) -> Self {
        self.token_check = Some(token_check);
        self
    }
    /// 상태 점검 주기마다 승인된 카탈로그 버전을 캐시에 반영한다. 준비 상태(ready)에는 영향을 주지 않는다.
    pub fn with_catalog(mut self, catalog: Arc<crate::catalog::CatalogService>) -> Self {
        self.catalog = Some(catalog);
        self
    }
    /// 상태 점검 주기마다 DB의 회사 키 변경을 캐시에 반영한다. 준비 상태(ready)에는 영향을 주지 않는다.
    pub fn with_credentials(
        mut self,
        credentials: Arc<crate::credentials::CredentialService>,
    ) -> Self {
        self.credentials = Some(credentials);
        self
    }
    pub fn with_inference(mut self, inference: Arc<crate::gateway::InferenceGateway>) -> Self {
        self.inference = Some(inference);
        self.missing
            .retain(|c| !matches!(c, RequiredComponent::MigratedSchema));
        self
    }
    pub async fn assemble_backend(config: &ValidatedConfig) -> Result<Self, StartupError> {
        let store = Arc::new(
            crate::ledger::PgIdentityStore::connect(config.database())
                .await
                .map_err(|_| StartupError::SchemaUnavailable)?,
        );
        store
            .migrate()
            .await
            .map_err(|_| StartupError::SchemaUnavailable)?;
        store
            .ensure_fixed_admin(&crate::auth::AccessSettings::from_config(config))
            .await
            .map_err(|_| StartupError::SchemaUnavailable)?;
        if config.auth_mode() == crate::config::AuthMode::Password {
            setup_link(config, &store).await?;
        }
        let auth = Arc::new(
            crate::auth::Authenticator::from_config(config, store.clone())
                .map_err(|_| StartupError::AuthenticationUnavailable)?,
        );
        let ledger = Arc::new(
            crate::ledger::PgLedger::new(store.clone(), crate::gateway::auth_config_digest(config))
                .with_ceiling(config.execution_ceiling().clone()),
        );
        let credentials = crate::credentials::CredentialService::new(config, store.clone())?;
        // 키가 하나도 없어도 기동한다(관리자가 대시보드에서 등록해야 하므로). DB 조회 실패만 기동 실패다.
        credentials
            .refresh()
            .await
            .map_err(|_| StartupError::SchemaUnavailable)?;
        let catalog = crate::catalog::CatalogService::new(config, store.clone())?;
        catalog
            .refresh()
            .await
            .map_err(|_| StartupError::SchemaUnavailable)?;
        let token_check =
            crate::token_check::TokenCheck::new(config, ledger.clone(), credentials.clone());
        let inference = Arc::new(
            crate::gateway::InferenceGateway::new(
                config,
                auth.clone(),
                ledger,
                credentials.cache(),
            )
            .map_err(|_| StartupError::InvalidProfile)?
            .with_catalog(catalog.cache()),
        );
        let dashboard = Arc::new({
            let api = crate::admin::DashboardApi::new(config, auth.clone(), store)
                .with_credentials(credentials.clone())
                .with_catalog(catalog.clone());
            match config.smtp() {
                Some(smtp) => api.with_mailer(Arc::new(
                    crate::auth::password::SmtpMailer::from_config(smtp, config.ca_certificate())?,
                )),
                None => api,
            }
        });
        let assembled = Self::unassembled()
            .with_dashboard(dashboard)
            .with_authentication(auth)
            .with_inference(inference)
            .with_credentials(credentials)
            .with_catalog(catalog);
        Ok(match token_check {
            Some(token_check) => assembled.with_token_check(token_check),
            None => assembled,
        })
    }
    pub fn with_ui(mut self, ui: Arc<crate::dashboard::UiBundle>) -> Self {
        self.ui = Some(ui);
        self.missing
            .retain(|c| !matches!(c, RequiredComponent::UiBundle));
        self
    }
    pub fn with_dashboard(mut self, dashboard: Arc<crate::admin::DashboardApi>) -> Self {
        self.dashboard = Some(dashboard);
        self
    }
    pub fn with_authentication(mut self, authentication: Arc<crate::auth::Authenticator>) -> Self {
        self.authentication = Some(authentication);
        self.missing
            .retain(|component| !matches!(component, RequiredComponent::Authentication));
        self
    }
}
pub struct RunningApp {
    config: ValidatedConfig,
    dependencies: Dependencies,
    pub state: AppState,
    router: Router,
}
pub fn build(
    config: ValidatedConfig,
    dependencies: Dependencies,
) -> Result<RunningApp, StartupError> {
    let runtime = Runtime::new(config.limits.clone())?;
    let state = AppState { runtime };
    let mut routes = Router::new();
    if let Some(api) = &dependencies.dashboard {
        routes = routes.merge(api.clone().routes(state.runtime.clone()));
    }
    if let Some(ui) = &dependencies.ui {
        routes = routes.merge(ui.clone().routes(state.runtime.clone()));
    }
    let router =
        http::router_with_components(state.clone(), dependencies.inference.clone(), routes);
    Ok(RunningApp {
        config,
        dependencies,
        state,
        router,
    })
}
impl RunningApp {
    pub fn router(&self) -> Router {
        self.router.clone()
    }
    pub fn authentication(&self) -> Option<&Arc<crate::auth::Authenticator>> {
        self.dependencies.authentication.as_ref()
    }
    pub fn config(&self) -> &ValidatedConfig {
        &self.config
    }
    pub fn runtime(&self) -> &Arc<Runtime> {
        &self.state.runtime
    }
    pub async fn refresh_health(&self) -> crate::runtime::HealthReport {
        let database = async {
            if let Some(g) = &self.dependencies.inference {
                g.ledger().database().health().await
            } else {
                (false, false)
            }
        };
        let jwks = async {
            if let Some(auth) = &self.dependencies.authentication {
                auth.refresh_health(&self.state.runtime).await
            } else {
                false
            }
        };
        let credentials = async {
            if let Some(c) = &self.dependencies.credentials {
                let _ = c.refresh().await;
            }
            if let Some(c) = &self.dependencies.catalog {
                let _ = c.refresh().await;
            }
        };
        let ((database, schema), jwks, ()) = tokio::join!(database, jwks, credentials);
        crate::runtime::HealthReport {
            database,
            schema,
            jwks,
            ui: self.dependencies.ui.is_some(),
        }
    }
    pub async fn run(
        self,
        address: std::net::SocketAddr,
        shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<(), StartupError> {
        self.validate_bootstrap()?;
        let report = self.refresh_health().await;
        if report.database && !report.schema {
            self.runtime().stop();
            return Err(StartupError::SchemaUnavailable);
        }
        let now = tokio::time::Instant::now();
        for (dependency, healthy) in [
            (crate::runtime::Dependency::Database, report.database),
            (crate::runtime::Dependency::Schema, report.schema),
            (crate::runtime::Dependency::Jwks, report.jwks),
            (crate::runtime::Dependency::Ui, report.ui),
        ] {
            self.runtime().record_health(dependency, healthy, now);
        }
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .map_err(|_| StartupError::HttpBind)?;
        self.runtime()
            .start_http()
            .map_err(|_| StartupError::RuntimeFailure)?;
        let runtime = self.state.runtime.clone();
        let monitor = runtime.clone().monitor(|| self.refresh_health());
        let serve = crate::server::serve(listener, self.router.clone(), runtime.clone(), shutdown);
        let reaper = async {
            if let Some(gateway) = &self.dependencies.inference {
                gateway.ledger().clone().run_reaper(runtime.clone()).await?;
            }
            Ok::<_, crate::runtime::AdmissionError>(())
        };
        let checking = async {
            if let Some(check) = &self.dependencies.token_check {
                check.clone().run(runtime.clone()).await?;
            }
            Ok::<_, crate::runtime::AdmissionError>(())
        };
        let (result, health, recovery, _) = tokio::join!(serve, monitor, reaper, checking);
        result.map_err(|_| StartupError::RuntimeFailure)?;
        recovery.map_err(|_| StartupError::RuntimeFailure)?;
        health.map_err(|_| StartupError::RuntimeFailure)
    }
    pub fn validate_bootstrap(&self) -> Result<(), StartupError> {
        if let Some(component) = self.dependencies.missing.first() {
            self.state.runtime.stop();
            return Err(match component {
                RequiredComponent::UiBundle => StartupError::UiUnavailable,
                RequiredComponent::MigratedSchema => StartupError::SchemaUnavailable,
                RequiredComponent::Authentication => StartupError::AuthenticationUnavailable,
            });
        }
        Ok(())
    }
}

/// 비밀번호 방식에서 고정 관리자에게 비밀번호가 없으면, 첫 관리자 설정 링크를 만들어 시작 로그에
/// 남긴다(결정 0011 규칙 8). 시작할 때마다 새 링크를 만들고, 예전 설정 링크는 만료될 때까지 유효하다.
async fn setup_link(
    config: &ValidatedConfig,
    store: &crate::ledger::PgIdentityStore,
) -> Result<(), StartupError> {
    use crate::auth::password;
    let email = config.admin_email();
    if store
        .password_hash(email)
        .await
        .map_err(|_| StartupError::SchemaUnavailable)?
        .is_some()
    {
        return Ok(());
    }
    let token = password::link_token().map_err(|_| StartupError::RuntimeFailure)?;
    store
        .create_link(
            email,
            password::LinkKind::Setup,
            &password::token_hash(&token),
            None,
        )
        .await
        .map_err(|_| StartupError::SchemaUnavailable)?;
    eprintln!(
        "{}",
        serde_json::json!({
            "warning": "first_admin_setup_link",
            "email": email,
            "valid_hours": 24,
            "url": password::link_url(config.public_origin().as_str(), &token),
        })
    );
    Ok(())
}
