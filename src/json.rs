//! 요청 본문 JSON 검사. 검사기는 adapter-kit에 있고(결정 0009), 본체는 64 KiB마다 tokio에 실행을 넘긴다.
use m_gated_adapter_kit::json::{Cooperate, scan_with};
pub use m_gated_adapter_kit::json::{JsonError, Kind, Visitor, string};
use std::future::Future;

/// 긴 검사 중에 tokio에 실행을 넘긴다.
pub(crate) struct TokioYield;
impl Cooperate for TokioYield {
    fn cooperate(&mut self) -> impl Future<Output = ()> + Send {
        tokio::task::yield_now()
    }
}
pub async fn scan(bytes: &[u8], visitor: &mut impl Visitor) -> Result<(), JsonError> {
    scan_with(bytes, visitor, TokioYield).await
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Check;
    impl Visitor for Check {
        fn value(&mut self, _: &[String], _: Kind, _: &str) -> Result<(), JsonError> {
            Ok(())
        }
    }
    #[test]
    fn parser_frames_fit_the_reserved_fixed_budget() {
        assert!(
            m_gated_adapter_kit::json::parser_frame_size(TokioYield) <= 1024,
            "recursive parser frame exceeded fixed budget"
        );
    }
    #[tokio::test]
    async fn large_string_and_whitespace_yield_before_completion() {
        use std::task::Poll;
        for raw in [
            format!(r#"{{"text":"{}"}}"#, "a".repeat(1024 * 1024)),
            format!("{}{{}}", " ".repeat(1024 * 1024)),
        ] {
            let mut visitor = Check;
            let mut future = Box::pin(scan(raw.as_bytes(), &mut visitor));
            std::future::poll_fn(|cx| {
                assert!(
                    future.as_mut().poll(cx).is_pending(),
                    "large scan monopolized first poll"
                );
                Poll::Ready(())
            })
            .await;
            future.await.unwrap();
        }
    }
}
