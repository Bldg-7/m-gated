use crate::{
    config::{ApiKind, ModelProfile, NativeFeature},
    json::{self, JsonError, Kind, Visitor},
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiError {
    InvalidRequest,
    UnsupportedOption,
    UnsupportedModel,
}
pub struct RequestMetadata {
    pub model: String,
    pub output_limit: Option<u64>,
    pub stream: bool,
}
struct Validation<'a> {
    profile: &'a ModelProfile,
    api: ApiKind,
    model: Option<String>,
    output: Option<u64>,
    stream: bool,
    thinking: Option<u64>,
    types: BTreeMap<Vec<String>, String>,
    images: usize,
    image_bytes: usize,
    body_start: usize,
    image_ranges: Vec<std::ops::Range<usize>>,
    /// `data` 문자열은 소속 block type을 object_end에서 확정한 뒤 image/redacted_thinking으로 분류한다.
    pending_data: BTreeMap<Vec<String>, std::ops::Range<usize>>,
    failure: Option<ApiError>,
}
/// 요청에 있으면 과금 방식·서버측 추가 실행이 바뀌는 top-level 옵션이다. 검증 전에는 통과시키지 않는다.
const COST_AFFECTING_TOP_LEVEL: &[&str] = &[
    "service_tier",
    "speed",
    "inference_geo",
    "mcp_servers",
    "container",
];
/// 검증기가 구조를 직접 검사하는 top-level 필드. 나머지는 비용과 무관한 옵션으로 보고 원본 그대로 전달한다.
const KNOWN_TOP_LEVEL: &[&str] = &[
    "model",
    "max_tokens",
    "stream",
    "messages",
    "system",
    "tools",
    "tool_choice",
    "thinking",
    "output_config",
    "stop_sequences",
    "temperature",
    "top_p",
    "top_k",
    "context_management",
];
/// client tool 정의에서 검증기가 구조를 검사하는 필드. 그 외(defer_loading 등)는 원본 그대로 전달한다.
const KNOWN_TOOL_FIELDS: &[&str] = &[
    "name",
    "description",
    "input_schema",
    "cache_control",
    "type",
];
pub async fn validate(
    body: &[u8],
    api: ApiKind,
    profile: &ModelProfile,
) -> Result<RequestMetadata, ApiError> {
    let mut v = Validation {
        profile,
        api,
        model: None,
        output: None,
        stream: false,
        thinking: None,
        types: BTreeMap::new(),
        images: 0,
        image_bytes: 0,
        body_start: body.as_ptr() as usize,
        image_ranges: Vec::new(),
        pending_data: BTreeMap::new(),
        failure: None,
    };
    if json::scan(body, &mut v).await.is_err() {
        return Err(v.failure.unwrap_or(ApiError::InvalidRequest));
    }
    for range in &v.image_ranges {
        let raw = std::str::from_utf8(body.get(range.clone()).ok_or(ApiError::InvalidRequest)?)
            .map_err(|_| ApiError::InvalidRequest)?;
        v.image_bytes += validate_png_data(raw)
            .await
            .map_err(|_| ApiError::InvalidRequest)?;
        if v.image_bytes > 4 * 1024 * 1024 {
            return Err(ApiError::InvalidRequest);
        }
    }
    let model = v.model.ok_or(ApiError::InvalidRequest)?;
    if model != profile.model {
        return Err(ApiError::UnsupportedModel);
    }
    if api == ApiKind::Messages
        && !v
            .output
            .is_some_and(|n| n > 0 && profile.max_output_tokens.is_some_and(|max| n <= max))
    {
        return Err(ApiError::InvalidRequest);
    }
    if v.thinking
        .is_some_and(|n| n < 1024 || v.output.is_none_or(|o| n >= o))
    {
        return Err(ApiError::InvalidRequest);
    }
    Ok(RequestMetadata {
        model,
        output_limit: v.output,
        stream: v.stream,
    })
}
fn path_is(path: &[String], parts: &[&str]) -> bool {
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
impl Validation<'_> {
    fn feature(&mut self, f: NativeFeature) -> Result<(), JsonError> {
        if self.profile.features.contains(&f) {
            Ok(())
        } else {
            self.failure = Some(ApiError::UnsupportedOption);
            Err(JsonError)
        }
    }
    fn opaque(path: &[String]) -> bool {
        path.iter()
            .any(|p| matches!(p.as_str(), "input_schema" | "input" | "schema"))
    }
    /// 비용 상한에 영향 없는 것으로 분류한 하위 트리. 내용은 검사하지 않고 원본 bytes로 전달한다.
    fn passthrough(path: &[String]) -> bool {
        let parts: Vec<&str> = path.iter().map(String::as_str).collect();
        match parts.as_slice() {
            // 과금 옵션은 top-level 값 방문에서 먼저 거부되므로 하위 트리를 통과시키지 않는다.
            [root, ..]
                if !KNOWN_TOP_LEVEL.contains(root) && !COST_AFFECTING_TOP_LEVEL.contains(root) =>
            {
                true
            }
            ["tools", _, field, ..] => !KNOWN_TOOL_FIELDS.contains(field),
            // 메시지 단위 옵션(per-turn output_config 등)도 같은 원칙이다. 과금 옵션은 값 방문에서 거부된다.
            ["messages", _, field, ..] => {
                !["role", "content"].contains(field) && !COST_AFFECTING_TOP_LEVEL.contains(field)
            }
            ["output_config", field, ..] => *field != "format",
            ["context_management", field, ..] if *field != "edits" => true,
            ["context_management", "edits", _, field, ..] => *field != "type",
            _ => false,
        }
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
        if Self::passthrough(path) {
            return Ok(());
        }
        if path_is(path, &["context_management", "edits"]) {
            return if kind == Kind::Array {
                Ok(())
            } else {
                Err(JsonError)
            };
        }
        if path.len() == 3 && path_is(&path[..2], &["context_management", "edits"]) {
            return if kind == Kind::Object {
                Ok(())
            } else {
                Err(JsonError)
            };
        }
        if path.len() == 4
            && path_is(&path[..2], &["context_management", "edits"])
            && path[3] == "type"
        {
            // clear_* 편집은 입력을 줄이기만 한다. 서버측 요약(compaction)은 추가 생성 비용이라 거부한다.
            return if json::string(raw, 64)?.starts_with("clear_") {
                Ok(())
            } else {
                self.failure = Some(ApiError::UnsupportedOption);
                Err(JsonError)
            };
        }
        if Self::opaque(path) {
            let root = path
                .iter()
                .position(|s| matches!(s.as_str(), "input_schema" | "input" | "schema"))
                == Some(path.len() - 1);
            if root && kind != Kind::Object {
                return Err(JsonError);
            }
            if path.len() < 3 {
                return Err(JsonError);
            }
            return Ok(());
        }
        let key = path.last().ok_or(JsonError)?.as_str();
        if path.len() == 1 {
            let expected = match key {
                "model" => {
                    self.model = Some(json::string(raw, 200)?);
                    Kind::String
                }
                "max_tokens" if self.api == ApiKind::Messages => {
                    self.output = Some(integer(raw)?);
                    Kind::Number
                }
                "stream" if self.api == ApiKind::Messages => {
                    self.stream = raw == "true";
                    Kind::Bool
                }
                "messages" => Kind::Array,
                "system" => {
                    if kind == Kind::String {
                        return Ok(());
                    }
                    Kind::Array
                }
                "tools" => {
                    self.feature(NativeFeature::ClientTools)?;
                    Kind::Array
                }
                "tool_choice" => {
                    self.feature(NativeFeature::ClientTools)?;
                    Kind::Object
                }
                "thinking" if self.api == ApiKind::Messages => {
                    self.feature(NativeFeature::Thinking)?;
                    Kind::Object
                }
                "output_config" if self.api == ApiKind::Messages => Kind::Object,
                "context_management" if self.api == ApiKind::Messages => Kind::Object,
                "stop_sequences" if self.api == ApiKind::Messages => Kind::Array,
                "temperature" | "top_p" if self.api == ApiKind::Messages => {
                    if kind != Kind::Number || raw.len() > 64 {
                        return Err(JsonError);
                    }
                    let n = raw.parse::<f64>().map_err(|_| JsonError)?;
                    if !(0.0..=1.0).contains(&n) {
                        return Err(JsonError);
                    }
                    Kind::Number
                }
                "top_k" if self.api == ApiKind::Messages => {
                    integer(raw)?;
                    Kind::Number
                }
                _ if KNOWN_TOP_LEVEL.contains(&key) || COST_AFFECTING_TOP_LEVEL.contains(&key) => {
                    // count의 생성 전용 옵션과 과금 방식이 바뀌는 옵션은 계속 거부한다.
                    self.failure = Some(ApiError::UnsupportedOption);
                    return Err(JsonError);
                }
                _ => return Ok(()),
            };
            return if kind == expected {
                Ok(())
            } else {
                Err(JsonError)
            };
        }
        if path_is(path, &["messages", "*"]) {
            return if kind == Kind::Object {
                Ok(())
            } else {
                Err(JsonError)
            };
        }
        if path_is(path, &["messages", "*", "role"]) {
            let role = json::string(raw, 16)?;
            return if ["user", "assistant", "system"].contains(&role.as_str()) {
                Ok(())
            } else {
                Err(JsonError)
            };
        }
        if key == "content" {
            return if matches!(kind, Kind::String | Kind::Array) {
                Ok(())
            } else {
                Err(JsonError)
            };
        }
        if key == "*" {
            let parent = path.get(path.len() - 2).map(String::as_str);
            return match parent {
                Some("stop_sequences") => {
                    if kind == Kind::String {
                        Ok(())
                    } else {
                        Err(JsonError)
                    }
                }
                Some("messages" | "content" | "system" | "tools") => {
                    if kind == Kind::Object {
                        Ok(())
                    } else {
                        Err(JsonError)
                    }
                }
                _ => Err(JsonError),
            };
        }
        match key {
            "type" => {
                let t = json::string(raw, 64)?;
                let parent = &path[..path.len() - 1];
                if (path_is(parent, &["system", "*"]) && t != "text")
                    || (path_is(parent, &["messages", "*", "content", "*", "content", "*"])
                        && !["text", "image"].contains(&t.as_str()))
                {
                    return Err(JsonError);
                }
                if path_is(parent, &["thinking"]) {
                    if !["enabled", "adaptive", "disabled"].contains(&t.as_str()) {
                        return Err(JsonError);
                    }
                    self.feature(NativeFeature::Thinking)?;
                } else if path.len() == 3 && path_is(&path[..1], &["tools"]) {
                    // client tool만 허용한다. server tool type은 별도 과금이다.
                    if t != "custom" {
                        self.failure = Some(ApiError::UnsupportedOption);
                        return Err(JsonError);
                    }
                } else if path_is(parent, &["tool_choice"]) {
                    if !["auto", "any", "tool", "none"].contains(&t.as_str()) {
                        return Err(JsonError);
                    }
                } else if parent.last().is_some_and(|s| s == "cache_control") {
                    self.feature(NativeFeature::Cache)?;
                    if t != "ephemeral" {
                        return Err(JsonError);
                    }
                } else if parent.last().is_some_and(|s| s == "source") {
                    self.feature(NativeFeature::InlinePng)?;
                    if t != "base64" {
                        self.failure = Some(ApiError::UnsupportedOption);
                        return Err(JsonError);
                    }
                } else if path_is(parent, &["output_config", "format"]) {
                    if t != "json_schema" {
                        return Err(JsonError);
                    }
                } else if path_is(parent, &["messages", "*", "content", "*", "caller"]) {
                    // 실제 API는 tool_use에 caller를 붙여 돌려준다. 모델이 직접 부른 경우만 허용하고
                    // code execution 같은 서버 도구 경유 호출은 별도 과금이라 거부한다.
                    if t != "direct" {
                        self.failure = Some(ApiError::UnsupportedOption);
                        return Err(JsonError);
                    }
                } else {
                    match t.as_str() {
                        "text" => {}
                        "tool_use" | "tool_result" => self.feature(NativeFeature::ClientTools)?,
                        "image" => self.feature(NativeFeature::InlinePng)?,
                        "thinking" | "redacted_thinking" => {
                            self.feature(NativeFeature::Thinking)?
                        }
                        _ => {
                            self.failure = Some(ApiError::UnsupportedOption);
                            return Err(JsonError);
                        }
                    }
                }
                self.types.insert(parent.to_vec(), t);
                Ok(())
            }
            "text" | "description" | "signature" => {
                if kind == Kind::String {
                    Ok(())
                } else {
                    Err(JsonError)
                }
            }
            "name" | "id" | "tool_use_id" => {
                let n = json::string(raw, 200)?;
                if n.is_empty() || n.chars().any(char::is_control) {
                    return Err(JsonError);
                }
                Ok(())
            }
            "input_schema" => {
                self.feature(NativeFeature::ClientTools)?;
                if kind == Kind::Object {
                    Ok(())
                } else {
                    Err(JsonError)
                }
            }
            "cache_control" => {
                self.feature(NativeFeature::Cache)?;
                if kind == Kind::Object {
                    Ok(())
                } else {
                    Err(JsonError)
                }
            }
            "ttl" => {
                if json::string(raw, 8)? == "5m" {
                    Ok(())
                } else {
                    Err(JsonError)
                }
            }
            "source" => {
                self.feature(NativeFeature::InlinePng)?;
                if kind == Kind::Object {
                    Ok(())
                } else {
                    Err(JsonError)
                }
            }
            "media_type" => {
                if json::string(raw, 32)? == "image/png" {
                    Ok(())
                } else {
                    Err(JsonError)
                }
            }
            "data" => {
                if kind != Kind::String {
                    return Err(JsonError);
                }
                let offset = (raw.as_ptr() as usize)
                    .checked_sub(self.body_start)
                    .ok_or(JsonError)?;
                self.pending_data
                    .insert(path[..path.len() - 1].to_vec(), offset..offset + raw.len());
                Ok(())
            }
            "thinking" if path.len() == 5 => {
                if kind == Kind::String {
                    Ok(())
                } else {
                    Err(JsonError)
                }
            }
            "display" if path_is(path, &["thinking", "display"]) => {
                json::string(raw, 32)?;
                Ok(())
            }
            "budget_tokens" if path_is(path, &["thinking", "budget_tokens"]) => {
                self.thinking = Some(integer(raw)?);
                Ok(())
            }
            "caller" if path_is(path, &["messages", "*", "content", "*", "caller"]) => {
                if kind == Kind::Object {
                    Ok(())
                } else {
                    Err(JsonError)
                }
            }
            "is_error" | "disable_parallel_tool_use" => {
                if kind == Kind::Bool {
                    Ok(())
                } else {
                    Err(JsonError)
                }
            }
            "format" if path_is(path, &["output_config", "format"]) => {
                self.feature(NativeFeature::JsonSchema)?;
                if kind == Kind::Object {
                    Ok(())
                } else {
                    Err(JsonError)
                }
            }
            _ => {
                self.failure = Some(ApiError::UnsupportedOption);
                Err(JsonError)
            }
        }
    }
    fn object_end(
        &mut self,
        path: &[String],
        keys: &BTreeSet<String>,
        raw: &str,
    ) -> Result<(), JsonError> {
        if Self::opaque(path) {
            if path
                .iter()
                .position(|s| matches!(s.as_str(), "input_schema" | "input" | "schema"))
                == Some(path.len() - 1)
                && path
                    .last()
                    .is_some_and(|s| s == "input_schema" || s == "schema")
                && raw.len() > 65536
            {
                return Err(JsonError);
            }
            return Ok(());
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
        if Self::passthrough(path) {
            return Ok(());
        }
        if path.is_empty() {
            if !keys.contains("model") || !keys.contains("messages") {
                return Err(JsonError);
            }
            return Ok(());
        }
        let data = self.pending_data.remove(path);
        let kind = self.types.get(path).cloned();
        if data.is_some() && !matches!(kind.as_deref(), Some("base64" | "redacted_thinking")) {
            return Err(JsonError);
        }
        if path_is(path, &["messages", "*"]) {
            // 그 외 필드는 passthrough 또는 과금 옵션 거부로 이미 처리됐다.
            return if keys.contains("role") && keys.contains("content") {
                Ok(())
            } else {
                Err(JsonError)
            };
        }
        if path_is(path, &["tools", "*"]) {
            self.types.remove(path);
            // 미지의 tool 필드는 passthrough로 이미 통과했으므로 여기서는 필수 필드만 확인한다.
            return if keys.contains("name") && keys.contains("input_schema") {
                Ok(())
            } else {
                Err(JsonError)
            };
        }
        if path_is(path, &["context_management"])
            || (path.len() == 3 && path_is(&path[..2], &["context_management", "edits"]))
        {
            return Ok(());
        }
        if let Some(kind) = kind {
            self.types.remove(path);
            if kind == "base64" {
                self.images += 1;
                if self.images > 20 {
                    return Err(JsonError);
                }
                self.image_ranges.push(data.ok_or(JsonError)?);
            }
            match kind.as_str() {
                "text" => exact(&["type", "text"], &["type", "text", "cache_control"]),
                "image" => exact(&["type", "source"], &["type", "source", "cache_control"]),
                "base64" => exact(
                    &["type", "media_type", "data"],
                    &["type", "media_type", "data"],
                ),
                "tool_use" => exact(
                    &["type", "id", "name", "input"],
                    &["type", "id", "name", "input", "caller", "cache_control"],
                ),
                "direct" => exact(&["type"], &["type"]),
                "tool_result" => exact(
                    &["type", "tool_use_id", "content"],
                    &[
                        "type",
                        "tool_use_id",
                        "content",
                        "is_error",
                        "cache_control",
                    ],
                ),
                "ephemeral" => exact(&["type"], &["type", "ttl"]),
                "enabled" => exact(
                    &["type", "budget_tokens"],
                    &["type", "budget_tokens", "display"],
                ),
                "adaptive" => exact(&["type"], &["type", "display"]),
                "disabled" => exact(&["type"], &["type"]),
                "thinking" => exact(&["type", "thinking"], &["type", "thinking", "signature"]),
                "redacted_thinking" => exact(&["type", "data"], &["type", "data"]),
                "json_schema" => exact(&["type", "schema"], &["type", "schema"]),
                "tool" => exact(
                    &["type", "name"],
                    &["type", "name", "disable_parallel_tool_use"],
                ),
                "auto" | "any" | "none" => exact(&["type"], &["type", "disable_parallel_tool_use"]),
                _ => Err(JsonError),
            }
        } else if path_is(path, &["output_config"]) {
            Ok(())
        } else {
            Err(JsonError)
        }
    }
    fn array_end(&mut self, path: &[String], items: usize, _: &str) -> Result<(), JsonError> {
        if path_is(path, &["tools"]) && items > 64 || path_is(path, &["messages"]) && items == 0 {
            return Err(JsonError);
        }
        Ok(())
    }
}
async fn validate_png_data(raw: &str) -> Result<usize, JsonError> {
    if !raw.starts_with('"') || !raw.ends_with('"') {
        return Err(JsonError);
    }
    validate_png_input(&raw.as_bytes()[1..raw.len() - 1]).await
}
pub(super) async fn validate_png_input(input: &[u8]) -> Result<usize, JsonError> {
    use base64::Engine;
    let mut i = 0;
    let mut block = [0u8; 4];
    let mut used = 0;
    let mut header = Vec::with_capacity(33);
    let mut count = 0;
    let mut ended = false;
    let mut yielded = 0;
    while i < input.len() {
        if i - yielded >= 64 * 1024 {
            yielded = i;
            tokio::task::yield_now().await;
        }
        if ended {
            return Err(JsonError);
        }
        let mut b = input[i];
        i += 1;
        if b == b'\\' {
            b = *input.get(i).ok_or(JsonError)?;
            i += 1;
            if b == b'u' {
                let digits = input.get(i..i + 4).ok_or(JsonError)?;
                let n =
                    u16::from_str_radix(std::str::from_utf8(digits).map_err(|_| JsonError)?, 16)
                        .map_err(|_| JsonError)?;
                b = u8::try_from(n).map_err(|_| JsonError)?;
                i += 4;
            } else if b != b'/' {
                return Err(JsonError);
            }
        }
        block[used] = b;
        used += 1;
        if used == 4 {
            let mut decoded = [0u8; 3];
            let n = base64::engine::general_purpose::STANDARD
                .decode_slice(block, &mut decoded)
                .map_err(|_| JsonError)?;
            count += n;
            if count > 4 * 1024 * 1024 {
                return Err(JsonError);
            }
            for b in decoded.into_iter().take(n) {
                if header.len() < 33 {
                    header.push(b);
                }
            }
            ended = block.contains(&b'=');
            used = 0;
        }
    }
    if used != 0
        || header.len() < 33
        || &header[..8] != b"\x89PNG\r\n\x1a\n"
        || &header[12..16] != b"IHDR"
    {
        return Err(JsonError);
    }
    let w = u32::from_be_bytes(header[16..20].try_into().map_err(|_| JsonError)?);
    let h = u32::from_be_bytes(header[20..24].try_into().map_err(|_| JsonError)?);
    if w == 0 || h == 0 || w > 8192 || h > 8192 {
        return Err(JsonError);
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn profile() -> ModelProfile {
        serde_json::from_value(serde_json::json!({"model":"synthetic-text","api":"messages","profile_version":"fixture-v1","pricing_version":"price-v1","fixture_version":"text-v1","actual_models":["synthetic-text"],"features":["client_tools","thinking","cache","json_schema","inline_png"],"max_input_tokens":4096,"max_output_tokens":4096,"max_body_bytes":8388608,"max_response_bytes":16777216,"rates":{"input":"1.000000000000","output":"2.000000000000"}})).unwrap()
    }
    #[tokio::test]
    async fn numeric_scalar_conversion_has_a_small_lexical_bound() {
        assert_eq!(integer("9223372036854775807").unwrap(), i64::MAX as u64);
        assert!(integer("9223372036854775808").is_err());
        assert!(integer(&"1".repeat(1024 * 1024)).is_err());
        for field in ["temperature", "top_p"] {
            for size in [64, 65] {
                let raw = format!(
                    r#"{{"model":"synthetic-text","max_tokens":1,"messages":[{{"role":"user","content":"x"}}],"{field}":0.{}}}"#,
                    "0".repeat(size - 2)
                );
                assert_eq!(
                    validate(raw.as_bytes(), ApiKind::Messages, &profile())
                        .await
                        .is_ok(),
                    size == 64
                );
            }
        }
    }
    #[tokio::test]
    async fn malformed_native_schema_never_becomes_approved() {
        let mut body = serde_json::json!({"model":"synthetic-text","max_tokens":100,"messages":[{"role":"user","content":"hello"}],"tools":[{"name":"f","input_schema":[]} ]});
        assert!(
            validate(
                &serde_json::to_vec(&body).unwrap(),
                ApiKind::Messages,
                &profile()
            )
            .await
            .is_err(),
            "array input_schema approved"
        );
        body["tools"][0]["input_schema"] = serde_json::json!({"type":"object"});
        validate(
            &serde_json::to_vec(&body).unwrap(),
            ApiKind::Messages,
            &profile(),
        )
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn ambiguous_or_unverified_messages_never_dispatch() {
        for body in [
            r#"{"model":"synthetic-text","\u006dodel":"other","max_tokens":1,"messages":[]}"#,
            r#"{"model":"synthetic-text","max_tokens":1,"messages":[{"role":"user","content":[{"type":"image","source":{"type":"url","url":"https://example.test"}}]}]}"#,
            r#"{"model":"synthetic-text","max_tokens":1,"messages":[],"stream":true}"#,
            r#"{"model":"synthetic-text","max_tokens":1,"messages":[],"metadata":{"user_id":"arbitrary"}}"#,
        ] {
            assert!(
                validate(body.as_bytes(), ApiKind::Messages, &profile())
                    .await
                    .is_err()
            );
        }
        validate(br#" {"messages":[{"content":"hello","role":"user"}],"max_tokens":1,"model":"synthetic-text"} "#,ApiKind::Messages,&profile()).await.unwrap();
    }

    /// Claude Code 2.1.322가 실제로 보낸 요청 shape(값은 축약). 비용 상한에 영향 없는 항목은 통과한다.
    fn claude_code_body() -> serde_json::Value {
        serde_json::json!({
            "model":"synthetic-text",
            "max_tokens":1000,
            "stream":true,
            "metadata":{"user_id":"{\"device_id\":\"d\",\"account_uuid\":\"\",\"session_id\":\"s\"}"},
            "thinking":{"type":"adaptive","display":"omitted"},
            "context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]},
            "output_config":{"effort":"high"},
            "system":[{"type":"text","text":"a"},{"type":"text","text":"b","cache_control":{"type":"ephemeral"}}],
            "tools":[
                {"name":"Read","description":"r","input_schema":{"type":"object"}},
                {"type":"custom","name":"Placeholder","description":"p","input_schema":{"type":"object"},"defer_loading":true,"strict":true}
            ],
            "messages":[
                {"role":"user","content":[{"type":"text","text":"hi"}]},
                {"role":"system","content":[{"type":"text","text":"reminder","cache_control":{"type":"ephemeral"}}],"output_config":{"effort":"medium"}},
                {"role":"assistant","content":[
                    {"type":"thinking","thinking":"","signature":"sig"},
                    {"type":"redacted_thinking","data":"opaque-not-png"},
                    {"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/x"},"caller":{"type":"direct"}}
                ]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok","cache_control":{"type":"ephemeral"}}]}
            ]
        })
    }
    async fn check(body: &serde_json::Value, api: ApiKind) -> Result<RequestMetadata, ApiError> {
        validate(&serde_json::to_vec(body).unwrap(), api, &profile()).await
    }
    #[tokio::test]
    async fn claude_code_request_shape_is_accepted() {
        let meta = check(&claude_code_body(), ApiKind::Messages).await.unwrap();
        assert_eq!(meta.output_limit, Some(1000));
        assert!(meta.stream);
        // 비용과 무관한 미래 top-level 옵션은 내용 검사 없이 통과한다.
        let mut body = claude_code_body();
        body["future_option"] = serde_json::json!({"nested":[1,{"x":null}],"flag":true});
        check(&body, ApiKind::Messages).await.unwrap();
        // 기존 enabled thinking도 display와 함께 허용한다.
        body["max_tokens"] = serde_json::json!(2000);
        body["thinking"] =
            serde_json::json!({"type":"enabled","budget_tokens":1024,"display":"summarized"});
        check(&body, ApiKind::Messages).await.unwrap();
        body["thinking"] = serde_json::json!({"type":"disabled"});
        check(&body, ApiKind::Messages).await.unwrap();
    }
    #[tokio::test]
    async fn cost_affecting_options_stay_rejected() {
        for (key, value) in [
            ("service_tier", serde_json::json!("priority")),
            ("speed", serde_json::json!("fast")),
            ("inference_geo", serde_json::json!("us")),
            (
                "mcp_servers",
                serde_json::json!([{"type":"url","url":"https://example.test","name":"m"}]),
            ),
            ("container", serde_json::json!("c")),
        ] {
            let mut body = claude_code_body();
            body[key] = value;
            assert!(
                matches!(
                    check(&body, ApiKind::Messages).await,
                    Err(ApiError::UnsupportedOption)
                ),
                "{key} must stay rejected"
            );
        }
        let mut body = claude_code_body();
        body["tools"] = serde_json::json!([{"type":"web_search_20250305","name":"web_search"}]);
        assert!(
            check(&body, ApiKind::Messages).await.is_err(),
            "server tool"
        );
        let mut body = claude_code_body();
        body["context_management"] = serde_json::json!({"edits":[{"type":"compact_20260112"}]});
        assert!(
            check(&body, ApiKind::Messages).await.is_err(),
            "server-side compaction"
        );
        let mut body = claude_code_body();
        body["thinking"] = serde_json::json!({"type":"adaptive","budget_tokens":5000});
        assert!(
            check(&body, ApiKind::Messages).await.is_err(),
            "adaptive with budget"
        );
        let mut body = claude_code_body();
        body["thinking"] = serde_json::json!({"type":"enabled","budget_tokens":5000});
        assert!(
            check(&body, ApiKind::Messages).await.is_err(),
            "budget over max_tokens"
        );
        let mut body = claude_code_body();
        body["max_tokens"] = serde_json::json!(4097);
        assert!(
            check(&body, ApiKind::Messages).await.is_err(),
            "output over profile"
        );
        let mut body = claude_code_body();
        body["messages"][0]["content"][0] = serde_json::json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"bm90LXBuZw=="}});
        assert!(
            check(&body, ApiKind::Messages).await.is_err(),
            "image data stays verified"
        );
        let mut body = claude_code_body();
        body["messages"][0]["content"][0] =
            serde_json::json!({"type":"text","text":"x","data":"y"});
        assert!(
            check(&body, ApiKind::Messages).await.is_err(),
            "data outside image/redacted"
        );
        // 실제 API가 돌려주는 tool_use.caller는 direct만 허용한다. 서버 도구 경유 호출은 별도 과금이다.
        for caller in [
            serde_json::json!({"type":"code_execution_20250825","tool_id":"srvtoolu_1"}),
            serde_json::json!({"type":"direct","tool_id":"x"}),
            serde_json::json!("direct"),
        ] {
            let mut body = claude_code_body();
            body["messages"][2]["content"][2]["caller"] = caller.clone();
            assert!(
                check(&body, ApiKind::Messages).await.is_err(),
                "tool_use caller {caller}"
            );
        }
        let mut body = claude_code_body();
        body["messages"][0]["content"][0]["caller"] = serde_json::json!({"type":"direct"});
        assert!(
            check(&body, ApiKind::Messages).await.is_err(),
            "caller outside tool_use"
        );
        for key in COST_AFFECTING_TOP_LEVEL {
            let mut body = claude_code_body();
            body["messages"][1][*key] = serde_json::json!("x");
            assert!(
                check(&body, ApiKind::Messages).await.is_err(),
                "message-level {key}"
            );
        }
        let mut body = claude_code_body();
        body["messages"][0]["role"] = serde_json::json!("developer");
        assert!(
            check(&body, ApiKind::Messages).await.is_err(),
            "unknown role"
        );
        // count는 생성 전용 옵션을 계속 거부한다.
        let count = serde_json::json!({"model":"synthetic-text","messages":[{"role":"user","content":"x"}],"max_tokens":1});
        assert!(check(&count, ApiKind::CountTokens).await.is_err());
    }
}

use crate::ledger::finance::{FinalObservation, FinalOutcome, FinalUsage, SendPermit};
use axum::{
    body::Bytes,
    http::{HeaderMap, HeaderValue, StatusCode, header},
};

pub use super::transport::{
    NativeClient as AnthropicClient, SendOptions, UpstreamError, UpstreamResponse,
};
pub(super) use super::transport::{classify_transport, response_headers};
impl AnthropicClient {
    pub async fn send(
        &self,
        permit: SendPermit,
        body: impl Into<reqwest::Body>,
        options: SendOptions<'_>,
    ) -> Result<UpstreamResponse, UpstreamError> {
        let response = self.begin_send(permit, body, options).await?;
        self.read_response(response, options).await
    }
    pub(super) async fn read_response(
        &self,
        mut response: reqwest::Response,
        options: SendOptions<'_>,
    ) -> Result<UpstreamResponse, UpstreamError> {
        if matches!(
            options.api,
            ApiKind::Responses | ApiKind::OpenaiChat | ApiKind::DeepseekChat
        ) {
            return super::openai::read_response(response, options).await;
        }
        let api = options.api;
        let profile = options.profile;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(UpstreamError::Auth);
        }
        if status.is_redirection() {
            return Err(UpstreamError::Redirect);
        }
        if response
            .headers()
            .get_all(header::CONTENT_ENCODING)
            .iter()
            .any(|v| v.as_bytes() != b"identity")
        {
            return Err(UpstreamError::Encoding);
        }
        if !json_content_type(response.headers()) {
            return Err(UpstreamError::Protocol);
        }
        let limit = if status.is_success() {
            if api == ApiKind::CountTokens {
                profile.max_response_bytes.min(256 * 1024)
            } else {
                profile.max_response_bytes.min(16 * 1024 * 1024)
            }
        } else {
            64 * 1024
        };
        if response.content_length().is_some_and(|n| n > limit as u64) {
            return Err(UpstreamError::TooLarge);
        }
        let mut bytes = crate::runtime::BoundedBuffer::new(limit);
        while let Some(chunk) = response.chunk().await.map_err(classify_transport)? {
            bytes.extend(&chunk).map_err(|_| UpstreamError::TooLarge)?;
        }
        let bytes = bytes.freeze();
        let mut headers = response_headers(response.headers());
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert("x-should-retry", HeaderValue::from_static("false"));
        let provider_request_id = headers
            .get("request-id")
            .or_else(|| headers.get("x-request-id"))
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let (mut usage, counted) = if status == StatusCode::OK {
            parse_response(&bytes, api).await?
        } else {
            validate_native_error(&bytes).await?;
            (None, None)
        };
        if api == ApiKind::CountTokens
            && profile.count_billing.as_deref() == Some("counted_input_tokens")
        {
            usage = counted.map(|n| FinalUsage {
                actual_model: profile.model.clone(),
                input_tokens: n,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: None,
            });
        }
        let observation = FinalObservation {
            outcome: if status == StatusCode::OK {
                FinalOutcome::Completed
            } else {
                FinalOutcome::Failed
            },
            usage,
            counted_input_tokens: counted,
            reason: if status == StatusCode::OK {
                "completed"
            } else {
                "provider_error"
            },
            http_status: Some(status.as_u16()),
            provider_request_id,
        };
        Ok(UpstreamResponse {
            status,
            headers,
            body: bytes,
            observation,
        })
    }
}
pub fn json_content_type(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let Some(v) = values.next().and_then(|v| v.to_str().ok()) else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    let mut parts = v.split(';');
    if !parts
        .next()
        .is_some_and(|s| s.trim().eq_ignore_ascii_case("application/json"))
    {
        return false;
    }
    let parameters: Vec<_> = parts.collect();
    parameters.is_empty()
        || (parameters.len() == 1 && parameters[0].trim().eq_ignore_ascii_case("charset=utf-8"))
}
pub fn approved_headers(
    headers: &HeaderMap,
    profile: &ModelProfile,
) -> Result<HeaderMap, ApiError> {
    let nominated = |name: &str| {
        headers
            .get_all(header::CONNECTION)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| {
                v.split(',')
                    .any(|field| field.trim().eq_ignore_ascii_case(name))
            })
    };
    let mut result = HeaderMap::new();
    if nominated("content-type") || !json_content_type(headers) {
        return Err(ApiError::UnsupportedOption);
    }
    result.insert(header::CONTENT_TYPE, headers[header::CONTENT_TYPE].clone());
    for name in ["anthropic-version", "anthropic-beta"] {
        let mut values = headers.get_all(name).iter();
        let value = values.next();
        if values.next().is_some() {
            return Err(ApiError::UnsupportedOption);
        }
        if nominated(name) {
            return Err(ApiError::UnsupportedOption);
        }
        if let Some(v) = value {
            let s = v.to_str().map_err(|_| ApiError::UnsupportedOption)?;
            if name == "anthropic-version" {
                if s != "2023-06-01" {
                    return Err(ApiError::UnsupportedOption);
                }
            } else {
                let entries: Vec<_> = s.split(',').map(str::trim).collect();
                if entries.is_empty()
                    || entries
                        .iter()
                        .any(|s| !profile.approved_beta.iter().any(|p| p == s))
                    || entries.iter().collect::<BTreeSet<_>>().len() != entries.len()
                {
                    return Err(ApiError::UnsupportedOption);
                }
            }
            result.insert(name, v.clone());
        } else if name == "anthropic-version" {
            return Err(ApiError::UnsupportedOption);
        }
    }
    Ok(result)
}
/// 응답 usage의 한 필드(`usage.` 이하 점 경로)를 회계에 써도 되는지 판정한다.
/// 표준 요금과 같은 값만 허용하고, 요금이 달라지는 값(priority/batch tier, fast, us 지역,
/// 서버 도구 사용, 서버측 추가 iteration)이나 알 수 없는 필드는 거부해 unknown으로 남긴다.
pub(super) fn usage_field_settles(
    field: &str,
    kind: Kind,
    text: Option<&str>,
    integer: Option<u64>,
) -> bool {
    let child = |prefix: &str| {
        field
            .strip_prefix(prefix)
            .is_some_and(|rest| !rest.is_empty() && !rest.contains('.'))
    };
    match field {
        "input_tokens"
        | "output_tokens"
        | "cache_read_input_tokens"
        | "cache_creation_input_tokens"
        | "cache_creation.ephemeral_5m_input_tokens"
        | "cache_creation.ephemeral_1h_input_tokens" => integer.is_some(),
        "cache_creation" => kind == Kind::Object,
        "output_tokens_details" | "server_tool_use" => matches!(kind, Kind::Object | Kind::Null),
        "service_tier" | "speed" => kind == Kind::Null || text == Some("standard"),
        "inference_geo" => kind == Kind::Null || matches!(text, Some("" | "global")),
        "iterations" => matches!(kind, Kind::Array | Kind::Null),
        // output 내역은 output_tokens에 포함된 부분집합이라 별도 청구가 없다.
        _ if child("output_tokens_details.") => integer.is_some(),
        // 서버 도구는 요청 단위 별도 과금이므로 0회일 때만 표준 정산한다.
        _ if child("server_tool_use.") => integer == Some(0),
        _ => false,
    }
}
#[derive(Default)]
struct ResponseMetadata {
    model: Option<String>,
    message: bool,
    role: bool,
    content: bool,
    terminal: bool,
    input: Option<u64>,
    output: Option<u64>,
    cache_read: u64,
    cache_write: u64,
    cache5: Option<u64>,
    cache1: Option<u64>,
    counted: Option<u64>,
    explicit_error: bool,
    invalid_usage: bool,
}
impl Visitor for ResponseMetadata {
    fn value(&mut self, path: &[String], kind: Kind, raw: &str) -> Result<(), JsonError> {
        if path.is_empty() {
            return if kind == Kind::Object {
                Ok(())
            } else {
                Err(JsonError)
            };
        }
        if path.len() == 1 {
            match path[0].as_str() {
                "model" => self.model = Some(json::string(raw, 200)?),
                "type" => {
                    let response_type = json::string(raw, 32)?;
                    self.message = response_type == "message";
                    self.explicit_error |= response_type == "error";
                }
                "error" => self.explicit_error |= kind != Kind::Null,
                "role" => self.role = json::string(raw, 32)? == "assistant",
                "content" => self.content = kind == Kind::Array,
                "stop_reason" => {
                    self.terminal = kind == Kind::String
                        && [
                            "end_turn",
                            "max_tokens",
                            "stop_sequence",
                            "tool_use",
                            "pause_turn",
                            "refusal",
                        ]
                        .contains(&json::string(raw, 32)?.as_str())
                }
                "usage" => {
                    if kind != Kind::Object {
                        self.invalid_usage = true;
                    }
                }
                "input_tokens" => self.counted = integer(raw).ok(),
                _ => {}
            }
        }
        if path.first().is_some_and(|s| s == "usage") && path.len() > 1 {
            let field = path[1..].join(".");
            let text = if kind == Kind::String {
                json::string(raw, 64).ok()
            } else {
                None
            };
            let n = if kind == Kind::Number {
                integer(raw).ok()
            } else {
                None
            };
            if !usage_field_settles(&field, kind, text.as_deref(), n) {
                self.invalid_usage = true;
                return Ok(());
            }
            let Some(n) = n else {
                return Ok(());
            };
            match path
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .as_slice()
            {
                ["usage", "input_tokens"] => self.input = Some(n),
                ["usage", "output_tokens"] => self.output = Some(n),
                ["usage", "cache_read_input_tokens"] => self.cache_read = n,
                ["usage", "cache_creation_input_tokens"] => self.cache_write = n,
                ["usage", "cache_creation", "ephemeral_5m_input_tokens"] => self.cache5 = Some(n),
                ["usage", "cache_creation", "ephemeral_1h_input_tokens"] => self.cache1 = Some(n),
                _ => {}
            }
        }
        Ok(())
    }
}
async fn parse_response(
    bytes: &[u8],
    api: ApiKind,
) -> Result<(Option<FinalUsage>, Option<u64>), UpstreamError> {
    let mut v = ResponseMetadata::default();
    json::scan(bytes, &mut v)
        .await
        .map_err(|_| UpstreamError::Protocol)?;
    if v.explicit_error {
        return Err(UpstreamError::Protocol);
    }
    if api == ApiKind::CountTokens {
        return if v.counted.is_some() {
            Ok((None, v.counted))
        } else {
            Err(UpstreamError::Protocol)
        };
    }
    if !v.message || !v.role || !v.content || !v.terminal {
        return Err(UpstreamError::Protocol);
    }
    let cache = v.cache5.unwrap_or(0).checked_add(v.cache1.unwrap_or(0));
    if v.invalid_usage
        || v.cache1.unwrap_or(0) > 0
        || ((v.cache5.is_some() || v.cache1.is_some()) && cache != Some(v.cache_write))
    {
        return Ok((None, None));
    }
    let usage = match (v.model, v.input, v.output) {
        (Some(actual_model), Some(input_tokens), Some(output_tokens)) => Some(FinalUsage {
            actual_model,
            input_tokens,
            output_tokens,
            cache_read_tokens: v.cache_read,
            cache_write_tokens: v.cache_write,
            reasoning_tokens: None,
        }),
        _ => None,
    };
    Ok((usage, None))
}
async fn validate_native_error(bytes: &[u8]) -> Result<(), UpstreamError> {
    struct ErrorMetadata {
        error: bool,
        kind: bool,
        message: bool,
    }
    impl Visitor for ErrorMetadata {
        fn value(&mut self, path: &[String], kind: Kind, raw: &str) -> Result<(), JsonError> {
            if path.is_empty() && kind != Kind::Object {
                return Err(JsonError);
            }
            if path_is(path, &["type"]) {
                self.error = json::string(raw, 32)? == "error";
            }
            if path_is(path, &["error", "type"]) {
                self.kind = kind == Kind::String;
            }
            if path_is(path, &["error", "message"]) {
                self.message = kind == Kind::String;
            }
            Ok(())
        }
    }
    let mut v = ErrorMetadata {
        error: false,
        kind: false,
        message: false,
    };
    json::scan(bytes, &mut v)
        .await
        .map_err(|_| UpstreamError::Protocol)?;
    if v.error && v.kind && v.message {
        Ok(())
    } else {
        Err(UpstreamError::Protocol)
    }
}

/// 승인된 Messages 원본의 입력 필드만 선택한다. 대형 문자열/트리를 복제하지 않고
/// 동일 Bytes backing의 유한 slice들을 단일 HTTP body로 보낸다.
pub async fn count_projection(body: Bytes) -> Result<reqwest::Body, ApiError> {
    struct Projection {
        start: usize,
        fields: BTreeMap<String, std::ops::Range<usize>>,
    }
    impl Projection {
        fn collect(&mut self, path: &[String], raw: &str) {
            if path.len() == 1
                && ["model", "messages", "system", "tools", "tool_choice"]
                    .contains(&path[0].as_str())
            {
                let start = raw.as_ptr() as usize - self.start;
                self.fields
                    .insert(path[0].clone(), start..start + raw.len());
            }
        }
    }
    impl Visitor for Projection {
        fn value(&mut self, path: &[String], kind: Kind, raw: &str) -> Result<(), JsonError> {
            if !matches!(kind, Kind::Object | Kind::Array) {
                self.collect(path, raw);
            }
            Ok(())
        }
        fn object_end(
            &mut self,
            path: &[String],
            _: &BTreeSet<String>,
            raw: &str,
        ) -> Result<(), JsonError> {
            self.collect(path, raw);
            Ok(())
        }
        fn array_end(&mut self, path: &[String], _: usize, raw: &str) -> Result<(), JsonError> {
            self.collect(path, raw);
            Ok(())
        }
    }
    let mut projection = Projection {
        start: body.as_ptr() as usize,
        fields: BTreeMap::new(),
    };
    json::scan(&body, &mut projection)
        .await
        .map_err(|_| ApiError::InvalidRequest)?;
    if !projection.fields.contains_key("model") || !projection.fields.contains_key("messages") {
        return Err(ApiError::InvalidRequest);
    }
    let mut chunks = vec![Bytes::from_static(b"{")];
    for (index, (name, range)) in projection.fields.into_iter().enumerate() {
        if index > 0 {
            chunks.push(Bytes::from_static(b","));
        }
        chunks.push(Bytes::from(format!("\"{name}\":")));
        chunks.push(body.slice(range));
    }
    chunks.push(Bytes::from_static(b"}"));
    Ok(reqwest::Body::wrap_stream(futures_util::stream::iter(
        chunks.into_iter().map(Ok::<_, std::io::Error>),
    )))
}

/// native context 계약은 hidden/cache를 포함한 청구 입력 전체의 최악 상한이다.
/// count 추정치는 이 상한을 줄이거나 만들어내지 않는다. 실제 모델 계약은 운영자가 profile로 선언한다.
pub fn verified_input_bound(profile: &ModelProfile) -> Option<u64> {
    let contract = profile.input_bound_contract.as_ref()?;
    let categories = contract
        .input_categories
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if contract.kind != "anthropic_context_v1"
        || contract.context_tokens != profile.max_input_tokens
        || contract.context_tokens == 0
        || contract.context_tokens > 1_000_000_000
        || !matches!(profile.api, ApiKind::Messages | ApiKind::CountTokens)
        || categories != BTreeSet::from(["input", "cache_read", "cache_write"])
        || contract.input_categories.len() != 3
    {
        return None;
    }
    Some(contract.context_tokens)
}

#[cfg(test)]
mod response_tests {
    use super::*;

    #[tokio::test]
    async fn nonstream_usage_extensions_follow_pricing_effect() {
        let body = |usage: &str| {
            format!(
                r#"{{"type":"message","role":"assistant","model":"synthetic-text","content":[],"stop_reason":"end_turn","usage":{{"input_tokens":100,"output_tokens":20{usage}}}}}"#
            )
        };
        for ok in [
            r#","service_tier":"standard","inference_geo":"global","speed":"standard","server_tool_use":{"web_search_requests":0},"output_tokens_details":{"thinking_tokens":5},"iterations":[]"#,
            r#","service_tier":null,"inference_geo":null,"server_tool_use":null"#,
        ] {
            let (usage, _) = parse_response(body(ok).as_bytes(), ApiKind::Messages)
                .await
                .unwrap();
            assert_eq!(usage.map(|u| u.output_tokens), Some(20), "{ok}");
        }
        for bad in [
            r#","service_tier":"priority""#,
            r#","speed":"fast""#,
            r#","inference_geo":"us""#,
            r#","server_tool_use":{"web_search_requests":2}"#,
            r#","iterations":[{"input_tokens":1}]"#,
            r#","brand_new_billing":"x""#,
        ] {
            let (usage, _) = parse_response(body(bad).as_bytes(), ApiKind::Messages)
                .await
                .unwrap();
            assert!(usage.is_none(), "{bad} must stay unresolved");
        }
    }

    #[tokio::test]
    async fn nonstream_explicit_error_never_promotes_count_or_message_usage() {
        let message = r#""type":"message","role":"assistant","model":"synthetic-text","content":[],"stop_reason":"end_turn","usage":{"input_tokens":100,"output_tokens":20}"#;
        for (api, fields) in [
            (ApiKind::CountTokens, r#""input_tokens":1"#),
            (ApiKind::Messages, message),
        ] {
            for error in [
                r#""error":{"type":"api_error","message":"synthetic failure"}"#,
                r#""error":false"#,
                r#""error":[]"#,
            ] {
                for body in [
                    format!("{{{fields},{error}}}"),
                    format!("{{{error},{fields}}}"),
                ] {
                    assert!(
                        parse_response(body.as_bytes(), api).await.is_err(),
                        "명시 오류를 final 근거로 사용하면 안 된다"
                    );
                }
            }
            for body in [
                format!("{{{fields}}}"),
                format!("{{{fields},\"error\":null}}"),
                format!("{{\"error\":null,{fields}}}"),
            ] {
                assert!(parse_response(body.as_bytes(), api).await.is_ok());
            }
        }
        for body in [
            br#"{"type":"error","input_tokens":1}"#.as_slice(),
            br#"{"input_tokens":1,"type":"error"}"#,
        ] {
            assert!(parse_response(body, ApiKind::CountTokens).await.is_err());
        }
    }
}
