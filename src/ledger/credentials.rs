//! Provider 회사 키 등록·해제 명령. 다른 관리 명령과 같은 명령 기록·감사·재전송 규칙을 따른다.
//!
//! 키 원문은 명령 기록·감사·응답 어디에도 남지 않는다. 재전송 비교용 입력 digest도 원문 대신
//! root key로 만든 fingerprint로 계산한다. 등록 전 확인(실제 Provider 호출)은 transaction 밖에서 한다.
use super::{
    commands::CommandContext,
    management::{self, CommandResult, ManagementError},
    management_query::stamp,
};
use crate::{
    config::ProviderId,
    credentials::{self, CredentialService},
    providers::transport::VerifyError,
};
use serde_json::{Value, json};
use sqlx::{PgConnection, Row};
use uuid::Uuid;
use zeroize::Zeroizing;

pub enum CredentialCommand {
    Set {
        provider: ProviderId,
        secret: Zeroizing<String>,
        /// 지금 active인 키 id. 없으면 None. 다른 관리자의 교체를 덮어쓰지 않게 한다.
        expected: Option<Uuid>,
    },
    Retire {
        provider: ProviderId,
        expected: Uuid,
    },
}
impl CredentialCommand {
    pub fn operation(&self) -> &'static str {
        match self {
            Self::Set { .. } => "set_provider_credential",
            Self::Retire { .. } => "retire_provider_credential",
        }
    }
    pub fn provider(&self) -> ProviderId {
        match self {
            Self::Set { provider, .. } | Self::Retire { provider, .. } => *provider,
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 화면·감사에 쓰는 메타데이터. 원문·암호문·fingerprint는 없다.
pub async fn metadata(
    c: &mut PgConnection,
    id: Uuid,
    kek_id: &[u8],
) -> Result<Value, ManagementError> {
    let row = sqlx::query(&format!(
        "SELECT c.*,{} AS verified,{} AS created,{} AS retired,p.email AS created_email,r.email AS retired_email FROM gw_provider_credentials c JOIN gw_principals p ON p.id=c.created_by LEFT JOIN gw_principals r ON r.id=c.retired_by WHERE c.id=$1",
        stamp("c.verified_at"),
        stamp("c.created_at"),
        stamp("c.retired_at")
    ))
    .bind(id)
    .fetch_optional(c)
    .await?
    .ok_or(ManagementError::NotFound)?;
    Ok(row_metadata(&row, kek_id))
}
fn row_metadata(row: &sqlx::postgres::PgRow, kek_id: &[u8]) -> Value {
    let state: String = row.get("state");
    let decryptable = state == "active" && row.get::<Vec<u8>, _>("kek_id") == kek_id;
    json!({
        "id": row.get::<Uuid, _>("id"),
        "provider": row.get::<String, _>("provider"),
        "state": state,
        "decryptable": decryptable,
        "hint": row.get::<String, _>("hint"),
        "version": row.get::<i64, _>("version").to_string(),
        "verified_at": row.get::<String, _>("verified"),
        "created_at": row.get::<String, _>("created"),
        "created_by": row.get::<Uuid, _>("created_by"),
        "created_by_email": row.get::<Option<String>, _>("created_email"),
        "retired_at": row.get::<Option<String>, _>("retired"),
        "retired_by_email": row.get::<Option<String>, _>("retired_email"),
    })
}

impl CredentialService {
    /// 설정된 Provider마다 현재 상태와 최근 이력(최대 10건)을 돌려준다.
    pub async fn list(&self) -> Result<Value, ManagementError> {
        let mut c = self.store().pool().acquire().await?;
        let kek = self.cipher().kek_id();
        let mut providers = Vec::new();
        for provider in self.configured() {
            let rows = sqlx::query(&format!(
                "SELECT c.*,{} AS verified,{} AS created,{} AS retired,p.email AS created_email,r.email AS retired_email FROM gw_provider_credentials c JOIN gw_principals p ON p.id=c.created_by LEFT JOIN gw_principals r ON r.id=c.retired_by WHERE c.provider=$1 ORDER BY c.created_at DESC,c.id LIMIT 10",
                stamp("c.verified_at"),
                stamp("c.created_at"),
                stamp("c.retired_at")
            ))
            .bind(provider.name())
            .fetch_all(&mut *c)
            .await?;
            let active = rows
                .iter()
                .find(|r| r.get::<String, _>("state") == "active")
                .map(|r| row_metadata(r, &kek));
            let status = match &active {
                None => "missing",
                Some(a) if a["decryptable"] == json!(true) => "active",
                Some(_) => "undecryptable",
            };
            providers.push(json!({
                "provider": provider.name(),
                "status": status,
                "active": active,
                "history": rows.iter().map(|r| row_metadata(r, &kek)).collect::<Vec<_>>(),
            }));
        }
        Ok(json!({ "providers": providers }))
    }

    pub async fn execute(
        &self,
        ctx: &CommandContext<'_>,
        command: CredentialCommand,
    ) -> Result<CommandResult, ManagementError> {
        let provider = command.provider();
        if self.verifier(provider).is_none() {
            return Err(ManagementError::NotFound);
        }
        let input = match &command {
            CredentialCommand::Set {
                secret, expected, ..
            } => {
                if !credentials::valid_secret(provider, secret) {
                    return Err(ManagementError::Invalid);
                }
                json!({"provider":provider.name(),"fingerprint":hex(&self.cipher().fingerprint(provider,secret)),"expected_credential_id":expected})
            }
            CredentialCommand::Retire { expected, .. } => {
                json!({"provider":provider.name(),"expected_credential_id":expected})
            }
        };
        let digest = management::input_digest(ctx.method, ctx.path, &input);
        // 이미 처리한 명령은 Provider를 다시 호출하지 않고 기록된 결과를 돌려준다.
        if let Some(result) = self.replay(ctx, &digest).await? {
            return Ok(result);
        }
        if let CredentialCommand::Set { secret, .. } = &command {
            let verifier = self.verifier(provider).ok_or(ManagementError::NotFound)?;
            verifier
                .verify_credential(secret)
                .await
                .map_err(|e| match e {
                    VerifyError::Rejected => ManagementError::CredentialRejected,
                    VerifyError::Unavailable => ManagementError::CredentialUnverified,
                })?;
        }
        let _gate = self
            .store()
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| ManagementError::Database)?;
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            self.transaction(ctx, &command, &digest),
        )
        .await
        .map_err(|_| ManagementError::Busy)?
    }

    async fn replay(
        &self,
        ctx: &CommandContext<'_>,
        digest: &[u8; 32],
    ) -> Result<Option<CommandResult>, ManagementError> {
        let mut c = self.store().pool().acquire().await?;
        replay_on(&mut c, ctx, digest).await
    }

    async fn transaction(
        &self,
        ctx: &CommandContext<'_>,
        command: &CredentialCommand,
        digest: &[u8; 32],
    ) -> Result<CommandResult, ManagementError> {
        let provider = command.provider();
        let kek = self.cipher().kek_id();
        let mut tx = self.store().begin_transaction().await?;
        management::check_actor(&mut tx, ctx.actor, ctx.settings, true).await?;
        if let Some(result) = replay_on(&mut tx, ctx, digest).await? {
            return Ok(result);
        }
        let current = sqlx::query(
            "SELECT id,fingerprint,kek_id,version FROM gw_provider_credentials WHERE provider=$1 AND state='active' FOR UPDATE",
        )
        .bind(provider.name())
        .fetch_optional(&mut *tx)
        .await?;
        let current_id = current.as_ref().map(|r| r.get::<Uuid, _>("id"));
        let expected = match command {
            CredentialCommand::Set { expected, .. } => *expected,
            CredentialCommand::Retire { expected, .. } => Some(*expected),
        };
        if current_id != expected {
            return Err(ManagementError::CredentialConflict(current_id));
        }
        let before = match current_id {
            Some(id) => metadata(&mut tx, id, &kek).await?,
            None => json!({"provider":provider.name(),"state":"missing"}),
        };
        let (status, target, changed) = match command {
            CredentialCommand::Set { secret, .. } => {
                let fingerprint = self.cipher().fingerprint(provider, secret);
                let same = current.as_ref().is_some_and(|r| {
                    r.get::<Vec<u8>, _>("fingerprint") == fingerprint
                        && r.get::<Vec<u8>, _>("kek_id") == kek
                });
                if same {
                    (200, current_id.expect("같은 키"), false)
                } else {
                    let id = Uuid::new_v4();
                    let (nonce, sealed) = self
                        .cipher()
                        .seal(provider, id, secret)
                        .ok_or(ManagementError::Invariant)?;
                    if let Some(old) = current_id {
                        retire(&mut tx, old, ctx.actor).await?;
                    }
                    sqlx::query("INSERT INTO gw_provider_credentials(id,provider,state,kek_id,nonce,ciphertext,fingerprint,hint,verified_at,created_by) VALUES($1,$2,'active',$3,$4,$5,$6,$7,clock_timestamp(),$8)")
                        .bind(id).bind(provider.name()).bind(kek.as_slice()).bind(nonce.as_slice()).bind(&sealed).bind(fingerprint.as_slice()).bind(credentials::hint(secret)).bind(ctx.actor)
                        .execute(&mut *tx).await?;
                    (201, id, true)
                }
            }
            CredentialCommand::Retire { expected, .. } => {
                retire(&mut tx, *expected, ctx.actor).await?;
                (200, *expected, true)
            }
        };
        let after = metadata(&mut tx, target, &kek).await?;
        let version: i64 =
            sqlx::query_scalar("SELECT version FROM gw_provider_credentials WHERE id=$1")
                .bind(target)
                .fetch_one(&mut *tx)
                .await?;
        let ack = json!({"command_id":ctx.command_id,"operation":command.operation(),"target":{"kind":"credential","id":target},"result_version":version.to_string(),"effect":if changed{"changed"}else{"noop"},"credential":after});
        sqlx::query("INSERT INTO gw_commands(actor_principal_id,command_id,input_digest,operation,target_kind,target_id,result_status,result_version,result_projection,created_at) VALUES($1,$2,$3,$4,'credential',$5,$6,$7,$8,clock_timestamp())")
            .bind(ctx.actor).bind(ctx.command_id).bind(digest.as_slice()).bind(command.operation()).bind(target).bind(status as i16).bind(version).bind(&ack)
            .execute(&mut *tx).await?;
        if changed {
            sqlx::query("INSERT INTO gw_audit_events(actor_kind,actor_principal_id,command_id,actor_auth_config_digest,operation,target_kind,target_id,before_snapshot,after_snapshot) VALUES('human',$1,$2,$3,$4,'credential',$5,$6,$7)")
                .bind(ctx.actor).bind(ctx.command_id).bind(ctx.digest.as_slice()).bind(command.operation()).bind(target).bind(before).bind(&after)
                .execute(&mut *tx).await?;
        }
        tx.commit().await?;
        let mut body = ack;
        body["replayed"] = json!(false);
        Ok(CommandResult {
            status,
            body: Some(body),
            replayed: false,
        })
    }
}

async fn retire(c: &mut PgConnection, id: Uuid, actor: Uuid) -> Result<(), ManagementError> {
    // 해제와 동시에 암호문을 지운다. 메타데이터만 이력으로 남는다.
    let n = sqlx::query("UPDATE gw_provider_credentials SET state='retired',nonce=NULL,ciphertext=NULL,retired_by=$2,retired_at=clock_timestamp(),version=version+1 WHERE id=$1 AND state='active'")
        .bind(id)
        .bind(actor)
        .execute(c)
        .await?
        .rows_affected();
    if n != 1 {
        return Err(ManagementError::Invariant);
    }
    Ok(())
}

async fn replay_on(
    c: &mut PgConnection,
    ctx: &CommandContext<'_>,
    digest: &[u8; 32],
) -> Result<Option<CommandResult>, ManagementError> {
    let Some(prior) = sqlx::query(
        "SELECT input_digest,operation,result_status,result_projection FROM gw_commands WHERE actor_principal_id=$1 AND command_id=$2",
    )
    .bind(ctx.actor)
    .bind(ctx.command_id)
    .fetch_optional(c)
    .await?
    else {
        return Ok(None);
    };
    if prior.get::<Vec<u8>, _>("input_digest") != digest {
        return Err(ManagementError::CommandConflict);
    }
    let status = prior.get::<i16, _>("result_status") as u16;
    let mut ack: Value = prior.get("result_projection");
    ack["replayed"] = json!(true);
    Ok(Some(CommandResult {
        status,
        body: Some(ack),
        replayed: true,
    }))
}
