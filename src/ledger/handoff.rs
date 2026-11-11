mod import;
pub mod types;
mod validate;
use super::{
    PgLedger,
    commands::CommandContext,
    management::{self, Command, Grant, ManagementError, ReplacePolicy},
    policy::PolicyLimits,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction, postgres::PgRow};
use uuid::Uuid;
pub use validate::SourceValidation;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Handoff {
    Fence {
        target_id: Uuid,
        expected_version: String,
        source_id: Uuid,
        grants: Vec<Grant>,
        limits: PolicyLimits,
    },
    Import(Box<types::Import>),
    Activate {
        target_id: Uuid,
        expected_version: String,
        source_id: Uuid,
        previous_executors_stopped_ref: String,
    },
    Rollback {
        target_id: Uuid,
        expected_version: String,
        source_id: Uuid,
        new_executors_stopped_ref: String,
        legacy_represents_all_state: bool,
        legacy_all_credentials_blocked: bool,
    },
}
async fn lock_target(
    tx: &mut Transaction<'_, Postgres>,
    ctx: &CommandContext<'_>,
    target: Uuid,
) -> Result<PgRow, ManagementError> {
    let owner: Option<Uuid> =
        sqlx::query_scalar("SELECT owner_principal_id FROM gw_principals WHERE id=$1")
            .bind(target)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(ManagementError::NotFound)?;
    let mut ids = vec![ctx.actor, target];
    if let Some(owner) = owner {
        ids.push(owner);
    }
    ids.sort();
    ids.dedup();
    for id in ids {
        sqlx::query("SELECT id FROM gw_principals WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
    }
    management::check_actor(tx, ctx.actor, ctx.settings, true).await?;
    let p = sqlx::query("SELECT * FROM gw_principals WHERE id=$1")
        .bind(target)
        .fetch_one(&mut **tx)
        .await?;
    if p.get::<Option<Uuid>, _>("owner_principal_id") != owner {
        return Err(ManagementError::Busy);
    }
    Ok(p)
}
async fn replay(
    tx: &mut Transaction<'_, Postgres>,
    ctx: &CommandContext<'_>,
    digest: &[u8; 32],
    state: &str,
) -> Result<Option<Value>, ManagementError> {
    let prior=sqlx::query("SELECT input_digest,result_projection FROM gw_commands WHERE actor_principal_id=$1 AND command_id=$2").bind(ctx.actor).bind(ctx.command_id).fetch_optional(&mut **tx).await?;
    if let Some(prior) = prior {
        if prior.get::<Vec<u8>, _>("input_digest") != digest {
            return Err(ManagementError::CommandConflict);
        }
        let mut ack: Value = prior.get("result_projection");
        ack["replayed"] = json!(true);
        return Ok(Some(json!({"ack":ack,"state":state})));
    }
    Ok(None)
}
#[allow(clippy::too_many_arguments)]
async fn commit_result(
    mut tx: Transaction<'_, Postgres>,
    ctx: &CommandContext<'_>,
    digest: &[u8; 32],
    target: Uuid,
    version: i64,
    state: &str,
    changed: bool,
    summary: Option<Value>,
) -> Result<Value, ManagementError> {
    let ack = json!({"command_id":ctx.command_id,"operation":"handoff","target":{"kind":"principal","id":target},"result_version":version.to_string(),"effect":if changed{"changed"}else{"noop"}});
    sqlx::query("INSERT INTO gw_commands(actor_principal_id,command_id,input_digest,operation,target_kind,target_id,result_status,result_version,result_projection) VALUES($1,$2,$3,'handoff','principal',$4,200,$5,$6)").bind(ctx.actor).bind(ctx.command_id).bind(digest.as_slice()).bind(target).bind(version).bind(&ack).execute(&mut *tx).await?;
    if changed {
        sqlx::query("INSERT INTO gw_audit_events(actor_kind,actor_principal_id,command_id,actor_auth_config_digest,operation,target_kind,target_id,before_snapshot,after_snapshot) VALUES('human',$1,$2,$3,'handoff','principal',$4,'{}',$5)").bind(ctx.actor).bind(ctx.command_id).bind(ctx.digest.as_slice()).bind(target).bind(json!({"state":state,"summary":summary})).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    let mut ack = ack;
    ack["replayed"] = json!(false);
    Ok(json!({"ack":ack,"state":state}))
}
impl PgLedger {
    pub async fn handoff(
        &self,
        ctx: &CommandContext<'_>,
        input: Handoff,
    ) -> Result<Value, ManagementError> {
        match input {
            Handoff::Fence {
                target_id,
                expected_version,
                source_id,
                grants,
                limits,
            } => {
                let path = format!("handoff/fence/{target_id}/{source_id}");
                let fence = CommandContext {
                    path: &path,
                    ..*ctx
                };
                let result = self
                    .management_command(
                        &fence,
                        Command::Policy(
                            target_id,
                            ReplacePolicy {
                                expected_version,
                                disabled: true,
                                grants,
                                limits,
                                reason: format!("security_config_fence:{source_id}"),
                                fence_release: None,
                            },
                        ),
                    )
                    .await?;
                Ok(json!({"ack":result.body,"state":"fenced"}))
            }
            Handoff::Import(input) => self.import_financial_bundle(ctx, *input).await,
            Handoff::Activate {
                target_id,
                expected_version,
                source_id,
                previous_executors_stopped_ref,
            } => {
                management::text(&previous_executors_stopped_ref, 500, 2048)?;
                self.handoff_transition(
                    ctx,
                    target_id,
                    &expected_version,
                    source_id,
                    Some(&previous_executors_stopped_ref),
                    None,
                )
                .await
            }
            Handoff::Rollback {
                target_id,
                expected_version,
                source_id,
                new_executors_stopped_ref,
                legacy_represents_all_state,
                legacy_all_credentials_blocked,
            } => {
                management::text(&new_executors_stopped_ref, 500, 2048)?;
                if !legacy_represents_all_state && !legacy_all_credentials_blocked {
                    return Err(ManagementError::SecurityFence);
                }
                self.handoff_transition(
                    ctx,
                    target_id,
                    &expected_version,
                    source_id,
                    None,
                    Some((
                        &new_executors_stopped_ref,
                        legacy_represents_all_state,
                        legacy_all_credentials_blocked,
                    )),
                )
                .await
            }
        }
    }
    #[allow(clippy::too_many_arguments)]
    async fn handoff_transition(
        &self,
        ctx: &CommandContext<'_>,
        target: Uuid,
        expected: &str,
        source: Uuid,
        activate: Option<&str>,
        rollback: Option<(&str, bool, bool)>,
    ) -> Result<Value, ManagementError> {
        management::version(expected)?;
        let _gate = self
            .database()
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| ManagementError::Database)?;
        tokio::time::timeout(std::time::Duration::from_secs(3),async{
   let mut tx=self.database().begin_transaction().await?;let p=lock_target(&mut tx,ctx,target).await?;
   let state=if activate.is_some(){"active"}else{"rollback_blocked"};
   let digest=management::input_digest("MAINTENANCE","handoff/transition",&json!({"target":target,"expected_version":expected,"source_id":source,"activate":activate,"rollback":rollback}));
   if let Some(prior)=replay(&mut tx,ctx,&digest,state).await?{return Ok(prior);}
   management::cas(expected,p.get("version"))?;
   let receipt=sqlx::query("SELECT * FROM gw_handoffs WHERE principal_id=$1 FOR UPDATE").bind(target).fetch_optional(&mut *tx).await?.ok_or(ManagementError::NotFound)?;
   if receipt.get::<Uuid,_>("source_id")!=source||p.get::<Option<String>,_>("security_fence_ref").as_deref()!=Some(source.to_string().as_str()){return Err(ManagementError::SecurityFence);}
   let running:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM gw_requests WHERE principal_id=$1 AND phase<>'finished')").bind(target).fetch_one(&mut *tx).await?;
   if running||ctx.execution_running{return Err(ManagementError::ExecutionNotFinal);}
   if let Some(reference)=activate {
    if receipt.get::<bool,_>("source_disabled")||receipt.get::<String,_>("phase")!="imported"||receipt.get::<String,_>("previous_executors_stopped_ref")!=reference{return Err(ManagementError::SecurityFence);}
    if let Some(owner)=p.get::<Option<Uuid>,_>("owner_principal_id"){management::check_actor(&mut tx,owner,ctx.settings,false).await?;}
    sqlx::query("UPDATE gw_policies SET version=version+1,updated_at=clock_timestamp(),updated_by=$2 WHERE principal_id=$1 AND key_id IS NULL").bind(target).bind(ctx.actor).execute(&mut *tx).await?;
    sqlx::query("UPDATE gw_principals SET disabled_at=NULL,disabled_reason=NULL,security_fence_ref=NULL,version=version+1,updated_at=clock_timestamp() WHERE id=$1").bind(target).execute(&mut *tx).await?;
   }else{
    // 별도 fence commit/실제 sender 종료가 먼저다. rollback은 이미 열린 writer를 여기서 슬쩍 차단하지 않는다.
    if p.get::<Option<String>,_>("disabled_reason").as_deref()!=Some("security_config_fence"){return Err(ManagementError::SecurityFence);}
    sqlx::query("UPDATE gw_principals SET version=version+1,updated_at=clock_timestamp() WHERE id=$1").bind(target).execute(&mut *tx).await?;
   }
   sqlx::query("UPDATE gw_handoffs SET phase=$2,version=version+1 WHERE principal_id=$1").bind(target).bind(state).execute(&mut *tx).await?;
   let version=p.get::<i64,_>("version").checked_add(1).ok_or(ManagementError::Invariant)?;
   let summary=sqlx::query_scalar::<_,Value>("SELECT jsonb_build_object('source_id',h.source_id,'source_ref',h.source_ref,'cutoff',h.cutoff,'safety_version',p.inference_safety_version::text,'safety_reason',p.inference_pause_reason,'safety_request_id',p.inference_pause_request_id,'unknown_count',(SELECT count(*)::text FROM gw_requests r WHERE r.principal_id=p.id AND r.accounting_state='unknown'),'cost_usd',(SELECT coalesce(sum(t.cost_usd),0)::text FROM gw_usage_totals t JOIN gw_policies q ON q.id=t.scope_id WHERE q.principal_id=p.id AND q.key_id IS NULL),'reserved_usd',(SELECT coalesce(sum(t.reserved_usd),0)::text FROM gw_usage_totals t JOIN gw_policies q ON q.id=t.scope_id WHERE q.principal_id=p.id AND q.key_id IS NULL)) FROM gw_handoffs h JOIN gw_principals p ON p.id=h.principal_id WHERE h.principal_id=$1").bind(target).fetch_one(&mut *tx).await?;
   commit_result(tx,ctx,&digest,target,version,state,true,Some(json!({"ledger":summary,"operator_execution_stop_ref":activate.or(rollback.map(|r|r.0)),"legacy_represents_all_state":rollback.map(|r|r.1),"legacy_all_credentials_blocked":rollback.map(|r|r.2)}))).await
  }).await.map_err(|_|ManagementError::Busy)?
    }
}
