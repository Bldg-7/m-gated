//! DeepSeek 어댑터 crate(`adapters/deepseek`)를 port에 연결한다. DeepSeek 규칙은 그 crate에 있다.
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
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use m_gated_adapter_deepseek as adapter;

/// DeepSeek 요청 본문을 검사한다.
pub async fn validate(body: &[u8], profile: &ModelProfile) -> Result<RequestMetadata, ApiError> {
    if profile.api != ApiKind::DeepseekChat {
        return Err(ApiError::UnsupportedOption);
    }
    Ok(adapter::validate_with(body, &port::adapter_profile(profile), TokioYield).await?)
}

/// DeepSeek 어댑터.
pub struct Deepseek;
pub static PROVIDER: Deepseek = Deepseek;
static APIS: [&dyn Api; 1] = [&CHAT];
impl port::Provider for Deepseek {
    fn id(&self) -> ProviderId {
        ProviderId::Deepseek
    }
    fn origin(&self) -> &'static str {
        adapter::ORIGIN
    }
    fn apis(&self) -> &'static [&'static dyn Api] {
        &APIS
    }
    fn auth_header(&self, secret: &str) -> Option<(HeaderName, HeaderValue)> {
        super::openai::bearer(secret)
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
}
/// DeepSeek Chat Completions API.
pub struct Chat;
static CHAT: Chat = Chat;
impl Api for Chat {
    fn kind(&self) -> ApiKind {
        ApiKind::DeepseekChat
    }
    fn routes(&self) -> &'static [&'static str] {
        &[adapter::ROUTE]
    }
    fn upstream_path(&self) -> &'static str {
        adapter::UPSTREAM_PATH
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
        let names = adapter::approved_headers(&port::adapter_headers(incoming))?;
        Ok(port::select_headers(incoming, &names))
    }
    fn read<'a>(
        &'a self,
        status: StatusCode,
        body: &'a [u8],
        _request: &'a [u8],
        _profile: &'a ModelProfile,
    ) -> BoxFuture<'a, Result<Reading, UpstreamError>> {
        Box::pin(async move {
            let r = adapter::read_with(status.as_u16(), body, &mut TokioYield).await?;
            Ok(Reading {
                usage: r.usage.and_then(port::final_usage),
                counted_input_tokens: None,
                failed: r.failed,
            })
        })
    }
    fn stream(&self, _profile: &ModelProfile) -> Option<Box<dyn port::StreamReader>> {
        Some(Box::new(Stream(adapter::Stream::default())))
    }
    fn token_rule(&self, _profile: &ModelProfile) -> &'static str {
        adapter::TOKEN_RULE
    }
    fn usage_categories(&self) -> &'static [UsageCategory] {
        adapter::USAGE_CATEGORIES
    }
}
struct Stream(adapter::Stream);
impl port::StreamReader for Stream {
    fn decoder(&self) -> SseDecoder {
        adapter::decoder()
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
