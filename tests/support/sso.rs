use axum::{
    Router, body::Body, extract::State, http::StatusCode, response::Response, routing::get,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{Value, json};
use std::{
    io::Write,
    process::{Command, Stdio},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::{Notify, RwLock};

pub struct Signer {
    pem: Vec<u8>,
    pub jwk: Value,
}
impl Signer {
    pub fn new(bits: usize) -> Self {
        let output = Command::new("openssl")
            .env_clear()
            .args([
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                &format!("rsa_keygen_bits:{bits}"),
            ])
            .output()
            .expect("공개 openssl 설치");
        assert!(output.status.success(), "합성 RSA 생성");
        let mut child = Command::new("openssl")
            .env_clear()
            .args(["rsa", "-noout", "-modulus"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&output.stdout)
            .unwrap();
        let modulus = child.wait_with_output().unwrap();
        assert!(modulus.status.success());
        let hex = std::str::from_utf8(&modulus.stdout)
            .unwrap()
            .trim()
            .strip_prefix("Modulus=")
            .unwrap();
        let n: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        Self {
            pem: output.stdout,
            jwk: json!({"kty":"RSA","kid":"key1","n":URL_SAFE_NO_PAD.encode(n),"e":"AQAB","use":"sig","alg":"RS256","key_ops":["verify"]}),
        }
    }
    pub fn shared() -> &'static Self {
        static SIGNER: OnceLock<Signer> = OnceLock::new();
        SIGNER.get_or_init(|| Self::new(2048))
    }
    pub fn sign(&self, claims: &Value, kid: &str) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.into());
        self.sign_header(claims, &header)
    }
    pub fn sign_header(&self, claims: &Value, header: &Header) -> String {
        jsonwebtoken::encode(
            header,
            claims,
            &EncodingKey::from_rsa_pem(&self.pem).unwrap(),
        )
        .unwrap()
    }
    pub fn sign_raw(&self, claims: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","kid":"key1"}"#);
        let payload = URL_SAFE_NO_PAD.encode(claims);
        let input = format!("{header}.{payload}");
        let signature = jsonwebtoken::crypto::sign(
            input.as_bytes(),
            &EncodingKey::from_rsa_pem(&self.pem).unwrap(),
            Algorithm::RS256,
        )
        .unwrap();
        format!("{input}.{signature}")
    }
}
pub fn claims() -> Value {
    json!({"iss":"https://auth.example.test","aud":"https://example.test","sub":"human-1","email":"Alice@Example.test","email_verified":true,"exp":2000000000,"name":"Alice"})
}
pub struct FakeJwks {
    pub calls: AtomicUsize,
    pub response: RwLock<(StatusCode, Vec<u8>)>,
    pub location: RwLock<String>,
    pub entered: Notify,
    pub release: Notify,
    pub hold: std::sync::atomic::AtomicBool,
}
impl FakeJwks {
    pub fn new(keys: Vec<Value>) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            response: RwLock::new((
                StatusCode::OK,
                serde_json::to_vec(&json!({"keys":keys})).unwrap(),
            )),
            location: RwLock::new("http://127.0.0.1:1/never".into()),
            entered: Notify::new(),
            release: Notify::new(),
            hold: std::sync::atomic::AtomicBool::new(false),
        })
    }
    pub async fn set(&self, status: StatusCode, bytes: Vec<u8>) {
        *self.response.write().await = (status, bytes);
    }
    pub async fn keys(&self, keys: Vec<Value>) {
        self.set(
            StatusCode::OK,
            serde_json::to_vec(&json!({"keys":keys})).unwrap(),
        )
        .await;
    }
    pub fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    pub async fn serve(self: &Arc<Self>) -> Server {
        async fn handler(State(s): State<Arc<FakeJwks>>) -> Response {
            s.calls.fetch_add(1, Ordering::SeqCst);
            s.entered.notify_one();
            if s.hold.load(Ordering::SeqCst) {
                s.release.notified().await;
            }
            let (status, bytes) = s.response.read().await.clone();
            Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .header("location", s.location.read().await.clone())
                .body(Body::from(bytes))
                .unwrap()
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new()
            .route("/jwks", get(handler))
            .with_state(self.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Server {
            uri: format!("http://{address}/jwks").parse().unwrap(),
            task,
        }
    }
}
pub struct Server {
    pub uri: url::Url,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub fn credential(token: String) -> m_gated::auth::Credential {
    let mut h = axum::http::HeaderMap::new();
    h.insert("authorization", format!("Bearer {token}").parse().unwrap());
    m_gated::auth::extract_credential(&h, m_gated::auth::AuthRoute::Management, "mgated_access")
        .unwrap()
}
