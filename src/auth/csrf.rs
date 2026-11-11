use super::{AuthError, CredentialSource};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    v: u8,
    nonce: String,
    iat: i64,
    exp: i64,
    access_token_sha256: String,
}
fn mac(key: &[u8], body: &[u8]) -> Hmac<Sha256> {
    let mut h = Hmac::<Sha256>::new_from_slice(key).expect("HMAC key");
    h.update(b"m-gated-csrf-v1\0");
    h.update(body);
    h
}
fn digest(access: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(access.as_bytes()))
}
pub fn issue(key: &[u8], access: &str, now: i64, jwt_exp: i64) -> Result<(String, i64), AuthError> {
    if jwt_exp <= now {
        return Err(AuthError::InvalidCredentials);
    }
    let mut nonce = [0; 16];
    getrandom::getrandom(&mut nonce).map_err(|_| AuthError::DependencyUnavailable)?;
    let exp = jwt_exp.min(now.saturating_add(300));
    let body = serde_json::to_vec(&Payload {
        v: 1,
        nonce: URL_SAFE_NO_PAD.encode(nonce),
        iat: now,
        exp,
        access_token_sha256: digest(access),
    })
    .map_err(|_| AuthError::DependencyUnavailable)?;
    Ok((
        format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&body),
            URL_SAFE_NO_PAD.encode(mac(key, &body).finalize().into_bytes())
        ),
        exp,
    ))
}
pub fn verify(key: &[u8], token: &str, access: &str, now: i64, jwt_exp: i64) -> bool {
    if token.len() > 2048 {
        return false;
    }
    let Some((a, b)) = token.split_once('.') else {
        return false;
    };
    let (Ok(body), Ok(tag)) = (URL_SAFE_NO_PAD.decode(a), URL_SAFE_NO_PAD.decode(b)) else {
        return false;
    };
    if mac(key, &body).verify_slice(&tag).is_err() {
        return false;
    }
    let Ok(p) = serde_json::from_slice::<Payload>(&body) else {
        return false;
    };
    p.v == 1
        && p.iat <= now
        && now < p.exp
        && p.exp <= jwt_exp
        && p.exp > p.iat
        && p.exp.checked_sub(p.iat).is_some_and(|age| age <= 300)
        && URL_SAFE_NO_PAD.decode(p.nonce).is_ok_and(|n| n.len() >= 16)
        && p.access_token_sha256
            .as_bytes()
            .ct_eq(digest(access).as_bytes())
            .unwrap_u8()
            == 1
}
pub fn required(source: CredentialSource) -> bool {
    source == CredentialSource::Cookie
}
#[cfg(test)]
mod tests {
    use super::*;
    fn signed(key: &[u8], payload: serde_json::Value) -> String {
        let body = serde_json::to_vec(&payload).unwrap();
        format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&body),
            URL_SAFE_NO_PAD.encode(mac(key, &body).finalize().into_bytes())
        )
    }
    #[test]
    fn csrf_rejects_signed_time_overflow_and_invalid_payloads_without_panic() {
        let key = [7; 32];
        let valid = serde_json::json!({"v":1,"nonce":URL_SAFE_NO_PAD.encode([0;16]),"iat":100,"exp":400,"access_token_sha256":digest("a")});
        assert!(verify(&key, &signed(&key, valid.clone()), "a", 100, 400));
        for (field, value) in [
            ("iat", serde_json::json!(i64::MIN)),
            ("iat", serde_json::json!(101)),
            ("exp", serde_json::json!(401)),
            ("exp", serde_json::json!(100)),
            ("v", serde_json::json!(2)),
            ("nonce", serde_json::json!(URL_SAFE_NO_PAD.encode([0; 15]))),
            ("nonce", serde_json::json!("not!base64")),
            ("access_token_sha256", serde_json::json!(digest("b"))),
            ("unknown", serde_json::json!(true)),
        ] {
            let mut p = valid.clone();
            p[field] = value;
            assert!(
                !verify(&key, &signed(&key, p), "a", 100, i64::MAX),
                "{field}"
            );
        }
        let body = serde_json::to_vec(&valid).unwrap();
        let mut cursor = Hmac::<Sha256>::new_from_slice(&key).unwrap();
        cursor.update(b"m-gated-cursor-v1\0");
        cursor.update(&body);
        let token = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&body),
            URL_SAFE_NO_PAD.encode(cursor.finalize().into_bytes())
        );
        assert!(!verify(&key, &token, "a", 100, 400));
    }
    #[test]
    fn csrf_is_bound_to_access_and_actual_expiry() {
        let (token, exp) = issue(&[7; 32], "a", 100, 200).unwrap();
        assert_eq!(exp, 200);
        assert!(verify(&[7; 32], &token, "a", 199, 200));
        assert!(!verify(&[7; 32], &token, "b", 199, 200));
        assert!(!verify(&[7; 32], &token, "a", 200, 200));
        assert!(!verify(&[8; 32], &token, "a", 199, 200));
        assert!(issue(&[7; 32], "a", 200, 200).is_err());
    }
}
