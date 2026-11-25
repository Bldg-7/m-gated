//! 토큰 대조와 사후 보정(결정 0005 11번, 23~27번). `ANTHROPIC_ADMIN_API_KEY`가 있을 때만 돈다.
//! - 5분마다 서버 한 대가 Anthropic Admin API를 조회한다(장부의 lease로 한 대만 고른다).
//! - API 키 목록의 앞뒤 글자로 Provider 키와 API 키 ID를 짝짓는다.
//! - 1시간 칸의 사용량을 저장한다. 늦게 들어오는 값이 있어 최근 48시간은 매번 다시 받고, 받지 못한
//!   시간이 있으면 30일 안에서 이어서 받는다. 끝난 지 10분이 안 된 시간은 건너뛴다.
//! - 사후 보정을 켜 두었으면 1분 칸을 받는다. 보고서가 요청을 보낸 분과 끝난 분 가운데 어디에 넣는지
//!   먼저 확인하고(`count_minutes`), 확인한 기준으로만 끊긴 요청을 고친다(`attribute`).
use crate::{
    config::{ProviderId, ValidatedConfig},
    credentials::CredentialService,
    ledger::{
        PgLedger,
        token_check::{Convention, Known, SETTLE_SECONDS, UsageBucket, WindowRequest},
    },
    providers::transport::{AdminError, NativeClient},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;
use zeroize::Zeroizing;

/// 응답 하나의 크기 상한.
const RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// 한 번의 조회에서 따라가는 페이지 수 상한.
const MAX_PAGES: usize = 20;
/// 한 페이지에 받는 1시간 칸 수(이틀). 30일을 처음 받으면 15페이지다.
const HOURS_PER_PAGE: &str = "48";
const DAY: i64 = 86_400;
const MINUTE_MS: i64 = 60_000;
/// 사후 보정은 끝난 지 10분에서 2시간 사이의 끊긴 요청만 본다. 2시간이 지나면 보고서도, 같은 분의
/// 다른 요청도 더 바뀌지 않으므로 다시 보지 않는다.
const CORRECT_AFTER_MS: i64 = 10 * MINUTE_MS;
const CORRECT_WITHIN_MS: i64 = 120 * MINUTE_MS;
/// 기준 확인은 끝난 지 30분이 지난 분만 쓴다. 보고서가 덜 채워진 분으로 틀리게 세지 않으려는 것이다.
const CALIBRATE_AFTER_MS: i64 = 30 * MINUTE_MS;
/// 보낸 시각과 끝난 시각은 게이트웨이와 Provider 사이에 몇 초 다를 수 있다. 기준 확인은 분 경계 앞뒤
/// 3초 안에 보내거나 끝난 요청이 있는 분을 쓰지 않는다.
const EDGE_MS: i64 = 3_000;
/// 고칠 요청은 그 분의 경계에서 5초 넘게 떨어져 있어야 한다. 끊긴 요청은 받은 출력을 세느라
/// 게이트웨이가 기록한 끝난 시각이 2초쯤 늦을 수 있다.
const CANDIDATE_EDGE_MS: i64 = 5_000;
/// 보정에 쓰는 시간. 넘으면 남은 요청은 다음 조회로 넘긴다.
const CORRECTION_BUDGET: Duration = Duration::from_secs(120);

/// 1분 칸 보고서. 칸 값은 [입력(캐시 제외), 캐시 읽기, 캐시 쓰기(5분+1시간), 출력]이고,
/// 열쇠의 분은 칸이 시작하는 시각(epoch 밀리초)이다. `minutes`는 보고서가 돌려준 분이다.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MinuteReport {
    pub cells: BTreeMap<(String, String, i64), [u64; 4]>,
    pub minutes: BTreeSet<i64>,
}
/// 사용량 보고서 한 번 조회한 결과. `starts`는 돌려준 칸의 시작 시각, `until`은 돌려준 마지막 칸의 끝 시각(epoch 초)이다.
struct Fetched {
    buckets: Vec<UsageBucket>,
    starts: BTreeSet<i64>,
    until: Option<i64>,
}

pub struct TokenCheck {
    ledger: Arc<PgLedger>,
    credentials: Arc<CredentialService>,
    admin_key: Zeroizing<String>,
}
impl std::fmt::Debug for TokenCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenCheck([REDACTED])")
    }
}
fn database<E>(_: E) -> &'static str {
    "database_unavailable"
}
impl TokenCheck {
    /// Admin API 키와 Anthropic Provider가 설정됐을 때만 만든다.
    pub fn new(
        config: &ValidatedConfig,
        ledger: Arc<PgLedger>,
        credentials: Arc<CredentialService>,
    ) -> Option<Arc<Self>> {
        let key = config.anthropic_admin_api_key()?;
        credentials.verifier(ProviderId::Anthropic)?;
        Some(Arc::new(Self {
            ledger,
            credentials,
            admin_key: Zeroizing::new(key.expose().to_owned()),
        }))
    }
    /// 종료 신호가 올 때까지 5분마다 조회한다. 첫 조회는 시작하고 10초 뒤다.
    /// 한 번의 조회가 4분을 넘으면 멈추고 `poll_timeout`을 남긴다.
    pub async fn run(
        self: Arc<Self>,
        runtime: Arc<crate::runtime::Runtime>,
    ) -> Result<(), crate::runtime::AdmissionError> {
        let mut stop = runtime.shutdown_signal();
        let mut wait = Duration::from_secs(10);
        loop {
            if *stop.borrow() {
                return Ok(());
            }
            tokio::select! { biased; _=stop.changed()=>return Ok(()), _=tokio::time::sleep(wait)=>{} }
            wait = Duration::from_secs(300);
            tokio::select! {
                biased;
                _=stop.changed()=>return Ok(()),
                result=tokio::time::timeout(Duration::from_secs(240), self.poll_once())=>{
                    if result.is_err() {
                        let _ = self.ledger.finish_token_check(Some("poll_timeout")).await;
                    }
                },
            }
        }
    }
    /// 한 번 조회한다. 다른 서버가 이번 차례를 맡았으면 아무것도 하지 않는다.
    pub async fn poll_once(&self) -> Result<(), &'static str> {
        match self.ledger.take_token_check_lease().await {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(_) => return Err("database_unavailable"),
        }
        let result = self.poll().await;
        let _ = self.ledger.finish_token_check(result.err()).await;
        result
    }
    async fn poll(&self) -> Result<(), &'static str> {
        let client = self
            .credentials
            .verifier(ProviderId::Anthropic)
            .ok_or("admin_api_unavailable")?;
        let known = self.match_keys(client).await?;
        // 보정을 먼저 해야, 이번 조회에서 다시 계산하는 게이트웨이 쪽 합계에 고친 출력이 들어간다.
        // 보정이 실패해도 사용량은 받고, 오류는 먼저 난 것을 남긴다.
        let corrected = if self.ledger.correction_enabled().await.map_err(database)? {
            self.correct(client).await
        } else {
            Ok(())
        };
        let (start_minutes, end_minutes, _) =
            self.ledger.minute_counts().await.map_err(database)?;
        let stored = self
            .fetch_usage(
                client,
                known,
                Convention::decide(start_minutes, end_minutes),
            )
            .await;
        corrected.and(stored)
    }
    /// API 키 목록의 앞뒤 글자로 Provider 키와 API 키 ID를 짝짓는다.
    /// 저장된 API 키 ID마다 사용량 보고서를 받아 둔 끝 시각(epoch 초)을 돌려준다.
    async fn match_keys(
        &self,
        client: &NativeClient,
    ) -> Result<Vec<(String, Option<i64>)>, &'static str> {
        let keys = self.api_keys(client).await?;
        let mut pairs = Vec::new();
        for credential in self.credentials.cache().active(ProviderId::Anthropic) {
            let found: Vec<&String> = keys
                .iter()
                .filter(|(_, hint)| hint_matches(hint, credential.secret.as_str()))
                .map(|(id, _)| id)
                .collect();
            if let [id] = found.as_slice() {
                pairs.push((credential.id, (*id).clone()));
            }
        }
        self.ledger.save_key_ids(&pairs).await.map_err(database)?;
        let known = self.ledger.token_check_keys().await.map_err(database)?;
        if known.is_empty() {
            return Err("no_matched_keys");
        }
        Ok(known)
    }
    /// 1시간 칸을 받아 저장한다. 게이트웨이 쪽 합계도 같은 시간을 다시 계산한다.
    async fn fetch_usage(
        &self,
        client: &NativeClient,
        known: Vec<(String, Option<i64>)>,
        convention: Option<Convention>,
    ) -> Result<(), &'static str> {
        // 끝난 1시간 칸만 읽는다. 끝난 지 10분이 안 된 칸은 아직 바뀔 수 있어 건너뛴다.
        // 최근 48시간은 늦게 들어온 값이 있을 수 있어 다시 읽고, 처음 짝지은 키나 오래 못 받은 키는 30일까지 채운다.
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let to = (now - SETTLE_SECONDS).div_euclid(3600) * 3600;
        let oldest = to - 30 * DAY;
        let from = known
            .iter()
            .map(|(_, until)| match until {
                Some(until) if *until >= oldest => (*until).min(to - 48 * 3600),
                _ => oldest,
            })
            .min()
            .unwrap_or(oldest);
        let ids: Vec<String> = known.into_iter().map(|(id, _)| id).collect();
        let fetched = self.usage(client, &ids, from, to, "1h").await?;
        // 보고서가 돌려준 마지막 칸까지만 받았다고 본다.
        let Some(until) = fetched.until.map(|until| until.min(to)) else {
            return Ok(());
        };
        self.ledger
            .store_usage(&ids, from, until, &fetched.buckets, convention)
            .await
            .map_err(database)
    }
    /// 조직의 API 키 목록: (키 ID, 앞뒤 글자). 상태와 무관하게 모두 읽는다(해제한 키의 지난 사용량도 대조한다).
    async fn api_keys(&self, client: &NativeClient) -> Result<Vec<(String, String)>, &'static str> {
        let mut keys = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..MAX_PAGES {
            // 주소 조립기는 스레드 사이에 보낼 수 없으므로 await 전에 버린다.
            let path = {
                let mut query = url::form_urlencoded::Serializer::new(String::new());
                query.append_pair("limit", "1000");
                if let Some(after) = &after {
                    query.append_pair("after_id", after);
                }
                format!("v1/organizations/api_keys?{}", query.finish())
            };
            let page = self.get(client, &path).await?;
            for key in page["data"].as_array().ok_or("admin_api_unavailable")? {
                if let (Some(id), Some(hint)) =
                    (key["id"].as_str(), key["partial_key_hint"].as_str())
                {
                    keys.push((id.to_owned(), hint.to_owned()));
                }
            }
            if page["has_more"] != json!(true) {
                return Ok(keys);
            }
            after = Some(
                page["last_id"]
                    .as_str()
                    .ok_or("admin_api_unavailable")?
                    .to_owned(),
            );
        }
        Err("admin_api_unavailable")
    }
    /// 사용량 보고서의 `[from, to)` 칸(epoch 초). API 키 ID와 모델로 나눈다.
    async fn usage(
        &self,
        client: &NativeClient,
        ids: &[String],
        from: i64,
        to: i64,
        width: &str,
    ) -> Result<Fetched, &'static str> {
        let seconds = if width == "1m" { 60 } else { 3600 };
        let (start, end) = (epoch(from)?, epoch(to)?);
        let mut fetched = Fetched {
            buckets: Vec::new(),
            starts: BTreeSet::new(),
            until: None,
        };
        // 주소가 너무 길어지지 않도록 API 키 ID는 100개씩 나눠 묻는다.
        for chunk in ids.chunks(100) {
            let mut page_token: Option<String> = None;
            let mut done = false;
            for _ in 0..MAX_PAGES {
                let path = {
                    let mut query = url::form_urlencoded::Serializer::new(String::new());
                    query
                        .append_pair("starting_at", &rfc3339(start))
                        .append_pair("ending_at", &rfc3339(end))
                        .append_pair("bucket_width", width)
                        .append_pair(
                            "limit",
                            if width == "1m" {
                                "1440"
                            } else {
                                HOURS_PER_PAGE
                            },
                        )
                        .append_pair("group_by[]", "api_key_id")
                        .append_pair("group_by[]", "model");
                    for id in chunk {
                        query.append_pair("api_key_ids[]", id);
                    }
                    if let Some(token) = &page_token {
                        query.append_pair("page", token);
                    }
                    format!("v1/organizations/usage_report/messages?{}", query.finish())
                };
                let page = self.get(client, &path).await?;
                for bucket in page["data"].as_array().ok_or("admin_api_unavailable")? {
                    let at = bucket["starting_at"]
                        .as_str()
                        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
                        .ok_or("admin_api_unavailable")?
                        .unix_timestamp();
                    // 요청한 구간 밖의 칸은 버린다(끝 시각을 포함해 돌려주는 경우).
                    if at < from || at >= to || at.rem_euclid(seconds) != 0 {
                        continue;
                    }
                    fetched.starts.insert(at);
                    fetched.until = Some(fetched.until.unwrap_or(at + seconds).max(at + seconds));
                    for r in bucket["results"].as_array().into_iter().flatten() {
                        let (Some(key), Some(model)) =
                            (r["api_key_id"].as_str(), r["model"].as_str())
                        else {
                            continue;
                        };
                        let n = |v: &Value| v.as_u64().unwrap_or(0);
                        fetched.buckets.push(UsageBucket {
                            api_key_id: key.to_owned(),
                            model: model.to_owned(),
                            start: at,
                            uncached_input: n(&r["uncached_input_tokens"]),
                            cache_write_5m: n(&r["cache_creation"]["ephemeral_5m_input_tokens"]),
                            cache_write_1h: n(&r["cache_creation"]["ephemeral_1h_input_tokens"]),
                            cache_read: n(&r["cache_read_input_tokens"]),
                            output: n(&r["output_tokens"]),
                        });
                    }
                }
                match (page["has_more"] == json!(true), page["next_page"].as_str()) {
                    (true, Some(next)) => page_token = Some(next.to_owned()),
                    _ => {
                        done = true;
                        break;
                    }
                }
            }
            if !done {
                return Err("admin_api_unavailable");
            }
        }
        Ok(fetched)
    }
    /// 끊긴 요청을 1분 칸의 사용량으로 고친다(결정 0005 11번).
    async fn correct(&self, client: &NativeClient) -> Result<(), &'static str> {
        let started = tokio::time::Instant::now();
        let now = OffsetDateTime::now_utc().unix_timestamp() * 1000;
        let to = minute_of(now - CORRECT_AFTER_MS);
        // 보낸 분은 끝난 분보다 실행 시간만큼 앞서므로 20분을 더 덮는다.
        let from = minute_of(now - CORRECT_WITHIN_MS - 20 * MINUTE_MS);
        let (start_minutes, end_minutes, checked) =
            self.ledger.minute_counts().await.map_err(database)?;
        let mut convention = Convention::decide(start_minutes, end_minutes);
        let requests = self
            .ledger
            .window_requests(from, to)
            .await
            .map_err(database)?;
        let candidates: Vec<&WindowRequest> = requests
            .iter()
            .filter(|r| {
                r.known == Known::Cutoff
                    && !r.corrected
                    && r.ended_ms
                        .is_some_and(|end| end >= now - CORRECT_WITHIN_MS && end < to)
            })
            .collect();
        // 기준을 이미 알고 고칠 요청도 없으면 보고서를 받지 않는다.
        if requests.is_empty() || (convention.is_some() && candidates.is_empty()) {
            return Ok(());
        }
        let mut ids: Vec<String> = requests.iter().map(|r| r.api_key_id.clone()).collect();
        ids.sort();
        ids.dedup();
        let fetched = self
            .usage(client, &ids, from / 1000, to / 1000, "1m")
            .await?;
        let report = minute_report(&fetched);
        // 1. 기준 확인: 아직 세지 않은 분 가운데 끝난 지 30분이 지난 분.
        let until = minute_of(now - CALIBRATE_AFTER_MS);
        let since = checked.map_or(from, |checked| checked.max(from));
        if since < until {
            let (start, end) = count_minutes(&report, &requests, since, until);
            self.ledger
                .add_minute_counts(start, end, until)
                .await
                .map_err(database)?;
            convention = Convention::decide(start_minutes + start, end_minutes + end);
        }
        let Some(convention) = convention else {
            return Ok(());
        };
        // 2. 보정. 한 요청이 실패해도 나머지는 계속하고, 실패는 다음 조회에서 다시 본다.
        let mut failed = false;
        for candidate in candidates {
            if started.elapsed() > CORRECTION_BUDGET {
                break;
            }
            if let Some((output, evidence)) = attribute(&report, &requests, convention, candidate)
                && self
                    .ledger
                    .apply_correction(candidate.id, output, &evidence)
                    .await
                    .is_err()
            {
                failed = true;
            }
        }
        if failed {
            Err("database_unavailable")
        } else {
            Ok(())
        }
    }
    async fn get(&self, client: &NativeClient, path: &str) -> Result<Value, &'static str> {
        client
            .admin_get(path, self.admin_key.as_str(), RESPONSE_BYTES)
            .await
            .map_err(|e| match e {
                AdminError::Rejected => "admin_key_rejected",
                AdminError::Unavailable => "admin_api_unavailable",
            })
    }
}

/// API 키 목록의 앞뒤 글자(예: `sk-ant-api03-R2D...igAA`)가 키 원문과 맞는지.
pub fn hint_matches(hint: &str, secret: &str) -> bool {
    let Some((head, tail)) = hint.split_once("...") else {
        return false;
    };
    head.len() >= 8
        && tail.len() >= 2
        && secret.len() > head.len() + tail.len()
        && secret.starts_with(head)
        && secret.ends_with(tail)
}
fn minute_report(fetched: &Fetched) -> MinuteReport {
    let mut report = MinuteReport {
        cells: BTreeMap::new(),
        minutes: fetched.starts.iter().map(|s| s * 1000).collect(),
    };
    for b in &fetched.buckets {
        let cell = report
            .cells
            .entry((b.api_key_id.clone(), b.model.clone(), b.start * 1000))
            .or_default();
        for (i, n) in [
            b.uncached_input,
            b.cache_read,
            b.cache_write_5m.saturating_add(b.cache_write_1h),
            b.output,
        ]
        .into_iter()
        .enumerate()
        {
            cell[i] = cell[i].saturating_add(n);
        }
    }
    report
}
fn minute_of(ms: i64) -> i64 {
    ms.div_euclid(MINUTE_MS) * MINUTE_MS
}
fn add_tokens(a: [u64; 4], b: &[u64; 4]) -> [u64; 4] {
    [0, 1, 2, 3].map(|i| a[i].saturating_add(b[i]))
}
/// `[from, to)`의 분 가운데, 같은 API 키·모델의 요청 사용량을 모두 알고 보낸 분 기준과 끝난 분 기준이
/// 다른 요청을 묶는 분을 골라, 보고서 칸과 맞는 기준을 센다. (보낸 분으로만 맞은 분, 끝난 분으로만 맞은 분).
/// - 사용량을 모르는 요청(모델을 모르면 모든 모델)이 그 분에 걸치면 쓰지 않는다.
/// - 분 경계 앞뒤 3초 안에 보내거나 끝난 요청이 있으면 쓰지 않는다(시계 차이로 다른 분에 들어갈 수 있다).
/// - 보고서가 그 분을 돌려주지 않았거나 칸이 없으면 쓰지 않는다(늦게 들어오는 값과 0을 구별하지 못한다).
pub fn count_minutes(
    report: &MinuteReport,
    requests: &[WindowRequest],
    from: i64,
    to: i64,
) -> (i64, i64) {
    let mut by_key: BTreeMap<&str, Vec<&WindowRequest>> = BTreeMap::new();
    for r in requests {
        by_key.entry(&r.api_key_id).or_default().push(r);
    }
    let (mut start, mut end) = (0, 0);
    for (key, list) in &by_key {
        for &minute in report.minutes.range(from..to) {
            let near_edge =
                |t: i64| (t - minute).abs() < EDGE_MS || (t - minute - MINUTE_MS).abs() < EDGE_MS;
            if list
                .iter()
                .any(|r| near_edge(r.sent_ms) || r.ended_ms.is_some_and(near_edge))
            {
                continue;
            }
            let touching: Vec<&WindowRequest> = list
                .iter()
                .copied()
                .filter(|r| {
                    minute_of(r.sent_ms) == minute || r.ended_ms.map(minute_of) == Some(minute)
                })
                .collect();
            let models: BTreeSet<&str> =
                touching.iter().filter_map(|r| r.model.as_deref()).collect();
            for model in models {
                if touching.iter().any(|r| {
                    r.known != Known::Exact && r.model.as_deref().is_none_or(|m| m == model)
                }) {
                    continue;
                }
                let same: Vec<&WindowRequest> = touching
                    .iter()
                    .copied()
                    .filter(|r| r.model.as_deref() == Some(model))
                    .collect();
                let sent: BTreeSet<Uuid> = same
                    .iter()
                    .filter(|r| minute_of(r.sent_ms) == minute)
                    .map(|r| r.id)
                    .collect();
                let ended: BTreeSet<Uuid> = same
                    .iter()
                    .filter(|r| r.ended_ms.map(minute_of) == Some(minute))
                    .map(|r| r.id)
                    .collect();
                if sent == ended {
                    continue;
                }
                let Some(cell) = report
                    .cells
                    .get(&((*key).to_owned(), model.to_owned(), minute))
                else {
                    continue;
                };
                let sum = |ids: &BTreeSet<Uuid>| {
                    same.iter()
                        .filter(|r| ids.contains(&r.id))
                        .fold([0u64; 4], |a, r| add_tokens(a, &r.tokens))
                };
                match (sum(&sent) == *cell, sum(&ended) == *cell) {
                    (true, false) => start += 1,
                    (false, true) => end += 1,
                    _ => {}
                }
            }
        }
    }
    (start, end)
}
/// 끊긴 요청 하나의 출력 토큰을 1분 칸에서 찾는다. 찾으면 (출력 토큰, 근거)다.
/// 확인한 기준으로 그 요청이 들어간 분의 칸에서, 같은 기준으로 그 분에 들어간 다른 요청의 사용량을 뺀다.
/// 다른 요청은 모두 Provider가 알린 사용량이어야 하고(모델을 모르는 요청도 막는다), 남은 입력·캐시 토큰이
/// 이 요청과 정확히 같아야 한다. 남은 출력은 이 요청의 max_tokens를 넘지 않는다.
pub fn attribute(
    report: &MinuteReport,
    requests: &[WindowRequest],
    convention: Convention,
    candidate: &WindowRequest,
) -> Option<(u64, Value)> {
    let model = candidate.model.as_deref()?;
    let max_tokens = candidate.max_tokens?;
    let assigned = |r: &WindowRequest| match convention {
        Convention::Start => Some(r.sent_ms),
        Convention::End => r.ended_ms,
    };
    let at = assigned(candidate)?;
    let minute = minute_of(at);
    if at - minute < CANDIDATE_EDGE_MS || minute + MINUTE_MS - at < CANDIDATE_EDGE_MS {
        return None;
    }
    if !report.minutes.contains(&minute) {
        return None;
    }
    let cell = report
        .cells
        .get(&(candidate.api_key_id.clone(), model.to_owned(), minute))?;
    let others: Vec<&WindowRequest> = requests
        .iter()
        .filter(|r| {
            r.id != candidate.id
                && r.api_key_id == candidate.api_key_id
                && r.model.as_deref().is_none_or(|m| m == model)
                && assigned(r).map(minute_of) == Some(minute)
        })
        .collect();
    if others.iter().any(|r| r.known != Known::Exact) {
        return None;
    }
    let mut rest = *cell;
    for r in &others {
        for (left, used) in rest.iter_mut().zip(r.tokens) {
            *left = left.checked_sub(used)?;
        }
    }
    if rest[..3] != candidate.tokens[..3] {
        return None;
    }
    let evidence = json!({
        "source": "anthropic_usage_report",
        "bucket_width": "1m",
        "minute": epoch(minute / 1000).ok().map(rfc3339),
        "convention": convention.name(),
        "api_key_id": candidate.api_key_id,
        "model": model,
        "report": {"uncached_input_tokens":cell[0],"cache_read_tokens":cell[1],"cache_write_tokens":cell[2],"output_tokens":cell[3]},
        "other_requests": others.len(),
    });
    Some((rest[3].min(max_tokens), evidence))
}
fn epoch(t: i64) -> Result<OffsetDateTime, &'static str> {
    OffsetDateTime::from_unix_timestamp(t).map_err(|_| "admin_api_unavailable")
}
fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    const M: i64 = 1_790_000_040_000; // 분 경계(epoch 밀리초)
    fn request(
        n: u128,
        sent: i64,
        ended: Option<i64>,
        known: Known,
        tokens: [u64; 4],
    ) -> WindowRequest {
        WindowRequest {
            id: Uuid::from_u128(n),
            api_key_id: "apikey_1".into(),
            model: Some("claude-opus-5-5".into()),
            sent_ms: sent,
            ended_ms: ended,
            known,
            tokens,
            max_tokens: Some(4096),
            corrected: false,
        }
    }
    fn report(cells: &[(i64, [u64; 4])]) -> MinuteReport {
        MinuteReport {
            cells: cells
                .iter()
                .map(|(m, c)| (("apikey_1".into(), "claude-opus-5-5".into(), *m), *c))
                .collect(),
            minutes: (-5..5).map(|i| M + i * MINUTE_MS).collect(),
        }
    }
    #[test]
    fn key_hint_matches_only_the_same_head_and_tail() {
        let secret = "sk-ant-api03-R2Dxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx-igAA";
        assert!(hint_matches("sk-ant-api03-R2D...igAA", secret));
        assert!(!hint_matches("sk-ant-api03-R2E...igAA", secret));
        assert!(!hint_matches("sk-ant-api03-R2D...igAB", secret));
        assert!(!hint_matches("sk-ant-api03-R2DigAA", secret));
        // 너무 짧은 앞뒤 글자로는 짝짓지 않는다.
        assert!(!hint_matches("sk...A", secret));
    }
    #[test]
    fn report_times_are_utc_rfc3339() {
        let t = OffsetDateTime::parse("2026-11-24T10:00:00Z", &Rfc3339).unwrap();
        assert_eq!(
            rfc3339(epoch(t.unix_timestamp()).unwrap()),
            "2026-11-24T10:00:00Z"
        );
    }
    #[test]
    fn a_minute_counts_for_the_convention_whose_requests_add_up_to_the_report() {
        // A는 앞 분에 보내 이 분에 끝났고, B는 이 분에 보내 다음 분에 끝났다.
        let a = request(
            1,
            M - 30_000,
            Some(M + 20_000),
            Known::Exact,
            [100, 0, 0, 10],
        );
        let b = request(
            2,
            M + 40_000,
            Some(M + 90_000),
            Known::Exact,
            [200, 0, 0, 20],
        );
        let requests = [a, b];
        // 보고서가 끝난 분 기준이면 이 분의 칸은 A다.
        let by_end = report(&[(M, [100, 0, 0, 10])]);
        assert_eq!(count_minutes(&by_end, &requests, M, M + MINUTE_MS), (0, 1));
        // 보낸 분 기준이면 B다.
        let by_start = report(&[(M, [200, 0, 0, 20])]);
        assert_eq!(
            count_minutes(&by_start, &requests, M, M + MINUTE_MS),
            (1, 0)
        );
        // 어느 쪽과도 맞지 않으면 세지 않는다.
        let neither = report(&[(M, [300, 0, 0, 30])]);
        assert_eq!(count_minutes(&neither, &requests, M, M + MINUTE_MS), (0, 0));
    }
    #[test]
    fn minutes_with_unknown_usage_or_requests_near_the_edge_are_not_counted() {
        let a = request(
            1,
            M - 30_000,
            Some(M + 20_000),
            Known::Exact,
            [100, 0, 0, 10],
        );
        let by_end = report(&[(M, [100, 0, 0, 10])]);
        // 같은 분에 걸친 끊긴 요청이 있으면 모른다.
        let cut = request(3, M + 10_000, Some(M + 50_000), Known::Cutoff, [5, 0, 0, 1]);
        assert_eq!(
            count_minutes(&by_end, &[a.clone(), cut], M, M + MINUTE_MS),
            (0, 0)
        );
        // 모델을 모르는 요청은 모든 모델의 칸을 막는다.
        let mut unknown = request(4, M + 10_000, None, Known::Unknown, [0; 4]);
        unknown.model = None;
        assert_eq!(
            count_minutes(&by_end, &[a.clone(), unknown], M, M + MINUTE_MS),
            (0, 0)
        );
        // 분 경계 3초 안에 끝난 요청이 있으면 쓰지 않는다.
        let edge = request(5, M - 50_000, Some(M + 1_000), Known::Exact, [7, 0, 0, 1]);
        assert_eq!(
            count_minutes(&by_end, &[a.clone(), edge], M, M + MINUTE_MS),
            (0, 0)
        );
        // 보고서가 돌려주지 않은 분은 세지 않는다.
        let mut missing = by_end.clone();
        missing.minutes.remove(&M);
        assert_eq!(count_minutes(&missing, &[a], M, M + MINUTE_MS), (0, 0));
    }
    #[test]
    fn a_cut_off_request_gets_the_rest_of_its_minute_only_with_the_known_convention() {
        // C는 이 분에 끊겼고(받은 출력 40), 같은 분에 끝난 A는 사용량을 안다.
        let c = request(
            1,
            M - 50_000,
            Some(M + 20_000),
            Known::Cutoff,
            [500, 0, 0, 40],
        );
        let a = request(
            2,
            M + 10_000,
            Some(M + 30_000),
            Known::Exact,
            [100, 0, 0, 10],
        );
        let requests = [c.clone(), a];
        let by_end = report(&[(M, [600, 0, 0, 910])]);
        let (output, evidence) = attribute(&by_end, &requests, Convention::End, &c).unwrap();
        assert_eq!(output, 900);
        assert_eq!(evidence["convention"], "end");
        assert_eq!(evidence["other_requests"], 1);
        // 입력이 맞지 않으면 고치지 않는다.
        let wrong = report(&[(M, [601, 0, 0, 910])]);
        assert_eq!(attribute(&wrong, &requests, Convention::End, &c), None);
        // 남은 출력은 max_tokens를 넘지 않는다.
        let big = report(&[(M, [600, 0, 0, 9_000])]);
        assert_eq!(
            attribute(&big, &requests, Convention::End, &c).map(|r| r.0),
            Some(4096)
        );
    }
    #[test]
    fn a_retry_with_the_same_prompt_is_not_mistaken_for_the_cut_off_request() {
        // 리뷰에서 찾은 경우: 보고서는 보낸 분 기준이다. C는 앞 분에 보내 이 분에 끊겼고,
        // 같은 프롬프트의 R은 이 분에 보내 다음 분에 끝났다. 이 분의 칸에는 R만 있다.
        let c = request(
            1,
            M - 40_000,
            Some(M + 20_000),
            Known::Cutoff,
            [500, 0, 0, 40],
        );
        let r = request(
            2,
            M + 25_000,
            Some(M + 100_000),
            Known::Exact,
            [500, 0, 0, 1800],
        );
        let requests = [c.clone(), r];
        let by_start = report(&[(M - MINUTE_MS, [500, 0, 0, 300]), (M, [500, 0, 0, 1800])]);
        // 기준을 보낸 분으로 알고 있으면 C의 칸(앞 분)에서 C의 실제 출력 300을 찾는다.
        assert_eq!(
            attribute(&by_start, &requests, Convention::Start, &c).map(|r| r.0),
            Some(300)
        );
        // 끝난 분 기준으로 잘못 알면 이 분의 칸(R)이 C의 것처럼 보인다. 그래서 기준은 데이터로 확인한 뒤에만 쓰고,
        // C처럼 사용량을 모르는 요청이 걸친 분은 기준 확인에도 쓰지 않는다.
        assert_eq!(
            attribute(&by_start, &requests, Convention::End, &c).map(|r| r.0),
            Some(1800)
        );
        assert_eq!(
            count_minutes(&by_start, &requests, M - MINUTE_MS, M + MINUTE_MS),
            (0, 0)
        );
    }
    #[test]
    fn candidates_near_a_minute_edge_or_beside_another_cut_off_are_left_alone() {
        let c = request(
            1,
            M - 50_000,
            Some(M + 3_000),
            Known::Cutoff,
            [500, 0, 0, 40],
        );
        let by_end = report(&[(M, [500, 0, 0, 900])]);
        assert_eq!(
            attribute(&by_end, std::slice::from_ref(&c), Convention::End, &c),
            None
        );
        let c = request(
            1,
            M - 50_000,
            Some(M + 20_000),
            Known::Cutoff,
            [500, 0, 0, 40],
        );
        let other = request(
            2,
            M - 10_000,
            Some(M + 40_000),
            Known::Cutoff,
            [100, 0, 0, 5],
        );
        let by_end = report(&[(M, [600, 0, 0, 905])]);
        assert_eq!(
            attribute(&by_end, &[c.clone(), other], Convention::End, &c),
            None
        );
    }
}
