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
use std::time::Duration;
use url::Url;
pub struct NativeClient {
    client: reqwest::Client,
    base: Url,
    key: String,
    provider: ProviderId,
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
        }
    }
    pub fn status(self) -> StatusCode {
        if self == Self::Timeout {
            StatusCode::GATEWAY_TIMEOUT
        } else if self == Self::Cancelled {
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
    ) -> Result<Self, ApiError> {
        if config.base_url.as_str() != format!("{}/", config.provider.origin()) {
            return Err(ApiError::InvalidRequest);
        }
        if let Some(shared) = shared {
            return Ok(Self {
                client: shared.client.clone(),
                base: config.base_url.clone(),
                key: config.api_key.expose().to_owned(),
                provider: config.provider,
            });
        }
        Self::build(
            config.base_url.clone(),
            config.api_key.expose().to_owned(),
            true,
            ca,
            config.provider,
        )
    }

    pub fn new(config: &ProviderConfig) -> Result<Self, ApiError> {
        Self::new_with_ca(config, None)
    }
    pub(crate) fn new_with_ca(
        config: &ProviderConfig,
        ca: Option<&[u8]>,
    ) -> Result<Self, ApiError> {
        if config.provider != ProviderId::Anthropic
            || config.base_url.as_str() != "https://api.anthropic.com/"
        {
            return Err(ApiError::InvalidRequest);
        }
        Self::build(
            config.base_url.clone(),
            config.api_key.expose().to_owned(),
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
        Self::build(base, key, false, None, ProviderId::Anthropic)
    }
    fn build(
        base: Url,
        key: String,
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
        if matches!(self.provider, ProviderId::Openai | ProviderId::Deepseek) {
            outbound = outbound.bearer_auth(&self.key);
        } else {
            outbound = outbound.header("x-api-key", &self.key);
        }
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
    pub fn openai(config: &ProviderConfig, ca: Option<&[u8]>) -> Result<Self, ApiError> {
        if config.provider != ProviderId::Openai
            || config.base_url.as_str() != "https://api.openai.com/"
        {
            return Err(ApiError::InvalidRequest);
        }
        Self::build(
            config.base_url.clone(),
            config.api_key.expose().to_owned(),
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
