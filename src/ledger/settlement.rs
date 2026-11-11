use super::{
    PgLedger,
    amount::Amount,
    management::{ManagementError, Settlement},
};
use serde_json::json;
use sqlx::{PgConnection, Row, postgres::PgRow};
use uuid::Uuid;
impl PgLedger {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn apply_manual_settlement(
        &self,
        c: &mut PgConnection,
        id: Uuid,
        r: &PgRow,
        budgets: &[PgRow],
        input: &Settlement,
        actor: Uuid,
        now: &str,
        stop_source: &str,
        digest: &[u8],
    ) -> Result<(), ManagementError> {
        let cost: Amount = input
            .cost_usd
            .parse()
            .map_err(|_| ManagementError::Invalid)?;
        for b in budgets {
            // 금액/카운터 overflow는 전체 command/audit와 함께 rollback한다.
            let old: Amount = b
                .get::<String, _>("total_cost")
                .parse()
                .map_err(|_| ManagementError::Invariant)?;
            old.checked_add(cost)
                .map_err(|_| ManagementError::Invariant)?;
            for name in ["total_settled_count", "total_unknown_tokens_count"] {
                b.get::<i64, _>(name)
                    .checked_add(1)
                    .ok_or(ManagementError::Invariant)?;
            }
            let reserved: i64 = b.get("remaining_reserved_tokens");
            b.get::<i64, _>("rate_settled_tokens")
                .checked_add(reserved)
                .ok_or(ManagementError::Invariant)?;
            if b.get::<i64, _>("total_unknown_count") == 0 {
                return Err(ManagementError::Invariant);
            }
            let scope: Uuid = b.get("scope_id");
            sqlx::query("UPDATE gw_usage_totals SET cost_usd=gw_amount(cost_usd+gw_amount($3::numeric)),reserved_usd=gw_amount(reserved_usd-$4::numeric),unknown_count=unknown_count-1,settled_count=settled_count+1,settled_unknown_tokens_count=settled_unknown_tokens_count+1,updated_at=$5::timestamptz WHERE scope_id=$1 AND period_start=$2::timestamptz").bind(scope).bind(b.get::<String,_>("start_text")).bind(cost.to_string()).bind(b.get::<String,_>("remaining_reservation_usd_text")).bind(now).execute(&mut *c).await?;
            // 未확인 token을 청구량으로 꾸미지 않고 남은 보수 예약을 원래 rate 창의 상한으로 전환한다.
            sqlx::query("UPDATE gw_rate_windows SET reserved_tokens=reserved_tokens-$4,settled_tokens=settled_tokens+$4,updated_at=$5::timestamptz WHERE scope_id=$1 AND lane=$2 AND window_start=$3::timestamptz").bind(scope).bind(b.get::<String,_>("rate_lane")).bind(b.get::<String,_>("rate_text")).bind(reserved).bind(now).execute(&mut *c).await?;
            sqlx::query("UPDATE gw_request_budgets SET state='settled',remaining_reservation_usd=gw_amount(0),remaining_reserved_tokens=0,applied_cost_usd=gw_amount($3::numeric),applied_input_tokens=NULL,applied_output_tokens=NULL WHERE request_id=$1 AND scope_id=$2").bind(id).bind(scope).bind(cost.to_string()).execute(&mut *c).await?;
        }
        let selected = json!({"source":"manual_entry","actual_model":null,"input_tokens":null,"output_tokens":null,"cache_read_tokens":null,"cache_write_tokens":null,"reasoning_tokens":null,"counted_input_tokens":null});
        let record = json!({"source":"manual_entry","decision":input.decision,"cost_usd":input.cost_usd,"reason":input.reason,"actor_principal_id":actor,"settled_at":now,"executor_id":r.get::<Uuid,_>("executor_id"),"execution_stop_source":stop_source});
        sqlx::query("INSERT INTO gw_usage_events(request_id,kind,provider_usage,normalized_usage,pricing_resolution,content_digest) VALUES($1,'settlement','{}',$2,$3,$4)").bind(id).bind(selected).bind(record).bind(digest).execute(&mut *c).await?;
        sqlx::query("UPDATE gw_requests SET accounting_state='settled',cost_usd=gw_amount($2::numeric),settlement_digest=$3,version=version+1 WHERE id=$1").bind(id).bind(cost.to_string()).bind(digest).execute(c).await?;
        Ok(())
    }
}
