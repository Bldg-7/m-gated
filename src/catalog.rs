//! 공급사별 모델·가격 카탈로그.
//!
//! - 관리자가 동기화하면 공개 가격 목록 models.dev 를 받아 변경안을 만든다. 가격 출처는 이 하나만 쓴다.
//! - 관리자가 변경안을 승인해야 새 버전으로 적용된다. 자동 적용은 없다.
//! - 승인된 목록은 공급사별 정책(`CatalogTemplate`)과 합쳐 일반 profile 이 되고, 설정 파일 profile 과 같은 규칙으로 검증한다.
//! - 요청마다 입장 시점의 가격이 장부에 저장되므로, 버전이 바뀌어도 이미 기록된 금액은 바뀌지 않는다.
use crate::{
    config::{CatalogTemplate, Limits, ModelProfile, ProviderId, StartupError, ValidatedConfig},
    ledger::{
        PgIdentityStore,
        commands::CommandContext,
        management::{self, CommandResult, ManagementError},
        management_query::stamp,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, RwLock},
    time::Duration,
};
use url::Url;
use uuid::Uuid;

/// 가격 출처(models.dev)의 기본 주소. 시험에서만 다른 주소를 쓴다.
pub const SOURCE: &str = "models_dev";
pub fn default_source_url() -> Url {
    "https://models.dev/api.json".parse().expect("고정 주소")
}

/// 가격 출처가 알려 준 모델 하나의 값. 가격은 100만 토큰당 USD, 소수점 12자리 문자열.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub rates: BTreeMap<String, String>,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    /// 이 입력 토큰 수를 넘으면 다른(비싼) 가격이 적용된다는 표시.
    pub tier_threshold: Option<u64>,
}
type Parsed = BTreeMap<(ProviderId, String), (String, Candidate)>;

/// 모델 이름 표기가 조금 다를 수 있다(`claude-opus-5.5` / `claude-opus-5-5`). 비교에만 쓰는 키.
fn match_key(model: &str) -> String {
    model.to_ascii_lowercase().replace('.', "-")
}
fn per_mtok(value: &Value) -> Option<String> {
    let v = value.as_f64()?;
    (v.is_finite() && (0.0..1e9).contains(&v)).then(|| format!("{v:.12}"))
}
fn tokens(value: &Value) -> Option<u64> {
    value.as_u64().filter(|v| *v > 0 && *v <= 1_000_000_000)
}
fn provider_of(name: &str) -> Option<ProviderId> {
    match name {
        "anthropic" => Some(ProviderId::Anthropic),
        "openai" => Some(ProviderId::Openai),
        _ => None,
    }
}
fn insert(parsed: &mut Parsed, provider: ProviderId, model: &str, candidate: Candidate) {
    if model.is_empty() || model.len() > 200 || model.chars().any(|c| c.is_control()) {
        return;
    }
    parsed
        .entry((provider, match_key(model)))
        .or_insert((model.to_owned(), candidate));
}

pub fn parse_models_dev(body: &Value) -> Parsed {
    let mut parsed = Parsed::new();
    for name in ["anthropic", "openai"] {
        let provider = provider_of(name).expect("고정 이름");
        let Some(models) = body[name]["models"].as_object() else {
            continue;
        };
        for (id, model) in models {
            let cost = &model["cost"];
            let mut rates = BTreeMap::new();
            for (key, field) in [
                ("input", "input"),
                ("output", "output"),
                ("cache_read", "cache_read"),
                ("cache_write_5m", "cache_write"),
            ] {
                if let Some(v) = per_mtok(&cost[field]) {
                    rates.insert(key.to_owned(), v);
                }
            }
            let tier_threshold = cost["tiers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|t| t["tier"]["type"] == "context")
                .filter_map(|t| tokens(&t["tier"]["size"]))
                .min();
            insert(
                &mut parsed,
                provider,
                id,
                Candidate {
                    rates,
                    max_input_tokens: tokens(&model["limit"]["context"]),
                    max_output_tokens: tokens(&model["limit"]["output"]),
                    tier_threshold,
                },
            );
        }
    }
    parsed
}

/// 승인된 카탈로그 항목.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub provider: String,
    pub model: String,
    pub rates: BTreeMap<String, String>,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
    pub source: String,
    pub note: Option<String>,
}
impl Entry {
    fn provider_id(&self) -> Option<ProviderId> {
        ProviderId::from_name(&self.provider)
    }
    fn values(&self) -> Value {
        json!({"rates":self.rates,"max_input_tokens":self.max_input_tokens,"max_output_tokens":self.max_output_tokens})
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    /// 현재 값과 제안 값이 같다.
    Unchanged,
    /// 가격이나 토큰 한도가 바뀌었다.
    Changed,
    /// 카탈로그에 없는 새 모델 후보.
    New,
    /// 카탈로그에 있지만 가격 출처에 없다(승인하면 제거).
    Missing,
    /// 가격 출처의 값이 모자라 profile 을 만들 수 없다.
    Incomplete,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProposalItem {
    pub provider: String,
    pub model: String,
    pub status: ItemStatus,
    pub current: Option<Entry>,
    pub proposed: Option<Entry>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SourceReport {
    pub source: String,
    pub models: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Proposal {
    pub base_version: i64,
    /// 0.6 이전 변경안에는 없다(출처 여러 개를 쓰던 때). 승인에는 쓰지 않는다.
    #[serde(default)]
    pub source: SourceReport,
    pub items: Vec<ProposalItem>,
}

/// 가격 출처의 값으로 모델 하나의 제안 값을 만든다. 필요한 가격·토큰 한도가 하나라도 없으면 None.
fn propose(
    provider: ProviderId,
    template: &CatalogTemplate,
    model: &str,
    found: &Candidate,
) -> Option<Entry> {
    let required = template.required_rates(provider);
    if !required.iter().all(|k| found.rates.contains_key(*k)) {
        return None;
    }
    let context = found.max_input_tokens?;
    let output = found.max_output_tokens?;
    // 구간 가격(일정 입력 이상은 더 비쌈)은 장부가 표현하지 않으므로 그 아래로 입력을 제한한다.
    let tier = found.tier_threshold;
    let max_input = tier.map_or(context, |t| t.min(context));
    let rates = required
        .iter()
        .map(|k| ((*k).to_owned(), found.rates[*k].clone()))
        .collect();
    Some(Entry {
        provider: provider.name().to_owned(),
        model: model.to_owned(),
        rates,
        max_input_tokens: max_input,
        max_output_tokens: output,
        source: SOURCE.to_owned(),
        note: tier.map(|t| format!("tier_input_limit:{t}")),
    })
}

pub fn build_proposal(
    base_version: i64,
    current: &[Entry],
    templates: &BTreeMap<ProviderId, CatalogTemplate>,
    fetched: &Parsed,
) -> Proposal {
    let mut keys: BTreeMap<(ProviderId, String), String> = BTreeMap::new();
    for entry in current {
        if let Some(p) = entry.provider_id().filter(|p| templates.contains_key(p)) {
            keys.insert((p, match_key(&entry.model)), entry.model.clone());
        }
    }
    for ((provider, key), (model, _)) in fetched {
        if let Some(template) = templates.get(provider)
            && template
                .model_prefixes
                .iter()
                .any(|p| model.starts_with(p.as_str()))
        {
            keys.entry((*provider, key.clone()))
                .or_insert_with(|| model.clone());
        }
    }
    let mut items = Vec::new();
    for ((provider, key), model) in keys {
        let template = &templates[&provider];
        let found = fetched.get(&(provider, key.clone())).map(|(_, c)| c);
        let current = current
            .iter()
            .find(|e| e.provider_id() == Some(provider) && match_key(&e.model) == key)
            .cloned();
        let proposed = found.and_then(|f| propose(provider, template, &model, f));
        let status = match (&current, &proposed) {
            (Some(_), None) if found.is_none() => ItemStatus::Missing,
            (_, None) => ItemStatus::Incomplete,
            (None, Some(_)) => ItemStatus::New,
            (Some(c), Some(p)) if c.values() == p.values() => ItemStatus::Unchanged,
            (Some(_), Some(_)) => ItemStatus::Changed,
        };
        items.push(ProposalItem {
            provider: provider.name().to_owned(),
            model,
            status,
            current,
            proposed,
        });
    }
    Proposal {
        base_version,
        source: SourceReport {
            source: SOURCE.to_owned(),
            models: fetched.len(),
        },
        items,
    }
}

/// 승인된 카탈로그를 profile 로 만든 결과. 요청 처리 중에는 이 묶음 하나를 고정해서 쓴다.
#[derive(Default)]
pub struct CatalogSnapshot {
    pub version: i64,
    pub profiles: Vec<(ProviderId, ModelProfile)>,
}
#[derive(Default)]
pub struct CatalogCache {
    snapshot: RwLock<Arc<CatalogSnapshot>>,
}
impl CatalogCache {
    pub fn snapshot(&self) -> Arc<CatalogSnapshot> {
        self.snapshot.read().map(|s| s.clone()).unwrap_or_default()
    }
}

pub struct CatalogService {
    templates: BTreeMap<ProviderId, CatalogTemplate>,
    limits: Limits,
    store: Arc<PgIdentityStore>,
    cache: Arc<CatalogCache>,
    source: Url,
    client: reqwest::Client,
}
impl CatalogService {
    pub fn new(
        config: &ValidatedConfig,
        store: Arc<PgIdentityStore>,
    ) -> Result<Arc<Self>, StartupError> {
        Self::build(config, store, default_source_url(), true)
    }
    /// 테스트 조립 전용: 가격 출처를 loopback 합성 서버로 바꾼다.
    pub fn for_test(
        config: &ValidatedConfig,
        store: Arc<PgIdentityStore>,
        source: Url,
    ) -> Arc<Self> {
        Self::build(config, store, source, false).expect("테스트 조립")
    }
    fn build(
        config: &ValidatedConfig,
        store: Arc<PgIdentityStore>,
        source: Url,
        https_only: bool,
    ) -> Result<Arc<Self>, StartupError> {
        let client = reqwest::Client::builder()
            .https_only(https_only)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| StartupError::RuntimeFailure)?;
        Ok(Arc::new(Self {
            templates: config
                .providers()
                .iter()
                .filter_map(|p| Some((p.provider, p.catalog.clone()?)))
                .collect(),
            limits: config.limits().clone(),
            store,
            cache: Arc::new(CatalogCache::default()),
            source,
            client,
        }))
    }
    pub fn cache(&self) -> Arc<CatalogCache> {
        self.cache.clone()
    }
    pub fn enabled(&self) -> bool {
        !self.templates.is_empty()
    }

    async fn latest(c: &mut sqlx::PgConnection) -> Result<(i64, Vec<Entry>), sqlx::Error> {
        let row = sqlx::query(
            "SELECT version,entries FROM gw_model_catalog_versions ORDER BY version DESC LIMIT 1",
        )
        .fetch_optional(c)
        .await?;
        Ok(match row {
            None => (0, Vec::new()),
            Some(row) => (
                row.get("version"),
                serde_json::from_value(row.get("entries")).unwrap_or_default(),
            ),
        })
    }
    fn profiles(&self, version: i64, entries: &[Entry]) -> Vec<(ProviderId, ModelProfile)> {
        entries
            .iter()
            .filter_map(|e| {
                let provider = e.provider_id()?;
                let profile = self.templates.get(&provider)?.profile(
                    &e.model,
                    version,
                    e.rates.clone(),
                    e.max_input_tokens,
                    e.max_output_tokens,
                );
                profile.validate(provider, &self.limits).ok()?;
                Some((provider, profile))
            })
            .collect()
    }
    /// DB의 최신 버전과 캐시를 맞춘다. 버전이 같으면 다시 읽지 않는다.
    pub async fn refresh(&self) -> Result<(), sqlx::Error> {
        let mut c = self.store.pool().acquire().await?;
        let version: Option<i64> =
            sqlx::query_scalar("SELECT max(version) FROM gw_model_catalog_versions")
                .fetch_one(&mut *c)
                .await?;
        if version.unwrap_or(0) == self.cache.snapshot().version {
            return Ok(());
        }
        let (version, entries) = Self::latest(&mut c).await?;
        let snapshot = Arc::new(CatalogSnapshot {
            version,
            profiles: self.profiles(version, &entries),
        });
        if let Ok(mut current) = self.cache.snapshot.write() {
            *current = snapshot;
        }
        Ok(())
    }
    /// 관리 화면용: 현재 버전과 항목, 공급사별 정책 요약.
    pub async fn view(&self) -> Result<Value, ManagementError> {
        let mut c = self.store.pool().acquire().await?;
        let row = sqlx::query(&format!(
            "SELECT v.version,v.entries,{} AS approved,p.email FROM gw_model_catalog_versions v JOIN gw_principals p ON p.id=v.approved_by ORDER BY v.version DESC LIMIT 1",
            stamp("v.approved_at")
        ))
        .fetch_optional(&mut *c)
        .await?;
        let providers: Vec<Value> = self
            .templates
            .iter()
            .map(|(p, t)| json!({"provider":p.name(),"api":crate::ledger::finance::api_name(t.api),"model_prefixes":t.model_prefixes,"required_rates":t.required_rates(*p)}))
            .collect();
        Ok(match row {
            None => {
                json!({"version":"0","approved_at":null,"approved_by_email":null,"entries":[],"providers":providers})
            }
            Some(row) => {
                json!({"version":row.get::<i64,_>("version").to_string(),"approved_at":row.get::<String,_>("approved"),"approved_by_email":row.get::<Option<String>,_>("email"),"entries":row.get::<Value,_>("entries"),"providers":providers})
            }
        })
    }

    async fn fetch(&self) -> Option<Parsed> {
        let response = self
            .client
            .get(self.source.clone())
            .header("accept", "application/json")
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        // 목록 크기 상한. 넘으면 받지 못한 것으로 본다.
        let mut body = Vec::new();
        let mut response = response;
        while let Some(chunk) = response.chunk().await.ok()? {
            body.extend_from_slice(&chunk);
            if body.len() > 32 * 1024 * 1024 {
                return None;
            }
        }
        let value: Value = serde_json::from_slice(&body).ok()?;
        Some(parse_models_dev(&value))
    }

    async fn replay(
        &self,
        ctx: &CommandContext<'_>,
        digest: &[u8; 32],
    ) -> Result<Option<CommandResult>, ManagementError> {
        let mut c = self.store.pool().acquire().await?;
        let Some(prior) = sqlx::query(
            "SELECT input_digest,result_status,result_projection FROM gw_commands WHERE actor_principal_id=$1 AND command_id=$2",
        )
        .bind(ctx.actor)
        .bind(ctx.command_id)
        .fetch_optional(&mut *c)
        .await?
        else {
            return Ok(None);
        };
        if prior.get::<Vec<u8>, _>("input_digest") != digest {
            return Err(ManagementError::CommandConflict);
        }
        let mut ack: Value = prior.get("result_projection");
        ack["replayed"] = json!(true);
        if let Some(id) = ack["proposal_id"]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            && ack["operation"] == "sync_model_catalog"
        {
            let payload: Value =
                sqlx::query_scalar("SELECT payload FROM gw_model_catalog_proposals WHERE id=$1")
                    .bind(id)
                    .fetch_one(&mut *c)
                    .await?;
            ack["proposal"] = payload;
        }
        Ok(Some(CommandResult {
            status: prior.get::<i16, _>("result_status") as u16,
            body: Some(ack),
            replayed: true,
        }))
    }

    /// 공개 가격 목록을 받아 변경안을 만든다. 적용은 하지 않는다.
    pub async fn sync(&self, ctx: &CommandContext<'_>) -> Result<CommandResult, ManagementError> {
        if !self.enabled() {
            return Err(ManagementError::NotFound);
        }
        let digest = management::input_digest(ctx.method, ctx.path, &json!({}));
        if let Some(result) = self.replay(ctx, &digest).await? {
            return Ok(result);
        }
        {
            let mut c = self.store.pool().acquire().await?;
            management::check_actor(&mut c, ctx.actor, ctx.settings, true).await?;
        }
        let fetched = self
            .fetch()
            .await
            .ok_or(ManagementError::CatalogSourcesUnavailable)?;
        let mut tx = self.store.begin_transaction().await?;
        management::check_actor(&mut tx, ctx.actor, ctx.settings, true).await?;
        let (version, current) = Self::latest(&mut tx).await?;
        let proposal = build_proposal(version, &current, &self.templates, &fetched);
        let payload = serde_json::to_value(&proposal).map_err(|_| ManagementError::Invariant)?;
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO gw_model_catalog_proposals(id,base_version,created_by,payload) VALUES($1,$2,$3,$4)")
            .bind(id).bind(version).bind(ctx.actor).bind(&payload)
            .execute(&mut *tx).await?;
        let ack = json!({"command_id":ctx.command_id,"operation":"sync_model_catalog","target":{"kind":"catalog","id":id},"proposal_id":id,"result_version":version.to_string(),"effect":"changed"});
        sqlx::query("INSERT INTO gw_commands(actor_principal_id,command_id,input_digest,operation,target_kind,target_id,result_status,result_version,result_projection,created_at) VALUES($1,$2,$3,'sync_model_catalog','catalog',$4,201,NULL,$5,clock_timestamp())")
            .bind(ctx.actor).bind(ctx.command_id).bind(digest.as_slice()).bind(id).bind(&ack)
            .execute(&mut *tx).await?;
        tx.commit().await?;
        let mut body = ack;
        body["replayed"] = json!(false);
        body["proposal"] = payload;
        Ok(CommandResult {
            status: 201,
            body: Some(body),
            replayed: false,
        })
    }

    /// 변경안에서 고른 항목만 적용해 새 버전을 만든다.
    pub async fn approve(
        &self,
        ctx: &CommandContext<'_>,
        proposal_id: Uuid,
        expected_version: i64,
        selected: BTreeSet<(String, String)>,
    ) -> Result<CommandResult, ManagementError> {
        if !self.enabled() {
            return Err(ManagementError::NotFound);
        }
        if selected.is_empty() || selected.len() > 512 {
            return Err(ManagementError::Invalid);
        }
        let input = json!({"proposal_id":proposal_id,"expected_version":expected_version.to_string(),"items":selected.iter().map(|(p,m)|json!([p,m])).collect::<Vec<_>>()});
        let digest = management::input_digest(ctx.method, ctx.path, &input);
        if let Some(result) = self.replay(ctx, &digest).await? {
            return Ok(result);
        }
        let mut tx = self.store.begin_transaction().await?;
        management::check_actor(&mut tx, ctx.actor, ctx.settings, true).await?;
        // 동시 승인을 하나씩 처리한다.
        sqlx::query("SELECT pg_advisory_xact_lock(7234638650625913)")
            .execute(&mut *tx)
            .await?;
        let (version, current) = Self::latest(&mut tx).await?;
        if version != expected_version {
            return Err(ManagementError::Version(version));
        }
        let row =
            sqlx::query("SELECT base_version,payload FROM gw_model_catalog_proposals WHERE id=$1")
                .bind(proposal_id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(ManagementError::NotFound)?;
        if row.get::<i64, _>("base_version") != version {
            return Err(ManagementError::CatalogProposalStale);
        }
        let proposal: Proposal =
            serde_json::from_value(row.get("payload")).map_err(|_| ManagementError::Invariant)?;
        let mut entries: BTreeMap<(String, String), Entry> = current
            .iter()
            .map(|e| ((e.provider.clone(), e.model.clone()), e.clone()))
            .collect();
        let mut changes = Vec::new();
        for key in &selected {
            let item = proposal
                .items
                .iter()
                .find(|i| (&i.provider, &i.model) == (&key.0, &key.1))
                .ok_or(ManagementError::Invalid)?;
            match (item.status, &item.proposed, &item.current) {
                (ItemStatus::Changed | ItemStatus::New, Some(proposed), _) => {
                    if let Some(old) = &item.current {
                        entries.remove(&(old.provider.clone(), old.model.clone()));
                    }
                    entries.insert(key.clone(), proposed.clone());
                }
                (ItemStatus::Missing, _, Some(old)) => {
                    entries.remove(&(old.provider.clone(), old.model.clone()));
                }
                _ => return Err(ManagementError::Invalid),
            }
            changes.push(json!({"provider":item.provider,"model":item.model,"status":item.status,"before":item.current.as_ref().map(Entry::values),"after":item.proposed.as_ref().filter(|_|item.status!=ItemStatus::Missing).map(Entry::values)}));
        }
        let entries: Vec<Entry> = entries.into_values().collect();
        let next = version + 1;
        if entries.len() > 512 || self.profiles(next, &entries).len() != entries.len() {
            return Err(ManagementError::Invalid);
        }
        sqlx::query("INSERT INTO gw_model_catalog_versions(version,proposal_id,approved_by,entries) VALUES($1,$2,$3,$4)")
            .bind(next).bind(proposal_id).bind(ctx.actor).bind(serde_json::to_value(&entries).map_err(|_|ManagementError::Invariant)?)
            .execute(&mut *tx).await?;
        let target = Uuid::from_u64_pair(0, next as u64);
        let ack = json!({"command_id":ctx.command_id,"operation":"approve_model_catalog","target":{"kind":"catalog","id":target},"proposal_id":proposal_id,"result_version":next.to_string(),"effect":"changed"});
        sqlx::query("INSERT INTO gw_commands(actor_principal_id,command_id,input_digest,operation,target_kind,target_id,result_status,result_version,result_projection,created_at) VALUES($1,$2,$3,'approve_model_catalog','catalog',$4,200,$5,$6,clock_timestamp())")
            .bind(ctx.actor).bind(ctx.command_id).bind(digest.as_slice()).bind(target).bind(next).bind(&ack)
            .execute(&mut *tx).await?;
        // 감사 기록 크기 상한(32KiB) 안에 들어가게 변경 목록을 자른다.
        let mut listed = Vec::new();
        for change in changes {
            listed.push(change);
            if serde_json::to_vec(&listed).map_or(0, |v| v.len()) > 28 * 1024 {
                listed.pop();
                break;
            }
        }
        sqlx::query("INSERT INTO gw_audit_events(actor_kind,actor_principal_id,command_id,actor_auth_config_digest,operation,target_kind,target_id,before_snapshot,after_snapshot) VALUES('human',$1,$2,$3,'approve_model_catalog','catalog',$4,$5,$6)")
            .bind(ctx.actor).bind(ctx.command_id).bind(ctx.digest.as_slice()).bind(target)
            .bind(json!({"version":version,"models":current.len()}))
            .bind(json!({"version":next,"models":entries.len(),"proposal_id":proposal_id,"changes":listed}))
            .execute(&mut *tx).await?;
        tx.commit().await?;
        let mut body = ack;
        body["replayed"] = json!(false);
        Ok(CommandResult {
            status: 200,
            body: Some(body),
            replayed: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn template(prefix: &str) -> CatalogTemplate {
        serde_json::from_value(json!({"api":"messages","fixture_version":"anthropic-messages-v1","features":["client_tools","thinking","cache"],"max_body_bytes":8388608,"max_response_bytes":16777216,"model_prefixes":[prefix]})).unwrap()
    }
    fn fetched() -> Parsed {
        parse_models_dev(&json!({"anthropic":{"models":{
            "claude-opus-5-5":{"cost":{"input":4,"output":20,"cache_read":0.2,"cache_write":5},"limit":{"context":1000000,"output":128000}},
            "claude-sonnet-5.5":{"cost":{"input":2,"output":10,"cache_read":0.1,"cache_write":2.5},"limit":{"context":1000000,"output":128000}},
            "claude-haiku-5-5":{"cost":{"input":0.1,"output":0.5,"cache_read":0.01,"cache_write":0.125,"tiers":[{"input":0.5,"tier":{"type":"context","size":100000}}]},"limit":{"context":1000000,"output":128000}},
            "gpt-like":{"cost":{"input":1,"output":2},"limit":{"context":1000,"output":10}}}},
            "openai":{"models":{"gpt-x":{"cost":{"input":1,"output":2},"limit":{"context":1000,"output":10}}}},
            "bedrock":{"models":{"claude-opus-5-5":{"cost":{"input":9}}}}}))
    }
    #[test]
    fn parser_normalizes_units_names_and_tiers() {
        let p = fetched();
        let sonnet = &p[&(ProviderId::Anthropic, "claude-sonnet-5-5".into())];
        assert_eq!(
            sonnet.0, "claude-sonnet-5.5",
            "원래 표기를 유지하고 비교 키만 맞춘다"
        );
        assert_eq!(sonnet.1.rates["cache_read"], "0.100000000000");
        let haiku = &p[&(ProviderId::Anthropic, "claude-haiku-5-5".into())].1;
        assert_eq!(haiku.tier_threshold, Some(100_000));
        assert_eq!(haiku.rates["cache_write_5m"], "0.125000000000");
        assert!(p.contains_key(&(ProviderId::Openai, "gpt-x".into())));
        assert_eq!(p.len(), 5, "anthropic·openai 밖의 공급사는 버린다");
    }
    #[test]
    fn proposal_marks_new_changed_and_missing_without_applying() {
        let templates = BTreeMap::from([(ProviderId::Anthropic, template("claude-"))]);
        let current = vec![
            Entry {
                provider: "anthropic".into(),
                model: "claude-opus-5-5".into(),
                rates: BTreeMap::from([
                    ("input".into(), "5.000000000000".into()),
                    ("output".into(), "20.000000000000".into()),
                    ("cache_read".into(), "0.200000000000".into()),
                    ("cache_write_5m".into(), "5.000000000000".into()),
                ]),
                max_input_tokens: 1000000,
                max_output_tokens: 128000,
                source: "models_dev".into(),
                note: None,
            },
            Entry {
                provider: "anthropic".into(),
                model: "claude-retired-1".into(),
                rates: BTreeMap::new(),
                max_input_tokens: 1,
                max_output_tokens: 1,
                source: "models_dev".into(),
                note: None,
            },
        ];
        let p = build_proposal(3, &current, &templates, &fetched());
        let get = |m: &str| p.items.iter().find(|i| i.model == m).unwrap();
        let opus = get("claude-opus-5-5");
        assert_eq!(opus.status, ItemStatus::Changed);
        assert_eq!(
            opus.proposed.as_ref().unwrap().rates["input"],
            "4.000000000000"
        );
        assert_eq!(opus.proposed.as_ref().unwrap().source, "models_dev");
        let haiku = get("claude-haiku-5-5");
        assert_eq!(haiku.status, ItemStatus::New);
        assert_eq!(
            haiku.proposed.as_ref().unwrap().max_input_tokens,
            100_000,
            "구간 가격 아래로 입력 제한"
        );
        assert_eq!(get("claude-sonnet-5.5").status, ItemStatus::New);
        assert_eq!(get("claude-retired-1").status, ItemStatus::Missing);
        assert!(
            p.items.iter().all(|i| i.model != "gpt-like"),
            "접두사 밖 모델은 후보가 아니다"
        );
        assert_eq!(p.base_version, 3);
        assert_eq!(
            (p.source.source.as_str(), p.source.models),
            ("models_dev", 5)
        );
    }
    #[test]
    fn incomplete_prices_cannot_be_proposed() {
        let templates = BTreeMap::from([(ProviderId::Anthropic, template("claude-"))]);
        let only = json!({"anthropic":{"models":{"claude-x-1":{"cost":{"input":1,"output":2},"limit":{"context":1000,"output":10}}}}});
        let p = build_proposal(0, &[], &templates, &parse_models_dev(&only));
        assert_eq!(
            p.items[0].status,
            ItemStatus::Incomplete,
            "cache 기능에는 캐시 가격이 필요하다"
        );
    }
}
