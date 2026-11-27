//! 관리 토큰의 비밀값(결정 0014). `mgt_` 뒤에 32바이트 난수의 base64url(43자)이 온다.
//! DB에는 비밀값 전체의 SHA-256만 둔다.
use super::AuthError;
use sha2::{Digest, Sha256};

const PREFIX: &str = "mgt_";

/// 새 비밀값. 발급 응답에서 한 번만 보여 준다.
pub fn generate() -> Result<String, AuthError> {
    use base64::Engine;
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| AuthError::DependencyUnavailable)?;
    Ok(format!(
        "{PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    ))
}
/// 관리 토큰으로 보이는 자격 증명인지. 머리만 본다.
pub fn is_token(token: &str) -> bool {
    token.starts_with(PREFIX)
}
/// 비밀값의 모양. 이 모양이 아니면 DB를 보지 않는다.
pub fn plausible(token: &str) -> bool {
    token.strip_prefix(PREFIX).is_some_and(|body| {
        body.len() == 43
            && body
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}
pub fn hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_tokens_are_plausible_and_distinct() {
        let a = generate().unwrap();
        let b = generate().unwrap();
        assert_ne!(a, b);
        assert!(is_token(&a) && plausible(&a));
        assert_ne!(hash(&a), hash(&b));
        for bad in [
            "mgt_".to_owned(),
            format!("{a}x"),
            a[..a.len() - 1].to_owned(),
            a.replace("mgt_", "mgs_"),
            format!("mgt_{}", "+".repeat(43)),
        ] {
            assert!(!plausible(&bad), "{bad}");
        }
    }
}
