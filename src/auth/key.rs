use super::AuthError;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

pub struct VirtualSecret {
    pub id: Uuid,
    secret: [u8; 32],
}
impl VirtualSecret {
    pub fn generate() -> Result<Self, AuthError> {
        let mut secret = [0; 32];
        getrandom::getrandom(&mut secret).map_err(|_| AuthError::DependencyUnavailable)?;
        Ok(Self {
            id: Uuid::new_v4(),
            secret,
        })
    }
    pub fn parse(raw: &str) -> Result<Self, AuthError> {
        let invalid = || AuthError::InvalidCredentials;
        if raw.len() != 83 || !raw.starts_with("mgw_v1_") || raw.as_bytes()[39] != b'_' {
            return Err(invalid());
        }
        let id = &raw[7..39];
        if !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(invalid());
        }
        let secret: [u8; 32] = URL_SAFE_NO_PAD
            .decode(&raw[40..])
            .map_err(|_| invalid())?
            .try_into()
            .map_err(|_| invalid())?;
        if URL_SAFE_NO_PAD.encode(secret) != raw[40..] {
            return Err(invalid());
        }
        Ok(Self {
            id: Uuid::parse_str(id).map_err(|_| invalid())?,
            secret,
        })
    }
    pub fn expose_once(&self) -> String {
        format!(
            "mgw_v1_{}_{}",
            self.id.simple(),
            URL_SAFE_NO_PAD.encode(self.secret)
        )
    }
    pub fn hash(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"m-gated-key-v1");
        hash.update(self.id.as_bytes());
        hash.update(self.secret);
        hash.finalize().into()
    }
    pub fn matches(&self, stored: &[u8]) -> bool {
        self.hash().as_slice().ct_eq(stored).unwrap_u8() == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn virtual_key_strict_format_and_hash() {
        let a = VirtualSecret::generate().unwrap();
        let b = VirtualSecret::generate().unwrap();
        assert_ne!(a.hash(), b.hash());
        let raw = a.expose_once();
        assert_eq!(raw.len(), 83);
        assert!(VirtualSecret::parse(&raw).unwrap().matches(&a.hash()));
        for bad in [
            raw.to_uppercase(),
            format!("{raw}="),
            raw[..82].to_owned(),
            raw.replace("mgw_v1_", "mgw_v2_"),
        ] {
            assert!(VirtualSecret::parse(&bad).is_err());
        }
    }
}
