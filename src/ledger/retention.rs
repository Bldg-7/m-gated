use super::{
    PgLedger,
    commands::CommandContext,
    management::{self, ManagementError},
};
use crate::config::Retention;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionDelete {
    pub after: Option<Uuid>,
    pub limit: u16,
    pub deletion_approved: bool,
    pub approval_ref: String,
    pub reason: String,
}
impl PgLedger {
    /// 명시적 삭제 승인 입력만 실행한다. 기본 서버/reaper는 이 함수를 호출하지 않는다.
    pub async fn retention_delete(
        &self,
        ctx: &CommandContext<'_>,
        policy: &Retention,
        input: &RetentionDelete,
    ) -> Result<Value, ManagementError> {
        if !input.deletion_approved || !(1..=50).contains(&input.limit) {
            return Err(ManagementError::Invalid);
        }
        management::text(&input.approval_ref, 500, 2048)?;
        management::text(&input.reason, 1000, 4096)?;
        let _gate = self
            .database()
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| ManagementError::Database)?;
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            self.retention_transaction(ctx, policy, input),
        )
        .await
        .map_err(|_| ManagementError::Busy)?
    }
    async fn retention_transaction(
        &self,
        ctx: &CommandContext<'_>,
        policy: &Retention,
        input: &RetentionDelete,
    ) -> Result<Value, ManagementError> {
        let mut tx = self.database().begin_transaction().await?;
        // 제한 전 전체 조회하지 않는다. 최대50 root/100 request만 하나의 작업에 보관한다.
        let roots: Vec<Uuid> = if let Some(months) = policy.ledger.and_then(|p| p.months()) {
            sqlx::query_scalar("SELECT r.id FROM gw_requests r WHERE r.parent_request_id IS NULL AND ($1::uuid IS NULL OR r.id>$1) AND NOT EXISTS(SELECT 1 FROM gw_requests f WHERE (f.id=r.id OR f.parent_request_id=r.id) AND (f.phase<>'finished' OR f.accounting_state<>'settled')) AND ((GREATEST((SELECT max(f.finished_at) FROM gw_requests f WHERE f.id=r.id OR f.parent_request_id=r.id),(SELECT max(e.observed_at) FROM gw_usage_events e JOIN gw_requests f ON f.id=e.request_id WHERE f.id=r.id OR f.parent_request_id=r.id)) AT TIME ZONE 'UTC')+make_interval(months=>$2)) AT TIME ZONE 'UTC'<=clock_timestamp() ORDER BY r.id LIMIT $3")
                .bind(input.after).bind(months as i32).bind(i64::from(input.limit)).fetch_all(&mut *tx).await?
        } else {
            Vec::new()
        };
        let families = sqlx::query("SELECT r.id,r.principal_id,p.owner_principal_id,r.key_id FROM gw_requests r JOIN gw_principals p ON p.id=r.principal_id WHERE r.id=ANY($1) OR r.parent_request_id=ANY($1) ORDER BY r.id").bind(&roots).fetch_all(&mut *tx).await?;
        if families.len() > 100 {
            return Err(ManagementError::Invariant);
        }
        let mut principals = vec![ctx.actor];
        let mut ids = Vec::new();
        for r in &families {
            ids.push(r.get::<Uuid, _>("id"));
            principals.push(r.get("principal_id"));
            if let Some(owner) = r.get::<Option<Uuid>, _>("owner_principal_id") {
                principals.push(owner);
            }
        }
        principals.sort();
        principals.dedup();
        for principal in &principals {
            sqlx::query("SELECT id FROM gw_principals WHERE id=$1 FOR UPDATE")
                .bind(principal)
                .fetch_one(&mut *tx)
                .await?;
        }
        management::check_actor(&mut tx, ctx.actor, ctx.settings, true).await?;
        for r in &families {
            let owner: Option<Uuid> =
                sqlx::query_scalar("SELECT owner_principal_id FROM gw_principals WHERE id=$1")
                    .bind(r.get::<Uuid, _>("principal_id"))
                    .fetch_one(&mut *tx)
                    .await?;
            if owner != r.get::<Option<Uuid>, _>("owner_principal_id") {
                return Err(ManagementError::Busy);
            }
        }
        let digest = management::input_digest(
            "MAINTENANCE",
            "retention_delete",
            &json!({"input":input,"policy":policy}),
        );
        if let Some(prior)=sqlx::query("SELECT input_digest,result_projection FROM gw_commands WHERE actor_principal_id=$1 AND command_id=$2").bind(ctx.actor).bind(ctx.command_id).fetch_optional(&mut *tx).await? {
            if prior.get::<Vec<u8>,_>("input_digest")!=digest { return Err(ManagementError::CommandConflict); }
            let mut ack:Value=prior.get("result_projection"); ack["replayed"]=json!(true);
            let detail: Option<Value>=sqlx::query_scalar("SELECT after_snapshot FROM gw_audit_events WHERE actor_principal_id=$1 AND command_id=$2").bind(ctx.actor).bind(ctx.command_id).fetch_optional(&mut *tx).await?;
            return Ok(json!({"ack":ack,"result":detail.unwrap_or(json!({"deleted_requests":0,"next_after":null}))}));
        }
        // 가족 전체의 공통 잠금 순서. root request를 잠근 뒤 child의 rate 잠금을 잡지 않는다.
        sqlx::query("SELECT k.id FROM gw_virtual_keys k WHERE k.id IN(SELECT key_id FROM gw_requests WHERE id=ANY($1)) ORDER BY k.id FOR UPDATE").bind(&ids).fetch_all(&mut *tx).await?;
        sqlx::query("SELECT p.id FROM gw_policies p WHERE p.id IN(SELECT scope_id FROM gw_request_budgets WHERE request_id=ANY($1)) ORDER BY p.id FOR UPDATE").bind(&ids).fetch_all(&mut *tx).await?;
        sqlx::query("SELECT t.scope_id FROM gw_usage_totals t WHERE EXISTS(SELECT 1 FROM gw_request_budgets b WHERE b.request_id=ANY($1) AND b.scope_id=t.scope_id AND b.period_start=t.period_start) ORDER BY t.scope_id,t.period_start FOR UPDATE").bind(&ids).fetch_all(&mut *tx).await?;
        sqlx::query("SELECT w.scope_id FROM gw_rate_windows w WHERE EXISTS(SELECT 1 FROM gw_request_budgets b WHERE b.request_id=ANY($1) AND b.scope_id=w.scope_id AND b.rate_lane=w.lane AND b.rate_start=w.window_start) ORDER BY w.scope_id,w.lane,w.window_start FOR UPDATE").bind(&ids).fetch_all(&mut *tx).await?;
        sqlx::query("SELECT id FROM gw_requests WHERE id=ANY($1) ORDER BY id FOR UPDATE")
            .bind(&ids)
            .fetch_all(&mut *tx)
            .await?;
        sqlx::query("SELECT request_id FROM gw_request_budgets WHERE request_id=ANY($1) ORDER BY request_id,scope_id FOR UPDATE").bind(&ids).fetch_all(&mut *tx).await?;
        let months = policy.ledger.and_then(|p| p.months()).unwrap_or(0) as i32;
        let eligible: Vec<Uuid>=sqlx::query_scalar("SELECT r.id FROM gw_requests r WHERE r.id=ANY($1) AND NOT EXISTS(SELECT 1 FROM gw_requests f WHERE (f.id=r.id OR f.parent_request_id=r.id) AND (f.phase<>'finished' OR f.accounting_state<>'settled' OR EXISTS(SELECT 1 FROM gw_request_budgets b WHERE b.request_id=f.id AND (b.state<>'settled' OR b.remaining_reservation_usd<>0 OR b.remaining_reserved_tokens<>0)) OR EXISTS(SELECT 1 FROM gw_principals p WHERE p.inference_pause_request_id=f.id))) AND ((GREATEST((SELECT max(f.finished_at) FROM gw_requests f WHERE f.id=r.id OR f.parent_request_id=r.id),(SELECT max(e.observed_at) FROM gw_usage_events e JOIN gw_requests f ON f.id=e.request_id WHERE f.id=r.id OR f.parent_request_id=r.id)) AT TIME ZONE 'UTC')+make_interval(months=>$2)) AT TIME ZONE 'UTC'<=clock_timestamp() ORDER BY r.id").bind(&roots).bind(months).fetch_all(&mut *tx).await?;
        let deleted: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM gw_requests WHERE id=ANY($1) OR parent_request_id=ANY($1) ORDER BY id",
        )
        .bind(&eligible)
        .fetch_all(&mut *tx)
        .await?;
        sqlx::query("WITH carried AS (SELECT b.scope_id,b.period_start,sum(b.applied_cost_usd) AS cost,count(*) AS n,sum(coalesce(b.applied_input_tokens,0)) AS i,sum(coalesce(b.applied_output_tokens,0)) AS o,count(*) FILTER(WHERE b.applied_input_tokens IS NULL OR b.applied_output_tokens IS NULL) AS u,max(r.admitted_at) AS boundary FROM gw_request_budgets b JOIN gw_requests r ON r.id=b.request_id WHERE b.request_id=ANY($1) GROUP BY b.scope_id,b.period_start) UPDATE gw_usage_totals t SET carried_cost_usd=gw_amount(t.carried_cost_usd+c.cost),carried_settled_count=t.carried_settled_count+c.n,carried_input_tokens=t.carried_input_tokens+c.i,carried_output_tokens=t.carried_output_tokens+c.o,carried_settled_unknown_tokens_count=t.carried_settled_unknown_tokens_count+c.u,detail_deleted_through=GREATEST(t.detail_deleted_through,c.boundary),updated_at=clock_timestamp() FROM carried c WHERE t.scope_id=c.scope_id AND t.period_start=c.period_start").bind(&deleted).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM gw_usage_events WHERE request_id=ANY($1)")
            .bind(&deleted)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM gw_request_budgets WHERE request_id=ANY($1)")
            .bind(&deleted)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM gw_requests WHERE id=ANY($1) AND parent_request_id IS NOT NULL")
            .bind(&deleted)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM gw_requests WHERE id=ANY($1)")
            .bind(&deleted)
            .execute(&mut *tx)
            .await?;
        // 명령의 작은 Ack는 idempotency tombstone이다. 키 발급/회수·정산·handoff 재사용 효과를 막기 위해 보존한다.
        // human 명령 감사도 rollback/삭제 추적과 replay 결과의 참조이므로 함께 보호한다.
        if let Some(months) = policy.audit.and_then(|p| p.months()) {
            sqlx::query("DELETE FROM gw_audit_events WHERE id IN(SELECT a.id FROM gw_audit_events a WHERE a.actor_kind='system' AND ((a.occurred_at AT TIME ZONE 'UTC')+make_interval(months=>$1)) AT TIME ZONE 'UTC'<=clock_timestamp() AND NOT EXISTS(SELECT 1 FROM gw_requests r WHERE r.id=a.request_id) AND NOT EXISTS(SELECT 1 FROM gw_principals p WHERE p.inference_pause_request_id=a.request_id) ORDER BY a.occurred_at,a.id LIMIT 100)").bind(months as i32).execute(&mut *tx).await?;
        }
        if let Some(months) = policy.ledger.and_then(|p| p.months()) {
            sqlx::query("DELETE FROM gw_rate_windows WHERE (scope_id,lane,window_start) IN(SELECT w.scope_id,w.lane,w.window_start FROM gw_rate_windows w WHERE ((w.window_end AT TIME ZONE 'UTC')+make_interval(months=>$1)) AT TIME ZONE 'UTC'<=clock_timestamp() AND NOT EXISTS(SELECT 1 FROM gw_request_budgets b WHERE b.scope_id=w.scope_id AND b.rate_lane=w.lane AND b.rate_start=w.window_start) ORDER BY w.window_end,w.scope_id,w.lane,w.window_start LIMIT 100)").bind(months as i32).execute(&mut *tx).await?;
        }
        let target = ctx.command_id;
        let ack = json!({"command_id":ctx.command_id,"operation":"retention_delete","target":{"kind":"ledger","id":target},"result_version":null,"effect":if deleted.is_empty(){"noop"}else{"changed"}});
        let detail = json!({"deleted_requests":deleted.len(),"next_after":roots.last(),"request_ids":deleted});
        sqlx::query("INSERT INTO gw_commands(actor_principal_id,command_id,input_digest,operation,target_kind,target_id,result_status,result_projection) VALUES($1,$2,$3,'retention_delete','ledger',$2,200,$4)").bind(ctx.actor).bind(ctx.command_id).bind(digest.as_slice()).bind(&ack).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO gw_audit_events(actor_kind,actor_principal_id,command_id,actor_auth_config_digest,operation,target_kind,target_id,before_snapshot,after_snapshot,reason,evidence_ref) VALUES('human',$1,$2,$3,'retention_delete','ledger',$2,'{}',$4,$5,$6)").bind(ctx.actor).bind(ctx.command_id).bind(ctx.digest.as_slice()).bind(&detail).bind(&input.reason).bind(&input.approval_ref).execute(&mut *tx).await?;
        tx.commit().await?;
        crate::telemetry::add(crate::telemetry::Metric::Deleted, deleted.len() as u64);
        let mut ack = ack;
        ack["replayed"] = json!(false);
        Ok(json!({"ack":ack,"result":detail}))
    }
}
