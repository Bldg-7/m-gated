//! 이메일/비밀번호 로그인(결정 0011)의 토큰, 비밀번호 해시, 메일.
use super::AuthError;
use sha2::{Digest, Sha256};
use std::{future::Future, pin::Pin, sync::Mutex, time::Duration};

/// 비밀번호 길이(글자 수). 글자 종류 규칙은 없다.
pub const PASSWORD_CHARS: std::ops::RangeInclusive<usize> = 12..=128;
/// 세션 수명. 로그인한 때부터 센다.
pub const SESSION_SECONDS: i64 = 12 * 3600;
/// 세션 쿠키 이름. 비밀번호 방식에서만 쓴다.
pub const SESSION_COOKIE: &str = "mgated_session";
/// 세션 토큰 머리. 가상 키(`mgw_`)·SSO 토큰과 섞이지 않게 한다.
const SESSION_PREFIX: &str = "mgs_";

/// 일회용 링크의 종류와 수명.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind {
    Invitation,
    Reset,
    Setup,
}
impl LinkKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Invitation => "invitation",
            Self::Reset => "reset",
            Self::Setup => "setup",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "invitation" => Some(Self::Invitation),
            "reset" => Some(Self::Reset),
            "setup" => Some(Self::Setup),
            _ => None,
        }
    }
    /// 초대는 7일, 재설정과 첫 관리자 설정은 24시간.
    pub fn seconds(self) -> i64 {
        match self {
            Self::Invitation => 7 * 24 * 3600,
            Self::Reset | Self::Setup => 24 * 3600,
        }
    }
}

/// 32바이트 무작위 값을 base64url(패딩 없음)로 쓴다.
fn random_token() -> Result<String, AuthError> {
    use base64::Engine;
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| AuthError::DependencyUnavailable)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}
/// 링크 토큰. URL fragment에 넣는다.
pub fn link_token() -> Result<String, AuthError> {
    random_token()
}
/// 세션 토큰. 쿠키에 넣는다.
pub fn session_token() -> Result<String, AuthError> {
    Ok(format!("{SESSION_PREFIX}{}", random_token()?))
}
pub fn is_session_token(token: &str) -> bool {
    token.starts_with(SESSION_PREFIX)
}
/// DB에는 토큰의 SHA-256만 둔다.
pub fn token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}
/// 링크·세션 토큰의 모양. 이 모양이 아니면 DB를 보지 않는다.
pub fn plausible_token(token: &str) -> bool {
    let body = token.strip_prefix(SESSION_PREFIX).unwrap_or(token);
    body.len() == 43
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// 비밀번호 길이 규칙(12~128자).
pub fn password_allowed(password: &str) -> bool {
    PASSWORD_CHARS.contains(&password.chars().count())
}

fn hasher() -> argon2::Argon2<'static> {
    // OWASP 권장값: 메모리 19 MiB, 2회, 병렬 1.
    let params = argon2::Params::new(19 * 1024, 2, 1, None).expect("고정 argon2 매개변수");
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
}
/// 비밀번호의 argon2id 해시(PHC 문자열). CPU를 오래 쓰므로 블로킹 스레드에서 돈다.
pub async fn hash_password(password: String) -> Result<String, AuthError> {
    tokio::task::spawn_blocking(move || {
        use argon2::password_hash::PasswordHasher;
        hasher()
            .hash_password(password.as_bytes())
            .map(|h| h.to_string())
            .map_err(|_| AuthError::DependencyUnavailable)
    })
    .await
    .map_err(|_| AuthError::DependencyUnavailable)?
}
/// 비밀번호를 해시와 비교한다. 해시를 읽을 수 없으면 틀린 것으로 본다.
pub async fn verify_password(password: String, phc: String) -> Result<bool, AuthError> {
    tokio::task::spawn_blocking(move || {
        use argon2::password_hash::{PasswordVerifier, phc::PasswordHash};
        let Ok(parsed) = PasswordHash::new(&phc) else {
            return false;
        };
        hasher()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
    .await
    .map_err(|_| AuthError::DependencyUnavailable)
}

/// 보낼 메일 한 통.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mail {
    pub to: String,
    pub subject: String,
    pub body: String,
}
/// 메일 보내기. 실제 서버는 SMTP, 시험은 메모리에 모은다.
pub trait Mailer: Send + Sync {
    fn send<'a>(&'a self, mail: Mail) -> Pin<Box<dyn Future<Output = Result<(), ()>> + Send + 'a>>;
}

/// SMTP로 보낸다. 포트 465는 처음부터 TLS, 그 밖의 포트는 STARTTLS가 필수다. 10초 안에 끝나야 한다.
pub struct SmtpMailer {
    transport: lettre::AsyncSmtpTransport<lettre::Tokio1Executor>,
    from: lettre::message::Mailbox,
}
impl SmtpMailer {
    pub fn from_config(
        smtp: &crate::config::SmtpConfig,
        ca_certificate: Option<&[u8]>,
    ) -> Result<Self, crate::config::StartupError> {
        use crate::config::StartupError;
        use lettre::transport::smtp::{
            authentication::Credentials,
            client::{Certificate, Tls, TlsParameters},
        };
        let mut tls = TlsParameters::builder(smtp.host.clone());
        if let Some(pem) = ca_certificate {
            tls = tls.add_root_certificate(
                Certificate::from_pem(pem).map_err(|_| StartupError::InvalidSmtp)?,
            );
        }
        let tls = tls.build_rustls().map_err(|_| StartupError::InvalidSmtp)?;
        let transport =
            lettre::AsyncSmtpTransport::<lettre::Tokio1Executor>::builder_dangerous(&smtp.host)
                .port(smtp.port)
                .tls(if smtp.port == 465 {
                    Tls::Wrapper(tls)
                } else {
                    Tls::Required(tls)
                })
                .credentials(Credentials::new(
                    smtp.username.clone(),
                    smtp.password.expose().to_owned(),
                ))
                .timeout(Some(Duration::from_secs(10)))
                .build();
        Ok(Self {
            transport,
            from: smtp.from.parse().map_err(|_| StartupError::InvalidSmtp)?,
        })
    }
}
impl Mailer for SmtpMailer {
    fn send<'a>(&'a self, mail: Mail) -> Pin<Box<dyn Future<Output = Result<(), ()>> + Send + 'a>> {
        Box::pin(async move {
            use lettre::AsyncTransport;
            let message = lettre::Message::builder()
                .from(self.from.clone())
                .to(mail.to.parse().map_err(|_| ())?)
                .subject(mail.subject)
                .header(lettre::message::header::ContentType::TEXT_PLAIN)
                .body(mail.body)
                .map_err(|_| ())?;
            tokio::time::timeout(Duration::from_secs(10), self.transport.send(message))
                .await
                .map_err(|_| ())?
                .map(|_| ())
                .map_err(|_| ())
        })
    }
}

/// 시험용. 보낸 메일을 모은다. `fail`을 켜면 보내기가 실패한다.
#[derive(Default)]
pub struct MemoryMailer {
    pub sent: Mutex<Vec<Mail>>,
    pub fail: std::sync::atomic::AtomicBool,
}
impl Mailer for MemoryMailer {
    fn send<'a>(&'a self, mail: Mail) -> Pin<Box<dyn Future<Output = Result<(), ()>> + Send + 'a>> {
        Box::pin(async move {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(());
            }
            self.sent.lock().expect("메일 목록 잠금").push(mail);
            Ok(())
        })
    }
}

/// 비밀번호 설정 화면 주소. 토큰은 URL fragment에 넣어 서버 로그와 Referer에 남지 않게 한다.
pub fn link_url(origin: &str, token: &str) -> String {
    format!(
        "{}/dashboard/password#token={token}",
        origin.trim_end_matches('/')
    )
}
/// 초대·재설정 메일. 한국어로 쓴다.
pub fn link_mail(kind: LinkKind, to: &str, origin: &str, token: &str) -> Mail {
    let url = link_url(origin, token);
    let (subject, lead, valid) = match kind {
        LinkKind::Invitation => (
            "[m-gated] 초대",
            "m-gated LLM 게이트웨이에 초대되었습니다. 아래 링크에서 비밀번호를 정하면 로그인할 수 있습니다.",
            "이 링크는 7일 동안, 한 번만 쓸 수 있습니다.",
        ),
        LinkKind::Reset | LinkKind::Setup => (
            "[m-gated] 비밀번호 재설정",
            "관리자가 m-gated 비밀번호 재설정 링크를 보냈습니다. 아래 링크에서 새 비밀번호를 정하세요.",
            "이 링크는 24시간 동안, 한 번만 쓸 수 있습니다. 요청한 적이 없다면 관리자에게 알리세요.",
        ),
    };
    Mail {
        to: to.to_owned(),
        subject: subject.to_owned(),
        body: format!("{lead}\n\n{url}\n\n{valid}\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn hashes_verify_and_tokens_have_the_expected_shape() {
        let phc = hash_password("correct horse battery".into()).await.unwrap();
        assert!(phc.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        assert!(
            verify_password("correct horse battery".into(), phc.clone())
                .await
                .unwrap()
        );
        assert!(
            !verify_password("wrong horse battery".into(), phc)
                .await
                .unwrap()
        );
        assert!(
            !verify_password("x".into(), "not a hash".into())
                .await
                .unwrap()
        );
        let s = session_token().unwrap();
        assert!(is_session_token(&s) && plausible_token(&s));
        let l = link_token().unwrap();
        assert!(!is_session_token(&l) && plausible_token(&l));
        assert!(!plausible_token("mgs_short"));
        assert!(password_allowed(&"가".repeat(12)) && !password_allowed(&"a".repeat(11)));
        assert!(!password_allowed(&"a".repeat(129)));
        assert_eq!(
            link_url("https://gw.example.test/", "abc"),
            "https://gw.example.test/dashboard/password#token=abc"
        );
    }
}
