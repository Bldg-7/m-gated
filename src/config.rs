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
        }
    }
}
impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
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
    issuer: Option<String>,
    jwks_uri: Option<String>,
    audience: Option<String>,
    access_cookie_name: Option<String>,
    login_url: Option<String>,
    refresh_url: Option<String>,
    logout_url: Option<String>,
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
    #[serde(default)]
    allowed_emails: Vec<String>,
    #[serde(default)]
    admin_emails: Vec<String>,
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
}
impl ProviderId {
    fn secret_name(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::Openai => "OPENAI_API_KEY",
            Self::Deepseek => "DEEPSEEK_API_KEY",
        }
    }
    pub fn origin(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com",
            Self::Openai => "https://api.openai.com",
            Self::Deepseek => "https://api.deepseek.com",
        }
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
    profiles: Vec<ModelProfile>,
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
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputBoundContract {
    pub kind: String,
    pub context_tokens: u64,
    pub input_categories: Vec<String>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingDimensions {
    pub default: u32,
    pub max: u32,
}
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BillingTimeRule {
    FixedAdmissionV1,
    DeepseekTimeUnresolvedV1,
}
impl BillingTimeRule {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FixedAdmissionV1 => "fixed_admission_v1",
            Self::DeepseekTimeUnresolvedV1 => "deepseek_time_unresolved_v1",
        }
    }
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
    #[serde(default)]
    pub internal_count: bool,
    pub count_billing: Option<String>,
    pub billing_time_rule: Option<BillingTimeRule>,
    pub input_bound_contract: Option<InputBoundContract>,
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
    pub profiles: Vec<ModelProfile>,
    pub api_key: Secret,
}
pub struct AuthConfig {
    pub ca_certificate: Option<Vec<u8>>,
    pub issuer_literal: String,
    pub audience_literal: String,
    pub issuer: Url,
    pub jwks_uri: Url,
    pub audience: Url,
    pub access_cookie_name: String,
    pub login_url: Url,
    pub refresh_url: Url,
    pub logout_url: Url,
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
    pub(crate) auth: AuthConfig,
    pub(crate) public_origin: Url,
    pub(crate) allowed_emails: BTreeSet<String>,
    pub(crate) admin_emails: BTreeSet<String>,
    pub(crate) database: DatabaseConfig,
    pub(crate) csrf_signing_key: Secret,
    pub(crate) providers: Vec<ProviderConfig>,
    pub(crate) limits: Limits,
    pub(crate) retention: Retention,
    pub(crate) execution_ceiling: crate::ledger::policy::PolicyLimits,
}
impl ValidatedConfig {
    pub fn auth(&self) -> &AuthConfig {
        &self.auth
    }
    pub fn public_origin(&self) -> &Url {
        &self.public_origin
    }
    pub fn allowed_emails(&self) -> &BTreeSet<String> {
        &self.allowed_emails
    }
    pub fn admin_emails(&self) -> &BTreeSet<String> {
        &self.admin_emails
    }
    pub fn database(&self) -> &DatabaseConfig {
        &self.database
    }
    pub fn csrf_signing_key(&self) -> &Secret {
        &self.csrf_signing_key
    }
    pub fn csrf_signing_key_bytes(&self) -> [u8; 32] {
        let encoded = self.csrf_signing_key.expose();
        std::array::from_fn(|i| {
            u8::from_str_radix(&encoded[i * 2..i * 2 + 2], 16).expect("검증된 64자리 lowercase hex")
        })
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
            || ["DB_PASSWORD", "CSRF_SIGNING_KEY"].contains(&key)
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
    let read = |key: &str, value: Option<String>| {
        env.get(key)
            .cloned()
            .or(value)
            .ok_or(StartupError::ConfigInput)
    };
    let parse = |key, value| -> Result<Url, StartupError> { parse_url(&read(key, value)?) };
    let issuer_literal = read("AUTH_ISSUER", raw.auth.issuer)?;
    let audience_literal = read("SSO_AUDIENCE", raw.auth.audience)?;
    let issuer = parse_url(&issuer_literal)?;
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
    let mut config = ValidatedConfig {
        auth: AuthConfig {
            ca_certificate: ca_certificate.clone(),
            issuer,
            issuer_literal,
            audience: parse_url(&audience_literal)?,
            audience_literal,
            jwks_uri: parse("AUTH_JWKS_URI", raw.auth.jwks_uri)?,
            access_cookie_name: read("SSO_ACCESS_COOKIE_NAME", raw.auth.access_cookie_name)?,
            login_url: parse("SSO_LOGIN_URL", raw.auth.login_url)?,
            refresh_url: parse("SSO_REFRESH_URL", raw.auth.refresh_url)?,
            logout_url: parse("SSO_LOGOUT_URL", raw.auth.logout_url)?,
        },
        public_origin: parse("PUBLIC_ORIGIN", raw.public_origin)?,
        allowed_emails: raw.allowed_emails.into_iter().collect(),
        admin_emails: raw.admin_emails.into_iter().collect(),
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
        providers: raw
            .providers
            .into_iter()
            .map(|p| {
                Ok(ProviderConfig {
                    provider: p.provider,
                    base_url: parse_url(&p.base_url)?,
                    profiles: p.profiles,
                    api_key: secret(env, p.provider.secret_name())?,
                })
            })
            .collect::<Result<_, StartupError>>()?,
        limits: raw.limits,
        retention: raw.retention,
        execution_ceiling: raw.execution_ceiling,
    };
    config.validate(env)?;
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
    if name == "CSRF_SIGNING_KEY"
        && (value.len() != 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
    {
        return Err(StartupError::InvalidSecret);
    }
    Ok(Secret(value.clone()))
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
    "PUBLIC_ORIGIN",
    "ALLOWED_EMAILS",
    "ADMIN_EMAILS",
    "DB_HOST",
    "DB_PORT",
    "DB_NAME",
    "DB_USER",
    "DB_SSL_MODE",
    "DB_PASSWORD",
    "CSRF_SIGNING_KEY",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "DEEPSEEK_API_KEY",
];

impl ValidatedConfig {
    fn validate(&mut self, env: &BTreeMap<String, String>) -> Result<(), StartupError> {
        let auth = &self.auth;
        for url in [
            &auth.issuer,
            &auth.jwks_uri,
            &auth.audience,
            &auth.login_url,
            &auth.refresh_url,
            &auth.logout_url,
            &self.public_origin,
        ] {
            secure_url(url)?;
        }
        if self.public_origin.path() != "/"
            || auth.issuer.path() != "/"
            || auth.audience.path() != "/"
        {
            return Err(StartupError::InvalidUrl);
        }
        // JWT의 iss/aud는 설정 문자열과 그대로 비교한다. URL 정규화로 다른 값이 같아지지 않도록
        // 설정값은 정규 형태(소문자 host, 끝 슬래시 없음, percent-encoding 없음)여야 한다.
        if auth.issuer_literal != auth.issuer.as_str().trim_end_matches('/')
            || auth.audience_literal != auth.audience.as_str().trim_end_matches('/')
            || !cookie_name_valid(&auth.access_cookie_name)
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
        self.admin_emails = email_set(env.get("ADMIN_EMAILS"), &self.admin_emails)?;
        self.allowed_emails = email_set(env.get("ALLOWED_EMAILS"), &self.allowed_emails)?;
        self.allowed_emails
            .extend(self.admin_emails.iter().cloned());
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
        self.execution_ceiling
            .validate()
            .map_err(|_| StartupError::InvalidLimits)?;
        if self.providers.is_empty() || self.providers.len() > 3 {
            return Err(StartupError::InvalidProfile);
        }
        let mut providers = BTreeSet::new();
        for provider in &self.providers {
            secure_url(&provider.base_url)?;
            if provider.base_url.as_str() != format!("{}/", provider.provider.origin())
                || !providers.insert(provider.provider)
                || provider.profiles.is_empty()
                || provider.profiles.len() > 64
            {
                return Err(StartupError::InvalidProfile);
            }
            let mut profiles = BTreeSet::new();
            for profile in &provider.profiles {
                profile.validate(provider.provider, &self.limits)?;
                if profile.internal_count
                    && (provider.provider != ProviderId::Anthropic
                        || profile.api != ApiKind::Messages
                        || !provider.profiles.iter().any(|count| {
                            count.model == profile.model
                                && count.api == ApiKind::CountTokens
                                && profile
                                    .features
                                    .iter()
                                    .filter(|f| {
                                        !matches!(
                                            f,
                                            NativeFeature::Thinking | NativeFeature::JsonSchema
                                        )
                                    })
                                    .all(|f| count.features.contains(f))
                        }))
                {
                    return Err(StartupError::InvalidProfile);
                }
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
fn email_set(
    injected: Option<&String>,
    file: &BTreeSet<String>,
) -> Result<BTreeSet<String>, StartupError> {
    let values: Vec<&str> = match injected {
        Some(v) if v.is_empty() => Vec::new(),
        Some(v) => v.split(',').map(str::trim).collect(),
        None => file.iter().map(String::as_str).collect(),
    };
    if values.len() > 10000 {
        return Err(StartupError::InvalidIdentity);
    }
    let mut result = BTreeSet::new();
    for value in values {
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
        result.insert(value.to_lowercase());
    }
    Ok(result)
}
impl ModelProfile {
    fn validate(&self, provider: ProviderId, limits: &Limits) -> Result<(), StartupError> {
        let allowed = matches!(
            (provider, self.api),
            (
                ProviderId::Anthropic,
                ApiKind::Messages | ApiKind::CountTokens
            ) | (
                ProviderId::Openai,
                ApiKind::Responses | ApiKind::OpenaiChat | ApiKind::Embeddings
            ) | (ProviderId::Deepseek, ApiKind::DeepseekChat)
        );
        let bounded_text =
            |s: &str, max| !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control);
        let count = self.api == ApiKind::CountTokens;
        if self.input_bound_contract.is_some()
            && match provider {
                ProviderId::Anthropic => {
                    crate::providers::anthropic::verified_input_bound(self).is_none()
                }
                ProviderId::Openai => {
                    crate::providers::openai::verified_input_bound(self).is_none()
                }
                ProviderId::Deepseek => {
                    crate::providers::deepseek::verified_input_bound(self).is_none()
                }
            }
        {
            return Err(StartupError::InvalidProfile);
        }

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
        let output = !matches!(self.api, ApiKind::CountTokens | ApiKind::Embeddings);
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
                && !(provider == ProviderId::Anthropic && count))
        {
            return Err(StartupError::InvalidProfile);
        }
        if provider == ProviderId::Deepseek
            && (self.rates.len() != 3
                || !["input", "output", "cache_read"]
                    .iter()
                    .all(|k| self.rates.contains_key(*k)))
        {
            return Err(StartupError::InvalidPrice);
        }
        if (provider == ProviderId::Deepseek
            && (self.billing_time_rule.is_none()
                || self.features.contains(&NativeFeature::InlinePng)))
            || (provider != ProviderId::Deepseek
                && self
                    .billing_time_rule
                    .is_some_and(|r| r != BillingTimeRule::FixedAdmissionV1))
        {
            return Err(StartupError::InvalidProfile);
        }
        if provider != ProviderId::Anthropic && !self.approved_beta.is_empty() {
            return Err(StartupError::InvalidProfile);
        }
        if matches!(provider, ProviderId::Openai | ProviderId::Deepseek)
            && self.features.contains(&NativeFeature::Cache)
            && !self.rates.contains_key("cache_read")
        {
            return Err(StartupError::InvalidPrice);
        }
        if !self.rates.contains_key("input")
            || (output && !self.rates.contains_key("output"))
            || self.rates.len() > 8
        {
            return Err(StartupError::InvalidPrice);
        }
        if provider == ProviderId::Anthropic
            && self.features.contains(&NativeFeature::Cache)
            && self.billing_class == BillingClass::Chargeable
            && (!self.rates.contains_key("cache_read")
                || !(self.rates.contains_key("cache_write")
                    ^ self.rates.contains_key("cache_write_5m"))
                || self.rates.contains_key("cache_write_1h"))
        {
            return Err(StartupError::InvalidPrice);
        }
        for (category, value) in &self.rates {
            if ![
                "input",
                "output",
                "cache_read",
                "cache_write",
                "cache_write_5m",
                "cache_write_1h",
            ]
            .contains(&category.as_str())
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
