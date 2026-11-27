use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};
use url::Url;

const MAX_CONFIG_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupError {
    ConfigInput,
    ConfigSyntax,
    InvalidAuth,
    InvalidUrl,
    InvalidIdentity,
    MissingSecret,
    InvalidSecret,
    InvalidDatabase,
    InvalidProfile,
    InvalidPrice,
    InvalidLimits,
    UiUnavailable,
    SchemaUnavailable,
    AuthenticationUnavailable,
    HttpBind,
    RuntimeFailure,
    MaintenanceFailure,
    RemovedProviderKeyEnv,
    /// WASM 어댑터 파일을 읽지 못했거나, 해시가 다르거나, 규칙에 맞지 않는다.
    InvalidAdapter,
    /// 이 인증 방식에 필요한 설정값이 없다. 값의 이름(환경 변수 이름)을 담는다.
    MissingValue(&'static str),
    /// 다른 인증 방식의 설정값이 있다(결정 0011 규칙 1). 값의 이름을 담는다.
    RefusedValue(&'static str),
    /// SMTP 설정이 규칙에 맞지 않는다.
    InvalidSmtp,
}
impl StartupError {
    pub fn code(self) -> &'static str {
        match self {
            Self::ConfigInput => "config_input_invalid",
            Self::ConfigSyntax => "config_syntax_invalid",
            Self::InvalidAuth => "config_auth_invalid",
            Self::InvalidUrl => "config_url_invalid",
            Self::InvalidIdentity => "config_identity_invalid",
            Self::MissingSecret => "config_secret_missing",
            Self::InvalidSecret => "config_secret_invalid",
            Self::InvalidDatabase => "config_database_invalid",
            Self::InvalidProfile => "config_profile_invalid",
            Self::InvalidPrice => "config_price_invalid",
            Self::InvalidLimits => "config_limits_invalid",
            Self::UiUnavailable => "ui_bundle_unavailable",
            Self::SchemaUnavailable => "schema_unavailable",
            Self::AuthenticationUnavailable => "authentication_unavailable",
            Self::HttpBind => "http_bind_failed",
            Self::RuntimeFailure => "runtime_failed",
            Self::MaintenanceFailure => "maintenance_failed",
            Self::RemovedProviderKeyEnv => "config_provider_key_env_removed",
            Self::InvalidAdapter => "config_adapter_invalid",
            Self::MissingValue(_) => "config_value_missing",
            Self::RefusedValue(_) => "config_value_refused",
            Self::InvalidSmtp => "config_smtp_invalid",
        }
    }
}
impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingValue(name) | Self::RefusedValue(name) => {
                write!(f, "{}: {name}", self.code())
            }
            _ => f.write_str(self.code()),
        }
    }
}
impl std::error::Error for StartupError {}

pub struct Secret(String);
impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum DurationPreset {
    #[serde(rename = "3_months")]
    ThreeMonths,
    #[serde(rename = "6_months")]
    SixMonths,
    #[serde(rename = "1_year")]
    OneYear,
    #[serde(rename = "3_years")]
    ThreeYears,
    #[serde(rename = "unlimited")]
    Unlimited,
}
impl DurationPreset {
    pub fn months(self) -> Option<u32> {
        match self {
            Self::ThreeMonths => Some(3),
            Self::SixMonths => Some(6),
            Self::OneYear => Some(12),
            Self::ThreeYears => Some(36),
            Self::Unlimited => None,
        }
    }
}

#[derive(Default, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Retention {
    #[serde(default, deserialize_with = "present_duration")]
    pub ledger: Option<DurationPreset>,
    #[serde(default, deserialize_with = "present_duration")]
    pub audit: Option<DurationPreset>,
    #[serde(default, deserialize_with = "present_duration")]
    pub commands: Option<DurationPreset>,
}
fn present_duration<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<DurationPreset>, D::Error> {
    DurationPreset::deserialize(d).map(Some)
}
impl Retention {
    /// 기간 선택은 삭제 승인이 아니다. 실제 삭제는 명시적 승인 입력을 받는 장부 명령이 담당한다.
    pub fn has_finite_selection(&self) -> bool {
        [self.ledger, self.audit, self.commands]
            .into_iter()
            .flatten()
            .any(|p| p.months().is_some())
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAuth {
    /// 로그인 방식(결정 0011). `sso` 또는 `password`.
    mode: Option<String>,
    issuer: Option<String>,
    jwks_uri: Option<String>,
    audience: Option<String>,
    access_cookie_name: Option<String>,
    login_url: Option<String>,
    refresh_url: Option<String>,
    logout_url: Option<String>,
    return_param: Option<String>,
}
/// `password` 방식에서 초대·재설정 메일을 보내는 SMTP 서버. 비밀번호는 `SMTP_PASSWORD`로만 받는다.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSmtp {
    host: Option<String>,
    port: Option<u16>,
    username: Option<String>,
    from: Option<String>,
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDatabase {
    host: Option<String>,
    port: Option<u16>,
    name: Option<String>,
    user: Option<String>,
    ssl_mode: Option<DbSslMode>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    auth: RawAuth,
    public_origin: Option<String>,
    /// 고정 관리자의 이메일(결정 0001). 다른 사람은 대시보드에서 접근 목록에 더한다.
    admin_email: Option<String>,
    #[serde(default)]
    smtp: RawSmtp,
    #[serde(default)]
    database: RawDatabase,
    providers: Vec<RawProvider>,
    #[serde(default)]
    limits: Limits,
    #[serde(default)]
    retention: Retention,
    #[serde(default)]
    execution_ceiling: crate::ledger::policy::PolicyLimits,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ProviderId {
    Anthropic,
    Openai,
    Deepseek,
    Typesafe,
}
impl ProviderId {
    pub const ALL: [Self; 4] = [
        Self::Anthropic,
        Self::Openai,
        Self::Deepseek,
        Self::Typesafe,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Openai => "openai",
            Self::Deepseek => "deepseek",
            Self::Typesafe => "typesafe",
        }
    }
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == name)
    }
}
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ApiKind {
    Messages,
    CountTokens,
    Responses,
    OpenaiChat,
    Embeddings,
    DeepseekChat,
    Systemone,
}
impl ApiKind {
    /// 설정 파일의 `api` 값. WASM 어댑터에도 이 이름을 넘긴다.
    pub fn name(self) -> &'static str {
        match self {
            Self::Messages => "messages",
            Self::CountTokens => "count_tokens",
            Self::Responses => "responses",
            Self::OpenaiChat => "openai_chat",
            Self::Embeddings => "embeddings",
            Self::DeepseekChat => "deepseek_chat",
            Self::Systemone => "systemone",
        }
    }
}
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BillingClass {
    #[default]
    Chargeable,
    GuaranteedFree,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProvider {
    provider: ProviderId,
    base_url: String,
    /// WASM 어댑터 컴포넌트 파일(결정 0009). `adapter_sha256`과 함께 쓴다.
    adapter: Option<String>,
    adapter_sha256: Option<String>,
    #[serde(default)]
    profiles: Vec<ModelProfile>,
    catalog: Option<CatalogTemplate>,
}
/// 모델·가격을 DB 카탈로그에서 관리하는 공급사의 공통 정책.
/// 모델별 값(이름·가격·토큰 한도)은 관리자가 공개 가격 목록에서 동기화한 뒤 승인한다.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogTemplate {
    pub api: ApiKind,
    pub fixture_version: String,
    #[serde(default)]
    pub features: Vec<NativeFeature>,
    #[serde(default)]
    pub approved_beta: Vec<String>,
    pub chat_output_limit_field: Option<String>,
    pub max_body_bytes: usize,
    pub max_response_bytes: usize,
    /// 동기화 때 새 모델 후보로 보여 줄 모델 이름 접두사(예: `claude-`).
    pub model_prefixes: Vec<String>,
}
impl CatalogTemplate {
    /// 카탈로그 항목 하나를 일반 profile 로 만든다. 검증은 설정 파일 profile 과 같은 규칙을 쓴다.
    pub fn profile(
        &self,
        model: &str,
        version: i64,
        rates: BTreeMap<String, String>,
        max_input_tokens: u64,
        max_output_tokens: u64,
    ) -> ModelProfile {
        ModelProfile {
            model: model.to_owned(),
            api: self.api,
            profile_version: format!("catalog-v{version}"),
            pricing_version: format!("catalog-v{version}"),
            fixture_version: self.fixture_version.clone(),
            actual_models: vec![model.to_owned()],
            features: self.features.clone(),
            approved_beta: self.approved_beta.clone(),
            billing_class: BillingClass::Chargeable,
            count_billing: None,
            chat_output_limit_field: self.chat_output_limit_field.clone(),
            embedding_dimensions: None,
            embedding_encodings: Vec::new(),
            max_input_tokens,
            max_output_tokens: Some(max_output_tokens),
            max_body_bytes: self.max_body_bytes,
            max_response_bytes: self.max_response_bytes,
            rates,
        }
    }
    /// 이 정책으로 profile 을 만들 때 필요한 가격 항목.
    pub fn required_rates(&self, provider: ProviderId) -> Vec<&'static str> {
        let mut keys = vec!["input", "output"];
        if self.features.contains(&NativeFeature::Cache) {
            keys.push("cache_read");
            keys.extend(crate::providers::port::provider(provider).cache_write_rate());
        }
        keys
    }
    fn validate(&self, provider: ProviderId, limits: &Limits) -> Result<(), StartupError> {
        if !crate::providers::port::api(provider, self.api).is_some_and(|api| api.catalog())
            || self.model_prefixes.is_empty()
            || self.model_prefixes.len() > 16
            || self.model_prefixes.iter().any(|p| {
                p.is_empty()
                    || p.len() > 64
                    || !p.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b"-._".contains(&b)
                    })
            })
        {
            return Err(StartupError::InvalidProfile);
        }
        let rates = self
            .required_rates(provider)
            .into_iter()
            .map(|k| (k.to_owned(), "0.000000000000".to_owned()))
            .collect();
        self.profile("catalog-template-check", 1, rates, 1, 1)
            .validate(provider, limits)
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum NativeFeature {
    ClientTools,
    Thinking,
    Cache,
    JsonSchema,
    InlinePng,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingDimensions {
    pub default: u32,
    pub max: u32,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProfile {
    pub model: String,
    pub api: ApiKind,
    pub profile_version: String,
    pub pricing_version: String,
    pub fixture_version: String,
    pub actual_models: Vec<String>,
    #[serde(default)]
    pub features: Vec<NativeFeature>,
    #[serde(default)]
    pub approved_beta: Vec<String>,
    #[serde(default)]
    pub billing_class: BillingClass,
    pub count_billing: Option<String>,
    pub chat_output_limit_field: Option<String>,
    pub embedding_dimensions: Option<EmbeddingDimensions>,
    #[serde(default)]
    pub embedding_encodings: Vec<String>,
    pub max_input_tokens: u64,
    pub max_output_tokens: Option<u64>,
    pub max_body_bytes: usize,
    pub max_response_bytes: usize,
    #[serde(deserialize_with = "strict_rates")]
    pub rates: BTreeMap<String, String>,
}
#[derive(Debug)]
pub struct ProviderConfig {
    pub provider: ProviderId,
    pub base_url: Url,
    /// 본체 어댑터 대신 쓰는 WASM 어댑터 컴포넌트.
    pub adapter: Option<AdapterFile>,
    pub profiles: Vec<ModelProfile>,
    /// 있으면 이 공급사의 모델·가격은 DB 카탈로그에서 온다(`profiles` 는 비어 있어야 한다).
    pub catalog: Option<CatalogTemplate>,
}
/// WASM 어댑터 컴포넌트 파일과 그 SHA-256. 해시가 다르면 시작하지 않는다.
#[derive(Clone, Debug)]
pub struct AdapterFile {
    pub path: String,
    pub sha256: [u8; 32],
}
impl AdapterFile {
    fn parse(path: Option<String>, sha256: Option<String>) -> Result<Option<Self>, StartupError> {
        match (path, sha256) {
            (None, None) => Ok(None),
            (Some(path), Some(hex)) => {
                let mut sha256 = [0u8; 32];
                if path.is_empty()
                    || hex.len() != 64
                    || !hex
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(StartupError::InvalidAdapter);
                }
                for (i, byte) in sha256.iter_mut().enumerate() {
                    *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                        .map_err(|_| StartupError::InvalidAdapter)?;
                }
                Ok(Some(Self { path, sha256 }))
            }
            _ => Err(StartupError::InvalidAdapter),
        }
    }
}
pub struct AuthConfig {
    pub issuer_literal: String,
    pub audience_literal: String,
    pub issuer: Url,
    pub jwks_uri: Url,
    pub audience: Url,
    pub access_cookie_name: String,
    pub login_url: Url,
    pub refresh_url: Url,
    pub logout_url: Url,
    /// 로그인·로그아웃 뒤 돌아올 주소를 SSO에 넘기는 query 이름(예: `return_to`). 없으면 붙이지 않는다.
    pub return_param: Option<String>,
}
impl AuthConfig {
    fn with_return(&self, url: &Url, origin: &Url) -> String {
        let mut url = url.clone();
        if let Some(param) = &self.return_param {
            url.query_pairs_mut().append_pair(
                param,
                &format!("{}/dashboard/", origin.as_str().trim_end_matches('/')),
            );
        }
        url.to_string()
    }
}
/// 로그인 방식(결정 0011). 게이트웨이마다 하나만 쓴다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMode {
    Sso,
    Password,
}
/// 초대·재설정 메일을 보내는 SMTP 서버. 포트 465는 처음부터 TLS, 그 밖의 포트는 STARTTLS가 필수다.
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Secret,
    /// 보내는 주소(소문자 이메일).
    pub from: String,
}
/// Postgres TLS 수준. 평문 연결은 선택지에 없다.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
pub enum DbSslMode {
    /// 인증서 체인과 hostname까지 검증한다(libpq `verify-full`).
    #[default]
    #[serde(rename = "verify-full")]
    VerifyFull,
    /// 암호화만 요구하고 인증서는 검증하지 않는다(libpq `require`).
    /// 서버가 TLS를 거부하면 연결하지 않는다.
    #[serde(rename = "require")]
    Require,
}
impl DbSslMode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "verify-full" => Some(Self::VerifyFull),
            "require" => Some(Self::Require),
            _ => None,
        }
    }
}
pub struct DatabaseConfig {
    pub ca_certificate: Option<Vec<u8>>,
    pub ssl_mode: DbSslMode,
    pub host: String,
    pub port: u16,
    pub name: String,
    pub user: String,
    pub password: Secret,
}
pub struct ValidatedConfig {
    pub(crate) auth_mode: AuthMode,
    /// SSO 설정. `sso` 방식에서만 있다.
    pub(crate) auth: Option<AuthConfig>,
    /// SMTP 설정. `password` 방식에서만 있다.
    pub(crate) smtp: Option<SmtpConfig>,
    /// `TLS_CA_FILE`의 추가 신뢰 인증서. Postgres, JWKS, Provider 연결이 함께 쓴다.
    pub(crate) ca_certificate: Option<Vec<u8>>,
    pub(crate) public_origin: Url,
    pub(crate) admin_email: String,
    pub(crate) database: DatabaseConfig,
    pub(crate) csrf_signing_key: Secret,
    pub(crate) credential_encryption_key: Secret,
    /// 토큰 대조와 사후 보정에 쓰는 Anthropic Admin API 키(선택). 없으면 두 기능이 꺼진다.
    pub(crate) anthropic_admin_api_key: Option<Secret>,
    pub(crate) providers: Vec<ProviderConfig>,
    pub(crate) limits: Limits,
    pub(crate) retention: Retention,
    pub(crate) execution_ceiling: crate::ledger::policy::PolicyLimits,
}
impl ValidatedConfig {
    pub fn auth_mode(&self) -> AuthMode {
        self.auth_mode
    }
    /// SSO 설정. `password` 방식에서는 없다.
    pub fn sso(&self) -> Option<&AuthConfig> {
        self.auth.as_ref()
    }
    /// SMTP 설정. `sso` 방식에서는 없다.
    pub fn smtp(&self) -> Option<&SmtpConfig> {
        self.smtp.as_ref()
    }
    pub fn ca_certificate(&self) -> Option<&[u8]> {
        self.ca_certificate.as_deref()
    }
    pub fn public_origin(&self) -> &Url {
        &self.public_origin
    }
    /// 고정 관리자의 이메일. 시작할 때마다 접근 목록에 관리자로 넣는다.
    pub fn admin_email(&self) -> &str {
        &self.admin_email
    }
    pub fn database(&self) -> &DatabaseConfig {
        &self.database
    }
    pub fn csrf_signing_key(&self) -> &Secret {
        &self.csrf_signing_key
    }
    /// 토큰 대조와 사후 보정에 쓰는 Anthropic Admin API 키. 설정하지 않았으면 None이다.
    pub fn anthropic_admin_api_key(&self) -> Option<&Secret> {
        self.anthropic_admin_api_key.as_ref()
    }
    pub fn csrf_signing_key_bytes(&self) -> [u8; 32] {
        hex32(self.csrf_signing_key.expose())
    }
    /// Provider 키를 DB에 암호화해 두는 root key. 이 값에서 용도별 key를 파생한다.
    pub fn credential_encryption_key_bytes(&self) -> [u8; 32] {
        hex32(self.credential_encryption_key.expose())
    }
    /// 로그인 화면 주소. SSO 방식은 SSO 로그인 화면이고, `auth.return_param`이 있으면 대시보드로
    /// 돌아올 주소를 붙인다. 비밀번호 방식은 대시보드의 로그인 화면이다.
    pub fn login_redirect(&self) -> String {
        match &self.auth {
            Some(auth) => auth.with_return(&auth.login_url, &self.public_origin),
            None => "/dashboard/signin".to_owned(),
        }
    }
    /// 로그아웃 뒤 갈 주소. SSO 방식은 SSO 로그아웃 주소이고, 비밀번호 방식은 대시보드의 로그인 화면이다.
    pub fn logout_redirect(&self) -> String {
        match &self.auth {
            Some(auth) => auth.with_return(&auth.logout_url, &self.public_origin),
            None => "/dashboard/signin".to_owned(),
        }
    }
    pub fn providers(&self) -> &[ProviderConfig] {
        &self.providers
    }
    pub fn limits(&self) -> &Limits {
        &self.limits
    }
    pub fn execution_ceiling(&self) -> &crate::ledger::policy::PolicyLimits {
        &self.execution_ceiling
    }
    pub fn retention(&self) -> &Retention {
        &self.retention
    }
}
impl fmt::Debug for ValidatedConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ValidatedConfig([REDACTED])")
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub connections: usize,
    pub http2_streams: u32,
    pub http_requests: usize,
    pub inference: usize,
    pub per_principal: usize,
    pub pre_auth: usize,
    pub authentication: usize,
    pub nonstream: usize,
    pub management: usize,
    pub management_writes: usize,
    pub assets: usize,
    pub header_bytes: usize,
    pub header_count: usize,
    pub token_bytes: usize,
    pub header_timeout_ms: u64,
    pub inference_body_bytes: usize,
    pub management_body_bytes: usize,
    pub body_timeout_ms: u64,
    pub body_idle_timeout_ms: u64,
    pub management_timeout_ms: u64,
    pub asset_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub transport_timeout_ms: u64,
    pub finalization_timeout_ms: u64,
    pub health_interval_ms: u64,
    pub health_valid_ms: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            connections: 64,
            http2_streams: 16,
            http_requests: 40,
            inference: 16,
            per_principal: 4,
            pre_auth: 4,
            authentication: 4,
            nonstream: 4,
            management: 4,
            management_writes: 1,
            assets: 8,
            header_bytes: 32768,
            header_count: 64,
            token_bytes: 8192,
            header_timeout_ms: 5000,
            inference_body_bytes: 8 * 1024 * 1024,
            management_body_bytes: 64 * 1024,
            body_timeout_ms: 30000,
            body_idle_timeout_ms: 5000,
            management_timeout_ms: 10000,
            asset_timeout_ms: 30000,
            request_timeout_ms: 900000,
            transport_timeout_ms: 895000,
            finalization_timeout_ms: 5000,
            health_interval_ms: 5000,
            health_valid_ms: 10000,
        }
    }
}

pub struct ConfigInputs<'a> {
    pub yaml: &'a str,
    pub file_environment: Option<&'a str>,
    pub environment: &'a BTreeMap<String, String>,
}

pub fn parse_non_secret_env(input: &str) -> Result<BTreeMap<String, String>, StartupError> {
    if input.len() > 64 * 1024 {
        return Err(StartupError::ConfigInput);
    }
    let mut values = BTreeMap::new();
    for line in input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let (key, value) = line.split_once('=').ok_or(StartupError::ConfigSyntax)?;
        if !ENV_KEYS.contains(&key)
            || key.ends_with("API_KEY")
            || ["DB_PASSWORD", "CSRF_SIGNING_KEY", "SMTP_PASSWORD"].contains(&key)
            || value.contains(['$', '`', '\'', '"'])
            || value.chars().any(char::is_control)
            || value != value.trim()
            || values.contains_key(key)
        {
            return Err(StartupError::ConfigSyntax);
        }
        values.insert(key.to_owned(), value.to_owned());
    }
    Ok(values)
}

/// 실제 환경 조회는 main의 명시 allowlist만 담당한다. 단위 시험은 합성 map을 전달한다.
pub fn load(inputs: ConfigInputs<'_>) -> Result<ValidatedConfig, StartupError> {
    check_yaml_budget(inputs.yaml)?;
    // Value의 mapping 중복 검사도 통과시켜 rates 같은 map의 조용한 덮어쓰기를 막는다.
    let value: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(inputs.yaml).map_err(|_| StartupError::ConfigSyntax)?;
    let raw: RawConfig =
        serde_yaml_ng::from_value(value).map_err(|_| StartupError::ConfigSyntax)?;
    if REMOVED_ENV_KEYS
        .iter()
        .any(|k| inputs.environment.contains_key(*k))
    {
        return Err(StartupError::RemovedProviderKeyEnv);
    }
    let mut values = parse_non_secret_env(inputs.file_environment.unwrap_or(""))?;
    values.extend(
        inputs
            .environment
            .iter()
            .filter(|(k, _)| ENV_KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone())),
    );
    validate(raw, &values)
}

fn validate(
    raw: RawConfig,
    env: &BTreeMap<String, String>,
) -> Result<ValidatedConfig, StartupError> {
    // 필수값이 없으면 그 이름을 오류에 담는다.
    let read = |key: &'static str, value: Option<String>| {
        env.get(key)
            .cloned()
            .or(value)
            .ok_or(StartupError::MissingValue(key))
    };
    let parse = |key, value| -> Result<Url, StartupError> { parse_url(&read(key, value)?) };
    let ca_certificate = env
        .get("TLS_CA_FILE")
        .map(|path| {
            use std::io::Read;
            let file = std::fs::File::open(path).map_err(|_| StartupError::InvalidSecret)?;
            let mut bytes = Vec::new();
            file.take(65537)
                .read_to_end(&mut bytes)
                .map_err(|_| StartupError::InvalidSecret)?;
            if bytes.len() > 65536 || reqwest::Certificate::from_pem(&bytes).is_err() {
                return Err(StartupError::InvalidSecret);
            }
            Ok(bytes)
        })
        .transpose()?;
    // 로그인 방식(결정 0011 규칙 1). 다른 방식의 값이 하나라도 있으면 시작하지 않는다.
    let auth_mode = match read("AUTH_MODE", raw.auth.mode.clone())?.as_str() {
        "sso" => AuthMode::Sso,
        "password" => AuthMode::Password,
        _ => return Err(StartupError::InvalidAuth),
    };
    let sso_values = [
        ("AUTH_ISSUER", raw.auth.issuer.is_some()),
        ("AUTH_JWKS_URI", raw.auth.jwks_uri.is_some()),
        ("SSO_AUDIENCE", raw.auth.audience.is_some()),
        (
            "SSO_ACCESS_COOKIE_NAME",
            raw.auth.access_cookie_name.is_some(),
        ),
        ("SSO_LOGIN_URL", raw.auth.login_url.is_some()),
        ("SSO_REFRESH_URL", raw.auth.refresh_url.is_some()),
        ("SSO_LOGOUT_URL", raw.auth.logout_url.is_some()),
        ("SSO_RETURN_PARAM", raw.auth.return_param.is_some()),
    ];
    let smtp_values = [
        ("SMTP_HOST", raw.smtp.host.is_some()),
        ("SMTP_PORT", raw.smtp.port.is_some()),
        ("SMTP_USERNAME", raw.smtp.username.is_some()),
        ("SMTP_FROM", raw.smtp.from.is_some()),
        ("SMTP_PASSWORD", false),
    ];
    let other = match auth_mode {
        AuthMode::Sso => &smtp_values[..],
        AuthMode::Password => &sso_values[..],
    };
    if let Some((name, _)) = other
        .iter()
        .find(|(name, in_file)| *in_file || env.contains_key(*name))
    {
        return Err(StartupError::RefusedValue(name));
    }
    let auth = match auth_mode {
        AuthMode::Password => None,
        AuthMode::Sso => {
            let issuer_literal = read("AUTH_ISSUER", raw.auth.issuer)?;
            let audience_literal = read("SSO_AUDIENCE", raw.auth.audience)?;
            Some(AuthConfig {
                issuer: parse_url(&issuer_literal)?,
                issuer_literal,
                audience: parse_url(&audience_literal)?,
                audience_literal,
                jwks_uri: parse("AUTH_JWKS_URI", raw.auth.jwks_uri)?,
                access_cookie_name: read("SSO_ACCESS_COOKIE_NAME", raw.auth.access_cookie_name)?,
                login_url: parse("SSO_LOGIN_URL", raw.auth.login_url)?,
                refresh_url: parse("SSO_REFRESH_URL", raw.auth.refresh_url)?,
                logout_url: parse("SSO_LOGOUT_URL", raw.auth.logout_url)?,
                return_param: env
                    .get("SSO_RETURN_PARAM")
                    .cloned()
                    .or(raw.auth.return_param),
            })
        }
    };
    let smtp = match auth_mode {
        AuthMode::Sso => None,
        AuthMode::Password => Some(SmtpConfig {
            host: read("SMTP_HOST", raw.smtp.host)?,
            port: read("SMTP_PORT", raw.smtp.port.map(|p| p.to_string()))?
                .parse()
                .map_err(|_| StartupError::InvalidSmtp)?,
            username: read("SMTP_USERNAME", raw.smtp.username)?,
            password: secret(env, "SMTP_PASSWORD").map_err(|e| match e {
                StartupError::MissingSecret => StartupError::MissingValue("SMTP_PASSWORD"),
                e => e,
            })?,
            from: read("SMTP_FROM", raw.smtp.from)?,
        }),
    };
    let mut config = ValidatedConfig {
        auth_mode,
        auth,
        smtp,
        ca_certificate: ca_certificate.clone(),
        public_origin: parse("PUBLIC_ORIGIN", raw.public_origin)?,
        admin_email: read("ADMIN_EMAIL", raw.admin_email)?,
        database: DatabaseConfig {
            ca_certificate,
            host: read("DB_HOST", raw.database.host)?,
            port: read("DB_PORT", raw.database.port.map(|p| p.to_string()))?
                .parse()
                .map_err(|_| StartupError::InvalidDatabase)?,
            name: read("DB_NAME", raw.database.name)?,
            user: read("DB_USER", raw.database.user)?,
            ssl_mode: match env.get("DB_SSL_MODE") {
                Some(value) => DbSslMode::parse(value).ok_or(StartupError::InvalidDatabase)?,
                None => raw.database.ssl_mode.unwrap_or_default(),
            },
            password: secret(env, "DB_PASSWORD")?,
        },
        csrf_signing_key: secret(env, "CSRF_SIGNING_KEY")?,
        credential_encryption_key: secret(env, "CREDENTIAL_ENCRYPTION_KEY")?,
        anthropic_admin_api_key: admin_api_key(env)?,
        providers: raw
            .providers
            .into_iter()
            .map(|p| {
                Ok(ProviderConfig {
                    provider: p.provider,
                    base_url: parse_url(&p.base_url)?,
                    adapter: AdapterFile::parse(p.adapter, p.adapter_sha256)?,
                    profiles: p.profiles,
                    catalog: p.catalog,
                })
            })
            .collect::<Result<_, StartupError>>()?,
        limits: raw.limits,
        retention: raw.retention,
        execution_ceiling: raw.execution_ceiling,
    };
    config.validate()?;
    Ok(config)
}
fn secret(env: &BTreeMap<String, String>, name: &str) -> Result<Secret, StartupError> {
    let value = env
        .get(name)
        .filter(|s| !s.is_empty())
        .ok_or(StartupError::MissingSecret)?;
    if value.len() > 8192 || value.chars().any(char::is_control) {
        return Err(StartupError::InvalidSecret);
    }
    if ["CSRF_SIGNING_KEY", "CREDENTIAL_ENCRYPTION_KEY"].contains(&name)
        && (value.len() != 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
    {
        return Err(StartupError::InvalidSecret);
    }
    // 두 key의 수명이 섞이지 않게 같은 값을 거부한다(CSRF key 교체가 저장 키를 잃게 하지 않도록).
    if name == "CREDENTIAL_ENCRYPTION_KEY" && env.get("CSRF_SIGNING_KEY") == Some(value) {
        return Err(StartupError::InvalidSecret);
    }
    Ok(Secret(value.clone()))
}
/// 선택 비밀값 `ANTHROPIC_ADMIN_API_KEY`. 비어 있으면 없는 것으로 본다.
/// 일반 API 키를 잘못 넣지 않게 Admin API 키의 머리(`sk-ant-admin01-`)를 확인한다.
fn admin_api_key(env: &BTreeMap<String, String>) -> Result<Option<Secret>, StartupError> {
    let Some(value) = env.get("ANTHROPIC_ADMIN_API_KEY").filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    // 앞부분만 있는 값은 받지 않는다. 본문 길이는 문서에 없어 16자 이상만 요구한다.
    if value.len() > 256
        || value.len() < "sk-ant-admin01-".len() + 16
        || !value.starts_with("sk-ant-admin01-")
        || !value.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(StartupError::InvalidSecret);
    }
    Ok(Some(Secret(value.clone())))
}
fn hex32(encoded: &str) -> [u8; 32] {
    std::array::from_fn(|i| {
        u8::from_str_radix(&encoded[i * 2..i * 2 + 2], 16).expect("검증된 64자리 lowercase hex")
    })
}

/// 환경 전체나 .env 파일을 읽지 않는다. 비밀도 이 목록의 주입값만 받는다.
pub const ENV_KEYS: &[&str] = &[
    "TLS_CA_FILE",
    "AUTH_ISSUER",
    "AUTH_JWKS_URI",
    "SSO_AUDIENCE",
    "SSO_ACCESS_COOKIE_NAME",
    "SSO_LOGIN_URL",
    "SSO_REFRESH_URL",
    "SSO_LOGOUT_URL",
    "SSO_RETURN_PARAM",
    "AUTH_MODE",
    "SMTP_HOST",
    "SMTP_PORT",
    "SMTP_USERNAME",
    "SMTP_FROM",
    "PUBLIC_ORIGIN",
    "ADMIN_EMAIL",
    "DB_HOST",
    "DB_PORT",
    "DB_NAME",
    "DB_USER",
    "DB_SSL_MODE",
    "DB_PASSWORD",
    "CSRF_SIGNING_KEY",
    "CREDENTIAL_ENCRYPTION_KEY",
    "ANTHROPIC_ADMIN_API_KEY",
    "SMTP_PASSWORD",
];
/// Provider 키는 DB에서만 관리한다. 예전 환경변수가 남아 있으면 조용히 무시하지 않고 기동을 거부한다.
pub const REMOVED_ENV_KEYS: &[&str] = &["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "DEEPSEEK_API_KEY"];

impl ValidatedConfig {
    fn validate_sso(auth: &AuthConfig) -> Result<(), StartupError> {
        for url in [
            &auth.issuer,
            &auth.jwks_uri,
            &auth.audience,
            &auth.login_url,
            &auth.refresh_url,
            &auth.logout_url,
        ] {
            secure_url(url)?;
        }
        if auth.issuer.path() != "/" || auth.audience.path() != "/" {
            return Err(StartupError::InvalidUrl);
        }
        // JWT의 iss/aud는 설정 문자열과 그대로 비교한다. URL 정규화로 다른 값이 같아지지 않도록
        // 설정값은 정규 형태(소문자 host, 끝 슬래시 없음, percent-encoding 없음)여야 한다.
        if auth.issuer_literal != auth.issuer.as_str().trim_end_matches('/')
            || auth.audience_literal != auth.audience.as_str().trim_end_matches('/')
            || !cookie_name_valid(&auth.access_cookie_name)
            || auth.return_param.as_ref().is_some_and(|p| {
                p.is_empty()
                    || p.len() > 64
                    || !p
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            })
        {
            return Err(StartupError::InvalidAuth);
        }
        // 로그인·갱신·로그아웃·JWKS는 issuer와 같은 origin이어야 한다. CSP connect-src가 SSO origin 하나만 허용한다.
        for url in [
            &auth.jwks_uri,
            &auth.login_url,
            &auth.refresh_url,
            &auth.logout_url,
        ] {
            if url.origin() != auth.issuer.origin() {
                return Err(StartupError::InvalidAuth);
            }
        }
        Ok(())
    }
    /// SMTP 서버 이름·포트·계정·보내는 주소를 검사한다. 보내는 주소는 소문자로 바꾼다.
    fn validate_smtp(smtp: &mut SmtpConfig) -> Result<(), StartupError> {
        if url::Host::parse(&smtp.host).is_err()
            || smtp.host.len() > 253
            || smtp.port == 0
            || smtp.username.is_empty()
            || smtp.username.len() > 256
            || smtp.username.chars().any(char::is_control)
        {
            return Err(StartupError::InvalidSmtp);
        }
        smtp.from = email_value(&smtp.from).map_err(|_| StartupError::InvalidSmtp)?;
        Ok(())
    }
    fn validate(&mut self) -> Result<(), StartupError> {
        secure_url(&self.public_origin)?;
        if self.public_origin.path() != "/" {
            return Err(StartupError::InvalidUrl);
        }
        if let Some(auth) = &self.auth {
            Self::validate_sso(auth)?;
        }
        if let Some(smtp) = &mut self.smtp {
            Self::validate_smtp(smtp)?;
        }
        self.admin_email = email_value(&self.admin_email)?;
        let db = &self.database;
        if db.port == 0
            || url::Host::parse(&db.host).is_err()
            || db.host.len() > 253
            || db
                .host
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b"/@?#=\\".contains(&b))
            || [&db.name, &db.user].into_iter().any(|v| {
                v.is_empty()
                    || v.len() > 63
                    || !v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            })
        {
            return Err(StartupError::InvalidDatabase);
        }
        self.limits.validate()?;
        // 예산 기간의 시간대는 관리자 화면의 설정 하나로 정한다. 설정 파일에는 두지 않는다.
        if !self.execution_ceiling.without_timezone() {
            return Err(StartupError::InvalidLimits);
        }
        self.execution_ceiling
            .validate()
            .map_err(|_| StartupError::InvalidLimits)?;
        if self.providers.is_empty() || self.providers.len() > ProviderId::ALL.len() {
            return Err(StartupError::InvalidProfile);
        }
        let mut providers = BTreeSet::new();
        for provider in &self.providers {
            // WASM 어댑터를 먼저 설치한다. 그 뒤의 검사는 설치한 어댑터를 거친다.
            // 한 프로세스는 Provider마다 구현을 하나만 쓴다(결정 0009).
            match &provider.adapter {
                Some(a) => crate::providers::port::install(provider.provider, a.sha256, || {
                    crate::providers::wasm::load(provider.provider, &a.path, a.sha256)
                })?,
                None if crate::providers::port::installed(provider.provider) => {
                    return Err(StartupError::InvalidAdapter);
                }
                None => {}
            }
            secure_url(&provider.base_url)?;
            if provider.base_url.as_str()
                != format!(
                    "{}/",
                    crate::providers::port::provider(provider.provider).origin()
                )
                || !providers.insert(provider.provider)
                || provider.profiles.is_empty() == provider.catalog.is_none()
                || provider.profiles.len() > 64
            {
                return Err(StartupError::InvalidProfile);
            }
            if let Some(template) = &provider.catalog {
                template.validate(provider.provider, &self.limits)?;
            }
            let mut profiles = BTreeSet::new();
            for profile in &provider.profiles {
                profile.validate(provider.provider, &self.limits)?;
                if !profiles.insert((&profile.model, profile.api)) {
                    return Err(StartupError::InvalidProfile);
                }
            }
        }
        Ok(())
    }
}
fn parse_url(value: &str) -> Result<Url, StartupError> {
    if value
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '@' || c == '\\')
    {
        return Err(StartupError::InvalidUrl);
    }
    let url = Url::parse(value).map_err(|_| StartupError::InvalidUrl)?;
    if value != url.as_str() && Some(value) != url.as_str().strip_suffix('/') {
        return Err(StartupError::InvalidUrl);
    }
    Ok(url)
}
/// RFC 6265 cookie-name(token) 문자만 허용한다.
fn cookie_name_valid(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}
fn secure_url(url: &Url) -> Result<(), StartupError> {
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
        || url.port().is_some()
        || url.host_str().is_none()
    {
        return Err(StartupError::InvalidUrl);
    }
    Ok(())
}
/// 이메일 주소 하나를 검사하고 소문자로 바꾼다. 설정과 접근 목록이 같은 규칙을 쓴다.
pub(crate) fn email_value(value: &str) -> Result<String, StartupError> {
    {
        let value = value.trim();
        let Some((local, domain)) = value.split_once('@') else {
            return Err(StartupError::InvalidIdentity);
        };
        if value.len() > 320
            || local.is_empty()
            || domain.is_empty()
            || value
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || "*,".contains(c))
            || domain.contains('@')
            || url::Host::parse(domain).is_err()
        {
            return Err(StartupError::InvalidIdentity);
        }
        Ok(value.to_lowercase())
    }
}
impl ModelProfile {
    pub(crate) fn validate(
        &self,
        provider: ProviderId,
        limits: &Limits,
    ) -> Result<(), StartupError> {
        let api = crate::providers::port::api(provider, self.api);
        let allowed = api.is_some();
        let bounded_text =
            |s: &str, max| !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control);
        let count = self.api == ApiKind::CountTokens;
        if (self.api == ApiKind::OpenaiChat
            && self.chat_output_limit_field.as_deref() == Some("max_tokens")
            && self.features.contains(&NativeFeature::Thinking))
            || (self.api == ApiKind::OpenaiChat
                && !matches!(
                    self.chat_output_limit_field.as_deref(),
                    Some("max_tokens" | "max_completion_tokens")
                ))
            || (self.api != ApiKind::OpenaiChat && self.chat_output_limit_field.is_some())
            || (self.api == ApiKind::Embeddings
                && (self
                    .embedding_dimensions
                    .as_ref()
                    .is_none_or(|d| d.default == 0 || d.default > d.max || d.max > 4096)
                    || self.embedding_encodings.is_empty()
                    || self.embedding_encodings.len() > 2
                    || self
                        .embedding_encodings
                        .iter()
                        .any(|s| !["float", "base64"].contains(&s.as_str()))
                    || self
                        .embedding_encodings
                        .iter()
                        .collect::<BTreeSet<_>>()
                        .len()
                        != self.embedding_encodings.len()
                    || !self.features.is_empty()
                    || self.rates.len() != 1
                    || !self.rates.contains_key("input")))
            || (self.api != ApiKind::Embeddings
                && (self.embedding_dimensions.is_some() || !self.embedding_encodings.is_empty()))
        {
            return Err(StartupError::InvalidProfile);
        }
        // 출력 상한을 요청에 담는 API만 profile에 max_output_tokens가 있다.
        let output = !matches!(
            self.api,
            ApiKind::CountTokens | ApiKind::Embeddings | ApiKind::Systemone
        );
        if (self.count_billing.is_some()
            && !(count
                && self.billing_class == BillingClass::Chargeable
                && self.count_billing.as_deref() == Some("counted_input_tokens")))
            || (count
                && self.billing_class == BillingClass::Chargeable
                && self.count_billing.is_none())
            || !allowed
            || !bounded_text(&self.model, 200)
            || [
                &self.profile_version,
                &self.pricing_version,
                &self.fixture_version,
            ]
            .into_iter()
            .any(|s| !bounded_text(s, 64))
            || self.features.len() > 5
            || self.features.iter().collect::<BTreeSet<_>>().len() != self.features.len()
            || self.approved_beta.len() > 32
            || self.approved_beta.iter().any(|s| {
                !bounded_text(s, 128) || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
            || self.actual_models.is_empty()
            || self.actual_models.len() > 64
            || !self.actual_models.contains(&self.model)
            || self.actual_models.iter().any(|s| !bounded_text(s, 200))
            || self.actual_models.iter().collect::<BTreeSet<_>>().len() != self.actual_models.len()
            || self.max_input_tokens == 0
            || self.max_input_tokens > 1_000_000_000
            || (output
                && !self
                    .max_output_tokens
                    .is_some_and(|v| v > 0 && v <= 1_000_000_000))
            || (!output && self.max_output_tokens.is_some())
            || self.max_body_bytes == 0
            || self.max_body_bytes > limits.inference_body_bytes
            || self.max_response_bytes == 0
            || self.max_response_bytes > if count { 256 * 1024 } else { 16 * 1024 * 1024 }
            || (self.billing_class == BillingClass::GuaranteedFree
                && !api.is_some_and(|api| api.may_be_free()))
        {
            return Err(StartupError::InvalidProfile);
        }
        crate::providers::port::provider(provider).check_profile(self)?;
        if !self.rates.contains_key("input")
            || (output && !self.rates.contains_key("output"))
            || self.rates.len() > 8
        {
            return Err(StartupError::InvalidPrice);
        }
        // 가격 항목은 그 API가 알린 사용량 칸만 쓴다(결정 0012).
        let declared = api.map_or(&[][..], |api| api.usage_categories());
        for (category, value) in &self.rates {
            if !crate::ledger::finance::UsageCategory::from_name(category)
                .is_some_and(|c| declared.contains(&c))
                || !valid_price(value)
                || (self.billing_class == BillingClass::GuaranteedFree && value != "0.000000000000")
            {
                return Err(StartupError::InvalidPrice);
            }
        }
        Ok(())
    }
}
fn valid_price(value: &str) -> bool {
    let Some((whole, fraction)) = value.split_once('.') else {
        return false;
    };
    !whole.is_empty()
        && whole.len() <= 16
        && (whole.len() == 1 || !whole.starts_with('0'))
        && whole.bytes().all(|b| b.is_ascii_digit())
        && fraction.len() == 12
        && fraction.bytes().all(|b| b.is_ascii_digit())
}
fn strict_rates<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    struct Text(String);
    impl<'de> Deserialize<'de> for Text {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct Visitor;
            impl serde::de::Visitor<'_> for Visitor {
                type Value = Text;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("고정 소수점 문자열")
                }
                fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Text, E> {
                    Ok(Text(v.to_owned()))
                }
            }
            d.deserialize_str(Visitor)
        }
    }
    BTreeMap::<String, Text>::deserialize(d).map(|m| m.into_iter().map(|(k, v)| (k, v.0)).collect())
}
impl Limits {
    pub fn validate(&self) -> Result<(), StartupError> {
        let max = Self::default();
        macro_rules! bounded { ($($field:ident),*) => { $(if self.$field==0 || self.$field>max.$field {return Err(StartupError::InvalidLimits);})* }; }
        bounded!(
            connections,
            http2_streams,
            http_requests,
            inference,
            per_principal,
            pre_auth,
            authentication,
            nonstream,
            management,
            management_writes,
            assets,
            header_bytes,
            header_count,
            token_bytes,
            header_timeout_ms,
            inference_body_bytes,
            management_body_bytes,
            body_timeout_ms,
            body_idle_timeout_ms,
            management_timeout_ms,
            asset_timeout_ms,
            request_timeout_ms,
            transport_timeout_ms,
            finalization_timeout_ms,
            health_interval_ms,
            health_valid_ms
        );
        if self.header_bytes < 8192
            || self.inference > self.http_requests
            || self.per_principal > self.inference
            || self.nonstream > self.inference
            || self.pre_auth > self.inference
            || self.authentication > self.pre_auth
            || self.management_writes > self.management
            || self.management > self.http_requests
            || self.assets > self.http_requests
            || self.token_bytes > self.header_bytes
            || self.header_count > self.header_bytes
            || self.transport_timeout_ms + self.finalization_timeout_ms > self.request_timeout_ms
            || self.body_timeout_ms > self.transport_timeout_ms
            || self.body_idle_timeout_ms > self.body_timeout_ms
            || self.health_interval_ms > self.health_valid_ms
        {
            return Err(StartupError::InvalidLimits);
        }
        Ok(())
    }
}

/// 설정은 bounded block/flow YAML만 지원한다. anchor/alias/tag/merge는 확장 전 거부한다.
fn check_yaml_budget(input: &str) -> Result<(), StartupError> {
    if input.len() > MAX_CONFIG_BYTES {
        return Err(StartupError::ConfigInput);
    }
    let mut quote = None;
    let mut escaped = false;
    let mut flow = 0_u32;
    let mut previous = b' ';
    for line in input.lines() {
        let indent = line.bytes().take_while(|b| *b == b' ').count();
        if indent > 64 {
            return Err(StartupError::ConfigInput);
        }
        let mut plain = false;
        let line_bytes = line.as_bytes();
        for (index, &byte) in line_bytes.iter().enumerate() {
            if let Some(q) = quote {
                if escaped {
                    escaped = false;
                } else if (q == b'"' && byte == b'\\')
                    || (q == b'\'' && byte == q && line_bytes.get(index + 1) == Some(&q))
                {
                    escaped = true;
                } else if byte == q {
                    quote = None;
                    plain = true;
                }
            } else {
                match byte {
                    b'#' if previous.is_ascii_whitespace() => break,
                    b'\'' | b'"' if !plain => quote = Some(byte),
                    b':' if flow > 0
                        || line_bytes
                            .get(index + 1)
                            .is_none_or(u8::is_ascii_whitespace) =>
                    {
                        plain = false
                    }
                    b',' if flow > 0 => plain = false,
                    b'-' if !plain
                        && line_bytes
                            .get(index + 1)
                            .is_some_and(u8::is_ascii_whitespace) => {}
                    b'&' | b'*' | b'!'
                        if previous.is_ascii_whitespace() || b"[{,:".contains(&previous) =>
                    {
                        return Err(StartupError::ConfigSyntax);
                    }
                    b'[' | b'{' => {
                        plain = false;
                        flow += 1;
                        if flow > 32 {
                            return Err(StartupError::ConfigInput);
                        }
                    }
                    b']' | b'}' => {
                        flow = flow.saturating_sub(1);
                        plain = false;
                    }
                    byte if byte.is_ascii_whitespace() => {}
                    _ => plain = true,
                }
            }
            previous = byte;
        }
        previous = b' ';
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn api_names_are_config_values() {
        for kind in [
            ApiKind::Messages,
            ApiKind::CountTokens,
            ApiKind::Responses,
            ApiKind::OpenaiChat,
            ApiKind::Embeddings,
            ApiKind::DeepseekChat,
            ApiKind::Systemone,
        ] {
            assert_eq!(
                serde_json::from_value::<ApiKind>(serde_json::json!(kind.name())).unwrap(),
                kind
            );
        }
    }
}
