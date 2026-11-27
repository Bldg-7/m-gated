//! OpenAI 어댑터 crate(`adapters/openai`)를 port에 연결한다(결정 0013). OpenAI 규칙은 그 crate에
//! 있다. 이 파일은 Provider와 Responses API를 잇고, Chat Completions와 Embeddings는 각 파일이 잇는다.
use super::{
    port::{self, Api, ApiError, BoxFuture, ErrorFormat, Reading, RequestMetadata},
    transport::UpstreamError,
};
use crate::{
    config::{ApiKind, ModelProfile, ProviderId, StartupError},
    json::TokioYield,
    ledger::finance::{FinalUsage, UsageCategory},
    stream::{Event, SseDecoder},
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use m_gated_adapter_kit::openai as kit;
use m_gated_adapter_openai as adapter;

/// profile의 API에 맞는 규칙으로 요청 본문을 검사한다. Chat Completions와 Embeddings 말고는
/// Responses 규칙이다.
pub async fn validate(body: &[u8], profile: &ModelProfile) -> Result<RequestMetadata, ApiError> {
    let api = match profile.api {
        ApiKind::OpenaiChat => adapter::Api::Chat,
        ApiKind::Embeddings => adapter::Api::Embeddings,
        _ => adapter::Api::Responses,
    };
    validate_api(api, body, profile).await
}
/// 이 API의 규칙으로 요청 본문을 검사한다. 긴 본문은 tokio에 실행을 넘기며 읽는다.
pub(super) async fn validate_api(
    api: adapter::Api,
    body: &[u8],
    profile: &ModelProfile,
) -> Result<RequestMetadata, ApiError> {
    Ok(adapter::validate_with(api, body, &port::adapter_profile(profile), TokioYield).await?)
}
/// 비스트림 응답 본문을 읽는다.
pub(super) async fn read_api(
    api: adapter::Api,
    status: StatusCode,
    body: &[u8],
    request: &[u8],
    profile: &ModelProfile,
) -> Result<Reading, UpstreamError> {
    let r = adapter::read_with(
        api,
        status.as_u16(),
        body,
        request,
        &port::adapter_profile(profile),
        &mut TokioYield,
    )
    .await?;
    Ok(Reading {
        usage: r.usage.and_then(port::final_usage),
        counted_input_tokens: None,
        failed: r.failed,
    })
}
/// 스트림 읽기. Embeddings는 스트림이 없다.
pub(super) fn stream_api(
    api: adapter::Api,
    profile: &ModelProfile,
) -> Option<Box<dyn port::StreamReader>> {
    adapter::Stream::new(api, &port::adapter_profile(profile))
        .map(|s| Box::new(Stream(s)) as Box<dyn port::StreamReader>)
}
/// OpenAI 호환 API가 업스트림에 보내는 클라이언트 헤더. JSON `content-type`만 보낸다.
pub fn approved_headers(headers: &HeaderMap) -> Result<HeaderMap, ApiError> {
    let names = kit::approved_headers(&port::adapter_headers(headers))?;
    Ok(port::select_headers(headers, &names))
}

/// OpenAI 호환 Provider(OpenAI, DeepSeek, TypeSafe)가 같이 쓰는 인증 헤더.
pub(super) fn bearer(secret: &str) -> Option<(HeaderName, HeaderValue)> {
    Some((
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {secret}")).ok()?,
    ))
}

/// OpenAI 어댑터.
pub struct Openai;
pub static PROVIDER: Openai = Openai;
static APIS: [&dyn Api; 3] = [
    &RESPONSES,
    &super::openai_chat::CHAT,
    &super::embeddings::EMBEDDINGS,
];
impl port::Provider for Openai {
    fn id(&self) -> ProviderId {
        ProviderId::Openai
    }
    fn origin(&self) -> &'static str {
        adapter::ORIGIN
    }
    fn apis(&self) -> &'static [&'static dyn Api] {
        &APIS
    }
    fn auth_header(&self, secret: &str) -> Option<(HeaderName, HeaderValue)> {
        bearer(secret)
    }
    fn accepts_key(&self, secret: &str) -> bool {
        adapter::accepts_key(secret)
    }
    fn verify_path(&self) -> &'static str {
        adapter::VERIFY_PATH
    }
    fn check_profile(&self, profile: &ModelProfile) -> Result<(), StartupError> {
        Ok(adapter::check_profile(&port::adapter_profile(profile))?)
    }
    fn models_dev_name(&self) -> Option<&'static str> {
        adapter::MODELS_DEV_NAME
    }
}

/// OpenAI Responses API.
pub struct Responses;
static RESPONSES: Responses = Responses;
const API: adapter::Api = adapter::Api::Responses;
impl Api for Responses {
    fn kind(&self) -> ApiKind {
        ApiKind::Responses
    }
    fn routes(&self) -> &'static [&'static str] {
        API.routes()
    }
    fn upstream_path(&self) -> &'static str {
        API.upstream_path()
    }
    fn error_format(&self) -> ErrorFormat {
        adapter::ERROR_FORMAT
    }
    fn validate<'a>(
        &'a self,
        body: &'a [u8],
        profile: &'a ModelProfile,
    ) -> BoxFuture<'a, Result<RequestMetadata, ApiError>> {
        Box::pin(validate(body, profile))
    }
    fn approved_headers(
        &self,
        incoming: &HeaderMap,
        _: &ModelProfile,
    ) -> Result<HeaderMap, ApiError> {
        approved_headers(incoming)
    }
    fn read<'a>(
        &'a self,
        status: StatusCode,
        body: &'a [u8],
        request: &'a [u8],
        profile: &'a ModelProfile,
    ) -> BoxFuture<'a, Result<Reading, UpstreamError>> {
        Box::pin(read_api(API, status, body, request, profile))
    }
    fn stream(&self, profile: &ModelProfile) -> Option<Box<dyn port::StreamReader>> {
        stream_api(API, profile)
    }
    fn token_rule(&self, _profile: &ModelProfile) -> &'static str {
        API.token_rule()
    }
    fn usage_categories(&self) -> &'static [UsageCategory] {
        API.usage_categories()
    }
    fn catalog(&self) -> bool {
        API.catalog()
    }
}
/// Responses와 Chat Completions의 스트림 읽기.
struct Stream(adapter::Stream);
impl port::StreamReader for Stream {
    fn decoder(&self) -> SseDecoder {
        self.0.decoder()
    }
    fn accept(&mut self, event: Event) -> Result<port::Step, UpstreamError> {
        Ok(self.0.accept(event)?)
    }
    fn final_usage(&self) -> Option<FinalUsage> {
        self.0.final_usage().and_then(port::final_usage)
    }
    fn provider_error(&self) -> bool {
        self.0.provider_error()
    }
    fn close(&self) -> Result<(Option<FinalUsage>, bool), UpstreamError> {
        let (usage, failed) = self.0.close()?;
        Ok((usage.and_then(port::final_usage), failed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::finance::PricingSnapshot;
    fn profile() -> ModelProfile {
        serde_json::from_value(serde_json::json!({"model":"synthetic-text","api":"responses","profile_version":"responses-fixture-v1","pricing_version":"responses-price-v1","fixture_version":"responses-native-v1","actual_models":["synthetic-text"],"features":["client_tools","thinking","cache","json_schema","inline_png"],"max_input_tokens":4096,"max_output_tokens":1024,"max_body_bytes":8388608,"max_response_bytes":16777216,"rates":{"input":"1.000000000000","output":"2.000000000000","cache_read":"0.500000000000"}})).unwrap()
    }
    fn native(status: &str, reason: Option<&str>) -> String {
        serde_json::json!({"id":"resp_fixture","object":"response","model":"synthetic-text","status":status,"output":[],"incomplete_details":reason.map(|r|serde_json::json!({"reason":r})),"usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cached_tokens":40},"output_tokens_details":{"reasoning_tokens":10}}}).to_string()
    }
    #[tokio::test]
    async fn responses_terminal_usage_is_priced_from_included_subsets() {
        for (status, reason) in [
            ("completed", None),
            ("incomplete", Some("max_output_tokens")),
        ] {
            let r = RESPONSES
                .read(
                    StatusCode::OK,
                    native(status, reason).as_bytes(),
                    b"",
                    &profile(),
                )
                .await
                .unwrap();
            let u = r.usage.unwrap();
            // 캐시에서 읽은 40은 `input` 칸에서 빠져 `cache_read` 칸에 들어간다.
            assert_eq!(
                (
                    u.get(UsageCategory::Input),
                    u.get(UsageCategory::Output),
                    u.get(UsageCategory::CacheRead),
                    u.reasoning_tokens
                ),
                (60, 20, 40, Some(10))
            );
            let price = PricingSnapshot::from_profile(ProviderId::Openai, &profile());
            assert_eq!(
                price.price(&u, false).unwrap().to_string(),
                "0.000120000000"
            );
        }
    }
    #[test]
    fn responses_price_snapshot_survives_profile_change_and_preserves_included_counts() {
        let mut profile = profile();
        let snapshot = PricingSnapshot::from_profile(ProviderId::Openai, &profile);
        let usage = FinalUsage::new(
            "synthetic-text",
            [
                (UsageCategory::Input, 60),
                (UsageCategory::Output, 20),
                (UsageCategory::CacheRead, 40),
            ],
            Some(10),
        );
        profile
            .rates
            .insert("input".into(), "3.000000000000".into());
        profile.pricing_version = "new-price-v2".into();
        assert_eq!(
            snapshot.price(&usage, false).unwrap().to_string(),
            "0.000120000000"
        );
        assert_eq!(
            PricingSnapshot::from_profile(ProviderId::Openai, &profile)
                .price(&usage, false)
                .unwrap()
                .to_string(),
            "0.000240000000"
        );
        assert_eq!(snapshot.pricing_version, "responses-price-v1");
    }
}
