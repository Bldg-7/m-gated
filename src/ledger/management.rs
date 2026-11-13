use super::{
    CurrentHuman, PgIdentityStore,
    amount::Amount,
    finance::{KeyScope, LedgerError},
    policy::PolicyLimits,
};
use crate::auth::{AccessSettings, AuthError, AuthenticatedInference, key::VirtualSecret};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, Row, postgres::PgRow};
use std::{collections::BTreeSet, time::Duration};
use uuid::Uuid;

#[derive(Debug)]
pub enum ManagementError {
    Invalid,
    Forbidden,
    NotFound,
    Database,
    Busy,
    Invariant,
    CommandConflict,
    Version(i64),
    SecretUnavailable(Uuid),
    Quota,
    PolicyPeriod,
    ConfigurationConflict,
    SecurityFence,
    SettlementConflict,
    ExecutionNotFinal,
    Query(super::dashboard::QueryError),
    /// Provider가 후보 키를 401/403으로 거절했다.
    CredentialRejected,
    /// Provider 확인 호출이 실패해 키가 유효한지 알 수 없다. 저장하지 않는다.
    CredentialUnverified,
    /// 같은 키 원문이 이미 다른 이름으로 등록돼 있다.
    CredentialDuplicate(Uuid),
    /// 같은 Provider에서 쓰고 있는 다른 키가 이미 이 이름을 쓴다.
    CredentialLabelTaken,
    /// 기본 키이거나 가상 키가 연결된 키라서 옮길 키를 정하지 않고는 해제할 수 없다.
    CredentialInUse {
        bound_keys: i64,
        default: bool,
    },
    /// 가상 키의 지금 연결이 요청이 가정한 회사 키와 다르다.
    BindingConflict(Option<Uuid>),
    /// 공개 가격 목록을 하나도 받지 못했다.
    CatalogSourcesUnavailable,
    /// 변경안을 만든 뒤 카탈로그가 이미 다른 버전으로 바뀌었다. 다시 동기화해야 한다.
    CatalogProposalStale,
}
impl From<sqlx::Error> for ManagementError {
    fn from(e: sqlx::Error) -> Self {
        if e.as_database_error()
            .is_some_and(|e| e.code().as_deref() == Some("55P03"))
        {
            Self::Busy
        } else {
            Self::Database
        }
    }
}
impl From<super::dashboard::QueryError> for ManagementError {
    fn from(e: super::dashboard::QueryError) -> Self {
        Self::Query(e)
    }
}
impl From<LedgerError> for ManagementError {
    fn from(e: LedgerError) -> Self {
        match e {
            LedgerError::Conflict => Self::PolicyPeriod,
            LedgerError::ConfigurationConflict => Self::ConfigurationConflict,
            LedgerError::InvalidSnapshot => Self::Invalid,
            LedgerError::AccessDenied | LedgerError::OwnerInactive => Self::Forbidden,
            LedgerError::InvariantViolation | LedgerError::AmountOverflow => Self::Invariant,
            _ => Self::Database,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub provider: String,
    pub model: String,
    pub capabilities: Vec<String>,
}
pub fn normalize_grants(mut grants: Vec<Grant>) -> Result<Value, ManagementError> {
    if grants.len() > 64 {
        return Err(ManagementError::Invalid);
    }
    let mut tuples = BTreeSet::new();
    for g in &mut grants {
        text(&g.model, 200, 200)?;
        if g.capabilities.is_empty() {
            return Err(ManagementError::Invalid);
        }
        g.capabilities.sort();
        for cap in &g.capabilities {
            let valid = match g.provider.as_str() {
                "anthropic" => ["messages", "count_tokens"].contains(&cap.as_str()),
                "openai" => ["responses", "chat_completions", "embeddings"].contains(&cap.as_str()),
                "deepseek" => cap == "chat_completions",
                _ => false,
            };
            if !valid || !tuples.insert((g.provider.clone(), g.model.clone(), cap.clone())) {
                return Err(ManagementError::Invalid);
            }
        }
    }
    grants.sort_by(|a, b| {
        (&a.provider, &a.model, &a.capabilities).cmp(&(&b.provider, &b.model, &b.capabilities))
    });
    serde_json::to_value(grants).map_err(|_| ManagementError::Invariant)
}
pub fn text(s: &str, chars: usize, bytes: usize) -> Result<(), ManagementError> {
    if s.is_empty()
        || s.chars().count() > chars
        || s.len() > bytes
        || s.chars().any(char::is_control)
    {
        Err(ManagementError::Invalid)
    } else {
        Ok(())
    }
}
pub fn version(s: &str) -> Result<i64, ManagementError> {
    if s.is_empty() || s.starts_with('0') || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ManagementError::Invalid);
    }
    s.parse::<i64>()
        .ok()
        .filter(|v| *v > 0)
        .ok_or(ManagementError::Invalid)
}
#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum Validity {
    #[serde(rename = "3_months")]
    ThreeMonths,
    #[serde(rename = "6_months")]
    SixMonths,
    #[serde(rename = "1_year")]
    OneYear,
    #[serde(rename = "3_years")]
    ThreeYears,
    #[serde(rename = "unlimited")]
    Unlimited,
}
impl Validity {
    pub fn months(self) -> Option<i32> {
        match self {
            Self::ThreeMonths => Some(3),
            Self::SixMonths => Some(6),
            Self::OneYear => Some(12),
            Self::ThreeYears => Some(36),
            Self::Unlimited => None,
        }
    }
    pub async fn expires_at(
        self,
        c: &mut PgConnection,
        created_at: &str,
    ) -> Result<Option<String>, ManagementError> {
        sqlx::query_scalar(&format!("SELECT {}",super::management_query::stamp("CASE WHEN $2::int IS NULL THEN NULL ELSE (($1::timestamptz AT TIME ZONE 'UTC')+make_interval(months=>$2)) AT TIME ZONE 'UTC' END"))).bind(created_at).bind(self.months()).fetch_one(c).await.map_err(Into::into)
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::ThreeMonths => "3_months",
            Self::SixMonths => "6_months",
            Self::OneYear => "1_year",
            Self::ThreeYears => "3_years",
            Self::Unlimited => "unlimited",
        }
    }
}
fn deserialize_api_limits<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<PolicyLimits, D::Error> {
    let value = Value::deserialize(d)?;
    if !["budget", "rpm", "tpm", "count_rpm"]
        .iter()
        .all(|k| value.get(*k).is_some())
    {
        return Err(serde::de::Error::custom("필수 Limits 필드"));
    }
    serde_json::from_value(value).map_err(serde::de::Error::custom)
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueKey {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_principal_id: Option<Uuid>,
    pub validity: Validity,
    pub grants: Vec<Grant>,
    #[serde(deserialize_with = "deserialize_api_limits")]
    pub limits: PolicyLimits,
    /// Provider별로 연결할 회사 키(관리자만). 고르지 않은 Provider는 그 Provider의 기본 키에 연결된다.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub provider_credentials: std::collections::BTreeMap<String, Uuid>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateService {
    pub display_name: String,
    pub management_owner_id: Uuid,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateService {
    pub expected_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub management_owner_id: Option<Uuid>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FenceRelease {
    pub fence_ref: String,
    pub old_processes_stopped_ref: String,
    pub confirmed: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplacePolicy {
    pub expected_version: String,
    pub disabled: bool,
    pub grants: Vec<Grant>,
    #[serde(deserialize_with = "deserialize_api_limits")]
    pub limits: PolicyLimits,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fence_release: Option<FenceRelease>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseSafety {
    pub expected_version: String,
    pub reason: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settlement {
    pub expected_version: String,
    pub decision: String,
    pub cost_usd: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_stopped_confirmed: Option<bool>,
}
pub enum Command {
    Issue(IssueKey),
    Revoke { id: Uuid, expected: String },
    Create(CreateService),
    Update(Uuid, UpdateService),
    Policy(Uuid, ReplacePolicy),
    Release(Uuid, ReleaseSafety),
    Settle(Uuid, Settlement),
}
impl Command {
    pub fn operation(&self) -> &'static str {
        match self {
            Self::Issue(_) => "issue_key",
            Self::Revoke { .. } => "revoke_key",
            Self::Create(_) => "create_service",
            Self::Update(..) => "update_service",
            Self::Policy(..) => "replace_policy",
            Self::Release(..) => "release_inference_pause",
            Self::Settle(..) => "settle_unknown",
        }
    }
    pub fn input(&self) -> Result<Value, ManagementError> {
        let v = match self {
            Self::Issue(v) => serde_json::to_value(v),
            Self::Revoke { expected, .. } => Ok(json!({"expected_version":expected})),
            Self::Create(v) => serde_json::to_value(v),
            Self::Update(_, v) => serde_json::to_value(v),
            Self::Policy(_, v) => serde_json::to_value(v),
            Self::Release(_, v) => serde_json::to_value(v),
            Self::Settle(_, v) => serde_json::to_value(v),
        }
        .map_err(|_| ManagementError::Invalid)?;
        Ok(v)
    }
    pub fn validate(&mut self) -> Result<(), ManagementError> {
        match self {
            Self::Issue(v) => {
                text(&v.name, 100, 400)?;
                if v.provider_credentials
                    .keys()
                    .any(|p| crate::config::ProviderId::from_name(p).is_none())
                {
                    return Err(ManagementError::Invalid);
                }
                v.limits.validate()?;
                v.grants = serde_json::from_value(normalize_grants(v.grants.clone())?)
                    .map_err(|_| ManagementError::Invalid)?;
            }
            Self::Revoke { expected, .. } => {
                version(expected)?;
            }
            Self::Create(v) => text(&v.display_name, 100, 400)?,
            Self::Update(_, v) => {
                version(&v.expected_version)?;
                if v.display_name.is_none() && v.management_owner_id.is_none() {
                    return Err(ManagementError::Invalid);
                }
                if let Some(s) = &v.display_name {
                    text(s, 100, 400)?;
                }
            }
            Self::Policy(_, v) => {
                version(&v.expected_version)?;
                text(&v.reason, 1000, 4096)?;
                v.limits.validate()?;
                v.grants = serde_json::from_value(normalize_grants(v.grants.clone())?)
                    .map_err(|_| ManagementError::Invalid)?;
                if let Some(f) = &v.fence_release {
                    text(&f.fence_ref, 2048, 2048)?;
                    text(&f.old_processes_stopped_ref, 2048, 2048)?;
                    if !f.confirmed || v.disabled {
                        return Err(ManagementError::Invalid);
                    }
                }
            }
            Self::Release(_, v) => {
                version(&v.expected_version)?;
                text(&v.reason, 1000, 4096)?;
            }
            Self::Settle(_, v) => {
                version(&v.expected_version)?;
                text(&v.reason, 1000, 4096)?;
                let cost = v
                    .cost_usd
                    .parse::<Amount>()
                    .map_err(|_| ManagementError::Invalid)?;
                if !["charged", "no_charge"].contains(&v.decision.as_str())
                    || (v.decision == "no_charge" && cost != Amount::ZERO)
                    || v.execution_stopped_confirmed == Some(false)
                {
                    return Err(ManagementError::Invalid);
                }
            }
        }
        Ok(())
    }
}
pub struct CommandResult {
    pub status: u16,
    pub body: Option<Value>,
    pub replayed: bool,
}
pub fn input_digest(method: &str, path: &str, input: &Value) -> [u8; 32] {
    Sha256::digest(
        serde_json::to_vec(&json!({"v":1,"method":method,"path":path,"input":input}))
            .expect("정규 JSON"),
    )
    .into()
}
pub(crate) fn cas(expected: &str, current: i64) -> Result<(), ManagementError> {
    if version(expected)? != current {
        Err(ManagementError::Version(current))
    } else {
        Ok(())
    }
}
pub(crate) fn active_human(p: &PgRow, settings: &AccessSettings) -> bool {
    p.get::<String, _>("kind") == "human"
        && p.get::<Option<String>, _>("disabled_reason").is_none()
        && p.get::<Option<String>, _>("security_fence_ref").is_none()
        && p.get::<Option<String>, _>("email")
            .is_some_and(|e| settings.allows(&e))
}
pub(crate) async fn check_actor(
    c: &mut PgConnection,
    id: Uuid,
    settings: &AccessSettings,
    admin: bool,
) -> Result<(), ManagementError> {
    let p = sqlx::query("SELECT * FROM gw_principals WHERE id=$1")
        .bind(id)
        .fetch_optional(c)
        .await?
        .ok_or(ManagementError::Forbidden)?;
    if !active_human(&p, settings) || (admin && !settings.is_admin(&p.get::<String, _>("email"))) {
        return Err(ManagementError::Forbidden);
    }
    Ok(())
}
impl PgIdentityStore {
    pub async fn authenticate_key(
        &self,
        raw: &str,
        settings: &AccessSettings,
    ) -> Result<AuthenticatedInference, AuthError> {
        let key = VirtualSecret::parse(raw)?;
        let _gate = self
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| AuthError::DatabaseUnavailable)?;
        tokio::time::timeout(Duration::from_secs(3),async {
            let mut tx=self.begin_transaction().await.map_err(|_|AuthError::DatabaseUnavailable)?;
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await.map_err(|_|AuthError::DatabaseUnavailable)?;
            let row=sqlx::query("SELECT k.secret_hash,k.version AS kv,k.principal_id,k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at>clock_timestamp()) AS active,p.*,q.id AS policy_id,q.version AS pv FROM gw_virtual_keys k JOIN gw_principals p ON p.id=k.principal_id JOIN gw_policies q ON q.principal_id=p.id AND q.key_id IS NULL WHERE k.id=$1").bind(key.id).fetch_optional(&mut *tx).await.map_err(|_|AuthError::DatabaseUnavailable)?.ok_or(AuthError::InvalidCredentials)?;
            if !key.matches(&row.get::<Vec<u8>,_>("secret_hash"))||!row.get::<bool,_>("active"){return Err(AuthError::InvalidCredentials);}
            if row.get::<Option<String>,_>("disabled_reason").is_some()||row.get::<Option<String>,_>("security_fence_ref").is_some(){return Err(AuthError::AccessDenied);}
            if row.get::<String,_>("kind")=="human" {
                if !active_human(&row,settings){return Err(AuthError::AccessDenied);}
            } else {
                let owner=sqlx::query("SELECT * FROM gw_principals WHERE id=$1").bind(row.get::<Option<Uuid>,_>("owner_principal_id")).fetch_one(&mut *tx).await.map_err(|_|AuthError::DatabaseUnavailable)?;
                if !active_human(&owner,settings){return Err(AuthError::OwnerInactive);}
            }
            let credentials=sqlx::query("SELECT provider,credential_id FROM gw_virtual_key_credentials WHERE key_id=$1").bind(key.id).fetch_all(&mut *tx).await.map_err(|_|AuthError::DatabaseUnavailable)?.iter().filter_map(|r|Some((crate::config::ProviderId::from_name(&r.get::<String,_>("provider"))?,r.get::<Uuid,_>("credential_id")))).collect();
            let result=AuthenticatedInference{current:CurrentHuman{id:row.get("principal_id"),version:row.get("version"),policy_id:row.get("policy_id"),policy_version:row.get("pv"),admin:false},key:Some(KeyScope{id:key.id,version:row.get("kv")}),credentials};
            tx.commit().await.map_err(|_|AuthError::DatabaseUnavailable)?;Ok(result)
        }).await.map_err(|_|AuthError::DatabaseUnavailable)?
    }
}

impl PgIdentityStore {
    pub async fn settlement_root(&self, id: Uuid) -> Result<Option<Uuid>, ManagementError> {
        let _gate = self
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| ManagementError::Database)?;
        tokio::time::timeout(Duration::from_secs(3), async {
            Ok(sqlx::query_scalar::<_, Option<Uuid>>(
                "SELECT parent_request_id FROM gw_requests WHERE id=$1",
            )
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .flatten())
        })
        .await
        .map_err(|_| ManagementError::Database)?
    }
}
