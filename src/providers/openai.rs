//! OpenAI 어댑터와 Responses 계약. Chat Completions와 Embeddings의 입력·usage 계약은 각 전용 모듈이 정한다.
use super::{
    anthropic::{self, ApiError, RequestMetadata},
    port::{self, Api, BoxFuture, ErrorFormat, Reading},
    transport::UpstreamError,
};
use crate::{
    config::{ApiKind, ModelProfile, NativeFeature, ProviderId, StartupError},
    json::{self, JsonError, Kind, Visitor},
    ledger::finance::FinalUsage,
    stream::{Event, JsonDecoder, Scalar, SseDecoder},
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use std::collections::{BTreeMap, BTreeSet};

fn at(path: &[String], parts: &[&str]) -> bool {
    path.iter().map(String::as_str).eq(parts.iter().copied())
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
struct Validation<'a> {
    profile: &'a ModelProfile,
    model: Option<String>,
    output: Option<u64>,
    stream: bool,
    store: bool,
    types: BTreeMap<Vec<String>, String>,
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
    fn schema(path: &[String]) -> bool {
        path.starts_with(&["text".into(), "format".into(), "schema".into()])
            || path.starts_with(&["tools".into(), "*".into(), "parameters".into()])
    }
}
impl Visitor for Validation<'_> {
    fn value(&mut self, path: &[String], kind: Kind, raw: &str) -> Result<(), JsonError> {
        if path.is_empty() {
            return if kind == Kind::Object {
                Ok(())
            } else {
                Err(JsonError)
            };
        }
        if Self::schema(path) {
            if path.len() == 3 && kind != Kind::Object {
                return Err(JsonError);
            }
            return Ok(());
        }
        if kind == Kind::Object {
            self.types.remove(path);
        }
        let expected = if path.len() == 1 {
            match path[0].as_str() {
                "model" => {
                    self.model = Some(json::string(raw, 200)?);
                    Kind::String
                }
                "input" => {
                    if kind == Kind::String {
                        return Ok(());
                    }
                    Kind::Array
                }
                "store" => {
                    self.store = raw == "false";
                    Kind::Bool
                }
                "max_output_tokens" => {
                    self.output = Some(integer(raw)?);
                    Kind::Number
                }
                "stream" => {
                    self.stream = raw == "true";
                    Kind::Bool
                }
                "instructions" => Kind::String,
                "tools" => {
                    self.feature(NativeFeature::ClientTools)?;
                    Kind::Array
                }
                "tool_choice" => {
                    self.feature(NativeFeature::ClientTools)?;
                    if kind == Kind::String {
                        let s = json::string(raw, 16)?;
                        return if ["auto", "none", "required"].contains(&s.as_str()) {
                            Ok(())
                        } else {
                            Err(JsonError)
                        };
                    }
                    Kind::Object
                }
                "parallel_tool_calls" => {
                    self.feature(NativeFeature::ClientTools)?;
                    Kind::Bool
                }
                "reasoning" => {
                    self.feature(NativeFeature::Thinking)?;
                    Kind::Object
                }
                "text" => Kind::Object,
                "temperature" | "top_p" => {
                    if kind != Kind::Number || raw.len() > 64 {
                        return Err(JsonError);
                    }
                    let n = raw.parse::<f64>().map_err(|_| JsonError)?;
                    let max = if path[0] == "temperature" { 2.0 } else { 1.0 };
                    return if (0.0..=max).contains(&n) {
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
        } else if at(path, &["input", "*"])
            || at(path, &["input", "*", "content", "*"])
            || at(path, &["tools", "*"])
        {
            Kind::Object
        } else if at(path, &["input", "*", "role"]) {
            let role = json::string(raw, 16)?;
            return if ["user", "assistant", "system", "developer"].contains(&role.as_str()) {
                Ok(())
            } else {
                Err(JsonError)
            };
        } else if at(path, &["input", "*", "content"]) {
            if kind == Kind::String {
                return Ok(());
            }
            Kind::Array
        } else if at(path, &["input", "*", "type"])
            || at(path, &["input", "*", "content", "*", "type"])
            || at(path, &["tools", "*", "type"])
            || at(path, &["text", "format", "type"])
            || at(path, &["tool_choice", "type"])
        {
            let t = json::string(raw, 32)?;
            self.types.insert(path[..path.len() - 1].to_vec(), t);
            Kind::String
        } else if at(path, &["input", "*", "name"])
            || at(path, &["input", "*", "call_id"])
            || at(path, &["tools", "*", "name"])
            || at(path, &["text", "format", "name"])
            || at(path, &["tool_choice", "name"])
        {
            let s = json::string(raw, 200)?;
            if s.is_empty() {
                return Err(JsonError);
            }
            Kind::String
        } else if at(path, &["input", "*", "arguments"]) || at(path, &["input", "*", "output"]) {
            self.feature(NativeFeature::ClientTools)?;
            Kind::String
        } else if at(path, &["input", "*", "content", "*", "text"])
            || at(path, &["input", "*", "content", "*", "refusal"])
            || at(path, &["tools", "*", "description"])
            || at(path, &["text", "format", "description"])
        {
            Kind::String
        } else if at(path, &["input", "*", "content", "*", "image_url"]) {
            self.feature(NativeFeature::InlinePng)?;
            if kind != Kind::String || !raw.starts_with("\"data:image/png;base64,") {
                self.unsupported = true;
                return Err(JsonError);
            }
            let start = raw.as_ptr() as usize - self.start + "\"data:image/png;base64,".len();
            self.images
                .push(start..start + raw.len() - "\"data:image/png;base64,".len() - 1);
            if self.images.len() > 20 {
                return Err(JsonError);
            }
            Kind::String
        } else if at(path, &["input", "*", "content", "*", "detail"]) {
            return if json::string(raw, 16)? == "auto" {
                Ok(())
            } else {
                Err(JsonError)
            };
        } else if at(path, &["tools", "*", "strict"]) || at(path, &["text", "format", "strict"]) {
            Kind::Bool
        } else if at(path, &["text", "format"]) {
            Kind::Object
        } else if at(path, &["reasoning", "effort"]) {
            return if ["low", "medium", "high"].contains(&json::string(raw, 16)?.as_str()) {
                Ok(())
            } else {
                Err(JsonError)
            };
        } else if at(path, &["reasoning", "summary"]) {
            return if ["auto", "concise", "detailed"].contains(&json::string(raw, 16)?.as_str()) {
                Ok(())
            } else {
                Err(JsonError)
            };
        } else {
            self.unsupported = true;
            return Err(JsonError);
        };
        if kind == expected {
            Ok(())
        } else {
            Err(JsonError)
        }
    }
    fn object_end(
        &mut self,
        path: &[String],
        keys: &BTreeSet<String>,
        raw: &str,
    ) -> Result<(), JsonError> {
        if Self::schema(path) {
            return if path.len() == 3 && raw.len() > 64 * 1024 {
                Err(JsonError)
            } else {
                Ok(())
            };
        }
        let exact = |required: &[&str], allowed: &[&str]| -> Result<(), JsonError> {
            if required.iter().all(|k| keys.contains(*k))
                && keys.iter().all(|k| allowed.contains(&k.as_str()))
            {
                Ok(())
            } else {
                Err(JsonError)
            }
        };
        if path.is_empty() {
            return exact(
                &["model", "input", "store", "max_output_tokens"],
                &[
                    "model",
                    "input",
                    "store",
                    "max_output_tokens",
                    "stream",
                    "instructions",
                    "tools",
                    "tool_choice",
                    "parallel_tool_calls",
                    "reasoning",
                    "text",
                    "temperature",
                    "top_p",
                ],
            );
        }
        if at(path, &["reasoning"]) {
            return if keys.is_empty() {
                Err(JsonError)
            } else {
                Ok(())
            };
        }
        if at(path, &["text"]) {
            return exact(&["format"], &["format"]);
        }
        let t = self.types.remove(path);
        if at(path, &["input", "*"]) {
            return match t.as_deref() {
                None | Some("message") => exact(&["role", "content"], &["type", "role", "content"]),
                Some("function_call") => {
                    self.feature(NativeFeature::ClientTools)?;
                    exact(
                        &["type", "name", "call_id", "arguments"],
                        &["type", "name", "call_id", "arguments"],
                    )
                }
                Some("function_call_output") => {
                    self.feature(NativeFeature::ClientTools)?;
                    exact(
                        &["type", "call_id", "output"],
                        &["type", "call_id", "output"],
                    )
                }
                _ => Err(JsonError),
            };
        }
        if at(path, &["input", "*", "content", "*"]) {
            return match t.as_deref() {
                Some("input_text") => exact(&["type", "text"], &["type", "text"]),
                Some("input_image") => {
                    exact(&["type", "image_url"], &["type", "image_url", "detail"])
                }
                _ => Err(JsonError),
            };
        }
        if at(path, &["tools", "*"]) {
            return if t.as_deref() == Some("function") {
                exact(
                    &["type", "name", "parameters"],
                    &["type", "name", "parameters", "strict", "description"],
                )
            } else {
                Err(JsonError)
            };
        }
        if at(path, &["tool_choice"]) {
            return if t.as_deref() == Some("function") {
                exact(&["type", "name"], &["type", "name"])
            } else {
                Err(JsonError)
            };
        }
        if at(path, &["text", "format"]) {
            return match t.as_deref() {
                Some("text" | "json_object") => {
                    if t.as_deref() == Some("json_object") {
                        self.feature(NativeFeature::JsonSchema)?;
                    }
                    exact(&["type"], &["type"])
                }
                Some("json_schema") => {
                    self.feature(NativeFeature::JsonSchema)?;
                    exact(
                        &["type", "name", "schema"],
                        &["type", "name", "schema", "strict", "description"],
                    )
                }
                _ => Err(JsonError),
            };
        }
        Err(JsonError)
    }
    fn array_end(&mut self, path: &[String], items: usize, _: &str) -> Result<(), JsonError> {
        if (at(path, &["tools"]) && items > 64) || (at(path, &["input"]) && items == 0) {
            Err(JsonError)
        } else {
            Ok(())
        }
    }
}
pub async fn validate(body: &[u8], profile: &ModelProfile) -> Result<RequestMetadata, ApiError> {
    match profile.api {
        crate::config::ApiKind::OpenaiChat => {
            return super::openai_chat::validate(body, profile).await;
        }
        crate::config::ApiKind::Embeddings => {
            return super::embeddings::validate(body, profile).await;
        }
        _ => {}
    }
    let mut v = Validation {
        profile,
        model: None,
        output: None,
        stream: false,
        store: false,
        types: BTreeMap::new(),
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
    if !v.store
        || !v
            .output
            .is_some_and(|n| n > 0 && profile.max_output_tokens.is_some_and(|max| n <= max))
    {
        return Err(ApiError::InvalidRequest);
    }
    let mut total = 0;
    for range in v.images {
        total += anthropic::validate_png_input(&body[range])
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
pub fn approved_headers(headers: &HeaderMap) -> Result<HeaderMap, ApiError> {
    if !crate::http::json_content_type(headers)
        || headers
            .get_all(header::CONNECTION)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| {
                v.split(',')
                    .any(|s| s.trim().eq_ignore_ascii_case("content-type"))
            })
    {
        return Err(ApiError::UnsupportedOption);
    }
    Ok(HeaderMap::from_iter([(
        header::CONTENT_TYPE,
        headers[header::CONTENT_TYPE].clone(),
    )]))
}
const FIELDS: &[&str] = &[
    "object",
    "id",
    "model",
    "status",
    "error",
    "service_tier",
    "incomplete_details",
    "incomplete_details.reason",
    "usage.**",
];
const STREAM_FIELDS: &[&str] = &[
    "type",
    "sequence_number",
    "response.object",
    "response.id",
    "response.model",
    "response.status",
    "response.error",
    "response.service_tier",
    "response.incomplete_details",
    "response.incomplete_details.reason",
    "response.usage.**",
];
fn text<'a>(v: &'a BTreeMap<String, Scalar>, prefix: &str, key: &str) -> Option<&'a str> {
    v.get(&format!("{prefix}{key}")).and_then(Scalar::text)
}
fn number(v: &BTreeMap<String, Scalar>, prefix: &str, key: &str) -> Option<u64> {
    v.get(&format!("{prefix}{key}")).and_then(Scalar::integer)
}
fn final_usage(v: &BTreeMap<String, Scalar>, p: &str) -> Option<FinalUsage> {
    if text(v, p, "object") != Some("response")
        || !(text(v, p, "status") == Some("completed")
            || (text(v, p, "status") == Some("incomplete")
                && text(v, p, "incomplete_details.reason") == Some("max_output_tokens")))
    {
        return None;
    }
    // 이 profile의 고정 요율은 기본 tier만 검증했다. 알 수 없는 추가 요율/상충 상태는 추정 정산하지 않는다.
    if v.get(&format!("{p}service_tier"))
        .is_some_and(|v| v.text() != Some("default"))
        || v.get(&format!("{p}error"))
            .is_some_and(|v| v != &Scalar::Null)
        || (text(v, p, "status") == Some("completed")
            && v.get(&format!("{p}incomplete_details"))
                .is_some_and(|v| v != &Scalar::Null))
    {
        return None;
    }
    let model = text(v, p, "model")?.to_owned();
    if model.is_empty() || model.len() > 200 {
        return None;
    }
    let u = format!("{p}usage.");
    if v.get(&format!("{p}usage")) != Some(&Scalar::Object) {
        return None;
    }
    for (k, value) in v {
        if let Some(field) = k.strip_prefix(&u) {
            let valid = match field {
                "input_tokens"
                | "output_tokens"
                | "total_tokens"
                | "input_tokens_details.cached_tokens"
                | "output_tokens_details.reasoning_tokens" => value.integer().is_some(),
                "input_tokens_details" | "output_tokens_details" => value == &Scalar::Object,
                _ => false,
            };
            if !valid {
                return None;
            }
        }
    }
    let input = number(v, &u, "input_tokens")?;
    let output = number(v, &u, "output_tokens")?;
    if input.checked_add(output) != number(v, &u, "total_tokens") {
        return None;
    }
    let cached = number(v, &u, "input_tokens_details.cached_tokens")?;
    let reasoning = Some(number(v, &u, "output_tokens_details.reasoning_tokens")?);
    if cached > input || reasoning.is_some_and(|n| n > output) {
        return None;
    }
    Some(FinalUsage {
        actual_model: model,
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cached,
        cache_write_tokens: 0,
        reasoning_tokens: reasoning,
    })
}
#[derive(Default)]
pub struct ResponsesStream {
    started: bool,
    terminal: bool,
    sequence: Option<u64>,
    id: Option<String>,
    model: Option<String>,
    usage: Option<FinalUsage>,
    failed: bool,
    protocol_failed: bool,
}
impl ResponsesStream {
    pub fn event(&mut self, event: Event) -> Result<(), UpstreamError> {
        if self.protocol_failed {
            return Err(UpstreamError::Protocol);
        }
        let result = self.apply_event(event);
        if result.is_err() {
            self.protocol_failed = true;
            self.usage = None;
        }
        result
    }
    fn apply_event(&mut self, event: Event) -> Result<(), UpstreamError> {
        let v = event.values;
        let kind = text(&v, "", "type").ok_or(UpstreamError::Protocol)?;
        if self.terminal || (!event.name.is_empty() && event.name != kind) {
            return Err(UpstreamError::Protocol);
        }
        let seq = number(&v, "", "sequence_number").ok_or(UpstreamError::Protocol)?;
        if self.sequence.is_some_and(|n| seq <= n) {
            return Err(UpstreamError::Protocol);
        }
        self.sequence = Some(seq);
        if kind == "error" {
            if event.wire_bytes > 64 * 1024 {
                return Err(UpstreamError::StreamLimit);
            }
            self.terminal = true;
            self.failed = true;
            self.usage = None;
            return Ok(());
        }
        if self.started
            && ((text(&v, "response.", "id").is_some()
                && text(&v, "response.", "id") != self.id.as_deref())
                || (text(&v, "response.", "model").is_some()
                    && text(&v, "response.", "model") != self.model.as_deref()))
        {
            return Err(UpstreamError::Protocol);
        }
        if kind == "response.created" {
            if self.started {
                return Err(UpstreamError::Protocol);
            }
            self.started = true;
            self.id = text(&v, "response.", "id")
                .filter(|s| !s.is_empty() && s.len() <= 256)
                .map(str::to_owned);
            self.model = text(&v, "response.", "model")
                .filter(|s| !s.is_empty() && s.len() <= 200)
                .map(str::to_owned);
            if self.id.is_none()
                || self.model.is_none()
                || text(&v, "response.", "object") != Some("response")
            {
                return Err(UpstreamError::Protocol);
            }
        } else if !self.started {
            return Err(UpstreamError::Protocol);
        }
        if kind == "response.completed"
            || kind == "response.incomplete"
            || kind == "response.failed"
        {
            if text(&v, "response.", "id") != self.id.as_deref()
                || text(&v, "response.", "model") != self.model.as_deref()
                || text(&v, "response.", "status") != kind.strip_prefix("response.")
            {
                return Err(UpstreamError::Protocol);
            }
            self.usage = final_usage(&v, "response.");
            self.failed = kind == "response.failed";
            self.terminal = true;
        } else {
            if matches!(kind, "response.created" | "response.in_progress")
                && (text(&v, "response.", "object") != Some("response")
                    || text(&v, "response.", "id") != self.id.as_deref()
                    || text(&v, "response.", "model") != self.model.as_deref()
                    || text(&v, "response.", "status") != Some("in_progress"))
            {
                return Err(UpstreamError::Protocol);
            }
            if v.get("response.status")
                .is_some_and(|v| v.text() != Some("in_progress"))
                || [
                    "response.error",
                    "response.incomplete_details",
                    "response.usage",
                ]
                .iter()
                .any(|key| v.get(*key).is_some_and(|v| v != &Scalar::Null))
            {
                return Err(UpstreamError::Protocol);
            }
        }
        Ok(())
    }
    pub fn finish(&self) -> Result<Option<FinalUsage>, UpstreamError> {
        if self.terminal && !self.protocol_failed {
            Ok(self.usage.clone())
        } else {
            Err(UpstreamError::Protocol)
        }
    }
}
async fn decode(bytes: &[u8]) -> Result<BTreeMap<String, Scalar>, UpstreamError> {
    let mut p = JsonDecoder::new(FIELDS);
    for part in bytes.chunks(64 * 1024) {
        for &b in part {
            p.byte(b).map_err(|_| UpstreamError::Protocol)?;
        }
        tokio::task::yield_now().await;
    }
    p.finish().map_err(|_| UpstreamError::Protocol)
}
async fn decode_chat(
    bytes: &[u8],
    deepseek: bool,
) -> Result<BTreeMap<String, Scalar>, UpstreamError> {
    let mut p = JsonDecoder::with_string_shapes(
        if deepseek {
            super::deepseek::FIELDS
        } else {
            super::openai_chat::FIELDS
        },
        if deepseek {
            super::deepseek::STRING_SHAPES
        } else {
            super::openai_chat::STRING_SHAPES
        },
    );
    for part in bytes.chunks(64 * 1024) {
        for &b in part {
            p.byte(b).map_err(|_| UpstreamError::Protocol)?;
        }
        tokio::task::yield_now().await;
    }
    p.finish().map_err(|_| UpstreamError::Protocol)
}
/// Chat profile이 출력 상한을 `max_tokens`로 받으면 추론 토큰이 0이어야 정산한다.
fn chat_output_contract(u: &FinalUsage, profile: &ModelProfile) -> bool {
    profile.chat_output_limit_field.as_deref() != Some("max_tokens")
        || u.reasoning_tokens == Some(0)
}
fn chat_final(v: &BTreeMap<String, Scalar>) -> Option<FinalUsage> {
    if v.get("error").is_some_and(|v| v != &Scalar::Null)
        || text(v, "", "object") != Some("chat.completion")
        || text(v, "", "id").is_none_or(|s| s.is_empty() || s.len() > 256)
        || !super::openai_chat::choice_shape(v, false)
        || v.get("choices.$length").and_then(Scalar::integer) != Some(1)
        || v.get("choices.*.index").and_then(Scalar::integer) != Some(0)
        || !matches!(
            text(v, "", "choices.*.finish_reason"),
            Some("stop" | "length" | "tool_calls" | "content_filter")
        )
    {
        return None;
    }
    super::openai_chat::usage(v)
}
/// Responses, Chat Completions, DeepSeek Chat의 비스트림 응답 본문을 읽는다.
/// Embeddings의 오류 응답도 Responses와 같이 읽는다.
pub(super) async fn read_json(
    kind: ApiKind,
    status: StatusCode,
    body: &[u8],
    profile: &ModelProfile,
) -> Result<Reading, UpstreamError> {
    let deepseek = kind == ApiKind::DeepseekChat;
    let chat = deepseek || kind == ApiKind::OpenaiChat;
    let values = if chat {
        decode_chat(body, deepseek).await?
    } else {
        decode(body).await?
    };
    let chat_error =
        status == StatusCode::OK && chat && values.get("error").is_some_and(|v| v != &Scalar::Null);
    if chat_error {
        if body.len() > 64 * 1024 {
            return Err(UpstreamError::TooLarge);
        }
        if !(if deepseek {
            super::deepseek::native_error_shape(&values)
        } else {
            super::openai_chat::native_error_shape(&values)
        }) {
            return Err(UpstreamError::Protocol);
        }
    }
    let usage = if status == StatusCode::OK {
        if deepseek {
            super::deepseek::final_usage(&values)
        } else if chat {
            chat_final(&values).filter(|u| chat_output_contract(u, profile))
        } else {
            final_usage(&values, "")
        }
    } else {
        validate_error(body).await?;
        None
    };
    Ok(Reading {
        usage,
        counted_input_tokens: None,
        failed: chat_error || text(&values, "", "status") == Some("failed"),
    })
}
async fn validate_error(body: &[u8]) -> Result<(), UpstreamError> {
    let mut p = JsonDecoder::with_string_shapes(&["error"], &["error.message", "error.type"]);
    for part in body.chunks(64 * 1024) {
        for &b in part {
            p.byte(b).map_err(|_| UpstreamError::Protocol)?;
        }
        tokio::task::yield_now().await;
    }
    let v = p.finish().map_err(|_| UpstreamError::Protocol)?;
    if v.get("error") == Some(&Scalar::Object)
        && v.get("error.message") == Some(&Scalar::StringShape)
        && v.get("error.type") == Some(&Scalar::StringShape)
    {
        Ok(())
    } else {
        Err(UpstreamError::Protocol)
    }
}

/// OpenAI 호환 Provider(OpenAI, DeepSeek)가 같이 쓰는 키 규칙. Anthropic 키는 받지 않는다.
pub(super) fn bearer(secret: &str) -> Option<(HeaderName, HeaderValue)> {
    Some((
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {secret}")).ok()?,
    ))
}
pub(super) fn accepts_sk_key(secret: &str) -> bool {
    secret.starts_with("sk-") && !secret.starts_with("sk-ant-")
}
/// OpenAI 호환 Provider가 같이 쓰는 profile 규칙.
pub(super) fn check_compatible_profile(profile: &ModelProfile) -> Result<(), StartupError> {
    if !profile.approved_beta.is_empty() {
        return Err(StartupError::InvalidProfile);
    }
    if profile.features.contains(&NativeFeature::Cache) && !profile.rates.contains_key("cache_read")
    {
        return Err(StartupError::InvalidPrice);
    }
    Ok(())
}

/// OpenAI 어댑터.
pub struct Openai;
pub static PROVIDER: Openai = Openai;
static APIS: [&dyn Api; 3] = [
    &RESPONSES,
    &super::openai_chat::CHAT,
    &super::embeddings::EMBEDDINGS,
];
impl port::Provider for Openai {
    fn id(&self) -> ProviderId {
        ProviderId::Openai
    }
    fn origin(&self) -> &'static str {
        "https://api.openai.com"
    }
    fn apis(&self) -> &'static [&'static dyn Api] {
        &APIS
    }
    fn auth_header(&self, secret: &str) -> Option<(HeaderName, HeaderValue)> {
        bearer(secret)
    }
    fn accepts_key(&self, secret: &str) -> bool {
        accepts_sk_key(secret)
    }
    fn verify_path(&self) -> &'static str {
        "v1/models"
    }
    fn check_profile(&self, profile: &ModelProfile) -> Result<(), StartupError> {
        check_compatible_profile(profile)
    }
    fn models_dev_name(&self) -> Option<&'static str> {
        Some("openai")
    }
}

/// OpenAI Responses API.
pub struct Responses;
static RESPONSES: Responses = Responses;
impl Api for Responses {
    fn kind(&self) -> ApiKind {
        ApiKind::Responses
    }
    fn routes(&self) -> &'static [&'static str] {
        &["/openai/v1/responses"]
    }
    fn upstream_path(&self) -> &'static str {
        "v1/responses"
    }
    fn error_format(&self) -> ErrorFormat {
        ErrorFormat::Openai
    }
    fn validate<'a>(
        &'a self,
        body: &'a [u8],
        profile: &'a ModelProfile,
    ) -> BoxFuture<'a, Result<RequestMetadata, ApiError>> {
        Box::pin(validate(body, profile))
    }
    fn approved_headers(
        &self,
        incoming: &HeaderMap,
        _: &ModelProfile,
    ) -> Result<HeaderMap, ApiError> {
        approved_headers(incoming)
    }
    fn read<'a>(
        &'a self,
        status: StatusCode,
        body: &'a [u8],
        _request: &'a [u8],
        profile: &'a ModelProfile,
    ) -> BoxFuture<'a, Result<Reading, UpstreamError>> {
        Box::pin(read_json(ApiKind::Responses, status, body, profile))
    }
    fn stream(&self, _profile: &ModelProfile) -> Option<Box<dyn port::StreamReader>> {
        Some(Box::<ResponsesStream>::default())
    }
    fn token_rule(&self, _profile: &ModelProfile) -> &'static str {
        "openai_responses_input_includes_cache_output_includes_reasoning_v1"
    }
    fn catalog(&self) -> bool {
        true
    }
}
impl port::StreamReader for ResponsesStream {
    fn decoder(&self) -> SseDecoder {
        SseDecoder::new(STREAM_FIELDS)
    }
    fn accept(&mut self, event: Event) -> Result<port::Step, UpstreamError> {
        let kind = event.values.get("type").and_then(Scalar::text);
        let terminal = matches!(
            kind,
            Some("response.completed" | "response.incomplete" | "response.failed")
        );
        let error = kind == Some("error");
        self.event(event)?;
        Ok(port::Step {
            terminal,
            error,
            content: Vec::new(),
        })
    }
    fn final_usage(&self) -> Option<FinalUsage> {
        self.finish().ok().flatten()
    }
    fn provider_error(&self) -> bool {
        self.failed
    }
    fn close(&self) -> Result<(Option<FinalUsage>, bool), UpstreamError> {
        self.finish().map(|usage| (usage, self.failed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn profile() -> ModelProfile {
        serde_json::from_value(serde_json::json!({"model":"synthetic-text","api":"responses","profile_version":"responses-fixture-v1","pricing_version":"responses-price-v1","fixture_version":"responses-native-v1","actual_models":["synthetic-text"],"features":["client_tools","thinking","cache","json_schema","inline_png"],"max_input_tokens":4096,"max_output_tokens":1024,"max_body_bytes":8388608,"max_response_bytes":16777216,"rates":{"input":"1.000000000000","output":"2.000000000000","cache_read":"0.500000000000"}})).unwrap()
    }
    #[tokio::test]
    async fn responses_native_function_and_schema_optional_strict_is_not_injected() {
        let raw=br#" {"model":"synthetic-text","store":false,"max_output_tokens":100,"input":[{"type":"function_call","name":"f","call_id":"call_1","arguments":"{not JSON: opaque}"},{"type":"function_call_output","call_id":"call_1","output":"synthetic"}],"tools":[{"type":"function","name":"f","parameters":{"type":"object","properties":{"store":{"type":"boolean"}}}}],"text":{"format":{"type":"json_schema","name":"answer","schema":{"type":"object"}}},"reasoning":{"effort":"low","summary":"auto"}} "#;
        assert!(
            validate(raw, &profile()).await.is_ok(),
            "정상 native optional strict를 필수로 강제했다"
        );
    }
    #[tokio::test]
    async fn responses_request_fields_are_exact_and_state_cannot_hide_in_order_or_escapes() {
        let p = profile();
        for extra in [
            r#", "previous_response_id":"resp_1""#,
            r#", "conversation":"c""#,
            r#", "background":false"#,
            r#", "include":[]"#,
            r#", "truncation":"auto""#,
            r#", "metadata":{}"#,
            r#", "service_tier":"auto""#,
            r#", "stream_options":{}"#,
            r#", "audio":{}"#,
        ] {
            let raw = format!(
                r#"{{"model":"synthetic-text","input":"x","store":false,"max_output_tokens":1{extra}}}"#
            );
            assert!(validate(raw.as_bytes(), &p).await.is_err());
        }
        for raw in [
            r#"{"model":"synthetic-text","input":"x","max_output_tokens":1}"#,
            r#"{"model":"synthetic-text","input":"x","store":true,"max_output_tokens":1}"#,
            r#"{"model":"synthetic-text","input":"x","store":null,"max_output_tokens":1}"#,
            r#"{"model":"synthetic-text","input":"x","store":false,"max_output_tokens":0}"#,
            r#"{"model":"synthetic-text","input":"x","store":false,"max_output_tokens":1025}"#,
            r#"{"model":"synthetic-text","input":[{"type":"item_reference","id":"x"}],"store":false,"max_output_tokens":1}"#,
            r#"{"model":"synthetic-text","input":[{"role":"user","content":[{"type":"input_image","file_id":"file_x"}]}],"store":false,"max_output_tokens":1}"#,
            r#"{"model":"synthetic-text","input":"x","store":false,"\u0073tore":true,"max_output_tokens":1}"#,
            r#"{"model":"synthetic-text","input":"x","store":false,"max_output_tokens":1,"tools":[{"type":"web_search_preview"}]}"#,
        ] {
            assert!(
                validate(raw.as_bytes(), &p).await.is_err(),
                "미지원 요청이 통과했다"
            );
        }
        let raw=br#"{"\u006dodel":"synthetic-text","input":[{"role":"user","content":[{"type":"input_text","text":"x"}]}],"\u0073tore":false,"max_output_tokens":1}"#;
        assert!(validate(raw, &p).await.is_ok());
    }
    fn native(status: &str, reason: Option<&str>) -> String {
        serde_json::json!({"id":"resp_fixture","object":"response","model":"synthetic-text","status":status,"output":[],"incomplete_details":reason.map(|r|serde_json::json!({"reason":r})),"usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cached_tokens":40},"output_tokens_details":{"reasoning_tokens":10}}}).to_string()
    }
    #[tokio::test]
    async fn responses_terminal_usage_distinguishes_incomplete_finality_and_included_subsets() {
        for (status, reason, verified) in [
            ("completed", None, true),
            ("incomplete", Some("max_output_tokens"), true),
            ("incomplete", Some("content_filter"), false),
            ("failed", None, false),
        ] {
            let values = decode(native(status, reason).as_bytes()).await.unwrap();
            let u = final_usage(&values, "");
            assert_eq!(u.is_some(), verified);
            if let Some(u) = u {
                assert_eq!(
                    (
                        u.input_tokens,
                        u.output_tokens,
                        u.cache_read_tokens,
                        u.reasoning_tokens
                    ),
                    (100, 20, 40, Some(10))
                );
                let price = crate::ledger::finance::PricingSnapshot::from_profile(
                    crate::config::ProviderId::Openai,
                    &profile(),
                );
                assert_eq!(
                    price.price(&u, false).unwrap().to_string(),
                    "0.000120000000"
                );
            }
        }
        for (from, to) in [
            ("\"cached_tokens\":40", "\"cached_tokens\":101"),
            ("\"reasoning_tokens\":10", "\"reasoning_tokens\":21"),
            ("\"total_tokens\":120", "\"total_tokens\":121"),
            ("\"input_tokens\":100", "\"input_tokens\":\"100\""),
            ("\"output_tokens\":20", "\"output_tokens\":null"),
        ] {
            let wire = native("incomplete", Some("max_output_tokens")).replace(from, to);
            assert!(final_usage(&decode(wire.as_bytes()).await.unwrap(), "").is_none());
        }
    }
    #[test]
    fn responses_price_snapshot_survives_profile_change_and_preserves_included_counts() {
        let mut profile = profile();
        let snapshot = crate::ledger::finance::PricingSnapshot::from_profile(
            crate::config::ProviderId::Openai,
            &profile,
        );
        let usage = FinalUsage {
            actual_model: "synthetic-text".into(),
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 40,
            cache_write_tokens: 0,
            reasoning_tokens: Some(10),
        };
        profile
            .rates
            .insert("input".into(), "3.000000000000".into());
        profile.pricing_version = "new-price-v2".into();
        assert_eq!(
            snapshot.price(&usage, false).unwrap().to_string(),
            "0.000120000000"
        );
        assert_eq!(
            crate::ledger::finance::PricingSnapshot::from_profile(
                crate::config::ProviderId::Openai,
                &profile
            )
            .price(&usage, false)
            .unwrap()
            .to_string(),
            "0.000240000000"
        );
        assert_eq!(snapshot.pricing_version, "responses-price-v1");
    }
    #[tokio::test]
    async fn responses_unknown_tier_or_conflicting_status_metadata_is_not_final_usage() {
        for extra in [
            r#", "service_tier":"priority""#,
            r#", "service_tier":"flex""#,
            r#", "error":{"code":"server_error"}"#,
        ] {
            let raw = native("completed", None);
            let raw = format!("{}{extra}}}", &raw[..raw.len() - 1]);
            assert!(
                final_usage(&decode(raw.as_bytes()).await.unwrap(), "").is_none(),
                "검증되지않은요율/상충종료metadata를정산했다"
            );
        }
        let raw = native("completed", Some("max_output_tokens"));
        assert!(final_usage(&decode(raw.as_bytes()).await.unwrap(), "").is_none());
    }
    #[tokio::test]
    async fn responses_missing_detail_counts_and_prior_error_never_become_final() {
        let raw = native("completed", None);
        for (from, to) in [
            ("\"cached_tokens\":40", "\"cached_other\":40"),
            ("\"input_tokens_details\":{\"cached_tokens\":40},", ""),
            ("\"output_tokens_details\":{\"reasoning_tokens\":10},", ""),
        ] {
            let wire = raw.replace(from, to);
            assert_ne!(wire, raw);
            assert!(
                final_usage(&decode(wire.as_bytes()).await.unwrap(), "").is_none(),
                "부재한 상세 청구량을0으로추정했다"
            );
        }
        for middle in [r#"{"type":"error","sequence_number":1,"code":"server_error","message":"synthetic"}"#.to_string(),r#"{"type":"response.in_progress","sequence_number":1,"response":{"id":"resp_other","object":"response","status":"in_progress","model":"synthetic-other","usage":null}}"#.to_string()] {
            let created=r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_fixture","object":"response","model":"synthetic-text","status":"in_progress","usage":null}}"#;
            let terminal=format!(r#"{{"type":"response.completed","sequence_number":2,"response":{raw}}}"#);
            let mut parser=SseDecoder::new(STREAM_FIELDS);let mut reducer=ResponsesStream::default();let wire=format!("data: {created}\n\ndata: {middle}\n\ndata: {terminal}\n\n");
            let mut result=Ok(());for b in wire.bytes(){if let Some(e)=parser.byte(b).unwrap() && let Err(e)=reducer.event(e){result=Err(e);break}}
            assert!(result.is_err(),"이전 오류/상충 response를무시하고최종비용으로승격했다");
        }
    }
    fn stream_event(kind: &str, sequence: u64, response: &serde_json::Value) -> Event {
        let raw = serde_json::json!({"type":kind,"sequence_number":sequence,"response":response});
        let mut parser = SseDecoder::new(STREAM_FIELDS);
        let mut event = None;
        for b in format!("data: {raw}\n\n").bytes() {
            if let Some(e) = parser.byte(b).unwrap() {
                event = Some(e);
            }
        }
        parser.finish().unwrap();
        event.unwrap()
    }
    fn progress_response() -> serde_json::Value {
        serde_json::json!({"id":"resp_fixture","object":"response","model":"synthetic-text","status":"in_progress","error":null,"incomplete_details":null,"usage":null})
    }
    #[test]
    fn responses_nonterminal_contradictions_poison_reducer_even_after_valid_terminal() {
        let terminal: serde_json::Value = serde_json::from_str(&native("completed", None)).unwrap();
        let mut improperly_accepted = 0;
        for kind in ["response.created", "response.in_progress"] {
            for (field, value) in [
                ("error", serde_json::json!({"code":"server_error"})),
                ("error", serde_json::json!(false)),
                ("error", serde_json::json!("synthetic")),
                ("status", serde_json::json!("failed")),
                ("status", serde_json::json!("completed")),
                ("status", serde_json::json!("incomplete")),
                ("status", serde_json::Value::Null),
                (
                    "incomplete_details",
                    serde_json::json!({"reason":"max_output_tokens"}),
                ),
                ("incomplete_details", serde_json::json!(false)),
                ("usage", serde_json::json!({})),
            ] {
                let mut reducer = ResponsesStream::default();
                let sequence = if kind == "response.in_progress" {
                    reducer
                        .event(stream_event("response.created", 0, &progress_response()))
                        .unwrap();
                    1
                } else {
                    0
                };
                let mut response = progress_response();
                response[field] = value;
                let rejected = reducer
                    .event(stream_event(kind, sequence, &response))
                    .is_err();
                let sticky = reducer
                    .event(stream_event("response.completed", sequence + 1, &terminal))
                    .is_err();
                let not_final = reducer.finish().is_err();
                improperly_accepted += usize::from(!(rejected && sticky && not_final));
            }
        }
        assert_eq!(
            improperly_accepted, 0,
            "비종결 상충 뒤 Protocol/usage 폐기가 고정되지 않았다"
        );
    }
    #[test]
    fn responses_repeated_in_progress_and_delta_preserve_valid_terminal() {
        for status in ["completed", "incomplete"] {
            let mut reducer = ResponsesStream::default();
            reducer
                .event(stream_event("response.created", 0, &progress_response()))
                .unwrap();
            for sequence in 1..=2 {
                reducer
                    .event(stream_event(
                        "response.in_progress",
                        sequence,
                        &progress_response(),
                    ))
                    .unwrap();
            }
            let mut parser = SseDecoder::new(STREAM_FIELDS);
            for b in b"data: {\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"delta\":\"synthetic\"}\n\n" {
                if let Some(e) = parser.byte(*b).unwrap() {
                    reducer.event(e).unwrap();
                }
            }
            parser.finish().unwrap();
            let terminal = serde_json::from_str(&native(
                status,
                (status == "incomplete").then_some("max_output_tokens"),
            ))
            .unwrap();
            reducer
                .event(stream_event(&format!("response.{status}"), 4, &terminal))
                .unwrap();
            assert!(reducer.finish().unwrap().is_some());
        }
    }
    #[tokio::test]
    async fn responses_output_message_content_is_not_supported_input_for_any_role_or_order() {
        let p = profile();
        let mut accepted = 0;
        for role in ["user", "system", "developer", "assistant"] {
            for variant in ["output_text", "refusal"] {
                for role_first in [false, true] {
                    let field = if variant == "refusal" {
                        "refusal"
                    } else {
                        "text"
                    };
                    let content = format!(r#"[{{"type":"{variant}","{field}":"synthetic"}}]"#);
                    let item = if role_first {
                        format!(r#"{{"role":"{role}","content":{content}}}"#)
                    } else {
                        format!(r#"{{"content":{content},"role":"{role}"}}"#)
                    };
                    let raw = format!(
                        r#"{{"model":"synthetic-text","input":[{item}],"store":false,"max_output_tokens":1}}"#
                    );
                    accepted += usize::from(validate(raw.as_bytes(), &p).await.is_ok());
                }
            }
        }
        assert_eq!(
            accepted, 0,
            "지원하지 않는 OutputMessage content가 입력으로 허용됐다"
        );
        for content in [
            r#""synthetic""#,
            r#"[{"type":"input_text","text":"synthetic"}]"#,
        ] {
            let raw = format!(
                r#"{{"model":"synthetic-text","input":[{{"role":"assistant","content":{content}}}],"store":false,"max_output_tokens":1}}"#
            );
            assert!(validate(raw.as_bytes(), &p).await.is_ok());
        }
    }
    #[tokio::test]
    async fn responses_large_terminal_validates_all_skipped_syntax_under_retained_limit() {
        let head = "event: response.incomplete\r\ndata: {\"type\":\"response.incomplete\",\"sequence_number\":1,\"response\":{\"instructions\":\"";
        let tail = ",\"id\":\"resp_fixture\",\"object\":\"response\",\"model\":\"synthetic-text\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\r\ndata: \"usage\":{\"input_tokens\":1,\"output_tokens\":2,\"total_tokens\":3,\"input_tokens_details\":{\"cached_tokens\":0},\"output_tokens_details\":{\"reasoning_tokens\":0}},\"output\":[],\"tools\":[]}}\r\n\r\n";
        for size in [256 * 1024 + 1, 16 * 1024 * 1024, 16 * 1024 * 1024 + 1] {
            let mut parser = SseDecoder::new(STREAM_FIELDS);
            let mut events = Vec::new();
            let padding = size - head.len() - tail.len() - 1;
            let mut valid = true;
            for part in [
                head.as_bytes(),
                &vec![b'x'; padding],
                b"\"",
                tail.as_bytes(),
            ] {
                for chunk in part.chunks(64 * 1024) {
                    for &b in chunk {
                        match parser.byte(b) {
                            Ok(Some(e)) => events.push(e),
                            Ok(None) => {}
                            Err(_) => {
                                valid = false;
                                break;
                            }
                        }
                    }
                    if !valid {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                if !valid {
                    break;
                }
            }
            assert_eq!(valid, size <= 16 * 1024 * 1024);
            assert!(parser.retained_peak() <= crate::stream::RETAINED_BYTES);
            if valid {
                parser.finish().unwrap();
                assert_eq!(events.len(), 1);
                assert!(final_usage(&events[0].values, "response.").is_some());
                println!(
                    "S6_PARSER wire_bytes={size} retained_peak={}",
                    parser.retained_peak()
                );
            }
        }
        for suffix in [
            r#", "after":[1,]}"#,
            r#", "after":"\uD800x"}"#,
            r#", "usage":{}}"#,
            r#"x"#,
        ] {
            let raw = native("completed", None);
            let raw = format!("{}{suffix}", &raw[..raw.len() - 1]);
            assert!(decode(raw.as_bytes()).await.is_err());
        }
    }
}

#[cfg(test)]
mod chat_shape_tests {
    use super::*;

    fn completion() -> String {
        serde_json::json!({"id":"c1","object":"chat.completion","model":"synthetic-text","choices":[{"index":0,"message":{"role":"assistant","content":"synthetic"},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_tokens_details":{"cached_tokens":40},"completion_tokens_details":{"reasoning_tokens":10}}}).to_string()
    }
    #[tokio::test]
    async fn nonstream_error_metadata_conflict_never_produces_final_usage() {
        let mut accepted = Vec::new();
        for (i, error) in [
            r#"{"message":"synthetic failure","type":"server_error","param":null,"code":"server_error"}"#,
            "{}", "[]", "true", "0", r#""synthetic""#,
            r#"{"message":0,"type":"server_error"}"#,
            r#"{"message":"synthetic failure","type":null}"#,
        ].iter().enumerate() {
            for first in [true, false] {
                let base = completion();
                let raw = if first {
                    format!("{{\"error\":{error},{}", &base[1..])
                } else {
                    format!("{},\"error\":{error}}}", &base[..base.len()-1])
                };
                let projection = decode_chat(raw.as_bytes(), false).await.unwrap();
                if chat_final(&projection).is_some() {
                    accepted.push((i, first));
                }
            }
        }
        assert!(
            accepted.is_empty(),
            "명시 error 모순을 final usage로 승격: {accepted:?}"
        );
    }
    #[tokio::test]
    async fn nonstream_error_metadata_missing_or_null_preserves_final_usage() {
        let base = completion();
        for raw in [
            base.clone(),
            format!("{{\"error\":null,{}", &base[1..]),
            format!("{},\"error\":null}}", &base[..base.len() - 1]),
        ] {
            let projection = decode_chat(raw.as_bytes(), false).await.unwrap();
            assert!(chat_final(&projection).is_some());
        }
    }
    #[tokio::test]
    async fn nonstream_error_metadata_native_shape_retains_no_error_strings() {
        for error in [
            serde_json::json!({"message":"synthetic failure","type":"server_error"}),
            serde_json::json!({"code":null,"param":null,"type":"server_error","message":"synthetic failure"}),
            serde_json::json!({"message":"x".repeat(18000),"type":"x".repeat(18000),"param":"x".repeat(18000),"code":"x".repeat(1000)}),
        ] {
            let raw = serde_json::json!({"error":error}).to_string();
            let mut decoder = JsonDecoder::with_string_shapes(
                super::super::openai_chat::FIELDS,
                super::super::openai_chat::STRING_SHAPES,
            );
            for b in raw.bytes() {
                decoder.byte(b).unwrap();
            }
            assert!(decoder.retained_peak() <= crate::stream::RETAINED_BYTES);
            let projection = decoder.finish().unwrap();
            assert!(super::super::openai_chat::native_error_shape(&projection));
            assert!(chat_final(&projection).is_none());
            assert_eq!(projection.get("error.message"), Some(&Scalar::StringShape));
            assert_eq!(projection.get("error.type"), Some(&Scalar::StringShape));
        }
        for error in [
            "{}",
            "[]",
            "true",
            "0",
            r#""synthetic""#,
            r#"{"message":0,"type":"server_error"}"#,
            r#"{"message":"failure","type":null}"#,
            r#"{"message":"failure","type":"server_error","param":{}}"#,
            r#"{"message":"failure","type":"server_error","code":[]}"#,
            r#"{"message":"failure","type":"server_error","unknown":0}"#,
        ] {
            let raw = format!("{{\"error\":{error}}}");
            let projection = decode_chat(raw.as_bytes(), false).await.unwrap();
            assert!(!super::super::openai_chat::native_error_shape(&projection));
        }
    }
    #[tokio::test]
    async fn nonstream_requires_assistant_message_object() {
        let raw = br#"{"id":"c1","object":"chat.completion","model":"synthetic-text","choices":[{"index":0,"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_tokens_details":{"cached_tokens":40},"completion_tokens_details":{"reasoning_tokens":10}}}"#;
        let values = decode_chat(raw, false).await.unwrap();
        assert!(
            chat_final(&values).is_none(),
            "message 없는 응답을 정상 확정"
        );
    }
    #[tokio::test]
    async fn nonstream_message_shape_matrix_skips_content() {
        let base = serde_json::json!({"id":"c1","object":"chat.completion","model":"synthetic-text","choices":[{"index":0,"message":{"role":"assistant","content":"synthetic"},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_tokens_details":{"cached_tokens":40},"completion_tokens_details":{"reasoning_tokens":10}}});
        for message in [
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!({}),
            serde_json::json!({"role":"user","content":"x"}),
            serde_json::json!({"role":null,"content":"x"}),
            serde_json::json!({"role":"assistant"}),
            serde_json::json!({"role":"assistant","content":1}),
            serde_json::json!({"role":"assistant","content":{}}),
            serde_json::json!({"role":"assistant","content":[]}),
            serde_json::json!({"role":"assistant","content":null,"refusal":true}),
            serde_json::json!({"role":"assistant","content":null,"tool_calls":{}}),
        ] {
            let mut value = base.clone();
            value["choices"][0]["message"] = message;
            let projection = decode_chat(value.to_string().as_bytes(), false)
                .await
                .unwrap();
            assert!(
                chat_final(&projection).is_none(),
                "비정상 message shape 거부"
            );
        }
        for message in [
            serde_json::json!({"role":"assistant","content":"x".repeat(256*1024)}),
            serde_json::json!({"role":"assistant","content":null,"refusal":"synthetic refusal"}),
            serde_json::json!({"role":"assistant","content":null,"tool_calls":[{"id":"a","type":"function","function":{"name":"f","arguments":"{opaque"}},{"id":"b","type":"function","function":{"name":"g","arguments":"not JSON"}}]}),
            serde_json::json!({"role":"assistant","content":"{\"answer\":1}"}),
        ] {
            let mut value = base.clone();
            value["choices"][0]["message"] = message;
            let projection = decode_chat(value.to_string().as_bytes(), false)
                .await
                .unwrap();
            assert!(chat_final(&projection).is_some());
            assert!(matches!(
                projection.get("choices.*.message.content"),
                Some(Scalar::StringShape | Scalar::Null)
            ));
        }
    }
}
