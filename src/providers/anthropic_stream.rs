//! Anthropic의 누계 usage와 message_stop을 다른 API의 terminal 규칙과 분리한다.
use super::anthropic::*;
use crate::{
    ledger::finance::{FinalUsage, UsageCategory},
    stream::{Event, Scalar, SseDecoder},
};
use std::collections::BTreeMap;
const FIELDS: &[&str] = &[
    "type",
    "message.type",
    "message.role",
    "message.model",
    "message.usage.**",
    "usage.**",
    "delta.stop_reason",
    "index",
    "content_block.type",
    "delta.type",
];
/// 끊긴 요청의 `received` 정산에 쓰는 출력 내용. 메모리에만 두고 저장하지 않는다.
/// 회계 metadata가 아니므로 한 조각이 decoder 상한을 넘어도 스트림을 끊지 않고 내용만 버린다.
const CONTENT: &[&str] = &["delta.text", "delta.thinking", "delta.partial_json"];
#[derive(Default)]
pub struct AnthropicStream {
    started: bool,
    delta: bool,
    stopped: bool,
    model: Option<String>,
    input: Option<u64>,
    output: Option<u64>,
    cache_read: u64,
    cache_write: u64,
    /// 캐시 쓰기의 (5분 몫, 1시간 몫). 결정 0012.
    /// None이면 시작 event가 캐시 쓰기를 나누지 않았다. 단가를 고를 수 없어 비용은 unknown이다.
    cache_split: Option<(u64, u64)>,
    blocks: BTreeMap<u64, bool>,
}
impl AnthropicStream {
    pub fn event(&mut self, event: Event) -> Result<(), UpstreamError> {
        let values = event.values;
        let kind = values
            .get("type")
            .and_then(Scalar::text)
            .ok_or(UpstreamError::Protocol)?;
        if !event.name.is_empty() && event.name != kind {
            return Err(UpstreamError::Protocol);
        }
        if self.stopped {
            return Err(UpstreamError::Protocol);
        }
        // 선택된 회계 metadata를 다른 event의 확장 필드로 무시하면 상충 final이 된다.
        for (root, allowed_event) in [
            ("message.usage", "message_start"),
            ("usage", "message_delta"),
        ] {
            if kind != allowed_event
                && values.keys().any(|key| {
                    key.strip_prefix(root)
                        .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with('.'))
                })
            {
                return Err(UpstreamError::Protocol);
            }
        }
        match kind {
            "message_start" => {
                if self.started
                    || values.get("message.type").and_then(Scalar::text) != Some("message")
                    || values.get("message.role").and_then(Scalar::text) != Some("assistant")
                {
                    return Err(UpstreamError::Protocol);
                }
                self.started = true;
                self.model = values
                    .get("message.model")
                    .and_then(Scalar::text)
                    .filter(|s| !s.is_empty() && s.len() <= 200)
                    .map(str::to_owned);
                self.input = number(&values, "message.usage.input_tokens")?;
                self.output = number(&values, "message.usage.output_tokens")?;
                self.cache_read =
                    number(&values, "message.usage.cache_read_input_tokens")?.unwrap_or(0);
                self.cache_write =
                    number(&values, "message.usage.cache_creation_input_tokens")?.unwrap_or(0);
                // 캐시 쓰기를 5분·1시간 몫으로 나누지 못하면 단가를 고를 수 없다(결정 0012).
                // 응답은 끝까지 전달하고, 비용만 unknown으로 둔다([`Self::unpriced`]).
                let (five, hour) = check_usage(&values, "message.usage.")?;
                self.cache_split = super::anthropic::cache_split(self.cache_write, five, hour);
            }
            "content_block_start" => {
                let index = number(&values, "index")?.ok_or(UpstreamError::Protocol)?;
                if !self.started
                    || self.delta
                    || self.blocks.len() >= 1024
                    || self.blocks.insert(index, false).is_some()
                {
                    return Err(UpstreamError::Protocol);
                }
            }
            "content_block_delta" => {
                let index = number(&values, "index")?.ok_or(UpstreamError::Protocol)?;
                if self.blocks.get(&index) != Some(&false) || self.delta {
                    return Err(UpstreamError::Protocol);
                }
            }
            "content_block_stop" => {
                let index = number(&values, "index")?.ok_or(UpstreamError::Protocol)?;
                let block = self.blocks.get_mut(&index).ok_or(UpstreamError::Protocol)?;
                if *block {
                    return Err(UpstreamError::Protocol);
                }
                *block = true;
            }
            "message_delta" => {
                if !self.started || self.delta || self.blocks.values().any(|closed| !*closed) {
                    return Err(UpstreamError::Protocol);
                }
                let stop = values
                    .get("delta.stop_reason")
                    .and_then(Scalar::text)
                    .ok_or(UpstreamError::Protocol)?;
                if ![
                    "end_turn",
                    "max_tokens",
                    "stop_sequence",
                    "tool_use",
                    "pause_turn",
                    "refusal",
                ]
                .contains(&stop)
                {
                    return Err(UpstreamError::Protocol);
                }
                // 끝 event가 캐시 쓰기를 다시 나눠 알리면 시작 event와 같아야 한다.
                let (five, hour) = check_usage(&values, "usage.")?;
                if (five.is_some() || hour.is_some())
                    && super::anthropic::cache_split(self.cache_write, five, hour)
                        != self.cache_split
                {
                    return Err(UpstreamError::Protocol);
                }
                let output =
                    number(&values, "usage.output_tokens")?.ok_or(UpstreamError::Protocol)?;
                if self.output.is_some_and(|n| output < n) {
                    return Err(UpstreamError::Protocol);
                }
                for (key, expected) in [
                    ("input_tokens", self.input),
                    ("cache_read_input_tokens", Some(self.cache_read)),
                    ("cache_creation_input_tokens", Some(self.cache_write)),
                ] {
                    if let Some(n) = number(&values, &format!("usage.{key}"))?
                        && Some(n) != expected
                    {
                        return Err(UpstreamError::Protocol);
                    }
                }
                self.output = Some(output);
                self.delta = true;
            }
            "message_stop" => {
                if !self.started || !self.delta || self.blocks.values().any(|closed| !*closed) {
                    return Err(UpstreamError::Protocol);
                }
                self.stopped = true;
            }
            "error" => return Err(UpstreamError::Protocol),
            "ping" => {}
            _ => {} // 확장 event도 JSON 전체 문법은 이미 검사했으며 terminal 근거로 쓰지 않는다.
        }
        Ok(())
    }
    /// `message_start`를 받은 뒤의 입력 usage. 끊긴 요청은 이 값을 입력 토큰으로 쓴다.
    pub fn start_usage(&self) -> Option<crate::ledger::finance::StartUsage> {
        if !self.started {
            return None;
        }
        Some(crate::ledger::finance::StartUsage {
            actual_model: self.model.clone()?,
            tokens: self.input_tokens(self.input?)?.into_iter().collect(),
        })
    }
    /// 시작 event가 캐시 쓰기를 5분·1시간 몫으로 나누지 않았다. 비용은 unknown이다(결정 0012).
    pub fn unpriced(&self) -> bool {
        self.started && self.cache_split.is_none()
    }
    /// 입력 쪽 칸(입력, 캐시 읽기, 5분·1시간 캐시 쓰기). 캐시 쓰기를 나누지 못했으면 None이다.
    fn input_tokens(&self, input: u64) -> Option<[(UsageCategory, u64); 4]> {
        let (five, hour) = self.cache_split?;
        Some([
            (UsageCategory::Input, input),
            (UsageCategory::CacheRead, self.cache_read),
            (UsageCategory::CacheWrite5m, five),
            (UsageCategory::CacheWrite1h, hour),
        ])
    }
    pub fn finish(&self) -> Result<FinalUsage, UpstreamError> {
        if !self.stopped {
            return Err(UpstreamError::Protocol);
        }
        let input = self.input.ok_or(UpstreamError::Protocol)?;
        let output = self.output.ok_or(UpstreamError::Protocol)?;
        Ok(FinalUsage::new(
            self.model.clone().ok_or(UpstreamError::Protocol)?,
            self.input_tokens(input)
                .ok_or(UpstreamError::Protocol)?
                .into_iter()
                .chain([(UsageCategory::Output, output)]),
            None,
        ))
    }
}
fn number(values: &BTreeMap<String, Scalar>, key: &str) -> Result<Option<u64>, UpstreamError> {
    values
        .get(key)
        .map(|v| v.integer().ok_or(UpstreamError::Protocol))
        .transpose()
}
/// usage 필드가 표준 요금의 값인지 검사하고, 이 event가 알린 캐시 쓰기의 (5분 몫, 1시간 몫)을 돌려준다.
fn check_usage(
    values: &BTreeMap<String, Scalar>,
    prefix: &str,
) -> Result<(Option<u64>, Option<u64>), UpstreamError> {
    let root = prefix.strip_suffix('.').ok_or(UpstreamError::Protocol)?;
    if values.get(root) != Some(&Scalar::Object) {
        return Err(UpstreamError::Protocol);
    }
    for (key, value) in values {
        let Some(field) = key.strip_prefix(prefix) else {
            continue;
        };
        let kind = match value {
            Scalar::Object => crate::json::Kind::Object,
            Scalar::Array => crate::json::Kind::Array,
            Scalar::String(_) | Scalar::StringShape => crate::json::Kind::String,
            Scalar::Number(_) => crate::json::Kind::Number,
            Scalar::Bool(_) => crate::json::Kind::Bool,
            Scalar::Null => crate::json::Kind::Null,
        };
        if !usage_field_settles(field, kind, value.text(), value.integer()) {
            return Err(UpstreamError::Protocol);
        }
    }
    let five = number(
        values,
        &format!("{prefix}cache_creation.ephemeral_5m_input_tokens"),
    )?;
    let hour = number(
        values,
        &format!("{prefix}cache_creation.ephemeral_1h_input_tokens"),
    )?;
    Ok((five, hour))
}
/// Anthropic 스트림 읽기 어댑터. 오류 event를 받으면 읽기를 멈춘다. Anthropic은 오류 뒤에 연결을 닫는다.
#[derive(Default)]
pub struct AnthropicReader {
    stream: AnthropicStream,
    native_error: bool,
}
impl super::port::StreamReader for AnthropicReader {
    fn decoder(&self) -> SseDecoder {
        SseDecoder::with_string_shapes(FIELDS, &["error.type", "error.message"])
            .with_content(CONTENT)
    }
    fn accept(&mut self, event: Event) -> Result<super::port::Step, UpstreamError> {
        if self.native_error {
            return Err(UpstreamError::Protocol);
        }
        let kind = event.values.get("type").and_then(Scalar::text);
        if kind == Some("error") {
            if event.wire_bytes > 64 * 1024 {
                return Err(UpstreamError::StreamLimit);
            }
            if (!event.name.is_empty() && event.name != "error")
                || event.values.get("error.type") != Some(&Scalar::StringShape)
                || event.values.get("error.message") != Some(&Scalar::StringShape)
            {
                return Err(UpstreamError::Protocol);
            }
            self.native_error = true;
            return Ok(super::port::Step {
                error: true,
                ..Default::default()
            });
        }
        let terminal = kind == Some("message_stop");
        // 받은 출력 내용은 event를 넘기기 전에 꺼내 둔다.
        // 상한을 넘어 버린 조각(StringShape)은 None으로 남겨 내용이 너무 컸다고 기록한다.
        let content = if kind == Some("content_block_delta") {
            CONTENT
                .iter()
                .filter_map(|key| match event.values.get(*key) {
                    Some(Scalar::String(s)) => Some(Some(s.clone())),
                    Some(Scalar::StringShape) => Some(None),
                    _ => None,
                })
                .collect()
        } else {
            Vec::new()
        };
        self.stream.event(event)?;
        Ok(super::port::Step {
            terminal,
            error: false,
            content,
        })
    }
    fn final_usage(&self) -> Option<FinalUsage> {
        if self.native_error {
            None
        } else {
            self.stream.finish().ok()
        }
    }
    fn start(&self) -> Option<crate::ledger::finance::StartUsage> {
        self.stream.start_usage()
    }
    fn unpriced(&self) -> bool {
        self.stream.unpriced()
    }
    fn provider_error(&self) -> bool {
        self.native_error
    }
    fn stops_at_error(&self) -> bool {
        true
    }
    fn close(&self) -> Result<(Option<FinalUsage>, bool), UpstreamError> {
        if self.native_error {
            Ok((None, true))
        } else if self.stream.stopped && self.stream.unpriced() {
            // 끝까지 받은 응답이다. 사용량만 단가를 고를 수 없다.
            Ok((None, false))
        } else {
            self.stream.finish().map(|usage| (Some(usage), false))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_stream(start_usage: &str, delta_usage: &str) -> Result<FinalUsage, UpstreamError> {
        let wire = format!(
            "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"type\":\"message\",\"role\":\"assistant\",\"model\":\"synthetic-text\",\"usage\":{{{start_usage}}}}}}}\n\n\
             event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}}}\n\n\
             event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"signature_delta\",\"signature\":\"sig\"}}}}\n\n\
             event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
             event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{{delta_usage}}}}}\n\n\
             event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
        );
        let mut parser = SseDecoder::new(FIELDS);
        let mut reducer = AnthropicStream::default();
        for b in wire.bytes() {
            if let Some(event) = parser.byte(b).map_err(|_| UpstreamError::Protocol)? {
                reducer.event(event)?;
            }
        }
        reducer.finish()
    }
    #[test]
    fn unsplit_cache_write_sends_the_stream_and_leaves_the_cost_unknown() {
        use super::super::port::StreamReader;
        let wire = "data: {\"type\":\"message_start\",\"message\":{\"type\":\"message\",\"role\":\"assistant\",\"model\":\"synthetic-text\",\"usage\":{\"input_tokens\":100,\"cache_creation_input_tokens\":30,\"output_tokens\":1}}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\ndata: {\"type\":\"message_stop\"}\n\n";
        let mut reader = AnthropicReader::default();
        let mut parser = reader.decoder();
        for b in wire.bytes() {
            if let Some(event) = parser.byte(b).unwrap() {
                // 캐시 쓰기를 나누지 않았어도 스트림은 끊지 않는다.
                reader.accept(event).unwrap();
                assert!(reader.unpriced());
                // 끊긴 요청에 쓸 시작 사용량도 만들지 않는다. 0원 정산을 막는다.
                assert!(reader.start().is_none());
            }
        }
        assert!(reader.final_usage().is_none());
        assert_eq!(reader.close(), Ok((None, false)));
    }
    const STANDARD_USAGE: &str = r#""input_tokens":100,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0},"output_tokens":1,"service_tier":"standard","inference_geo":"global","speed":"standard","server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"output_tokens_details":{"thinking_tokens":0},"iterations":[]"#;
    #[test]
    fn standard_priced_usage_extensions_settle() {
        let usage = run_stream(
            STANDARD_USAGE,
            r#""output_tokens":20,"output_tokens_details":{"thinking_tokens":15},"service_tier":"standard""#,
        )
        .unwrap();
        assert_eq!(
            (
                usage.get(UsageCategory::Input),
                usage.get(UsageCategory::Output)
            ),
            (100, 20)
        );
        let nulls = r#""input_tokens":100,"output_tokens":1,"service_tier":null,"inference_geo":null,"speed":null,"server_tool_use":null,"iterations":null,"output_tokens_details":null"#;
        run_stream(nulls, r#""output_tokens":20"#).unwrap();
    }
    #[test]
    fn cache_writes_split_into_five_minute_and_one_hour_categories() {
        let start = r#""input_tokens":100,"output_tokens":1,"cache_read_input_tokens":7,"cache_creation_input_tokens":30,"cache_creation":{"ephemeral_5m_input_tokens":10,"ephemeral_1h_input_tokens":20}"#;
        let usage = run_stream(start, r#""output_tokens":20"#).unwrap();
        assert_eq!(
            usage.tokens.iter().collect::<Vec<_>>(),
            vec![
                (UsageCategory::Input, 100),
                (UsageCategory::Output, 20),
                (UsageCategory::CacheRead, 7),
                (UsageCategory::CacheWrite5m, 10),
                (UsageCategory::CacheWrite1h, 20),
            ]
        );
        // 끝 event가 같은 값을 다시 알리는 것은 받는다.
        run_stream(
            start,
            r#""output_tokens":20,"cache_creation_input_tokens":30,"cache_creation":{"ephemeral_5m_input_tokens":10,"ephemeral_1h_input_tokens":20}"#,
        )
        .unwrap();
        for (start, delta) in [
            // 합이 다르다.
            (
                r#""input_tokens":100,"output_tokens":1,"cache_creation_input_tokens":31,"cache_creation":{"ephemeral_5m_input_tokens":10,"ephemeral_1h_input_tokens":20}"#,
                r#""output_tokens":20"#,
            ),
            // 캐시 쓰기가 있는데 나눈 값이 없다.
            (
                r#""input_tokens":100,"output_tokens":1,"cache_creation_input_tokens":30"#,
                r#""output_tokens":20"#,
            ),
            // 끝 event가 다르게 나눴다.
            (
                start,
                r#""output_tokens":20,"cache_creation":{"ephemeral_5m_input_tokens":20,"ephemeral_1h_input_tokens":10}"#,
            ),
        ] {
            assert!(run_stream(start, delta).is_err(), "{start} / {delta}");
        }
    }
    #[test]
    fn pricing_changing_usage_never_settles() {
        for change in [
            r#""service_tier":"priority""#,
            r#""service_tier":"batch""#,
            r#""speed":"fast""#,
            r#""inference_geo":"us""#,
            r#""server_tool_use":{"web_search_requests":1}"#,
            r#""iterations":[{"input_tokens":10,"output_tokens":5}]"#,
            r#""brand_new_billing":1"#,
        ] {
            let field = change.split(':').next().unwrap();
            let start = STANDARD_USAGE
                .split(',')
                .filter(|part| {
                    !part.starts_with(field)
                        && !part.contains("_requests")
                        && !part.starts_with("\"web_fetch")
                })
                .collect::<Vec<_>>()
                .join(",");
            let start = format!("{start},{change}");
            assert!(
                run_stream(&start, r#""output_tokens":20"#).is_err(),
                "{change}"
            );
            let delta = format!(r#""output_tokens":20,{change}"#);
            assert!(
                run_stream(r#""input_tokens":100,"output_tokens":1"#, &delta).is_err(),
                "delta {change}"
            );
        }
    }
    #[test]
    fn usage_outside_event_accounting_location_is_protocol_error() {
        let start = r#"{"type":"message_start","message":{"type":"message","role":"assistant","model":"synthetic-text","usage":{"input_tokens":100,"output_tokens":0}}}"#;
        let delta = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":20}}"#;
        for (index, event) in [
            (0, start),
            (1, delta),
            (2, r#"{"type":"message_stop"}"#),
            (2, r#"{"type":"ping"}"#),
        ] {
            for extra in [
                r#""usage":{"output_tokens":99999}"#,
                r#""message":{"usage":{"output_tokens":99999}}"#,
            ] {
                // 각 event의 정상 위치는 이 반례가 아니며 중복 JSON member도 만들지 않는다.
                if (index == 0 && extra.starts_with("\"message\""))
                    || (index == 1 && extra.starts_with("\"usage\""))
                {
                    continue;
                }
                let bad = format!("{},{}{}", &event[..event.len() - 1], extra, "}");
                let parts = [start, delta, r#"{"type":"message_stop"}"#];
                let mut parser = SseDecoder::new(FIELDS);
                let mut reducer = AnthropicStream::default();
                let mut rejected = false;
                for (i, part) in parts.iter().enumerate() {
                    let wire =
                        format!("data: {}\n\n", if i == index { bad.as_str() } else { part });
                    for b in wire.bytes() {
                        if let Some(event) = parser.byte(b).unwrap()
                            && reducer.event(event).is_err()
                        {
                            rejected = true;
                            break;
                        }
                    }
                    if rejected {
                        break;
                    }
                }
                assert!(rejected, "예상 밖 usage를 무시하고 final을 만들면 안 된다");
            }
        }
    }
    #[test]
    fn native_error_strings_do_not_consume_metadata_projection() {
        for (length, kind) in [
            (1024, "overloaded_error".to_string()),
            (1025, "overloaded_error".to_string()),
            (60 * 1024, "overloaded_error".to_string()),
            (1025, "x".repeat(16 * 1024)),
        ] {
            let wire = format!(
                "event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"{kind}\",\"message\":\"{}\"}}}}\n\n",
                "x".repeat(length)
            );
            let mut parser =
                SseDecoder::with_string_shapes(FIELDS, &["error.type", "error.message"]);
            let mut events = 0;
            let result = wire.bytes().try_for_each(|b| {
                if let Some(event) = parser.byte(b)? {
                    assert_eq!(event.values.get("error.type"), Some(&Scalar::StringShape));
                    assert_eq!(
                        event.values.get("error.message"),
                        Some(&Scalar::StringShape)
                    );
                    events += 1;
                }
                Ok::<_, crate::stream::ParseError>(())
            });
            assert!(
                result.is_ok(),
                "정상 native 오류 문자열 {length}bytes가 소실됐다"
            );
            assert_eq!(events, 1);
            assert!(parser.retained_peak() < 32 * 1024);
        }
    }
    #[test]
    fn empty_array_in_usage_is_not_a_valid_cache_category() {
        let wire = "data: {\"type\":\"message_start\",\"message\":{\"type\":\"message\",\"role\":\"assistant\",\"model\":\"synthetic-text\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0,\"cache_creation\":[]}}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\ndata: {\"type\":\"message_stop\"}\n\n";
        let mut parser = SseDecoder::with_string_shapes(FIELDS, &["error.type", "error.message"]);
        let mut reducer = AnthropicStream::default();
        let result = wire
            .bytes()
            .try_for_each(|b| {
                if let Some(event) = parser.byte(b).map_err(|_| UpstreamError::Protocol)? {
                    reducer.event(event)?;
                }
                Ok(())
            })
            .and_then(|_| reducer.finish());
        assert!(
            result.is_err(),
            "skip한 빈배열이 유효 usage 객체로 위장되면 안 된다"
        );
    }
}
