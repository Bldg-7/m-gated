//! 응답 JSON/SSE 검사. 검사기는 adapter-kit에 있고(결정 0009), 본체는 64 KiB마다 tokio에 실행을 넘긴다.
pub use m_gated_adapter_kit::stream::*;
use std::future::Future;

/// [`SseDecoder`]에 바이트를 넣으면서 64 KiB마다 tokio에 실행을 넘긴다.
pub trait Feed {
    fn feed(
        &mut self,
        bytes: &[u8],
        event: impl FnMut(Event) -> Result<(), ParseError>,
    ) -> impl Future<Output = Result<(), ParseError>>;
}
impl Feed for SseDecoder {
    async fn feed(
        &mut self,
        bytes: &[u8],
        event: impl FnMut(Event) -> Result<(), ParseError>,
    ) -> Result<(), ParseError> {
        self.feed_with(bytes, &mut crate::json::TokioYield, event)
            .await
    }
}
