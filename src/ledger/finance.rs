use super::{CurrentHuman, PgIdentityStore, amount::Amount};
use crate::config::{ApiKind, BillingClass, ModelProfile, ProviderId};
pub use m_gated_adapter_kit::adapter::UsageCategory;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Row, Transaction, postgres::PgRow};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerError {
    DatabaseUnavailable,
    ConfigurationConflict,
    DispatchUncertain,
    AccessDenied,
    OwnerInactive,
    PolicyChanged,
    Unsettled,
    InferencePaused,
    BudgetExhausted,
    RateLimited,
    Conflict,
    Expired,
    AmountOverflow,
    InvalidSnapshot,
    InvariantViolation,
}
impl From<sqlx::Error> for LedgerError {
    fn from(_: sqlx::Error) -> Self {
        Self::DatabaseUnavailable
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PriceRate {
    pub category: String,
    pub usd_per_million_tokens: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PricingSnapshot {
    pub schema_version: u8,
    pub pricing_version: String,
    pub provider: String,
    pub requested_model: String,
    pub api: String,
    pub allowed_actual_models: Vec<String>,
    pub alias_mapping: BTreeMap<String, String>,
    pub rates: Vec<PriceRate>,
    pub included_token_rules: String,
    pub cache_tier_rules: String,
    pub billing_time_rule: String,
    pub rounding_version: String,
    pub currency: String,
}
impl PricingSnapshot {
    pub fn from_profile(provider: ProviderId, profile: &ModelProfile) -> Self {
        Self {
            schema_version: 1,
            pricing_version: profile.pricing_version.clone(),
            provider: provider_name(provider).into(),
            requested_model: profile.model.clone(),
            api: api_name(profile.api).into(),
            allowed_actual_models: profile.actual_models.clone(),
            alias_mapping: BTreeMap::new(),
            rates: profile
                .rates
                .iter()
                .map(|(category, rate)| PriceRate {
                    category: category.clone(),
                    usd_per_million_tokens: rate.clone(),
                })
                .collect(),
            // 설정 검사를 통과한 profile에는 늘 어댑터가 있다. 없으면 장부가 snapshot을 거절한다.
            included_token_rules: crate::providers::port::api(provider, profile.api)
                .map_or("", |api| api.token_rule(profile))
                .into(),
            // 사용량이 칸을 모두 따로 알리므로 칸마다 그 칸의 단가를 쓴다(결정 0012).
            cache_tier_rules: "explicit_categories_v1".into(),
            billing_time_rule: "fixed_admission_v1".into(),
            rounding_version: "sum_half_up_12_v1".into(),
            currency: "USD".into(),
        }
    }
    pub(crate) fn validate(&self) -> Result<(), LedgerError> {
        let text = |s: &str, max: usize| {
            !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control)
        };
        if self.schema_version != 1
            || self.currency != "USD"
            || (self.provider == "openai"
                && self.api == "responses"
                && self.included_token_rules
                    != "openai_responses_input_includes_cache_output_includes_reasoning_v1")
            || (self.included_token_rules
                == "openai_responses_input_includes_cache_output_includes_reasoning_v1"
                && (self.provider != "openai" || self.api != "responses"))
            || (self.provider == "openai"
                && self.api == "chat_completions"
                && self.included_token_rules
                    != "openai_chat_prompt_includes_cache_completion_includes_reasoning_v1")
            || (self.included_token_rules
                == "openai_chat_prompt_includes_cache_completion_includes_reasoning_v1"
                && (self.provider != "openai" || self.api != "chat_completions"))
            || (self.provider == "openai"
                && self.api == "embeddings"
                && self.included_token_rules != "openai_embeddings_prompt_v1")
            || (self.included_token_rules == "openai_embeddings_prompt_v1"
                && (self.provider != "openai" || self.api != "embeddings"))
            || (self.api == "embeddings"
                && self.provider == "openai"
                && (self.rates.len() != 1 || self.rates[0].category != "input"))
            || (self.provider == "deepseek"
                && (self.rates.len() != 3
                    || !["input", "output", "cache_read"]
                        .iter()
                        .all(|category| self.rates.iter().any(|r| r.category == *category))
                    || self.cache_tier_rules != "explicit_categories_v1"
                    || self.api != "chat_completions"
                    || self.included_token_rules
                        != "deepseek_prompt_includes_hit_completion_includes_reasoning_v1"))
            || (self.included_token_rules
                == "deepseek_prompt_includes_hit_completion_includes_reasoning_v1"
                && self.provider != "deepseek")
            || ((self.provider == "typesafe")
                != (self.included_token_rules == "typesafe_input_output_v1"))
            || (self.provider == "typesafe"
                && (self.api != "systemone"
                    || self.rates.len() != 2
                    || !["input", "output"]
                        .iter()
                        .all(|category| self.rates.iter().any(|r| r.category == *category))
                    || self.cache_tier_rules != "explicit_categories_v1"))
            || !text(&self.pricing_version, 64)
            || !text(&self.requested_model, 200)
            || self.allowed_actual_models.is_empty()
            || self.allowed_actual_models.len() > 64
            || self.allowed_actual_models.iter().any(|s| !text(s, 200))
            || self
                .allowed_actual_models
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.allowed_actual_models.len()
            || !self.allowed_actual_models.contains(&self.requested_model)
            || self.alias_mapping.len() > 64
            || self
                .alias_mapping
                .iter()
                .any(|(a, b)| !text(a, 200) || !self.allowed_actual_models.contains(b))
            || self.rates.is_empty()
            || self.rates.len() > 8
            // 단가는 그 API가 알린 사용량 칸에만 둔다(결정 0012). 그래서 Anthropic에는 `cache_write`가 없다.
            || self.rates.iter().any(|r| {
                !UsageCategory::from_name(&r.category)
                    .is_some_and(|c| self.usage_categories().contains(&c))
                    || r.usd_per_million_tokens.parse::<Amount>().is_err()
            })
            || self
                .rates
                .iter()
                .map(|r| &r.category)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.rates.len()
            || ![
                "anthropic_input_excludes_cache_output_includes_thinking_v1",
                "anthropic_count_input_v1",
                "openai_responses_input_includes_cache_output_includes_reasoning_v1",
                "openai_chat_prompt_includes_cache_completion_includes_reasoning_v1",
                "openai_embeddings_prompt_v1",
                "deepseek_prompt_includes_hit_completion_includes_reasoning_v1",
                "typesafe_input_output_v1",
            ]
            .contains(&self.included_token_rules.as_str())
            || self.cache_tier_rules != "explicit_categories_v1"
            || self.billing_time_rule != "fixed_admission_v1"
            || self.rounding_version != "sum_half_up_12_v1"
        {
            return Err(LedgerError::InvalidSnapshot);
        }
        Ok(())
    }
    /// 이 snapshot의 API가 알린 사용량 칸(결정 0012). 모르는 Provider나 API면 빈 목록이다.
    pub fn usage_categories(&self) -> &'static [UsageCategory] {
        ProviderId::from_name(&self.provider)
            .and_then(|id| {
                crate::providers::port::provider(id)
                    .apis()
                    .iter()
                    .find(|a| api_name(a.kind()) == self.api)
            })
            .map_or(&[], |a| a.usage_categories())
    }
    /// 분당 토큰 한도와 기간 합계에 더할 (입력, 출력) 토큰.
    fn tokens(&self, u: &FinalUsage) -> Result<(i64, i64), LedgerError> {
        u.checked_tokens()
    }
    /// 장부의 `provider_usage`. Provider 응답의 사용량 모양으로 다시 쓴다.
    fn provider_usage(&self, u: &FinalUsage) -> Value {
        use UsageCategory::*;
        let (input, output, cache_read) = (u.get(Input), u.get(Output), u.get(CacheRead));
        // OpenAI와 DeepSeek의 입력 토큰에는 캐시에서 읽은 입력이 들어 있다.
        let prompt = input.checked_add(cache_read);
        if self.provider == "deepseek" {
            let mut selected = json!({"prompt_tokens":prompt,"completion_tokens":output,"total_tokens":prompt.and_then(|n|n.checked_add(output)),"prompt_cache_hit_tokens":cache_read,"prompt_cache_miss_tokens":input});
            if let Some(reasoning) = u.reasoning_tokens {
                selected["completion_tokens_details"] = json!({"reasoning_tokens":reasoning});
            }
            selected
        } else if self.provider == "openai" && self.api == "chat_completions" {
            json!({"prompt_tokens":prompt,"completion_tokens":output,"total_tokens":prompt.and_then(|n|n.checked_add(output)),"prompt_tokens_details":{"cached_tokens":cache_read},"completion_tokens_details":{"reasoning_tokens":u.reasoning_tokens}})
        } else if self.provider == "openai" && self.api == "embeddings" {
            json!({"prompt_tokens":input,"total_tokens":input})
        } else if self.included_token_rules == "typesafe_input_output_v1" {
            json!({"input_tokens":input,"output_tokens":output})
        } else if self.provider == "openai" {
            json!({"input_tokens":prompt,"output_tokens":output,"total_tokens":prompt.and_then(|n|n.checked_add(output)),"input_tokens_details":{"cached_tokens":cache_read},"output_tokens_details":{"reasoning_tokens":u.reasoning_tokens}})
        } else {
            anthropic_usage(&u.tokens, Some(output))
        }
    }
    /// 칸마다 토큰 수 × 그 칸의 단가를 더하고 한 번 반올림한다(결정 0012).
    /// 토큰이 있는 칸이 API가 알리지 않은 칸이거나 단가가 없으면 금액을 정하지 않는다(`unknown`). 0원으로 보지 않는다.
    pub fn price(&self, usage: &FinalUsage, reservation: bool) -> Result<Amount, LedgerError> {
        self.validate()?;
        if self.billing_time_rule != "fixed_admission_v1"
            || self.schema_version != 1
            || self.currency != "USD"
            || self.rounding_version != "sum_half_up_12_v1"
            || !self.allowed_actual_models.contains(&usage.actual_model)
        {
            return Err(LedgerError::InvalidSnapshot);
        }
        let declared = self.usage_categories();
        // 추론 토큰은 출력에 들어 있는 몫이다. 출력이 없는 API는 추론 토큰도 없다.
        if usage.reasoning_tokens.is_some_and(|n| {
            !declared.contains(&UsageCategory::Output) || n > usage.get(UsageCategory::Output)
        }) {
            return Err(LedgerError::InvalidSnapshot);
        }
        let mut items = Vec::new();
        for category in UsageCategory::ALL {
            let tokens = usage.tokens.get(category);
            // 입력과 출력 단가는 토큰이 0이어도 있어야 한다. 빠진 단가를 0원으로 짐작하지 않는다.
            let base = matches!(category, UsageCategory::Input | UsageCategory::Output)
                && declared.contains(&category);
            if tokens == 0 && !base {
                continue;
            }
            if !declared.contains(&category) {
                return Err(LedgerError::InvalidSnapshot);
            }
            let rate = self
                .rates
                .iter()
                .find(|r| r.category == category.name())
                .ok_or(LedgerError::InvalidSnapshot)?;
            items.push((
                tokens,
                rate.usd_per_million_tokens
                    .parse()
                    .map_err(|_| LedgerError::InvalidSnapshot)?,
            ));
        }
        Amount::price(items, reservation).map_err(|_| LedgerError::AmountOverflow)
    }
    /// 가격 계산 기록의 칸 목록. 단가가 있는 칸마다 토큰 수와 단가를 남긴다.
    fn resolution_categories(&self, usage: Option<&FinalUsage>) -> Vec<Value> {
        let Some(u) = usage else {
            return Vec::new();
        };
        self.rates
            .iter()
            .filter_map(|rate| {
                let category = UsageCategory::from_name(&rate.category)?;
                Some(json!({"category":rate.category,"tokens":u.get(category).to_string(),"rate":rate.usd_per_million_tokens}))
            })
            .collect()
    }
}
/// Anthropic 응답 모양의 사용량. 캐시 쓰기는 합계와 5분·1시간 몫을 함께 쓴다.
fn anthropic_usage(tokens: &UsageTokens, output: Option<u64>) -> Value {
    use UsageCategory::*;
    let get = |c| tokens.get(c);
    let mut usage = json!({"input_tokens":get(Input),"cache_read_input_tokens":get(CacheRead),"cache_creation_input_tokens":get(CacheWrite5m).checked_add(get(CacheWrite1h)),"cache_creation":{"ephemeral_5m_input_tokens":get(CacheWrite5m),"ephemeral_1h_input_tokens":get(CacheWrite1h)}});
    if let Some(output) = output {
        usage["output_tokens"] = json!(output);
    }
    usage
}
/// 장부의 사용량 기록(`normalized_usage`). 칸마다 `<칸>_tokens`를 남긴다(결정 0012).
/// `tokens`가 None이면 토큰을 모르는 것이고 칸의 값은 null이다. 없는 칸은 0이다.
pub(crate) fn usage_record(
    source: &str,
    actual_model: Option<&str>,
    tokens: Option<&UsageTokens>,
    reasoning_tokens: Option<u64>,
) -> Value {
    let mut record = json!({"source":source,"actual_model":actual_model});
    for c in UsageCategory::ALL {
        record[format!("{}_tokens", c.name())] = json!(tokens.map(|t| t.get(c).to_string()));
    }
    record["reasoning_tokens"] = json!(reasoning_tokens.map(|n| n.to_string()));
    record["counted_input_tokens"] = Value::Null;
    record
}
pub(crate) fn policy_limits(p: &PgRow) -> Result<super::policy::PolicyLimits, LedgerError> {
    use super::policy::{BudgetLimit, BudgetPeriod, PolicyLimits};
    let limits = PolicyLimits {
        budget: p
            .get::<Option<String>, _>("amount")
            .map(|limit_usd| BudgetLimit {
                limit_usd,
                period: BudgetPeriod {
                    kind: p.get("period_kind"),
                    timezone: p.get("period_timezone"),
                },
            }),
        rpm: p.get("rpm_limit"),
        tpm: p.get("tpm_limit"),
        count_rpm: p.get("count_rpm_limit"),
    };
    limits.validate()?;
    Ok(limits)
}
/// 권한의 기능 이름이 그 Provider가 가진 API 이름인지.
pub(crate) fn capability_allowed(provider: &str, capability: &str) -> bool {
    ProviderId::from_name(provider).is_some_and(|id| {
        crate::providers::port::provider(id)
            .apis()
            .iter()
            .any(|api| api_name(api.kind()) == capability)
    })
}
pub fn provider_name(p: ProviderId) -> &'static str {
    p.name()
}
pub fn api_name(a: ApiKind) -> &'static str {
    match a {
        ApiKind::Messages => "messages",
        ApiKind::CountTokens => "count_tokens",
        ApiKind::Responses => "responses",
        ApiKind::OpenaiChat | ApiKind::DeepseekChat => "chat_completions",
        ApiKind::Embeddings => "embeddings",
        ApiKind::Systemone => "systemone",
    }
}

/// 칸별 토큰 수(결정 0012). 칸마다 자리가 하나라서 한 칸은 한 번만 있다. 없는 칸은 0이다.
/// 고정 크기라서 스트림 감독자가 예약한 메모리 안에 들어간다.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageTokens([u64; 6]);
impl UsageTokens {
    pub fn get(&self, category: UsageCategory) -> u64 {
        self.0[category as usize]
    }
    pub fn set(&mut self, category: UsageCategory, tokens: u64) {
        self.0[category as usize] = tokens;
    }
    /// 토큰이 있는(0이 아닌) 칸. 칸 목록 순서다.
    pub fn iter(&self) -> impl Iterator<Item = (UsageCategory, u64)> + '_ {
        UsageCategory::ALL
            .into_iter()
            .map(|c| (c, self.get(c)))
            .filter(|(_, n)| *n > 0)
    }
    /// 관측 지문과 기록에 쓰는 모양(`{"input": n, ...}`). 0인 칸은 뺀다.
    fn projection(&self) -> BTreeMap<&'static str, u64> {
        self.iter().map(|(c, n)| (c.name(), n)).collect()
    }
}
impl FromIterator<(UsageCategory, u64)> for UsageTokens {
    fn from_iter<I: IntoIterator<Item = (UsageCategory, u64)>>(iter: I) -> Self {
        let mut tokens = Self::default();
        for (c, n) in iter {
            tokens.set(c, n);
        }
        tokens
    }
}
/// Anthropic 시작 event(`message_start`)가 알린 입력 usage. 끊긴 요청을 정산할 때 입력 토큰으로 쓴다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartUsage {
    pub actual_model: String,
    /// 입력과 캐시 칸의 토큰 수(결정 0012).
    pub tokens: UsageTokens,
}
/// 최종 usage 없이 끝난 요청을 어떻게 정산할지. 게이트웨이가 끝난 까닭을 보고 정한다(결정 0005).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum NoUsage {
    /// 사용량을 알 수 없다. 미정산으로 남기고 사용자의 과금 호출을 막는다.
    #[default]
    Unknown,
    /// 첫 응답 전에 끊겼거나 Provider가 HTTP 오류로 답했다. 0원으로 정산한다.
    NoResponse,
    /// Anthropic 스트림이 시작 event 뒤에 끊겼다. 받을 때 기록한 정산 규칙으로 정산한다.
    /// `received_output`은 받은 출력 내용을 센 토큰 수다. 세지 못했으면 None이고 `count_failure`에 까닭이 있다.
    Cutoff {
        start: StartUsage,
        received_output: Option<u64>,
        count_failure: Option<&'static str>,
    },
}
/// Provider가 알린 최종 사용량(결정 0012).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalUsage {
    pub actual_model: String,
    /// 칸별 토큰 수. 없는 칸은 0이다.
    pub tokens: UsageTokens,
    /// 추론 토큰. `output`에 들어 있고 따로 값을 매기지 않는다.
    pub reasoning_tokens: Option<u64>,
}
impl FinalUsage {
    /// 본체 어댑터가 만든다. 한 칸은 한 번만 넘긴다.
    pub fn new(
        actual_model: impl Into<String>,
        tokens: impl IntoIterator<Item = (UsageCategory, u64)>,
        reasoning_tokens: Option<u64>,
    ) -> Self {
        Self {
            actual_model: actual_model.into(),
            tokens: tokens.into_iter().collect(),
            reasoning_tokens,
        }
    }
    /// 어댑터 crate가 알린 사용량. 같은 칸이 두 번 나오면 형식 오류(None)다.
    pub fn from_list(
        actual_model: String,
        tokens: Vec<(UsageCategory, u64)>,
        reasoning_tokens: Option<u64>,
    ) -> Option<Self> {
        let unique: std::collections::BTreeSet<_> = tokens.iter().map(|(c, _)| *c).collect();
        (unique.len() == tokens.len()).then(|| Self::new(actual_model, tokens, reasoning_tokens))
    }
    /// 한 칸의 토큰 수. 없으면 0이다.
    pub fn get(&self, category: UsageCategory) -> u64 {
        self.tokens.get(category)
    }
    fn selected(&self) -> Value {
        usage_record(
            "provider",
            Some(&self.actual_model),
            Some(&self.tokens),
            self.reasoning_tokens,
        )
    }
    /// 관측 지문에 넣는 모양.
    fn projection(&self) -> Value {
        json!({"actual_model":self.actual_model,"tokens":self.tokens.projection(),"reasoning_tokens":self.reasoning_tokens})
    }
    /// (입력, 출력) 토큰. 입력은 `input`과 모든 캐시 칸의 합이고 출력은 `output`이다.
    fn checked_tokens(&self) -> Result<(i64, i64), LedgerError> {
        let mut input = 0u64;
        for (category, n) in self.tokens.iter() {
            if category != UsageCategory::Output {
                input = input.checked_add(n).ok_or(LedgerError::AmountOverflow)?;
            }
        }
        let input = i64::try_from(input).map_err(|_| LedgerError::AmountOverflow)?;
        let output = i64::try_from(self.get(UsageCategory::Output))
            .map_err(|_| LedgerError::AmountOverflow)?;
        input
            .checked_add(output)
            .ok_or(LedgerError::AmountOverflow)?;
        Ok((input, output))
    }
}
// 정상/늦은 final은 같은 고정 snapshot의 금액·token 상한과 overflow 우선순위를 쓴다.
fn final_safety_violation(
    usage: &FinalUsage,
    price: &Result<Amount, LedgerError>,
    execution: &Value,
) -> Result<Option<&'static str>, LedgerError> {
    let tokens = usage.checked_tokens();
    if *price == Err(LedgerError::AmountOverflow) || tokens.is_err() {
        return Ok(Some("amount_overflow"));
    }
    let (input, output) = tokens?;
    let cost_bound = execution["cost_bound_usd"]
        .as_str()
        .map(parse_amount)
        .transpose()?;
    if price
        .as_ref()
        .ok()
        .zip(cost_bound)
        .is_some_and(|(cost, bound)| *cost > bound)
        || execution["input_bound_tokens"]
            .as_u64()
            .is_some_and(|bound| input as u64 > bound)
        || execution["output_bound_tokens"]
            .as_u64()
            .is_some_and(|bound| output as u64 > bound)
    {
        Ok(Some("cost_bound_violation"))
    } else {
        Ok(None)
    }
}
#[derive(Clone, Copy)]
pub struct KeyScope {
    pub id: Uuid,
    pub version: i64,
}
pub struct RequestSpec {
    pub id: Uuid,
    pub provider: ProviderId,
    pub api: ApiKind,
    pub pricing: PricingSnapshot,
    pub profile_version: String,
    pub billing_class: BillingClass,
    pub output_limit: Option<u64>,
    pub stream: bool,
    pub api_version: Option<String>,
    pub approved_beta: Vec<String>,
    pub cost_bound: Option<Amount>,
    pub deadline: crate::runtime::RequestDeadline,
    /// 이 요청이 고정한 회사 키의 DB id. 테스트 고정 키·이관 요청은 None이다.
    pub provider_credential_id: Option<Uuid>,
}
impl RequestSpec {
    pub fn from_profile(
        provider: ProviderId,
        profile: &ModelProfile,
        output_limit: Option<u64>,
    ) -> Self {
        let observed = tokio::time::Instant::now();
        Self {
            id: Uuid::new_v4(),
            provider,
            api: profile.api,
            pricing: PricingSnapshot::from_profile(provider, profile),
            profile_version: profile.profile_version.clone(),
            billing_class: profile.billing_class,
            output_limit,
            stream: false,
            api_version: None,
            approved_beta: Vec::new(),
            cost_bound: if profile.billing_class == BillingClass::GuaranteedFree {
                Some(Amount::ZERO)
            } else {
                let input_rate = profile
                    .rates
                    .iter()
                    .filter(|(k, _)| k.as_str() != "output")
                    .filter_map(|(_, r)| r.parse::<Amount>().ok())
                    .max();
                let output_rate = profile
                    .rates
                    .get("output")
                    .and_then(|r| r.parse::<Amount>().ok())
                    .unwrap_or(Amount::ZERO);
                input_rate.and_then(|rate| {
                    Amount::price(
                        [
                            (profile.max_input_tokens, rate),
                            (output_limit.unwrap_or(0), output_rate),
                        ],
                        true,
                    )
                    .ok()
                })
            },
            provider_credential_id: None,
            deadline: crate::runtime::RequestDeadline::new(
                observed + Duration::from_secs(895),
                observed + Duration::from_secs(900),
            ),
        }
    }
}
/// 새 dispatch commit에 성공한 호출자만 소유하는 한 번뿐인 송신 권한.
pub struct SendPermit {
    request_id: Uuid,
}
impl SendPermit {
    pub(crate) fn consume(self) -> Uuid {
        self.request_id
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinalOutcome {
    Completed,
    Rejected,
    Failed,
    Cancelled,
}
impl FinalOutcome {
    fn name(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Rejected => "rejected",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}
#[derive(Clone, Debug)]
pub struct FinalObservation {
    pub outcome: FinalOutcome,
    pub usage: Option<FinalUsage>,
    pub counted_input_tokens: Option<u64>,
    pub reason: &'static str,
    pub http_status: Option<u16>,
    pub provider_request_id: Option<String>,
}
impl FinalObservation {
    pub fn completed(usage: FinalUsage) -> Self {
        Self {
            outcome: FinalOutcome::Completed,
            usage: Some(usage),
            counted_input_tokens: None,
            reason: "completed",
            http_status: Some(200),
            provider_request_id: None,
        }
    }
    fn digest(&self) -> Vec<u8> {
        Sha256::digest(serde_json::to_vec(&json!({"outcome":self.outcome.name(),"usage":self.usage.as_ref().map(FinalUsage::projection),"counted_input_tokens":self.counted_input_tokens,"reason":self.reason,"status":self.http_status,"provider_request_id":self.provider_request_id})).expect("유한 typed projection")).to_vec()
    }
    /// 같은 관측이라도 usage 없이 정산한 방법이 다르면 다른 마감이다.
    fn digest_with(&self, settlement: &NoUsage) -> Vec<u8> {
        let way = match settlement {
            NoUsage::Unknown => return self.digest(),
            NoUsage::NoResponse => json!({"kind":"no_response"}),
            NoUsage::Cutoff {
                start,
                received_output,
                count_failure,
            } => {
                json!({"kind":"cutoff","model":start.actual_model,"tokens":start.tokens.projection(),"received_output":received_output,"count_failure":count_failure})
            }
        };
        let mut bytes = self.digest();
        bytes.extend(serde_json::to_vec(&way).expect("유한 typed projection"));
        Sha256::digest(bytes).to_vec()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finalized {
    Settled,
    Unknown,
    Noop,
}

pub struct PgLedger {
    database: Arc<PgIdentityStore>,
    executor_id: Uuid,
    auth_digest: [u8; 32],
    ceiling: super::policy::PolicyLimits,
}
impl PgLedger {
    /// identity와 금융이 같은 pool8/ordinary6을 공유해 마감용 연결 두 개를 남긴다.
    pub fn new(database: Arc<PgIdentityStore>, auth_digest: [u8; 32]) -> Self {
        Self {
            database,
            executor_id: Uuid::new_v4(),
            auth_digest,
            ceiling: Default::default(),
        }
    }
    pub fn with_ceiling(mut self, ceiling: super::policy::PolicyLimits) -> Self {
        self.ceiling = ceiling;
        self
    }
    pub fn database(&self) -> &Arc<PgIdentityStore> {
        &self.database
    }
    /// 이 프로세스의 실행자 ID. 시스템 감사 기록에 남긴다.
    pub(crate) fn executor_id(&self) -> Uuid {
        self.executor_id
    }
    /// 실행 설정의 지문. 감사 기록에 남긴다.
    pub(crate) fn auth_digest(&self) -> [u8; 32] {
        self.auth_digest
    }
    pub async fn admit_request(
        &self,
        human: &CurrentHuman,
        spec: &RequestSpec,
    ) -> Result<Uuid, LedgerError> {
        self.admit_scoped(human, spec, None, None).await
    }
    pub async fn admit_scoped(
        &self,
        human: &CurrentHuman,
        spec: &RequestSpec,
        key: Option<KeyScope>,
        parent: Option<Uuid>,
    ) -> Result<Uuid, LedgerError> {
        let _gate = self
            .database
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| LedgerError::DatabaseUnavailable)?;
        let attempt = async {
            for n in 0..3 {
                let result = self.admit(human, spec, key, parent).await;
                if !matches!(result, Err(LedgerError::Expired))
                    || n == 2
                    || spec
                        .deadline
                        .remaining_at(tokio::time::Instant::now())
                        .is_none()
                {
                    return result;
                }
            }
            Err(LedgerError::Expired)
        };
        tokio::time::timeout(Duration::from_secs(3), attempt)
            .await
            .map_err(|_| LedgerError::DatabaseUnavailable)?
    }
    async fn admit(
        &self,
        human: &CurrentHuman,
        spec: &RequestSpec,
        key: Option<KeyScope>,
        parent: Option<Uuid>,
    ) -> Result<Uuid, LedgerError> {
        if spec.pricing.provider != provider_name(spec.provider)
            || spec.pricing.api != api_name(spec.api)
            || spec.pricing.currency != "USD"
            || spec.pricing.schema_version != 1
            || (spec.billing_class == BillingClass::GuaranteedFree
                && !crate::providers::port::api(spec.provider, spec.api)
                    .is_some_and(|api| api.may_be_free()))
        {
            return Err(LedgerError::InvalidSnapshot);
        }
        if spec
            .deadline
            .remaining_at(tokio::time::Instant::now())
            .is_none()
        {
            return Err(LedgerError::Expired);
        }
        spec.pricing.validate()?;
        if spec.billing_class == BillingClass::GuaranteedFree
            && spec
                .pricing
                .rates
                .iter()
                .any(|r| r.usd_per_million_tokens != "0.000000000000")
        {
            return Err(LedgerError::InvalidSnapshot);
        }
        if spec.cost_bound.is_none() {
            return Err(LedgerError::AmountOverflow);
        }
        let mut tx = self.database.begin_transaction().await?;
        let principal = self.lock_principal(&mut tx, human.id).await?;
        if principal.get::<i64, _>("version") != human.version {
            return Err(LedgerError::PolicyChanged);
        }
        if let Some(key) = key {
            self.lock_key(&mut tx, human.id, key, spec).await?;
        }
        let mut policies=sqlx::query("SELECT *,budget_limit_usd::text AS amount FROM gw_policies WHERE principal_id=$1 AND (key_id IS NULL OR key_id=$2) ORDER BY id FOR UPDATE")
            .bind(human.id).bind(key.map(|k|k.id)).fetch_all(&mut *tx).await?;
        if policies.len() != if key.is_some() { 2 } else { 1 } {
            return Err(LedgerError::InvalidSnapshot);
        }
        if policies
            .iter()
            .find(|p| p.get::<Option<Uuid>, _>("key_id").is_none())
            .is_none_or(|p| p.get::<i64, _>("version") != human.policy_version)
        {
            return Err(LedgerError::PolicyChanged);
        }
        for policy in &policies {
            self.check_grant(policy, spec)?;
        }
        policies.sort_by_key(|p| p.get::<Uuid, _>("id"));
        // 끊긴 요청의 정산 규칙은 받을 때 정해 실행 기록에 남긴다. 시작 event가 입력 토큰을 알려 주는
        // API의 스트림만 쓴다.
        let cutoff_rule: Option<String> = if crate::providers::port::api(spec.provider, spec.api)
            .is_some_and(|api| api.settles_cutoff())
            && spec.stream
            && spec.billing_class != BillingClass::GuaranteedFree
        {
            sqlx::query_scalar("SELECT cutoff_rule FROM gw_settlement_rules WHERE provider=$1")
                .bind(provider_name(spec.provider))
                .fetch_optional(&mut *tx)
                .await?
        } else {
            None
        };
        let provisional: String = sqlx::query_scalar("SELECT transaction_timestamp()::text")
            .fetch_one(&mut *tx)
            .await?;
        let lane = if spec.api == ApiKind::CountTokens {
            "token_count"
        } else {
            "inference"
        };
        let mut attributions = Vec::new();
        for policy in &policies {
            let stored = policy_limits(policy)?;
            let mut ceiling = self.ceiling.clone();
            if spec.billing_class == BillingClass::GuaranteedFree {
                ceiling.budget = None;
            }
            let effective = stored.effective(&ceiling)?;
            let scope: Uuid = policy.get("id");
            // 예산 기간은 관리자가 정한 예산 시간대 하나로 정한다(정책마다 따로 두지 않는다).
            let period=sqlx::query(&format!("SELECT b.period_start::text AS start,b.period_end::text AS finish,b.timezone,to_timestamp(floor(extract(epoch FROM $1::timestamptz)/60)*60)::text AS rate FROM {} b", super::settings::budget_period_sql("$1::timestamptz")))
                .bind(&provisional).fetch_one(&mut *tx).await?;
            let timezone: String = period.get("timezone");
            let start: String = period.get("start");
            let end: String = period.get("finish");
            let rate: String = period.get("rate");
            sqlx::query("INSERT INTO gw_usage_totals(scope_id,period_start,period_end,timezone_snapshot) VALUES($1,$2::timestamptz,$3::timestamptz,$4) ON CONFLICT DO NOTHING").bind(scope).bind(&start).bind(&end).bind(&timezone).execute(&mut *tx).await?;
            sqlx::query("SELECT scope_id FROM gw_usage_totals WHERE scope_id=$1 AND period_start=$2::timestamptz FOR UPDATE").bind(scope).bind(&start).fetch_one(&mut *tx).await?;
            attributions.push((scope, timezone, start, end, rate, stored, effective));
        }
        for (scope, _, _, _, rate, _, _) in &attributions {
            sqlx::query("INSERT INTO gw_rate_windows(scope_id,lane,window_start,window_end) VALUES($1,$2,$3::timestamptz,$3::timestamptz+interval '60 seconds') ON CONFLICT DO NOTHING").bind(scope).bind(lane).bind(rate).execute(&mut *tx).await?;
            sqlx::query("SELECT scope_id FROM gw_rate_windows WHERE scope_id=$1 AND lane=$2 AND window_start=$3::timestamptz FOR UPDATE").bind(scope).bind(lane).bind(rate).fetch_one(&mut *tx).await?;
        }
        if let Some(parent) = parent {
            let root = sqlx::query("SELECT *,admit_expires_at>clock_timestamp() AS unexpired FROM gw_requests WHERE id=$1 FOR UPDATE").bind(parent).fetch_one(&mut *tx).await?;
            if root.get::<String, _>("phase") != "admitted"
                || root.get::<String, _>("purpose") != "root"
                || root.get::<Uuid, _>("principal_id") != human.id
                || root.get::<Option<Uuid>, _>("key_id") != key.map(|k| k.id)
                || !root.get::<bool, _>("unexpired")
                || !crate::providers::port::api(spec.provider, spec.api)
                    .is_some_and(|api| api.may_be_free())
                || root.get::<String, _>("requested_model") != spec.pricing.requested_model
            {
                return Err(LedgerError::Conflict);
            }
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM gw_requests WHERE parent_request_id=$1)",
            )
            .bind(parent)
            .fetch_one(&mut *tx)
            .await?;
            if exists {
                return Err(LedgerError::Conflict);
            }
        }
        let existing: Option<Uuid> = sqlx::query_scalar("SELECT id FROM gw_requests WHERE id=$1")
            .bind(spec.id)
            .fetch_optional(&mut *tx)
            .await?;
        if existing.is_some() {
            return Err(LedgerError::Conflict);
        }
        let anchor: String = sqlx::query_scalar("SELECT clock_timestamp()::text")
            .fetch_one(&mut *tx)
            .await?;
        let (transport_ms, execution_ms) = spec
            .deadline
            .remaining_at(tokio::time::Instant::now())
            .ok_or(LedgerError::Expired)?;
        for (scope, _, start, end, rate, _, effective) in &attributions {
            let in_window:bool=sqlx::query_scalar("SELECT $1::timestamptz >= $2::timestamptz AND $1::timestamptz < $3::timestamptz AND $1::timestamptz >= $4::timestamptz AND $1::timestamptz < $4::timestamptz+interval '60 seconds'").bind(&anchor).bind(start).bind(end).bind(rate).fetch_one(&mut *tx).await?;
            if !in_window {
                return Err(LedgerError::Expired);
            }
            let admitted:i64=sqlx::query_scalar("SELECT admitted_count FROM gw_rate_windows WHERE scope_id=$1 AND lane=$2 AND window_start=$3::timestamptz").bind(scope).bind(lane).bind(rate).fetch_one(&mut *tx).await?;
            if (if lane == "inference" {
                effective.rpm
            } else {
                effective.count_rpm
            })
            .is_some_and(|n| admitted >= n)
            {
                return Err(LedgerError::RateLimited);
            }
            admitted
                .checked_add(1)
                .ok_or(LedgerError::InvariantViolation)?;
        }
        let mut dependencies =
            vec![json!({"id":human.id.to_string(),"version":human.version.to_string()})];
        if let Some(owner) = principal.get::<Option<Uuid>, _>("owner_principal_id") {
            let version: i64 = sqlx::query_scalar("SELECT version FROM gw_principals WHERE id=$1")
                .bind(owner)
                .fetch_one(&mut *tx)
                .await?;
            dependencies.push(json!({"id":owner.to_string(),"version":version.to_string()}));
        }
        let execution = json!({"schema_version":1,"profile_version":spec.profile_version,"billing_class":if spec.billing_class==BillingClass::GuaranteedFree {"guaranteed_free"}else{"chargeable"},"capability":api_name(spec.api),"stream":spec.stream,"output_limit":spec.output_limit,"input_bound_tokens":null,"output_bound_tokens":spec.output_limit,"cost_bound_usd":spec.cost_bound.map(|n|n.to_string()),"auth_dependency_versions":{"principals":dependencies,"target_inference_safety_version":principal.get::<i64,_>("inference_safety_version").to_string(),"key":key.map(|k|json!({"id":k.id.to_string(),"version":k.version.to_string()})),"policies":policies.iter().map(|p|json!({"id":p.get::<Uuid,_>("id").to_string(),"version":p.get::<i64,_>("version").to_string()})).collect::<Vec<_>>()},"api_version":spec.api_version,"approved_beta":spec.approved_beta,"cutoff_rule":cutoff_rule});
        let execution = read_execution_snapshot(execution)?;
        sqlx::query("INSERT INTO gw_requests(id,principal_id,key_id,parent_request_id,purpose,provider,requested_model,api,admitted_at,admit_expires_at,transport_deadline,execution_deadline,executor_id,phase,accounting_state,pricing_snapshot,execution_snapshot,auth_config_digest,provider_credential_id) VALUES($1,$2,$13,$14,$15,$3,$4,$5,$6::timestamptz,least($6::timestamptz+interval '60 seconds',$6::timestamptz+$11::bigint*interval '1 millisecond',coalesce((SELECT admit_expires_at FROM gw_requests WHERE id=$14),$6::timestamptz+interval '60 seconds')),least($6::timestamptz+$11::bigint*interval '1 millisecond',coalesce((SELECT transport_deadline FROM gw_requests WHERE id=$14),$6::timestamptz+$11::bigint*interval '1 millisecond')),least($6::timestamptz+$12::bigint*interval '1 millisecond',coalesce((SELECT execution_deadline FROM gw_requests WHERE id=$14),$6::timestamptz+$12::bigint*interval '1 millisecond')),$7,'admitted','pending',$8,$9,$10,$16)")
            .bind(spec.id).bind(human.id).bind(provider_name(spec.provider)).bind(&spec.pricing.requested_model).bind(api_name(spec.api)).bind(&anchor).bind(self.executor_id).bind(serde_json::to_value(&spec.pricing).map_err(|_|LedgerError::InvalidSnapshot)?).bind(execution).bind(self.auth_digest.as_slice()).bind(transport_ms as i64).bind(execution_ms as i64).bind(key.map(|k|k.id)).bind(parent).bind(if parent.is_some(){"internal_count"}else{"root"}).bind(spec.provider_credential_id).execute(&mut *tx).await?;
        for (scope, timezone, start, end, rate, stored, _) in attributions {
            let policy = policies
                .iter()
                .find(|p| p.get::<Uuid, _>("id") == scope)
                .ok_or(LedgerError::InvariantViolation)?;
            let snapshot = json!({"scope_id":scope.to_string(),"version":policy.get::<i64,_>("version").to_string(),"grants":policy.get::<Value,_>("allow_rules"),"limits":stored});
            sqlx::query("UPDATE gw_rate_windows SET admitted_count=admitted_count+1 WHERE scope_id=$1 AND lane=$2 AND window_start=$3::timestamptz").bind(scope).bind(lane).bind(&rate).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO gw_request_budgets(request_id,scope_id,policy_version,policy_snapshot,period_start,period_end,timezone_snapshot,rate_lane,rate_start,rate_end,state) VALUES($1,$2,$3,$4,$5::timestamptz,$6::timestamptz,$7,$8,$9::timestamptz,$9::timestamptz+interval '60 seconds','admitted')")
                .bind(spec.id).bind(scope).bind(policy.get::<i64,_>("version")).bind(snapshot).bind(start).bind(end).bind(timezone).bind(lane).bind(rate).execute(&mut *tx).await?;
        }
        if spec
            .deadline
            .remaining_at(tokio::time::Instant::now())
            .is_none()
        {
            return Err(LedgerError::Expired);
        }
        tx.commit().await?;
        Ok(spec.id)
    }
    async fn lock_key(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        principal: Uuid,
        key: KeyScope,
        spec: &RequestSpec,
    ) -> Result<(), LedgerError> {
        let row=sqlx::query("SELECT *,revoked_at IS NULL AND (expires_at IS NULL OR expires_at>clock_timestamp()) AS active FROM gw_virtual_keys WHERE id=$1 AND principal_id=$2 FOR UPDATE").bind(key.id).bind(principal).fetch_optional(&mut **tx).await?.ok_or(LedgerError::AccessDenied)?;
        if !row.get::<bool, _>("active") {
            return Err(LedgerError::AccessDenied);
        }
        if row.get::<i64, _>("version") != key.version {
            return Err(LedgerError::PolicyChanged);
        }
        if !read_grants(row.get("grant_snapshot"))?.iter().any(|g| {
            g.provider == provider_name(spec.provider)
                && g.model == spec.pricing.requested_model
                && g.capabilities.iter().any(|a| a == api_name(spec.api))
        }) {
            return Err(LedgerError::AccessDenied);
        }
        Ok(())
    }
    async fn lock_principal(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
    ) -> Result<PgRow, LedgerError> {
        let owner: Option<Uuid> =
            sqlx::query_scalar("SELECT owner_principal_id FROM gw_principals WHERE id=$1")
                .bind(id)
                .fetch_one(&mut **tx)
                .await?;
        let mut ids = vec![id];
        if let Some(owner) = owner {
            ids.push(owner);
        }
        ids.sort();
        ids.dedup();
        for id in ids {
            sqlx::query("SELECT id FROM gw_principals WHERE id=$1 FOR UPDATE")
                .bind(id)
                .fetch_one(&mut **tx)
                .await?;
        }
        let p = sqlx::query("SELECT * FROM gw_principals WHERE id=$1")
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
        if p.get::<Option<Uuid>, _>("owner_principal_id") != owner {
            return Err(LedgerError::PolicyChanged);
        }
        self.check_principal_access(tx, &p).await?;
        Ok(p)
    }
    async fn check_principal_access(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        p: &PgRow,
    ) -> Result<(), LedgerError> {
        if p.get::<Option<String>, _>("disabled_reason").is_some()
            || p.get::<Option<String>, _>("security_fence_ref").is_some()
        {
            return Err(LedgerError::AccessDenied);
        }
        if p.get::<String, _>("kind") == "human" {
            if !super::management::active_human(tx, p).await? {
                return Err(LedgerError::AccessDenied);
            }
        } else {
            let owner = sqlx::query("SELECT * FROM gw_principals WHERE id=$1")
                .bind(p.get::<Option<Uuid>, _>("owner_principal_id"))
                .fetch_one(&mut **tx)
                .await?;
            if !super::management::active_human(tx, &owner).await? {
                return Err(LedgerError::OwnerInactive);
            }
        }
        Ok(())
    }
    fn check_grant(&self, policy: &PgRow, spec: &RequestSpec) -> Result<(), LedgerError> {
        let rules = read_grants(policy.get("allow_rules"))?;
        if rules.iter().any(|g| {
            g.provider == provider_name(spec.provider)
                && g.model == spec.pricing.requested_model
                && g.capabilities.iter().any(|a| a == api_name(spec.api))
        }) {
            Ok(())
        } else {
            Err(LedgerError::AccessDenied)
        }
    }
}

impl PgLedger {
    // 선조회는 불변 관계만 수집하며, 쓰기 잠금은 공통 순서로만 획득한다.
    pub(super) async fn lock_request(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
    ) -> Result<(PgRow, PgRow, Vec<PgRow>), LedgerError> {
        self.lock_request_mode(tx, id, false).await
    }
    async fn lock_request_mode(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
        nowait: bool,
    ) -> Result<(PgRow, PgRow, Vec<PgRow>), LedgerError> {
        let mut metric = crate::telemetry::Timer::start(
            crate::telemetry::Metric::DbLock,
            crate::telemetry::Metric::DbLockMicros,
            crate::telemetry::Metric::DbLockFailed,
        );
        let principal: Uuid =
            sqlx::query_scalar("SELECT principal_id FROM gw_requests WHERE id=$1")
                .bind(id)
                .fetch_one(&mut **tx)
                .await?;
        let owner: Option<Uuid> =
            sqlx::query_scalar("SELECT owner_principal_id FROM gw_principals WHERE id=$1")
                .bind(principal)
                .fetch_one(&mut **tx)
                .await?;
        let mut ids = vec![principal];
        if let Some(owner) = owner {
            ids.push(owner);
        }
        ids.sort();
        ids.dedup();
        for related in ids {
            let query = if nowait {
                "SELECT id FROM gw_principals WHERE id=$1 FOR UPDATE NOWAIT"
            } else {
                "SELECT id FROM gw_principals WHERE id=$1 FOR UPDATE"
            };
            sqlx::query(query)
                .bind(related)
                .fetch_one(&mut **tx)
                .await
                .map_err(|e| {
                    if e.as_database_error()
                        .is_some_and(|e| e.code().as_deref() == Some("55P03"))
                    {
                        LedgerError::Conflict
                    } else {
                        LedgerError::from(e)
                    }
                })?;
        }
        let p = sqlx::query(
            "SELECT *,disabled_at IS NOT NULL AS disabled FROM gw_principals WHERE id=$1",
        )
        .bind(principal)
        .fetch_one(&mut **tx)
        .await?;
        if p.get::<Option<Uuid>, _>("owner_principal_id") != owner {
            return Err(LedgerError::PolicyChanged);
        }
        let key: Option<Uuid> = sqlx::query_scalar("SELECT key_id FROM gw_requests WHERE id=$1")
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
        if let Some(key) = key {
            sqlx::query("SELECT id FROM gw_virtual_keys WHERE id=$1 FOR UPDATE")
                .bind(key)
                .fetch_one(&mut **tx)
                .await?;
        }
        sqlx::query("SELECT p.id FROM gw_policies p JOIN gw_request_budgets b ON b.scope_id=p.id WHERE b.request_id=$1 ORDER BY p.id FOR UPDATE OF p").bind(id).fetch_all(&mut **tx).await?;
        sqlx::query("SELECT t.scope_id FROM gw_usage_totals t JOIN gw_request_budgets b ON b.scope_id=t.scope_id AND b.period_start=t.period_start WHERE b.request_id=$1 ORDER BY t.scope_id,t.period_start FOR UPDATE OF t").bind(id).fetch_all(&mut **tx).await?;
        sqlx::query("SELECT w.scope_id FROM gw_rate_windows w JOIN gw_request_budgets b ON b.scope_id=w.scope_id AND b.rate_lane=w.lane AND b.rate_start=w.window_start WHERE b.request_id=$1 ORDER BY w.scope_id,w.lane,w.window_start FOR UPDATE OF w").bind(id).fetch_all(&mut **tx).await?;
        let _request = sqlx::query("SELECT id FROM gw_requests WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
        let budgets=sqlx::query("SELECT b.*,b.period_start::text AS start_text,b.rate_start::text AS rate_text,p.version AS current_version,p.budget_limit_usd::text AS budget_limit,t.cost_usd::text AS total_cost,t.input_tokens AS total_input_tokens,t.output_tokens AS total_output_tokens,t.settled_count AS total_settled_count,t.settled_unknown_tokens_count AS total_unknown_tokens_count,t.unknown_count AS total_unknown_count,w.settled_tokens AS rate_settled_tokens FROM gw_request_budgets b JOIN gw_policies p ON p.id=b.scope_id JOIN gw_usage_totals t ON t.scope_id=b.scope_id AND t.period_start=b.period_start JOIN gw_rate_windows w ON w.scope_id=b.scope_id AND w.lane=b.rate_lane AND w.window_start=b.rate_start WHERE b.request_id=$1 ORDER BY b.scope_id FOR UPDATE OF b").bind(id).fetch_all(&mut **tx).await?;
        if budgets.is_empty() {
            return Err(LedgerError::InvalidSnapshot);
        }
        let r=sqlx::query("WITH n AS MATERIALIZED (SELECT clock_timestamp() AS t) SELECT r.*,r.admit_expires_at<=n.t AS admit_expired,r.lease_expires_at<=n.t AS lease_expired,n.t::text AS db_now FROM gw_requests r CROSS JOIN n WHERE r.id=$1").bind(id).fetch_one(&mut **tx).await?;
        metric.success();
        Ok((p, r, budgets))
    }
    pub async fn authorize_dispatch(&self, id: Uuid) -> Result<SendPermit, LedgerError> {
        let _gate = self
            .database
            .gate
            .clone()
            .try_acquire_owned()
            .map_err(|_| LedgerError::DatabaseUnavailable)?;
        // timeout 동안 commit이 끝났을 수 있다. 성공을 확인하지 못하면 permit은 절대 생성하지 않는다.
        tokio::time::timeout(Duration::from_secs(3), self.authorize(id))
            .await
            .map_err(|_| LedgerError::DispatchUncertain)?
    }
    async fn authorize(&self, id: Uuid) -> Result<SendPermit, LedgerError> {
        let mut tx = self.database.begin_transaction().await?;
        let (p, r, budgets) = self.lock_request(&mut tx, id).await?;
        if r.get::<String, _>("phase") != "admitted"
            || r.get::<Uuid, _>("executor_id") != self.executor_id
        {
            return Err(LedgerError::Conflict);
        }
        if r.get::<bool, _>("admit_expired") {
            return Err(LedgerError::Expired);
        }
        self.check_principal_access(&mut tx, &p).await?;
        let execution = read_execution_snapshot(r.get("execution_snapshot"))?;
        if execution["capability"].as_str() != Some(r.get::<String, _>("api").as_str())
            || execution["auth_dependency_versions"]["principals"][0]["id"].as_str()
                != Some(p.get::<Uuid, _>("id").to_string().as_str())
        {
            return Err(LedgerError::PolicyChanged);
        }
        let deps = execution["auth_dependency_versions"]["principals"]
            .as_array()
            .ok_or(LedgerError::InvalidSnapshot)?;
        let owner = p.get::<Option<Uuid>, _>("owner_principal_id");
        if deps.len() != if owner.is_some() { 2 } else { 1 } {
            return Err(LedgerError::PolicyChanged);
        }
        if let Some(owner) = owner {
            let current: i64 = sqlx::query_scalar("SELECT version FROM gw_principals WHERE id=$1")
                .bind(owner)
                .fetch_one(&mut *tx)
                .await?;
            if deps[1]["id"].as_str().and_then(|s| Uuid::parse_str(s).ok()) != Some(owner)
                || deps[1]["version"]
                    .as_str()
                    .and_then(|s| s.parse::<i64>().ok())
                    != Some(current)
            {
                return Err(LedgerError::PolicyChanged);
            }
        }
        if let Some(key) = r.get::<Option<Uuid>, _>("key_id") {
            let row=sqlx::query("SELECT version,grant_snapshot,revoked_at IS NULL AND (expires_at IS NULL OR expires_at>clock_timestamp()) AS active FROM gw_virtual_keys WHERE id=$1").bind(key).fetch_one(&mut *tx).await?;
            if !row.get::<bool, _>("active") {
                return Err(LedgerError::AccessDenied);
            }
            if execution["auth_dependency_versions"]["key"]["id"].as_str()
                != Some(key.to_string().as_str())
                || execution["auth_dependency_versions"]["key"]["version"]
                    .as_str()
                    .and_then(|s| s.parse::<i64>().ok())
                    != Some(row.get("version"))
            {
                return Err(LedgerError::PolicyChanged);
            }
            if !read_grants(row.get("grant_snapshot"))?.iter().any(|g| {
                g.provider == r.get::<String, _>("provider")
                    && g.model == r.get::<String, _>("requested_model")
                    && g.capabilities.contains(&r.get::<String, _>("api"))
            }) {
                return Err(LedgerError::AccessDenied);
            }
        } else if !execution["auth_dependency_versions"]["key"].is_null() {
            return Err(LedgerError::PolicyChanged);
        }
        let free = execution["billing_class"] == "guaranteed_free";
        if execution["auth_dependency_versions"]["principals"][0]["version"]
            .as_str()
            .and_then(|v| v.parse::<i64>().ok())
            != Some(p.get("version"))
            || r.get::<Vec<u8>, _>("auth_config_digest") != self.auth_digest
            || budgets
                .iter()
                .any(|b| b.get::<i64, _>("policy_version") != b.get::<i64, _>("current_version"))
        {
            return Err(LedgerError::PolicyChanged);
        }
        if !free {
            if execution["auth_dependency_versions"]["target_inference_safety_version"]
                .as_str()
                .and_then(|v| v.parse::<i64>().ok())
                != Some(p.get("inference_safety_version"))
            {
                return Err(LedgerError::PolicyChanged);
            }
            if p.get::<Option<String>, _>("inference_pause_reason")
                .is_some()
            {
                return Err(LedgerError::InferencePaused);
            }
            let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM gw_requests WHERE principal_id=$1 AND (accounting_state='unknown' OR (phase='dispatching' AND lease_expires_at<=clock_timestamp() AND execution_snapshot->>'billing_class'='chargeable')))").bind(r.get::<Uuid,_>("principal_id")).fetch_one(&mut *tx).await?;
            if blocked {
                return Err(LedgerError::Unsettled);
            }
            for b in &budgets {
                let stored: super::policy::PolicyLimits =
                    serde_json::from_value(b.get::<Value, _>("policy_snapshot")["limits"].clone())
                        .map_err(|_| LedgerError::InvalidSnapshot)?;
                let limits = stored.effective(&self.ceiling)?;
                // 예산은 이번 기간의 확정 비용만 본다. 실행 중인 요청은 금액을 잡아 두지 않는다.
                if let Some(budget) = &limits.budget {
                    let limit = parse_amount(&budget.limit_usd)?;
                    if limit == Amount::ZERO
                        || parse_amount(&b.get::<String, _>("total_cost"))? >= limit
                    {
                        return Err(LedgerError::BudgetExhausted);
                    }
                }
                // 분당 토큰 한도는 송신을 승인하는 지금 1분 칸에 정산된 토큰으로 판단한다.
                // 받은 분의 칸이 아니다(분 경계에서 받고 다음 분에 승인할 수 있다). 같은 scope의 칸을 쓰는
                // 정산은 모두 이 사용자 잠금을 먼저 잡으므로 잠그지 않고 읽어도 된다.
                // count 결과 수는 생성 토큰이 아니다.
                if r.get::<String, _>("api") != "count_tokens"
                    && let Some(limit) = limits.tpm
                {
                    let used: i64 = sqlx::query_scalar("SELECT COALESCE((SELECT settled_tokens FROM gw_rate_windows WHERE scope_id=$1 AND lane=$2 AND window_start=to_timestamp(floor(extract(epoch FROM $3::timestamptz)/60)*60)),0)").bind(b.get::<Uuid,_>("scope_id")).bind(b.get::<String,_>("rate_lane")).bind(r.get::<String,_>("db_now")).fetch_one(&mut *tx).await?;
                    if used >= limit {
                        return Err(LedgerError::RateLimited);
                    }
                }
            }
        }
        sqlx::query("UPDATE gw_request_budgets SET state='active' WHERE request_id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE gw_requests SET phase='dispatching',dispatch_authorized_at=$2::timestamptz,lease_expires_at=execution_deadline,version=version+1 WHERE id=$1").bind(id).bind(r.get::<String,_>("db_now")).execute(&mut *tx).await?;
        tx.commit()
            .await
            .map_err(|_| LedgerError::DispatchUncertain)?;
        Ok(SendPermit { request_id: id })
    }
    /// 후보 하나만 처리하며 송신/실행정지 권한을 재구성하지 않는다.
    pub async fn recover_expired(&self, id: Uuid) -> Result<Option<Finalized>, LedgerError> {
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut tx = self.database.begin_transaction().await?;
            let (p, r, budgets) = match self.lock_request_mode(&mut tx, id, true).await {
                Ok(rows) => rows,
                Err(LedgerError::Conflict | LedgerError::PolicyChanged) => {
                    tx.rollback().await?;
                    return Ok(None);
                }
                Err(e) => return Err(e),
            };
            let phase: String = r.get("phase");
            if phase == "finished"
                || (phase == "admitted" && !r.get::<bool, _>("admit_expired"))
                || (phase == "dispatching"
                    && !r.get::<Option<bool>, _>("lease_expired").unwrap_or(false))
            {
                tx.rollback().await?;
                return Ok(None);
            }
            let observation = FinalObservation {
                outcome: FinalOutcome::Failed,
                usage: None,
                counted_input_tokens: None,
                reason: if phase == "admitted" {
                    "admission_expired"
                } else {
                    "lease_expired"
                },
                http_status: None,
                provider_request_id: None,
            };
            self.finalize_locked(tx, p, r, budgets, id, &observation, &NoUsage::Unknown)
                .await
                .map(Some)
        })
        .await
        .map_err(|_| LedgerError::DatabaseUnavailable)?
    }
    /// 마감은 ordinary gate를 사용하지 않는다. 같은 pool의 남겨둔 연결 두 개를 사용한다.
    pub async fn finalize_request(
        &self,
        id: Uuid,
        observation: &FinalObservation,
    ) -> Result<Finalized, LedgerError> {
        self.finalize_request_with(id, observation, &NoUsage::Unknown)
            .await
    }
    /// 최종 usage가 없을 때의 정산 방법(`settlement`)을 함께 넘기는 마감. usage가 있으면 무시한다.
    pub async fn finalize_request_with(
        &self,
        id: Uuid,
        observation: &FinalObservation,
        settlement: &NoUsage,
    ) -> Result<Finalized, LedgerError> {
        let mut metric = crate::telemetry::Timer::start(
            crate::telemetry::Metric::Finalization,
            crate::telemetry::Metric::FinalizationMicros,
            crate::telemetry::Metric::FinalizationFailed,
        );
        let result = tokio::time::timeout(Duration::from_secs(3), async {
            let result = self.finalize(id, observation, settlement).await;
            // commit 결과가 불명이어도 다음 reaper/replay에서 같은 절대누계를 복원한다.
            let _ = self.observe_durable_alarms().await;
            result
        })
        .await
        .map_err(|_| LedgerError::DatabaseUnavailable)?;
        if result.is_ok() {
            metric.success();
        }
        result
    }
    async fn finalize(
        &self,
        id: Uuid,
        observation: &FinalObservation,
        settlement: &NoUsage,
    ) -> Result<Finalized, LedgerError> {
        let mut tx = self.database.begin_transaction().await?;
        let (p, r, budgets) = self.lock_request(&mut tx, id).await?;
        self.finalize_locked(tx, p, r, budgets, id, observation, settlement)
            .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn finalize_locked(
        &self,
        mut tx: Transaction<'_, Postgres>,
        p: PgRow,
        r: PgRow,
        budgets: Vec<PgRow>,
        id: Uuid,
        observation: &FinalObservation,
        settlement: &NoUsage,
    ) -> Result<Finalized, LedgerError> {
        let digest = observation.digest_with(settlement);
        if r.get::<String, _>("phase") == "finished" {
            if r.get::<Option<Vec<u8>>, _>("result_digest").as_ref() == Some(&digest) {
                return Ok(Finalized::Noop);
            }
            if (r.get::<String, _>("accounting_state") == "unknown"
                || r.get::<Option<Vec<u8>>, _>("settlement_digest").is_some())
                && observation.usage.is_some()
            {
                let final_observed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM gw_usage_events WHERE request_id=$1 AND kind='final' AND actual_model IS NOT NULL AND normalized_usage->>'source'='provider')").bind(id).fetch_one(&mut *tx).await?;
                if final_observed {
                    return Err(LedgerError::Conflict);
                }
                let usage = observation
                    .usage
                    .as_ref()
                    .ok_or(LedgerError::InvalidSnapshot)?;
                if let Some(prior)=sqlx::query_scalar::<_,Vec<u8>>("SELECT content_digest FROM gw_usage_events WHERE request_id=$1 AND kind='late_final'").bind(id).fetch_optional(&mut *tx).await? {
                    return if prior==digest{Ok(Finalized::Noop)}else{Err(LedgerError::Conflict)};
                }
                let pricing: PricingSnapshot = serde_json::from_value(r.get("pricing_snapshot"))
                    .map_err(|_| LedgerError::InvalidSnapshot)?;
                let execution = read_execution_snapshot(r.get("execution_snapshot"))?;
                let price = pricing.price(usage, false);
                let reason = final_safety_violation(usage, &price, &execution)?;
                let mut selected = usage.selected();
                selected["counted_input_tokens"] =
                    json!(observation.counted_input_tokens.map(|n| n.to_string()));
                let resolution = json!({"pricing_version":pricing.pricing_version,"categories":pricing.resolution_categories(Some(usage)),"rounding_version":pricing.rounding_version,"cost_usd":price.as_ref().ok().map(|n|n.to_string()),"unresolved_reason":if price.is_err(){Some("price_unresolved")}else{None}});
                sqlx::query("INSERT INTO gw_usage_events(request_id,kind,actual_model,provider_usage,normalized_usage,pricing_resolution,reason_code,content_digest) VALUES($1,'late_final',$2,$3,$4,$5,$6,$7)").bind(id).bind(&usage.actual_model).bind(pricing.provider_usage(usage)).bind(selected).bind(resolution).bind(observation.reason).bind(&digest).execute(&mut *tx).await?;
                if let Some(reason) = reason {
                    self.pause(&mut tx, &p, &r, id, reason).await?;
                }
                tx.commit().await?;
                return Ok(Finalized::Noop);
            }
            return Err(LedgerError::Conflict);
        }
        r.get::<i64, _>("version")
            .checked_add(1)
            .ok_or(LedgerError::InvariantViolation)?;
        let execution = read_execution_snapshot(r.get("execution_snapshot"))?;
        let pricing: PricingSnapshot = serde_json::from_value(r.get("pricing_snapshot"))
            .map_err(|_| LedgerError::InvalidSnapshot)?;
        let no_dispatch = r.get::<String, _>("phase") == "admitted";
        let free = execution["billing_class"] == "guaranteed_free"
            && r.get::<String, _>("provider") == "anthropic"
            && r.get::<String, _>("api") == "count_tokens";
        let expired = r.get::<Option<bool>, _>("lease_expired").unwrap_or(false);
        // 최종 usage 없이 끝난 요청(결정 0005).
        // - 시작 event 뒤에 끊긴 Anthropic 스트림은 받을 때 기록한 규칙으로 출력 토큰을 정한다.
        //   입력 토큰은 시작 event가 알린 값이다. 출력은 요청의 max_tokens를 넘지 않는다.
        // - 첫 응답 전에 끊겼거나 Provider가 HTTP 오류로 답한 요청은 0원이다.
        // - 그 밖(처리 중 프로세스 중단, 알 수 없는 응답)은 지금처럼 미정산이다.
        let usable = observation.usage.is_none() && !no_dispatch && !free && !expired;
        let mut cutoff: Option<(FinalUsage, &'static str, Option<&'static str>)> = None;
        if usable
            && let NoUsage::Cutoff {
                start,
                received_output,
                count_failure,
            } = settlement
            && let Some(limit) = execution["output_limit"].as_u64()
        {
            let way = match (execution["cutoff_rule"].as_str(), received_output) {
                (Some("received"), Some(n)) => Some(((*n).min(limit), "cutoff_received", None)),
                (Some("received"), None) => Some((
                    limit,
                    "cutoff_max_tokens",
                    Some(count_failure.unwrap_or("cutoff_count_unavailable")),
                )),
                (Some("max_tokens"), _) => Some((limit, "cutoff_max_tokens", None)),
                _ => None,
            };
            if let Some((output, source, why)) = way {
                cutoff = Some((
                    FinalUsage::new(
                        start.actual_model.clone(),
                        start
                            .tokens
                            .iter()
                            .filter(|(c, _)| *c != UsageCategory::Output)
                            .chain([(UsageCategory::Output, output)]),
                        None,
                    ),
                    source,
                    why,
                ));
            }
        }
        let no_response = usable && cutoff.is_none() && *settlement == NoUsage::NoResponse;
        let priced = observation
            .usage
            .as_ref()
            .or(cutoff.as_ref().map(|(u, _, _)| u));
        let mut safety = None;
        let mut amount = if no_dispatch || free || no_response {
            Some(Amount::ZERO)
        } else if let Some(usage) = priced {
            let price = pricing.price(usage, false);
            safety = final_safety_violation(usage, &price, &execution)?;
            price.ok()
        } else {
            None
        };
        if expired && !free && !no_dispatch {
            amount = None;
        }
        if let Some(cost) = amount {
            for b in &budgets {
                if parse_amount(&b.get::<String, _>("total_cost"))?
                    .checked_add(cost)
                    .is_err()
                {
                    amount = None;
                    safety = Some("amount_overflow");
                    break;
                }
            }
        }
        let (source, usage, provider_usage, tokens) = if no_dispatch || free {
            let source = if no_dispatch {
                "gateway_no_dispatch"
            } else {
                "profile_no_charge"
            };
            (
                source,
                {
                    let mut record =
                        usage_record(source, None, Some(&UsageTokens::default()), None);
                    record["counted_input_tokens"] = json!(if free {
                        observation.counted_input_tokens.map(|n| n.to_string())
                    } else {
                        None
                    });
                    record
                },
                json!({}),
                Some((0, 0)),
            )
        } else if let Some(u) = &observation.usage {
            let tokens = pricing.tokens(u).ok();
            if tokens.is_none() {
                amount = None;
                safety = Some("amount_overflow");
            }
            let mut selected = u.selected();
            selected["counted_input_tokens"] =
                json!(observation.counted_input_tokens.map(|n| n.to_string()));
            ("provider", selected, pricing.provider_usage(u), tokens)
        } else if let Some((u, source, why)) = &cutoff {
            let tokens = pricing.tokens(u).ok();
            if tokens.is_none() {
                amount = None;
                safety = Some("amount_overflow");
            }
            let mut selected = u.selected();
            selected["source"] = json!(source);
            selected["cutoff_rule"] = execution["cutoff_rule"].clone();
            selected["cutoff_fallback_reason"] = json!(why);
            (*source, selected, anthropic_usage(&u.tokens, None), tokens)
        } else if no_response {
            (
                "no_response",
                usage_record("no_response", None, Some(&UsageTokens::default()), None),
                json!({}),
                Some((0, 0)),
            )
        } else {
            (
                "provider",
                usage_record("provider", None, None, None),
                json!({}),
                None,
            )
        };
        // 공통 순서로 잠근 원래 기간의 현재 합계와 지금 1분 칸에 적용할 수 있어야 정산한다.
        // SQL bigint overflow를 일시 DB 장애로 재시도하거나 기존 합계를 clamp하지 않는다.
        // 분당 토큰 한도는 요청이 정산된 1분 칸으로 판단한다. 받은 시각의 칸은 긴 요청이 끝날 때 이미 지나 있다.
        let rate_tokens = r.get::<String, _>("api") != "count_tokens";
        if amount.is_some() {
            let (input, output) = tokens.unwrap_or((0, 0));
            let rate_increment = input
                .checked_add(output)
                .ok_or(LedgerError::InvariantViolation)?;
            for b in &budgets {
                // 0원 정산처럼 토큰이 없으면 1분 칸을 만들지 않는다.
                let window_tokens = if rate_tokens && rate_increment > 0 {
                    let scope: Uuid = b.get("scope_id");
                    let lane: String = b.get("rate_lane");
                    sqlx::query("INSERT INTO gw_rate_windows(scope_id,lane,window_start,window_end) VALUES($1,$2,to_timestamp(floor(extract(epoch FROM $3::timestamptz)/60)*60),to_timestamp(floor(extract(epoch FROM $3::timestamptz)/60)*60)+interval '60 seconds') ON CONFLICT DO NOTHING").bind(scope).bind(&lane).bind(r.get::<String,_>("db_now")).execute(&mut *tx).await?;
                    sqlx::query_scalar::<_, i64>("SELECT settled_tokens FROM gw_rate_windows WHERE scope_id=$1 AND lane=$2 AND window_start=to_timestamp(floor(extract(epoch FROM $3::timestamptz)/60)*60) FOR UPDATE").bind(scope).bind(&lane).bind(r.get::<String,_>("db_now")).fetch_one(&mut *tx).await?
                } else {
                    0
                };
                if [
                    ("total_input_tokens", input),
                    ("total_output_tokens", output),
                    ("total_settled_count", 1),
                    ("total_unknown_tokens_count", i64::from(tokens.is_none())),
                ]
                .iter()
                .any(|(field, increment)| b.get::<i64, _>(*field).checked_add(*increment).is_none())
                    || (rate_tokens && window_tokens.checked_add(rate_increment).is_none())
                {
                    amount = None;
                    safety = Some("amount_overflow");
                }
            }
        }
        // 미송신/보장 무료 사실은 금전 unknown으로 바꿀 수 없다. 저장 공간 자체가
        // 소진된 손상 상태는 성공한 unknown/일시 DB 장애가 아닌 불변식 장애다.
        if amount.is_none() && (no_dispatch || free) {
            return Err(LedgerError::InvariantViolation);
        }
        if amount.is_none()
            && budgets.iter().any(|b| {
                b.get::<i64, _>("total_unknown_count")
                    .checked_add(1)
                    .is_none()
            })
        {
            return Err(LedgerError::InvariantViolation);
        }
        if safety.is_some() && !r.get::<bool, _>("safety_violation_recorded") {
            p.get::<i64, _>("inference_safety_version")
                .checked_add(1)
                .ok_or(LedgerError::InvariantViolation)?;
        }
        let reason = if safety.is_some() {
            "pricing_contract_violation"
        } else if expired && !free && !no_dispatch {
            "lease_expired"
        } else if amount.is_none() && observation.reason == "completed" {
            if observation.usage.is_some() {
                "price_unresolved"
            } else {
                "usage_unresolved"
            }
        } else {
            observation.reason
        };
        let state = if amount.is_some() {
            "settled"
        } else {
            "unknown"
        };
        let outcome = if no_dispatch && observation.outcome == FinalOutcome::Completed {
            "failed"
        } else {
            observation.outcome.name()
        };
        for b in &budgets {
            let scope: Uuid = b.get("scope_id");
            let start: String = b.get("start_text");
            let lane: String = b.get("rate_lane");
            if let Some(cost) = amount {
                let (input, output) = tokens.unwrap_or((0, 0));
                sqlx::query("UPDATE gw_usage_totals SET cost_usd=(cost_usd+($3::numeric)),settled_count=settled_count+1,input_tokens=input_tokens+$4,output_tokens=output_tokens+$5,settled_unknown_tokens_count=settled_unknown_tokens_count+$6,updated_at=clock_timestamp() WHERE scope_id=$1 AND period_start=$2::timestamptz")
                    .bind(scope).bind(&start).bind(cost.to_string()).bind(input).bind(output).bind(i64::from(tokens.is_none())).execute(&mut *tx).await?;
                if rate_tokens && input.checked_add(output).is_some_and(|n| n > 0) {
                    sqlx::query("UPDATE gw_rate_windows SET settled_tokens=settled_tokens+$4,updated_at=clock_timestamp() WHERE scope_id=$1 AND lane=$2 AND window_start=to_timestamp(floor(extract(epoch FROM $3::timestamptz)/60)*60)").bind(scope).bind(&lane).bind(r.get::<String,_>("db_now")).bind(input.checked_add(output).ok_or(LedgerError::AmountOverflow)?).execute(&mut *tx).await?;
                }
                sqlx::query("UPDATE gw_request_budgets SET state='settled',applied_cost_usd=($3::numeric),applied_input_tokens=$4,applied_output_tokens=$5 WHERE request_id=$1 AND scope_id=$2").bind(id).bind(scope).bind(cost.to_string()).bind(tokens.map(|t|t.0)).bind(tokens.map(|t|t.1)).execute(&mut *tx).await?;
            } else {
                sqlx::query("UPDATE gw_usage_totals SET unknown_count=unknown_count+1,updated_at=clock_timestamp() WHERE scope_id=$1 AND period_start=$2::timestamptz").bind(scope).bind(start).execute(&mut *tx).await?;
                sqlx::query("UPDATE gw_request_budgets SET state='unknown' WHERE request_id=$1 AND scope_id=$2").bind(id).bind(scope).execute(&mut *tx).await?;
            }
        }
        // 최초 관측의 최종성은 가격/누적 합계/lease에 따른 회계 확정 여부와 독립이다.
        // late_final은 기존 unknown 이후의 별도 관측이며 이 최초 마감에는 해당하지 않는다.
        let kind = if source != "provider" || observation.usage.is_some() {
            "final"
        } else {
            "partial"
        };
        let resolution = json!({"pricing_version":pricing.pricing_version,"categories":pricing.resolution_categories(if source=="provider"{observation.usage.as_ref()}else{cutoff.as_ref().map(|(u,_,_)|u)}),"rounding_version":pricing.rounding_version,"cost_usd":amount.map(|n|n.to_string()),"unresolved_reason":if amount.is_none(){Some(reason)}else{None}});
        sqlx::query("INSERT INTO gw_usage_events(request_id,kind,actual_model,provider_usage,normalized_usage,pricing_resolution,reason_code,content_digest) VALUES($1,$2,$3,$4,$5,$6,$7,$8)").bind(id).bind(kind).bind(if source=="provider"{observation.usage.as_ref().map(|u|u.actual_model.as_str())}else{cutoff.as_ref().map(|(u,_,_)|u.actual_model.as_str())}).bind(provider_usage).bind(usage).bind(resolution).bind(reason).bind(&digest).execute(&mut *tx).await?;
        sqlx::query("UPDATE gw_requests SET phase='finished',outcome=$2,accounting_state=$3,cost_usd=($4::numeric),finished_at=$9::timestamptz,provider_request_id=$5,http_status=$6,reason_code=$7,result_digest=$8,version=version+1 WHERE id=$1")
            .bind(id).bind(outcome).bind(state).bind(amount.map(|n|n.to_string())).bind(&observation.provider_request_id).bind(observation.http_status.map(|n|n as i16)).bind(reason).bind(&digest).bind(r.get::<String,_>("db_now")).execute(&mut *tx).await?;
        if let Some(reason) = safety {
            self.pause(&mut tx, &p, &r, id, reason).await?;
        }
        if amount.is_none() {
            sqlx::query("UPDATE gw_alarm_totals SET total=total+1 WHERE code='unknown'")
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        if reason == "price_unresolved" {
            crate::telemetry::add(crate::telemetry::Metric::PriceUnresolved, 1);
        }
        Ok(if amount.is_some() {
            Finalized::Settled
        } else {
            Finalized::Unknown
        })
    }
    async fn pause(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        p: &PgRow,
        r: &PgRow,
        id: Uuid,
        reason: &str,
    ) -> Result<(), LedgerError> {
        if r.get::<bool, _>("safety_violation_recorded") {
            return Ok(());
        }
        sqlx::query("UPDATE gw_alarm_totals SET total=total+1 WHERE code='bound'")
            .execute(&mut **tx)
            .await?;
        let principal: Uuid = p.get("id");
        let before = json!({"paused":p.get::<Option<String>,_>("inference_pause_reason").is_some(),"version":p.get::<i64,_>("inference_safety_version").to_string(),"reason":p.get::<Option<String>,_>("inference_pause_reason"),"trigger_request_id":p.get::<Option<Uuid>,_>("inference_pause_request_id").map(|id|id.to_string())});
        sqlx::query("UPDATE gw_principals SET inference_paused_at=$4::timestamptz,inference_pause_reason=$2,inference_pause_request_id=$3,inference_safety_version=inference_safety_version+1 WHERE id=$1").bind(principal).bind(reason).bind(id).bind(r.get::<String,_>("db_now")).execute(&mut **tx).await?;
        sqlx::query("UPDATE gw_requests SET safety_violation_recorded=true WHERE id=$1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
        let after = json!({"paused":true,"version":(p.get::<i64,_>("inference_safety_version")+1).to_string(),"reason":reason,"trigger_request_id":id.to_string()});
        sqlx::query("INSERT INTO gw_audit_events(actor_kind,executor_id,request_id,actor_auth_config_digest,operation,target_kind,target_id,before_snapshot,after_snapshot) VALUES('system',$1,$2,$3,'inference_auto_pause','principal',$4,$5,$6)").bind(r.get::<Uuid,_>("executor_id")).bind(id).bind(self.auth_digest.as_slice()).bind(principal).bind(before).bind(after).execute(&mut **tx).await?;
        Ok(())
    }
    /// 끊긴 요청에 쓸 정산 규칙. 받을 때 실행 기록에 남긴 값이고, 규칙을 쓰지 않는 요청은 None이다.
    /// 마감 직전에 읽으므로 일반 gate를 쓰지 않는다.
    pub async fn cutoff_rule(&self, id: Uuid) -> Result<Option<String>, LedgerError> {
        tokio::time::timeout(
            Duration::from_secs(2),
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT execution_snapshot->>'cutoff_rule' FROM gw_requests WHERE id=$1",
            )
            .bind(id)
            .fetch_one(self.database.pool()),
        )
        .await
        .map_err(|_| LedgerError::DatabaseUnavailable)?
        .map_err(LedgerError::from)
    }
    pub async fn record_execution_stopped(&self, id: Uuid) -> Result<(), LedgerError> {
        tokio::time::timeout(Duration::from_secs(3),async {
            let mut tx=self.database.begin_transaction().await?;
            let (_,r,_)=self.lock_request(&mut tx,id).await?;
            if r.get::<Uuid,_>("executor_id")!=self.executor_id {return Err(LedgerError::Conflict);}
            sqlx::query("UPDATE gw_requests SET execution_stopped_at=clock_timestamp(),version=version+1 WHERE id=$1 AND execution_stopped_at IS NULL").bind(id).execute(&mut *tx).await?;
            tx.commit().await?;Ok(())
        }).await.map_err(|_|LedgerError::DatabaseUnavailable)?
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantSnapshot {
    provider: String,
    model: String,
    capabilities: Vec<String>,
}
fn read_grants(value: Value) -> Result<Vec<GrantSnapshot>, LedgerError> {
    let rules: Vec<GrantSnapshot> =
        serde_json::from_value(value).map_err(|_| LedgerError::InvalidSnapshot)?;
    if rules.len() > 64 {
        return Err(LedgerError::InvalidSnapshot);
    }
    let mut tuples = std::collections::BTreeSet::new();
    for grant in &rules {
        if grant.model.is_empty()
            || grant.model.len() > 200
            || grant.model.chars().any(char::is_control)
            || grant.capabilities.is_empty()
        {
            return Err(LedgerError::InvalidSnapshot);
        }
        for api in &grant.capabilities {
            if !capability_allowed(&grant.provider, api)
                || !tuples.insert((&grant.provider, &grant.model, api))
            {
                return Err(LedgerError::InvalidSnapshot);
            }
        }
    }
    Ok(rules)
}
fn parse_amount(value: &str) -> Result<Amount, LedgerError> {
    value.parse().map_err(|_| LedgerError::InvalidSnapshot)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutionSnapshot {
    schema_version: u8,
    profile_version: String,
    billing_class: String,
    capability: String,
    stream: bool,
    #[serde(deserialize_with = "required_nullable")]
    output_limit: Option<u64>,
    #[serde(deserialize_with = "required_nullable")]
    input_bound_tokens: Option<u64>,
    #[serde(deserialize_with = "required_nullable")]
    output_bound_tokens: Option<u64>,
    #[serde(deserialize_with = "required_nullable")]
    cost_bound_usd: Option<String>,
    auth_dependency_versions: DependencyVersions,
    #[serde(deserialize_with = "required_nullable")]
    api_version: Option<String>,
    approved_beta: Vec<String>,
    /// 끊긴 요청의 정산 규칙(`received` 또는 `max_tokens`). 규칙을 쓰지 않는 요청은 null이다.
    #[serde(default)]
    cutoff_rule: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DependencyVersions {
    principals: Vec<VersionedId>,
    target_inference_safety_version: String,
    #[serde(deserialize_with = "required_nullable")]
    key: Option<VersionedId>,
    policies: Vec<VersionedId>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionedId {
    id: String,
    version: String,
}
fn valid_version(value: &str) -> bool {
    value
        .parse::<i64>()
        .is_ok_and(|n| n > 0 && n.to_string() == value)
}
fn required_nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    Option::<T>::deserialize(d)
}
pub(crate) fn read_execution_snapshot(value: Value) -> Result<Value, LedgerError> {
    let e: ExecutionSnapshot =
        serde_json::from_value(value).map_err(|_| LedgerError::InvalidSnapshot)?;
    let ids = |entries: &[VersionedId]| {
        !entries.is_empty()
            && entries.len() <= 3
            && entries
                .iter()
                .all(|v| v.id.parse::<Uuid>().is_ok() && valid_version(&v.version))
            && entries
                .iter()
                .map(|v| &v.id)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == entries.len()
    };
    if e.schema_version != 1
        || e.profile_version.is_empty()
        || e.profile_version.len() > 64
        || e.profile_version.chars().any(char::is_control)
        || !["chargeable", "guaranteed_free"].contains(&e.billing_class.as_str())
        || !crate::providers::port::providers()
            .into_iter()
            .flat_map(|p| p.apis())
            .any(|api| api_name(api.kind()) == e.capability)
        || (e.billing_class == "guaranteed_free" && e.capability != "count_tokens")
        || !ids(&e.auth_dependency_versions.principals)
        || !ids(&e.auth_dependency_versions.policies)
        || !valid_version(&e.auth_dependency_versions.target_inference_safety_version)
        || e.auth_dependency_versions
            .key
            .as_ref()
            .is_some_and(|v| v.id.parse::<Uuid>().is_err() || !valid_version(&v.version))
        || e.cost_bound_usd
            .as_ref()
            .is_some_and(|n| n.parse::<Amount>().is_err())
        || [e.input_bound_tokens, e.output_bound_tokens, e.output_limit]
            .iter()
            .flatten()
            .any(|n| *n == 0 || *n > i64::MAX as u64)
        || e.approved_beta.len() > 8
        || e.approved_beta.iter().any(|s| {
            s.is_empty()
                || s.len() > 128
                || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        || e.api_version.as_ref().is_some_and(|s| s != "2023-06-01")
        || e.cutoff_rule
            .as_ref()
            .is_some_and(|s| !["received", "max_tokens"].contains(&s.as_str()))
    {
        return Err(LedgerError::InvalidSnapshot);
    }
    serde_json::to_value(e).map_err(|_| LedgerError::InvalidSnapshot)
}
#[cfg(test)]
mod snapshot_tests {
    use super::*;
    fn execution() -> Value {
        json!({"schema_version":1,"profile_version":"fixture-v1","billing_class":"chargeable","capability":"messages","stream":false,"output_limit":100,"input_bound_tokens":4096,"output_bound_tokens":100,"cost_bound_usd":"0.004296000000","auth_dependency_versions":{"principals":[{"id":Uuid::nil().to_string(),"version":"1"}],"target_inference_safety_version":"1","key":null,"policies":[{"id":Uuid::nil().to_string(),"version":"1"}]},"api_version":"2023-06-01","approved_beta":[]})
    }
    #[test]
    fn execution_snapshot_null_bound_is_not_a_money_proof() {
        let mut e = execution();
        e["cost_bound_usd"] = Value::Null;
        assert!(
            read_execution_snapshot(e.clone()).is_ok(),
            "nullable canonical bound"
        );
        for bad in [json!("1"), json!("NaN"), json!("-0.000000000001"), json!(0)] {
            e["cost_bound_usd"] = bad;
            assert!(read_execution_snapshot(e.clone()).is_err());
        }
    }
    #[test]
    fn policy_grants_are_strict_capability_tuples() {
        read_grants(json!([{ "provider":"anthropic","model":"synthetic-text","capabilities":["messages","count_tokens"]}])).unwrap();
        for bad in [
            json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"],"role":"admin"}]),
            json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["responses"]}]),
            json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages","messages"]}]),
        ] {
            assert!(read_grants(bad).is_err());
        }
    }
    use UsageCategory::*;
    fn usage(tokens: &[(UsageCategory, u64)]) -> FinalUsage {
        FinalUsage::new("synthetic-text", tokens.iter().copied(), None)
    }
    #[test]
    fn normalized_tokens_fit_the_bigint_rate_accumulator() {
        let u = usage(&[(Input, i64::MAX as u64), (Output, 1)]);
        assert!(
            u.checked_tokens().is_err(),
            "combined normalized tokens overflowed the bigint rate lane"
        );
        // 분당 토큰 한도의 입력은 input과 모든 캐시 칸의 합이다.
        let u = usage(&[
            (Input, 1),
            (Output, 2),
            (CacheRead, 3),
            (CacheWrite5m, 4),
            (CacheWrite1h, 5),
        ]);
        assert_eq!(u.checked_tokens(), Ok((13, 2)));
    }
    #[test]
    fn duplicate_categories_are_malformed() {
        assert!(FinalUsage::from_list("m".into(), vec![(Input, 1), (Input, 2)], None).is_none());
        let u = FinalUsage::from_list("m".into(), vec![(Input, 1), (CacheRead, 0)], None).unwrap();
        assert_eq!((u.get(Input), u.get(CacheRead), u.get(Output)), (1, 0, 0));
    }
    fn anthropic(rates: serde_json::Value) -> PricingSnapshot {
        let profile: ModelProfile = serde_json::from_value(json!({"model":"synthetic-text","api":"messages","profile_version":"fixture-v1","pricing_version":"price-v1","fixture_version":"text-v1","actual_models":["synthetic-text"],"features":["cache"],"max_input_tokens":4096,"max_output_tokens":4096,"max_body_bytes":8388608,"max_response_bytes":16777216,"rates":rates})).unwrap();
        PricingSnapshot::from_profile(ProviderId::Anthropic, &profile)
    }
    #[test]
    fn categories_are_priced_separately_and_unpriced_tokens_are_unknown() {
        let both = anthropic(
            json!({"input":"1.000000000000","output":"2.000000000000","cache_read":"0.100000000000","cache_write_5m":"1.250000000000","cache_write_1h":"2.000000000000"}),
        );
        assert_eq!(both.cache_tier_rules, "explicit_categories_v1");
        let u = usage(&[
            (Input, 1_000_000),
            (Output, 1_000_000),
            (CacheRead, 1_000_000),
            (CacheWrite5m, 1_000_000),
            (CacheWrite1h, 1_000_000),
        ]);
        assert_eq!(both.price(&u, false).unwrap().to_string(), "6.350000000000");
        let five = anthropic(
            json!({"input":"1.000000000000","output":"2.000000000000","cache_read":"0.100000000000","cache_write_5m":"1.250000000000"}),
        );
        // 1시간 캐시 쓰기 단가가 없으면 5분 단가로 대신하지 않는다.
        assert_eq!(five.price(&u, false), Err(LedgerError::InvalidSnapshot));
        let without_1h = usage(&[(Input, 1_000_000), (CacheWrite5m, 1_000_000)]);
        assert_eq!(
            five.price(&without_1h, false).unwrap().to_string(),
            "2.250000000000"
        );
        // Anthropic messages가 알리지 않은 칸(`cache_write`)에 토큰이 있으면 금액을 정하지 않는다.
        assert_eq!(
            both.price(&usage(&[(Input, 1), (CacheWrite, 1)]), false),
            Err(LedgerError::InvalidSnapshot)
        );
        // 알리지 않은 칸도 0이면 없는 칸이다.
        assert!(
            both.price(&usage(&[(Input, 1), (CacheWrite, 0)]), false)
                .is_ok()
        );
        // Anthropic snapshot에는 `cache_write` 단가를 둘 수 없다.
        let mut legacy = five.clone();
        legacy.rates.push(PriceRate {
            category: "cache_write".into(),
            usd_per_million_tokens: "1.000000000000".into(),
        });
        assert_eq!(legacy.validate(), Err(LedgerError::InvalidSnapshot));
        let mut removed = five;
        removed.cache_tier_rules = "anthropic_5m_v1".into();
        assert_eq!(removed.validate(), Err(LedgerError::InvalidSnapshot));
    }
    #[test]
    fn final_safety_violation_shares_token_cost_and_overflow_precedence() {
        let execution = execution();
        let base = usage(&[
            (Input, 4094),
            (Output, 100),
            (CacheRead, 1),
            (CacheWrite5m, 1),
        ]);
        let price = Ok(Amount::ZERO);
        assert_eq!(
            final_safety_violation(&base, &price, &execution).unwrap(),
            None
        );
        for (input, output, cache_read, cache_write) in [
            (4097, 0, 0, 0),
            (4094, 0, 2, 1),
            (4094, 0, 1, 2),
            (10, 101, 0, 0),
        ] {
            let u = usage(&[
                (Input, input),
                (Output, output),
                (CacheRead, cache_read),
                (CacheWrite1h, cache_write),
            ]);
            assert_eq!(
                final_safety_violation(&u, &price, &execution).unwrap(),
                Some("cost_bound_violation")
            );
            assert_eq!(
                final_safety_violation(&u, &Err(LedgerError::AmountOverflow), &execution).unwrap(),
                Some("amount_overflow")
            );
        }
        let cost = Ok("0.004296000001".parse().unwrap());
        assert_eq!(
            final_safety_violation(&base, &cost, &execution).unwrap(),
            Some("cost_bound_violation")
        );
        let mut overflow = base.clone();
        overflow.tokens.set(Input, i64::MAX as u64);
        assert_eq!(
            final_safety_violation(&overflow, &cost, &execution).unwrap(),
            Some("amount_overflow")
        );
        let mut overflow = base;
        overflow.tokens.set(CacheRead, u64::MAX);
        assert_eq!(
            final_safety_violation(&overflow, &price, &execution).unwrap(),
            Some("amount_overflow")
        );
    }
    #[test]
    fn snapshots_are_strict_projections_not_content_containers() {
        let mut v = execution();
        v["prompt"] = json!("forbidden content");
        assert!(
            read_execution_snapshot(v).is_err(),
            "unknown execution snapshot field admitted"
        );
        read_execution_snapshot(execution()).unwrap();
    }
}
