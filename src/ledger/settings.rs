//! 관리자가 게이트웨이 전체에 한 번 정하는 설정. 예산 기간의 시간대와 끊긴 요청의 정산 규칙이다.
//!
//! - 예산(사용자 정책·가상 키)의 월 기간은 모두 이 시간대의 달력 한 달이다.
//! - 바꾸면 지금 기간은 이전 시간대로 끝까지 세고, 다음 기간부터 새 시간대를 쓴다. 바뀐 직후의 기간 하나만
//!   이전 기간이 끝난 시각부터 새 시간대의 다음 달 1일 0시까지로 센다([`budget_period_sql`]).
//! - 다음 기간이 시작되기 전에 다시 바꾸면 예약된 변경을 덮어쓴다. 지금 시간대와 같게 바꾸면 예약을 취소한다.
use super::{
    PgIdentityStore,
    commands::CommandContext,
    management::{self, CommandResult, ManagementError},
};
use serde_json::{Value, json};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

/// 시각 `t`가 속한 예산 기간과 그 기간의 시간대를 구하는 조회문 조각. `FROM` 뒤에 넣어 쓴다.
///
/// - 열: `period_start`, `period_end`, `timezone`. `t`는 `timestamptz` 값을 내는 SQL 식이다.
/// - 보통은 그 시간대의 달력 한 달이다.
/// - 시간대가 바뀐 직후의 기간 하나만, 이전 기간이 끝난 시각부터 새 시간대의 다음 달 1일 0시까지다.
/// - 다음 시간대 변경이 기간 끝보다 먼저 오면 기간은 그 변경 시각에 끝난다.
/// - 달의 경계는 Postgres 의 시간대 자료로 계산한다. 그래서 장부의 다른 시각 계산과 어긋나지 않는다.
pub fn budget_period_sql(t: &str) -> String {
    const MONTH: &str = "date_trunc('month',bp_a.t AT TIME ZONE bp_z.timezone)";
    const SHORT: &str = "bp_c.boundary IS NOT NULL AND bp_a.t<bp_c.boundary";
    format!(
        "(SELECT CASE WHEN {SHORT} THEN bp_z.since ELSE bp_c.month_start END AS period_start,\
         LEAST(CASE WHEN {SHORT} THEN bp_c.boundary ELSE bp_c.month_end END,bp_n.next_change) AS period_end,\
         bp_z.timezone \
         FROM (SELECT ({t})::timestamptz AS t) bp_a \
         CROSS JOIN LATERAL (SELECT effective_from AS since,timezone FROM gw_budget_timezones WHERE effective_from<=bp_a.t ORDER BY effective_from DESC LIMIT 1) bp_z \
         CROSS JOIN LATERAL (SELECT {MONTH} AT TIME ZONE bp_z.timezone AS month_start,\
         ({MONTH}+interval '1 month') AT TIME ZONE bp_z.timezone AS month_end,\
         CASE WHEN bp_z.since>'-infinity'::timestamptz THEN (date_trunc('month',(bp_z.since+interval '2 days') AT TIME ZONE bp_z.timezone)+interval '1 month') AT TIME ZONE bp_z.timezone END AS boundary) bp_c \
         CROSS JOIN LATERAL (SELECT min(effective_from) AS next_change FROM gw_budget_timezones WHERE effective_from>bp_a.t) bp_n)"
    )
}

/// 지금 적용 중인 예산 시간대와 기간, 예약된 변경. 모든 사용자가 볼 수 있는 값이다.
pub async fn budget_timezone_view(c: &mut PgConnection) -> Result<Value, sqlx::Error> {
    let now = sqlx::query(&format!(
        "SELECT b.timezone,{} AS period_start,{} AS period_end FROM {} b",
        super::dashboard::stamp_sql("b.period_start"),
        super::dashboard::stamp_sql("b.period_end"),
        budget_period_sql("clock_timestamp()")
    ))
    .fetch_one(&mut *c)
    .await?;
    let pending = sqlx::query(&format!(
        "SELECT z.timezone,{} AS effective_from,{} AS first_period_end FROM gw_budget_timezones z CROSS JOIN LATERAL {} b WHERE z.effective_from>clock_timestamp() ORDER BY z.effective_from LIMIT 1",
        super::dashboard::stamp_sql("z.effective_from"),
        super::dashboard::stamp_sql("b.period_end"),
        budget_period_sql("z.effective_from")
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

/// 끊긴 요청의 정산 규칙(결정 0005). 이번 단계에서는 Anthropic 규칙만 쓴다.
pub async fn cutoff_rule_view(c: &mut PgConnection) -> Result<Value, sqlx::Error> {
    let row = sqlx::query(
        "SELECT cutoff_rule,version FROM gw_settlement_rules WHERE provider='anthropic'",
    )
    .fetch_one(&mut *c)
    .await?;
    Ok(json!({
        "provider": "anthropic",
        "cutoff_rule": row.get::<String, _>("cutoff_rule"),
        "version": row.get::<i64, _>("version").to_string(),
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
            let cutoff = cutoff_rule_view(&mut tx).await?;
            tx.commit().await?;
            Ok(json!({ "budget_timezone": view, "cutoff_rule": cutoff }))
        })
        .await
        .map_err(|_| ManagementError::Database)?
    }

    /// 끊긴 요청의 정산 규칙을 바꾼다. 바꾼 뒤에 받는 요청부터 적용한다.
    pub async fn set_cutoff_rule(
        &self,
        ctx: &CommandContext<'_>,
        provider: &str,
        rule: &str,
        expected: i64,
    ) -> Result<CommandResult, ManagementError> {
        if provider != "anthropic" || !["received", "max_tokens"].contains(&rule) {
            return Err(ManagementError::Invalid);
        }
        let input =
            json!({"provider":provider,"cutoff_rule":rule,"expected_version":expected.to_string()});
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
            let row = sqlx::query(
                "SELECT cutoff_rule,version FROM gw_settlement_rules WHERE provider=$1 FOR UPDATE",
            )
            .bind(provider)
            .fetch_one(&mut *tx)
            .await?;
            let version: i64 = row.get("version");
            if version != expected {
                return Err(ManagementError::Version(version));
            }
            let before = cutoff_rule_view(&mut tx).await?;
            let changed = row.get::<String, _>("cutoff_rule") != rule;
            if changed {
                sqlx::query("UPDATE gw_settlement_rules SET cutoff_rule=$2,version=version+1,updated_by=$3,updated_at=clock_timestamp() WHERE provider=$1")
                    .bind(provider)
                    .bind(rule)
                    .bind(ctx.actor)
                    .execute(&mut *tx)
                    .await?;
            }
            let after = cutoff_rule_view(&mut tx).await?;
            let result_version = version + i64::from(changed);
            let mut ack = json!({"command_id":ctx.command_id,"operation":"set_settlement_rule","target":{"kind":"settings","id":Uuid::nil()},"result_version":result_version.to_string(),"effect":if changed{"changed"}else{"noop"},"cutoff_rule":after});
            sqlx::query("INSERT INTO gw_commands(actor_principal_id,command_id,input_digest,operation,target_kind,target_id,result_status,result_version,result_projection,created_at) VALUES($1,$2,$3,'set_settlement_rule','settings',$4,200,$5,$6,clock_timestamp())")
                .bind(ctx.actor)
                .bind(ctx.command_id)
                .bind(digest.as_slice())
                .bind(Uuid::nil())
                .bind(result_version)
                .bind(&ack)
                .execute(&mut *tx)
                .await?;
            if changed {
                sqlx::query("INSERT INTO gw_audit_events(actor_kind,actor_principal_id,command_id,actor_auth_config_digest,operation,target_kind,target_id,before_snapshot,after_snapshot) VALUES('human',$1,$2,$3,'set_settlement_rule','settings',$4,$5,$6)")
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
                sqlx::query_scalar(&format!("SELECT timezone FROM {} b", budget_period_sql("clock_timestamp()")))
                    .fetch_one(&mut *tx)
                    .await?;
            if current != timezone {
                sqlx::query(&format!("INSERT INTO gw_budget_timezones(effective_from,timezone,set_by) SELECT b.period_end,$1,$2 FROM {} b", budget_period_sql("clock_timestamp()")))
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
