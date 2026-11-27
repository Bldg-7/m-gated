//! OpenAI Chat Completions API를 port에 연결한다(결정 0013). 규칙은 `adapters/openai`의 `chat`에 있다.
use super::{
    openai::{approved_headers, read_api, stream_api, validate_api},
    port::{self, Api, ApiError, BoxFuture, ErrorFormat, Reading, RequestMetadata},
    transport::UpstreamError,
};
use crate::{
    config::{ApiKind, ModelProfile},
    ledger::finance::UsageCategory,
};
use axum::http::{HeaderMap, StatusCode};
use m_gated_adapter_openai as adapter;

/// OpenAI Chat Completions API.
pub struct Chat;
pub(super) static CHAT: Chat = Chat;
const API: adapter::Api = adapter::Api::Chat;
impl Api for Chat {
    fn kind(&self) -> ApiKind {
        ApiKind::OpenaiChat
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
        Box::pin(validate_api(API, body, profile))
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
