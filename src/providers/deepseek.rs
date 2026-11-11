//! DeepSeek 고유 text/tools/thinking/JSON·hit/miss·마지막 일반 chunk 계약.
use super::{
    anthropic::{ApiError, RequestMetadata},
    transport::UpstreamError,
};
use crate::{
    config::{ApiKind, ModelProfile, NativeFeature},
    json::{self, JsonError, Kind, Visitor},
    ledger::finance::FinalUsage,
    stream::{Event, Scalar},
};
use std::collections::{BTreeMap, BTreeSet};
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
    profile: &'a ModelProfile,
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
    fn feature(&mut self, f: NativeFeature) -> Result<(), JsonError> {
        if self.profile.features.contains(&f) {
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
                    self.feature(NativeFeature::ClientTools)?;
                    Kind::Array
                }
                "tool_choice" => {
                    self.feature(NativeFeature::ClientTools)?;
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
                    self.feature(NativeFeature::JsonSchema)?;
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
            self.feature(NativeFeature::Thinking)?;
            self.reasoning = true;
            Kind::String
        } else if at(p, &["messages", "*", "tool_calls"]) {
            self.feature(NativeFeature::ClientTools)?;
            self.tools = true;
            Kind::Array
        } else if at(p, &["messages", "*", "tool_call_id"]) {
            self.feature(NativeFeature::ClientTools)?;
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
pub async fn validate(body: &[u8], profile: &ModelProfile) -> Result<RequestMetadata, ApiError> {
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
    if profile.api != ApiKind::DeepseekChat {
        return Err(ApiError::UnsupportedOption);
    }
    if json::scan(body, &mut v).await.is_err() {
        return Err(if v.unsupported {
            ApiError::UnsupportedOption
        } else {
            ApiError::InvalidRequest
        });
    }
    // 생략은 native enabled다. 명시 disabled만 미검증 thinking의 opt-out이며 원본을 수정하지 않는다.
    if !v.thinking_disabled && !profile.features.contains(&NativeFeature::Thinking) {
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
pub fn verified_input_bound(p: &ModelProfile) -> Option<u64> {
    let c = p.input_bound_contract.as_ref()?;
    (p.api == ApiKind::DeepseekChat
        && c.kind == "deepseek_chat_context_v1"
        && c.context_tokens == p.max_input_tokens
        && c.context_tokens > 0
        && c.context_tokens <= 1_000_000_000
        && c.input_categories == ["messages"]
        && !p.profile_version.is_empty()
        && !p.fixture_version.is_empty())
    .then_some(c.context_tokens)
}
pub(super) const FIELDS: &[&str] = &[
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
pub(super) const STRING_SHAPES: &[&str] = &[
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
fn usage(v: &BTreeMap<String, Scalar>) -> Option<FinalUsage> {
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
    Some(FinalUsage {
        actual_model: v.get("model")?.text()?.into(),
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: hit,
        cache_write_tokens: 0,
        reasoning_tokens: reasoning,
    })
}
pub(super) fn final_usage(v: &BTreeMap<String, Scalar>) -> Option<FinalUsage> {
    if v.get("error").is_some_and(|v| v != &Scalar::Null)
        || !identity(v, "chat.completion")
        || !shape(v, false)
        || !finish_reason(v)
    {
        return None;
    }
    usage(v)
}
pub(super) fn native_error_shape(v: &BTreeMap<String, Scalar>) -> bool {
    v.get("error") == Some(&Scalar::Object)
        && v.get("error.message") == Some(&Scalar::StringShape)
        && v.get("error.type") == Some(&Scalar::StringShape)
        && v.iter().all(|(k, v)| match k.as_str() {
            "error" | "error.message" | "error.type" => true,
            "error.param" | "error.code" => matches!(v, Scalar::Null | Scalar::StringShape),
            _ => false,
        })
}
#[derive(Default)]
pub struct DeepseekStream {
    id: Option<String>,
    model: Option<String>,
    usage: Option<FinalUsage>,
    finished: bool,
    done: bool,
    failed: bool,
    native_error: bool,
    tier_unresolved: bool,
}
impl DeepseekStream {
    pub fn event(&mut self, e: Event) -> Result<(), UpstreamError> {
        let r = self.apply(e);
        if r.is_err() {
            self.failed = true;
            self.usage = None
        }
        r
    }
    fn apply(&mut self, e: Event) -> Result<(), UpstreamError> {
        if self.failed || self.done {
            return Err(UpstreamError::Protocol);
        }
        let v = &e.values;
        if e.name.is_empty() && v.get("$done") == Some(&Scalar::Bool(true)) {
            if !self.finished && !self.native_error {
                return Err(UpstreamError::Protocol);
            }
            self.done = true;
            return Ok(());
        }
        if self.native_error {
            return Err(UpstreamError::Protocol);
        }
        if v.contains_key("error") {
            if e.wire_bytes > 65536 {
                return Err(UpstreamError::StreamLimit);
            }
            if (!e.name.is_empty() && e.name != "error") || !native_error_shape(v) {
                return Err(UpstreamError::Protocol);
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
            return Err(UpstreamError::Protocol);
        }
        let id = v["id"].text().ok_or(UpstreamError::Protocol)?;
        let model = v["model"].text().ok_or(UpstreamError::Protocol)?;
        if self.id.as_deref().is_some_and(|s| s != id)
            || self.model.as_deref().is_some_and(|s| s != model)
        {
            return Err(UpstreamError::Protocol);
        }
        self.id = Some(id.into());
        self.model = Some(model.into());
        self.tier_unresolved |= v.contains_key("service_tier");
        if v.get("choices.*.finish_reason")
            .is_some_and(|r| r != &Scalar::Null)
        {
            if !finish_reason(v) {
                return Err(UpstreamError::Protocol);
            }
            self.finished = true;
            self.usage = usage(v).filter(|_| !self.tier_unresolved);
        } else if v.get("usage").is_some_and(|u| u != &Scalar::Null) {
            return Err(UpstreamError::Protocol);
        }
        Ok(())
    }
    pub(super) fn native_error(&self) -> bool {
        self.native_error
    }
    pub fn finish(&self) -> Result<Option<FinalUsage>, UpstreamError> {
        if (self.done || self.native_error) && !self.failed {
            Ok(self.usage.clone())
        } else {
            Err(UpstreamError::Protocol)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{JsonDecoder, SseDecoder};
    use serde_json::{Value, json};
    fn usage() -> Value {
        json!({"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_cache_hit_tokens":40,"prompt_cache_miss_tokens":60,"completion_tokens_details":{"reasoning_tokens":10}})
    }
    fn chunk(end: bool) -> Value {
        json!({"id":"ds1","object":"chat.completion.chunk","model":"synthetic-text","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":if end{Some("stop")}else{None}}],"usage":if end{usage()}else{Value::Null}})
    }
    fn reduce(raw: &str) -> Result<Option<FinalUsage>, UpstreamError> {
        let mut p = SseDecoder::chat_with_string_shapes(FIELDS, STRING_SHAPES);
        let mut r = DeepseekStream::default();
        for b in raw.bytes() {
            if let Some(e) = p.byte(b).map_err(|_| UpstreamError::Protocol)? {
                r.event(e)?
            }
        }
        p.finish().map_err(|_| UpstreamError::Protocol)?;
        assert!(p.retained_peak() <= crate::stream::RETAINED_BYTES);
        r.finish()
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
                expected.input_tokens,
                expected.output_tokens,
                expected.cache_read_tokens,
                expected.reasoning_tokens
            ),
            (100, 20, 40, Some(10))
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
}
