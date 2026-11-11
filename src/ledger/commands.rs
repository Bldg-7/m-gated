use super::{
    PgLedger,
    management::{self, Command, CommandResult, ManagementError, cas},
    management_query::{key_metadata, stamp},
    policy::PolicyLimits,
};
use crate::auth::{AccessSettings, key::VirtualSecret};
use serde_json::{Value, json};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

pub struct CommandContext<'a> {
    pub actor: Uuid,
    pub command_id: Uuid,
    pub method: &'a str,
    pub path: &'a str,
    pub settings: &'a AccessSettings,
    pub grants: &'a Value,
    pub digest: [u8; 32],
    pub ceiling: &'a PolicyLimits,
    pub execution_running: bool,
}
async fn insert_policy(
    c: &mut PgConnection,
    id: Uuid,
    principal: Uuid,
    key: Option<Uuid>,
    grants: Value,
    limits: &PolicyLimits,
    actor: Uuid,
) -> Result<(), ManagementError> {
    sqlx::query("INSERT INTO gw_policies(id,principal_id,key_id,allow_rules,budget_mode,budget_limit_usd,period_kind,period_timezone,rpm_limit,tpm_limit,count_rpm_limit,updated_by) VALUES($1,$2,$3,$4,$5,gw_amount($6::numeric),'calendar_month',$7,$8,$9,$10,$11)").bind(id).bind(principal).bind(key).bind(grants).bind(limits.budget.as_ref().map_or("actual",|b|b.mode.as_str())).bind(limits.budget.as_ref().map(|b|&b.limit_usd)).bind(limits.budget.as_ref().map_or("UTC",|b|b.period.timezone.as_str())).bind(limits.rpm).bind(limits.tpm).bind(limits.count_rpm).bind(actor).execute(c).await?;
    Ok(())
}
fn require_subset(requested: &Value, allowed: &Value) -> Result<(), ManagementError> {
    if super::dashboard::intersect_grants(requested, allowed) != *requested {
        Err(ManagementError::Forbidden)
    } else {
        Ok(())
    }
}
impl PgLedger {
    pub async fn management_command(
        &self,
        ctx: &CommandContext<'_>,
        mut command: Command,
    ) -> Result<CommandResult, ManagementError> {
        command.validate()?;
        let _gate = self
            .database()
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| ManagementError::Database)?;
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            self.command_transaction(ctx, &command),
        )
        .await
        .map_err(|_| ManagementError::Busy)?
    }
    async fn command_transaction(
        &self,
        ctx: &CommandContext<'_>,
        command: &Command,
    ) -> Result<CommandResult, ManagementError> {
        let mut tx = self.database().begin_transaction().await?;
        let admin = !matches!(command,Command::Issue(v) if v.service_principal_id.is_none())
            && !matches!(command, Command::Revoke { .. });
        let (target, kind) = match command {
            Command::Issue(v) => (v.service_principal_id.unwrap_or(ctx.actor), "key"),
            Command::Revoke { id, .. } => (*id, "key"),
            Command::Create(_) => (Uuid::new_v4(), "principal"),
            Command::Update(id, _) | Command::Policy(id, _) | Command::Release(id, _) => {
                (*id, "principal")
            }
            Command::Settle(id, _) => (*id, "request"),
        };
        let target_principal = match command {
            Command::Revoke { id, .. } => sqlx::query_scalar::<_, Uuid>(
                "SELECT principal_id FROM gw_virtual_keys WHERE id=$1",
            )
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(ManagementError::NotFound)?,
            Command::Settle(id, _) => {
                sqlx::query_scalar::<_, Uuid>("SELECT principal_id FROM gw_requests WHERE id=$1")
                    .bind(id)
                    .fetch_optional(&mut *tx)
                    .await?
                    .ok_or(ManagementError::NotFound)?
            }
            _ => target,
        };
        let owner: Option<Uuid> = if matches!(command, Command::Create(_)) {
            None
        } else {
            sqlx::query_scalar("SELECT owner_principal_id FROM gw_principals WHERE id=$1")
                .bind(target_principal)
                .fetch_optional(&mut *tx)
                .await?
                .flatten()
        };
        let mut principals = vec![ctx.actor];
        if !matches!(command, Command::Create(_)) {
            principals.push(target_principal);
        }
        if let Some(o) = owner {
            principals.push(o);
        }
        match command {
            Command::Create(v) => principals.push(v.management_owner_id),
            Command::Update(_, v) => {
                if let Some(o) = v.management_owner_id {
                    principals.push(o);
                }
            }
            _ => {}
        }
        principals.sort();
        principals.dedup();
        let admin_emails = ctx
            .settings
            .admin_emails()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        // 관련 행과 현재 설정 admin을 한 UUID 순서로 잠그되 DB identity 수만큼 ID를 축적하지 않는다.
        {
            use futures_util::TryStreamExt;
            let rows=sqlx::query("SELECT id FROM gw_principals WHERE id=ANY($1) OR ($2 AND kind='human' AND email=ANY($3)) ORDER BY id FOR UPDATE").bind(&principals).bind(matches!(command,Command::Policy(..))).bind(&admin_emails).fetch(&mut *tx);
            let mut rows = std::pin::pin!(rows);
            let mut related = 0;
            while let Some(row) = rows.try_next().await? {
                if principals.binary_search(&row.get::<Uuid, _>("id")).is_ok() {
                    related += 1;
                }
            }
            if related != principals.len() {
                return Err(ManagementError::NotFound);
            }
        }
        management::check_actor(&mut tx, ctx.actor, ctx.settings, admin).await?;
        if matches!(command, Command::Revoke { .. }) && target_principal != ctx.actor {
            management::check_actor(&mut tx, ctx.actor, ctx.settings, true)
                .await
                .map_err(|_| ManagementError::NotFound)?;
        }
        if !matches!(command, Command::Create(_)) {
            let current: Option<Uuid> =
                sqlx::query_scalar("SELECT owner_principal_id FROM gw_principals WHERE id=$1")
                    .bind(target_principal)
                    .fetch_optional(&mut *tx)
                    .await?
                    .ok_or(ManagementError::NotFound)?;
            if current != owner {
                return Err(ManagementError::Busy);
            }
        }
        if let Command::Revoke { id, .. } = command {
            sqlx::query("SELECT id FROM gw_virtual_keys WHERE id=$1 FOR UPDATE")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        }
        if matches!(command, Command::Issue(_) | Command::Policy(..)) {
            sqlx::query("SELECT id FROM gw_policies WHERE principal_id=$1 AND key_id IS NULL ORDER BY id FOR UPDATE").bind(target_principal).fetch_all(&mut *tx).await?;
        }
        // 정산도 actor/target/owner 전체를 먼저 잠근 후 금융 공통 순서를 따른다.
        let settlement_locks = if let Command::Settle(id, _) = command {
            Some(self.lock_request(&mut tx, *id).await?)
        } else {
            None
        };
        let digest = management::input_digest(ctx.method, ctx.path, &command.input()?);
        if let Some(prior) =
            sqlx::query("SELECT * FROM gw_commands WHERE actor_principal_id=$1 AND command_id=$2")
                .bind(ctx.actor)
                .bind(ctx.command_id)
                .fetch_optional(&mut *tx)
                .await?
        {
            if prior.get::<Vec<u8>, _>("input_digest") != digest {
                return Err(ManagementError::CommandConflict);
            }
            if prior.get::<String, _>("operation") == "issue_key" {
                return Err(ManagementError::SecretUnavailable(prior.get("target_id")));
            }
            let status = prior.get::<i16, _>("result_status") as u16;
            let mut ack: Value = prior.get("result_projection");
            ack["replayed"] = json!(true);
            return Ok(CommandResult {
                status,
                body: if status == 204 { None } else { Some(ack) },
                replayed: true,
            });
        }
        let now: String = sqlx::query_scalar(&format!("SELECT {}", stamp("clock_timestamp()")))
            .fetch_one(&mut *tx)
            .await?;
        let mut before = json!({});
        let mut after = json!({});
        let mut reason = None;
        let mut evidence = None;
        let mut changed = true;
        let mut result_version = Some(1_i64);
        let mut result_target = target;
        let mut secret = None;
        let status = match command {
            Command::Issue(v) => {
                let p = sqlx::query("SELECT * FROM gw_principals WHERE id=$1")
                    .bind(target_principal)
                    .fetch_one(&mut *tx)
                    .await?;
                if v.service_principal_id.is_some() && p.get::<String, _>("kind") != "service" {
                    return Err(ManagementError::Invalid);
                }
                if p.get::<Option<String>, _>("disabled_reason").is_some()
                    || p.get::<Option<String>, _>("security_fence_ref").is_some()
                {
                    return Err(ManagementError::Forbidden);
                }
                if let Some(owner) = owner {
                    management::check_actor(&mut tx, owner, ctx.settings, false).await?;
                }
                let policy: Value = sqlx::query_scalar(
                    "SELECT allow_rules FROM gw_policies WHERE principal_id=$1 AND key_id IS NULL",
                )
                .bind(target_principal)
                .fetch_one(&mut *tx)
                .await?;
                let grants = management::normalize_grants(v.grants.clone())?;
                require_subset(&grants, ctx.grants)?;
                require_subset(&grants, &policy)?;
                let count:i64=sqlx::query_scalar("SELECT count(*) FROM gw_virtual_keys WHERE principal_id=$1 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at>$2::timestamptz)").bind(target_principal).bind(&now).fetch_one(&mut *tx).await?;
                if count >= 32 {
                    return Err(ManagementError::Quota);
                }
                let limits = v.limits.effective(ctx.ceiling)?;
                let key = VirtualSecret::generate().map_err(|_| ManagementError::Invariant)?;
                result_target = key.id;
                let expires = v.validity.expires_at(&mut tx, &now).await?;
                sqlx::query("INSERT INTO gw_virtual_keys(id,principal_id,secret_hash,hash_version,public_prefix,name,grant_snapshot,validity_preset,expires_at,created_by,created_command_id,created_at) VALUES($1,$2,$3,1,$4,$5,$6,$7,$8::timestamptz,$9,$10,$11::timestamptz)").bind(key.id).bind(target_principal).bind(key.hash().as_slice()).bind(format!("mgw_v1_{}",&key.id.simple().to_string()[..12])).bind(&v.name).bind(grants.clone()).bind(v.validity.name()).bind(expires).bind(ctx.actor).bind(ctx.command_id).bind(&now).execute(&mut *tx).await?;
                insert_policy(
                    &mut tx,
                    Uuid::new_v4(),
                    target_principal,
                    Some(key.id),
                    grants,
                    &limits,
                    ctx.actor,
                )
                .await?;
                after = json!({"id":key.id,"principal_id":target_principal,"version":"1"});
                secret = Some(key.expose_once());
                201
            }
            Command::Revoke { id, expected } => {
                let row=sqlx::query("SELECT version,revoked_at IS NOT NULL AS revoked FROM gw_virtual_keys WHERE id=$1").bind(id).fetch_one(&mut *tx).await?;
                let current: i64 = row.get("version");
                before =
                    json!({"revoked":row.get::<bool,_>("revoked"),"version":current.to_string()});
                changed = !row.get::<bool, _>("revoked");
                if changed {
                    cas(expected, current)?;
                    sqlx::query("UPDATE gw_virtual_keys SET revoked_at=$2::timestamptz,version=version+1 WHERE id=$1").bind(id).bind(&now).execute(&mut *tx).await?;
                }
                result_version = Some(current + i64::from(changed));
                after = json!({"revoked":true,"version":result_version.map(|v|v.to_string())});
                204
            }
            Command::Create(v) => {
                management::check_actor(&mut tx, v.management_owner_id, ctx.settings, false)
                    .await?;
                sqlx::query("INSERT INTO gw_principals(id,kind,display_name,owner_principal_id,created_at,updated_at) VALUES($1,'service',$2,$3,$4::timestamptz,$4::timestamptz)").bind(target).bind(&v.display_name).bind(v.management_owner_id).bind(&now).execute(&mut *tx).await?;
                insert_policy(
                    &mut tx,
                    Uuid::new_v4(),
                    target,
                    None,
                    json!([]),
                    &PolicyLimits::default(),
                    ctx.actor,
                )
                .await?;
                after = json!({"kind":"service","display_name":v.display_name,"management_owner_id":v.management_owner_id,"version":"1"});
                201
            }
            Command::Update(id, v) => {
                let p = sqlx::query("SELECT * FROM gw_principals WHERE id=$1")
                    .bind(id)
                    .fetch_one(&mut *tx)
                    .await?;
                if p.get::<String, _>("kind") != "service" {
                    return Err(ManagementError::Invalid);
                }
                cas(&v.expected_version, p.get("version"))?;
                if let Some(o) = v.management_owner_id {
                    management::check_actor(&mut tx, o, ctx.settings, false).await?;
                }
                let name = v.display_name.as_deref().unwrap_or(p.get("display_name"));
                let new_owner = v
                    .management_owner_id
                    .or(owner)
                    .ok_or(ManagementError::Invariant)?;
                before = json!({"display_name":p.get::<String,_>("display_name"),"management_owner_id":owner,"version":p.get::<i64,_>("version").to_string()});
                changed = name != p.get::<String, _>("display_name") || Some(new_owner) != owner;
                result_version = Some(p.get::<i64, _>("version") + i64::from(changed));
                if changed {
                    sqlx::query("UPDATE gw_principals SET display_name=$2,owner_principal_id=$3,version=version+1,updated_at=$4::timestamptz WHERE id=$1").bind(id).bind(name).bind(new_owner).bind(&now).execute(&mut *tx).await?;
                }
                after = json!({"display_name":name,"management_owner_id":new_owner,"version":result_version.map(|v|v.to_string())});
                200
            }
            Command::Policy(id, v) => {
                let p = sqlx::query("SELECT * FROM gw_principals WHERE id=$1")
                    .bind(id)
                    .fetch_one(&mut *tx)
                    .await?;
                let policy=sqlx::query("SELECT *,budget_limit_usd::text AS budget FROM gw_policies WHERE principal_id=$1 AND key_id IS NULL").bind(id).fetch_one(&mut *tx).await?;
                cas(&v.expected_version, policy.get("version"))?;
                if v.disabled {
                    if *id == ctx.actor {
                        return Err(ManagementError::SecurityFence);
                    }
                    if p.get::<Option<String>, _>("email")
                        .is_some_and(|e| ctx.settings.is_admin(&e))
                    {
                        let remains:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM gw_principals WHERE kind='human' AND id<>$1 AND disabled_at IS NULL AND security_fence_ref IS NULL AND email=ANY($2))").bind(id).bind(&admin_emails).fetch_one(&mut *tx).await?;
                        if !remains {
                            return Err(ManagementError::SecurityFence);
                        }
                    }
                }
                let fence = v.reason.strip_prefix("security_config_fence:");
                if fence.is_some() && !v.disabled {
                    return Err(ManagementError::SecurityFence);
                }
                if let Some(f) = fence {
                    management::text(f, 100, 100)?;
                }
                if let Some(existing) = p.get::<Option<String>, _>("security_fence_ref") {
                    if !v.disabled {
                        let release = v
                            .fence_release
                            .as_ref()
                            .ok_or(ManagementError::SecurityFence)?;
                        if release.fence_ref != existing || *id == ctx.actor {
                            return Err(ManagementError::SecurityFence);
                        }
                        evidence = Some(release.old_processes_stopped_ref.clone());
                    } else if fence != Some(existing.as_str()) {
                        return Err(ManagementError::SecurityFence);
                    }
                } else if v.fence_release.is_some() {
                    return Err(ManagementError::SecurityFence);
                }
                let grants = management::normalize_grants(v.grants.clone())?;
                require_subset(&grants, ctx.grants)?;
                let limits = super::policy::validate_policy_transition(
                    &mut tx,
                    policy.get("id"),
                    &v.limits,
                    ctx.ceiling,
                )
                .await?;
                before = json!({"version":policy.get::<i64,_>("version").to_string(),"disabled":p.get::<Option<String>,_>("disabled_reason").is_some(),"grants":policy.get::<Value,_>("allow_rules"),"limits":super::management_query::limits(&policy),"security_fence_ref":p.get::<Option<String>,_>("security_fence_ref")});
                let old_disabled = p.get::<Option<String>, _>("disabled_reason").is_some();
                let new_fence = if v.disabled {
                    fence.map(str::to_owned)
                } else {
                    None
                };
                changed = before["grants"] != grants
                    || before["limits"]
                        != serde_json::to_value(&limits).map_err(|_| ManagementError::Invariant)?
                    || old_disabled != v.disabled
                    || p.get::<Option<String>, _>("security_fence_ref") != new_fence;
                result_version = Some(policy.get::<i64, _>("version") + i64::from(changed));
                if changed {
                    sqlx::query("UPDATE gw_policies SET allow_rules=$2,budget_mode=$3,budget_limit_usd=gw_amount($4::numeric),period_timezone=$5,rpm_limit=$6,tpm_limit=$7,count_rpm_limit=$8,version=version+1,updated_by=$9,updated_at=$10::timestamptz WHERE id=$1").bind(policy.get::<Uuid,_>("id")).bind(&grants).bind(limits.budget.as_ref().map_or(policy.get::<String,_>("budget_mode"),|b|b.mode.clone())).bind(limits.budget.as_ref().map(|b|&b.limit_usd)).bind(limits.budget.as_ref().map_or(policy.get::<String,_>("period_timezone"),|b|b.period.timezone.clone())).bind(limits.rpm).bind(limits.tpm).bind(limits.count_rpm).bind(ctx.actor).bind(&now).execute(&mut *tx).await?;
                    if old_disabled != v.disabled
                        || p.get::<Option<String>, _>("security_fence_ref") != new_fence
                    {
                        sqlx::query("UPDATE gw_principals SET disabled_at=CASE WHEN $2 THEN $3::timestamptz END,disabled_reason=CASE WHEN $2 THEN $4 END,security_fence_ref=$5,version=version+1,updated_at=$3::timestamptz WHERE id=$1").bind(id).bind(v.disabled).bind(&now).bind(if new_fence.is_some(){"security_config_fence"}else{"admin_policy"}).bind(&new_fence).execute(&mut *tx).await?;
                    }
                }
                reason = Some(v.reason.clone());
                after = json!({"version":result_version.map(|v|v.to_string()),"disabled":v.disabled,"grants":grants,"limits":limits,"security_fence_ref":new_fence});
                200
            }
            Command::Release(id, v) => {
                let p = sqlx::query("SELECT * FROM gw_principals WHERE id=$1")
                    .bind(id)
                    .fetch_one(&mut *tx)
                    .await?;
                let version: i64 = p.get("inference_safety_version");
                cas(&v.expected_version, version)?;
                changed = p
                    .get::<Option<String>, _>("inference_pause_reason")
                    .is_some();
                before = json!({"paused":changed,"version":version.to_string(),"reason":p.get::<Option<String>,_>("inference_pause_reason"),"trigger_request_id":p.get::<Option<Uuid>,_>("inference_pause_request_id")});
                if changed {
                    sqlx::query("UPDATE gw_principals SET inference_paused_at=NULL,inference_pause_reason=NULL,inference_pause_request_id=NULL,inference_safety_version=inference_safety_version+1 WHERE id=$1").bind(id).execute(&mut *tx).await?;
                }
                result_version = Some(version + i64::from(changed));
                reason = Some(v.reason.clone());
                after = json!({"paused":false,"version":result_version.map(|v|v.to_string())});
                200
            }
            Command::Settle(id, v) => {
                let (_, r, budgets) = settlement_locks.ok_or(ManagementError::Invariant)?;
                let semantics = super::management::input_digest(
                    "SETTLEMENT",
                    &id.to_string(),
                    &json!({"decision":v.decision,"cost_usd":v.cost_usd,"reason":v.reason}),
                );
                if let Some(prior) = r.get::<Option<Vec<u8>>, _>("settlement_digest") {
                    if prior != semantics {
                        return Err(ManagementError::SettlementConflict);
                    }
                    changed = false;
                    result_version = Some(r.get("version"));
                } else {
                    cas(&v.expected_version, r.get("version"))?;
                    if r.get::<String, _>("accounting_state") != "unknown" {
                        return Err(ManagementError::SettlementConflict);
                    }
                    // 실행 중인 로컬 송신 경로는 관리자 진술로 해제할 수 없다.
                    if ctx.execution_running {
                        return Err(ManagementError::ExecutionNotFinal);
                    }
                    let stopped: bool = sqlx::query_scalar(
                        "SELECT execution_stopped_at IS NOT NULL FROM gw_requests WHERE id=$1",
                    )
                    .bind(id)
                    .fetch_one(&mut *tx)
                    .await?;
                    if !stopped && v.execution_stopped_confirmed != Some(true) {
                        return Err(ManagementError::ExecutionNotFinal);
                    }
                    let source = if stopped {
                        "runtime"
                    } else {
                        "admin_confirmation"
                    };
                    self.apply_manual_settlement(
                        &mut tx, *id, &r, &budgets, v, ctx.actor, &now, source, &semantics,
                    )
                    .await?;
                    result_version = Some(r.get::<i64, _>("version") + 1);
                    before = json!({"accounting_state":"unknown","cost_usd":null,"version":r.get::<i64,_>("version").to_string()});
                    after = json!({"accounting_state":"settled","cost_usd":v.cost_usd,"source":"manual_entry","decision":v.decision,"executor_id":r.get::<Uuid,_>("executor_id"),"execution_stop_source":source,"version":result_version.map(|v|v.to_string())});
                    reason = Some(v.reason.clone());
                }
                200
            }
        };
        let ack = json!({"command_id":ctx.command_id,"operation":command.operation(),"target":{"kind":kind,"id":result_target},"result_version":result_version.map(|v|v.to_string()),"effect":if changed{"changed"}else{"noop"}});
        sqlx::query("INSERT INTO gw_commands(actor_principal_id,command_id,input_digest,operation,target_kind,target_id,result_status,result_version,result_projection,created_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10::timestamptz)").bind(ctx.actor).bind(ctx.command_id).bind(digest.as_slice()).bind(command.operation()).bind(kind).bind(result_target).bind(status as i16).bind(result_version).bind(&ack).bind(&now).execute(&mut *tx).await?;
        if changed {
            sqlx::query("INSERT INTO gw_audit_events(actor_kind,actor_principal_id,command_id,actor_auth_config_digest,operation,target_kind,target_id,before_snapshot,after_snapshot,reason,evidence_ref,occurred_at) VALUES('human',$1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11::timestamptz)").bind(ctx.actor).bind(ctx.command_id).bind(ctx.digest.as_slice()).bind(command.operation()).bind(kind).bind(result_target).bind(before).bind(after).bind(reason).bind(evidence).bind(&now).execute(&mut *tx).await?;
        }
        let body = if let Some(secret) = secret {
            Some(json!({"key":secret,"metadata":key_metadata(&mut tx,result_target).await?}))
        } else if status == 204 {
            None
        } else {
            let mut ack = ack;
            ack["replayed"] = json!(false);
            Some(ack)
        };
        tx.commit().await?;
        Ok(CommandResult {
            status,
            body,
            replayed: false,
        })
    }
}
