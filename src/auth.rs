pub mod csrf;
pub mod key;
pub mod password;
use axum::http::HeaderMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthError {
    InvalidCredentials,
    AuthSourceForbidden,
    AccessDenied,
    OwnerInactive,
    DependencyUnavailable,
    DatabaseUnavailable,
    /// 유효한 SSO 로그인 토큰이지만 추론에는 쓸 수 없다. 추론은 가상 키로만 받는다.
    VirtualKeyRequired,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthRoute {
    Inference,
    Management,
    CookieOnly,
    Login,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialSource {
    Header,
    Cookie,
}
pub struct Credential {
    pub(crate) token: String,
    pub source: CredentialSource,
}
pub fn extract_credential(
    headers: &HeaderMap,
    route: AuthRoute,
    cookie: &str,
) -> Result<Credential, AuthError> {
    let auth = single_header(headers, "authorization")?;
    let key = single_header(headers, "x-api-key")?;
    if auth.is_some() || key.is_some() {
        let bearer = auth
            .map(|v| {
                let (scheme, token) = v.split_once(' ').ok_or(AuthError::InvalidCredentials)?;
                if !scheme.eq_ignore_ascii_case("bearer") {
                    return Err(AuthError::InvalidCredentials);
                }
                token_value(token.trim_matches([' ', '\t']))
            })
            .transpose()?;
        let key = key.map(token_value).transpose()?;
        if let (Some(a), Some(b)) = (bearer, key) {
            use subtle::ConstantTimeEq;
            if a.as_bytes().ct_eq(b.as_bytes()).unwrap_u8() != 1 {
                return Err(AuthError::InvalidCredentials);
            }
        }
        return Ok(Credential {
            token: bearer
                .or(key)
                .ok_or(AuthError::InvalidCredentials)?
                .to_owned(),
            source: CredentialSource::Header,
        });
    }
    if route == AuthRoute::Inference {
        return Err(AuthError::InvalidCredentials);
    }
    let mut selected = None;
    for value in headers.get_all("cookie") {
        let value = value.to_str().map_err(|_| AuthError::InvalidCredentials)?;
        for pair in value.split(';') {
            let Some((name, value)) = pair.trim().split_once('=') else {
                continue;
            };
            if name == cookie {
                if selected.is_some() {
                    return Err(AuthError::InvalidCredentials);
                }
                selected = Some(token_value(value)?);
            }
        }
    }
    Ok(Credential {
        token: selected.ok_or(AuthError::InvalidCredentials)?.to_owned(),
        source: CredentialSource::Cookie,
    })
}
fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, AuthError> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(AuthError::InvalidCredentials);
    }
    let raw = value.to_str().map_err(|_| AuthError::InvalidCredentials)?;
    let raw = raw.trim_matches([' ', '\t']);
    if raw.bytes().any(|b| b.is_ascii_control()) || raw.contains(',') {
        return Err(AuthError::InvalidCredentials);
    }
    Ok(Some(raw))
}
fn token_value(value: &str) -> Result<&str, AuthError> {
    if value.is_empty()
        || value.len() > 8192
        || !value.bytes().all(|b| b.is_ascii_graphic() && b != b',')
    {
        return Err(AuthError::InvalidCredentials);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    #[test]
    fn explicit_bad_credentials_never_fall_back() {
        let mut h = HeaderMap::new();
        h.insert(
            "cookie",
            HeaderValue::from_static("mgated_access=valid.cookie"),
        );
        for bad in [
            "",
            "Basic abc",
            "Bearer",
            "Bearer a,b",
            "Bearer a b",
            "Bearer a\tb",
            "Bearer \tgood",
        ] {
            h.insert("authorization", HeaderValue::from_str(bad).unwrap());
            assert!(matches!(
                extract_credential(&h, AuthRoute::Management, "mgated_access"),
                Err(AuthError::InvalidCredentials)
            ));
        }
        h.insert("authorization", HeaderValue::from_static("bEaReR good.jwt"));
        h.insert("x-api-key", HeaderValue::from_static("good.jwt"));
        let c = extract_credential(&h, AuthRoute::Management, "mgated_access")
            .expect("같은 두 명시 토큰은 한 번 인증한다");
        assert_eq!(c.token, "good.jwt");
        assert_eq!(c.source, CredentialSource::Header);
        h.insert(
            "authorization",
            HeaderValue::from_static(" \tbEaReR good.jwt \t"),
        );
        h.insert("x-api-key", HeaderValue::from_static("\tgood.jwt \t"));
        assert_eq!(
            extract_credential(&h, AuthRoute::Management, "mgated_access")
                .unwrap()
                .token,
            "good.jwt"
        );
    }
    #[test]
    fn credential_source_matrix_is_strict_and_bounded() {
        let cookie = "mgated_access";
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("Bearer same"));
        h.insert("x-api-key", HeaderValue::from_static("other"));
        assert!(extract_credential(&h, AuthRoute::Management, cookie).is_err());
        h.insert("x-api-key", HeaderValue::from_static("same"));
        h.append("authorization", HeaderValue::from_static("Bearer same"));
        assert!(extract_credential(&h, AuthRoute::Management, cookie).is_err());
        h.remove("authorization");
        h.append("x-api-key", HeaderValue::from_static("same"));
        assert!(extract_credential(&h, AuthRoute::Management, cookie).is_err());
        h.clear();
        h.insert(
            "x-api-key",
            HeaderValue::from_str(&"a".repeat(8192)).unwrap(),
        );
        assert!(extract_credential(&h, AuthRoute::Inference, cookie).is_ok());
        h.insert(
            "x-api-key",
            HeaderValue::from_str(&"a".repeat(8193)).unwrap(),
        );
        assert!(extract_credential(&h, AuthRoute::Inference, cookie).is_err());
        h.clear();
        h.insert(
            "cookie",
            HeaderValue::from_static("other_access=wrong-cookie"),
        );
        assert!(extract_credential(&h, AuthRoute::Management, cookie).is_err());
        h.insert(
            "cookie",
            HeaderValue::from_static("mgated_access=a; mgated_access=b"),
        );
        assert!(extract_credential(&h, AuthRoute::Management, cookie).is_err());
        h.insert("cookie", HeaderValue::from_static("mgated_access=a"));
        h.append("cookie", HeaderValue::from_static("mgated_access=a"));
        assert!(extract_credential(&h, AuthRoute::Management, cookie).is_err());
        h.remove("cookie");
        h.insert(
            "cookie",
            HeaderValue::from_static("other=ignored; mgated_access=a"),
        );
        assert_eq!(
            extract_credential(&h, AuthRoute::Management, cookie)
                .unwrap()
                .source,
            CredentialSource::Cookie
        );
        assert!(extract_credential(&h, AuthRoute::Inference, cookie).is_err());
        h.insert("authorization", HeaderValue::from_static("Bearer a"));
        assert_eq!(
            extract_credential(&h, AuthRoute::CookieOnly, cookie)
                .unwrap()
                .source,
            CredentialSource::Header
        );
        for value in [
            b"Bearer a\x7f".as_slice(),
            b"Bearer a\x01",
            b"Bearer a\tinside",
        ] {
            if let Ok(value) = HeaderValue::from_bytes(value) {
                h.insert("authorization", value);
                assert!(extract_credential(&h, AuthRoute::Login, cookie).is_err());
            }
            // HeaderValue 자체가 거부하는 제어문자는 이 함수까지 도달하지 않는다.
        }
    }
}

pub mod jwks;
use serde::Deserialize;
use std::{sync::Arc, time::Duration};

/// 서명과 claim 검증 성공 경로만 생성할 수 있는 identity다. 이메일은 lookup 키가 아니다.
pub struct VerifiedIdentity {
    issuer: String,
    subject: String,
    email: String,
    display_name: String,
    expires_at: i64,
}
impl VerifiedIdentity {
    /// 비밀번호 방식의 신원. issuer는 게이트웨이 주소, subject는 소문자 이메일이다(결정 0011 규칙 3).
    /// 비밀번호를 확인했거나 유효한 세션을 찾은 경로만 만든다.
    pub(crate) fn for_password(issuer: String, email: String, display_name: String) -> Self {
        Self {
            issuer,
            subject: email.clone(),
            email,
            display_name,
            expires_at: time::OffsetDateTime::now_utc().unix_timestamp()
                + crate::auth::password::SESSION_SECONDS,
        }
    }
    pub fn issuer(&self) -> &str {
        &self.issuer
    }
    pub fn subject(&self) -> &str {
        &self.subject
    }
    pub fn email(&self) -> &str {
        &self.email
    }
    pub fn display_name(&self) -> &str {
        &self.display_name
    }
    pub fn expires_at(&self) -> i64 {
        self.expires_at
    }
}
#[derive(Deserialize)]
struct Claims {
    iss: String,
    aud: Audience,
    sub: String,
    email: String,
    email_verified: bool,
    exp: i64,
    #[serde(default, deserialize_with = "present_nbf")]
    nbf: Option<i64>,
    name: Option<String>,
}
fn present_nbf<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    i64::deserialize(d).map(Some)
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}
impl Audience {
    fn matches(&self, expected: &str) -> bool {
        match self {
            Self::One(s) => s == expected,
            Self::Many(s) => s.len() == 1 && s[0] == expected,
        }
    }
}
pub struct SsoVerifier {
    issuer: String,
    audience: String,
    pub jwks: Arc<jwks::JwksCache>,
}
impl SsoVerifier {
    pub fn new(config: &crate::config::AuthConfig, jwks: Arc<jwks::JwksCache>) -> Self {
        Self {
            issuer: config.issuer_literal.clone(),
            audience: config.audience_literal.clone(),
            jwks,
        }
    }
    pub async fn verify(
        &self,
        credential: &Credential,
        now: i64,
    ) -> Result<VerifiedIdentity, AuthError> {
        tokio::time::timeout(Duration::from_secs(3), self.verify_inner(credential, now))
            .await
            .map_err(|_| AuthError::DependencyUnavailable)?
    }
    async fn verify_inner(
        &self,
        credential: &Credential,
        now: i64,
    ) -> Result<VerifiedIdentity, AuthError> {
        use jsonwebtoken::{Algorithm, Validation, decode, decode_header};
        let started = tokio::time::Instant::now();
        let invalid = || AuthError::InvalidCredentials;
        token_value(&credential.token)?;
        let header = decode_header(&credential.token).map_err(|_| invalid())?;
        if header.alg != Algorithm::RS256 {
            return Err(invalid());
        }
        let kid = header
            .kid
            .filter(|s| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control))
            .ok_or_else(invalid)?;
        let key = self.jwks.key(&kid).await?;
        let mut validation = Validation::new(Algorithm::RS256);
        // 시각 입력을 명시해 경계 시험과 실제 요청에서 동일 규칙을 적용한다.
        validation.validate_exp = false;
        validation.validate_aud = false;
        validation.required_spec_claims.clear();
        let claims: Claims = decode::<Claims>(&credential.token, &key, &validation)
            .map_err(|_| invalid())?
            .claims;
        let now =
            now.saturating_add(i64::try_from(started.elapsed().as_secs()).map_err(|_| invalid())?);
        if claims.iss != self.issuer
            || !claims.aud.matches(&self.audience)
            || claims.exp <= 0
            || now >= claims.exp.saturating_add(30)
            || claims
                .nbf
                .is_some_and(|n| n < 0 || n > now.saturating_add(30))
            || claims.sub.is_empty()
            || claims.sub.len() > 512
            || claims.sub.chars().any(char::is_control)
            || !claims.email_verified
            || claims.email.is_empty()
            || claims.email.len() > 320
            || claims
                .email
                .chars()
                .any(|c| c.is_control() || c.is_whitespace())
            || !claims.email.contains('@')
        {
            return Err(invalid());
        }
        let email = claims.email.to_lowercase();
        let display_name = claims.name.unwrap_or_else(|| email.clone());
        if display_name.is_empty()
            || display_name.chars().count() > 100
            || display_name.len() > 400
            || display_name.chars().any(char::is_control)
        {
            return Err(invalid());
        }
        Ok(VerifiedIdentity {
            issuer: claims.iss,
            subject: claims.sub,
            email,
            display_name,
            expires_at: claims.exp,
        })
    }
}
/// 접근 설정. 누가 쓸 수 있고 누가 관리자인지는 DB의 접근 목록(`gw_access`)이 정한다(결정 0001).
/// 설정에는 고정 관리자 한 명만 있다.
#[derive(Clone)]
pub struct AccessSettings {
    fixed_admin: String,
}
impl AccessSettings {
    pub fn from_config(config: &crate::config::ValidatedConfig) -> Self {
        Self {
            fixed_admin: config.admin_email().to_owned(),
        }
    }
    /// 고정 관리자의 이메일. 이 사람의 관리자 역할은 대시보드에서 바꾸거나 지울 수 없다.
    pub fn fixed_admin(&self) -> &str {
        &self.fixed_admin
    }
}

pub struct AuthenticatedHuman {
    pub identity: VerifiedIdentity,
    pub current: crate::ledger::CurrentHuman,
    pub source: CredentialSource,
}
/// 사람의 자격 증명을 확인하는 방법(결정 0011). 게이트웨이마다 하나만 쓴다.
enum Method {
    /// 외부 SSO의 JWT.
    Sso(SsoVerifier),
    /// 게이트웨이가 발급한 세션. issuer는 게이트웨이의 public origin이다.
    Password { issuer: String },
}
pub struct Authenticator {
    method: Method,
    store: Arc<crate::ledger::PgIdentityStore>,
    settings: AccessSettings,
    cookie_name: String,
    token_bytes: usize,
    operations: tokio::sync::Semaphore,
}
impl Authenticator {
    /// 설정의 로그인 방식에 맞는 인증기를 만든다.
    pub fn from_config(
        config: &crate::config::ValidatedConfig,
        store: Arc<crate::ledger::PgIdentityStore>,
    ) -> Result<Self, AuthError> {
        match config.sso() {
            Some(sso) => {
                let jwks = Arc::new(jwks::JwksCache::with_ca(
                    sso.jwks_uri.clone(),
                    config.ca_certificate(),
                )?);
                Ok(Self::new(config, SsoVerifier::new(sso, jwks), store))
            }
            None => Ok(Self::password(config, store)),
        }
    }
    /// SSO 방식 인증기.
    pub fn new(
        config: &crate::config::ValidatedConfig,
        verifier: SsoVerifier,
        store: Arc<crate::ledger::PgIdentityStore>,
    ) -> Self {
        let cookie_name = config
            .sso()
            .map(|s| s.access_cookie_name.clone())
            .unwrap_or_else(|| password::SESSION_COOKIE.to_owned());
        Self::with_method(config, Method::Sso(verifier), cookie_name, store)
    }
    /// 비밀번호 방식 인증기. 세션 쿠키 이름은 고정이다.
    pub fn password(
        config: &crate::config::ValidatedConfig,
        store: Arc<crate::ledger::PgIdentityStore>,
    ) -> Self {
        let issuer = config
            .public_origin()
            .as_str()
            .trim_end_matches('/')
            .to_owned();
        Self::with_method(
            config,
            Method::Password { issuer },
            password::SESSION_COOKIE.to_owned(),
            store,
        )
    }
    fn with_method(
        config: &crate::config::ValidatedConfig,
        method: Method,
        cookie_name: String,
        store: Arc<crate::ledger::PgIdentityStore>,
    ) -> Self {
        Self {
            method,
            store,
            settings: AccessSettings::from_config(config),
            cookie_name,
            token_bytes: config.limits().token_bytes,
            operations: tokio::sync::Semaphore::new(config.limits().authentication),
        }
    }
    pub fn mode(&self) -> crate::config::AuthMode {
        match self.method {
            Method::Sso(_) => crate::config::AuthMode::Sso,
            Method::Password { .. } => crate::config::AuthMode::Password,
        }
    }
    pub fn cookie_name(&self) -> &str {
        &self.cookie_name
    }
    pub fn store(&self) -> &Arc<crate::ledger::PgIdentityStore> {
        &self.store
    }
    pub fn settings(&self) -> &AccessSettings {
        &self.settings
    }
    pub(crate) fn credential(
        &self,
        headers: &HeaderMap,
        route: AuthRoute,
    ) -> Result<Credential, AuthError> {
        extract_credential(headers, route, &self.cookie_name)
    }
    pub async fn authenticate_inference(
        &self,
        headers: &HeaderMap,
        now: i64,
    ) -> Result<AuthenticatedInference, AuthError> {
        let credential = self.credential(headers, AuthRoute::Inference)?;
        if credential.token.len() > self.token_bytes {
            return Err(AuthError::InvalidCredentials);
        }
        if credential.token.starts_with("mgw_") {
            let mut metric = crate::telemetry::Timer::start(
                crate::telemetry::Metric::Authentication,
                crate::telemetry::Metric::AuthenticationMicros,
                crate::telemetry::Metric::AuthenticationFailed,
            );
            let _operation = self
                .operations
                .try_acquire()
                .map_err(|_| AuthError::DependencyUnavailable)?;
            let result = self.store.authenticate_key(&credential.token).await;
            if result.is_ok() {
                metric.success();
            }
            return result;
        }
        // SSO 로그인 토큰으로는 추론하지 않는다. 어느 Provider 키로 보낼지 정할 수 없기 때문이다.
        // 토큰이 유효한지는 먼저 확인해서, 잘못된 토큰은 지금처럼 인증 오류로 알린다.
        self.authenticate(headers, AuthRoute::Inference, now)
            .await?;
        Err(AuthError::VirtualKeyRequired)
    }
    pub async fn authenticate(
        &self,
        headers: &HeaderMap,
        route: AuthRoute,
        now: i64,
    ) -> Result<AuthenticatedHuman, AuthError> {
        let mut metric = crate::telemetry::Timer::start(
            crate::telemetry::Metric::Authentication,
            crate::telemetry::Metric::AuthenticationMicros,
            crate::telemetry::Metric::AuthenticationFailed,
        );
        let result = self.authenticate_human(headers, route, now).await;
        if result.is_ok() {
            metric.success();
        }
        result
    }
    async fn authenticate_human(
        &self,
        headers: &HeaderMap,
        route: AuthRoute,
        now: i64,
    ) -> Result<AuthenticatedHuman, AuthError> {
        let credential = extract_credential(headers, route, &self.cookie_name)?;
        if credential.token.starts_with("mgw_") {
            self.store.authenticate_key(&credential.token).await?;
            return Err(AuthError::AuthSourceForbidden);
        }
        if credential.token.len() > self.token_bytes {
            return Err(AuthError::InvalidCredentials);
        }
        let _operation = self
            .operations
            .try_acquire()
            .map_err(|_| AuthError::DependencyUnavailable)?;
        tokio::time::timeout(Duration::from_secs(3), async {
            let identity = match &self.method {
                Method::Sso(verifier) => verifier.verify(&credential, now).await?,
                Method::Password { issuer } => {
                    if !password::is_session_token(&credential.token)
                        || !password::plausible_token(&credential.token)
                    {
                        return Err(AuthError::InvalidCredentials);
                    }
                    let session = self
                        .store
                        .session(&password::token_hash(&credential.token))
                        .await?
                        .ok_or(AuthError::InvalidCredentials)?;
                    VerifiedIdentity {
                        issuer: issuer.clone(),
                        subject: session.email.clone(),
                        email: session.email,
                        display_name: session.display_name,
                        expires_at: session.expires_at,
                    }
                }
            };
            // 유효 헤더도 cookie-only 경로에서 cookie로 대체하지 않는다.
            if route == AuthRoute::CookieOnly && credential.source != CredentialSource::Cookie {
                return Err(AuthError::AuthSourceForbidden);
            }
            let current = self.store.current_human(&identity).await?;
            Ok(AuthenticatedHuman {
                identity,
                current,
                source: credential.source,
            })
        })
        .await
        .map_err(|_| AuthError::DependencyUnavailable)?
    }
    /// SSO 방식은 JWKS를 다시 확인한다. 비밀번호 방식은 JWKS가 없으므로 늘 건강하다.
    pub async fn refresh_health(&self, runtime: &crate::runtime::Runtime) -> bool {
        let healthy = match &self.method {
            Method::Sso(verifier) => verifier.jwks.refresh_health().await,
            Method::Password { .. } => true,
        };
        runtime.record_health(
            crate::runtime::Dependency::Jwks,
            healthy,
            tokio::time::Instant::now(),
        );
        healthy
    }
}

pub struct AuthenticatedInference {
    pub current: crate::ledger::CurrentHuman,
    pub key: Option<crate::ledger::finance::KeyScope>,
    /// 가상 키에 연결된 Provider별 Provider 키. 연결이 없는 Provider로 가는 요청은 거절한다.
    pub credentials: Vec<(crate::config::ProviderId, uuid::Uuid)>,
    /// 가상 키가 연결된 Provider 키의 사용 인가가 해제된 Provider. 이 Provider로 가는 요청은 거절한다.
    pub blocked: Vec<crate::config::ProviderId>,
}
impl AuthenticatedInference {
    pub fn blocked(&self, provider: crate::config::ProviderId) -> bool {
        self.blocked.contains(&provider)
    }
    pub fn credential_for(&self, provider: crate::config::ProviderId) -> Option<uuid::Uuid> {
        self.credentials
            .iter()
            .find(|(p, _)| *p == provider)
            .map(|(_, id)| *id)
    }
}
