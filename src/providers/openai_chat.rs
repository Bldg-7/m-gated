//! Chat Completions 전용 입력/usage 계약. Responses·DeepSeek 종료 규칙을 승계하지 않는다.
use super::{
    anthropic::{self, ApiError, RequestMetadata},
    transport::UpstreamError,
};
use crate::{
    config::{ModelProfile, NativeFeature},
    json::{self, JsonError, Kind, Visitor},
    ledger::finance::FinalUsage,
    stream::{Event, Scalar},
};
use std::collections::{BTreeMap, BTreeSet};
fn at(p: &[String], s: &[&str]) -> bool {
    p.iter().map(String::as_str).eq(s.iter().copied())
}
fn int(s: &str) -> Result<u64, JsonError> {
    if s.len() > 19 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(JsonError);
    }
    s.parse::<u64>()
        .ok()
        .filter(|n| *n <= i64::MAX as u64)
        .ok_or(JsonError)
}
fn exact(keys: &BTreeSet<String>, required: &[&str], allowed: &[&str]) -> Result<(), JsonError> {
    if required.iter().all(|k| keys.contains(*k))
        && keys.iter().all(|k| allowed.contains(&k.as_str()))
    {
        Ok(())
    } else {
        Err(JsonError)
    }
}
struct Validation<'a> {
    profile: &'a ModelProfile,
    model: Option<String>,
    output: Option<u64>,
    stream: bool,
    usage: bool,
    stream_options: bool,
    types: BTreeMap<Vec<String>, String>,
    role: Option<String>,
    image: bool,
    call: bool,
    null_content: bool,
    array_content: bool,
    images: Vec<std::ops::Range<usize>>,
    start: usize,
    unsupported: bool,
}
impl Validation<'_> {
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
        ]) || p.starts_with(&[
            "response_format".into(),
            "json_schema".into(),
            "schema".into(),
        ])
    }
}
impl Visitor for Validation<'_> {
    fn value(&mut self, p: &[String], k: Kind, r: &str) -> Result<(), JsonError> {
        if p.is_empty() {
            return if k == Kind::Object {
                Ok(())
            } else {
                Err(JsonError)
            };
        }
        if Self::schema(p) {
            return if (at(p, &["tools", "*", "function", "parameters"])
                || at(p, &["response_format", "json_schema", "schema"]))
                && k != Kind::Object
            {
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
                "max_tokens" | "max_completion_tokens" => {
                    if self.profile.chat_output_limit_field.as_deref() != Some(&p[0])
                        || self.output.is_some()
                    {
                        self.unsupported = true;
                        return Err(JsonError);
                    }
                    self.output = Some(int(r)?);
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
                "n" => {
                    return if k == Kind::Number && r == "1" {
                        Ok(())
                    } else {
                        Err(JsonError)
                    };
                }
                "store" => {
                    return if k == Kind::Bool && r == "false" {
                        Ok(())
                    } else {
                        Err(JsonError)
                    };
                }
                "tools" => {
                    self.feature(NativeFeature::ClientTools)?;
                    Kind::Array
                }
                "parallel_tool_calls" => {
                    self.feature(NativeFeature::ClientTools)?;
                    Kind::Bool
                }
                "tool_choice" => {
                    self.feature(NativeFeature::ClientTools)?;
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
                "reasoning_effort" => {
                    self.feature(NativeFeature::Thinking)?;
                    return if ["low", "medium", "high"].contains(&json::string(r, 16)?.as_str()) {
                        Ok(())
                    } else {
                        Err(JsonError)
                    };
                }
                "response_format" => {
                    self.feature(NativeFeature::JsonSchema)?;
                    Kind::Object
                }
                "temperature" | "top_p" => {
                    return if k == Kind::Number
                        && r.len() <= 64
                        && r.parse::<f64>().is_ok_and(|n| {
                            (0.0..=if p[0] == "temperature" { 2.0 } else { 1.0 }).contains(&n)
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
            self.image = false;
            self.call = false;
            self.null_content = false;
            self.array_content = false;
            Kind::Object
        } else if at(p, &["messages", "*", "role"]) {
            let role = json::string(r, 16)?;
            if !["user", "assistant", "system", "developer", "tool"].contains(&role.as_str()) {
                return Err(JsonError);
            }
            self.role = Some(role);
            Kind::String
        } else if at(p, &["messages", "*", "content"]) {
            self.null_content = k == Kind::Null;
            self.array_content = k == Kind::Array;
            if matches!(k, Kind::String | Kind::Null) {
                return Ok(());
            }
            Kind::Array
        } else if at(p, &["messages", "*", "name"]) {
            Kind::String
        } else if at(p, &["messages", "*", "tool_call_id"]) {
            self.call = true;
            self.feature(NativeFeature::ClientTools)?;
            if json::string(r, 200)?.is_empty() {
                return Err(JsonError);
            }
            Kind::String
        } else if at(p, &["messages", "*", "tool_calls"]) {
            self.call = true;
            self.feature(NativeFeature::ClientTools)?;
            Kind::Array
        } else if at(p, &["messages", "*", "content", "*"])
            || at(p, &["messages", "*", "tool_calls", "*"])
            || at(p, &["tools", "*"])
        {
            Kind::Object
        } else if p.last().is_some_and(|s| s == "type")
            && (at(&p[..p.len() - 1], &["messages", "*", "content", "*"])
                || at(&p[..p.len() - 1], &["messages", "*", "tool_calls", "*"])
                || at(&p[..p.len() - 1], &["tools", "*"])
                || at(&p[..p.len() - 1], &["tool_choice"])
                || at(&p[..p.len() - 1], &["response_format"]))
        {
            self.types
                .insert(p[..p.len() - 1].to_vec(), json::string(r, 32)?);
            Kind::String
        } else if at(p, &["messages", "*", "content", "*", "text"]) {
            Kind::String
        } else if at(p, &["messages", "*", "content", "*", "image_url"]) {
            self.image = true;
            self.feature(NativeFeature::InlinePng)?;
            Kind::Object
        } else if at(p, &["messages", "*", "content", "*", "image_url", "url"]) {
            if k != Kind::String || !r.starts_with("\"data:image/png;base64,") {
                self.unsupported = true;
                return Err(JsonError);
            }
            let start = r.as_ptr() as usize - self.start + "\"data:image/png;base64,".len();
            self.images
                .push(start..start + r.len() - "\"data:image/png;base64,".len() - 1);
            if self.images.len() > 20 {
                return Err(JsonError);
            }
            Kind::String
        } else if at(p, &["messages", "*", "content", "*", "image_url", "detail"]) {
            return if json::string(r, 16)? == "auto" {
                Ok(())
            } else {
                Err(JsonError)
            };
        } else if at(p, &["tools", "*", "function"])
            || at(p, &["messages", "*", "tool_calls", "*", "function"])
            || at(p, &["tool_choice", "function"])
            || at(p, &["response_format", "json_schema"])
        {
            Kind::Object
        } else if at(p, &["messages", "*", "tool_calls", "*", "id"]) {
            if json::string(r, 200)?.is_empty() {
                return Err(JsonError);
            }
            Kind::String
        } else if at(
            p,
            &["messages", "*", "tool_calls", "*", "function", "arguments"],
        ) {
            Kind::String
        } else if at(p, &["tools", "*", "function", "name"])
            || at(p, &["messages", "*", "tool_calls", "*", "function", "name"])
            || at(p, &["tool_choice", "function", "name"])
            || at(p, &["response_format", "json_schema", "name"])
        {
            if json::string(r, 200)?.is_empty() {
                return Err(JsonError);
            }
            Kind::String
        } else if at(p, &["tools", "*", "function", "description"])
            || at(p, &["response_format", "json_schema", "description"])
        {
            Kind::String
        } else if at(p, &["tools", "*", "function", "strict"])
            || at(p, &["response_format", "json_schema", "strict"])
        {
            Kind::Bool
        } else if at(p, &["stream_options", "include_usage"]) {
            self.usage = r == "true";
            Kind::Bool
        } else {
            self.unsupported = true;
            return Err(JsonError);
        };
        if k == want { Ok(()) } else { Err(JsonError) }
    }
    fn object_end(
        &mut self,
        p: &[String],
        keys: &BTreeSet<String>,
        r: &str,
    ) -> Result<(), JsonError> {
        if Self::schema(p) {
            return if r.len() > 64 * 1024 {
                Err(JsonError)
            } else {
                Ok(())
            };
        }
        if p.is_empty() {
            let field = self
                .profile
                .chat_output_limit_field
                .as_deref()
                .ok_or(JsonError)?;
            return exact(
                keys,
                &["model", "messages", field],
                &[
                    "model",
                    "messages",
                    field,
                    "stream",
                    "stream_options",
                    "n",
                    "store",
                    "tools",
                    "tool_choice",
                    "parallel_tool_calls",
                    "reasoning_effort",
                    "response_format",
                    "temperature",
                    "top_p",
                ],
            );
        }
        if at(p, &["messages", "*"]) {
            let role = self.role.as_deref().ok_or(JsonError)?;
            if (self.null_content && (role != "assistant" || !keys.contains("tool_calls")))
                || (self.array_content && role == "tool")
                || (self.image && role != "user")
                || (self.call && !["assistant", "tool"].contains(&role))
            {
                return Err(JsonError);
            }
            return match role {
                "assistant" => {
                    if !keys.contains("content") && !keys.contains("tool_calls") {
                        return Err(JsonError);
                    }
                    exact(keys, &["role"], &["role", "content", "name", "tool_calls"])
                }
                "tool" => exact(
                    keys,
                    &["role", "content", "tool_call_id"],
                    &["role", "content", "tool_call_id"],
                ),
                _ => exact(keys, &["role", "content"], &["role", "content", "name"]),
            };
        }
        let t = self.types.remove(p);
        if at(p, &["messages", "*", "content", "*"]) {
            return match t.as_deref() {
                Some("text") => exact(keys, &["type", "text"], &["type", "text"]),
                Some("image_url") => exact(keys, &["type", "image_url"], &["type", "image_url"]),
                _ => Err(JsonError),
            };
        }
        if at(p, &["messages", "*", "content", "*", "image_url"]) {
            return exact(keys, &["url"], &["url", "detail"]);
        }
        if at(p, &["tools", "*"])
            || at(p, &["messages", "*", "tool_calls", "*"])
            || at(p, &["tool_choice"])
        {
            if t.as_deref() != Some("function") {
                return Err(JsonError);
            }
            let call = at(p, &["messages", "*", "tool_calls", "*"]);
            return exact(
                keys,
                if call {
                    &["id", "type", "function"]
                } else {
                    &["type", "function"]
                },
                if call {
                    &["id", "type", "function"]
                } else {
                    &["type", "function"]
                },
            );
        }
        if at(p, &["tools", "*", "function"]) {
            return exact(
                keys,
                &["name", "parameters"],
                &["name", "parameters", "description", "strict"],
            );
        }
        if at(p, &["messages", "*", "tool_calls", "*", "function"]) {
            return exact(keys, &["name", "arguments"], &["name", "arguments"]);
        }
        if at(p, &["tool_choice", "function"]) {
            return exact(keys, &["name"], &["name"]);
        }
        if at(p, &["stream_options"]) {
            return exact(keys, &["include_usage"], &["include_usage"]);
        }
        if at(p, &["response_format"]) {
            return match t.as_deref() {
                Some("text" | "json_object") => exact(keys, &["type"], &["type"]),
                Some("json_schema") => {
                    exact(keys, &["type", "json_schema"], &["type", "json_schema"])
                }
                _ => Err(JsonError),
            };
        }
        if at(p, &["response_format", "json_schema"]) {
            return exact(
                keys,
                &["name", "schema"],
                &["name", "schema", "strict", "description"],
            );
        }
        Err(JsonError)
    }
    fn array_end(&mut self, p: &[String], n: usize, _: &str) -> Result<(), JsonError> {
        if (at(p, &["messages"]) && n == 0)
            || ((at(p, &["tools"]) || at(p, &["messages", "*", "tool_calls"]))
                && (n == 0 || n > 64))
            || (at(p, &["messages", "*", "content"]) && n == 0)
        {
            Err(JsonError)
        } else {
            Ok(())
        }
    }
}
pub async fn validate(body: &[u8], profile: &ModelProfile) -> Result<RequestMetadata, ApiError> {
    let mut v = Validation {
        profile,
        model: None,
        output: None,
        stream: false,
        usage: false,
        stream_options: false,
        types: BTreeMap::new(),
        role: None,
        image: false,
        call: false,
        null_content: false,
        array_content: false,
        images: Vec::new(),
        start: body.as_ptr() as usize,
        unsupported: false,
    };
    if json::scan(body, &mut v).await.is_err() {
        return Err(if v.unsupported {
            ApiError::UnsupportedOption
        } else {
            ApiError::InvalidRequest
        });
    }
    if (v.stream && !v.usage)
        || (!v.stream && v.stream_options)
        || !v
            .output
            .is_some_and(|n| n > 0 && profile.max_output_tokens.is_some_and(|max| n <= max))
    {
        return Err(ApiError::InvalidRequest);
    }
    let mut total = 0;
    for r in v.images {
        total += anthropic::validate_png_input(&body[r])
            .await
            .map_err(|_| ApiError::InvalidRequest)?;
        if total > 4 * 1024 * 1024 {
            return Err(ApiError::InvalidRequest);
        }
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
    "choices.*.message.refusal",
    "choices.*.message.tool_calls",
    "choices.*.delta",
    "choices.*.delta.role",
    "choices.*.delta.content",
    "choices.*.delta.refusal",
    "choices.*.delta.tool_calls",
];
pub(super) const STRING_SHAPES: &[&str] = &[
    "error.message",
    "error.type",
    "error.param",
    "error.code",
    "choices.*.message.content",
    "choices.*.message.refusal",
    "choices.*.delta.content",
    "choices.*.delta.refusal",
];
/// 내용·인수는 보관하지 않고 완성 message와 증분 delta의 최소 native shape를 구분한다.
pub(super) fn choice_shape(v: &BTreeMap<String, Scalar>, stream: bool) -> bool {
    let prefix = if stream {
        "choices.*.delta"
    } else {
        "choices.*.message"
    };
    v.get("choices") == Some(&Scalar::Array)
        && v.get("choices.*") == Some(&Scalar::Object)
        && v.get(prefix) == Some(&Scalar::Object)
        && if stream {
            v.get(&format!("{prefix}.role"))
                .is_none_or(|v| v.text() == Some("assistant"))
        } else {
            v.get(&format!("{prefix}.role")).and_then(Scalar::text) == Some("assistant")
                && v.contains_key(&format!("{prefix}.content"))
        }
        && ["content", "refusal"].iter().all(|field| {
            v.get(&format!("{prefix}.{field}"))
                .is_none_or(|v| matches!(v, Scalar::StringShape | Scalar::Null))
        })
        && v.get(&format!("{prefix}.tool_calls"))
            .is_none_or(|v| v == &Scalar::Array)
}

pub(super) fn usage(v: &BTreeMap<String, Scalar>) -> Option<FinalUsage> {
    if v.get("service_tier")
        .is_some_and(|v| v.text() != Some("default"))
    {
        return None;
    }
    let get = |s: &str| v.get(s).and_then(Scalar::integer);
    if v.get("usage") != Some(&Scalar::Object) {
        return None;
    }
    for (k, val) in v {
        if let Some(k) = k.strip_prefix("usage.") {
            let ok = match k {
                "prompt_tokens"
                | "completion_tokens"
                | "total_tokens"
                | "prompt_tokens_details.cached_tokens"
                | "completion_tokens_details.reasoning_tokens"
                | "completion_tokens_details.accepted_prediction_tokens"
                | "completion_tokens_details.rejected_prediction_tokens"
                | "prompt_tokens_details.audio_tokens"
                | "completion_tokens_details.audio_tokens" => val.integer().is_some(),
                "prompt_tokens_details" | "completion_tokens_details" => val == &Scalar::Object,
                _ => false,
            };
            if !ok {
                return None;
            }
        }
    }
    let input = get("usage.prompt_tokens")?;
    let output = get("usage.completion_tokens")?;
    if input.checked_add(output) != get("usage.total_tokens") {
        return None;
    }
    let cached = get("usage.prompt_tokens_details.cached_tokens")?;
    let reasoning = get("usage.completion_tokens_details.reasoning_tokens")?;
    if cached > input
        || reasoning > output
        || [
            "usage.prompt_tokens_details.audio_tokens",
            "usage.completion_tokens_details.audio_tokens",
            "usage.completion_tokens_details.accepted_prediction_tokens",
            "usage.completion_tokens_details.rejected_prediction_tokens",
        ]
        .iter()
        .any(|k| v.get(*k).is_some_and(|v| v.integer() != Some(0)))
    {
        return None;
    }
    Some(FinalUsage {
        actual_model: v.get("model")?.text()?.to_owned(),
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cached,
        cache_write_tokens: 0,
        reasoning_tokens: Some(reasoning),
    })
}
/// native 오류는 성공 metadata와 공존할 수 없으며 문자열 본문은 shape만 보관한다.
pub(super) fn native_error_shape(v: &BTreeMap<String, Scalar>) -> bool {
    v.get("error") == Some(&Scalar::Object)
        && v.get("error.message") == Some(&Scalar::StringShape)
        && v.get("error.type") == Some(&Scalar::StringShape)
        && v.iter().all(|(k, value)| match k.as_str() {
            "error" | "error.message" | "error.type" => true,
            "error.param" | "error.code" => matches!(value, Scalar::Null | Scalar::StringShape),
            _ => false,
        })
}
#[derive(Default)]
pub struct ChatStream {
    id: Option<String>,
    model: Option<String>,
    usage: Option<FinalUsage>,
    usage_seen: bool,
    done: bool,
    failed: bool,
    finished: bool,
    tier_unresolved: bool,
    native_error: bool,
}
impl ChatStream {
    pub fn event(&mut self, e: Event) -> Result<(), UpstreamError> {
        let result = self.apply(e);
        if result.is_err() {
            self.failed = true;
            self.usage = None
        }
        result
    }
    fn apply(&mut self, e: Event) -> Result<(), UpstreamError> {
        if self.failed || self.done {
            return Err(UpstreamError::Protocol);
        }
        let v = &e.values;
        if e.name.is_empty() && v.get("$done") == Some(&Scalar::Bool(true)) {
            if !self.native_error && (!self.usage_seen || !self.finished) {
                return Err(UpstreamError::Protocol);
            }
            self.done = true;
            return Ok(());
        }
        // 오류 뒤에는 원본의 선택 DONE 하나만 허용한다. usage/정상 chunk로 비용을 복원하지 않는다.
        if self.native_error {
            return Err(UpstreamError::Protocol);
        }
        if v.contains_key("error") {
            if e.wire_bytes > 64 * 1024 {
                return Err(UpstreamError::StreamLimit);
            }
            if (!e.name.is_empty() && e.name != "error") || !native_error_shape(v) {
                return Err(UpstreamError::Protocol);
            }
            self.native_error = true;
            self.usage = None;
            return Ok(());
        }
        if !e.name.is_empty() {
            return Err(UpstreamError::Protocol);
        }
        // 미검증 요율은 protocol 오류와 구별해 원본을 유지하되, 이후 누락/default로 복구하지 않는다.
        self.tier_unresolved |= v
            .get("service_tier")
            .is_some_and(|tier| tier.text() != Some("default"));
        if v.get("object").and_then(Scalar::text) != Some("chat.completion.chunk") {
            return Err(UpstreamError::Protocol);
        }
        let id = v
            .get("id")
            .and_then(Scalar::text)
            .filter(|s| !s.is_empty() && s.len() <= 256)
            .ok_or(UpstreamError::Protocol)?;
        let model = v
            .get("model")
            .and_then(Scalar::text)
            .filter(|s| !s.is_empty() && s.len() <= 200)
            .ok_or(UpstreamError::Protocol)?;
        if self.id.as_deref().is_some_and(|s| s != id)
            || self.model.as_deref().is_some_and(|s| s != model)
        {
            return Err(UpstreamError::Protocol);
        }
        self.id = Some(id.into());
        self.model = Some(model.into());
        if v.get("choices") != Some(&Scalar::Array) {
            return Err(UpstreamError::Protocol);
        }
        let choices = v
            .get("choices.$length")
            .and_then(Scalar::integer)
            .ok_or(UpstreamError::Protocol)?;
        if choices == 0 {
            if self.usage_seen || !self.finished {
                return Err(UpstreamError::Protocol);
            }
            self.usage_seen = true;
            self.usage = usage(v).filter(|_| !self.tier_unresolved);
        } else if choices == 1 {
            if !choice_shape(v, true)
                || self.usage_seen
                || self.finished
                || v.get("usage").is_some_and(|v| v != &Scalar::Null)
                || v.get("choices.*.index").and_then(Scalar::integer) != Some(0)
            {
                return Err(UpstreamError::Protocol);
            }
            if let Some(r) = v.get("choices.*.finish_reason")
                && r != &Scalar::Null
            {
                if !matches!(
                    r.text(),
                    Some("stop" | "length" | "tool_calls" | "content_filter")
                ) {
                    return Err(UpstreamError::Protocol);
                }
                self.finished = true;
            }
        } else {
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
    use crate::stream::SseDecoder;

    fn wire(choices: serde_json::Value, usage: serde_json::Value) -> String {
        format!(
            "data: {}\n\n",
            serde_json::json!({"id":"c1","object":"chat.completion.chunk","model":"synthetic-text","choices":choices,"usage":usage})
        )
    }
    fn final_usage() -> serde_json::Value {
        serde_json::json!({"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_tokens_details":{"cached_tokens":40},"completion_tokens_details":{"reasoning_tokens":10}})
    }
    fn reduce(raw: &str) -> Result<Option<FinalUsage>, UpstreamError> {
        let mut parser = SseDecoder::chat_with_string_shapes(FIELDS, STRING_SHAPES);
        let mut reducer = ChatStream::default();
        for b in raw.bytes() {
            if let Some(e) = parser.byte(b).map_err(|_| UpstreamError::Protocol)? {
                reducer.event(e)?;
            }
        }
        parser.finish().map_err(|_| UpstreamError::Protocol)?;
        reducer.finish()
    }
    fn native_error(name: &str, message: usize) -> String {
        format!(
            "{name}data: {{\"error\":{{\"message\":\"{}\",\"type\":\"server_error\",\"param\":null,\"code\":\"synthetic_stream_error\"}}}}\n\n",
            "x".repeat(message)
        )
    }
    #[test]
    fn stream_native_error_preserves_failed_terminal_without_success_usage() {
        for name in ["", "event: error\n"] {
            for prefix in [
                String::new(),
                wire(
                    serde_json::json!([{"index":0,"delta":{},"finish_reason":"stop"}]),
                    serde_json::Value::Null,
                ),
                wire(
                    serde_json::json!([{"index":0,"delta":{},"finish_reason":"stop"}]),
                    serde_json::Value::Null,
                ) + &wire(serde_json::json!([]), final_usage()),
            ] {
                for done in ["", "data: [DONE]\n\n"] {
                    let raw = prefix.clone() + &native_error(name, 50 * 1024) + done;
                    assert!(
                        matches!(reduce(&raw), Ok(None)),
                        "유효 native 오류는 성공usage 없는 failed terminal"
                    );
                }
            }
        }
    }
    #[test]
    fn stream_native_error_shape_and_sequence_are_api_specific() {
        for error in [
            serde_json::json!(null),
            serde_json::json!({}),
            serde_json::json!({"message":0,"type":"server_error"}),
            serde_json::json!({"message":"x","type":null}),
            serde_json::json!({"message":"x","type":"server_error","param":[]}),
            serde_json::json!({"message":"x","type":"server_error","code":{}}),
        ] {
            assert!(reduce(&format!("data: {{\"error\":{error}}}\n\n")).is_err());
        }
        for tail in [
            native_error("", 1),
            wire(serde_json::json!([]), final_usage()),
            wire(
                serde_json::json!([{"index":0,"delta":{},"finish_reason":"stop"}]),
                serde_json::Value::Null,
            ),
        ] {
            assert!(reduce(&(native_error("", 1) + &tail + "data: [DONE]\n\n")).is_err());
        }
        assert!(reduce(&native_error("event: response.failed\n", 1)).is_err());
        assert!(reduce(&(native_error("", 1) + "data: [DONE]\n\ndata: [DONE]\n\n")).is_err());
    }
    #[test]
    fn stream_native_error_large_optional_strings_are_shapes_only() {
        let raw = format!(
            "event: error\ndata: {{\"error\":{{\"message\":\"{}\",\"type\":\"{}\",\"param\":\"{}\",\"code\":\"{}\"}}}}\n\n",
            "x".repeat(18000),
            "t".repeat(18000),
            "p".repeat(18000),
            "c".repeat(1000)
        );
        let mut parser = SseDecoder::chat_with_string_shapes(FIELDS, STRING_SHAPES);
        let mut events = 0;
        for b in raw.bytes() {
            if let Some(e) = parser.byte(b).unwrap() {
                for key in ["error.message", "error.type", "error.param", "error.code"] {
                    assert_eq!(e.values.get(key), Some(&Scalar::StringShape));
                }
                events += 1;
            }
        }
        assert_eq!(events, 1);
        assert!(parser.retained_peak() <= crate::stream::RETAINED_BYTES);
        assert!(matches!(reduce(&raw), Ok(None)));
        for fields in ["", ",\"param\":null,\"code\":null"] {
            assert!(matches!(
                reduce(&format!(
                    "data: {{\"error\":{{\"message\":\"x\",\"type\":\"server_error\"{fields}}}}}\n\n"
                )),
                Ok(None)
            ));
        }
    }
    #[test]
    fn stream_native_error_wire_limit_and_retained_strings() {
        for name in ["", "event: error\n"] {
            for ending in ["\n\n", "\r\n\r\n", "\r\r"] {
                for size in [65535, 65536, 65537] {
                    let base = native_error(name, 0).replace(
                        '\n',
                        if ending == "\r\n\r\n" {
                            "\r\n"
                        } else if ending == "\r\r" {
                            "\r"
                        } else {
                            "\n"
                        },
                    );
                    let raw = base.replacen(
                        "\"message\":\"\"",
                        &format!("\"message\":\"{}\"", "x".repeat(size - base.len())),
                        1,
                    );
                    assert_eq!(raw.len(), size);
                    // 마지막 blank CR의 선택 LF는 relay가 전체 wire 상한에 합산한다.
                    assert_eq!(
                        reduce(&raw).is_ok(),
                        size <= 65536 || (size == 65537 && ending == "\r\n\r\n")
                    );
                    let mut parser = SseDecoder::chat_with_string_shapes(FIELDS, STRING_SHAPES);
                    for b in raw.bytes() {
                        parser.byte(b).unwrap();
                    }
                    assert!(parser.retained_peak() <= crate::stream::RETAINED_BYTES);
                }
            }
        }
    }
    #[test]
    fn stream_requires_real_choices_array_not_literal_length() {
        let start = wire(
            serde_json::json!([{"index":0,"delta":{},"finish_reason":"stop"}]),
            serde_json::Value::Null,
        );
        let raw =
            start + &wire(serde_json::json!({"$length":0}), final_usage()) + "data: [DONE]\n\n";
        assert!(
            reduce(&raw).is_err(),
            "choices 객체를 usage-only 배열로 오인"
        );
    }
    #[test]
    fn stream_unverified_tier_is_sticky_before_final_usage() {
        let start = wire(
            serde_json::json!([{"index":0,"delta":{"role":"assistant","content":"synthetic"},"finish_reason":null}]),
            serde_json::Value::Null,
        );
        let finish = wire(
            serde_json::json!([{"index":0,"delta":{},"finish_reason":"stop"}]),
            serde_json::Value::Null,
        );
        let final_chunk = wire(serde_json::json!([]), final_usage());
        let mut wrongly_priced = 0;
        for position in [0, 1] {
            for tier in [
                serde_json::json!("flex"),
                serde_json::json!("priority"),
                serde_json::json!(0),
                serde_json::json!(null),
                serde_json::json!({}),
                serde_json::json!(true),
            ] {
                for final_default in [false, true] {
                    let mut chunks = [start.clone(), finish.clone(), final_chunk.clone()];
                    chunks[position] =
                        chunks[position].replacen("{", &format!("{{\"service_tier\":{tier},"), 1);
                    if final_default {
                        chunks[2] = chunks[2].replacen("{", "{\"service_tier\":\"default\",", 1);
                    }
                    let raw = chunks.join("") + "data: [DONE]\n\n";
                    wrongly_priced += usize::from(reduce(&raw).unwrap().is_some());
                }
            }
        }
        assert_eq!(
            wrongly_priced, 0,
            "앞 chunk의 미검증 tier를 뒤 usage가 복구"
        );
    }
    #[test]
    fn stream_delta_shape_and_default_tier_matrix() {
        for delta in [
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!(0),
            serde_json::json!({"role":"user"}),
            serde_json::json!({"role":null}),
            serde_json::json!({"content":1}),
            serde_json::json!({"content":[]}),
            serde_json::json!({"refusal":{}}),
            serde_json::json!({"tool_calls":{}}),
        ] {
            let raw = wire(
                serde_json::json!([{"index":0,"delta":delta,"finish_reason":"stop"}]),
                serde_json::Value::Null,
            ) + &wire(serde_json::json!([]), final_usage())
                + "data: [DONE]\n\n";
            assert!(reduce(&raw).is_err(), "Object/native delta shape 위반 거부");
        }
        for delta in [
            serde_json::json!({}),
            serde_json::json!({"role":"assistant","content":null}),
            serde_json::json!({"content":"x".repeat(256*1024)}),
            serde_json::json!({"refusal":"synthetic"}),
            serde_json::json!({"tool_calls":[{"index":0,"function":{"arguments":"}fragment"}},{"index":1,"function":{"arguments":"not JSON"}}]}),
        ] {
            for tier_mask in 0..8 {
                let mut chunks = [
                    wire(
                        serde_json::json!([{"index":0,"delta":delta,"finish_reason":null}]),
                        serde_json::Value::Null,
                    ),
                    wire(
                        serde_json::json!([{"index":0,"delta":{},"finish_reason":"stop"}]),
                        serde_json::Value::Null,
                    ),
                    wire(serde_json::json!([]), final_usage()),
                ];
                for (i, chunk) in chunks.iter_mut().enumerate() {
                    if tier_mask & (1 << i) != 0 {
                        *chunk = chunk.replacen("{", "{\"service_tier\":\"default\",", 1);
                    }
                }
                assert!(
                    reduce(&(chunks.join("") + "data: [DONE]\n\n"))
                        .unwrap()
                        .is_some()
                );
            }
        }
    }
}
