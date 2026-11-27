//! OpenAI Embeddings API를 port에 연결한다(결정 0013). 규칙은 `adapters/openai`의 `embeddings`에 있다.
use super::{
    openai::{approved_headers, read_api, validate_api},
    port::{Api, ApiError, BoxFuture, ErrorFormat, Reading, RequestMetadata},
    transport::UpstreamError,
};
use crate::{
    config::{ApiKind, ModelProfile},
    ledger::finance::UsageCategory,
};
use axum::http::{HeaderMap, StatusCode};
use m_gated_adapter_openai as adapter;

/// OpenAI Embeddings API. 스트림이 없다.
pub struct Embeddings;
pub(super) static EMBEDDINGS: Embeddings = Embeddings;
const API: adapter::Api = adapter::Api::Embeddings;
impl Api for Embeddings {
    fn kind(&self) -> ApiKind {
        ApiKind::Embeddings
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
    /// 성공 응답은 요청이 정한 벡터 수와 차원으로 검사한다. 오류 응답은 Responses와 같이 읽는다.
    fn read<'a>(
        &'a self,
        status: StatusCode,
        body: &'a [u8],
        request: &'a [u8],
        profile: &'a ModelProfile,
    ) -> BoxFuture<'a, Result<Reading, UpstreamError>> {
        Box::pin(read_api(API, status, body, request, profile))
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
