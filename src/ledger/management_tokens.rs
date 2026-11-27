//! 관리 토큰(결정 0014). 관리자가 스크립트용으로 발급하는 기한 있는 자격 증명이다.
//!
//! 토큰은 발급한 관리자(소유자)로 동작한다. DB에는 토큰의 SHA-256만 둔다. 토큰 발급과 폐기는
//! 다른 관리 명령처럼 명령 기록과 감사 기록을 남긴다.
use super::{
    CurrentHuman, PgIdentityStore,
    commands::CommandContext,
    management::{self, CommandResult, ManagementError},
    management_query::stamp,
};
use crate::auth::{AuthError, TokenScope};
use serde_json::{Value, json};
use sqlx::{PgConnection, Row, postgres::PgRow};
use std::time::Duration;
use uuid::Uuid;

/// 관리자 한 명이 동시에 가질 수 있는 활성 토큰 수. 폐기되지 않고 기한이 남은 토큰을 센다.
pub const ACTIVE_LIMIT: i64 = 30;

/// 토큰의 유효 기간. 기한 없는 토큰은 없다.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Validity {
    Days30,
    Days90,
    Year1,
}
impl Validity {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "30_days" => Some(Self::Days30),
            "90_days" => Some(Self::Days90),
            "1_year" => Some(Self::Year1),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Days30 => "30_days",
            Self::Days90 => "90_days",
            Self::Year1 => "1_year",
        }
    }
    fn interval(self) -> &'static str {
        match self {
            Self::Days30 => "30 days",
            Self::Days90 => "90 days",
            Self::Year1 => "1 year",
        }
    }
}

/// 토큰 이름 규칙: 1~64자, 앞뒤 공백과 제어 문자가 없다.
pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.chars().count())
        && name.trim() == name
        && !name.chars().any(char::is_control)
}

/// 유효한 토큰과 그 소유자. 인증이 이 값으로 소유자의 신원을 만든다.
pub struct TokenOwner {
    pub id: Uuid,
    pub scope: TokenScope,
    /// 토큰의 만료 시각(Unix 초).
    pub expires_at: i64,
    pub issuer: String,
    pub subject: String,
    pub email: String,
    pub display_name: String,
    pub current: CurrentHuman,
}

fn select_sql(filter: &str) -> String {
    format!(
        "SELECT t.id,t.name,t.scope,t.validity,{} AS created_at,{} AS expires_at,{} AS revoked_at,{} AS last_used_at,t.revoked_at IS NULL AND t.expires_at>clock_timestamp() AS active,o.id AS owner_id,o.email AS owner_email,o.display_name AS owner_name,r.email AS revoked_by_email FROM gw_management_tokens t JOIN gw_principals o ON o.id=t.owner_id LEFT JOIN gw_principals r ON r.id=t.revoked_by {filter}",
        stamp("t.created_at"),
        stamp("t.expires_at"),
        stamp("t.revoked_at"),
        stamp("t.last_used_at"),
    )
}

fn view(row: &PgRow) -> Value {
    json!({
        "id": row.get::<Uuid, _>("id"),
        "name": row.get::<String, _>("name"),
        "scope": row.get::<String, _>("scope"),
        "validity": row.get::<String, _>("validity"),
        "owner": {
            "id": row.get::<Uuid, _>("owner_id"),
            "email": row.get::<Option<String>, _>("owner_email"),
            "display_name": row.get::<String, _>("owner_name"),
        },
        "created_at": row.get::<String, _>("created_at"),
        "expires_at": row.get::<String, _>("expires_at"),
        "revoked_at": row.get::<Option<String>, _>("revoked_at"),
        "revoked_by_email": row.get::<Option<String>, _>("revoked_by_email"),
        "last_used_at": row.get::<Option<String>, _>("last_used_at"),
        "active": row.get::<bool, _>("active"),
    })
}

async fn entry(c: &mut PgConnection, id: Uuid) -> Result<Option<Value>, sqlx::Error> {
    Ok(sqlx::query(&select_sql("WHERE t.id=$1"))
        .bind(id)
        .fetch_optional(c)
        .await?
        .map(|row| view(&row)))
}

/// 명령 기록과 감사 기록을 남긴다. 감사 기록은 바뀐 것이 있을 때만 남긴다.
#[allow(clippy::too_many_arguments)]
async fn record(
    c: &mut PgConnection,
    ctx: &CommandContext<'_>,
    digest: &[u8; 32],
    operation: &str,
    id: Uuid,
    status: u16,
    before: &Value,
    after: &Value,
    changed: bool,
) -> Result<CommandResult, ManagementError> {
    let ack = json!({"command_id":ctx.command_id,"operation":operation,"target":{"kind":"management_token","id":id},"result_version":null,"effect":if changed{"changed"}else{"noop"},"token":after});
    sqlx::query("INSERT INTO gw_commands(actor_principal_id,command_id,input_digest,operation,target_kind,target_id,result_status,result_projection,created_at) VALUES($1,$2,$3,$4,'management_token',$5,$6,$7,clock_timestamp())")
        .bind(ctx.actor)
        .bind(ctx.command_id)
        .bind(digest.as_slice())
        .bind(operation)
        .bind(id)
        .bind(status as i16)
        .bind(&ack)
        .execute(&mut *c)
        .await?;
    if changed {
        sqlx::query("INSERT INTO gw_audit_events(actor_kind,actor_principal_id,actor_token_id,command_id,actor_auth_config_digest,operation,target_kind,target_id,before_snapshot,after_snapshot) VALUES('human',$1,$2,$3,$4,$5,'management_token',$6,$7,$8)")
            .bind(ctx.actor)
            .bind(ctx.token)
            .bind(ctx.command_id)
            .bind(ctx.digest.as_slice())
            .bind(operation)
            .bind(id)
            .bind(before)
            .bind(after)
            .execute(&mut *c)
            .await?;
    }
    let mut body = ack;
    body["replayed"] = json!(false);
    Ok(CommandResult {
        status,
        body: Some(body),
        replayed: false,
    })
}

impl PgIdentityStore {
    /// 토큰의 SHA-256으로 유효한 토큰과 그 소유자를 찾는다(결정 0014 규칙 1).
    /// 없거나, 폐기됐거나, 기한이 지난 토큰은 `InvalidCredentials`다. 소유자가 접근 목록에 없거나,
    /// 관리자가 아니거나, 정지됐으면 `AccessDenied`다. 마지막 사용 시각은 1분에 한 번만 고친다.
    pub async fn management_token_owner(&self, hash: &[u8; 32]) -> Result<TokenOwner, AuthError> {
        let _gate = self
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| AuthError::DatabaseUnavailable)?;
        tokio::time::timeout(Duration::from_secs(3), self.token_owner(hash))
            .await
            .map_err(|_| AuthError::DatabaseUnavailable)?
    }
    async fn token_owner(&self, hash: &[u8; 32]) -> Result<TokenOwner, AuthError> {
        let db = |_| AuthError::DatabaseUnavailable;
        let mut tx = self.begin_transaction().await.map_err(db)?;
        let row = sqlx::query("SELECT t.id AS token_id,t.scope,extract(epoch FROM t.expires_at)::bigint AS token_expires,t.revoked_at IS NULL AND t.expires_at>clock_timestamp() AS token_active,(t.last_used_at IS NULL OR t.last_used_at<=clock_timestamp()-interval '1 minute') AS token_stale,p.*,q.id AS policy_id,q.version AS policy_version FROM gw_management_tokens t JOIN gw_principals p ON p.id=t.owner_id JOIN gw_policies q ON q.principal_id=p.id AND q.key_id IS NULL WHERE t.token_hash=$1")
            .bind(hash.as_slice())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?
            .ok_or(AuthError::InvalidCredentials)?;
        if !row.get::<bool, _>("token_active") {
            return Err(AuthError::InvalidCredentials);
        }
        let email: Option<String> = row.get("email");
        let admin = match &email {
            Some(email) => {
                management::active_human(&mut tx, &row).await.map_err(db)?
                    && management::access_role(&mut tx, email)
                        .await
                        .map_err(db)?
                        .as_deref()
                        == Some("admin")
            }
            None => false,
        };
        let (Some(email), true) = (email, admin) else {
            return Err(AuthError::AccessDenied);
        };
        let id: Uuid = row.get("token_id");
        if row.get::<bool, _>("token_stale") {
            sqlx::query("UPDATE gw_management_tokens SET last_used_at=clock_timestamp() WHERE id=$1 AND (last_used_at IS NULL OR last_used_at<=clock_timestamp()-interval '1 minute')")
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(db)?;
        }
        tx.commit().await.map_err(db)?;
        Ok(TokenOwner {
            id,
            scope: TokenScope::parse(&row.get::<String, _>("scope"))
                .ok_or(AuthError::DatabaseUnavailable)?,
            expires_at: row.get("token_expires"),
            issuer: row.get("issuer"),
            subject: row.get("subject"),
            email,
            display_name: row.get("display_name"),
            current: CurrentHuman {
                id: row.get("id"),
                version: row.get("version"),
                policy_id: row.get("policy_id"),
                policy_version: row.get("policy_version"),
                admin: true,
            },
        })
    }

    /// `GET /api/admin/management-tokens`. 모든 관리자의 토큰을 새것부터 돌려준다.
    pub async fn management_tokens(
        &self,
        ctx: &CommandContext<'_>,
    ) -> Result<Value, ManagementError> {
        let _gate = self.token_gate()?;
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut tx = self.begin_transaction().await?;
            management::check_actor(&mut tx, ctx.actor, ctx.settings, true).await?;
            let rows = sqlx::query(&select_sql("ORDER BY t.created_at DESC,t.id LIMIT 10000"))
                .fetch_all(&mut *tx)
                .await?;
            tx.commit().await?;
            Ok(json!({"items": rows.iter().map(view).collect::<Vec<_>>()}))
        })
        .await
        .map_err(|_| ManagementError::Busy)?
    }

    /// `POST /api/admin/management-tokens`. 토큰을 만든다. 비밀값은 처리 함수가 만들고, 여기에는
    /// 그 SHA-256만 온다. 같은 명령을 다시 보내면 비밀값을 다시 줄 수 없으므로
    /// `management_token_secret_unavailable`이다.
    pub async fn create_management_token(
        &self,
        ctx: &CommandContext<'_>,
        name: &str,
        scope: TokenScope,
        validity: Validity,
        hash: &[u8; 32],
    ) -> Result<CommandResult, ManagementError> {
        if !valid_name(name) {
            return Err(ManagementError::Invalid);
        }
        let digest = management::input_digest(
            ctx.method,
            ctx.path,
            &json!({"name":name,"scope":scope.name(),"validity":validity.name()}),
        );
        let _gate = self.token_gate()?;
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut tx = self.begin_transaction().await?;
            management::check_actor(&mut tx, ctx.actor, ctx.settings, true).await?;
            if let Some(prior) = super::credentials::replay_on(&mut tx, ctx, &digest).await? {
                let id = prior
                    .body
                    .as_ref()
                    .and_then(|b| b["target"]["id"].as_str())
                    .and_then(|s| Uuid::parse_str(s).ok())
                    .ok_or(ManagementError::Invariant)?;
                return Err(ManagementError::TokenSecretUnavailable(id));
            }
            // 소유자 행을 잠가 같은 관리자의 발급이 동시에 상한을 넘지 않게 한다.
            sqlx::query("SELECT id FROM gw_principals WHERE id=$1 FOR UPDATE")
                .bind(ctx.actor)
                .fetch_one(&mut *tx)
                .await?;
            let active: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_management_tokens WHERE owner_id=$1 AND revoked_at IS NULL AND expires_at>clock_timestamp()")
                .bind(ctx.actor)
                .fetch_one(&mut *tx)
                .await?;
            if active >= ACTIVE_LIMIT {
                return Err(ManagementError::TokenQuota);
            }
            let id = Uuid::new_v4();
            sqlx::query("INSERT INTO gw_management_tokens(id,token_hash,owner_id,name,scope,validity,expires_at) VALUES($1,$2,$3,$4,$5,$6,clock_timestamp()+$7::interval)")
                .bind(id)
                .bind(hash.as_slice())
                .bind(ctx.actor)
                .bind(name)
                .bind(scope.name())
                .bind(validity.name())
                .bind(validity.interval())
                .execute(&mut *tx)
                .await?;
            let after = entry(&mut tx, id).await?.ok_or(ManagementError::Invariant)?;
            let result = record(
                &mut tx,
                ctx,
                &digest,
                "create_management_token",
                id,
                201,
                &json!({}),
                &after,
                true,
            )
            .await?;
            tx.commit().await?;
            Ok(result)
        })
        .await
        .map_err(|_| ManagementError::Busy)?
    }

    /// `DELETE /api/admin/management-tokens/{id}`. 토큰을 폐기한다. 어느 관리자든 할 수 있다.
    /// 이미 폐기된 토큰이면 바꾸지 않고 `effect`가 `noop`이다.
    pub async fn revoke_management_token(
        &self,
        ctx: &CommandContext<'_>,
        id: Uuid,
    ) -> Result<CommandResult, ManagementError> {
        let digest = management::input_digest(ctx.method, ctx.path, &json!({}));
        let _gate = self.token_gate()?;
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut tx = self.begin_transaction().await?;
            management::check_actor(&mut tx, ctx.actor, ctx.settings, true).await?;
            if let Some(result) = super::credentials::replay_on(&mut tx, ctx, &digest).await? {
                return Ok(result);
            }
            let revoked: bool = sqlx::query_scalar(
                "SELECT revoked_at IS NOT NULL FROM gw_management_tokens WHERE id=$1 FOR UPDATE",
            )
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(ManagementError::NotFound)?;
            let before = entry(&mut tx, id).await?.ok_or(ManagementError::Invariant)?;
            if !revoked {
                sqlx::query("UPDATE gw_management_tokens SET revoked_at=clock_timestamp(),revoked_by=$2 WHERE id=$1")
                    .bind(id)
                    .bind(ctx.actor)
                    .execute(&mut *tx)
                    .await?;
            }
            let after = entry(&mut tx, id).await?.ok_or(ManagementError::Invariant)?;
            let result = record(
                &mut tx,
                ctx,
                &digest,
                "revoke_management_token",
                id,
                200,
                &before,
                &after,
                !revoked,
            )
            .await?;
            tx.commit().await?;
            Ok(result)
        })
        .await
        .map_err(|_| ManagementError::Busy)?
    }

    fn token_gate(&self) -> Result<tokio::sync::OwnedSemaphorePermit, ManagementError> {
        self.gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| ManagementError::Database)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn name_rules_match_db_constraint() {
        for ok in ["ci", "배포 스크립트", &"가".repeat(64)] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", " lead", "trail ", "tab\there", &"가".repeat(65)] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }
    #[test]
    fn validity_names_round_trip() {
        for v in [Validity::Days30, Validity::Days90, Validity::Year1] {
            assert!(Validity::parse(v.name()) == Some(v));
        }
        assert!(Validity::parse("unlimited").is_none());
    }
}
