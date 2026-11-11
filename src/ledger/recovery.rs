use super::{PgLedger, finance::LedgerError};
use sqlx::Row;
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

impl PgLedger {
    /// 고정 두 행의 절대누계만 재관측한다. ACK 유실/replay/재시작에 add를 반복하지 않는다.
    pub(crate) async fn observe_durable_alarms(&self) -> Result<(), LedgerError> {
        use crate::telemetry::{self, Metric};
        let rows = sqlx::query("SELECT code,total FROM gw_alarm_totals ORDER BY code LIMIT 2")
            .fetch_all(self.database().pool())
            .await?;
        for row in rows {
            let total = row.get::<i64, _>("total") as u64;
            match row.get::<String, _>("code").as_str() {
                "unknown" => telemetry::max(Metric::AlarmUnknown, total),
                "bound" => {
                    telemetry::max(Metric::AlarmBound, total);
                    telemetry::max(Metric::BoundViolation, total);
                }
                _ => return Err(LedgerError::InvariantViolation),
            }
        }
        Ok(())
    }
    /// 한 tick의 후보는 비잠금 조회로 고정한다. NOWAIT skip 후보를 같은 tick에 재시도하지 않는다.
    pub async fn recovery_tick(
        &self,
        runtime: &crate::runtime::Runtime,
    ) -> Result<usize, LedgerError> {
        let _gate = self
            .database()
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| LedgerError::DatabaseUnavailable)?;
        let candidates: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM gw_requests WHERE (phase='admitted' AND admit_expires_at<=clock_timestamp()) OR (phase='dispatching' AND lease_expires_at<=clock_timestamp()) ORDER BY CASE WHEN phase='admitted' THEN admit_expires_at ELSE lease_expires_at END,id LIMIT 100")
            .fetch_all(self.database().pool()).await?;
        let mut recovered = 0;
        for id in candidates {
            if self.recover_expired(id).await?.is_some() {
                recovered += 1;
            }
        }
        let failed = runtime.failed_request_ids();
        if !failed.is_empty() {
            // request.finished만으로는 부족하다. 귀속과 전 기간 totals의 전역 차단도 함께 확인한다.
            let confirmed: Vec<Uuid> = sqlx::query("SELECT r.id FROM gw_requests r WHERE r.id=ANY($1) AND r.phase='finished' AND EXISTS(SELECT 1 FROM gw_request_budgets b WHERE b.request_id=r.id) AND NOT EXISTS(SELECT 1 FROM gw_request_budgets b WHERE b.request_id=r.id AND b.state<>r.accounting_state) AND NOT EXISTS(SELECT 1 FROM gw_usage_totals t JOIN gw_policies p ON p.id=t.scope_id WHERE p.principal_id=r.principal_id AND t.unknown_count<>(SELECT count(*) FROM gw_request_budgets b WHERE b.scope_id=t.scope_id AND b.period_start=t.period_start AND b.state='unknown') OR (p.principal_id=r.principal_id AND t.reserved_usd<>(SELECT coalesce(sum(b.remaining_reservation_usd),0) FROM gw_request_budgets b WHERE b.scope_id=t.scope_id AND b.period_start=t.period_start)))")
                .bind(&failed).fetch_all(self.database().pool()).await?.iter().map(|r|r.get("id")).collect();
            runtime.confirm_durable_finalizations(&confirmed);
        }
        let unresolved=sqlx::query("SELECT count(*)::bigint AS requests,count(DISTINCT principal_id)::bigint AS principals,coalesce(max(greatest(0,extract(epoch FROM clock_timestamp()-admitted_at)))::bigint,0) AS age FROM gw_requests WHERE accounting_state='unknown'").fetch_one(self.database().pool()).await?;
        use crate::telemetry::{self, Metric};
        telemetry::set(Metric::Unknown, unresolved.get::<i64, _>("requests") as u64);
        telemetry::set(
            Metric::BlockedPrincipals,
            unresolved.get::<i64, _>("principals") as u64,
        );
        telemetry::set(
            Metric::OldestUnknown,
            unresolved.get::<i64, _>("age") as u64,
        );
        telemetry::add(Metric::Recovered, recovered as u64);
        self.observe_durable_alarms().await?;
        Ok(recovered)
    }
    /// 별도 task를 tick마다 만들지 않는다. drain은 진행 중 SQL future도 취소한다.
    pub async fn run_reaper(
        self: Arc<Self>,
        runtime: Arc<crate::runtime::Runtime>,
    ) -> Result<(), crate::runtime::AdmissionError> {
        let _guard = runtime
            .reaper_guard()
            .ok_or(crate::runtime::AdmissionError::InvalidTransition)?;
        let mut stop = runtime.shutdown_signal();
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if *stop.borrow() {
                return Ok(());
            }
            tokio::select! { biased; _=stop.changed()=>return Ok(()), _=tick.tick()=>{} }
            tokio::select! {
                biased;
                _=stop.changed()=>return Ok(()),
                result=tokio::time::timeout(Duration::from_secs(5),self.recovery_tick(&runtime))=>{if !matches!(result,Ok(Ok(_))){crate::telemetry::add(crate::telemetry::Metric::ReaperFailed,1);}},
            }
        }
    }
}
