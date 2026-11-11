use super::{
    super::{
        PgLedger,
        commands::CommandContext,
        management::{self, ManagementError},
    },
    types::*,
};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

// table명과 열은 이 모듈의 고정 DTO만 생성한다. 입력 SQL/식/외부 DB adapter는 받지 않는다.
async fn insert(
    tx: &mut Transaction<'_, Postgres>,
    table: &'static str,
    value: Value,
) -> Result<(), ManagementError> {
    let o = value.as_object().ok_or(ManagementError::Invalid)?;
    let columns = o.keys().map(String::as_str).collect::<Vec<_>>().join(",");
    let query = format!(
        "INSERT INTO {table} ({columns}) SELECT {columns} FROM jsonb_populate_record(NULL::{table},$1)"
    );
    sqlx::query(&query).bind(value).execute(&mut **tx).await?;
    Ok(())
}
fn value(v: &impl Serialize) -> Result<Value, ManagementError> {
    serde_json::to_value(v).map_err(|_| ManagementError::Invalid)
}
impl PgLedger {
    pub(super) async fn import_financial_bundle(
        &self,
        ctx: &CommandContext<'_>,
        input: Import,
    ) -> Result<Value, ManagementError> {
        input.validate().map_err(|_| ManagementError::Invalid)?;
        let digest = management::input_digest(
            "MAINTENANCE",
            "handoff/import",
            &json!({"target_id":input.target_id,"source_id":input.source_id,"source_ref":input.source_ref,"previous_writer_fenced_ref":input.previous_writer_fenced_ref,"previous_executors_stopped_ref":input.previous_executors_stopped_ref,"cutoff":input.cutoff,"bundle":input.bundle}),
        );
        let _gate = self
            .database()
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| ManagementError::Database)?;
        tokio::time::timeout(std::time::Duration::from_secs(3),async {
   let mut tx=self.database().begin_transaction().await?;
   let p=super::lock_target(&mut tx,ctx,input.target_id).await?;
   let command_digest=management::input_digest("MAINTENANCE","handoff/import-command",&json!({"source_digest":digest,"expected_version":input.expected_version}));
   if let Some(prior)=super::replay(&mut tx,ctx,&command_digest,"imported").await?{return Ok(prior);}
   if let Some(prior)=sqlx::query("SELECT source_id,source_digest FROM gw_handoffs WHERE principal_id=$1").bind(input.target_id).fetch_optional(&mut *tx).await?{
    if prior.get::<Uuid,_>("source_id")!=input.source_id||prior.get::<Vec<u8>,_>("source_digest")!=digest{return Err(ManagementError::CommandConflict);}
    return super::commit_result(tx,ctx,&command_digest,input.target_id,p.get("version"),"imported",false,None).await;
   }
   management::cas(&input.expected_version,p.get("version"))?;
   if p.get::<Option<String>,_>("security_fence_ref").as_deref()!=Some(input.source_id.to_string().as_str()){return Err(ManagementError::SecurityFence);}
   let identity=&input.bundle.identity;
   if p.get::<String,_>("kind")!=identity.kind||p.get::<Option<String>,_>("issuer")!=identity.issuer||p.get::<Option<String>,_>("subject")!=identity.subject||p.get::<Option<Uuid>,_>("owner_principal_id")!=identity.owner_principal_id{return Err(ManagementError::Forbidden);}
   let empty:bool=sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM gw_requests WHERE principal_id=$1) AND NOT EXISTS(SELECT 1 FROM gw_virtual_keys WHERE principal_id=$1) AND NOT EXISTS(SELECT 1 FROM gw_usage_totals t JOIN gw_policies q ON q.id=t.scope_id WHERE q.principal_id=$1) AND NOT EXISTS(SELECT 1 FROM gw_policies WHERE principal_id=$1 AND (allow_rules<>'[]' OR key_id IS NOT NULL))").bind(input.target_id).fetch_one(&mut *tx).await?;
   let cutoff_valid:bool=sqlx::query_scalar("SELECT $1::timestamptz<=clock_timestamp()").bind(&input.cutoff).fetch_one(&mut *tx).await?;
   if !empty||!cutoff_valid||p.get::<Option<String>,_>("inference_pause_reason").is_some(){return Err(ManagementError::Invalid);}
   let now:String=sqlx::query_scalar("SELECT clock_timestamp()::text").fetch_one(&mut *tx).await?;
   // 새 주체의 아직 사용되지 않은 deny-all scope만 제거한다. 기존 usage/기간을 덮는 경로는 없다.
   sqlx::query("DELETE FROM gw_policies WHERE principal_id=$1").bind(input.target_id).execute(&mut *tx).await?;
   for key in &input.bundle.keys {
    let mut secret=crate::auth::key::VirtualSecret::generate().map_err(|_|ManagementError::Invariant)?;
    // hash_version1의 domain UUID도 보존하는 source key ID와 같아야 한다. 비밀은 새로 생성하고 노출하지 않는다.
    secret.id=key.id;
    let version=management::version(&key.version)?.checked_add(i64::from(key.revoked_at.is_none())).ok_or(ManagementError::Invariant)?;
    let row=json!({"id":key.id,"principal_id":input.target_id,"secret_hash":format!("\\x{}",secret.hash().iter().map(|b|format!("{b:02x}")).collect::<String>()),"hash_version":1,"public_prefix":key.public_prefix,"name":key.name,"grant_snapshot":management::normalize_grants(key.grant_snapshot.clone())?,"validity_preset":key.validity_preset,"expires_at":key.expires_at,"revoked_at":key.revoked_at.as_ref().unwrap_or(&now),"created_at":key.created_at,"created_by":ctx.actor,"created_command_id":ctx.command_id,"version":version});
    insert(&mut tx,"gw_virtual_keys",row).await?;
   }
   if input.bundle.scopes.iter().filter(|s|s.key_id.is_none()).count()!=1{return Err(ManagementError::Invalid);}
   for s in &input.bundle.scopes{
    let limits=&s.limits;
    let row=json!({"id":s.id,"principal_id":input.target_id,"key_id":s.key_id,"version":s.version,"allow_rules":management::normalize_grants(s.grants.clone())?,"budget_mode":s.budget_mode,"budget_limit_usd":limits.budget.as_ref().map(|b|&b.limit_usd),"period_kind":s.period_kind,"period_timezone":s.period_timezone,"rpm_limit":limits.rpm,"tpm_limit":limits.tpm,"count_rpm_limit":limits.count_rpm,"updated_by":ctx.actor,"created_at":s.created_at,"updated_at":s.updated_at});
    insert(&mut tx,"gw_policies",row).await?;
   }
   for t in &input.bundle.periods{insert(&mut tx,"gw_usage_totals",value(t)?).await?;}
   for w in &input.bundle.windows{insert(&mut tx,"gw_rate_windows",value(w)?).await?;}
   // root를 먼저 삽입한다. 입력의 부모/자식 배열 순서에 의존하지 않는다.
   for root in [true,false]{for r in input.bundle.requests.iter().filter(|r|r.parent_request_id.is_none()==root){let mut row=value(r)?;row["principal_id"]=json!(input.target_id);row["phase"]=json!("finished");insert(&mut tx,"gw_requests",row).await?;}}
   for b in &input.bundle.budgets{insert(&mut tx,"gw_request_budgets",value(b)?).await?;}
   for e in &input.bundle.events{insert(&mut tx,"gw_usage_events",value(e)?).await?;}
   let valid:bool=sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM gw_usage_totals t JOIN gw_policies p ON p.id=t.scope_id WHERE p.principal_id=$1 AND (t.timezone_snapshot<>p.period_timezone OR t.period_start<>date_trunc('month',t.period_start AT TIME ZONE t.timezone_snapshot) AT TIME ZONE t.timezone_snapshot OR t.period_end<>(date_trunc('month',t.period_start AT TIME ZONE t.timezone_snapshot)+interval '1 month') AT TIME ZONE t.timezone_snapshot OR t.cost_usd<>t.carried_cost_usd+(SELECT coalesce(sum(b.applied_cost_usd),0) FROM gw_request_budgets b WHERE b.scope_id=t.scope_id AND b.period_start=t.period_start AND b.state='settled') OR t.reserved_usd<>(SELECT coalesce(sum(b.remaining_reservation_usd),0) FROM gw_request_budgets b WHERE b.scope_id=t.scope_id AND b.period_start=t.period_start) OR t.unknown_count<>(SELECT count(*) FROM gw_request_budgets b WHERE b.scope_id=t.scope_id AND b.period_start=t.period_start AND b.state='unknown') OR t.settled_count<>t.carried_settled_count+(SELECT count(*) FROM gw_request_budgets b WHERE b.scope_id=t.scope_id AND b.period_start=t.period_start AND b.state='settled') OR t.input_tokens<>t.carried_input_tokens+(SELECT coalesce(sum(b.applied_input_tokens),0) FROM gw_request_budgets b WHERE b.scope_id=t.scope_id AND b.period_start=t.period_start AND b.state='settled') OR t.output_tokens<>t.carried_output_tokens+(SELECT coalesce(sum(b.applied_output_tokens),0) FROM gw_request_budgets b WHERE b.scope_id=t.scope_id AND b.period_start=t.period_start AND b.state='settled') OR t.settled_unknown_tokens_count<>t.carried_settled_unknown_tokens_count+(SELECT count(*) FROM gw_request_budgets b WHERE b.scope_id=t.scope_id AND b.period_start=t.period_start AND b.state='settled' AND (b.applied_input_tokens IS NULL OR b.applied_output_tokens IS NULL)))) AND NOT EXISTS(SELECT 1 FROM gw_requests r WHERE r.principal_id=$1 AND NOT EXISTS(SELECT 1 FROM gw_request_budgets b WHERE b.request_id=r.id)) AND NOT EXISTS(SELECT 1 FROM gw_rate_windows w JOIN gw_policies p ON p.id=w.scope_id WHERE p.principal_id=$1 AND w.reserved_tokens<>(SELECT coalesce(sum(b.remaining_reserved_tokens),0) FROM gw_request_budgets b WHERE b.scope_id=w.scope_id AND b.rate_lane=w.lane AND b.rate_start=w.window_start))").bind(input.target_id).fetch_one(&mut *tx).await?;
   if !valid{return Err(ManagementError::Invariant);}
   sqlx::query("UPDATE gw_usage_totals t SET detail_deleted_through=GREATEST(detail_deleted_through,LEAST($2::timestamptz,period_end)-interval '1 microsecond') FROM gw_policies p WHERE p.id=t.scope_id AND p.principal_id=$1 AND $2::timestamptz>t.period_start AND (t.carried_cost_usd>0 OR t.carried_settled_count>0 OR t.carried_input_tokens>0 OR t.carried_output_tokens>0 OR t.carried_settled_unknown_tokens_count>0)").bind(input.target_id).bind(&input.cutoff).execute(&mut *tx).await?;
   let safety=&input.bundle.safety;
   let new_version=p.get::<i64,_>("version").max(management::version(&input.bundle.source_principal_version)?).checked_add(1).ok_or(ManagementError::Invariant)?;
   sqlx::query("UPDATE gw_principals SET version=$2,inference_paused_at=$3::timestamptz,inference_pause_reason=$4,inference_pause_request_id=$5,inference_safety_version=$6,updated_at=clock_timestamp() WHERE id=$1").bind(input.target_id).bind(new_version).bind(&safety.inference_paused_at).bind(&safety.inference_pause_reason).bind(safety.inference_pause_request_id).bind(management::version(&safety.inference_safety_version)?).execute(&mut *tx).await?;
   // 최초 source receipt와 같은 transaction의 복구 가능한 baseline이다. request는 최대100개이며
   // 두 budget scope/중복 event/안전 version을 횟수로 더하지 않고, 삭제된 과거 이력은 추정하지 않는다.
   // 금융 pause→unknown과 같은 고정 행 잠금 순서다. replay/noop는 이 지점 전에 반환한다.
   for (code, query) in [
    ("bound", "SELECT count(*) FROM gw_requests WHERE principal_id=$1 AND (safety_violation_recorded OR id=$2)"),
    ("unknown", "SELECT count(*) FROM gw_requests r WHERE principal_id=$1 AND (accounting_state='unknown' OR EXISTS(SELECT 1 FROM gw_usage_events e WHERE e.request_id=r.id AND e.kind IN ('settlement','late_final')))")
   ] {
    let baseline = if code=="bound" {
     sqlx::query_scalar::<_,i64>(query).bind(input.target_id).bind(safety.inference_pause_request_id).fetch_one(&mut *tx).await?
    } else {
     sqlx::query_scalar::<_,i64>(query).bind(input.target_id).fetch_one(&mut *tx).await?
    };
    let updated=sqlx::query("UPDATE gw_alarm_totals SET total=total+$2 WHERE code=$1").bind(code).bind(baseline).execute(&mut *tx).await?;
    if updated.rows_affected()!=1{return Err(ManagementError::Invariant);}
   }
   sqlx::query("INSERT INTO gw_handoffs(principal_id,source_id,source_digest,source_ref,cutoff,previous_executors_stopped_ref,source_disabled,phase) VALUES($1,$2,$3,$4,$5::timestamptz,$6,$7,'imported')").bind(input.target_id).bind(input.source_id).bind(digest.as_slice()).bind(&input.source_ref).bind(&input.cutoff).bind(&input.previous_executors_stopped_ref).bind(input.bundle.original_disabled).execute(&mut *tx).await?;
   let summary=json!({"source_id":input.source_id,"source_ref":input.source_ref,"cutoff":input.cutoff,"requests":input.bundle.requests.len(),"scopes":input.bundle.scopes.len(),"source_disabled_reason":input.bundle.original_disabled_reason,"source_security_fence_ref":input.bundle.original_security_fence_ref,"keys_revoked_for_reissue":input.bundle.keys.iter().map(|k|json!({"id":k.id,"original_version":k.version,"original_revoked_at":k.revoked_at,"original_created_by":k.original_created_by,"original_created_command_id":k.original_created_command_id})).collect::<Vec<_>>(),"previous_writer_fenced_ref":input.previous_writer_fenced_ref,"previous_executors_stopped_ref":input.previous_executors_stopped_ref});
   super::commit_result(tx,ctx,&command_digest,input.target_id,new_version,"imported",true,Some(summary)).await
  }).await.map_err(|_|ManagementError::Busy)?
    }
}
