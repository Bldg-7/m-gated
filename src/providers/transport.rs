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
/// 감독자가 relay future를 폐기해도 완전 검증한 작은 관측만 남긴다.
/// 끊긴 요청을 정산하려고 Anthropic 시작 event의 입력 usage와 받은 출력 내용도 메모리에 둔다.
/// 받은 내용은 DB에 저장하지 않고, 요청이 끝나면 버린다.
#[derive(Default)]
pub struct StreamProgress {
    pub final_usage: Option<FinalUsage>,
    pub provider_error_observed: bool,
    /// Provider가 보낸 event를 하나라도 해석했는지. 없으면 첫 응답 전에 끊긴 것이다.
    pub started: bool,
    /// Anthropic `message_start`가 알린 입력 usage. 끊긴 요청의 입력 토큰이다.
    pub start: Option<crate::ledger::finance::StartUsage>,
    /// 끊기기 전에 받은 출력 내용(텍스트, thinking, 도구 인자).
    pub received: ReceivedOutput,
}
impl StreamProgress {
    // FinalUsage 고정 필드와 최대200byte model의 별도 보관을 기존 parser 예산에서 예약한다.
    pub const RETAINED_BYTES: usize = 512;
    pub fn invalidate(&mut self) {
        self.final_usage = None;
    }
}
/// 끊긴 요청의 `received` 정산에 쓰는 받은 출력 내용. 상한을 넘으면 내용을 버리고 넘었다는 것만 남긴다.
#[derive(Default)]
pub struct ReceivedOutput {
    text: String,
    cap: usize,
    overflow: bool,
}
impl ReceivedOutput {
    /// 요청의 max_tokens 1개당 8바이트까지 담는다. 서버 메모리를 지키려고 2 MiB를 넘지 않는다.
    pub const MAX_BYTES: usize = 2 * 1024 * 1024;
    pub fn with_output_limit(max_tokens: Option<u64>) -> Self {
        Self {
            text: String::new(),
            cap: max_tokens
                .and_then(|n| usize::try_from(n.saturating_mul(8)).ok())
                .map_or(0, |n| n.min(Self::MAX_BYTES)),
            overflow: false,
        }
    }
    pub fn push(&mut self, part: &str) {
        if self.overflow {
            return;
        }
        if self.text.len().saturating_add(part.len()) > self.cap {
            self.overflow = true;
            self.text = String::new();
            return;
        }
        self.text.push_str(part);
    }
    /// 한 조각이 decoder의 내용 상한을 넘어 받지 못했다. 담아 둔 내용도 버리고 넘었다는 것만 남긴다.
    pub fn drop_content(&mut self) {
        self.overflow = true;
        self.text = String::new();
    }
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn overflow(&self) -> bool {
        self.overflow
    }
    pub fn len(&self) -> usize {
        self.text.len()
    }
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
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
    /// Provider가 HTTP 오류로 답했지만 그 본문을 읽거나 확인하지 못했다(인증 오류·되돌림은 따로 센다).
    /// 클라이언트에는 형식 오류와 같은 코드로 알린다. 장부는 오류 응답이므로 0원으로 정산한다(결정 0005).
    ErrorResponse,
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
            Self::ErrorResponse => "upstream_protocol_error",
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
/// HTTP 오류 응답의 본문을 읽다 실패하면 그 실패를 [`UpstreamError::ErrorResponse`]로 바꾼다.
/// 상태 코드가 오류라는 사실은 본문을 읽지 못해도 바뀌지 않는다. 인증 오류·되돌림은 그대로 둔다.
pub(super) fn keep_error_status(
    status: StatusCode,
    result: Result<UpstreamResponse, UpstreamError>,
) -> Result<UpstreamResponse, UpstreamError> {
    match result {
        Err(e)
            if !status.is_success()
                && !matches!(e, UpstreamError::Auth | UpstreamError::Redirect) =>
        {
            Err(UpstreamError::ErrorResponse)
        }
        other => other,
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
    /// DB의 Provider 키를 쓰는 client인지(테스트 고정 키가 아닌지).
    pub fn uses_store(&self) -> bool {
        matches!(self.key, KeySource::Store(_))
    }
    /// 고정한 Provider 키의 DB id. 테스트 고정 키는 None이다.
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
    /// 끊긴 Anthropic 스트림에서 받은 출력 내용의 토큰 수를 센다(결정 0005의 `received` 규칙).
    /// 무료인 count_tokens를 쓰고, 사용자 요청이 아니므로 장부의 송신 권한(permit) 없이 보낸다.
    /// 받은 내용을 사용자 메시지 하나로 세고, 한 글자짜리 기준 메시지의 수를 빼서 내용의 토큰 수를 낸다.
    /// 재시도하지 않는다. 실패하면 호출한 쪽이 `max_tokens` 규칙으로 정산한다.
    pub async fn count_received(
        &self,
        model: &str,
        text: &str,
        deadline: tokio::time::Instant,
    ) -> Result<u64, UpstreamError> {
        if self.provider != ProviderId::Anthropic {
            return Err(UpstreamError::Protocol);
        }
        if text.is_empty() {
            return Ok(0);
        }
        let secret = self.secret().ok_or(UpstreamError::Credential)?;
        let url = self
            .base
            .join("v1/messages/count_tokens")
            .map_err(|_| UpstreamError::Protocol)?;
        let one = |content: &str| {
            let body =
                serde_json::json!({"model":model,"messages":[{"role":"user","content":content}]});
            let request = self
                .authorize(self.client.post(url.clone()), secret)
                .header("anthropic-version", "2023-06-01")
                .header(header::CONTENT_TYPE, "application/json")
                .body(body.to_string());
            async move {
                let response = tokio::time::timeout_at(deadline, request.send())
                    .await
                    .map_err(|_| UpstreamError::Timeout)?
                    .map_err(classify_transport)?;
                if response.status() != StatusCode::OK
                    || response.content_length().is_some_and(|n| n > 4096)
                {
                    return Err(UpstreamError::Protocol);
                }
                let bytes = tokio::time::timeout_at(deadline, response.bytes())
                    .await
                    .map_err(|_| UpstreamError::Timeout)?
                    .map_err(classify_transport)?;
                if bytes.len() > 4096 {
                    return Err(UpstreamError::Protocol);
                }
                serde_json::from_slice::<serde_json::Value>(&bytes)
                    .ok()
                    .and_then(|v| v["input_tokens"].as_u64())
                    .ok_or(UpstreamError::Protocol)
            }
        };
        let (base, full) = tokio::join!(one("."), one(text));
        Ok(full?.saturating_sub(base?).saturating_add(1))
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
            ..Default::default()
        };
        progress.invalidate();
        assert!(progress.final_usage.is_none());
        assert!(
            !progress.provider_error_observed,
            "검증 실패와 검증된 native 실패는 다르다"
        );
    }
    #[test]
    fn received_output_keeps_eight_bytes_per_max_token_then_drops_the_content() {
        let mut received = ReceivedOutput::with_output_limit(Some(1));
        received.push("abcd");
        received.push("efgh");
        assert_eq!(received.text(), "abcdefgh");
        assert!(!received.overflow());
        // 상한(max_tokens × 8바이트)을 넘으면 내용을 버리고 넘었다는 것만 남긴다.
        received.push("i");
        assert!(received.overflow());
        assert!(received.is_empty());
        received.push("j");
        assert!(received.is_empty());
        // max_tokens가 커도 2 MiB를 넘게 담지 않는다. max_tokens가 없으면 담지 않는다.
        let mut huge = ReceivedOutput::with_output_limit(Some(u64::MAX));
        huge.push(&"x".repeat(ReceivedOutput::MAX_BYTES + 1));
        assert!(huge.overflow());
        let mut none = ReceivedOutput::with_output_limit(None);
        none.push("a");
        assert!(none.overflow());
        // 한 조각이 decoder의 내용 상한을 넘어 받지 못하면 담아 둔 내용도 버린다.
        let mut dropped = ReceivedOutput::with_output_limit(Some(100));
        dropped.push("abc");
        dropped.drop_content();
        assert!(dropped.overflow() && dropped.is_empty());
        dropped.push("d");
        assert!(dropped.is_empty());
    }
}
