use super::{amount::Amount, finance::LedgerError};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyLimits {
    pub budget: Option<BudgetLimit>,
    pub rpm: Option<i64>,
    pub tpm: Option<i64>,
    pub count_rpm: Option<i64>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BudgetLimit {
    pub mode: String,
    pub limit_usd: String,
    pub period: BudgetPeriod,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BudgetPeriod {
    pub kind: String,
    /// 0.9.0부터 예산 기간의 시간대는 관리자 설정(`gw_budget_timezones`) 하나로 정한다.
    /// API 입력에는 쓰지 않는다. 이전 버전이 남긴 정책 행과 장부 snapshot을 읽을 때만 값이 있다.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub timezone: String,
}
impl PolicyLimits {
    pub fn effective(&self, ceiling: &Self) -> Result<Self, LedgerError> {
        self.validate()?;
        ceiling.validate()?;
        let budget = match (&self.budget, &ceiling.budget) {
            (Some(a), Some(b)) => {
                if a.mode != b.mode || a.period.kind != b.period.kind {
                    return Err(LedgerError::ConfigurationConflict);
                }
                let mut result = a.clone();
                result.limit_usd = a
                    .limit_usd
                    .parse::<Amount>()
                    .map_err(|_| LedgerError::InvalidSnapshot)?
                    .min(
                        b.limit_usd
                            .parse()
                            .map_err(|_| LedgerError::InvalidSnapshot)?,
                    )
                    .to_string();
                Some(result)
            }
            (a, b) => a.clone().or_else(|| b.clone()),
        };
        fn smaller(a: Option<i64>, b: Option<i64>) -> Option<i64> {
            match (a, b) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            }
        }
        Ok(Self {
            budget,
            rpm: smaller(self.rpm, ceiling.rpm),
            tpm: smaller(self.tpm, ceiling.tpm),
            count_rpm: smaller(self.count_rpm, ceiling.count_rpm),
        })
    }
    /// API로 받은 한도에는 시간대를 둘 수 없다. 예산 기간의 시간대는 관리자 설정에서 정한다.
    pub fn without_timezone(&self) -> bool {
        self.budget
            .as_ref()
            .is_none_or(|b| b.period.timezone.is_empty())
    }
    pub fn validate(&self) -> Result<(), LedgerError> {
        if [self.rpm, self.tpm, self.count_rpm]
            .into_iter()
            .flatten()
            .any(|n| !(1..=1_000_000_000).contains(&n))
        {
            return Err(LedgerError::InvalidSnapshot);
        }
        if let Some(budget) = &self.budget
            && (!["actual", "reserved"].contains(&budget.mode.as_str())
                || budget.period.kind != "calendar_month"
                || budget.limit_usd.parse::<Amount>().is_err()
                || budget.period.timezone.len() > 100
                || (!budget.period.timezone.is_empty()
                    && budget.period.timezone.parse::<chrono_tz::Tz>().is_err()))
        {
            return Err(LedgerError::InvalidSnapshot);
        }
        Ok(())
    }
}
/// 호출자는 principal→key→policy 순서의 업무 잠금을 먼저 보유한다.
/// S5 정책 명령은 반환된 정규값과 command/audit를 같은 transaction에 저장한다.
pub async fn validate_policy_transition(
    connection: &mut sqlx::PgConnection,
    scope: uuid::Uuid,
    proposed: &PolicyLimits,
    ceiling: &PolicyLimits,
) -> Result<PolicyLimits, LedgerError> {
    use sqlx::Row;
    let normalized = proposed.effective(ceiling)?;
    let policy = sqlx::query("SELECT key_id,budget_mode FROM gw_policies WHERE id=$1")
        .bind(scope)
        .fetch_one(&mut *connection)
        .await?;
    if policy.get::<Option<uuid::Uuid>, _>("key_id").is_some() {
        return Err(LedgerError::Conflict);
    }
    // 예산 기간의 시간대는 관리자 설정 하나로 정하므로 정책마다 바뀔 일이 없다. 모드만 확인한다.
    if let Some(budget) = &normalized.budget
        && budget.mode != policy.get::<String, _>("budget_mode")
    {
        let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM gw_request_budgets WHERE scope_id=$1 AND (state<>'settled' OR remaining_reservation_usd<>0 OR remaining_reserved_tokens<>0)) OR EXISTS(SELECT 1 FROM gw_usage_totals WHERE scope_id=$1 AND (reserved_usd<>0 OR unknown_count<>0))").bind(scope).fetch_one(&mut *connection).await?;
        if active {
            return Err(LedgerError::Conflict);
        }
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn effective_limits_min_does_not_expand_stored_scope() {
        let stored = PolicyLimits {
            rpm: Some(8),
            ..Default::default()
        };
        let ceiling = PolicyLimits {
            rpm: Some(3),
            tpm: Some(10),
            ..Default::default()
        };
        assert_eq!(stored.effective(&ceiling).unwrap().rpm, Some(3));
        assert_eq!(stored.effective(&ceiling).unwrap().tpm, Some(10));
        assert_eq!(stored.effective(&PolicyLimits::default()).unwrap(), stored);
    }
}
