//! DeepSeek Chat Completions 어댑터(결정 0009).
//!
//! DeepSeek 고유 text/tools/thinking/JSON·hit/miss·마지막 일반 chunk 계약을 검사한다.
//! 본체의 HTTP·설정·장부 타입에 기대지 않는다. 본체는 `src/providers/deepseek.rs`에서
//! 이 crate를 port에 연결한다.
use m_gated_adapter_kit::{
    adapter::{
        ApiError, ErrorFormat, Feature, Header, Profile, ProfileError, ReadError, Reading,
        RequestMetadata, Step, Usage, UsageCategory,
    },
    json::{self, Cooperate, JsonError, Kind, Visitor},
    openai,
    stream::{Event, JsonDecoder, Scalar, SseDecoder},
};
use std::collections::{BTreeMap, BTreeSet};

/// Provider 이름. 본체의 `ProviderId`와 같다.
pub const NAME: &str = "deepseek";
/// 업스트림 주소.
pub const ORIGIN: &str = "https://api.deepseek.com";
/// 키 확인 경로.
pub const VERIFY_PATH: &str = "models";
/// 키는 이 접두사로 시작해야 한다.
pub const KEY_PREFIX: &str = openai::SK_KEY_PREFIX;
/// 키가 이 접두사로 시작하면 받지 않는다.
pub const REFUSED_KEY_PREFIXES: &[&str] = openai::SK_REFUSED_KEY_PREFIXES;
/// 응답에서 Provider 요청 id를 읽을 헤더. 앞의 것이 먼저다.
pub const REQUEST_ID_HEADERS: &[&str] = &["x-request-id"];
/// 클라이언트에 그대로 넘기는 요청 id 응답 헤더.
pub const RESPONSE_ID_HEADERS: &[&str] = &["request-id", "x-request-id"];
/// 게이트웨이가 받는 경로.
pub const ROUTE: &str = "/deepseek/v1/chat/completions";
/// 업스트림 경로.
pub const UPSTREAM_PATH: &str = "chat/completions";
pub const ERROR_FORMAT: ErrorFormat = ErrorFormat::Openai;
/// 가격표 snapshot에 남기는 토큰 포함 규칙.
pub const TOKEN_RULE: &str = "deepseek_prompt_includes_hit_completion_includes_reasoning_v1";
/// 사용량 칸(결정 0012). `input`은 캐시에 없던 입력(miss)이고, `cache_read`는 캐시에서 읽은 입력(hit)이다.
pub const USAGE_CATEGORIES: &[UsageCategory] = &[
    UsageCategory::Input,
    UsageCategory::Output,
    UsageCategory::CacheRead,
];

/// `sk-` 키만 받는다.
pub fn accepts_key(secret: &str) -> bool {
    openai::accepts_sk_key(secret)
}
/// 가격은 `input`, `output`, `cache_read` 셋이다. 이미지는 받지 않는다.
pub fn check_profile(profile: &Profile) -> Result<(), ProfileError> {
    if profile.rates.len() != 3
        || !["input", "output", "cache_read"]
            .iter()
            .all(|k| profile.has_rate(k))
    {
        return Err(ProfileError::InvalidPrice);
    }
    if profile.has(Feature::InlinePng) {
        return Err(ProfileError::InvalidProfile);
    }
    openai::check_compatible_profile(profile)
}
/// 업스트림에 보낼 클라이언트 헤더의 이름.
pub fn approved_headers(headers: &[Header]) -> Result<Vec<&'static str>, ApiError> {
    openai::approved_headers(headers)
}
fn at(p: &[String], parts: &[&str]) -> bool {
    p.iter().map(String::as_str).eq(parts.iter().copied())
}
fn integer(raw: &str) -> Result<u64, JsonError> {
    if raw.len() > 19 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(JsonError);
    }
    raw.parse::<u64>()
        .ok()
        .filter(|n| *n <= i64::MAX as u64)
        .ok_or(JsonError)
}
fn keys(k: &BTreeSet<String>, required: &[&str], allowed: &[&str]) -> Result<(), JsonError> {
    if required.iter().all(|s| k.contains(*s)) && k.iter().all(|s| allowed.contains(&s.as_str())) {
        Ok(())
    } else {
        Err(JsonError)
    }
}
struct Input<'a> {
    profile: &'a Profile,
    model: Option<String>,
    output: Option<u64>,
    stream: bool,
    stream_options: bool,
    role: Option<String>,
    null_content: bool,
    reasoning: bool,
    tools: bool,
    types: BTreeMap<Vec<String>, String>,
    unsupported: bool,
    thinking_disabled: bool,
    forced_tool: bool,
}
impl Input<'_> {
    fn feature(&mut self, f: Feature) -> Result<(), JsonError> {
        if self.profile.has(f) {
            Ok(())
        } else {
            self.unsupported = true;
            Err(JsonError)
        }
    }
    fn schema(p: &[String]) -> bool {
        p.starts_with(&[
            "tools".into(),
            "*".into(),
            "function".into(),
            "parameters".into(),
        ])
    }
}
impl Visitor for Input<'_> {
    fn value(&mut self, p: &[String], k: Kind, r: &str) -> Result<(), JsonError> {
        if p.is_empty() {
            return if k == Kind::Object {
                Ok(())
            } else {
                Err(JsonError)
            };
        }
        if Self::schema(p) {
            return if at(p, &["tools", "*", "function", "parameters"]) && k != Kind::Object {
                Err(JsonError)
            } else {
                Ok(())
            };
        }
        if k == Kind::Object {
            self.types.remove(p);
        }
        let want = if p.len() == 1 {
            match p[0].as_str() {
                "model" => {
                    self.model = Some(json::string(r, 200)?);
                    Kind::String
                }
                "messages" => Kind::Array,
                "max_tokens" => {
                    self.output = Some(integer(r)?);
                    Kind::Number
                }
                "stream" => {
                    self.stream = r == "true";
                    Kind::Bool
                }
                "stream_options" => {
                    self.stream_options = true;
                    Kind::Object
                }
                "tools" => {
                    self.feature(Feature::ClientTools)?;
                    Kind::Array
                }
                "tool_choice" => {
                    self.feature(Feature::ClientTools)?;
                    self.forced_tool = k == Kind::Object
                        || (k == Kind::String && json::string(r, 16)? == "required");
                    if k == Kind::String {
                        return if ["auto", "none", "required"]
                            .contains(&json::string(r, 16)?.as_str())
                        {
                            Ok(())
                        } else {
                            Err(JsonError)
                        };
                    }
                    Kind::Object
                }
                "thinking" => Kind::Object,
                "response_format" => {
                    self.feature(Feature::JsonSchema)?;
                    Kind::Object
                }
                "temperature" | "top_p" => {
                    return if k == Kind::Number
                        && r.len() <= 64
                        && r.parse::<f64>().is_ok_and(|n| {
                            if p[0] == "temperature" {
                                (0.0..=2.0).contains(&n)
                            } else {
                                n > 0.0 && n <= 1.0
                            }
                        }) {
                        Ok(())
                    } else {
                        Err(JsonError)
                    };
                }
                _ => {
                    self.unsupported = true;
                    return Err(JsonError);
                }
            }
        } else if at(p, &["messages", "*"]) {
            self.role = None;
            self.null_content = false;
            self.reasoning = false;
            self.tools = false;
            Kind::Object
        } else if at(p, &["messages", "*", "role"]) {
            let role = json::string(r, 16)?;
            if !["system", "user", "assistant", "tool"].contains(&role.as_str()) {
                return Err(JsonError);
            }
            self.role = Some(role);
            Kind::String
        } else if at(p, &["messages", "*", "content"]) {
            self.null_content = k == Kind::Null;
            if k == Kind::Null {
                return Ok(());
            }
            Kind::String
        } else if at(p, &["messages", "*", "reasoning_content"]) {
            self.feature(Feature::Thinking)?;
            self.reasoning = true;
            Kind::String
        } else if at(p, &["messages", "*", "tool_calls"]) {
            self.feature(Feature::ClientTools)?;
            self.tools = true;
            Kind::Array
        } else if at(p, &["messages", "*", "tool_call_id"]) {
            self.feature(Feature::ClientTools)?;
            if json::string(r, 200)?.is_empty() {
                return Err(JsonError);
            }
            Kind::String
        } else if at(p, &["tools", "*"])
            || at(p, &["tools", "*", "function"])
            || at(p, &["messages", "*", "tool_calls", "*"])
            || at(p, &["messages", "*", "tool_calls", "*", "function"])
            || at(p, &["tool_choice", "function"])
        {
            Kind::Object
        } else if p.last().is_some_and(|s| s == "type")
            && (at(&p[..p.len() - 1], &["tools", "*"])
                || at(&p[..p.len() - 1], &["messages", "*", "tool_calls", "*"])
                || at(&p[..p.len() - 1], &["tool_choice"])
                || at(&p[..p.len() - 1], &["thinking"])
                || at(&p[..p.len() - 1], &["response_format"]))
        {
            self.types
                .insert(p[..p.len() - 1].to_vec(), json::string(r, 32)?);
            Kind::String
        } else if at(p, &["tools", "*", "function", "name"])
            || at(p, &["tool_choice", "function", "name"])
            || at(p, &["messages", "*", "tool_calls", "*", "function", "name"])
            || at(p, &["messages", "*", "tool_calls", "*", "id"])
        {
            if json::string(r, 200)?.is_empty() {
                return Err(JsonError);
            }
            Kind::String
        } else if at(p, &["tools", "*", "function", "description"])
            || at(
                p,
                &["messages", "*", "tool_calls", "*", "function", "arguments"],
            )
        {
            Kind::String
        } else if at(p, &["stream_options", "include_usage"]) {
            Kind::Bool
        } else {
            self.unsupported = true;
            return Err(JsonError);
        };
        if k == want { Ok(()) } else { Err(JsonError) }
    }
    fn object_end(&mut self, p: &[String], k: &BTreeSet<String>, r: &str) -> Result<(), JsonError> {
        if Self::schema(p) {
            return if r.len() <= 65536 {
                Ok(())
            } else {
                Err(JsonError)
            };
        }
        if p.is_empty() {
            return keys(
                k,
                &["model", "messages", "max_tokens"],
                &[
                    "model",
                    "messages",
                    "max_tokens",
                    "stream",
                    "stream_options",
                    "tools",
                    "tool_choice",
                    "thinking",
                    "response_format",
                    "temperature",
                    "top_p",
                ],
            );
        }
        if at(p, &["messages", "*"]) {
            let role = self.role.as_deref().ok_or(JsonError)?;
            if ((self.null_content || self.reasoning || self.tools) && role != "assistant")
                || (self.null_content && !self.tools)
            {
                return Err(JsonError);
            }
            return match role {
                "assistant" => {
                    if !k.contains("content") && !self.tools {
                        return Err(JsonError);
                    }
                    keys(
                        k,
                        &["role"],
                        &["role", "content", "tool_calls", "reasoning_content"],
                    )
                }
                "tool" => keys(
                    k,
                    &["role", "content", "tool_call_id"],
                    &["role", "content", "tool_call_id"],
                ),
                _ => keys(k, &["role", "content"], &["role", "content"]),
            };
        }
        let t = self.types.remove(p);
        if at(p, &["thinking"]) {
            self.thinking_disabled = t.as_deref() == Some("disabled");
        }
        if at(p, &["thinking"]) {
            return if matches!(t.as_deref(), Some("enabled" | "disabled")) {
                keys(k, &["type"], &["type"])
            } else {
                Err(JsonError)
            };
        }
        if at(p, &["response_format"]) {
            return if matches!(t.as_deref(), Some("text" | "json_object")) {
                keys(k, &["type"], &["type"])
            } else {
                Err(JsonError)
            };
        }
        if at(p, &["tools", "*"]) || at(p, &["tool_choice"]) {
            return if t.as_deref() == Some("function") {
                keys(k, &["type", "function"], &["type", "function"])
            } else {
                Err(JsonError)
            };
        }
        if at(p, &["messages", "*", "tool_calls", "*"]) {
            return if t.as_deref() == Some("function") {
                keys(k, &["id", "type", "function"], &["id", "type", "function"])
            } else {
                Err(JsonError)
            };
        }
        if at(p, &["tools", "*", "function"]) {
            return keys(
                k,
                &["name", "parameters"],
                &["name", "parameters", "description"],
            );
        }
        if at(p, &["messages", "*", "tool_calls", "*", "function"]) {
            return keys(k, &["name", "arguments"], &["name", "arguments"]);
        }
        if at(p, &["tool_choice", "function"]) {
            return keys(k, &["name"], &["name"]);
        }
        if at(p, &["stream_options"]) {
            return keys(k, &["include_usage"], &["include_usage"]);
        }
        Err(JsonError)
    }
    fn array_end(&mut self, p: &[String], n: usize, _: &str) -> Result<(), JsonError> {
        if (at(p, &["messages"]) && n == 0)
            || ((at(p, &["tools"]) || at(p, &["messages", "*", "tool_calls"]))
                && (n == 0 || n > 64))
        {
            Err(JsonError)
        } else {
            Ok(())
        }
    }
}
/// 요청 본문을 검사한다. 긴 본문은 `cooperate`로 실행을 넘기며 읽는다.
pub async fn validate_with<C: Cooperate>(
    body: &[u8],
    profile: &Profile,
    cooperate: C,
) -> Result<RequestMetadata, ApiError> {
    let mut v = Input {
        profile,
        model: None,
        output: None,
        stream: false,
        stream_options: false,
        role: None,
        null_content: false,
        reasoning: false,
        tools: false,
        types: BTreeMap::new(),
        unsupported: false,
        thinking_disabled: false,
        forced_tool: false,
    };
    if json::scan_with(body, &mut v, cooperate).await.is_err() {
        return Err(if v.unsupported {
            ApiError::UnsupportedOption
        } else {
            ApiError::InvalidRequest
        });
    }
    // 생략은 native enabled다. 명시 disabled만 미검증 thinking의 opt-out이며 원본을 수정하지 않는다.
    if !v.thinking_disabled && !profile.has(Feature::Thinking) {
        return Err(ApiError::UnsupportedOption);
    }
    if (v.forced_tool && !v.thinking_disabled)
        || (!v.stream && v.stream_options)
        || !v
            .output
            .is_some_and(|n| n > 0 && profile.max_output_tokens.is_some_and(|max| n <= max))
    {
        return Err(ApiError::InvalidRequest);
    }
    let model = v.model.ok_or(ApiError::InvalidRequest)?;
    if model != profile.model {
        return Err(ApiError::UnsupportedModel);
    }
    Ok(RequestMetadata {
        model,
        output_limit: v.output,
        stream: v.stream,
    })
}
pub const FIELDS: &[&str] = &[
    "error.**",
    "object",
    "id",
    "model",
    "service_tier",
    "usage.**",
    "choices",
    "choices.$length",
    "choices.*",
    "choices.*.index",
    "choices.*.finish_reason",
    "choices.*.message",
    "choices.*.message.role",
    "choices.*.message.content",
    "choices.*.message.reasoning_content",
    "choices.*.message.tool_calls",
    "choices.*.delta",
    "choices.*.delta.role",
    "choices.*.delta.content",
    "choices.*.delta.reasoning_content",
    "choices.*.delta.tool_calls",
];
pub const STRING_SHAPES: &[&str] = &[
    "error.message",
    "error.type",
    "error.param",
    "error.code",
    "choices.*.message.content",
    "choices.*.message.reasoning_content",
    "choices.*.delta.content",
    "choices.*.delta.reasoning_content",
];
fn shape(v: &BTreeMap<String, Scalar>, stream: bool) -> bool {
    let prefix = if stream {
        "choices.*.delta"
    } else {
        "choices.*.message"
    };
    v.get("choices") == Some(&Scalar::Array)
        && v.get("choices.$length").and_then(Scalar::integer) == Some(1)
        && v.get("choices.*") == Some(&Scalar::Object)
        && v.get("choices.*.index").and_then(Scalar::integer) == Some(0)
        && v.get(prefix) == Some(&Scalar::Object)
        && if stream {
            v.get(&format!("{prefix}.role"))
                .is_none_or(|v| v.text() == Some("assistant"))
        } else {
            v.get(&format!("{prefix}.role")).and_then(Scalar::text) == Some("assistant")
                && v.contains_key(&format!("{prefix}.content"))
        }
        && ["content", "reasoning_content"].iter().all(|s| {
            v.get(&format!("{prefix}.{s}"))
                .is_none_or(|v| matches!(v, Scalar::Null | Scalar::StringShape))
        })
        && v.get(&format!("{prefix}.tool_calls"))
            .is_none_or(|v| v == &Scalar::Array)
}
fn finish_reason(v: &BTreeMap<String, Scalar>) -> bool {
    matches!(
        v.get("choices.*.finish_reason").and_then(Scalar::text),
        Some("stop" | "length" | "tool_calls" | "content_filter" | "insufficient_system_resource")
    )
}
fn identity(v: &BTreeMap<String, Scalar>, object: &str) -> bool {
    v.get("object").and_then(Scalar::text) == Some(object)
        && v.get("id")
            .and_then(Scalar::text)
            .is_some_and(|s| !s.is_empty() && s.len() <= 256)
        && v.get("model")
            .and_then(Scalar::text)
            .is_some_and(|s| !s.is_empty() && s.len() <= 200)
}
fn usage(v: &BTreeMap<String, Scalar>) -> Option<Usage> {
    if v.contains_key("service_tier") || v.get("usage") != Some(&Scalar::Object) {
        return None;
    }
    for (k, val) in v {
        if let Some(k) = k.strip_prefix("usage.") {
            let ok = match k {
                "prompt_tokens"
                | "completion_tokens"
                | "total_tokens"
                | "prompt_cache_hit_tokens"
                | "prompt_cache_miss_tokens"
                | "completion_tokens_details.reasoning_tokens"
                | "prompt_tokens_details.cached_tokens" => val.integer().is_some(),
                "completion_tokens_details" | "prompt_tokens_details" => val == &Scalar::Object,
                _ => false,
            };
            if !ok {
                return None;
            }
        }
    }
    let get = |k: &str| v.get(k).and_then(Scalar::integer);
    let input = get("usage.prompt_tokens")?;
    let output = get("usage.completion_tokens")?;
    let hit = get("usage.prompt_cache_hit_tokens")?;
    let miss = get("usage.prompt_cache_miss_tokens")?;
    if hit.checked_add(miss) != Some(input)
        || input.checked_add(output) != get("usage.total_tokens")
        || v.get("usage.prompt_tokens_details")
            .is_some_and(|_| get("usage.prompt_tokens_details.cached_tokens") != Some(hit))
    {
        return None;
    }
    let reasoning = if v.contains_key("usage.completion_tokens_details") {
        Some(get("usage.completion_tokens_details.reasoning_tokens")?)
    } else {
        None
    };
    if reasoning.is_some_and(|n| n > output) {
        return None;
    }
    Some(Usage {
        actual_model: v.get("model")?.text()?.into(),
        tokens: vec![
            (UsageCategory::Input, miss),
            (UsageCategory::Output, output),
            (UsageCategory::CacheRead, hit),
        ],
        reasoning_tokens: reasoning,
    })
}
fn final_usage(v: &BTreeMap<String, Scalar>) -> Option<Usage> {
    if v.get("error").is_some_and(|v| v != &Scalar::Null)
        || !identity(v, "chat.completion")
        || !shape(v, false)
        || !finish_reason(v)
    {
        return None;
    }
    usage(v)
}
fn native_error_shape(v: &BTreeMap<String, Scalar>) -> bool {
    v.get("error") == Some(&Scalar::Object)
        && v.get("error.message") == Some(&Scalar::StringShape)
        && v.get("error.type") == Some(&Scalar::StringShape)
        && v.iter().all(|(k, v)| match k.as_str() {
            "error" | "error.message" | "error.type" => true,
            "error.param" | "error.code" => matches!(v, Scalar::Null | Scalar::StringShape),
            _ => false,
        })
}
/// 비스트림 응답 본문을 읽는다. 본체가 상태 코드, 인코딩, content-type, 크기를 먼저 확인한다.
pub async fn read_with<C: Cooperate>(
    status: u16,
    body: &[u8],
    cooperate: &mut C,
) -> Result<Reading, ReadError> {
    let (usage, failed) = openai::read_chat_with(
        status,
        body,
        JsonDecoder::with_string_shapes(FIELDS, STRING_SHAPES),
        native_error_shape,
        final_usage,
        cooperate,
    )
    .await?;
    Ok(Reading { usage, failed })
}

/// 스트림 event에서 읽을 필드.
pub fn decoder() -> SseDecoder {
    SseDecoder::chat_with_string_shapes(FIELDS, STRING_SHAPES)
}
/// 스트림 응답 하나를 읽는 상태 기계.
#[derive(Default)]
pub struct Stream {
    id: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
    finished: bool,
    done: bool,
    failed: bool,
    native_error: bool,
    tier_unresolved: bool,
}
impl Stream {
    /// event 하나를 받는다. 형식이 맞지 않으면 오류를 돌려주고, 그 뒤로는 사용량을 알려 주지 않는다.
    pub fn accept(&mut self, e: Event) -> Result<Step, ReadError> {
        let step = Step {
            terminal: e.values.get("$done") == Some(&Scalar::Bool(true)),
            error: e.values.contains_key("error"),
            content: Vec::new(),
        };
        let r = self.apply(e);
        if r.is_err() {
            self.failed = true;
            self.usage = None
        }
        r.map(|()| step)
    }
    fn apply(&mut self, e: Event) -> Result<(), ReadError> {
        if self.failed || self.done {
            return Err(ReadError::Protocol);
        }
        let v = &e.values;
        if e.name.is_empty() && v.get("$done") == Some(&Scalar::Bool(true)) {
            if !self.finished && !self.native_error {
                return Err(ReadError::Protocol);
            }
            self.done = true;
            return Ok(());
        }
        if self.native_error {
            return Err(ReadError::Protocol);
        }
        if v.contains_key("error") {
            if e.wire_bytes > 65536 {
                return Err(ReadError::StreamLimit);
            }
            if (!e.name.is_empty() && e.name != "error") || !native_error_shape(v) {
                return Err(ReadError::Protocol);
            }
            self.native_error = true;
            self.usage = None;
            return Ok(());
        }
        if !e.name.is_empty()
            || self.finished
            || !identity(v, "chat.completion.chunk")
            || !shape(v, true)
        {
            return Err(ReadError::Protocol);
        }
        let id = v["id"].text().ok_or(ReadError::Protocol)?;
        let model = v["model"].text().ok_or(ReadError::Protocol)?;
        if self.id.as_deref().is_some_and(|s| s != id)
            || self.model.as_deref().is_some_and(|s| s != model)
        {
            return Err(ReadError::Protocol);
        }
        self.id = Some(id.into());
        self.model = Some(model.into());
        self.tier_unresolved |= v.contains_key("service_tier");
        if v.get("choices.*.finish_reason")
            .is_some_and(|r| r != &Scalar::Null)
        {
            if !finish_reason(v) {
                return Err(ReadError::Protocol);
            }
            self.finished = true;
            self.usage = usage(v).filter(|_| !self.tier_unresolved);
        } else if v.get("usage").is_some_and(|u| u != &Scalar::Null) {
            return Err(ReadError::Protocol);
        }
        Ok(())
    }
    /// Provider가 오류 event를 보냈는지.
    pub fn provider_error(&self) -> bool {
        self.native_error
    }
    /// 스트림이 끝났을 때의 사용량과 실패 여부. 끝 event 없이 끝났으면 오류다.
    pub fn close(&self) -> Result<(Option<Usage>, bool), ReadError> {
        if (self.done || self.native_error) && !self.failed {
            Ok((self.usage.clone(), self.native_error))
        } else {
            Err(ReadError::Protocol)
        }
    }
    /// 끝 event를 받은 뒤 정산에 쓸 사용량. 오류 event 뒤에는 None이다.
    pub fn final_usage(&self) -> Option<Usage> {
        self.close().ok().and_then(|(usage, _)| usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use m_gated_adapter_kit::{
        json::{NoYield, ready as now},
        stream::RETAINED_BYTES,
    };
    use serde_json::{Value, json};
    fn usage() -> Value {
        json!({"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_cache_hit_tokens":40,"prompt_cache_miss_tokens":60,"completion_tokens_details":{"reasoning_tokens":10}})
    }
    fn chunk(end: bool) -> Value {
        json!({"id":"ds1","object":"chat.completion.chunk","model":"synthetic-text","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":if end{Some("stop")}else{None}}],"usage":if end{usage()}else{Value::Null}})
    }
    fn reduce(raw: &str) -> Result<Option<Usage>, ReadError> {
        let mut p = decoder();
        let mut r = Stream::default();
        for b in raw.bytes() {
            if let Some(e) = p.byte(b).map_err(|_| ReadError::Protocol)? {
                r.accept(e)?;
            }
        }
        p.finish().map_err(|_| ReadError::Protocol)?;
        assert!(p.retained_peak() <= RETAINED_BYTES);
        r.close().map(|(usage, _)| usage)
    }
    fn profile() -> Profile {
        Profile {
            model: "synthetic-text".into(),
            features: vec![Feature::Thinking],
            approved_beta: Vec::new(),
            max_output_tokens: Some(100),
            rates: ["input", "output", "cache_read"].map(String::from).to_vec(),
        }
    }
    fn wire(v: Value) -> String {
        format!("data: {v}\n\n")
    }
    #[test]
    fn last_normal_chunk_not_openai_usage_only() {
        let expected = reduce(&(wire(chunk(false)) + &wire(chunk(true)) + "data: [DONE]\n\n"))
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                expected.get(UsageCategory::Input),
                expected.get(UsageCategory::Output),
                expected.get(UsageCategory::CacheRead),
                expected.reasoning_tokens
            ),
            (60, 20, 40, Some(10))
        );
        let mut empty = chunk(true);
        empty["choices"] = json!([]);
        assert!(reduce(&(wire(chunk(false)) + &wire(empty) + "data: [DONE]\n\n")).is_err());
        for tail in [
            "",
            "data: [DONE]\n\ndata: [DONE]\n\n",
            "data: [DONE]\n\ndata: {bad}\n\n",
        ] {
            assert!(reduce(&(wire(chunk(true)) + tail)).is_err());
        }
        assert!(reduce(&(wire(chunk(true)) + &wire(chunk(true)) + "data: [DONE]\n\n")).is_err());
    }
    #[test]
    fn cache_total_reasoning_and_unknown_categories_do_not_guess() {
        for key in [
            "prompt_tokens",
            "prompt_cache_hit_tokens",
            "prompt_cache_miss_tokens",
            "completion_tokens",
            "total_tokens",
        ] {
            for value in [
                Value::Null,
                json!(-1),
                json!(1.5),
                json!("1"),
                json!(9223372036854775808u64),
            ] {
                let mut c = chunk(true);
                c["usage"][key] = value;
                assert!(reduce(&(wire(c) + "data: [DONE]\n\n")).unwrap().is_none());
            }
        }
        for (key, value) in [
            ("prompt_cache_miss_tokens", json!(59)),
            ("total_tokens", json!(121)),
            ("unverified_billing", json!(0)),
            ("prompt_tokens_details", json!({"cached_tokens":39})),
            ("completion_tokens_details", json!({"reasoning_tokens":21})),
            ("completion_tokens_details", json!({})),
        ] {
            let mut c = chunk(true);
            c["usage"][key] = value;
            assert!(reduce(&(wire(c) + "data: [DONE]\n\n")).unwrap().is_none());
        }
        for absent in [
            "prompt_cache_hit_tokens",
            "prompt_cache_miss_tokens",
            "prompt_tokens",
            "completion_tokens",
            "total_tokens",
        ] {
            let mut c = chunk(true);
            c["usage"].as_object_mut().unwrap().remove(absent);
            assert!(reduce(&(wire(c) + "data: [DONE]\n\n")).unwrap().is_none());
        }
        let mut no_reasoning = chunk(true);
        no_reasoning["usage"]
            .as_object_mut()
            .unwrap()
            .remove("completion_tokens_details");
        let u = reduce(&(wire(no_reasoning) + "data: [DONE]\n\n"))
            .unwrap()
            .unwrap();
        assert_eq!(u.reasoning_tokens, None);
    }
    #[test]
    fn early_tier_and_metadata_conflict_are_sticky() {
        for t in [json!("default"), json!("discount"), Value::Null, json!(0)] {
            let mut first = chunk(false);
            first["service_tier"] = t;
            assert!(
                reduce(&(wire(first) + &wire(chunk(true)) + "data: [DONE]\n\n"))
                    .unwrap()
                    .is_none()
            );
        }
        for key in ["id", "model"] {
            let mut last = chunk(true);
            last[key] = json!("other");
            assert!(reduce(&(wire(chunk(false)) + &wire(last) + "data: [DONE]\n\n")).is_err());
        }
        for delta in [
            Value::Null,
            json!([]),
            json!({"role":"user"}),
            json!({"content":3}),
            json!({"reasoning_content":false}),
            json!({"tool_calls":{}}),
        ] {
            let mut c = chunk(true);
            c["choices"][0]["delta"] = delta;
            assert!(reduce(&(wire(c) + "data: [DONE]\n\n")).is_err());
        }
        let mut early = chunk(false);
        early["usage"] = usage();
        assert!(reduce(&(wire(early) + &wire(chunk(true)) + "data: [DONE]\n\n")).is_err());
    }
    #[test]
    fn native_error_cannot_resurrect_usage_and_is_bounded() {
        for name in ["", "event: error\n"] {
            let error = format!(
                "{name}data: {{\"error\":{{\"message\":\"{}\",\"type\":\"server_error\",\"code\":\"synthetic_stream_error\"}}}}\n\n",
                "x".repeat(50000)
            );
            for prefix in [String::new(), wire(chunk(true))] {
                assert!(matches!(reduce(&(prefix.clone() + &error)), Ok(None)));
                assert!(matches!(
                    reduce(&(prefix + &error + "data: [DONE]\n\n")),
                    Ok(None)
                ));
            }
            assert!(reduce(&(error + &wire(chunk(true)))).is_err());
        }
        for size in [65535, 65536, 65537] {
            let mut error =
                "data: {\"error\":{\"message\":\"\",\"type\":\"server_error\"}}\n\n".to_string();
            error = error.replacen(
                "\"message\":\"\"",
                &format!("\"message\":\"{}\"", "x".repeat(size - error.len())),
                1,
            );
            assert_eq!(error.len(), size);
            assert_eq!(reduce(&error).is_ok(), size <= 65536);
        }
        for error in [json!(null), json!({}), json!({"message":"x","type":3})] {
            assert!(reduce(&format!("data: {{\"error\":{error}}}\n\n")).is_err());
        }
    }
    #[test]
    fn nonstream_error_and_large_content_are_independent() {
        let mut v = chunk(true);
        v["object"] = json!("chat.completion");
        let delta = v["choices"][0]
            .as_object_mut()
            .unwrap()
            .remove("delta")
            .unwrap();
        v["choices"][0]["message"] = delta;
        for content in [json!(null), json!("x".repeat(512 * 1024))] {
            v["choices"][0]["message"]["content"] = content;
            let raw = v.to_string();
            let mut p = JsonDecoder::with_string_shapes(FIELDS, STRING_SHAPES);
            for b in raw.bytes() {
                p.byte(b).unwrap();
            }
            let values = p.finish().unwrap();
            assert!(final_usage(&values).is_some());
            for error in [json!({}), json!(false), json!("x")] {
                let mut corrupted = v.clone();
                corrupted["error"] = error;
                let mut p = JsonDecoder::with_string_shapes(FIELDS, STRING_SHAPES);
                for b in corrupted.to_string().bytes() {
                    p.byte(b).unwrap();
                }
                assert!(final_usage(&p.finish().unwrap()).is_none());
            }
        }
    }
    #[test]
    fn validate_reports_metadata_and_model_mismatch() {
        let body = json!({"model":"synthetic-text","messages":[{"role":"user","content":"x"}],"max_tokens":10,"stream":true});
        assert_eq!(
            now(validate_with(
                body.to_string().as_bytes(),
                &profile(),
                NoYield
            )),
            Ok(RequestMetadata {
                model: "synthetic-text".into(),
                output_limit: Some(10),
                stream: true,
            })
        );
        let mut other = body.clone();
        other["model"] = json!("other");
        assert_eq!(
            now(validate_with(
                other.to_string().as_bytes(),
                &profile(),
                NoYield
            )),
            Err(ApiError::UnsupportedModel)
        );
        let mut large = body;
        large["max_tokens"] = json!(101);
        assert_eq!(
            now(validate_with(
                large.to_string().as_bytes(),
                &profile(),
                NoYield
            )),
            Err(ApiError::InvalidRequest)
        );
    }
    #[test]
    fn profile_needs_three_rates_and_no_images() {
        assert_eq!(check_profile(&profile()), Ok(()));
        let mut p = profile();
        p.rates.pop();
        assert_eq!(check_profile(&p), Err(ProfileError::InvalidPrice));
        let mut p = profile();
        p.features.push(Feature::InlinePng);
        assert_eq!(check_profile(&p), Err(ProfileError::InvalidProfile));
        let mut p = profile();
        p.approved_beta.push("x".into());
        assert_eq!(check_profile(&p), Err(ProfileError::InvalidProfile));
    }
    #[test]
    fn read_success_provider_error_and_error_body() {
        let mut v = chunk(true);
        v["object"] = json!("chat.completion");
        let delta = v["choices"][0]
            .as_object_mut()
            .unwrap()
            .remove("delta")
            .unwrap();
        v["choices"][0]["message"] = delta;
        let read = |status, body: &str| now(read_with(status, body.as_bytes(), &mut NoYield));
        let ok = read(200, &v.to_string()).unwrap();
        assert!(!ok.failed);
        assert_eq!(ok.usage.unwrap().get(UsageCategory::Input), 60);

        let error =
            |message: &str| json!({"error":{"message":message,"type":"server_error"}}).to_string();
        assert_eq!(
            read(200, &error("x")),
            Ok(Reading {
                usage: None,
                failed: true
            })
        );
        assert_eq!(
            read(200, &error(&"x".repeat(65536))),
            Err(ReadError::TooLarge)
        );
        assert_eq!(
            read(429, &error(&"x".repeat(65536))),
            Ok(Reading {
                usage: None,
                failed: false
            })
        );
        assert_eq!(read(500, "{}"), Err(ReadError::Protocol));
        assert_eq!(read(500, "not json"), Err(ReadError::Protocol));
    }
}
