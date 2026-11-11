use super::PgIdentityStore;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::{PgConnection, Row};
use std::{collections::BTreeMap, time::Duration};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

#[derive(Debug)]
pub enum QueryError {
    InvalidFilter,
    InvalidCursor,
    NotFound,
    AccessDenied,
    Database,
    Invariant,
    DetailUnavailable(String),
}
impl From<sqlx::Error> for QueryError {
    fn from(_: sqlx::Error) -> Self {
        Self::Database
    }
}
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UsageFilter {
    pub principal_id: Option<Uuid>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub key_id: Option<Uuid>,
    pub group_by: Option<String>,
    pub limit: Option<u32>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BucketCursor {
    v: u8,
    kind: String,
    resource: String,
    principal: Uuid,
    scope: String,
    filter: UsageFilter,
    as_of: String,
    expires: i64,
    last: Vec<String>,
}
pub struct UsageQuery {
    pub(crate) filter: UsageFilter,
    pub(crate) cursor: Option<String>,
}
impl UsageQuery {
    pub fn parse(raw: Option<&str>) -> Result<Self, QueryError> {
        let mut fields = BTreeMap::new();
        if raw.is_some_and(|s| s.len() > 4096) {
            return Err(QueryError::InvalidFilter);
        }
        for pair in raw.unwrap_or("").split('&').filter(|s| !s.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            if fields
                .insert(decode_query(key)?, decode_query(value)?)
                .is_some()
            {
                return Err(QueryError::InvalidFilter);
            }
        }
        if fields.keys().any(|k| {
            ![
                "from", "to", "provider", "model", "key_id", "group_by", "limit", "cursor",
            ]
            .contains(&k.as_str())
        }) {
            return Err(QueryError::InvalidFilter);
        }
        let from = fields.remove("from").map(|s| timestamp(&s)).transpose()?;
        let to = fields.remove("to").map(|s| timestamp(&s)).transpose()?;
        if from.is_some() != to.is_some() {
            return Err(QueryError::InvalidFilter);
        }
        if let (Some(a), Some(b)) = (&from, &to) {
            validate_period(a, b)?;
        }
        let provider = fields.remove("provider");
        if provider
            .as_ref()
            .is_some_and(|s| !["anthropic", "openai", "deepseek"].contains(&s.as_str()))
        {
            return Err(QueryError::InvalidFilter);
        }
        let model = fields.remove("model");
        if model
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > 200 || s.chars().any(char::is_control))
        {
            return Err(QueryError::InvalidFilter);
        }
        let key_id = fields
            .remove("key_id")
            .map(|s| Uuid::parse_str(&s).map_err(|_| QueryError::InvalidFilter))
            .transpose()?;
        let group_by = fields.remove("group_by");
        if group_by
            .as_ref()
            .is_some_and(|s| !["provider", "model", "key"].contains(&s.as_str()))
        {
            return Err(QueryError::InvalidFilter);
        }
        let limit = fields
            .remove("limit")
            .map(|s| s.parse::<u32>().map_err(|_| QueryError::InvalidFilter))
            .transpose()?;
        if limit.is_some_and(|n| n == 0 || n > 100) {
            return Err(QueryError::InvalidFilter);
        }
        let cursor = fields.remove("cursor");
        if cursor
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > 2048)
        {
            return Err(QueryError::InvalidCursor);
        }
        if group_by.is_none() && cursor.is_none() && limit.is_some() {
            return Err(QueryError::InvalidFilter);
        }
        Ok(Self {
            filter: UsageFilter {
                principal_id: None,
                from,
                to,
                provider,
                model,
                key_id,
                group_by,
                limit,
            },
            cursor,
        })
    }
}
pub(crate) fn decode_query(input: &str) -> Result<String, QueryError> {
    let mut bytes = Vec::with_capacity(input.len());
    let mut source = input.bytes();
    while let Some(byte) = source.next() {
        bytes.push(match byte {
            b'+' => b' ',
            b'%' => {
                let high = source
                    .next()
                    .and_then(|b| (b as char).to_digit(16))
                    .ok_or(QueryError::InvalidFilter)?;
                let low = source
                    .next()
                    .and_then(|b| (b as char).to_digit(16))
                    .ok_or(QueryError::InvalidFilter)?;
                (high * 16 + low) as u8
            }
            other => other,
        });
    }
    String::from_utf8(bytes).map_err(|_| QueryError::InvalidFilter)
}
pub(crate) fn timestamp(s: &str) -> Result<String, QueryError> {
    OffsetDateTime::parse(s, &Rfc3339)
        .map_err(|_| QueryError::InvalidFilter)?
        .to_offset(time::UtcOffset::UTC)
        .format(&Rfc3339)
        .map_err(|_| QueryError::InvalidFilter)
}
pub(crate) fn validate_period(a: &str, b: &str) -> Result<(), QueryError> {
    let a = OffsetDateTime::parse(a, &Rfc3339).map_err(|_| QueryError::InvalidFilter)?;
    let b = OffsetDateTime::parse(b, &Rfc3339).map_err(|_| QueryError::InvalidFilter)?;
    if a >= b || b - a > time::Duration::days(90) {
        return Err(QueryError::InvalidFilter);
    }
    Ok(())
}
fn mac(key: &[u8], payload: &[u8]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC key");
    mac.update(b"m-gated-cursor-v1\0");
    mac.update(payload);
    mac
}
fn encode_cursor(cursor: &BucketCursor, key: &[u8]) -> Result<String, QueryError> {
    let bytes = serde_json::to_vec(cursor).map_err(|_| QueryError::Invariant)?;
    let tag = mac(key, &bytes).finalize().into_bytes();
    let s = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(bytes),
        URL_SAFE_NO_PAD.encode(tag)
    );
    if s.len() > 2048 {
        return Err(QueryError::Invariant);
    }
    Ok(s)
}
fn decode_cursor(
    raw: &str,
    key: &[u8],
    principal: Uuid,
    now: i64,
    scope: &str,
) -> Result<BucketCursor, QueryError> {
    let invalid = || QueryError::InvalidCursor;
    let (a, b) = raw.split_once('.').ok_or_else(invalid)?;
    let bytes = URL_SAFE_NO_PAD.decode(a).map_err(|_| invalid())?;
    let tag = URL_SAFE_NO_PAD.decode(b).map_err(|_| invalid())?;
    mac(key, &bytes).verify_slice(&tag).map_err(|_| invalid())?;
    let c: BucketCursor = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if c.v != 1
        || c.kind != "usage_bucket"
        || c.resource != "usage"
        || c.scope != scope
        || c.principal != principal
        || c.expires <= now
        || c.expires > now + 900
    {
        return Err(invalid());
    }
    Ok(c)
}
fn agrees(a: &UsageFilter, b: &UsageFilter) -> bool {
    macro_rules! same {
        ($n:ident) => {
            a.$n.as_ref().is_none_or(|v| Some(v) == b.$n.as_ref())
        };
    }
    same!(principal_id)
        && same!(from)
        && same!(to)
        && same!(provider)
        && same!(model)
        && same!(key_id)
        && same!(group_by)
        && same!(limit)
}
fn validate_amounts(value: &Value) -> Result<(), QueryError> {
    match value {
        Value::Array(items) => {
            for item in items {
                validate_amounts(item)?;
            }
        }
        Value::Object(fields) => {
            for (key, value) in fields {
                if [
                    "settled_usd",
                    "held_reservation_usd",
                    "reserved_usd",
                    "limit_usd",
                    "remaining_usd",
                ]
                .contains(&key.as_str())
                    && !value.is_null()
                {
                    value
                        .as_str()
                        .ok_or(QueryError::Invariant)?
                        .parse::<super::amount::Amount>()
                        .map_err(|_| QueryError::Invariant)?;
                } else {
                    validate_amounts(value)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}
pub(crate) fn stamp_sql(expression: &str) -> String {
    format!("to_char(({expression}) AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')")
}
impl PgIdentityStore {
    pub async fn dashboard_me(
        &self,
        id: Uuid,
        grants: &Value,
        digest: &str,
    ) -> Result<Value, QueryError> {
        let _gate = self
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| QueryError::Database)?;
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut tx = self.begin_transaction().await?;
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *tx)
                .await?;
            let (p, _, _) = principal(&mut tx, id, grants, digest).await?;
            tx.commit().await?;
            Ok(p)
        })
        .await
        .map_err(|_| QueryError::Database)?
    }
    pub async fn dashboard_usage(
        &self,
        id: Uuid,
        query: UsageQuery,
        grants: &Value,
        digest: &str,
        signing_key: &[u8],
    ) -> Result<Value, QueryError> {
        self.dashboard_usage_with_ceiling(
            id,
            query,
            grants,
            digest,
            signing_key,
            &Default::default(),
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn dashboard_usage_with_ceiling(
        &self,
        id: Uuid,
        query: UsageQuery,
        grants: &Value,
        digest: &str,
        signing_key: &[u8],
        ceiling: &super::policy::PolicyLimits,
    ) -> Result<Value, QueryError> {
        let _gate = self
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| QueryError::Database)?;
        tokio::time::timeout(
            Duration::from_secs(3),
            self.usage_snapshot(id, query, grants, digest, signing_key, ceiling, None),
        )
        .await
        .map_err(|_| QueryError::Database)?
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn usage_snapshot(
        &self,
        id: Uuid,
        query: UsageQuery,
        grants: &Value,
        digest: &str,
        signing_key: &[u8],
        ceiling: &super::policy::PolicyLimits,
        admin_context: Option<&super::management_query::QueryContext<'_>>,
    ) -> Result<Value, QueryError> {
        let mut tx = self.begin_transaction().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        if let Some(ctx) = admin_context {
            super::management::check_actor(&mut tx, ctx.actor, ctx.settings, true)
                .await
                .map_err(|e| match e {
                    super::management::ManagementError::Forbidden => QueryError::AccessDenied,
                    _ => QueryError::Database,
                })?;
        }
        let cursor_scope = if admin_context.is_some() {
            "admin"
        } else {
            "self"
        };
        let now: String = sqlx::query_scalar(&format!("SELECT {}", stamp_sql("clock_timestamp()")))
            .fetch_one(&mut *tx)
            .await?;
        let now_unix = OffsetDateTime::parse(&now, &Rfc3339)
            .map_err(|_| QueryError::Invariant)?
            .unix_timestamp();
        let (mut filter, as_of, last, expires) = if let Some(raw) = query.cursor {
            let c = decode_cursor(&raw, signing_key, id, now_unix, cursor_scope)?;
            if !agrees(&query.filter, &c.filter) {
                return Err(QueryError::InvalidCursor);
            }
            (c.filter, c.as_of, c.last, c.expires)
        } else {
            (query.filter, now.clone(), Vec::new(), now_unix + 900)
        };
        let actor = id;
        let id = if admin_context.is_some() {
            filter.principal_id
        } else {
            Some(id)
        };
        if admin_context.is_some()
            && let Some(id) = id
        {
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM gw_principals WHERE id=$1)")
                    .bind(id)
                    .fetch_one(&mut *tx)
                    .await?;
            if !exists {
                return Err(QueryError::NotFound);
            }
        }
        let (mut p, mut balances, _) = if let Some(id) = id {
            principal(&mut tx, id, grants, digest).await?
        } else {
            let counts=sqlx::query("SELECT count(*) FILTER(WHERE accounting_state='unknown')::text AS unknown,count(*) FILTER(WHERE phase='dispatching' AND lease_expires_at<=$1::timestamptz AND execution_snapshot->>'billing_class'='chargeable')::text AS expired FROM gw_requests").bind(&now).fetch_one(&mut *tx).await?;
            (
                json!({"block_state":{"global_unknown_count":counts.get::<String,_>("unknown"),"expired_dispatch_count":counts.get::<String,_>("expired")}}),
                Vec::new(),
                now.clone(),
            )
        };
        if let (Some(ctx), Some(id)) = (admin_context, id) {
            p = super::management_query::principal(&mut tx, id, grants, digest, ctx.settings)
                .await
                .map_err(|e| match e {
                    super::management::ManagementError::NotFound => QueryError::NotFound,
                    _ => QueryError::Database,
                })?;
            for b in &mut balances {
                if let Some(reasons) = p["block_state"]["reasons"].as_array() {
                    let existing = b["reasons"].as_array_mut().ok_or(QueryError::Invariant)?;
                    for reason in reasons {
                        if !existing.contains(reason) {
                            existing.push(reason.clone());
                        }
                    }
                    b["blocked"] = json!(!existing.is_empty());
                }
            }
        }
        if filter.from.is_none() {
            let row = sqlx::query(&format!(
                "SELECT {} AS start",
                stamp_sql(
                    "date_trunc('month',$1::timestamptz AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'"
                )
            ))
            .bind(&as_of)
            .fetch_one(&mut *tx)
            .await?;
            filter.from = Some(timestamp(row.get("start")).map_err(|_| QueryError::Invariant)?);
            filter.to = Some(timestamp(&as_of).map_err(|_| QueryError::Invariant)?);
        }
        validate_period(
            filter.from.as_deref().ok_or(QueryError::Invariant)?,
            filter.to.as_deref().ok_or(QueryError::Invariant)?,
        )?;
        if filter.group_by.is_none() && filter.limit.is_some() {
            return Err(QueryError::InvalidFilter);
        }
        if filter.group_by.is_some() && filter.limit.is_none() {
            filter.limit = Some(50);
        }
        if let Some(key) = filter.key_id {
            if let Some(id) = id {
                balances.push(key_balance(&mut tx, id, key, &p, &now, digest).await?);
            } else {
                let exists: bool =
                    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM gw_virtual_keys WHERE id=$1)")
                        .bind(key)
                        .fetch_one(&mut *tx)
                        .await?;
                if !exists {
                    return Err(QueryError::NotFound);
                }
            }
        }
        for balance in &mut balances {
            apply_balance_ceiling(&mut tx, balance, ceiling).await?;
        }
        let boundary:Option<String>=sqlx::query_scalar(&format!("SELECT {} FROM gw_usage_totals t JOIN gw_policies p ON p.id=t.scope_id WHERE ($1::uuid IS NULL OR p.principal_id=$1) AND p.key_id IS NULL",stamp_sql("max(t.detail_deleted_through)+interval '1 microsecond'"))).bind(id).fetch_one(&mut *tx).await?;
        let from = filter.from.as_ref().ok_or(QueryError::Invariant)?;
        let to = filter.to.as_ref().ok_or(QueryError::Invariant)?;
        let before = if let Some(cutoff) = &boundary {
            sqlx::query_scalar::<_, bool>("SELECT $1::timestamptz<$2::timestamptz")
                .bind(from)
                .bind(cutoff)
                .fetch_one(&mut *tx)
                .await?
        } else {
            false
        };
        let scope = balances.first().and_then(|b| b["scope_id"].as_str());
        let mut buckets = Vec::new();
        let mut next_cursor = None;
        let (aggregation, totals) = if before {
            if id.is_none() {
                return Err(QueryError::DetailUnavailable(
                    boundary.clone().ok_or(QueryError::Invariant)?,
                ));
            }
            let timezone = balances[0]["timezone"]
                .as_str()
                .ok_or(QueryError::Invariant)?;
            let complete:bool=sqlx::query_scalar("SELECT ($1::timestamptz AT TIME ZONE $3)=date_trunc('month',$1::timestamptz AT TIME ZONE $3) AND ($2::timestamptz AT TIME ZONE $3)=date_trunc('month',$2::timestamptz AT TIME ZONE $3)").bind(from).bind(to).bind(timezone).fetch_one(&mut *tx).await?;
            if !complete
                || filter.provider.is_some()
                || filter.model.is_some()
                || filter.key_id.is_some()
                || filter.group_by.is_some()
                || filter.limit.is_some()
            {
                return Err(QueryError::DetailUnavailable(
                    boundary.clone().ok_or(QueryError::Invariant)?,
                ));
            }
            let totals=sqlx::query("SELECT COALESCE(sum(cost_usd),0)::numeric(38,12)::text AS cost,COALESCE(sum(reserved_usd),0)::numeric(38,12)::text AS held,COALESCE(sum(unknown_count),0)::text AS unknown FROM gw_usage_totals WHERE scope_id=$1::uuid AND period_start >= $2::timestamptz AND period_end <= $3::timestamptz").bind(scope).bind(from).bind(to).fetch_one(&mut *tx).await?;
            ("period_totals", totals)
        } else {
            let base = "FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id JOIN gw_policies p ON p.id=b.scope_id AND p.key_id IS NULL WHERE ($1::uuid IS NULL OR r.principal_id=$1) AND r.admitted_at >= $2::timestamptz AND r.admitted_at < $3::timestamptz AND r.admitted_at <= $4::timestamptz AND ($5::text IS NULL OR r.provider=$5) AND ($6::text IS NULL OR r.requested_model=$6) AND ($7::uuid IS NULL OR r.key_id=$7)";
            let sums = "COALESCE(sum(b.applied_cost_usd),0)::numeric(38,12)::text AS cost,COALESCE(sum(b.remaining_reservation_usd),0)::numeric(38,12)::text AS held,count(*) FILTER(WHERE r.accounting_state='unknown')::text AS unknown";
            let totals = sqlx::query(&format!("SELECT {sums} {base}"))
                .bind(id)
                .bind(from)
                .bind(to)
                .bind(&as_of)
                .bind(&filter.provider)
                .bind(&filter.model)
                .bind(filter.key_id)
                .fetch_one(&mut *tx)
                .await?;
            if let Some(group) = &filter.group_by {
                let (g1, g2) = match group.as_str() {
                    "provider" => ("r.provider", "''::text"),
                    "model" => ("r.provider", "r.requested_model"),
                    "key" => (
                        "CASE WHEN r.key_id IS NULL THEN '0' ELSE '1' END",
                        "COALESCE(replace(r.key_id::text,'-',''),'')",
                    ),
                    _ => return Err(QueryError::InvalidCursor),
                };
                let sql = format!(
                    "SELECT {g1} AS g1,{g2} AS g2,{sums},CASE WHEN count(*) FILTER(WHERE b.state='settled' AND (b.applied_input_tokens IS NULL OR b.applied_output_tokens IS NULL))=0 THEN COALESCE(sum(b.applied_input_tokens),0)::text END AS input, CASE WHEN count(*) FILTER(WHERE b.state='settled' AND (b.applied_input_tokens IS NULL OR b.applied_output_tokens IS NULL))=0 THEN COALESCE(sum(b.applied_output_tokens),0)::text END AS output {base} GROUP BY 1,2 HAVING ($8::text IS NULL OR ({g1} COLLATE \"C\",{g2} COLLATE \"C\")>($8 COLLATE \"C\",$9 COLLATE \"C\")) ORDER BY 1 COLLATE \"C\",2 COLLATE \"C\" LIMIT $10"
                );
                // ORDER BY expressions (not ordinal COLLATE) avoid locale-dependent bucket order.
                let sql = sql.replace(
                    "ORDER BY 1 COLLATE \"C\",2 COLLATE \"C\"",
                    &format!("ORDER BY ({g1}) COLLATE \"C\",({g2}) COLLATE \"C\""),
                );
                let mut rows = sqlx::query(&sql)
                    .bind(id)
                    .bind(from)
                    .bind(to)
                    .bind(&as_of)
                    .bind(&filter.provider)
                    .bind(&filter.model)
                    .bind(filter.key_id)
                    .bind(last.first())
                    .bind(last.get(1))
                    .bind(i64::from(filter.limit.unwrap_or(50)) + 1)
                    .fetch(&mut *tx);
                use futures_util::TryStreamExt;
                let mut previous = Vec::new();
                while let Some(row) = rows.try_next().await? {
                    if buckets.len() == filter.limit.unwrap_or(50) as usize {
                        next_cursor = Some(encode_cursor(
                            &BucketCursor {
                                v: 1,
                                kind: "usage_bucket".into(),
                                resource: "usage".into(),
                                principal: actor,
                                scope: cursor_scope.into(),
                                filter: filter.clone(),
                                as_of: as_of.clone(),
                                expires,
                                last: previous,
                            },
                            signing_key,
                        )?);
                        break;
                    }
                    let a: String = row.get("g1");
                    let b: String = row.get("g2");
                    let group = match group.as_str() {
                        "provider" => json!({"kind":"provider","provider":a}),
                        "model" => json!({"kind":"model","provider":a,"model":b}),
                        _ => {
                            json!({"kind":"key","key_id":if a=="0" {None} else { Some(Uuid::parse_str(&b).map_err(|_|QueryError::Invariant)?) }})
                        }
                    };
                    let input: Option<String> = row.get("input");
                    let output: Option<String> = row.get("output");
                    buckets.push(json!({"group":group,"settled_usd":row.get::<String,_>("cost"),"held_reservation_usd":row.get::<String,_>("held"),"unknown_count":row.get::<String,_>("unknown"),"tokens_complete":input.is_some()&&output.is_some(),"input_tokens":input,"output_tokens":output}));
                    previous = vec![a, b];
                }
            }
            ("detail", totals)
        };
        let value = json!({"currency":"USD","from":from,"to":to,"as_of":now,"detail_available_from":boundary,"aggregation":aggregation,"settled_usd":totals.get::<String,_>("cost"),"held_reservation_usd":totals.get::<String,_>("held"),"period_unknown_count":totals.get::<String,_>("unknown"),"global_unknown_count":p["block_state"]["global_unknown_count"],"global_expired_dispatch_count":p["block_state"]["expired_dispatch_count"],"block_state":if id.is_none(){Value::Null}else{p["block_state"].clone()},"balance_context":if id.is_none(){"aggregate_no_balance"}else if filter.key_id.is_some() {"current_principal_and_key"}else{"current_principal"},"budget_balances":balances,"buckets":{"items":buckets,"next_cursor":next_cursor,"as_of":as_of}});
        validate_amounts(&value)?;
        if serde_json::to_vec(&value)
            .map_err(|_| QueryError::Invariant)?
            .len()
            > 1024 * 1024
        {
            return Err(QueryError::Invariant);
        }
        tx.commit().await?;
        Ok(value)
    }
}
pub(crate) async fn principal(
    c: &mut PgConnection,
    id: Uuid,
    configured_grants: &Value,
    digest: &str,
) -> Result<(Value, Vec<Value>, String), QueryError> {
    let row=sqlx::query("SELECT p.*,q.id AS scope,q.version AS pv,q.allow_rules,q.budget_mode,q.budget_limit_usd::text AS budget,q.period_timezone,q.rpm_limit,q.tpm_limit,q.count_rpm_limit FROM gw_principals p JOIN gw_policies q ON q.principal_id=p.id AND q.key_id IS NULL WHERE p.id=$1").bind(id).fetch_optional(&mut *c).await?.ok_or(QueryError::AccessDenied)?;
    let now: String = sqlx::query_scalar(&format!("SELECT {}", stamp_sql("clock_timestamp()")))
        .fetch_one(&mut *c)
        .await?;
    let counts=sqlx::query("SELECT count(*) FILTER(WHERE accounting_state='unknown')::text AS unknown,count(*) FILTER(WHERE phase='dispatching' AND lease_expires_at<=$2::timestamptz AND execution_snapshot->>'billing_class'='chargeable')::text AS expired FROM gw_requests WHERE principal_id=$1").bind(id).bind(&now).fetch_one(&mut *c).await?;
    let unknown: String = counts.get("unknown");
    let expired: String = counts.get("expired");
    let paused = row.get::<Option<String>, _>("inference_pause_reason");
    let mut reasons = Vec::new();
    if row.get::<Option<String>, _>("disabled_reason").is_some() {
        reasons.push("disabled");
    }
    if row.get::<Option<String>, _>("security_fence_ref").is_some() {
        reasons.push("security_fence");
    }
    if unknown != "0" || expired != "0" {
        reasons.push("unsettled");
    }
    if paused.is_some() {
        reasons.push("inference_paused");
    }
    let grants: Value = row.get("allow_rules");
    let effective = intersect_grants(&grants, configured_grants);
    let scope: Uuid = row.get("scope");
    let version = row.get::<i64, _>("pv").to_string();
    let timezone: String = row.get("period_timezone");
    let mode: String = row.get("budget_mode");
    let limit: Option<String> = row.get("budget");
    let block = json!({"blocked":!reasons.is_empty(),"reasons":reasons,"global_unknown_count":unknown,"expired_dispatch_count":expired});
    let budget=limit.as_ref().map(|n|json!({"mode":mode,"limit_usd":n,"period":{"kind":"calendar_month","timezone":timezone}}));
    let p = json!({"id":id,"kind":row.get::<String,_>("kind"),"display_name":row.get::<String,_>("display_name"),"email":row.get::<Option<String>,_>("email"),"management_owner_id":row.get::<Option<Uuid>,_>("owner_principal_id"),"version":row.get::<i64,_>("version").to_string(),"policy":{"id":scope,"version":version,"disabled":row.get::<Option<String>,_>("disabled_reason").is_some(),"grants":grants,"limits":{"budget":budget,"rpm":row.get::<Option<i64>,_>("rpm_limit"),"tpm":row.get::<Option<i64>,_>("tpm_limit"),"count_rpm":row.get::<Option<i64>,_>("count_rpm_limit")}},"inference_safety":{"paused":paused.is_some(),"version":row.get::<i64,_>("inference_safety_version").to_string(),"reason":paused,"paused_at":null,"trigger_request_id":row.get::<Option<Uuid>,_>("inference_pause_request_id")},"block_state":block,"effective_grants":effective});
    let paused_at: Option<String> = sqlx::query_scalar(&format!(
        "SELECT {} FROM gw_principals WHERE id=$1",
        stamp_sql("inference_paused_at")
    ))
    .bind(id)
    .fetch_one(&mut *c)
    .await?;
    let mut p = p;
    p["inference_safety"]["paused_at"] = json!(paused_at);
    let balance = balance(
        c,
        scope,
        None,
        &version,
        &timezone,
        &mode,
        limit.as_deref(),
        &p["block_state"],
        &now,
        digest,
        Vec::new(),
    )
    .await?;
    Ok((p, vec![balance], now))
}
pub(crate) fn intersect_grants(policy: &Value, configured: &Value) -> Value {
    let mut result = Vec::new();
    for grant in policy.as_array().into_iter().flatten() {
        let caps: Vec<&Value> = grant["capabilities"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|cap| {
                configured.as_array().into_iter().flatten().any(|g| {
                    g["provider"] == grant["provider"]
                        && g["model"] == grant["model"]
                        && g["capabilities"]
                            .as_array()
                            .is_some_and(|v| v.contains(cap))
                })
            })
            .collect();
        if !caps.is_empty() {
            result.push(
                json!({"provider":grant["provider"],"model":grant["model"],"capabilities":caps}),
            );
        }
    }
    json!(result)
}
#[allow(clippy::too_many_arguments)]
async fn balance(
    c: &mut PgConnection,
    scope: Uuid,
    key: Option<Uuid>,
    version: &str,
    tz: &str,
    mode: &str,
    limit: Option<&str>,
    block: &Value,
    now: &str,
    digest: &str,
    extra: Vec<&str>,
) -> Result<Value, QueryError> {
    let sql = format!(
        "WITH period AS(SELECT date_trunc('month',$2::timestamptz AT TIME ZONE $3) AT TIME ZONE $3 AS start,(date_trunc('month',$2::timestamptz AT TIME ZONE $3)+interval '1 month') AT TIME ZONE $3 AS finish) SELECT {} AS start,{} AS finish,COALESCE(t.cost_usd,0)::numeric(38,12)::text AS cost,COALESCE(t.reserved_usd,0)::numeric(38,12)::text AS reserved,COALESCE(t.unknown_count,0)::text AS unknown,CASE WHEN $4::numeric IS NULL THEN NULL ELSE greatest($4::numeric-COALESCE(t.cost_usd,0)-CASE WHEN $5='reserved' THEN COALESCE(t.reserved_usd,0) ELSE 0 END,0)::numeric(38,12)::text END AS remaining FROM period LEFT JOIN gw_usage_totals t ON t.scope_id=$1 AND t.period_start=period.start",
        stamp_sql("period.start"),
        stamp_sql("period.finish")
    );
    let row = sqlx::query(&sql)
        .bind(scope)
        .bind(now)
        .bind(tz)
        .bind(limit)
        .bind(mode)
        .fetch_one(c)
        .await?;
    let unresolved = block["global_unknown_count"] != "0" || block["expired_dispatch_count"] != "0";
    let remaining: Option<String> = row.get("remaining");
    let mut reasons = block["reasons"]
        .as_array()
        .cloned()
        .ok_or(QueryError::Invariant)?;
    reasons.extend(extra.iter().map(|s| json!(s)));
    if remaining.as_deref() == Some("0.000000000000") {
        reasons.push(json!("budget_exhausted"));
    }
    Ok(
        json!({"scope_id":scope,"scope_kind":if key.is_some(){"key"}else{"principal"},"key_id":key,"period_start":row.get::<String,_>("start"),"period_end":row.get::<String,_>("finish"),"timezone":tz,"policy_version":version,"config_digest":digest,"mode":mode,"limit_usd":limit,"settled_usd":row.get::<String,_>("cost"),"reserved_usd":row.get::<String,_>("reserved"),"scope_unknown_count":row.get::<String,_>("unknown"),"principal_unknown_count":block["global_unknown_count"],"balance_state":if unresolved{"unresolved"}else if limit.is_none(){"unlimited"}else{"limited"},"remaining_usd":if unresolved {None}else{remaining},"blocked":!reasons.is_empty(),"reasons":reasons,"as_of":now}),
    )
}
pub(crate) async fn key_balance(
    c: &mut PgConnection,
    id: Uuid,
    key: Uuid,
    p: &Value,
    now: &str,
    digest: &str,
) -> Result<Value, QueryError> {
    let row=sqlx::query("SELECT q.id,q.version,q.budget_mode,q.budget_limit_usd::text AS budget,q.period_timezone,k.revoked_at IS NOT NULL AS revoked,k.expires_at<=$3::timestamptz AS expired FROM gw_virtual_keys k JOIN gw_policies q ON q.key_id=k.id WHERE k.id=$1 AND k.principal_id=$2").bind(key).bind(id).bind(now).fetch_optional(&mut *c).await?.ok_or(QueryError::NotFound)?;
    let mut reasons = Vec::new();
    if row.get::<bool, _>("revoked") {
        reasons.push("key_revoked");
    }
    if row.get::<Option<bool>, _>("expired") == Some(true) {
        reasons.push("key_expired");
    }
    balance(
        c,
        row.get("id"),
        Some(key),
        &row.get::<i64, _>("version").to_string(),
        &row.get::<String, _>("period_timezone"),
        &row.get::<String, _>("budget_mode"),
        row.get::<Option<String>, _>("budget").as_deref(),
        &p["block_state"],
        now,
        digest,
        reasons,
    )
    .await
}

pub(crate) async fn apply_balance_ceiling(
    c: &mut PgConnection,
    balance: &mut Value,
    ceiling: &super::policy::PolicyLimits,
) -> Result<(), QueryError> {
    use super::policy::{BudgetLimit, BudgetPeriod, PolicyLimits};
    let mode = balance["mode"]
        .as_str()
        .ok_or(QueryError::Invariant)?
        .to_owned();
    let tz = balance["timezone"]
        .as_str()
        .ok_or(QueryError::Invariant)?
        .to_owned();
    let stored = PolicyLimits {
        budget: balance["limit_usd"].as_str().map(|s| BudgetLimit {
            mode: mode.clone(),
            limit_usd: s.into(),
            period: BudgetPeriod {
                kind: "calendar_month".into(),
                timezone: tz.clone(),
            },
        }),
        ..Default::default()
    };
    let effective = stored.effective(ceiling);
    let conflict = effective.as_ref().is_err()
        || effective
            .as_ref()
            .ok()
            .and_then(|l| l.budget.as_ref())
            .is_some_and(|b| b.mode != mode || b.period.timezone != tz);
    if conflict {
        balance["balance_state"] = json!("unresolved");
        balance["remaining_usd"] = Value::Null;
        balance["blocked"] = json!(true);
        balance["reasons"]
            .as_array_mut()
            .ok_or(QueryError::Invariant)?
            .push(json!("configuration_conflict"));
    } else if let Some(budget) = effective.map_err(|_| QueryError::Invariant)?.budget {
        balance["limit_usd"] = json!(budget.limit_usd);
        let remaining:String=sqlx::query_scalar("SELECT greatest($1::numeric-$2::numeric-CASE WHEN $4='reserved' THEN $3::numeric ELSE 0 END,0)::numeric(28,12)::text").bind(&budget.limit_usd).bind(balance["settled_usd"].as_str()).bind(balance["reserved_usd"].as_str()).bind(&mode).fetch_one(c).await?;
        if balance["balance_state"] != "unresolved" {
            balance["balance_state"] = json!("limited");
            balance["remaining_usd"] = json!(remaining);
        }
        if remaining == "0.000000000000"
            && !balance["reasons"]
                .as_array()
                .ok_or(QueryError::Invariant)?
                .contains(&json!("budget_exhausted"))
        {
            balance["reasons"]
                .as_array_mut()
                .ok_or(QueryError::Invariant)?
                .push(json!("budget_exhausted"));
            balance["blocked"] = json!(true);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn usage_filters_and_cursor_are_strict() {
        for q in [
            "principal_id=00000000-0000-0000-0000-000000000000",
            "provider=other",
            "limit=101",
            "group_by=provider&limit=0",
            "from=2026-01-01T00:00:00Z",
            "from=2026-01-01&to=2026-02-01",
            "group_by=provider&group_by=model",
            "model=",
            "model=%FF",
            "model=%",
            "model=%GG",
            "from=2025-01-01T00:00:00Z&to=2026-01-01T00:00:00Z",
        ] {
            assert!(UsageQuery::parse(Some(q)).is_err(), "{q}");
        }
        let id = Uuid::new_v4();
        let c = BucketCursor {
            v: 1,
            kind: "usage_bucket".into(),
            resource: "usage".into(),
            principal: id,
            scope: "self".into(),
            filter: UsageFilter::default(),
            as_of: "2026-01-01T00:00:00Z".into(),
            expires: 1000,
            last: vec!["anthropic".into(), "".into()],
        };
        let raw = encode_cursor(&c, b"synthetic-key").unwrap();
        assert!(decode_cursor(&raw, b"synthetic-key", id, 999, "self").is_ok());
        assert!(decode_cursor(&raw, b"synthetic-key", id, 1000, "self").is_err());
        assert!(decode_cursor(&raw, b"other-key", id, 999, "self").is_err());
        assert!(decode_cursor(&raw, b"synthetic-key", Uuid::new_v4(), 999, "self").is_err());
    }
}
