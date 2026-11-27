//! 응답과 요청에 든 base64 값을 푼다(결정 0013). 본체와 WASM 어댑터가 같은 규칙을 쓴다.
use base64::Engine;

/// 표준 base64(RFC 4648, `=` 채움) 문자열을 푼다. 형식이 틀리면 None이다.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}
/// 표준 base64 4글자 한 덩어리를 푼다. 푼 바이트 수를 돌려준다. 형식이 틀리면 None이다.
pub fn base64_decode_block(block: [u8; 4], out: &mut [u8; 3]) -> Option<usize> {
    base64::engine::general_purpose::STANDARD
        .decode_slice(block, out)
        .ok()
}
