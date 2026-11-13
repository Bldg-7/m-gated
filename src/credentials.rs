//! Provider 회사 키를 DB에 암호화해 두고, 원문은 프로세스 메모리 캐시에서만 다룬다.
//!
//! - root key(`CREDENTIAL_ENCRYPTION_KEY`)에서 HKDF로 암호화·fingerprint·식별자 key를 따로 파생한다.
//! - 암호문은 AES-256-GCM이며 AAD에 Provider 이름과 행 id를 묶어 다른 행으로 옮기면 풀리지 않는다.
//! - 원문을 돌려주는 API는 없다. 캐시는 DB의 active 행이 바뀔 때만 다시 복호화한다.
use crate::{
    config::{ProviderConfig, ProviderId, StartupError, ValidatedConfig},
    ledger::PgIdentityStore,
    providers::transport::NativeClient,
};
use ring::{aead, hkdf, hmac};
use sqlx::Row;
use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, RwLock},
};
use uuid::Uuid;
use zeroize::Zeroizing;

const AAD_PREFIX: &[u8] = b"m-gated/provider-credential/v1";
pub const NONCE_LEN: usize = 12;
pub const KEK_ID_LEN: usize = 8;

struct Len(usize);
impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

pub struct CredentialCipher {
    aead: aead::LessSafeKey,
    fingerprint: hmac::Key,
    kek_id: [u8; KEK_ID_LEN],
}
impl fmt::Debug for CredentialCipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CredentialCipher([REDACTED])")
    }
}
impl CredentialCipher {
    pub fn new(root: &[u8; 32]) -> Self {
        let prk =
            hkdf::Salt::new(hkdf::HKDF_SHA256, b"m-gated credential encryption").extract(root);
        let expand = |info: &[u8], len: usize| {
            let mut out = Zeroizing::new(vec![0u8; len]);
            prk.expand(&[info], Len(len))
                .expect("고정 길이 HKDF")
                .fill(&mut out)
                .expect("고정 길이 HKDF");
            out
        };
        let encryption = expand(b"aead-aes256gcm-v1", 32);
        let fingerprint = expand(b"fingerprint-hmac-sha256-v1", 32);
        let id = expand(b"key-id-v1", KEK_ID_LEN);
        Self {
            aead: aead::LessSafeKey::new(
                aead::UnboundKey::new(&aead::AES_256_GCM, &encryption).expect("32바이트 AES key"),
            ),
            fingerprint: hmac::Key::new(hmac::HMAC_SHA256, &fingerprint),
            kek_id: std::array::from_fn(|i| id[i]),
        }
    }
    /// 어떤 root key로 잠갔는지 구별하는 값. root key 자체는 드러내지 않는다.
    pub fn kek_id(&self) -> [u8; KEK_ID_LEN] {
        self.kek_id
    }
    fn aad(provider: ProviderId, id: Uuid) -> Vec<u8> {
        let mut aad = AAD_PREFIX.to_vec();
        aad.push(0);
        aad.extend_from_slice(provider.name().as_bytes());
        aad.push(0);
        aad.extend_from_slice(id.as_bytes());
        aad
    }
    pub fn seal(
        &self,
        provider: ProviderId,
        id: Uuid,
        secret: &str,
    ) -> Option<([u8; NONCE_LEN], Vec<u8>)> {
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::getrandom(&mut nonce).ok()?;
        let mut buffer = secret.as_bytes().to_vec();
        self.aead
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(Self::aad(provider, id)),
                &mut buffer,
            )
            .ok()?;
        Some((nonce, buffer))
    }
    pub fn open(
        &self,
        provider: ProviderId,
        id: Uuid,
        kek_id: &[u8],
        nonce: &[u8],
        ciphertext: &[u8],
    ) -> Option<Zeroizing<String>> {
        if kek_id != self.kek_id {
            return None;
        }
        let nonce = aead::Nonce::try_assume_unique_for_key(nonce).ok()?;
        let mut buffer = Zeroizing::new(ciphertext.to_vec());
        let plain = self
            .aead
            .open_in_place(nonce, aead::Aad::from(Self::aad(provider, id)), &mut buffer)
            .ok()?;
        let text = std::str::from_utf8(plain).ok()?;
        valid_secret(provider, text).then(|| Zeroizing::new(text.to_owned()))
    }
    /// 같은 키의 재등록을 알아보는 값. root key 없이는 원문을 추측할 수 없다.
    pub fn fingerprint(&self, provider: ProviderId, secret: &str) -> [u8; 32] {
        let mut context = hmac::Context::with_key(&self.fingerprint);
        context.update(provider.name().as_bytes());
        context.update(&[0]);
        context.update(secret.as_bytes());
        let tag = context.sign();
        std::array::from_fn(|i| tag.as_ref()[i])
    }
}

/// 다른 Provider 키를 잘못 붙여 넣는 실수를 막는 형식 검사. 원문은 오류에 담지 않는다.
pub fn valid_secret(provider: ProviderId, secret: &str) -> bool {
    let prefix = match provider {
        ProviderId::Anthropic => "sk-ant-",
        ProviderId::Openai | ProviderId::Deepseek => "sk-",
    };
    (20..=512).contains(&secret.len())
        && secret.bytes().all(|b| b.is_ascii_graphic())
        && secret.starts_with(prefix)
        && !(provider != ProviderId::Anthropic && secret.starts_with("sk-ant-"))
}
/// 화면 표시용 끝 4자리.
pub fn hint(secret: &str) -> String {
    secret[secret.len() - 4..].to_owned()
}

#[derive(Clone)]
pub struct ActiveCredential {
    pub id: Uuid,
    pub version: i64,
    pub secret: Arc<Zeroizing<String>>,
}
#[derive(Clone)]
enum Entry {
    Undecryptable { version: i64 },
    Active(ActiveCredential),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialStatus {
    Active,
    Missing,
    Undecryptable,
}
impl CredentialStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Missing => "missing",
            Self::Undecryptable => "undecryptable",
        }
    }
}
#[derive(Clone, Default)]
struct Snapshot {
    /// 사용 중인 회사 키 전체(id → Provider, 상태).
    by_id: BTreeMap<Uuid, (ProviderId, Entry)>,
    /// Provider별 기본 키. 연결이 없는 가상 키와 SSO 직접 요청이 쓴다.
    defaults: BTreeMap<ProviderId, Uuid>,
}

/// 프로세스의 복호화된 회사 키. 해제되면 원문 메모리를 지운다.
#[derive(Default)]
pub struct CredentialCache {
    entries: RwLock<Snapshot>,
}
impl fmt::Debug for CredentialCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CredentialCache([REDACTED])")
    }
}
impl CredentialCache {
    /// 요청 하나에 쓸 키. 가상 키에 연결된 키가 있으면 그 키, 없으면 Provider의 기본 키.
    /// 연결된 키가 해제됐거나 풀 수 없으면 기본 키로 대신하지 않고 None을 돌려준다.
    pub fn get(&self, provider: ProviderId, bound: Option<Uuid>) -> Option<ActiveCredential> {
        let snapshot = self.entries.read().ok()?;
        let id = bound.or_else(|| snapshot.defaults.get(&provider).copied())?;
        match snapshot.by_id.get(&id)? {
            (p, Entry::Active(active)) if *p == provider => Some(active.clone()),
            _ => None,
        }
    }
    /// Provider 기본 키의 상태.
    pub fn status(&self, provider: ProviderId) -> CredentialStatus {
        let Ok(snapshot) = self.entries.read() else {
            return CredentialStatus::Missing;
        };
        match snapshot
            .defaults
            .get(&provider)
            .and_then(|id| snapshot.by_id.get(id))
        {
            Some((_, Entry::Active(_))) => CredentialStatus::Active,
            Some((_, Entry::Undecryptable { .. })) => CredentialStatus::Undecryptable,
            None => CredentialStatus::Missing,
        }
    }
    /// 테스트 조립 전용: DB 없이 키를 넣고 그 Provider의 기본 키로 둔다.
    pub fn insert_for_test(&self, provider: ProviderId, id: Uuid, secret: &str) {
        let mut snapshot = self.entries.write().expect("캐시 lock");
        snapshot.by_id.insert(
            id,
            (
                provider,
                Entry::Active(ActiveCredential {
                    id,
                    version: 1,
                    secret: Arc::new(Zeroizing::new(secret.to_owned())),
                }),
            ),
        );
        snapshot.defaults.insert(provider, id);
    }
}

/// 등록 확인에 쓰는 Provider client와 캐시·암호화를 함께 들고 있는 단위.
pub struct CredentialService {
    cipher: CredentialCipher,
    cache: Arc<CredentialCache>,
    store: Arc<PgIdentityStore>,
    verifiers: BTreeMap<ProviderId, NativeClient>,
}
impl fmt::Debug for CredentialService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CredentialService([REDACTED])")
    }
}
impl CredentialService {
    pub fn new(
        config: &ValidatedConfig,
        store: Arc<PgIdentityStore>,
    ) -> Result<Arc<Self>, StartupError> {
        let cache = Arc::new(CredentialCache::default());
        let mut verifiers = BTreeMap::new();
        for provider in config.providers() {
            let client = NativeClient::from_config(
                provider,
                config.auth().ca_certificate.as_deref(),
                verifiers.values().next(),
                cache.clone(),
            )
            .map_err(|_| StartupError::InvalidProfile)?;
            verifiers.insert(provider.provider, client);
        }
        Ok(Arc::new(Self {
            cipher: CredentialCipher::new(&config.credential_encryption_key_bytes()),
            cache,
            store,
            verifiers,
        }))
    }
    /// 테스트 조립 전용: 등록 확인을 loopback 합성 Provider로 보낸다.
    pub fn for_test(
        root: &[u8; 32],
        store: Arc<PgIdentityStore>,
        verifiers: Vec<NativeClient>,
    ) -> Arc<Self> {
        Arc::new(Self {
            cipher: CredentialCipher::new(root),
            cache: Arc::new(CredentialCache::default()),
            store,
            verifiers: verifiers.into_iter().map(|c| (c.provider(), c)).collect(),
        })
    }
    pub fn cache(&self) -> Arc<CredentialCache> {
        self.cache.clone()
    }
    pub fn cipher(&self) -> &CredentialCipher {
        &self.cipher
    }
    pub fn store(&self) -> &Arc<PgIdentityStore> {
        &self.store
    }
    pub fn configured(&self) -> impl Iterator<Item = ProviderId> + '_ {
        self.verifiers.keys().copied()
    }
    pub fn verifier(&self, provider: ProviderId) -> Option<&NativeClient> {
        self.verifiers.get(&provider)
    }
    /// DB의 active 행과 캐시를 맞춘다. 바뀐 행만 다시 복호화하고 결과는 telemetry에 남긴다.
    pub async fn refresh(&self) -> Result<(), sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id,provider,version,is_default,kek_id,nonce,ciphertext FROM gw_provider_credentials WHERE state='active'",
        )
        .fetch_all(self.store.pool())
        .await?;
        let current = self
            .cache
            .entries
            .read()
            .map(|e| e.clone())
            .unwrap_or_default();
        let mut next = Snapshot::default();
        for row in &rows {
            let Some(provider) = ProviderId::from_name(&row.get::<String, _>("provider")) else {
                continue;
            };
            if !self.verifiers.contains_key(&provider) {
                continue;
            }
            let id: Uuid = row.get("id");
            let version: i64 = row.get("version");
            let entry = match current.by_id.get(&id) {
                Some((_, Entry::Active(a))) if a.version == version => Entry::Active(a.clone()),
                Some((_, Entry::Undecryptable { version: v })) if *v == version => {
                    Entry::Undecryptable { version }
                }
                _ => match self.cipher.open(
                    provider,
                    id,
                    &row.get::<Vec<u8>, _>("kek_id"),
                    &row.get::<Vec<u8>, _>("nonce"),
                    &row.get::<Vec<u8>, _>("ciphertext"),
                ) {
                    Some(secret) => Entry::Active(ActiveCredential {
                        id,
                        version,
                        secret: Arc::new(secret),
                    }),
                    None => Entry::Undecryptable { version },
                },
            };
            if row.get::<bool, _>("is_default") {
                next.defaults.insert(provider, id);
            }
            next.by_id.insert(id, (provider, entry));
        }
        // 설정된 Provider 중 기본 키가 없거나 풀 수 없는 수 + 풀 수 없는 나머지 키 수.
        let unavailable = self
            .configured()
            .filter(|p| {
                next.defaults
                    .get(p)
                    .and_then(|id| next.by_id.get(id))
                    .is_none_or(|(_, e)| !matches!(e, Entry::Active(_)))
            })
            .count()
            + next
                .by_id
                .iter()
                .filter(|(id, (p, e))| {
                    matches!(e, Entry::Undecryptable { .. }) && next.defaults.get(p) != Some(id)
                })
                .count();
        if let Ok(mut entries) = self.cache.entries.write() {
            *entries = next;
        }
        crate::telemetry::set(
            crate::telemetry::Metric::ProviderCredentialsUnavailable,
            unavailable as u64,
        );
        Ok(())
    }
}

pub fn configured_provider(config: &[ProviderConfig], provider: ProviderId) -> bool {
    config.iter().any(|p| p.provider == provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    const KEY: &str = "sk-ant-synthetic-company-key-0001";
    #[test]
    fn sealed_secret_opens_only_for_same_root_provider_and_row() {
        let cipher = CredentialCipher::new(&[7; 32]);
        let id = Uuid::new_v4();
        let (nonce, sealed) = cipher.seal(ProviderId::Anthropic, id, KEY).unwrap();
        assert!(!sealed.windows(KEY.len()).any(|w| w == KEY.as_bytes()));
        let kek = cipher.kek_id();
        assert_eq!(
            cipher
                .open(ProviderId::Anthropic, id, &kek, &nonce, &sealed)
                .unwrap()
                .as_str(),
            KEY
        );
        // 다른 행·다른 Provider로 옮긴 암호문은 풀리지 않는다.
        assert!(
            cipher
                .open(ProviderId::Anthropic, Uuid::new_v4(), &kek, &nonce, &sealed)
                .is_none()
        );
        assert!(
            cipher
                .open(ProviderId::Openai, id, &kek, &nonce, &sealed)
                .is_none()
        );
        // 다른 root key는 식별자로 먼저 구별되고, 식별자를 맞춰도 풀리지 않는다.
        let other = CredentialCipher::new(&[8; 32]);
        assert_ne!(other.kek_id(), kek);
        assert!(
            other
                .open(ProviderId::Anthropic, id, &other.kek_id(), &nonce, &sealed)
                .is_none()
        );
        let mut tampered = sealed.clone();
        tampered[0] ^= 1;
        assert!(
            cipher
                .open(ProviderId::Anthropic, id, &kek, &nonce, &tampered)
                .is_none()
        );
        // 같은 원문도 nonce가 달라 암호문이 달라진다.
        assert_ne!(
            cipher.seal(ProviderId::Anthropic, id, KEY).unwrap().1,
            sealed
        );
    }
    #[test]
    fn fingerprint_is_keyed_and_provider_scoped() {
        let cipher = CredentialCipher::new(&[7; 32]);
        let a = cipher.fingerprint(ProviderId::Anthropic, KEY);
        assert_eq!(a, cipher.fingerprint(ProviderId::Anthropic, KEY));
        assert_ne!(a, cipher.fingerprint(ProviderId::Openai, KEY));
        assert_ne!(
            a,
            CredentialCipher::new(&[8; 32]).fingerprint(ProviderId::Anthropic, KEY)
        );
    }
    #[test]
    fn secret_format_matches_provider() {
        assert!(valid_secret(ProviderId::Anthropic, KEY));
        assert!(valid_secret(
            ProviderId::Openai,
            "sk-proj-synthetic-openai-0001"
        ));
        assert!(valid_secret(
            ProviderId::Deepseek,
            "sk-synthetic-deepseek-00001"
        ));
        for (provider, value) in [
            (ProviderId::Openai, KEY),
            (ProviderId::Anthropic, "sk-proj-synthetic-openai-0001"),
            (ProviderId::Anthropic, "sk-ant-short"),
            (ProviderId::Anthropic, "sk-ant-has space-in-the-middle"),
            (ProviderId::Anthropic, "sk-ant-한글이-들어간-키-000000"),
            (ProviderId::Deepseek, "pk-synthetic-deepseek-00001"),
        ] {
            assert!(!valid_secret(provider, value), "{value}");
        }
        assert_eq!(hint(KEY), "0001");
    }
}
