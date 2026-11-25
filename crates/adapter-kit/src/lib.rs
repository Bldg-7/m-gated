//! m-gated 본체와 WASM 어댑터가 같이 쓰는 코드(결정 0009). tokio를 쓰지 않는다.
//! 긴 검사 중에 실행을 넘기는 방법은 쓰는 쪽이 [`json::Cooperate`]로 정한다.
pub mod adapter;
pub mod json;
pub mod openai;
pub mod stream;
