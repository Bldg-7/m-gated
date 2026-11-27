//! 비밀번호 로그인(결정 0011)의 DB 작업: 비밀번호 해시, 일회용 링크, 세션, 로그인 실패.
//! 토큰은 SHA-256만 다룬다. 접근 목록에 없는 이메일은 로그인할 수 없다.
use super::PgIdentityStore;
use crate::auth::{AuthError, password::LinkKind};
use sqlx::Row;
use std::time::Duration;

/// 연속 실패 몇 번에 잠그는지, 몇 초 동안 잠그는지.
const MAX_FAILURES: i32 = 5;
const LOCK_SECONDS: i64 = 15 * 60;

/// 링크가 가리키는 사람과 링크 종류.
pub struct LinkInfo {
    pub email: String,
    pub kind: LinkKind,
    /// 이미 비밀번호를 정한 사람인지. 처음이면 표시 이름도 받는다.
    pub password_set: bool,
    pub expires_at: i64,
}
/// 세션이 가리키는 사람.
pub struct SessionInfo {
    pub email: String,
    pub display_name: String,
    pub expires_at: i64,
}
/// 로그인 시도에 필요한 값. 이메일이 접근 목록에 없거나 비밀번호가 없으면 `hash`가 없다.
pub struct SignInRecord {
    pub hash: Option<String>,
    /// 잠겨 있으면 남은 초(올림). DB 시계로 계산한다.
    pub locked_seconds: Option<i64>,
}

fn db<T>(r: Result<T, sqlx::Error>) -> Result<T, AuthError> {
    r.map_err(|_| AuthError::DatabaseUnavailable)
}

impl PgIdentityStore {
    async fn with_timeout<T>(
        &self,
        work: impl std::future::Future<Output = Result<T, AuthError>>,
    ) -> Result<T, AuthError> {
        let _gate = self
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| AuthError::DatabaseUnavailable)?;
        tokio::time::timeout(Duration::from_secs(3), work)
            .await
            .map_err(|_| AuthError::DatabaseUnavailable)?
    }

    /// 유효한 세션이면 그 사람을 돌려준다. 끝난 세션, 접근 목록에서 빠진 사람, 비밀번호가 없는 사람은 없다.
    pub async fn session(&self, token_hash: &[u8; 32]) -> Result<Option<SessionInfo>, AuthError> {
        self.with_timeout(async {
            let row = db(sqlx::query("SELECT s.email,p.display_name,extract(epoch FROM s.expires_at)::bigint AS expires FROM gw_sessions s JOIN gw_passwords p ON p.email=s.email JOIN gw_access a ON a.email=s.email WHERE s.token_hash=$1 AND s.expires_at>clock_timestamp()")
                .bind(token_hash.as_slice())
                .fetch_optional(&self.pool)
                .await)?;
            Ok(row.map(|r| SessionInfo {
                email: r.get("email"),
                display_name: r.get("display_name"),
                expires_at: r.get("expires"),
            }))
        })
        .await
    }

    /// 로그인에 쓸 해시와 잠금 상태. 이메일은 소문자로 받는다.
    pub async fn sign_in_record(&self, email: &str) -> Result<SignInRecord, AuthError> {
        self.with_timeout(async {
            let row = db(sqlx::query("SELECT (SELECT p.hash FROM gw_passwords p JOIN gw_access a ON a.email=p.email WHERE p.email=$1) AS hash,(SELECT ceil(extract(epoch FROM locked_until-clock_timestamp()))::bigint FROM gw_sign_in_failures WHERE email=$1 AND locked_until>clock_timestamp()) AS locked")
                .bind(email)
                .fetch_one(&self.pool)
                .await)?;
            Ok(SignInRecord {
                hash: row.get("hash"),
                locked_seconds: row.get("locked"),
            })
        })
        .await
    }

    /// 실패를 하나 센다. 다섯 번째 실패면 15분 동안 잠그고 수를 0으로 되돌린다.
    pub async fn record_sign_in_failure(&self, email: &str) -> Result<(), AuthError> {
        self.with_timeout(async {
            db(sqlx::query("INSERT INTO gw_sign_in_failures(email,failures) VALUES($1,1) ON CONFLICT(email) DO UPDATE SET failures=CASE WHEN gw_sign_in_failures.failures+1>=$2 THEN 0 ELSE gw_sign_in_failures.failures+1 END,locked_until=CASE WHEN gw_sign_in_failures.failures+1>=$2 THEN clock_timestamp()+make_interval(secs=>$3) ELSE gw_sign_in_failures.locked_until END")
                .bind(email)
                .bind(MAX_FAILURES)
                .bind(LOCK_SECONDS as f64)
                .execute(&self.pool)
                .await)?;
            Ok(())
        })
        .await
    }

    /// 로그인에 성공하면 실패 기록을 지우고 세션을 만든다. 끝난 세션도 함께 지운다.
    pub async fn start_session(
        &self,
        email: &str,
        token_hash: &[u8; 32],
        seconds: i64,
    ) -> Result<(), AuthError> {
        self.with_timeout(async {
            let mut tx = db(self.begin_transaction().await)?;
            db(sqlx::query("DELETE FROM gw_sign_in_failures WHERE email=$1")
                .bind(email)
                .execute(&mut *tx)
                .await)?;
            db(sqlx::query("DELETE FROM gw_sessions WHERE expires_at<=clock_timestamp()")
                .execute(&mut *tx)
                .await)?;
            db(sqlx::query("INSERT INTO gw_sessions(token_hash,email,expires_at) VALUES($1,$2,clock_timestamp()+make_interval(secs=>$3))")
                .bind(token_hash.as_slice())
                .bind(email)
                .bind(seconds as f64)
                .execute(&mut *tx)
                .await)?;
            db(tx.commit().await)
        })
        .await
    }

    /// 로그아웃. 그 세션 하나를 지운다.
    pub async fn end_session(&self, token_hash: &[u8; 32]) -> Result<(), AuthError> {
        self.with_timeout(async {
            db(sqlx::query("DELETE FROM gw_sessions WHERE token_hash=$1")
                .bind(token_hash.as_slice())
                .execute(&self.pool)
                .await)?;
            Ok(())
        })
        .await
    }

    /// 링크를 만든다. 초대·재설정 링크를 만들면 그 사람의 예전 초대·재설정 링크는 지운다.
    /// 첫 관리자 설정 링크는 예전 설정 링크를 남긴다(결정 0011 규칙 8).
    pub async fn create_link(
        &self,
        email: &str,
        kind: LinkKind,
        token_hash: &[u8; 32],
        created_by: Option<uuid::Uuid>,
    ) -> Result<(), AuthError> {
        self.with_timeout(async {
            let mut tx = db(self.begin_transaction().await)?;
            db(sqlx::query("DELETE FROM gw_auth_links WHERE expires_at<=clock_timestamp()")
                .execute(&mut *tx)
                .await)?;
            if kind != LinkKind::Setup {
                db(sqlx::query("DELETE FROM gw_auth_links WHERE email=$1 AND kind<>'setup'")
                    .bind(email)
                    .execute(&mut *tx)
                    .await)?;
            }
            db(sqlx::query("INSERT INTO gw_auth_links(token_hash,email,kind,created_by,expires_at) VALUES($1,$2,$3,$4,clock_timestamp()+make_interval(secs=>$5))")
                .bind(token_hash.as_slice())
                .bind(email)
                .bind(kind.name())
                .bind(created_by)
                .bind(kind.seconds() as f64)
                .execute(&mut *tx)
                .await)?;
            db(tx.commit().await)
        })
        .await
    }

    /// 메일을 보내지 못한 링크를 지운다.
    pub async fn delete_link(&self, token_hash: &[u8; 32]) -> Result<(), AuthError> {
        self.with_timeout(async {
            db(sqlx::query("DELETE FROM gw_auth_links WHERE token_hash=$1")
                .bind(token_hash.as_slice())
                .execute(&self.pool)
                .await)?;
            Ok(())
        })
        .await
    }

    /// 유효한 링크의 정보. 끝난 링크, 접근 목록에서 빠진 사람의 링크는 없다.
    pub async fn link(&self, token_hash: &[u8; 32]) -> Result<Option<LinkInfo>, AuthError> {
        self.with_timeout(async {
            let row = db(sqlx::query("SELECT l.email,l.kind,extract(epoch FROM l.expires_at)::bigint AS expires,EXISTS(SELECT 1 FROM gw_passwords p WHERE p.email=l.email) AS password_set FROM gw_auth_links l JOIN gw_access a ON a.email=l.email WHERE l.token_hash=$1 AND l.expires_at>clock_timestamp()")
                .bind(token_hash.as_slice())
                .fetch_optional(&self.pool)
                .await)?;
            Ok(row.and_then(|r| {
                Some(LinkInfo {
                    email: r.get("email"),
                    kind: LinkKind::parse(&r.get::<String, _>("kind"))?,
                    password_set: r.get("password_set"),
                    expires_at: r.get("expires"),
                })
            }))
        })
        .await
    }

    /// 링크로 비밀번호를 정한다. 그 사람의 링크와 세션을 모두 지운다. 처음 정하는 사람은
    /// 표시 이름이 있어야 한다. 성공하면 이메일을 돌려준다.
    pub async fn set_password_with_link(
        &self,
        token_hash: &[u8; 32],
        hash: &str,
        display_name: Option<&str>,
    ) -> Result<Option<String>, AuthError> {
        self.with_timeout(async {
            let mut tx = db(self.begin_transaction().await)?;
            let Some(row) = db(sqlx::query("SELECT l.email FROM gw_auth_links l JOIN gw_access a ON a.email=l.email WHERE l.token_hash=$1 AND l.expires_at>clock_timestamp() FOR UPDATE OF l")
                .bind(token_hash.as_slice())
                .fetch_optional(&mut *tx)
                .await)?
            else {
                return Ok(None);
            };
            let email: String = row.get("email");
            let updated = db(sqlx::query("UPDATE gw_passwords SET hash=$2,display_name=COALESCE($3,display_name),set_at=clock_timestamp() WHERE email=$1")
                .bind(&email)
                .bind(hash)
                .bind(display_name)
                .execute(&mut *tx)
                .await)?
            .rows_affected();
            if updated == 0 {
                let Some(name) = display_name else {
                    return Err(AuthError::InvalidCredentials);
                };
                db(sqlx::query("INSERT INTO gw_passwords(email,hash,display_name) VALUES($1,$2,$3)")
                    .bind(&email)
                    .bind(hash)
                    .bind(name)
                    .execute(&mut *tx)
                    .await)?;
            }
            for sql in [
                "DELETE FROM gw_auth_links WHERE email=$1",
                "DELETE FROM gw_sessions WHERE email=$1",
                "DELETE FROM gw_sign_in_failures WHERE email=$1",
            ] {
                db(sqlx::query(sql).bind(&email).execute(&mut *tx).await)?;
            }
            db(tx.commit().await)?;
            Ok(Some(email))
        })
        .await
    }

    /// 로그인한 사람이 자기 비밀번호를 바꾼다. 그 사람의 세션은 모두 끝난다.
    pub async fn change_password(&self, email: &str, hash: &str) -> Result<(), AuthError> {
        self.with_timeout(async {
            let mut tx = db(self.begin_transaction().await)?;
            db(sqlx::query(
                "UPDATE gw_passwords SET hash=$2,set_at=clock_timestamp() WHERE email=$1",
            )
            .bind(email)
            .bind(hash)
            .execute(&mut *tx)
            .await)?;
            db(sqlx::query("DELETE FROM gw_sessions WHERE email=$1")
                .bind(email)
                .execute(&mut *tx)
                .await)?;
            db(tx.commit().await)
        })
        .await
    }

    /// 비밀번호를 정한 사람의 표시 이름.
    pub async fn session_name(&self, email: &str) -> Result<Option<String>, AuthError> {
        self.with_timeout(async {
            db(
                sqlx::query_scalar("SELECT display_name FROM gw_passwords WHERE email=$1")
                    .bind(email)
                    .fetch_optional(&self.pool)
                    .await,
            )
        })
        .await
    }

    /// 이 이메일의 비밀번호 해시. 자기 비밀번호를 바꿀 때 지금 비밀번호를 확인한다.
    pub async fn password_hash(&self, email: &str) -> Result<Option<String>, AuthError> {
        self.with_timeout(async {
            db(
                sqlx::query_scalar("SELECT hash FROM gw_passwords WHERE email=$1")
                    .bind(email)
                    .fetch_optional(&self.pool)
                    .await,
            )
        })
        .await
    }
}
