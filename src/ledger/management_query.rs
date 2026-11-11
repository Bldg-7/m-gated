use super::{
    PgIdentityStore,
    dashboard::{self, QueryError},
    management::{self, ManagementError},
};
use crate::auth::AccessSettings;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::{PgConnection, Row};
use std::collections::BTreeMap;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceFilter(pub BTreeMap<String, String>);
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceCursor {
    v: u8,
    kind: String,
    resource: String,
    actor: Uuid,
    scope: String,
    filter: ResourceFilter,
    as_of: String,
    expires: i64,
    last_time: String,
    last_id: Uuid,
}
fn mac(key: &[u8], bytes: &[u8]) -> Hmac<Sha256> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("HMAC");
    m.update(b"m-gated-cursor-v1\0");
    m.update(bytes);
    m
}
fn encode(c: &ResourceCursor, key: &[u8]) -> Result<String, ManagementError> {
    let b = serde_json::to_vec(c).map_err(|_| ManagementError::Invariant)?;
    let s = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&b),
        URL_SAFE_NO_PAD.encode(mac(key, &b).finalize().into_bytes())
    );
    if s.len() > 2048 {
        return Err(QueryError::InvalidCursor.into());
    }
    Ok(s)
}
fn normalize_resource_filter(filter: &mut BTreeMap<String, String>) -> Result<(), ManagementError> {
    for name in ["principal_id", "key_id"] {
        if let Some(raw) = filter.get_mut(name) {
            *raw = Uuid::parse_str(raw)
                .map_err(|_| QueryError::InvalidFilter)?
                .to_string();
        }
    }
    for name in ["from", "to"] {
        if let Some(raw) = filter.get_mut(name) {
            *raw = dashboard::timestamp(raw)?;
        }
    }
    if let Some(raw) = filter.get_mut("limit") {
        let limit = raw.parse::<i64>().map_err(|_| QueryError::InvalidFilter)?;
        if !(1..=100).contains(&limit) {
            return Err(QueryError::InvalidFilter.into());
        }
        *raw = limit.to_string();
    }
    Ok(())
}
pub fn fields(raw: Option<&str>) -> Result<BTreeMap<String, String>, ManagementError> {
    if raw.is_some_and(|s| s.len() > 4096) {
        return Err(QueryError::InvalidFilter.into());
    }
    let mut fields = BTreeMap::new();
    for pair in raw.unwrap_or("").split('&').filter(|s| !s.is_empty()) {
        let (a, b) = pair.split_once('=').unwrap_or((pair, ""));
        if fields
            .insert(dashboard::decode_query(a)?, dashboard::decode_query(b)?)
            .is_some()
        {
            return Err(QueryError::InvalidFilter.into());
        }
    }
    Ok(fields)
}
pub(crate) fn stamp(e: &str) -> String {
    dashboard::stamp_sql(e)
}
pub(crate) async fn key_metadata(c: &mut PgConnection, id: Uuid) -> Result<Value, ManagementError> {
    let row=sqlx::query(&format!("SELECT k.*,{} AS created,{} AS expires,{} AS revoked,p.budget_mode,p.budget_limit_usd::text AS budget,p.period_timezone,p.rpm_limit,p.tpm_limit,p.count_rpm_limit FROM gw_virtual_keys k JOIN gw_policies p ON p.key_id=k.id WHERE k.id=$1",stamp("k.created_at"),stamp("k.expires_at"),stamp("k.revoked_at"))).bind(id).fetch_optional(c).await?.ok_or(ManagementError::NotFound)?;
    Ok(
        json!({"id":id,"principal_id":row.get::<Uuid,_>("principal_id"),"name":row.get::<String,_>("name"),"key_hint":row.get::<String,_>("public_prefix"),"created_at":row.get::<String,_>("created"),"created_by":row.get::<Uuid,_>("created_by"),"created_command_id":row.get::<Uuid,_>("created_command_id"),"validity":row.get::<String,_>("validity_preset"),"expires_at":row.get::<Option<String>,_>("expires"),"revoked_at":row.get::<Option<String>,_>("revoked"),"version":row.get::<i64,_>("version").to_string(),"grants":row.get::<Value,_>("grant_snapshot"),"limits":limits(&row)}),
    )
}
pub(crate) fn limits(row: &sqlx::postgres::PgRow) -> Value {
    json!({"budget":row.get::<Option<String>,_>("budget").map(|n|json!({"mode":row.get::<String,_>("budget_mode"),"limit_usd":n,"period":{"kind":"calendar_month","timezone":row.get::<String,_>("period_timezone")}})),"rpm":row.get::<Option<i64>,_>("rpm_limit"),"tpm":row.get::<Option<i64>,_>("tpm_limit"),"count_rpm":row.get::<Option<i64>,_>("count_rpm_limit")})
}
pub(crate) async fn principal(
    c: &mut PgConnection,
    id: Uuid,
    grants: &Value,
    digest: &str,
    settings: &AccessSettings,
) -> Result<Value, ManagementError> {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM gw_principals WHERE id=$1)")
        .bind(id)
        .fetch_one(&mut *c)
        .await?;
    if !exists {
        return Err(ManagementError::NotFound);
    }
    let (mut p, _, _) = dashboard::principal(c, id, grants, digest).await?;
    p.as_object_mut()
        .ok_or(ManagementError::Invariant)?
        .remove("effective_grants");
    let inactive = if p["kind"] == "human" {
        p["email"].as_str().is_none_or(|e| !settings.allows(e))
    } else {
        let owner = sqlx::query("SELECT * FROM gw_principals WHERE id=$1")
            .bind(
                p["management_owner_id"]
                    .as_str()
                    .and_then(|s| Uuid::parse_str(s).ok()),
            )
            .fetch_one(&mut *c)
            .await?;
        !management::active_human(&owner, settings)
    };
    if inactive {
        let reason = if p["kind"] == "human" {
            "not_allowed"
        } else {
            "owner_inactive"
        };
        p["block_state"]["blocked"] = json!(true);
        p["block_state"]["reasons"]
            .as_array_mut()
            .ok_or(ManagementError::Invariant)?
            .push(json!(reason));
    }
    Ok(p)
}
pub(crate) async fn request_detail(
    c: &mut PgConnection,
    id: Uuid,
    detail: bool,
) -> Result<Value, ManagementError> {
    let row=sqlx::query(&format!("SELECT r.*,r.cost_usd::text AS cost,{} AS admitted,{} AS finished,{} AS stopped FROM gw_requests r WHERE id=$1",stamp("admitted_at"),stamp("finished_at"),stamp("execution_stopped_at"))).bind(id).fetch_optional(&mut *c).await?.ok_or(ManagementError::NotFound)?;
    let mut value = json!({"id":id,"parent_request_id":row.get::<Option<Uuid>,_>("parent_request_id"),"purpose":row.get::<String,_>("purpose"),"principal_id":row.get::<Uuid,_>("principal_id"),"key_id":row.get::<Option<Uuid>,_>("key_id"),"provider":row.get::<String,_>("provider"),"model":row.get::<String,_>("requested_model"),"api":row.get::<String,_>("api"),"admitted_at":row.get::<String,_>("admitted"),"finished_at":row.get::<Option<String>,_>("finished"),"phase":row.get::<String,_>("phase"),"outcome":row.get::<Option<String>,_>("outcome"),"accounting_state":row.get::<String,_>("accounting_state"),"cost_usd":row.get::<Option<String>,_>("cost"),"version":row.get::<i64,_>("version").to_string(),"reason_code":row.get::<Option<String>,_>("reason_code")});
    if !detail {
        return Ok(value);
    }
    let mut budgets = Vec::new();
    let rows=sqlx::query(&format!("SELECT b.*,reservation_usd::text AS reservation,remaining_reservation_usd::text AS remaining,{} AS start,{} AS finish FROM gw_request_budgets b WHERE request_id=$1 ORDER BY scope_id LIMIT 3",stamp("period_start"),stamp("period_end"))).bind(id).fetch_all(&mut *c).await?;
    if rows.len() > 2 {
        return Err(ManagementError::Invariant);
    }
    for b in rows {
        budgets.push(json!({"scope_id":b.get::<Uuid,_>("scope_id"),"policy_version":b.get::<i64,_>("policy_version").to_string(),"period_start":b.get::<String,_>("start"),"period_end":b.get::<String,_>("finish"),"reservation_usd":b.get::<String,_>("reservation"),"remaining_reservation_usd":b.get::<String,_>("remaining"),"reserved_tokens":b.get::<i64,_>("reserved_tokens").to_string(),"state":b.get::<String,_>("state")}));
    }
    let events=sqlx::query("SELECT kind,normalized_usage,pricing_resolution FROM gw_usage_events WHERE request_id=$1 ORDER BY kind LIMIT 5").bind(id).fetch_all(&mut *c).await?;
    let mut usage = json!({"partial":null,"final":null,"late_final":null});
    let mut settlement = Value::Null;
    for e in events {
        let kind: String = e.get("kind");
        if kind == "settlement" {
            settlement = e.get("pricing_resolution");
        } else {
            usage[&kind] = e.get("normalized_usage");
        }
    }
    let o = value.as_object_mut().ok_or(ManagementError::Invariant)?;
    for (k, v) in [
        (
            "provider_request_id",
            json!(row.get::<Option<String>, _>("provider_request_id")),
        ),
        (
            "http_status",
            json!(row.get::<Option<i16>, _>("http_status")),
        ),
        ("pricing_snapshot", row.get("pricing_snapshot")),
        ("execution_snapshot", row.get("execution_snapshot")),
        ("budgets", json!(budgets)),
        ("usage", usage),
        ("executor_id", json!(row.get::<Uuid, _>("executor_id"))),
        (
            "execution_stopped_at",
            json!(row.get::<Option<String>, _>("stopped")),
        ),
        ("settlement", settlement),
    ] {
        o.insert(k.into(), v);
    }
    Ok(value)
}

pub struct QueryContext<'a> {
    pub actor: Uuid,
    pub admin: bool,
    pub settings: &'a AccessSettings,
    pub grants: &'a Value,
    pub digest: &'a str,
    pub signing_key: &'a [u8],
}
impl PgIdentityStore {
    pub async fn management_list(
        &self,
        ctx: &QueryContext<'_>,
        resource: &str,
        raw: Option<&str>,
    ) -> Result<Value, ManagementError> {
        let _gate = self
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| ManagementError::Database)?;
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            self.list_snapshot(ctx, resource, raw),
        )
        .await
        .map_err(|_| ManagementError::Database)?
    }
    async fn list_snapshot(
        &self,
        ctx: &QueryContext<'_>,
        resource: &str,
        raw: Option<&str>,
    ) -> Result<Value, ManagementError> {
        let mut filter = fields(raw)?;
        let cursor = filter.remove("cursor");
        let allowed = match resource {
            "keys" => {
                if ctx.admin {
                    vec!["principal_id", "status", "limit"]
                } else {
                    vec!["status", "limit"]
                }
            }
            "principals" => vec!["kind", "disabled", "limit"],
            "requests" => vec![
                "principal_id",
                "key_id",
                "provider",
                "model",
                "phase",
                "outcome",
                "accounting_state",
                "from",
                "to",
                "limit",
            ],
            _ => return Err(ManagementError::Invalid),
        };
        if filter.keys().any(|k| !allowed.contains(&k.as_str())) {
            return Err(QueryError::InvalidFilter.into());
        }
        normalize_resource_filter(&mut filter)?;
        let mut tx = self.begin_transaction().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        management::check_actor(&mut tx, ctx.actor, ctx.settings, ctx.admin).await?;
        let now: String = sqlx::query_scalar(&format!("SELECT {}", stamp("clock_timestamp()")))
            .fetch_one(&mut *tx)
            .await?;
        let unix = OffsetDateTime::parse(&now, &Rfc3339)
            .map_err(|_| ManagementError::Invariant)?
            .unix_timestamp();
        let mut as_of = now.clone();
        let mut expires = unix + 900;
        let mut last_time = None;
        let mut last_id = None;
        let scope = if ctx.admin { "admin" } else { "self" };
        if let Some(raw) = cursor {
            if raw.len() > 2048 {
                return Err(QueryError::InvalidCursor.into());
            }
            let (a, b) = raw.split_once('.').ok_or(QueryError::InvalidCursor)?;
            let bytes = URL_SAFE_NO_PAD
                .decode(a)
                .map_err(|_| QueryError::InvalidCursor)?;
            let tag = URL_SAFE_NO_PAD
                .decode(b)
                .map_err(|_| QueryError::InvalidCursor)?;
            mac(ctx.signing_key, &bytes)
                .verify_slice(&tag)
                .map_err(|_| QueryError::InvalidCursor)?;
            let c: ResourceCursor =
                serde_json::from_slice(&bytes).map_err(|_| QueryError::InvalidCursor)?;
            if c.v != 1
                || c.kind != "resource"
                || c.resource != resource
                || c.actor != ctx.actor
                || c.scope != scope
                || c.expires <= unix
                || c.expires > unix + 900
                || filter.iter().any(|(k, v)| c.filter.0.get(k) != Some(v))
            {
                return Err(QueryError::InvalidCursor.into());
            }
            filter = c.filter.0;
            as_of = c.as_of;
            expires = c.expires;
            last_time = Some(c.last_time);
            last_id = Some(c.last_id);
        }
        let limit = filter
            .get("limit")
            .map(|s| s.parse::<i64>().map_err(|_| QueryError::InvalidFilter))
            .transpose()?
            .unwrap_or(50);
        if !(1..=100).contains(&limit) {
            return Err(QueryError::InvalidFilter.into());
        }
        filter.insert("limit".into(), limit.to_string());
        let id_filter = |name: &str| -> Result<Option<Uuid>, ManagementError> {
            filter
                .get(name)
                .map(|s| Uuid::parse_str(s).map_err(|_| QueryError::InvalidFilter.into()))
                .transpose()
        };
        let principal_id = if ctx.admin {
            id_filter("principal_id")?
        } else {
            Some(ctx.actor)
        };
        let key_id = id_filter("key_id")?;
        if resource == "requests" && !filter.contains_key("accounting_state") {
            filter.insert("accounting_state".into(), "unknown".into());
        }
        let enum_filter =
            |name: &str, allowed: &[&str]| -> Result<Option<String>, ManagementError> {
                let v = filter.get(name).cloned();
                if v.as_ref().is_some_and(|v| !allowed.contains(&v.as_str())) {
                    return Err(QueryError::InvalidFilter.into());
                }
                Ok(v)
            };
        let status = enum_filter("status", &["active", "revoked", "expired"])?;
        let kind = enum_filter("kind", &["human", "service"])?;
        let disabled = enum_filter("disabled", &["true", "false"])?;
        let provider = enum_filter("provider", &["anthropic", "openai", "deepseek"])?;
        let phase = enum_filter("phase", &["admitted", "dispatching", "finished"])?;
        let outcome = enum_filter("outcome", &["completed", "rejected", "failed", "cancelled"])?;
        let state = enum_filter("accounting_state", &["pending", "settled", "unknown"])?;
        if let Some(model) = filter.get("model") {
            management::text(model, 200, 200).map_err(|_| QueryError::InvalidFilter)?;
        }
        if resource == "requests" {
            if filter.contains_key("from") != filter.contains_key("to") {
                return Err(QueryError::InvalidFilter.into());
            }
            if let (Some(a), Some(b)) = (filter.get("from"), filter.get("to")) {
                dashboard::validate_period(a, b)?;
                let a = dashboard::timestamp(a)?;
                let b = dashboard::timestamp(b)?;
                filter.insert("from".into(), a);
                filter.insert("to".into(), b);
            } else if state.as_deref() == Some("settled") {
                let t = OffsetDateTime::parse(&as_of, &Rfc3339)
                    .map_err(|_| ManagementError::Invariant)?;
                let start = time::Date::from_calendar_date(t.year(), t.month(), 1)
                    .map_err(|_| ManagementError::Invariant)?
                    .midnight()
                    .assume_utc()
                    .format(&Rfc3339)
                    .map_err(|_| ManagementError::Invariant)?;
                filter.insert("from".into(), start);
                filter.insert("to".into(), as_of.clone());
                normalize_resource_filter(&mut filter)?;
            }
            if let Some(from) = filter.get("from") {
                let boundary:Option<String>=sqlx::query_scalar(&format!("SELECT {} FROM gw_usage_totals t JOIN gw_policies p ON p.id=t.scope_id WHERE p.key_id IS NULL AND ($1::uuid IS NULL OR p.principal_id=$1)",stamp("max(t.detail_deleted_through)+interval '1 microsecond'"))).bind(principal_id).fetch_one(&mut *tx).await?;
                if let Some(b) = boundary
                    && OffsetDateTime::parse(from, &Rfc3339)
                        .map_err(|_| QueryError::InvalidFilter)?
                        < OffsetDateTime::parse(&b, &Rfc3339)
                            .map_err(|_| ManagementError::Invariant)?
                {
                    return Err(QueryError::DetailUnavailable(b).into());
                }
            }
        }
        let (table, clock) = match resource {
            "keys" => ("gw_virtual_keys", "created_at"),
            "principals" => ("gw_principals", "created_at"),
            _ => ("gw_requests", "admitted_at"),
        };
        let mut q = sqlx::QueryBuilder::<sqlx::Postgres>::new(format!(
            "SELECT id,{} AS time FROM {table} WHERE {clock}<=",
            stamp(clock)
        ));
        q.push_bind(&as_of).push("::timestamptz");
        if let (Some(t), Some(id)) = (&last_time, last_id) {
            q.push(format!(" AND ({clock},id)<("))
                .push_bind(t)
                .push("::timestamptz,")
                .push_bind(id)
                .push(")");
        }
        if resource == "keys" {
            if let Some(id) = principal_id {
                q.push(" AND principal_id=").push_bind(id);
            }
            if let Some(status) = status {
                q.push(match status.as_str() {
                    "active" => " AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at>",
                    "expired" => " AND revoked_at IS NULL AND expires_at<=",
                    _ => " AND revoked_at IS NOT NULL",
                });
                if status != "revoked" {
                    q.push_bind(&now).push("::timestamptz");
                    if status == "active" {
                        q.push(")");
                    }
                }
            }
        }
        if resource == "principals" {
            if let Some(v) = kind {
                q.push(" AND kind=").push_bind(v);
            }
            if let Some(v) = disabled {
                q.push(if v == "true" {
                    " AND disabled_at IS NOT NULL"
                } else {
                    " AND disabled_at IS NULL"
                });
            }
        }
        if resource == "requests" {
            for (column, v) in [("principal_id", principal_id), ("key_id", key_id)] {
                if let Some(v) = v {
                    q.push(format!(" AND {column}=")).push_bind(v);
                }
            }
            for (column, v) in [
                ("provider", provider),
                ("requested_model", filter.get("model").cloned()),
                ("phase", phase),
                ("outcome", outcome),
                ("accounting_state", state),
            ] {
                if let Some(v) = v {
                    q.push(format!(" AND {column}=")).push_bind(v);
                }
            }
            if let Some(v) = filter.get("from") {
                q.push(" AND admitted_at>=")
                    .push_bind(v)
                    .push("::timestamptz");
            }
            if let Some(v) = filter.get("to") {
                q.push(" AND admitted_at<")
                    .push_bind(v)
                    .push("::timestamptz");
            }
        }
        q.push(format!(" ORDER BY {clock} DESC,id DESC LIMIT "))
            .push_bind(limit + 1);
        // ID/time 목록만 최대101개다. 리소스 projection은 한 행씩 만들고 byte 한도 전에 중단한다.
        let ids = q.build().fetch_all(&mut *tx).await?;
        let mut items = Vec::new();
        let mut bytes = 256;
        let mut previous = None;
        let mut more = false;
        for row in ids {
            if items.len() == limit as usize {
                more = true;
                break;
            }
            let id: Uuid = row.get("id");
            let item = match resource {
                "keys" => key_metadata(&mut tx, id).await?,
                "principals" => {
                    let mut p =
                        principal(&mut tx, id, ctx.grants, ctx.digest, ctx.settings).await?;
                    let policy = p
                        .as_object_mut()
                        .ok_or(ManagementError::Invariant)?
                        .remove("policy")
                        .ok_or(ManagementError::Invariant)?;
                    p["policy_version"] = policy["version"].clone();
                    p["disabled"] = policy["disabled"].clone();
                    p
                }
                _ => request_detail(&mut tx, id, false).await?,
            };
            let size = serde_json::to_vec(&item)
                .map_err(|_| ManagementError::Invariant)?
                .len();
            if size > 1024 * 1024 - 4096 {
                return Err(ManagementError::Invariant);
            }
            if bytes + size > 1024 * 1024 - 4096 {
                more = true;
                break;
            }
            bytes += size;
            previous = Some((row.get::<String, _>("time"), id));
            items.push(item);
        }
        let next = if more {
            let (t, id) = previous.ok_or(ManagementError::Invariant)?;
            Some(encode(
                &ResourceCursor {
                    v: 1,
                    kind: "resource".into(),
                    resource: resource.into(),
                    actor: ctx.actor,
                    scope: scope.into(),
                    filter: ResourceFilter(filter),
                    as_of: as_of.clone(),
                    expires,
                    last_time: t,
                    last_id: id,
                },
                ctx.signing_key,
            )?)
        } else {
            None
        };
        tx.commit().await?;
        Ok(json!({"items":items,"next_cursor":next,"as_of":as_of}))
    }
}
impl PgIdentityStore {
    pub async fn management_detail(
        &self,
        ctx: &QueryContext<'_>,
        resource: &str,
        id: Uuid,
        ceiling: &super::policy::PolicyLimits,
    ) -> Result<Value, ManagementError> {
        let _gate = self
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| ManagementError::Database)?;
        tokio::time::timeout(std::time::Duration::from_secs(3),async{
            let mut tx=self.begin_transaction().await?;sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await?;
            management::check_actor(&mut tx,ctx.actor,ctx.settings,true).await?;
            let value=if resource=="principals"{json!({"principal":principal(&mut tx,id,ctx.grants,ctx.digest,ctx.settings).await?,"execution_ceiling":{"grants":ctx.grants,"limits":ceiling}})}else{request_detail(&mut tx,id,true).await?};
            if serde_json::to_vec(&value).map_err(|_|ManagementError::Invariant)?.len()>1024*1024{return Err(ManagementError::Invariant);}
            tx.commit().await?;Ok(value)
        }).await.map_err(|_|ManagementError::Database)?
    }
    pub async fn management_usage(
        &self,
        ctx: &QueryContext<'_>,
        raw: Option<&str>,
        ceiling: &super::policy::PolicyLimits,
    ) -> Result<Value, ManagementError> {
        let _gate = self
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| ManagementError::Database)?;
        let mut fields = fields(raw)?;
        let principal = fields
            .remove("principal_id")
            .map(|s| Uuid::parse_str(&s).map_err(|_| QueryError::InvalidFilter))
            .transpose()?;
        let raw = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields.iter())
            .finish();
        let mut query = dashboard::UsageQuery::parse(Some(&raw))?;
        query.filter.principal_id = principal;
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            self.usage_snapshot(
                ctx.actor,
                query,
                ctx.grants,
                ctx.digest,
                ctx.signing_key,
                ceiling,
                Some(ctx),
            ),
        )
        .await
        .map_err(|_| ManagementError::Database)?
        .map_err(Into::into)
    }
}
impl PgIdentityStore {
    pub async fn management_key_access(
        &self,
        actor: Uuid,
        key: Uuid,
        settings: &AccessSettings,
    ) -> Result<(), ManagementError> {
        let _gate = self
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| ManagementError::Database)?;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let mut tx = self.begin_transaction().await?;
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *tx)
                .await?;
            management::check_actor(&mut tx, actor, settings, false).await?;
            let owner: Uuid =
                sqlx::query_scalar("SELECT principal_id FROM gw_virtual_keys WHERE id=$1")
                    .bind(key)
                    .fetch_optional(&mut *tx)
                    .await?
                    .ok_or(ManagementError::NotFound)?;
            if owner != actor {
                management::check_actor(&mut tx, actor, settings, true)
                    .await
                    .map_err(|e| {
                        if matches!(e, ManagementError::Forbidden) {
                            ManagementError::NotFound
                        } else {
                            e
                        }
                    })?;
            }
            tx.commit().await?;
            Ok(())
        })
        .await
        .map_err(|_| ManagementError::Database)?
    }
}
