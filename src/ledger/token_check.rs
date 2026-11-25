//! 토큰 대조와 사후 보정의 장부 쪽(결정 0005 11번, 23~27번).
//! - Anthropic 사용량 보고서의 1시간 칸과, 같은 시간의 게이트웨이 쪽 합계를 함께 저장한다.
//!   화면은 저장한 두 표만 읽는다.
//! - 사후 보정에 쓸 요청 목록을 내주고, 보정을 요청마다 한 번 장부에 반영한다.
//!   어느 요청을 어떻게 고칠지는 `crate::token_check`가 정한다.
use super::{
    PgLedger,
    amount::Amount,
    finance::{FinalUsage, LedgerError, PricingSnapshot},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};
use tokio::sync::OwnedSemaphorePermit;
use uuid::Uuid;

/// 사용량 보고서 한 칸. `start`는 칸이 시작하는 시각(epoch 초)이다.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageBucket {
    pub api_key_id: String,
    pub model: String,
    pub start: i64,
    pub uncached_input: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub cache_read: u64,
    pub output: u64,
}
/// 사용량 보고서가 1분 칸에 요청을 넣는 기준: 보낸 분(`Start`) 또는 끝난 분(`End`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Convention {
    Start,
    End,
}
impl Convention {
    /// 한쪽 기준으로만 맞는 분이 3분 이상이고 반대쪽이 그 10분의 1 이하일 때만 정한다.
    /// 반대쪽 분은 보고서가 늦게 채워진 분이나 시계 차이로 생길 수 있어 0을 요구하지 않는다.
    pub fn decide(start_minutes: i64, end_minutes: i64) -> Option<Self> {
        if end_minutes >= 3 && start_minutes.saturating_mul(10) <= end_minutes {
            Some(Self::End)
        } else if start_minutes >= 3 && end_minutes.saturating_mul(10) <= start_minutes {
            Some(Self::Start)
        } else {
            None
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::End => "end",
        }
    }
}
/// 게이트웨이가 요청의 사용량을 얼마나 아는지.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Known {
    /// Provider가 알린 사용량으로 정산했다.
    Exact,
    /// 끊긴 스트림을 규칙으로 정산했다(입력은 정확하고 출력은 규칙 값이다).
    Cutoff,
    /// 그 밖의 모든 경우: 실행 중, unknown, 0원 정산(no_response) 등. 토큰을 모른다.
    Unknown,
}
/// 보정과 기준 확인에 쓰는 요청 하나. 시각은 epoch 밀리초다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowRequest {
    pub id: Uuid,
    pub api_key_id: String,
    /// Provider가 알린 모델. 모르면 None이고, 그 API 키의 어느 모델 칸에도 들어갈 수 있다고 본다.
    pub model: Option<String>,
    pub sent_ms: i64,
    pub ended_ms: Option<i64>,
    pub known: Known,
    /// 입력(캐시 제외), 캐시 읽기, 캐시 쓰기, 출력.
    pub tokens: [u64; 4],
    pub max_tokens: Option<u64>,
    pub corrected: bool,
}
/// 모델별 가격(카테고리 → 백만 토큰당 USD). 차이 금액을 지금 카탈로그 가격으로 환산할 때 쓴다.
pub type Prices = BTreeMap<String, BTreeMap<String, String>>;

/// 토큰 종류. 배열 순서는 이 순서다. 게이트웨이는 캐시 쓰기를 5분과 1시간으로 나누지 않으므로
/// 게이트웨이 쪽 값은 캐시 쓰기 합계를 5분 자리에 두고 1시간 자리는 0으로 둔다.
const KINDS: [&str; 5] = [
    "input",
    "cache_write_5m",
    "cache_write_1h",
    "cache_read",
    "output",
];
/// Provider가 더 많이 센 토큰의 까닭. 화면에 이 순서로 보인다.
const REASONS: [&str; 4] = ["cutoff", "unknown", "outside", "unexplained"];
/// 사용량 보고서는 요청이 끝나고 몇 분 뒤에 나온다. 끝난 지 이만큼이 안 된 시간 칸은 받지 않는다.
pub const SETTLE_SECONDS: i64 = 600;
const DAY: i64 = 86_400;

type Tokens = [u64; 5];

fn token(row: &sqlx::postgres::PgRow, name: &str) -> u64 {
    u64::try_from(row.get::<i64, _>(name)).unwrap_or(0)
}
fn usage_number(usage: &Value, name: &str) -> u64 {
    usage[name]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}
fn add(a: &mut Tokens, b: &Tokens) {
    for i in 0..5 {
        a[i] = a[i].saturating_add(b[i]);
    }
}
fn total(n: &Tokens) -> u64 {
    n.iter().fold(0u64, |a, b| a.saturating_add(*b))
}
fn stamp(t: i64) -> Option<String> {
    time::OffsetDateTime::from_unix_timestamp(t)
        .ok()?
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
}
fn big(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}
/// 지금 카탈로그 가격으로 환산한다. 가격을 모르는 종류가 있으면 None이다.
fn price(prices: &Prices, model: &str, tokens: &Tokens) -> Option<Amount> {
    let rates = prices.get(model)?;
    let mut items = Vec::new();
    for (i, kind) in KINDS.iter().enumerate() {
        if tokens[i] == 0 {
            continue;
        }
        // 1시간 캐시 쓰기 가격이 없으면 5분 가격을 쓴다.
        let names: &[&str] = match *kind {
            "cache_write_5m" => &["cache_write_5m", "cache_write"],
            "cache_write_1h" => &["cache_write_1h", "cache_write_5m", "cache_write"],
            "input" => &["input"],
            "cache_read" => &["cache_read"],
            _ => &["output"],
        };
        let rate: Amount = names
            .iter()
            .find_map(|n| rates.get(*n))
            .and_then(|r| r.parse().ok())?;
        items.push((tokens[i], rate));
    }
    Amount::price(items, false).ok()
}

/// 금액 합계. 가격을 모르는 모델은 금액에 넣지 않고 따로 모은다.
#[derive(Default)]
struct Money {
    usd: Option<Amount>,
    unpriced: BTreeSet<String>,
}
impl Money {
    fn add(&mut self, prices: &Prices, model: &str, tokens: &Tokens) {
        if total(tokens) == 0 {
            return;
        }
        match price(prices, model, tokens) {
            Some(a) => {
                self.usd = Some(self.usd.unwrap_or(Amount::ZERO).checked_add(a).unwrap_or(a))
            }
            None => {
                self.unpriced.insert(model.to_owned());
            }
        }
    }
    fn usd(&self) -> String {
        self.usd.unwrap_or(Amount::ZERO).to_string()
    }
}

/// 게이트웨이 쪽 합계를 시간 칸에 넣을 시각과 대상 거르기. 보고서의 1분 칸 기준을 알면 그 기준을 따르고,
/// 모르면 끝난 시각이다. (합계 열, 합계 거르기, 활동 열, 활동 거르기). 거르기는 색인을 타도록 범위를 함께 건다.
fn hour_columns(convention: Option<Convention>) -> [&'static str; 4] {
    match convention {
        Some(Convention::Start) => [
            "r.dispatch_authorized_at",
            "r.admitted_at>=to_timestamp($1)-interval '1 minute' AND r.admitted_at<to_timestamp($2) AND r.dispatch_authorized_at>=to_timestamp($1) AND r.dispatch_authorized_at<to_timestamp($2)",
            "r.dispatch_authorized_at",
            "r.admitted_at>=to_timestamp($1)-interval '1 minute' AND r.admitted_at<to_timestamp($2) AND r.dispatch_authorized_at>=to_timestamp($1) AND r.dispatch_authorized_at<to_timestamp($2)",
        ],
        _ => [
            "r.finished_at",
            "r.finished_at>=to_timestamp($1) AND r.finished_at<to_timestamp($2)",
            "coalesce(r.finished_at,r.dispatch_authorized_at)",
            "r.admitted_at>=to_timestamp($1)-interval '1 day' AND r.admitted_at<to_timestamp($2) AND coalesce(r.finished_at,r.dispatch_authorized_at)>=to_timestamp($1) AND coalesce(r.finished_at,r.dispatch_authorized_at)<to_timestamp($2)",
        ],
    }
}

impl PgLedger {
    /// 마감에 남겨 둔 DB 연결을 쓰지 않도록, 다른 관리 작업과 같은 허가를 잡는다. 2초 안에 못 잡으면 실패다.
    async fn token_check_gate(&self) -> Result<OwnedSemaphorePermit, LedgerError> {
        tokio::time::timeout(
            Duration::from_secs(2),
            self.database().gate.clone().acquire_owned(),
        )
        .await
        .map_err(|_| LedgerError::DatabaseUnavailable)?
        .map_err(|_| LedgerError::DatabaseUnavailable)
    }
    /// 이번 조회를 맡는다. 다른 서버가 4분 안에 맡았으면 false다.
    pub async fn take_token_check_lease(&self) -> Result<bool, LedgerError> {
        let _gate = self.token_check_gate().await?;
        Ok(sqlx::query_scalar::<_, String>("UPDATE gw_token_check_state SET lease_until=clock_timestamp()+interval '4 minutes',last_attempt_at=clock_timestamp() WHERE provider='anthropic' AND (lease_until IS NULL OR lease_until<=clock_timestamp()) RETURNING provider")
            .fetch_optional(self.database().pool())
            .await?
            .is_some())
    }
    /// 조회 결과를 남긴다. `error`가 있으면(영문 소문자 코드) 마지막 성공 시각은 그대로 둔다.
    pub async fn finish_token_check(&self, error: Option<&str>) -> Result<(), LedgerError> {
        let _gate = self.token_check_gate().await?;
        sqlx::query("UPDATE gw_token_check_state SET last_success_at=CASE WHEN $1::text IS NULL THEN clock_timestamp() ELSE last_success_at END,last_error=$1 WHERE provider='anthropic'")
            .bind(error)
            .execute(self.database().pool())
            .await?;
        Ok(())
    }
    /// Provider 키와 API 키 ID 짝을 저장한다. 바뀐 짝만 고친다.
    pub async fn save_key_ids(&self, pairs: &[(Uuid, String)]) -> Result<(), LedgerError> {
        let _gate = self.token_check_gate().await?;
        let mut tx = self.database().begin_transaction().await?;
        for (credential, api_key_id) in pairs {
            sqlx::query("INSERT INTO gw_provider_key_ids(credential_id,api_key_id) VALUES($1,$2) ON CONFLICT(credential_id) DO UPDATE SET api_key_id=EXCLUDED.api_key_id,matched_at=clock_timestamp() WHERE gw_provider_key_ids.api_key_id<>EXCLUDED.api_key_id")
                .bind(credential)
                .bind(api_key_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    /// 저장된 API 키 ID마다 사용량 보고서를 받아 둔 끝 시각(epoch 초). 아직 받은 적이 없으면 None이다.
    /// 해제한 Provider 키의 API 키 ID도 남는다.
    pub async fn token_check_keys(&self) -> Result<Vec<(String, Option<i64>)>, LedgerError> {
        let _gate = self.token_check_gate().await?;
        Ok(sqlx::query("SELECT DISTINCT m.api_key_id,extract(epoch FROM k.covered_until)::bigint AS covered_until FROM gw_provider_key_ids m LEFT JOIN gw_token_check_keys k ON k.api_key_id=m.api_key_id ORDER BY m.api_key_id")
            .fetch_all(self.database().pool())
            .await?
            .iter()
            .map(|r| (r.get("api_key_id"), r.get("covered_until")))
            .collect())
    }
    /// 사용량 보고서의 1시간 칸을 저장하고, 같은 시간의 게이트웨이 쪽 합계를 다시 계산한다. 시각은 epoch 초다.
    /// - `[from, until)`이 실제로 받은 시간이다. 늦게 들어온 값이 있으므로 이 구간 전체를 지우고 다시 넣는다.
    /// - API 키마다 받아 둔 시간을 넓힌다. 지난 구간과 이어지지 않으면 새로 시작한다.
    /// - 90일이 지난 칸은 지운다.
    pub async fn store_usage(
        &self,
        api_key_ids: &[String],
        from: i64,
        until: i64,
        buckets: &[UsageBucket],
        convention: Option<Convention>,
    ) -> Result<(), LedgerError> {
        if from >= until || api_key_ids.is_empty() {
            return Ok(());
        }
        // 같은 키·모델·시각의 칸이 나뉘어 와도 합친다.
        let mut merged: BTreeMap<(&str, &str, i64), Tokens> = BTreeMap::new();
        for b in buckets {
            if !api_key_ids.contains(&b.api_key_id) || b.start < from || b.start >= until {
                continue;
            }
            add(
                merged
                    .entry((&b.api_key_id, &b.model, b.start))
                    .or_default(),
                &[
                    b.uncached_input,
                    b.cache_write_5m,
                    b.cache_write_1h,
                    b.cache_read,
                    b.output,
                ],
            );
        }
        let mut keys: Vec<&str> = Vec::new();
        let mut models: Vec<&str> = Vec::new();
        let mut hours: Vec<i64> = Vec::new();
        let mut counts: [Vec<i64>; 5] = Default::default();
        for ((key, model, start), n) in &merged {
            keys.push(key);
            models.push(model);
            hours.push(*start);
            for (i, v) in n.iter().enumerate() {
                counts[i].push(big(*v));
            }
        }
        let _gate = self.token_check_gate().await?;
        let mut tx = self.database().begin_transaction().await?;
        // 30일을 처음 받을 때도 끝나도록 이 트랜잭션의 문장 제한만 늘린다. 게이트웨이 쪽 합계는 하루씩 계산한다.
        sqlx::query("SET LOCAL statement_timeout='10s'")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM gw_provider_usage WHERE api_key_id=ANY($3) AND hour_start>=to_timestamp($1) AND hour_start<to_timestamp($2)")
            .bind(from)
            .bind(until)
            .bind(api_key_ids)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO gw_provider_usage(api_key_id,model,hour_start,uncached_input_tokens,cache_write_5m_tokens,cache_write_1h_tokens,cache_read_tokens,output_tokens) SELECT k,m,to_timestamp(h),a,b,c,d,e FROM unnest($1::text[],$2::text[],$3::bigint[],$4::bigint[],$5::bigint[],$6::bigint[],$7::bigint[],$8::bigint[]) AS u(k,m,h,a,b,c,d,e)")
            .bind(&keys)
            .bind(&models)
            .bind(&hours)
            .bind(&counts[0])
            .bind(&counts[1])
            .bind(&counts[2])
            .bind(&counts[3])
            .bind(&counts[4])
            .execute(&mut *tx)
            .await?;
        let [usage_column, usage_filter, activity_column, activity_filter] =
            hour_columns(convention);
        for table in ["gw_gateway_usage", "gw_gateway_activity"] {
            sqlx::query(&format!("DELETE FROM {table} WHERE api_key_id=ANY($3) AND hour_start>=to_timestamp($1) AND hour_start<to_timestamp($2)"))
                .bind(from)
                .bind(until)
                .bind(api_key_ids)
                .execute(&mut *tx)
                .await?;
        }
        let usage_sql = format!(
            "INSERT INTO gw_gateway_usage(api_key_id,model,hour_start,input_tokens,cache_write_tokens,cache_read_tokens,output_tokens,cutoff_requests,corrected_requests) SELECT m.api_key_id,e.actual_model,to_timestamp(floor(extract(epoch FROM {usage_column})/3600)*3600),sum(coalesce((e.normalized_usage->>'input_tokens')::bigint,0)),sum(coalesce((e.normalized_usage->>'cache_write_tokens')::bigint,0)),sum(coalesce((e.normalized_usage->>'cache_read_tokens')::bigint,0)),sum(coalesce((c.normalized_usage->>'output_tokens')::bigint,(e.normalized_usage->>'output_tokens')::bigint,0)),count(*) FILTER(WHERE e.normalized_usage->>'source' IN ('cutoff_received','cutoff_max_tokens')),count(c.id) FROM gw_requests r JOIN gw_provider_key_ids m ON m.credential_id=r.provider_credential_id JOIN gw_usage_events e ON e.request_id=r.id AND e.kind='final' AND e.actual_model IS NOT NULL LEFT JOIN gw_usage_events c ON c.request_id=r.id AND c.kind='correction' WHERE r.phase='finished' AND r.accounting_state='settled' AND r.provider='anthropic' AND r.api='messages' AND m.api_key_id=ANY($3) AND {usage_filter} GROUP BY 1,2,3"
        );
        // 사용량을 모르는 요청: 정산하지 못했거나(unknown·실행 중), 정산했지만 Provider 사용량 기록이 없다(관리자가 금액만 정한 요청).
        // HTTP 오류 등으로 0원 정산한 요청은 규칙으로 정산했으므로 넣지 않는다.
        let activity_sql = format!(
            "INSERT INTO gw_gateway_activity(api_key_id,hour_start,requests,unknown_requests) SELECT m.api_key_id,to_timestamp(floor(extract(epoch FROM {activity_column})/3600)*3600),count(*),count(*) FILTER(WHERE r.accounting_state<>'settled' OR NOT EXISTS(SELECT 1 FROM gw_usage_events e WHERE e.request_id=r.id AND e.kind='final')) FROM gw_requests r JOIN gw_provider_key_ids m ON m.credential_id=r.provider_credential_id WHERE r.provider='anthropic' AND r.api='messages' AND r.dispatch_authorized_at IS NOT NULL AND m.api_key_id=ANY($3) AND {activity_filter} GROUP BY 1,2"
        );
        let mut day = from;
        while day < until {
            let end = (day + DAY).min(until);
            for sql in [&usage_sql, &activity_sql] {
                sqlx::query(sql)
                    .bind(day)
                    .bind(end)
                    .bind(api_key_ids)
                    .execute(&mut *tx)
                    .await?;
            }
            day = end;
        }
        sqlx::query("INSERT INTO gw_token_check_keys(api_key_id,covered_from,covered_until) SELECT k,to_timestamp($1),to_timestamp($2) FROM unnest($3::text[]) AS u(k) ON CONFLICT(api_key_id) DO UPDATE SET covered_from=CASE WHEN gw_token_check_keys.covered_until>=EXCLUDED.covered_from THEN least(gw_token_check_keys.covered_from,EXCLUDED.covered_from) ELSE EXCLUDED.covered_from END,covered_until=greatest(gw_token_check_keys.covered_until,EXCLUDED.covered_until)")
            .bind(from)
            .bind(until)
            .bind(api_key_ids)
            .execute(&mut *tx)
            .await?;
        for table in [
            "gw_provider_usage",
            "gw_gateway_usage",
            "gw_gateway_activity",
        ] {
            sqlx::query(&format!(
                "DELETE FROM {table} WHERE hour_start<clock_timestamp()-interval '90 days'"
            ))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// 토큰 대조 화면의 값. 최근 `hours`시간 가운데 API 키마다 사용량 보고서를 받아 둔 1시간 칸에서
    /// Provider가 센 토큰과 게이트웨이가 정산한 토큰을 비교한다.
    /// - 칸마다 토큰 종류별로 차이를 내고, Provider가 더 많은 몫과 게이트웨이가 더 많은 몫을 따로 더한다.
    /// - Provider가 더 많은 몫은 칸마다 까닭 하나로 나눈다: 게이트웨이 밖 사용(그 시간에 그 API 키로
    ///   보낸 요청이 없다) → 끊긴 요청 → 사용량을 모르는 요청 → 원인 모름 순서로 먼저 맞는 것이다.
    /// - 금액은 지금 카탈로그 가격으로 환산한다. 가격을 모르는 모델은 금액에서 빼고 이름을 알린다.
    pub async fn token_check_view(
        &self,
        hours: i64,
        prices: &Prices,
    ) -> Result<Value, LedgerError> {
        let _gate = self.token_check_gate().await?;
        tokio::time::timeout(Duration::from_secs(5), self.read_token_check(hours, prices))
            .await
            .map_err(|_| LedgerError::DatabaseUnavailable)?
    }
    async fn read_token_check(&self, hours: i64, prices: &Prices) -> Result<Value, LedgerError> {
        let pool = self.database().pool();
        let state = sqlx::query("SELECT floor(extract(epoch FROM clock_timestamp()))::bigint AS now,floor(extract(epoch FROM last_attempt_at))::bigint AS last_attempt_at,floor(extract(epoch FROM last_success_at))::bigint AS last_success_at,last_error,start_minutes,end_minutes FROM gw_token_check_state WHERE provider='anthropic'")
            .fetch_one(pool)
            .await?;
        let now: i64 = state.get("now");
        let to = (now - SETTLE_SECONDS).div_euclid(3600) * 3600;
        let from = to - hours * 3600;
        // API 키마다 비교하는 구간: 요청한 기간 가운데 그 키의 사용량 보고서를 받아 둔 시간.
        let mut compared: BTreeMap<String, (i64, i64)> = BTreeMap::new();
        for r in sqlx::query("SELECT api_key_id,extract(epoch FROM covered_from)::bigint AS f,extract(epoch FROM covered_until)::bigint AS u FROM gw_token_check_keys WHERE covered_until>to_timestamp($1) AND covered_from<to_timestamp($2)")
            .bind(from)
            .bind(to)
            .fetch_all(pool)
            .await?
        {
            let (f, u) = (from.max(r.get("f")), to.min(r.get("u")));
            if f < u {
                compared.insert(r.get("api_key_id"), (f, u));
            }
        }
        let inside = |key: &str, hour: i64| {
            compared
                .get(key)
                .is_some_and(|(f, u)| *f <= hour && hour < *u)
        };
        type Key = (String, String, i64);
        let mut provider: BTreeMap<Key, Tokens> = BTreeMap::new();
        let mut gateway: BTreeMap<Key, Tokens> = BTreeMap::new();
        let mut cutoffs: BTreeSet<Key> = BTreeSet::new();
        let mut activity: BTreeMap<(String, i64), (i64, i64)> = BTreeMap::new();
        let mut corrected = 0i64;
        if !compared.is_empty() {
            for r in sqlx::query("SELECT api_key_id,model,extract(epoch FROM hour_start)::bigint AS hour,uncached_input_tokens,cache_write_5m_tokens,cache_write_1h_tokens,cache_read_tokens,output_tokens FROM gw_provider_usage WHERE hour_start>=to_timestamp($1) AND hour_start<to_timestamp($2)")
                .bind(from)
                .bind(to)
                .fetch_all(pool)
                .await?
            {
                let key: Key = (r.get("api_key_id"), r.get("model"), r.get("hour"));
                if inside(&key.0, key.2) {
                    provider.insert(
                        key,
                        [
                            token(&r, "uncached_input_tokens"),
                            token(&r, "cache_write_5m_tokens"),
                            token(&r, "cache_write_1h_tokens"),
                            token(&r, "cache_read_tokens"),
                            token(&r, "output_tokens"),
                        ],
                    );
                }
            }
            for r in sqlx::query("SELECT api_key_id,model,extract(epoch FROM hour_start)::bigint AS hour,input_tokens,cache_write_tokens,cache_read_tokens,output_tokens,cutoff_requests,corrected_requests FROM gw_gateway_usage WHERE hour_start>=to_timestamp($1) AND hour_start<to_timestamp($2)")
                .bind(from)
                .bind(to)
                .fetch_all(pool)
                .await?
            {
                let key: Key = (r.get("api_key_id"), r.get("model"), r.get("hour"));
                if !inside(&key.0, key.2) {
                    continue;
                }
                if r.get::<i64, _>("cutoff_requests") > 0 {
                    cutoffs.insert(key.clone());
                }
                corrected = corrected.saturating_add(r.get::<i64, _>("corrected_requests"));
                gateway.insert(
                    key,
                    [
                        token(&r, "input_tokens"),
                        token(&r, "cache_write_tokens"),
                        0,
                        token(&r, "cache_read_tokens"),
                        token(&r, "output_tokens"),
                    ],
                );
            }
            for r in sqlx::query("SELECT api_key_id,extract(epoch FROM hour_start)::bigint AS hour,requests,unknown_requests FROM gw_gateway_activity WHERE hour_start>=to_timestamp($1) AND hour_start<to_timestamp($2)")
                .bind(from)
                .bind(to)
                .fetch_all(pool)
                .await?
            {
                let key: (String, i64) = (r.get("api_key_id"), r.get("hour"));
                if inside(&key.0, key.1) {
                    activity.insert(key, (r.get("requests"), r.get("unknown_requests")));
                }
            }
        }
        let credential = |c: &sqlx::postgres::PgRow| json!({"id":c.get::<Uuid,_>("id"),"label":c.get::<String,_>("label"),"hint":c.get::<String,_>("hint")});
        // 같은 API 키를 다시 등록했으면 쓰고 있는 키, 그다음 최근 키의 이름을 보인다.
        let mut credential_of: BTreeMap<String, Value> = BTreeMap::new();
        for c in sqlx::query("SELECT m.api_key_id,c.id,c.label,c.hint FROM gw_provider_key_ids m JOIN gw_provider_credentials c ON c.id=m.credential_id ORDER BY c.state='active' DESC,c.created_at DESC,c.id")
            .fetch_all(pool)
            .await?
        {
            credential_of
                .entry(c.get("api_key_id"))
                .or_insert_with(|| credential(&c));
        }
        let unmatched: Vec<Value> = sqlx::query("SELECT id,label,hint FROM gw_provider_credentials WHERE provider='anthropic' AND state='active' AND id NOT IN (SELECT credential_id FROM gw_provider_key_ids) ORDER BY label,id")
            .fetch_all(pool)
            .await?
            .iter()
            .map(credential)
            .collect();

        // 칸마다 차이를 내고, (API 키, 모델)로 모은다.
        #[derive(Default)]
        struct Sum {
            provider: Tokens,
            gateway: Tokens,
            more: Tokens,
            less: Tokens,
            reasons: BTreeMap<&'static str, Tokens>,
        }
        let keys: BTreeSet<Key> = provider.keys().chain(gateway.keys()).cloned().collect();
        let mut rows: BTreeMap<(String, String), Sum> = BTreeMap::new();
        for key in keys {
            let p = provider.get(&key).copied().unwrap_or_default();
            let g = gateway.get(&key).copied().unwrap_or_default();
            // 캐시 쓰기는 5분과 1시간을 합쳐 비교하고, 더 많은 몫은 1시간 쓰기부터 채운다.
            let pairs = [
                (p[0], g[0]),
                (p[1].saturating_add(p[2]), g[1]),
                (0, 0),
                (p[3], g[3]),
                (p[4], g[4]),
            ];
            let mut more: Tokens = [0; 5];
            let mut less: Tokens = [0; 5];
            for (i, (pv, gv)) in pairs.into_iter().enumerate() {
                more[i] = pv.saturating_sub(gv);
                less[i] = gv.saturating_sub(pv);
            }
            more[2] = more[1].min(p[2]);
            more[1] -= more[2];
            let sum = rows.entry((key.0.clone(), key.1.clone())).or_default();
            add(&mut sum.provider, &p);
            add(&mut sum.gateway, &g);
            add(&mut sum.more, &more);
            add(&mut sum.less, &less);
            if total(&more) > 0 {
                let (requests, unknown) = activity
                    .get(&(key.0.clone(), key.2))
                    .copied()
                    .unwrap_or((0, 0));
                let reason = if requests == 0 {
                    "outside"
                } else if cutoffs.contains(&key) {
                    "cutoff"
                } else if unknown > 0 {
                    "unknown"
                } else {
                    "unexplained"
                };
                add(sum.reasons.entry(reason).or_default(), &more);
            }
        }
        let mut totals = Sum::default();
        let mut more_money = Money::default();
        let mut less_money = Money::default();
        let mut reason_money: BTreeMap<&'static str, Money> = BTreeMap::new();
        let mut items = Vec::new();
        for ((api_key_id, model), sum) in &rows {
            let mut row_more = Money::default();
            let mut row_less = Money::default();
            row_more.add(prices, model, &sum.more);
            row_less.add(prices, model, &sum.less);
            more_money.add(prices, model, &sum.more);
            less_money.add(prices, model, &sum.less);
            add(&mut totals.provider, &sum.provider);
            add(&mut totals.gateway, &sum.gateway);
            add(&mut totals.more, &sum.more);
            add(&mut totals.less, &sum.less);
            for (reason, n) in &sum.reasons {
                add(totals.reasons.entry(reason).or_default(), n);
                reason_money
                    .entry(reason)
                    .or_default()
                    .add(prices, model, n);
            }
            items.push(json!({
                "credential": credential_of.get(api_key_id),
                "api_key_id": api_key_id,
                "model": model,
                "provider_tokens": total(&sum.provider).to_string(),
                "gateway_tokens": total(&sum.gateway).to_string(),
                "provider_more_tokens": total(&sum.more).to_string(),
                "gateway_more_tokens": total(&sum.less).to_string(),
                "provider_more_usd": row_more.unpriced.is_empty().then(|| row_more.usd()),
                "gateway_more_usd": row_less.unpriced.is_empty().then(|| row_less.usd()),
                "reasons": REASONS.iter().filter_map(|r| sum.reasons.get(r).map(|n| json!({"reason":r,"tokens":total(n).to_string()}))).collect::<Vec<_>>(),
            }));
        }
        let reasons: Vec<Value> = REASONS
            .iter()
            .map(|r| {
                let tokens = totals.reasons.get(r).map_or(0, total);
                // 토큰이 모두 가격을 모르는 모델의 것이면 금액은 null이다.
                let usd = if tokens == 0 {
                    Some(Amount::ZERO.to_string())
                } else {
                    reason_money
                        .get(r)
                        .and_then(|m| m.usd)
                        .map(|a| a.to_string())
                };
                json!({"reason": r, "tokens": tokens.to_string(), "usd": usd})
            })
            .collect();
        let unpriced: BTreeSet<&String> = more_money
            .unpriced
            .iter()
            .chain(less_money.unpriced.iter())
            .collect();
        let start_minutes: i64 = state.get("start_minutes");
        let end_minutes: i64 = state.get("end_minutes");
        let span = compared
            .values()
            .map(|r| r.0)
            .min()
            .zip(compared.values().map(|r| r.1).max());
        Ok(json!({
            "range": {"from": stamp(from), "to": stamp(to)},
            "compared": span.map(|(f, t)| json!({"from": stamp(f), "to": stamp(t)})),
            "state": {
                "last_attempt_at": state.get::<Option<i64>, _>("last_attempt_at").and_then(stamp),
                "last_success_at": state.get::<Option<i64>, _>("last_success_at").and_then(stamp),
                "last_error": state.get::<Option<String>, _>("last_error"),
                "minute_convention": Convention::decide(start_minutes, end_minutes).map(Convention::name),
                "start_minutes": start_minutes.to_string(),
                "end_minutes": end_minutes.to_string(),
            },
            "totals": {
                "provider_tokens": total(&totals.provider).to_string(),
                "gateway_tokens": total(&totals.gateway).to_string(),
                "provider_more_tokens": total(&totals.more).to_string(),
                "gateway_more_tokens": total(&totals.less).to_string(),
                "provider_more_usd": more_money.usd(),
                "gateway_more_usd": less_money.usd(),
                "unpriced_models": unpriced,
                "corrected_requests": corrected.to_string(),
                "reasons": reasons,
            },
            "rows": items,
            "unmatched_credentials": unmatched,
        }))
    }

    /// 사후 보정 스위치가 켜져 있는지.
    pub async fn correction_enabled(&self) -> Result<bool, LedgerError> {
        let _gate = self.token_check_gate().await?;
        Ok(sqlx::query_scalar::<_, bool>(
            "SELECT correction FROM gw_settlement_rules WHERE provider='anthropic'",
        )
        .fetch_one(self.database().pool())
        .await?)
    }
    /// 1분 칸 기준을 세어 둔 값: (보낸 분 기준으로만 맞은 분, 끝난 분 기준으로만 맞은 분,
    /// 이 시각(epoch 밀리초) 전의 분은 이미 셌다).
    pub async fn minute_counts(&self) -> Result<(i64, i64, Option<i64>), LedgerError> {
        let _gate = self.token_check_gate().await?;
        let r = sqlx::query("SELECT start_minutes,end_minutes,(extract(epoch FROM minutes_checked_until)*1000)::bigint AS until FROM gw_token_check_state WHERE provider='anthropic'")
            .fetch_one(self.database().pool())
            .await?;
        Ok((r.get("start_minutes"), r.get("end_minutes"), r.get("until")))
    }
    /// 새로 센 분을 더하고, `until`(epoch 밀리초, 분 경계) 전의 분은 셌다고 남긴다.
    pub async fn add_minute_counts(
        &self,
        start: i64,
        end: i64,
        until: i64,
    ) -> Result<(), LedgerError> {
        let _gate = self.token_check_gate().await?;
        sqlx::query("UPDATE gw_token_check_state SET start_minutes=start_minutes+$1,end_minutes=end_minutes+$2,minutes_checked_until=greatest(coalesce(minutes_checked_until,to_timestamp(0)),to_timestamp($3::double precision/1000)) WHERE provider='anthropic'")
            .bind(start)
            .bind(end)
            .bind(until)
            .execute(self.database().pool())
            .await?;
        Ok(())
    }
    /// 짝지은 API 키로 보낸 Anthropic messages 요청 가운데, `from`(epoch 밀리초) 뒤에 끝났거나
    /// 아직 끝나지 않았고 `to` 전에 접수한 것. 한 번의 조회로 기준 확인과 보정에 모두 쓴다.
    pub async fn window_requests(
        &self,
        from: i64,
        to: i64,
    ) -> Result<Vec<WindowRequest>, LedgerError> {
        let _gate = self.token_check_gate().await?;
        let rows = sqlx::query("SELECT r.id,m.api_key_id,e.actual_model,floor(extract(epoch FROM r.dispatch_authorized_at)*1000)::bigint AS sent_ms,floor(extract(epoch FROM r.finished_at)*1000)::bigint AS ended_ms,r.accounting_state,e.normalized_usage AS usage,(r.execution_snapshot->>'output_limit')::bigint AS max_tokens,EXISTS(SELECT 1 FROM gw_usage_events c WHERE c.request_id=r.id AND c.kind='correction') AS corrected FROM gw_requests r JOIN gw_provider_key_ids m ON m.credential_id=r.provider_credential_id LEFT JOIN gw_usage_events e ON e.request_id=r.id AND e.kind='final' WHERE r.provider='anthropic' AND r.api='messages' AND r.dispatch_authorized_at IS NOT NULL AND r.admitted_at>=to_timestamp($1::double precision/1000)-interval '1 day' AND r.admitted_at<to_timestamp($2::double precision/1000) AND (r.finished_at IS NULL OR r.finished_at>=to_timestamp($1::double precision/1000))")
            .bind(from)
            .bind(to)
            .fetch_all(self.database().pool())
            .await?;
        Ok(rows
            .iter()
            .map(|r| {
                let usage = r.get::<Option<Value>, _>("usage").unwrap_or(Value::Null);
                let model: Option<String> = r.get("actual_model");
                let settled = r.get::<String, _>("accounting_state") == "settled";
                let known = match usage["source"].as_str() {
                    Some("provider") if settled && model.is_some() => Known::Exact,
                    Some("cutoff_received" | "cutoff_max_tokens") if settled && model.is_some() => {
                        Known::Cutoff
                    }
                    _ => Known::Unknown,
                };
                WindowRequest {
                    id: r.get("id"),
                    api_key_id: r.get("api_key_id"),
                    model,
                    sent_ms: r.get("sent_ms"),
                    ended_ms: r.get("ended_ms"),
                    known,
                    tokens: [
                        usage_number(&usage, "input_tokens"),
                        usage_number(&usage, "cache_read_tokens"),
                        usage_number(&usage, "cache_write_tokens"),
                        usage_number(&usage, "output_tokens"),
                    ],
                    max_tokens: r
                        .get::<Option<i64>, _>("max_tokens")
                        .and_then(|n| u64::try_from(n).ok()),
                    corrected: r.get("corrected"),
                }
            })
            .collect())
    }
    /// 끊긴 요청의 출력 토큰을 사용량 보고서 값으로 고친다(결정 0005 11번).
    /// 원래 예산 기간의 합계와 요청 금액을 함께 고치고, usage 기록(`correction`)과 시스템 감사 기록을 남긴다.
    /// 이미 보정했거나 끊긴 요청 정산이 아니면 false다.
    pub async fn apply_correction(
        &self,
        id: Uuid,
        output: u64,
        evidence: &Value,
    ) -> Result<bool, LedgerError> {
        let _gate = self.token_check_gate().await?;
        let mut tx = self.database().begin_transaction().await?;
        let (_, r, budgets) = self.lock_request(&mut tx, id).await?;
        let event = sqlx::query(
            "SELECT normalized_usage,actual_model FROM gw_usage_events WHERE request_id=$1 AND kind='final'",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let corrected: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM gw_usage_events WHERE request_id=$1 AND kind='correction')",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        let Some(event) = event else {
            return Ok(false);
        };
        let usage: Value = event.get("normalized_usage");
        let previous_source = usage["source"].as_str().unwrap_or("");
        if corrected
            || r.get::<String, _>("phase") != "finished"
            || r.get::<String, _>("accounting_state") != "settled"
            || !["cutoff_received", "cutoff_max_tokens"].contains(&previous_source)
        {
            return Ok(false);
        }
        let pricing: PricingSnapshot = serde_json::from_value(r.get("pricing_snapshot"))
            .map_err(|_| LedgerError::InvalidSnapshot)?;
        let previous_output = usage_number(&usage, "output_tokens");
        let corrected_usage = FinalUsage {
            actual_model: event
                .get::<Option<String>, _>("actual_model")
                .ok_or(LedgerError::InvalidSnapshot)?,
            input_tokens: usage_number(&usage, "input_tokens"),
            output_tokens: output,
            cache_read_tokens: usage_number(&usage, "cache_read_tokens"),
            cache_write_tokens: usage_number(&usage, "cache_write_tokens"),
            reasoning_tokens: None,
        };
        let cost = pricing.price(&corrected_usage, false)?;
        let previous_cost: String =
            sqlx::query_scalar("SELECT cost_usd::text FROM gw_requests WHERE id=$1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        let previous_output_i =
            i64::try_from(previous_output).map_err(|_| LedgerError::AmountOverflow)?;
        let output_i = i64::try_from(output).map_err(|_| LedgerError::AmountOverflow)?;
        for b in &budgets {
            let scope: Uuid = b.get("scope_id");
            let start: String = b.get("start_text");
            sqlx::query("UPDATE gw_usage_totals SET cost_usd=(cost_usd-($3::numeric)+($4::numeric)),output_tokens=output_tokens-$5+$6,updated_at=clock_timestamp() WHERE scope_id=$1 AND period_start=$2::timestamptz")
                .bind(scope)
                .bind(&start)
                .bind(&previous_cost)
                .bind(cost.to_string())
                .bind(previous_output_i)
                .bind(output_i)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE gw_request_budgets SET applied_cost_usd=($3::numeric),applied_output_tokens=$4 WHERE request_id=$1 AND scope_id=$2")
                .bind(id)
                .bind(scope)
                .bind(cost.to_string())
                .bind(output_i)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("UPDATE gw_requests SET cost_usd=($2::numeric),version=version+1 WHERE id=$1")
            .bind(id)
            .bind(cost.to_string())
            .execute(&mut *tx)
            .await?;
        let mut normalized = usage.clone();
        normalized["source"] = json!("provider_report");
        normalized["output_tokens"] = json!(output.to_string());
        normalized["previous_output_tokens"] = json!(previous_output.to_string());
        normalized["previous_source"] = json!(previous_source);
        let resolution = json!({"pricing_version":pricing.pricing_version,"cost_usd":cost.to_string(),"previous_cost_usd":previous_cost,"rounding_version":pricing.rounding_version});
        let digest =
            Sha256::digest(serde_json::to_vec(evidence).map_err(|_| LedgerError::InvalidSnapshot)?)
                .to_vec();
        sqlx::query("INSERT INTO gw_usage_events(request_id,kind,actual_model,provider_usage,normalized_usage,pricing_resolution,reason_code,content_digest) VALUES($1,'correction',$2,$3,$4,$5,'usage_correction',$6)")
            .bind(id)
            .bind(&corrected_usage.actual_model)
            .bind(evidence)
            .bind(&normalized)
            .bind(&resolution)
            .bind(&digest)
            .execute(&mut *tx)
            .await?;
        let before = json!({"cost_usd":previous_cost,"output_tokens":previous_output.to_string(),"source":previous_source});
        let after = json!({"cost_usd":cost.to_string(),"output_tokens":output.to_string(),"source":"provider_report"});
        sqlx::query("INSERT INTO gw_audit_events(actor_kind,executor_id,request_id,actor_auth_config_digest,operation,target_kind,target_id,before_snapshot,after_snapshot) VALUES('system',$1,$2,$3,'usage_correction','request',$2,$4,$5)")
            .bind(self.executor_id())
            .bind(id)
            .bind(self.auth_digest().as_slice())
            .bind(&before)
            .bind(&after)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn convention_needs_three_minutes_and_a_clear_majority() {
        assert_eq!(Convention::decide(0, 2), None);
        assert_eq!(Convention::decide(0, 3), Some(Convention::End));
        assert_eq!(Convention::decide(1, 10), Some(Convention::End));
        assert_eq!(Convention::decide(2, 10), None);
        assert_eq!(Convention::decide(30, 3), Some(Convention::Start));
        assert_eq!(Convention::decide(3, 3), None);
    }
}
