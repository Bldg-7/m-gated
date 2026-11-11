use super::{
    super::{
        amount::Amount,
        management::{self, ManagementError},
    },
    types::*,
};
use serde_json::Value;
use uuid::Uuid;
fn invalid<T>() -> Result<T, ManagementError> {
    Err(ManagementError::Invalid)
}
pub(super) fn bytea(s: &str) -> bool {
    s.len() == 66
        && s.starts_with("\\x")
        && s[2..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn metadata(value: &Value) -> Result<(), ManagementError> {
    fn walk(v: &Value, name: &str) -> Result<(), ManagementError> {
        match v {
            Value::Object(o) => {
                for (k, v) in o {
                    walk(v, k)?;
                }
            }
            Value::Array(a) => {
                for v in a {
                    walk(v, name)?;
                }
            }
            Value::String(s) => {
                if s.len() > 4096 || s.chars().any(char::is_control) {
                    return invalid();
                }
                if name.ends_with("_usd") || name == "rate" || name == "usd_per_million_tokens" {
                    s.parse::<Amount>().map_err(|_| ManagementError::Invalid)?;
                }
                if (name == "version" || name.ends_with("_version"))
                    && name != "schema_version"
                    && s.bytes().all(|b| b.is_ascii_digit())
                {
                    management::version(s)?;
                }
                if name != "usd_per_million_tokens"
                    && (name.ends_with("_tokens") || name.ends_with("_count") || name == "tokens")
                {
                    let n = s.parse::<i64>().map_err(|_| ManagementError::Invalid)?;
                    if n < 0 || n.to_string() != *s {
                        return invalid();
                    }
                }
                if name.ends_with("_at")
                    || name.ends_with("_start")
                    || name.ends_with("_end")
                    || name.ends_with("_deadline")
                    || name == "cutoff"
                    || name == "detail_deleted_through"
                {
                    super::super::dashboard::timestamp(s)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    walk(value, "")
}
fn object_keys(v: &Value, keys: &[&str]) -> bool {
    v.as_object()
        .is_some_and(|o| o.len() == keys.len() && o.keys().all(|k| keys.contains(&k.as_str())))
}
fn event(e: &Event) -> Result<(), ManagementError> {
    let normalized = &e.normalized_usage;
    if !object_keys(
        normalized,
        &[
            "source",
            "actual_model",
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
            "reasoning_tokens",
            "counted_input_tokens",
        ],
    ) {
        return invalid();
    }
    if ![
        "provider",
        "gateway_no_dispatch",
        "profile_no_charge",
        "manual_entry",
    ]
    .contains(&normalized["source"].as_str().unwrap_or(""))
    {
        return invalid();
    }
    for (k, v) in normalized.as_object().unwrap() {
        if k.ends_with("_tokens") && !v.is_null() {
            let s = v.as_str().ok_or(ManagementError::Invalid)?;
            let n = s.parse::<i64>().map_err(|_| ManagementError::Invalid)?;
            if n < 0 || n.to_string() != s {
                return invalid();
            }
        }
    }
    if !normalized["actual_model"].is_null()
        && normalized["actual_model"]
            .as_str()
            .is_none_or(|s| s.is_empty() || s.len() > 200)
    {
        return invalid();
    }
    if normalized["source"] == "manual_entry"
        && normalized
            .as_object()
            .unwrap()
            .iter()
            .any(|(k, v)| (k.ends_with("_tokens") || k == "actual_model") && !v.is_null())
    {
        return invalid();
    }
    let allowed = [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
        "cache_creation",
        "ephemeral_5m_input_tokens",
        "ephemeral_1h_input_tokens",
        "prompt_tokens",
        "completion_tokens",
        "total_tokens",
        "prompt_cache_hit_tokens",
        "prompt_cache_miss_tokens",
        "input_tokens_details",
        "output_tokens_details",
        "prompt_tokens_details",
        "completion_tokens_details",
        "cached_tokens",
        "reasoning_tokens",
    ];
    fn numbers(v: &Value, allowed: &[&str], depth: usize) -> bool {
        depth <= 3
            && v.as_object().is_some_and(|o| {
                o.iter().all(|(k, v)| {
                    allowed.contains(&k.as_str())
                        && (v.as_u64().is_some_and(|n| n <= i64::MAX as u64)
                            || numbers(v, allowed, depth + 1))
                })
            })
    }
    if !numbers(&e.provider_usage, &allowed, 0) {
        return invalid();
    }
    let p = &e.pricing_resolution;
    if p.get("source").is_some() {
        if !object_keys(
            p,
            &[
                "source",
                "decision",
                "cost_usd",
                "reason",
                "actor_principal_id",
                "settled_at",
                "executor_id",
                "execution_stop_source",
            ],
        ) || p["source"] != "manual_entry"
            || !["charged", "no_charge"].contains(&p["decision"].as_str().unwrap_or(""))
            || !["runtime", "admin_confirmation"]
                .contains(&p["execution_stop_source"].as_str().unwrap_or(""))
        {
            return invalid();
        }
        for k in ["actor_principal_id", "executor_id"] {
            p[k].as_str()
                .and_then(|s| s.parse::<Uuid>().ok())
                .ok_or(ManagementError::Invalid)?;
        }
        management::text(
            p["reason"].as_str().ok_or(ManagementError::Invalid)?,
            1000,
            4096,
        )?;
        p["cost_usd"]
            .as_str()
            .ok_or(ManagementError::Invalid)?
            .parse::<Amount>()
            .map_err(|_| ManagementError::Invalid)?;
        super::super::dashboard::timestamp(
            p["settled_at"].as_str().ok_or(ManagementError::Invalid)?,
        )?;
    } else {
        if !object_keys(
            p,
            &[
                "pricing_version",
                "categories",
                "rounding_version",
                "cost_usd",
                "unresolved_reason",
            ],
        ) {
            return invalid();
        }
        management::text(
            p["pricing_version"]
                .as_str()
                .ok_or(ManagementError::Invalid)?,
            64,
            64,
        )?;
        if p["rounding_version"] != "sum_half_up_12_v1"
            || (!p["unresolved_reason"].is_null()
                && p["unresolved_reason"].as_str().is_none_or(|s| {
                    s.len() > 64 || !s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                }))
        {
            return invalid();
        }
        if !p["cost_usd"].is_null() {
            p["cost_usd"]
                .as_str()
                .ok_or(ManagementError::Invalid)?
                .parse::<Amount>()
                .map_err(|_| ManagementError::Invalid)?;
        }
        let categories = p["categories"].as_array().ok_or(ManagementError::Invalid)?;
        if categories.len() > 8
            || categories.iter().any(|c| {
                !object_keys(c, &["category", "tokens", "rate"])
                    || ![
                        "input",
                        "output",
                        "cache_read",
                        "cache_write",
                        "cache_write_5m",
                        "cache_write_1h",
                    ]
                    .contains(&c["category"].as_str().unwrap_or(""))
            })
        {
            return invalid();
        }
        for c in categories {
            let t = c["tokens"].as_str().ok_or(ManagementError::Invalid)?;
            let n = t.parse::<i64>().map_err(|_| ManagementError::Invalid)?;
            if n < 0 || n.to_string() != t {
                return invalid();
            }
            c["rate"]
                .as_str()
                .ok_or(ManagementError::Invalid)?
                .parse::<Amount>()
                .map_err(|_| ManagementError::Invalid)?;
        }
    }
    if !bytea(&e.content_digest) {
        return invalid();
    }
    Ok(())
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceValidation {
    Schema,
    Bounds,
    Graph,
    Snapshot,
    Timestamp,
    Metadata,
}
impl From<ManagementError> for SourceValidation {
    fn from(_: ManagementError) -> Self {
        Self::Schema
    }
}
impl From<super::super::finance::LedgerError> for SourceValidation {
    fn from(_: super::super::finance::LedgerError) -> Self {
        Self::Snapshot
    }
}
impl From<super::super::dashboard::QueryError> for SourceValidation {
    fn from(_: super::super::dashboard::QueryError) -> Self {
        Self::Timestamp
    }
}
impl Import {
    pub fn validate(&self) -> Result<(), SourceValidation> {
        management::version(&self.expected_version)?;
        for reference in [
            &self.source_ref,
            &self.previous_writer_fenced_ref,
            &self.previous_executors_stopped_ref,
        ] {
            management::text(reference, 500, 2048)?;
        }
        super::super::dashboard::timestamp(&self.cutoff)?;
        let b = &self.bundle;
        let cutoff = time::OffsetDateTime::parse(
            &self.cutoff,
            &time::format_description::well_known::Rfc3339,
        )
        .map_err(|_| SourceValidation::Timestamp)?;
        let before_cutoff = |s: &str| {
            time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                .is_ok_and(|t| t <= cutoff)
        };
        if b.requests.iter().any(|r| {
            !before_cutoff(&r.admitted_at)
                || !before_cutoff(&r.finished_at)
                || r.execution_stopped_at
                    .as_ref()
                    .is_some_and(|s| !before_cutoff(s))
        }) || b.events.iter().any(|e| !before_cutoff(&e.observed_at))
            || b.scopes
                .iter()
                .any(|s| !before_cutoff(&s.created_at) || !before_cutoff(&s.updated_at))
            || b.keys.iter().any(|k| {
                !before_cutoff(&k.created_at)
                    || k.revoked_at.as_ref().is_some_and(|s| !before_cutoff(s))
            })
            || b.safety
                .inference_paused_at
                .as_ref()
                .is_some_and(|s| !before_cutoff(s))
        {
            return Err(SourceValidation::Timestamp);
        }
        if b.keys.len() > 32
            || b.scopes.is_empty()
            || b.scopes.len() > 33
            || b.periods.len() > 100
            || b.windows.len() > 200
            || b.requests.len() > 100
            || b.budgets.len() > 200
            || b.events.len() > 400
            || serde_json::to_vec(self)
                .map_err(|_| ManagementError::Invalid)?
                .len()
                > 56 * 1024
        {
            return Err(SourceValidation::Bounds);
        }
        // 이전 입력이 선언한 객체만 서로 참조한다. FK가 존재하는 다른 주체의 행은 source의 일부가 아니다.
        let keys = b
            .keys
            .iter()
            .map(|k| (k.id, k))
            .collect::<std::collections::BTreeMap<_, _>>();
        let scopes = b
            .scopes
            .iter()
            .map(|s| (s.id, s))
            .collect::<std::collections::BTreeMap<_, _>>();
        let requests = b
            .requests
            .iter()
            .map(|r| (r.id, r))
            .collect::<std::collections::BTreeMap<_, _>>();
        if keys.len() != b.keys.len()
            || scopes.len() != b.scopes.len()
            || requests.len() != b.requests.len()
            || b.scopes.iter().filter(|s| s.key_id.is_none()).count() != 1
            || b.scopes
                .iter()
                .any(|s| s.key_id.is_some_and(|id| !keys.contains_key(&id)))
            || b.keys
                .iter()
                .any(|k| b.scopes.iter().filter(|s| s.key_id == Some(k.id)).count() != 1)
            || b.periods.iter().any(|t| !scopes.contains_key(&t.scope_id))
            || b.windows.iter().any(|w| !scopes.contains_key(&w.scope_id))
            || b.requests.iter().any(|r| {
                r.key_id.is_some_and(|k| !keys.contains_key(&k))
                    || r.parent_request_id
                        .is_some_and(|p| !requests.contains_key(&p))
            })
            || b.events
                .iter()
                .any(|e| !requests.contains_key(&e.request_id))
            || b.safety
                .inference_pause_request_id
                .is_some_and(|id| !requests.contains_key(&id))
        {
            return Err(SourceValidation::Graph);
        }
        for budget in &b.budgets {
            let request = requests
                .get(&budget.request_id)
                .ok_or(ManagementError::Invalid)?;
            let scope = scopes
                .get(&budget.scope_id)
                .ok_or(ManagementError::Invalid)?;
            if scope.key_id.is_some() && scope.key_id != request.key_id
                || budget.policy_snapshot.scope_id != budget.scope_id
                || budget.policy_snapshot.version != budget.policy_version
                || budget.state != request.accounting_state
                || budget.applied_cost_usd != request.cost_usd
            {
                return Err(SourceValidation::Graph);
            }
        }
        for request in &b.requests {
            let attribution = b
                .budgets
                .iter()
                .filter(|b| b.request_id == request.id)
                .collect::<Vec<_>>();
            if attribution.len() != if request.key_id.is_some() { 2 } else { 1 }
                || attribution
                    .iter()
                    .filter(|b| scopes[&b.scope_id].key_id.is_none())
                    .count()
                    != 1
                || request.pricing_snapshot.provider != request.provider
                || request.pricing_snapshot.api != request.api
                || request.pricing_snapshot.requested_model != request.requested_model
                || request.execution_snapshot["capability"] != request.api
                || (request.execution_snapshot["billing_class"] == "guaranteed_free"
                    && request.cost_usd.as_deref() != Some("0.000000000000"))
            {
                return Err(SourceValidation::Graph);
            }
        }
        if b.original_disabled != b.original_disabled_reason.is_some()
            || (b.original_security_fence_ref.is_some() && !b.original_disabled)
        {
            return Err(SourceValidation::Schema);
        }
        if let Some(reason) = &b.original_disabled_reason {
            management::text(reason, 128, 128)?;
        }
        if let Some(reference) = &b.original_security_fence_ref {
            management::text(reference, 500, 2048)?;
        }
        management::version(&b.source_principal_version)?;
        management::version(&b.safety.inference_safety_version)?;
        if !["human", "service"].contains(&b.identity.kind.as_str()) {
            return Err(SourceValidation::Schema);
        }
        for scope in &b.scopes {
            management::version(&scope.version)?;
            management::normalize_grants(scope.grants.clone())?;
            scope.limits.validate()?;
            if !["actual", "reserved"].contains(&scope.budget_mode.as_str())
                || scope.period_kind != "calendar_month"
                || scope.period_timezone.len() > 100
                || scope.period_timezone.parse::<chrono_tz::Tz>().is_err()
                || scope.limits.budget.as_ref().is_some_and(|budget| {
                    budget.mode != scope.budget_mode
                        || budget.period.kind != scope.period_kind
                        || budget.period.timezone != scope.period_timezone
                })
            {
                return Err(SourceValidation::Schema);
            }
        }
        for key in &b.keys {
            management::version(&key.version)?;
            management::text(&key.name, 100, 400)?;
            management::text(&key.public_prefix, 32, 32)?;
            management::normalize_grants(key.grant_snapshot.clone())?;
        }
        for r in &b.requests {
            management::version(&r.version)?;
            r.pricing_snapshot.validate()?;
            super::super::finance::read_execution_snapshot(r.execution_snapshot.clone())?;
            if !["settled", "unknown"].contains(&r.accounting_state.as_str())
                || !bytea(&r.auth_config_digest)
                || r.result_digest.as_ref().is_some_and(|d| !bytea(d))
                || r.settlement_digest.as_ref().is_some_and(|d| !bytea(d))
                || (r.execution_snapshot["billing_class"] == "guaranteed_free"
                    && r.accounting_state != "settled")
            {
                return Err(SourceValidation::Schema);
            }
        }
        for budget in &b.budgets {
            management::version(&budget.policy_version)?;
            management::version(&budget.policy_snapshot.version)?;
            management::normalize_grants(budget.policy_snapshot.grants.clone())?;
            budget.policy_snapshot.limits.validate()?;
        }
        for e in &b.events {
            event(e)?;
        }
        metadata(&serde_json::to_value(self).map_err(|_| ManagementError::Invalid)?)
            .map_err(|_| SourceValidation::Metadata)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pricing_rate_suffix_is_amount_not_a_token_count() {
        assert!(metadata(&serde_json::json!({"rates":[{"usd_per_million_tokens":"1.000000000000"}],"input_tokens":"1"})).is_ok());
        assert!(
            metadata(&serde_json::json!({"rates":[{"usd_per_million_tokens":"-0.000000000001"}]}))
                .is_err()
        );
        assert!(metadata(&serde_json::json!({"input_tokens":"1.000000000000"})).is_err());
    }
}
