use super::AuthError;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::DecodingKey;
use serde::Deserialize;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::{
    sync::{Mutex, RwLock},
    time::Instant,
};
use url::Url;

const TTL: Duration = Duration::from_secs(300);
const COOLDOWN: Duration = Duration::from_secs(10);
struct KeySet {
    keys: BTreeMap<String, Arc<DecodingKey>>,
    fetched: Instant,
}
struct Refresh {
    last_attempt: Option<Instant>,
}
/// issuer 하나당 집합 하나와 갱신 잠금 하나만 유지한다. 임의 kid는 저장하지 않는다.
pub struct JwksCache {
    client: reqwest::Client,
    uri: Url,
    set: RwLock<Option<KeySet>>,
    refresh: Mutex<Refresh>,
}
impl JwksCache {
    pub fn new(uri: Url) -> Result<Self, AuthError> {
        Self::with_ca(uri, None)
    }
    pub fn with_ca(uri: Url, ca: Option<&[u8]>) -> Result<Self, AuthError> {
        if uri.scheme() != "https"
            || !uri.username().is_empty()
            || uri.password().is_some()
            || uri.fragment().is_some()
        {
            return Err(AuthError::DependencyUnavailable);
        }
        Self::assemble_with_ca(uri, ca)
    }
    /// 합성 서버를 조립하는 test harness 전용이며 설정/main에는 노출하지 않는다.
    pub fn loopback_for_test(uri: Url) -> Result<Self, AuthError> {
        if uri.scheme() != "http"
            || !matches!(uri.host_str(), Some("127.0.0.1" | "[::1]"))
            || !uri.username().is_empty()
            || uri.password().is_some()
            || uri.fragment().is_some()
        {
            return Err(AuthError::DependencyUnavailable);
        }
        Self::assemble(uri)
    }
    fn assemble(uri: Url) -> Result<Self, AuthError> {
        Self::assemble_with_ca(uri, None)
    }
    fn assemble_with_ca(uri: Url, ca: Option<&[u8]>) -> Result<Self, AuthError> {
        let mut builder = reqwest::Client::builder();
        if let Some(pem) = ca {
            builder = builder.add_root_certificate(
                reqwest::Certificate::from_pem(pem)
                    .map_err(|_| AuthError::DependencyUnavailable)?,
            );
        }
        let client = builder
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .timeout(Duration::from_secs(2))
            .build()
            .map_err(|_| AuthError::DependencyUnavailable)?;
        Ok(Self {
            client,
            uri,
            set: RwLock::new(None),
            refresh: Mutex::new(Refresh { last_attempt: None }),
        })
    }
    pub async fn is_healthy(&self) -> bool {
        self.is_healthy_at(Instant::now()).await
    }
    pub async fn is_healthy_at(&self, now: Instant) -> bool {
        let set = self.set.read().await;
        crate::telemetry::set(
            crate::telemetry::Metric::JwksAge,
            set.as_ref()
                .map_or(300, |s| now.saturating_duration_since(s.fetched).as_secs()),
        );
        set.as_ref()
            .is_some_and(|s| now.saturating_duration_since(s.fetched) < TTL)
    }
    pub async fn refresh_health(&self) -> bool {
        if self.is_healthy().await {
            return true;
        }
        let _ = self.lookup(None, Instant::now()).await;
        self.is_healthy().await
    }
    pub async fn key(&self, kid: &str) -> Result<Arc<DecodingKey>, AuthError> {
        self.key_at(kid, Instant::now()).await
    }
    pub async fn key_at(&self, kid: &str, now: Instant) -> Result<Arc<DecodingKey>, AuthError> {
        self.lookup(Some(kid), now)
            .await?
            .ok_or(AuthError::InvalidCredentials)
    }
    async fn lookup(
        &self,
        kid: Option<&str>,
        now: Instant,
    ) -> Result<Option<Arc<DecodingKey>>, AuthError> {
        let requested_at = now;
        let started = Instant::now();
        if let Some(key) = self.known(kid, now).await {
            return Ok(Some(key));
        }
        // 호출자의 전체 인증 기한은 Authenticator가 별도로 제한한다.
        let mut refresh = self.refresh.lock().await;
        let now = now + started.elapsed();
        if let Some(key) = self.known(kid, now).await {
            return Ok(Some(key));
        }
        if refresh
            .last_attempt
            .is_some_and(|t| now.saturating_duration_since(t) < COOLDOWN)
        {
            return if self.is_healthy_at(now).await {
                Ok(None)
            } else {
                Err(AuthError::DependencyUnavailable)
            };
        }
        refresh.last_attempt = Some(requested_at);
        let mut metric = crate::telemetry::Timer::start(
            crate::telemetry::Metric::JwksFetch,
            crate::telemetry::Metric::JwksMicros,
            crate::telemetry::Metric::JwksFailed,
        );
        match self.fetch().await {
            Ok(keys) => {
                metric.success();
                *self.set.write().await = Some(KeySet {
                    keys,
                    fetched: requested_at,
                });
                Ok(self.known(kid, now).await)
            }
            Err(error) => Err(error),
        }
    }
    async fn known(&self, kid: Option<&str>, now: Instant) -> Option<Arc<DecodingKey>> {
        let set = self.set.read().await;
        let set = set.as_ref()?;
        if now.saturating_duration_since(set.fetched) >= TTL {
            return None;
        }
        set.keys.get(kid?).cloned()
    }
    async fn fetch(&self) -> Result<BTreeMap<String, Arc<DecodingKey>>, AuthError> {
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut response = self
                .client
                .get(self.uri.clone())
                .send()
                .await
                .map_err(|_| AuthError::DependencyUnavailable)?;
            if response.status() != reqwest::StatusCode::OK
                || response.content_length().is_some_and(|n| n > 65536)
            {
                return Err(AuthError::DependencyUnavailable);
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| AuthError::DependencyUnavailable)?
            {
                if bytes.len() + chunk.len() > 65536 {
                    return Err(AuthError::DependencyUnavailable);
                }
                bytes.extend_from_slice(&chunk);
            }
            parse_set(&bytes)
        })
        .await
        .map_err(|_| AuthError::DependencyUnavailable)?
    }
}
#[derive(Deserialize)]
struct WireSet {
    keys: Vec<WireKey>,
}
#[derive(Deserialize)]
struct WireKey {
    kid: String,
    kty: String,
    n: String,
    e: String,
    #[serde(rename = "use", default, deserialize_with = "present_metadata")]
    usage: Option<String>,
    #[serde(default, deserialize_with = "present_metadata")]
    alg: Option<String>,
    #[serde(default, deserialize_with = "present_metadata")]
    key_ops: Option<Vec<String>>,
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}
fn present_metadata<'de, T: Deserialize<'de>, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
fn parse_set(bytes: &[u8]) -> Result<BTreeMap<String, Arc<DecodingKey>>, AuthError> {
    let invalid = || AuthError::DependencyUnavailable;
    let wire: WireSet = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    if wire.keys.is_empty() || wire.keys.len() > 8 {
        return Err(invalid());
    }
    let mut keys = BTreeMap::new();
    for key in wire.keys {
        let n = URL_SAFE_NO_PAD.decode(&key.n).map_err(|_| invalid())?;
        let e = URL_SAFE_NO_PAD.decode(&key.e).map_err(|_| invalid())?;
        if e.is_empty() || e.len() > 5 || e.first() == Some(&0) {
            return Err(invalid());
        }
        // ring0.17의 검증용 exponent 범위만 게시한다. 5bytes 제한으로 변환 overflow도 없다.
        let exponent = e
            .iter()
            .fold(0_u64, |value, byte| (value << 8) | u64::from(*byte));
        let bits = n.len() * 8 - n.first().map_or(0, |b| b.leading_zeros() as usize);
        if key.kid.is_empty()
            || key.kid.len() > 128
            || key.kid.chars().any(char::is_control)
            || key.kty != "RSA"
            || !(2048..=4096).contains(&bits)
            || n.first() == Some(&0)
            || n.last().is_none_or(|b| b & 1 == 0)
            || !(3..=((1_u64 << 33) - 1)).contains(&exponent)
            || exponent & 1 == 0
            || key.usage.as_deref().is_some_and(|v| v != "sig")
            || key.alg.as_deref().is_some_and(|v| v != "RS256")
            || key
                .key_ops
                .as_ref()
                .is_some_and(|v| v.as_slice() != ["verify"])
            || ["d", "p", "q", "dp", "dq", "qi", "oth", "k"]
                .iter()
                .any(|v| key.extra.contains_key(*v))
        {
            return Err(invalid());
        }
        let decoding = DecodingKey::from_rsa_components(&key.n, &key.e).map_err(|_| invalid())?;
        if keys.insert(key.kid, Arc::new(decoding)).is_some() {
            return Err(invalid());
        }
    }
    Ok(keys)
}
