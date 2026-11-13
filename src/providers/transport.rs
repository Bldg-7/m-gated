//! Provider 간에 공유하는 고정 목적지·재시도 없는 단일 송신 경계. usage는 API별 adapter가 소유한다.
use super::anthropic::ApiError;
use crate::{
    config::{ApiKind, ModelProfile, ProviderConfig, ProviderId},
    ledger::finance::{FinalObservation, FinalUsage, SendPermit},
};
use axum::{
    body::Bytes,
    http::{HeaderMap, HeaderValue, StatusCode, header},
};
use std::{sync::Arc, time::Duration};
use url::Url;
use uuid::Uuid;
#[derive(Clone)]
pub struct NativeClient {
    client: reqwest::Client,
    base: Url,
    key: KeySource,
    provider: ProviderId,
}
/// 송신에 쓰는 회사 키의 출처. 제품은 DB 캐시에서 요청마다 고정(bind)한 키만 보낸다.
#[derive(Clone)]
enum KeySource {
    /// 테스트 조립 전용 고정 키.
    Fixed(String),
    /// 아직 고정하지 않은 제품 client. 이 상태로는 송신하지 않는다.
    Store(Arc<crate::credentials::CredentialCache>),
    /// 요청 시작 시 캐시에서 고정한 키. 교체되어도 진행 중 요청은 이 키를 끝까지 쓴다.
    Bound {
        id: Uuid,
        secret: Arc<zeroize::Zeroizing<String>>,
    },
}
/// 등록 확인 결과. 원문 응답은 보관하지 않는다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyError {
    Rejected,
    Unavailable,
}
/// 감독자가 relay future를 폐기해도 완전 검증한 작은 관측만 남긴다. 원문/부분 usage는 없다.
#[derive(Default)]
pub struct StreamProgress {
    pub final_usage: Option<FinalUsage>,
    pub provider_error_observed: bool,
}
impl StreamProgress {
    // FinalUsage 고정 필드와 최대200byte model의 별도 보관을 기존 parser 예산에서 예약한다.
    pub const RETAINED_BYTES: usize = 512;
    pub fn invalidate(&mut self) {
        self.final_usage = None;
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpstreamError {
    Transport,
    Timeout,
    Auth,
    Redirect,
    Encoding,
    Protocol,
    StreamParse,
    StreamLimit,
    TooLarge,
    Cancelled,
    Credential,
}
impl UpstreamError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Transport => "upstream_transport_error",
            Self::Timeout => "upstream_read_timeout",
            Self::Auth => "upstream_auth_error",
            Self::Redirect => "upstream_redirect",
            Self::Encoding => "upstream_encoding_error",
            Self::Protocol => "upstream_protocol_error",
            Self::StreamParse => "stream_parse_error",
            Self::StreamLimit => "stream_limit",
            Self::TooLarge => "upstream_response_too_large",
            Self::Cancelled => "shutdown_cancelled",
            Self::Credential => "provider_credential_unavailable",
        }
    }
    pub fn status(self) -> StatusCode {
        if self == Self::Timeout {
            StatusCode::GATEWAY_TIMEOUT
        } else if matches!(self, Self::Cancelled | Self::Credential) {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::BAD_GATEWAY
        }
    }
}
impl From<crate::stream::ParseError> for UpstreamError {
    fn from(error: crate::stream::ParseError) -> Self {
        match error {
            crate::stream::ParseError::Syntax => Self::StreamParse,
            crate::stream::ParseError::Limit => Self::StreamLimit,
        }
    }
}
#[derive(Clone, Copy)]
pub struct SendOptions<'a> {
    pub api: ApiKind,
    pub headers: &'a HeaderMap,
    pub profile: &'a ModelProfile,
    pub deadline: tokio::time::Instant,
    pub runtime: &'a crate::runtime::Runtime,
}
pub struct UpstreamResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub observation: FinalObservation,
}
/// stream/nonstream 공통: 중복·비숫자·금지 헤더는 원본에 있어도 전달하지 않는다.
pub(super) fn response_headers(source: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for name in ["request-id", "x-request-id"] {
        if let Some(v) = single_header(source, name)
            && !v.as_bytes().is_empty()
            && v.as_bytes().len() <= 256
            && v.to_str().is_ok_and(|s| !s.chars().any(char::is_control))
        {
            headers.insert(name, v.clone());
        }
    }
    if let Some(v) = single_header(source, "retry-after")
        && v.to_str()
            .ok()
            .and_then(|v| {
                v.bytes()
                    .all(|b| b.is_ascii_digit())
                    .then(|| v.parse::<u32>().ok())
                    .flatten()
            })
            .is_some_and(|n| (1..=120).contains(&n))
    {
        headers.insert(header::RETRY_AFTER, v.clone());
    }
    for name in [
        "anthropic-ratelimit-requests-limit",
        "anthropic-ratelimit-requests-remaining",
        "anthropic-ratelimit-input-tokens-limit",
        "anthropic-ratelimit-input-tokens-remaining",
        "anthropic-ratelimit-output-tokens-limit",
        "anthropic-ratelimit-output-tokens-remaining",
    ] {
        if let Some(v) = single_header(source, name)
            && v.to_str()
                .ok()
                .and_then(|v| {
                    v.bytes()
                        .all(|b| b.is_ascii_digit())
                        .then(|| v.parse::<u64>().ok())
                        .flatten()
                })
                .is_some()
        {
            headers.insert(name, v.clone());
        }
    }
    headers
}
fn single_header<'a>(source: &'a HeaderMap, name: &str) -> Option<&'a HeaderValue> {
    let mut values = source.get_all(name).iter();
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

impl NativeClient {
    pub(crate) fn from_config(
        config: &ProviderConfig,
        ca: Option<&[u8]>,
        shared: Option<&Self>,
        cache: Arc<crate::credentials::CredentialCache>,
    ) -> Result<Self, ApiError> {
        if config.base_url.as_str() != format!("{}/", config.provider.origin()) {
            return Err(ApiError::InvalidRequest);
        }
        if let Some(shared) = shared {
            return Ok(Self {
                client: shared.client.clone(),
                base: config.base_url.clone(),
                key: KeySource::Store(cache),
                provider: config.provider,
            });
        }
        Self::build(
            config.base_url.clone(),
            KeySource::Store(cache),
            true,
            ca,
            config.provider,
        )
    }
    /// 요청 하나에 쓸 키를 캐시에서 고정한다. `bound`는 가상 키에 연결된 회사 키이고, 없으면
    /// Provider의 기본 키를 쓴다. 사용 가능한 키가 없으면 송신 전에 거절한다.
    pub fn bind_credential(&self, bound: Option<Uuid>) -> Option<Self> {
        let key = match &self.key {
            KeySource::Store(cache) => {
                let active = cache.get(self.provider, bound)?;
                KeySource::Bound {
                    id: active.id,
                    secret: active.secret,
                }
            }
            other => other.clone(),
        };
        Some(Self {
            client: self.client.clone(),
            base: self.base.clone(),
            key,
            provider: self.provider,
        })
    }
    /// 고정한 회사 키의 DB id. 테스트 고정 키는 None이다.
    pub fn credential_id(&self) -> Option<Uuid> {
        match &self.key {
            KeySource::Bound { id, .. } => Some(*id),
            _ => None,
        }
    }
    pub fn provider(&self) -> ProviderId {
        self.provider
    }
    fn secret(&self) -> Option<&str> {
        match &self.key {
            KeySource::Fixed(s) => Some(s),
            KeySource::Bound { secret, .. } => Some(secret.as_str()),
            KeySource::Store(_) => None,
        }
    }
    fn authorize(&self, request: reqwest::RequestBuilder, secret: &str) -> reqwest::RequestBuilder {
        if matches!(self.provider, ProviderId::Openai | ProviderId::Deepseek) {
            request.bearer_auth(secret)
        } else {
            request.header("x-api-key", secret)
        }
    }
    /// 등록 직전 후보 키를 과금 없는 모델 목록 API로 확인한다. 응답 본문은 읽지 않는다.
    pub async fn verify_credential(&self, secret: &str) -> Result<(), VerifyError> {
        let path = match self.provider {
            ProviderId::Anthropic => "v1/models?limit=1",
            ProviderId::Openai => "v1/models",
            ProviderId::Deepseek => "models",
        };
        let url = self.base.join(path).map_err(|_| VerifyError::Unavailable)?;
        let mut request = self.authorize(self.client.get(url), secret);
        if self.provider == ProviderId::Anthropic {
            request = request.header("anthropic-version", "2023-06-01");
        }
        let response = tokio::time::timeout(Duration::from_secs(10), request.send())
            .await
            .map_err(|_| VerifyError::Unavailable)?
            .map_err(|_| VerifyError::Unavailable)?;
        match response.status().as_u16() {
            200..=299 => Ok(()),
            401 | 403 => Err(VerifyError::Rejected),
            _ => Err(VerifyError::Unavailable),
        }
    }

    pub fn new(
        config: &ProviderConfig,
        cache: Arc<crate::credentials::CredentialCache>,
    ) -> Result<Self, ApiError> {
        Self::new_with_ca(config, None, cache)
    }
    pub(crate) fn new_with_ca(
        config: &ProviderConfig,
        ca: Option<&[u8]>,
        cache: Arc<crate::credentials::CredentialCache>,
    ) -> Result<Self, ApiError> {
        if config.provider != ProviderId::Anthropic
            || config.base_url.as_str() != "https://api.anthropic.com/"
        {
            return Err(ApiError::InvalidRequest);
        }
        Self::build(
            config.base_url.clone(),
            KeySource::Store(cache),
            true,
            ca,
            ProviderId::Anthropic,
        )
    }
    /// 테스트 조립만 사용하는 별도 loopback 주소. 제품 설정 로더는 이 주소를 수용하지 않는다.
    pub fn loopback_for_test(base: Url, key: String) -> Result<Self, ApiError> {
        if base.scheme() != "http"
            || base.host_str() != Some("127.0.0.1")
            || base.path() != "/"
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(ApiError::InvalidRequest);
        }
        Self::build(
            base,
            KeySource::Fixed(key),
            false,
            None,
            ProviderId::Anthropic,
        )
    }
    /// 테스트 조립 전용: loopback 주소에 DB 캐시 키를 쓰는 client(등록 확인·회전 시험용).
    pub fn loopback_store_for_test(
        base: Url,
        provider: ProviderId,
        cache: Arc<crate::credentials::CredentialCache>,
    ) -> Result<Self, ApiError> {
        let mut client = Self::loopback_for_test(base, String::new())?;
        client.key = KeySource::Store(cache);
        client.provider = provider;
        Ok(client)
    }
    fn build(
        base: Url,
        key: KeySource,
        https_only: bool,
        ca: Option<&[u8]>,
        provider: ProviderId,
    ) -> Result<Self, ApiError> {
        let mut builder = reqwest::Client::builder()
            .https_only(https_only)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(120));
        if let Some(ca) = ca {
            builder = builder.add_root_certificate(
                reqwest::Certificate::from_pem(ca).map_err(|_| ApiError::InvalidRequest)?,
            );
        }
        let client = builder.build().map_err(|_| ApiError::InvalidRequest)?;
        Ok(Self {
            client,
            base,
            key,
            provider,
        })
    }
    pub(super) async fn begin_send(
        &self,
        permit: SendPermit,
        body: impl Into<reqwest::Body>,
        options: SendOptions<'_>,
    ) -> Result<reqwest::Response, UpstreamError> {
        let SendOptions {
            api,
            headers,
            profile: _,
            deadline,
            runtime,
        } = options;
        let path = match (self.provider, api) {
            (ProviderId::Anthropic, ApiKind::Messages) => "v1/messages",
            (ProviderId::Anthropic, ApiKind::CountTokens) => "v1/messages/count_tokens",
            (ProviderId::Openai, ApiKind::Responses) => "v1/responses",
            (ProviderId::Openai, ApiKind::OpenaiChat) => "v1/chat/completions",
            (ProviderId::Openai, ApiKind::Embeddings) => "v1/embeddings",
            (ProviderId::Deepseek, ApiKind::DeepseekChat) => "chat/completions",
            _ => return Err(UpstreamError::Protocol),
        };
        let mut outbound = self
            .client
            .post(self.base.join(path).map_err(|_| UpstreamError::Protocol)?)
            .header(
                header::CONTENT_TYPE,
                headers
                    .get(header::CONTENT_TYPE)
                    .cloned()
                    .unwrap_or(HeaderValue::from_static("application/json")),
            )
            .body(body);
        // 고정되지 않은 제품 client는 키 없이 송신하지 않는다(permit 소비 전 거절).
        let Some(secret) = self.secret() else {
            return Err(UpstreamError::Credential);
        };
        outbound = self.authorize(outbound, secret);
        for name in ["anthropic-version", "anthropic-beta"]
            .into_iter()
            .filter(|_| self.provider == ProviderId::Anthropic)
        {
            if let Some(v) = headers.get(name) {
                outbound = outbound.header(name, v);
            }
        }
        // 바깥 timeout의 poll 순서에 기대지 않고 실제 단일 send 바로 직전에 재확인한다.
        if tokio::time::Instant::now() >= deadline {
            return Err(UpstreamError::Timeout);
        }
        if matches!(
            runtime.lifecycle(),
            crate::runtime::Lifecycle::Draining
                | crate::runtime::Lifecycle::Stopping
                | crate::runtime::Lifecycle::Stopped
        ) {
            return Err(UpstreamError::Cancelled);
        }
        let _request_id = permit.consume();
        let response = outbound.send().await.map_err(classify_transport)?;
        use crate::telemetry::{self, Metric};
        let status = response.status().as_u16();
        telemetry::add(
            match status {
                200..=299 => Metric::Upstream2xx,
                400..=499 => Metric::Upstream4xx,
                500..=599 => Metric::Upstream5xx,
                _ => Metric::UpstreamOther,
            },
            1,
        );
        if matches!(status, 401 | 403) {
            telemetry::add(Metric::AlarmCompanyKey, 1);
        }
        Ok(response)
    }
}
impl NativeClient {
    pub fn openai(
        config: &ProviderConfig,
        ca: Option<&[u8]>,
        cache: Arc<crate::credentials::CredentialCache>,
    ) -> Result<Self, ApiError> {
        if config.provider != ProviderId::Openai
            || config.base_url.as_str() != "https://api.openai.com/"
        {
            return Err(ApiError::InvalidRequest);
        }
        Self::build(
            config.base_url.clone(),
            KeySource::Store(cache),
            true,
            ca,
            ProviderId::Openai,
        )
    }
    pub fn deepseek_loopback_for_test(base: Url, key: String) -> Result<Self, ApiError> {
        let mut client = Self::loopback_for_test(base, key)?;
        client.provider = ProviderId::Deepseek;
        Ok(client)
    }
    pub fn openai_loopback_for_test(base: Url, key: String) -> Result<Self, ApiError> {
        let mut client = Self::loopback_for_test(base, key)?;
        client.provider = ProviderId::Openai;
        Ok(client)
    }
}
pub(super) fn classify_transport(e: reqwest::Error) -> UpstreamError {
    if e.is_timeout() {
        UpstreamError::Timeout
    } else {
        UpstreamError::Transport
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn supervisor_final_usage_fits_reserved_parser_budget() {
        assert!(std::mem::size_of::<StreamProgress>() + 200 <= StreamProgress::RETAINED_BYTES);
        let mut progress = StreamProgress {
            final_usage: Some(FinalUsage {
                actual_model: "m".repeat(200),
                input_tokens: 100,
                output_tokens: 20,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: None,
            }),
            provider_error_observed: false,
        };
        progress.invalidate();
        assert!(progress.final_usage.is_none());
        assert!(
            !progress.provider_error_observed,
            "검증 실패와 검증된 native 실패는 다르다"
        );
    }
}
