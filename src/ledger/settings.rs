//! 관리자가 게이트웨이 전체에 한 번 정하는 설정. 지금은 예산 기간의 시간대 하나다.
//!
//! - 예산(사용자 정책·가상 키)의 월 기간은 모두 이 시간대의 달력 한 달이다.
//! - 바꾸면 지금 기간은 이전 시간대로 끝까지 세고, 다음 기간부터 새 시간대를 쓴다. 바뀐 직후의 기간 하나만
//!   이전 기간이 끝난 시각부터 새 시간대의 다음 달 1일 0시까지로 센다(`gw_budget_period`).
//! - 다음 기간이 시작되기 전에 다시 바꾸면 예약된 변경을 덮어쓴다. 지금 시간대와 같게 바꾸면 예약을 취소한다.
use super::{
    PgIdentityStore,
    commands::CommandContext,
    management::{self, CommandResult, ManagementError},
};
use serde_json::{Value, json};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

/// 지금 적용 중인 예산 시간대와 기간, 예약된 변경. 모든 사용자가 볼 수 있는 값이다.
pub async fn budget_timezone_view(c: &mut PgConnection) -> Result<Value, sqlx::Error> {
    let now = sqlx::query(&format!(
        "SELECT b.timezone,{} AS period_start,{} AS period_end FROM gw_budget_period(clock_timestamp()) b",
        super::dashboard::stamp_sql("b.period_start"),
        super::dashboard::stamp_sql("b.period_end")
    ))
    .fetch_one(&mut *c)
    .await?;
    let pending = sqlx::query(&format!(
        "SELECT z.timezone,{} AS effective_from,{} AS first_period_end FROM gw_budget_timezones z CROSS JOIN LATERAL gw_budget_period(z.effective_from) b WHERE z.effective_from>clock_timestamp() ORDER BY z.effective_from LIMIT 1",
        super::dashboard::stamp_sql("z.effective_from"),
        super::dashboard::stamp_sql("b.period_end")
    ))
    .fetch_optional(&mut *c)
    .await?;
    let version: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gw_commands WHERE operation='set_budget_timezone'",
    )
    .fetch_one(&mut *c)
    .await?;
    Ok(json!({
        "timezone": now.get::<String, _>("timezone"),
        "period_start": now.get::<String, _>("period_start"),
        "period_end": now.get::<String, _>("period_end"),
        "pending": pending.map(|p| json!({
            "timezone": p.get::<String, _>("timezone"),
            "effective_from": p.get::<String, _>("effective_from"),
            "first_period_end": p.get::<String, _>("first_period_end"),
        })),
        "version": version.to_string(),
    }))
}

impl PgIdentityStore {
    pub async fn settings_view(&self, ctx: &CommandContext<'_>) -> Result<Value, ManagementError> {
        let _gate = self
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| ManagementError::Database)?;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let mut tx = self.begin_transaction().await?;
            management::check_actor(&mut tx, ctx.actor, ctx.settings, true).await?;
            let view = budget_timezone_view(&mut tx).await?;
            tx.commit().await?;
            Ok(json!({ "budget_timezone": view }))
        })
        .await
        .map_err(|_| ManagementError::Database)?
    }

    /// 예산 시간대를 바꾼다. 다음 예산 기간부터 적용한다.
    pub async fn set_budget_timezone(
        &self,
        ctx: &CommandContext<'_>,
        timezone: &str,
        expected: i64,
    ) -> Result<CommandResult, ManagementError> {
        if timezone.len() > 100 || timezone.parse::<chrono_tz::Tz>().is_err() {
            return Err(ManagementError::Invalid);
        }
        let input = json!({"timezone":timezone,"expected_version":expected.to_string()});
        let digest = management::input_digest(ctx.method, ctx.path, &input);
        let _gate = self
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| ManagementError::Database)?;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let mut tx = self.begin_transaction().await?;
            management::check_actor(&mut tx, ctx.actor, ctx.settings, true).await?;
            if let Some(result) = super::credentials::replay_on(&mut tx, ctx, &digest).await? {
                return Ok(result);
            }
            // 변경끼리 한 줄로 세운다. 장부의 기간 계산(읽기)은 막지 않는다.
            sqlx::query("LOCK TABLE gw_budget_timezones IN SHARE ROW EXCLUSIVE MODE")
                .execute(&mut *tx)
                .await?;
            let before = budget_timezone_view(&mut tx).await?;
            let version: i64 = before["version"]
                .as_str()
                .and_then(|v| v.parse().ok())
                .ok_or(ManagementError::Invariant)?;
            if version != expected {
                return Err(ManagementError::Version(version));
            }
            let pending_before = before["pending"]["timezone"].as_str().map(str::to_owned);
            sqlx::query("DELETE FROM gw_budget_timezones WHERE effective_from>clock_timestamp()")
                .execute(&mut *tx)
                .await?;
            let current: String =
                sqlx::query_scalar("SELECT timezone FROM gw_budget_period(clock_timestamp())")
                    .fetch_one(&mut *tx)
                    .await?;
            if current != timezone {
                sqlx::query("INSERT INTO gw_budget_timezones(effective_from,timezone,set_by) SELECT b.period_end,$1,$2 FROM gw_budget_period(clock_timestamp()) b")
                    .bind(timezone)
                    .bind(ctx.actor)
                    .execute(&mut *tx)
                    .await?;
            }
            let pending_after = (current != timezone).then(|| timezone.to_owned());
            let changed = pending_before != pending_after;
            let mut after = budget_timezone_view(&mut tx).await?;
            // 이 명령도 버전에 들어간다.
            after["version"] = json!((version + 1).to_string());
            let mut ack = json!({"command_id":ctx.command_id,"operation":"set_budget_timezone","target":{"kind":"settings","id":Uuid::nil()},"result_version":(version + 1).to_string(),"effect":if changed{"changed"}else{"noop"},"budget_timezone":after});
            sqlx::query("INSERT INTO gw_commands(actor_principal_id,command_id,input_digest,operation,target_kind,target_id,result_status,result_version,result_projection,created_at) VALUES($1,$2,$3,'set_budget_timezone','settings',$4,200,$5,$6,clock_timestamp())")
                .bind(ctx.actor)
                .bind(ctx.command_id)
                .bind(digest.as_slice())
                .bind(Uuid::nil())
                .bind(version + 1)
                .bind(&ack)
                .execute(&mut *tx)
                .await?;
            if changed {
                sqlx::query("INSERT INTO gw_audit_events(actor_kind,actor_principal_id,command_id,actor_auth_config_digest,operation,target_kind,target_id,before_snapshot,after_snapshot) VALUES('human',$1,$2,$3,'set_budget_timezone','settings',$4,$5,$6)")
                    .bind(ctx.actor)
                    .bind(ctx.command_id)
                    .bind(ctx.digest.as_slice())
                    .bind(Uuid::nil())
                    .bind(&before)
                    .bind(&after)
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
            ack["replayed"] = json!(false);
            Ok(CommandResult {
                status: 200,
                body: Some(ack),
                replayed: false,
            })
        })
        .await
        .map_err(|_| ManagementError::Busy)?
    }
}
