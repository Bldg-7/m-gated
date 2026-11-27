//! OpenAI Responses API의 요청 검사, 응답 읽기, 스트림 읽기.
use m_gated_adapter_kit::{
    adapter::{
        ApiError, Feature, Profile, ReadError, Reading, RequestMetadata, Step, Usage, UsageCategory,
    },
    json::{self, Cooperate, JsonError, Kind, Visitor},
    openai, png,
    stream::{Event, JsonDecoder, Scalar},
};
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
    profile: &'a Profile,
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
    fn feature(&mut self, f: Feature) -> Result<(), JsonError> {
        if self.profile.has(f) {
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
                    self.feature(Feature::ClientTools)?;
                    Kind::Array
                }
                "tool_choice" => {
                    self.feature(Feature::ClientTools)?;
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
                    self.feature(Feature::ClientTools)?;
                    Kind::Bool
                }
                "reasoning" => {
                    self.feature(Feature::Thinking)?;
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
            self.feature(Feature::ClientTools)?;
            Kind::String
        } else if at(path, &["input", "*", "content", "*", "text"])
            || at(path, &["input", "*", "content", "*", "refusal"])
            || at(path, &["tools", "*", "description"])
            || at(path, &["text", "format", "description"])
        {
            Kind::String
        } else if at(path, &["input", "*", "content", "*", "image_url"]) {
            self.feature(Feature::InlinePng)?;
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
                    self.feature(Feature::ClientTools)?;
                    exact(
                        &["type", "name", "call_id", "arguments"],
                        &["type", "name", "call_id", "arguments"],
                    )
                }
                Some("function_call_output") => {
                    self.feature(Feature::ClientTools)?;
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
                        self.feature(Feature::JsonSchema)?;
                    }
                    exact(&["type"], &["type"])
                }
                Some("json_schema") => {
                    self.feature(Feature::JsonSchema)?;
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
/// 요청 본문을 검사한다. 긴 본문은 `cooperate`로 실행을 넘기며 읽는다.
pub async fn validate_with<C: Cooperate>(
    body: &[u8],
    profile: &Profile,
    mut cooperate: C,
) -> Result<RequestMetadata, ApiError> {
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
    if json::scan_with(body, &mut v, &mut cooperate).await.is_err() {
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
        total += png::validate_png_input_with(&body[range], &mut cooperate)
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
pub const FIELDS: &[&str] = &[
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
pub const STREAM_FIELDS: &[&str] = &[
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
pub(crate) fn final_usage(v: &BTreeMap<String, Scalar>, p: &str) -> Option<Usage> {
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
    // 입력 토큰에는 캐시에서 읽은 입력이 들어 있다. `input` 칸은 캐시가 아닌 입력이다(결정 0012).
    Some(Usage {
        actual_model: model,
        tokens: vec![
            (UsageCategory::Input, input - cached),
            (UsageCategory::Output, output),
            (UsageCategory::CacheRead, cached),
        ],
        reasoning_tokens: reasoning,
    })
}
#[derive(Default)]
pub struct Stream {
    started: bool,
    terminal: bool,
    sequence: Option<u64>,
    id: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
    failed: bool,
    protocol_failed: bool,
}
impl Stream {
    pub fn event(&mut self, event: Event) -> Result<(), ReadError> {
        if self.protocol_failed {
            return Err(ReadError::Protocol);
        }
        let result = self.apply_event(event);
        if result.is_err() {
            self.protocol_failed = true;
            self.usage = None;
        }
        result
    }
    fn apply_event(&mut self, event: Event) -> Result<(), ReadError> {
        let v = event.values;
        let kind = text(&v, "", "type").ok_or(ReadError::Protocol)?;
        if self.terminal || (!event.name.is_empty() && event.name != kind) {
            return Err(ReadError::Protocol);
        }
        let seq = number(&v, "", "sequence_number").ok_or(ReadError::Protocol)?;
        if self.sequence.is_some_and(|n| seq <= n) {
            return Err(ReadError::Protocol);
        }
        self.sequence = Some(seq);
        if kind == "error" {
            if event.wire_bytes > 64 * 1024 {
                return Err(ReadError::StreamLimit);
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
            return Err(ReadError::Protocol);
        }
        if kind == "response.created" {
            if self.started {
                return Err(ReadError::Protocol);
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
                return Err(ReadError::Protocol);
            }
        } else if !self.started {
            return Err(ReadError::Protocol);
        }
        if kind == "response.completed"
            || kind == "response.incomplete"
            || kind == "response.failed"
        {
            if text(&v, "response.", "id") != self.id.as_deref()
                || text(&v, "response.", "model") != self.model.as_deref()
                || text(&v, "response.", "status") != kind.strip_prefix("response.")
            {
                return Err(ReadError::Protocol);
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
                return Err(ReadError::Protocol);
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
                return Err(ReadError::Protocol);
            }
        }
        Ok(())
    }
    pub fn finish(&self) -> Result<Option<Usage>, ReadError> {
        if self.terminal && !self.protocol_failed {
            Ok(self.usage.clone())
        } else {
            Err(ReadError::Protocol)
        }
    }
}
impl Stream {
    /// event 하나를 받는다. 형식이 맞지 않으면 오류를 돌려주고, 그 뒤로는 사용량을 알려 주지 않는다.
    pub fn accept(&mut self, event: Event) -> Result<Step, ReadError> {
        let kind = event.values.get("type").and_then(Scalar::text);
        let terminal = matches!(
            kind,
            Some("response.completed" | "response.incomplete" | "response.failed")
        );
        let error = kind == Some("error");
        self.event(event)?;
        Ok(Step {
            terminal,
            error,
            content: Vec::new(),
        })
    }
    /// 끝 event를 받은 뒤 정산에 쓸 사용량. 오류 event 뒤에는 None이다.
    pub fn final_usage(&self) -> Option<Usage> {
        self.finish().ok().flatten()
    }
    /// Provider가 실패를 알렸는지(`error` event, `response.failed`).
    pub fn provider_error(&self) -> bool {
        self.failed
    }
    /// 스트림이 끝났을 때의 사용량과 실패 여부. 끝 event 없이 끝났으면 오류다.
    pub fn close(&self) -> Result<(Option<Usage>, bool), ReadError> {
        self.finish().map(|usage| (usage, self.failed))
    }
}
pub(crate) async fn decode<C: Cooperate>(
    bytes: &[u8],
    cooperate: &mut C,
) -> Result<BTreeMap<String, Scalar>, ReadError> {
    JsonDecoder::new(FIELDS)
        .read_with(bytes, cooperate)
        .await
        .map_err(|_| ReadError::Protocol)
}
/// 비스트림 응답 본문을 읽는다. Embeddings의 오류 응답도 이렇게 읽는다.
pub async fn read_with<C: Cooperate>(
    status: u16,
    body: &[u8],
    cooperate: &mut C,
) -> Result<Reading, ReadError> {
    let values = decode(body, cooperate).await?;
    let usage = if status == 200 {
        final_usage(&values, "")
    } else {
        openai::error_body_with(body, cooperate).await?;
        None
    };
    Ok(Reading {
        usage,
        failed: text(&values, "", "status") == Some("failed"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use m_gated_adapter_kit::{
        json::{NoYield, ready as now},
        stream::{RETAINED_BYTES, SseDecoder},
    };
    fn profile() -> Profile {
        Profile {
            model: "synthetic-text".into(),
            features: vec![
                Feature::ClientTools,
                Feature::Thinking,
                Feature::Cache,
                Feature::JsonSchema,
                Feature::InlinePng,
            ],
            approved_beta: Vec::new(),
            max_output_tokens: Some(1024),
            rates: ["input", "output", "cache_read"].map(String::from).to_vec(),
            chat_output_limit_field: None,
            embedding_dimensions: None,
            embedding_encodings: Vec::new(),
        }
    }
    #[test]
    fn responses_native_function_and_schema_optional_strict_is_not_injected() {
        let raw=br#" {"model":"synthetic-text","store":false,"max_output_tokens":100,"input":[{"type":"function_call","name":"f","call_id":"call_1","arguments":"{not JSON: opaque}"},{"type":"function_call_output","call_id":"call_1","output":"synthetic"}],"tools":[{"type":"function","name":"f","parameters":{"type":"object","properties":{"store":{"type":"boolean"}}}}],"text":{"format":{"type":"json_schema","name":"answer","schema":{"type":"object"}}},"reasoning":{"effort":"low","summary":"auto"}} "#;
        assert!(
            now(validate_with(raw, &profile(), NoYield)).is_ok(),
            "정상 native optional strict를 필수로 강제했다"
        );
    }
    #[test]
    fn responses_request_fields_are_exact_and_state_cannot_hide_in_order_or_escapes() {
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
            assert!(now(validate_with(raw.as_bytes(), &p, NoYield)).is_err());
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
                now(validate_with(raw.as_bytes(), &p, NoYield)).is_err(),
                "미지원 요청이 통과했다"
            );
        }
        let raw=br#"{"\u006dodel":"synthetic-text","input":[{"role":"user","content":[{"type":"input_text","text":"x"}]}],"\u0073tore":false,"max_output_tokens":1}"#;
        assert!(now(validate_with(raw, &p, NoYield)).is_ok());
    }
    fn native(status: &str, reason: Option<&str>) -> String {
        serde_json::json!({"id":"resp_fixture","object":"response","model":"synthetic-text","status":status,"output":[],"incomplete_details":reason.map(|r|serde_json::json!({"reason":r})),"usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cached_tokens":40},"output_tokens_details":{"reasoning_tokens":10}}}).to_string()
    }
    #[test]
    fn responses_terminal_usage_distinguishes_incomplete_finality_and_included_subsets() {
        for (status, reason, verified) in [
            ("completed", None, true),
            ("incomplete", Some("max_output_tokens"), true),
            ("incomplete", Some("content_filter"), false),
            ("failed", None, false),
        ] {
            let values = now(decode(native(status, reason).as_bytes(), &mut NoYield)).unwrap();
            let u = final_usage(&values, "");
            assert_eq!(u.is_some(), verified);
            if let Some(u) = u {
                // 캐시에서 읽은 40은 `input` 칸에서 빠져 `cache_read` 칸에 들어간다.
                assert_eq!(
                    (
                        u.get(UsageCategory::Input),
                        u.get(UsageCategory::Output),
                        u.get(UsageCategory::CacheRead),
                        u.reasoning_tokens
                    ),
                    (60, 20, 40, Some(10))
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
            assert!(
                final_usage(&now(decode(wire.as_bytes(), &mut NoYield)).unwrap(), "").is_none()
            );
        }
    }
    #[test]
    fn responses_unknown_tier_or_conflicting_status_metadata_is_not_final_usage() {
        for extra in [
            r#", "service_tier":"priority""#,
            r#", "service_tier":"flex""#,
            r#", "error":{"code":"server_error"}"#,
        ] {
            let raw = native("completed", None);
            let raw = format!("{}{extra}}}", &raw[..raw.len() - 1]);
            assert!(
                final_usage(&now(decode(raw.as_bytes(), &mut NoYield)).unwrap(), "").is_none(),
                "검증되지않은요율/상충종료metadata를정산했다"
            );
        }
        let raw = native("completed", Some("max_output_tokens"));
        assert!(final_usage(&now(decode(raw.as_bytes(), &mut NoYield)).unwrap(), "").is_none());
    }
    #[test]
    fn responses_missing_detail_counts_and_prior_error_never_become_final() {
        let raw = native("completed", None);
        for (from, to) in [
            ("\"cached_tokens\":40", "\"cached_other\":40"),
            ("\"input_tokens_details\":{\"cached_tokens\":40},", ""),
            ("\"output_tokens_details\":{\"reasoning_tokens\":10},", ""),
        ] {
            let wire = raw.replace(from, to);
            assert_ne!(wire, raw);
            assert!(
                final_usage(&now(decode(wire.as_bytes(), &mut NoYield)).unwrap(), "").is_none(),
                "부재한 상세 청구량을0으로추정했다"
            );
        }
        for middle in [r#"{"type":"error","sequence_number":1,"code":"server_error","message":"synthetic"}"#.to_string(),r#"{"type":"response.in_progress","sequence_number":1,"response":{"id":"resp_other","object":"response","status":"in_progress","model":"synthetic-other","usage":null}}"#.to_string()] {
            let created=r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_fixture","object":"response","model":"synthetic-text","status":"in_progress","usage":null}}"#;
            let terminal=format!(r#"{{"type":"response.completed","sequence_number":2,"response":{raw}}}"#);
            let mut parser=SseDecoder::new(STREAM_FIELDS);let mut reducer=Stream::default();let wire=format!("data: {created}\n\ndata: {middle}\n\ndata: {terminal}\n\n");
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
                let mut reducer = Stream::default();
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
            let mut reducer = Stream::default();
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
    #[test]
    fn responses_output_message_content_is_not_supported_input_for_any_role_or_order() {
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
                    accepted +=
                        usize::from(now(validate_with(raw.as_bytes(), &p, NoYield)).is_ok());
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
            assert!(now(validate_with(raw.as_bytes(), &p, NoYield)).is_ok());
        }
    }
    #[test]
    fn responses_large_terminal_validates_all_skipped_syntax_under_retained_limit() {
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
                }
                if !valid {
                    break;
                }
            }
            assert_eq!(valid, size <= 16 * 1024 * 1024);
            assert!(parser.retained_peak() <= RETAINED_BYTES);
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
            assert!(now(decode(raw.as_bytes(), &mut NoYield)).is_err());
        }
    }
}
