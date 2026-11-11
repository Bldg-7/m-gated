//! Anthropic의 누계 usage와 message_stop을 다른 API의 terminal 규칙과 분리한다.
use super::anthropic::*;
use crate::{
    ledger::finance::{FinalObservation, FinalOutcome, FinalUsage, SendPermit},
    runtime::{RequestControl, request::byte_queue},
    stream::{Event, Scalar, SseDecoder},
};
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, HeaderValue, StatusCode, header},
};
use std::{collections::BTreeMap, sync::Arc};
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
                check_usage(&values, "message.usage.", self.cache_write)?;
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
                check_usage(&values, "usage.", self.cache_write)?;
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
    pub fn finish(&self) -> Result<FinalUsage, UpstreamError> {
        if !self.stopped {
            return Err(UpstreamError::Protocol);
        }
        Ok(FinalUsage {
            actual_model: self.model.clone().ok_or(UpstreamError::Protocol)?,
            input_tokens: self.input.ok_or(UpstreamError::Protocol)?,
            output_tokens: self.output.ok_or(UpstreamError::Protocol)?,
            cache_read_tokens: self.cache_read,
            cache_write_tokens: self.cache_write,
            reasoning_tokens: None,
        })
    }
}
fn number(values: &BTreeMap<String, Scalar>, key: &str) -> Result<Option<u64>, UpstreamError> {
    values
        .get(key)
        .map(|v| v.integer().ok_or(UpstreamError::Protocol))
        .transpose()
}
fn check_usage(
    values: &BTreeMap<String, Scalar>,
    prefix: &str,
    cache: u64,
) -> Result<(), UpstreamError> {
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
    if hour.unwrap_or(0) > 0
        || ((five.is_some() || hour.is_some())
            && five.unwrap_or(0).checked_add(hour.unwrap_or(0)) != Some(cache))
    {
        return Err(UpstreamError::Protocol);
    }
    Ok(())
}
impl AnthropicClient {
    pub async fn relay(
        &self,
        permit: SendPermit,
        body: Bytes,
        options: SendOptions<'_>,
        control: Arc<RequestControl>,
        progress: &mut super::transport::StreamProgress,
        head: impl FnOnce(StatusCode, HeaderMap, Body),
    ) -> Result<FinalObservation, UpstreamError> {
        if matches!(
            options.api,
            crate::config::ApiKind::Responses
                | crate::config::ApiKind::OpenaiChat
                | crate::config::ApiKind::DeepseekChat
        ) {
            return super::openai::relay(self, permit, body, options, control, progress, head)
                .await;
        }
        let mut response = self.begin_send(permit, body, options).await?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(UpstreamError::Auth);
        }
        if status.is_redirection() {
            return Err(UpstreamError::Redirect);
        }
        if !status.is_success() {
            let result = self.read_response(response, options).await?;
            head(
                result.status,
                result.headers,
                Body::from(control.retain_body(result.body)),
            );
            return Ok(result.observation);
        }
        if status != StatusCode::OK {
            return Err(UpstreamError::Protocol);
        }
        if response
            .headers()
            .get_all(header::CONTENT_ENCODING)
            .iter()
            .any(|v| v.as_bytes() != b"identity")
        {
            return Err(UpstreamError::Encoding);
        }
        let content = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok());
        if !content.is_some_and(|s| {
            s.eq_ignore_ascii_case("text/event-stream")
                || s.eq_ignore_ascii_case("text/event-stream; charset=utf-8")
        }) {
            return Err(UpstreamError::Protocol);
        }
        let mut headers = response_headers(response.headers());
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        headers.insert("x-should-retry", HeaderValue::from_static("false"));
        let provider_request_id = headers
            .get("request-id")
            .or_else(|| headers.get("x-request-id"))
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let (queue, body) = byte_queue(control.clone());
        let mut delivery = Some((head, headers, body));
        let mut parser = SseDecoder::with_string_shapes(FIELDS, &["error.type", "error.message"]);
        parser.reserve_retained(super::transport::StreamProgress::RETAINED_BYTES);
        let mut reducer = AnthropicStream::default();
        let mut native_error = false;
        let mut success_invalidated = false;
        let mut error_cr_boundary = None;
        let mut pending_error_tail = None;
        // blank CR의 terminal 후보는 같은 event의 선택 LF 상한 확인 전에는 정산 근거가 아니다.
        let mut terminal_boundary_pending = false;
        let result = async {
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(super::anthropic::classify_transport)?
            {
                for part in chunk.chunks(crate::stream::CHUNK_BYTES) {
                    for &b in part {
                        let pending = terminal_boundary_pending;
                        if b != b'\n' {
                            terminal_boundary_pending = false;
                            if pending && !success_invalidated {
                                progress.final_usage = reducer.finish().ok();
                            }
                        }
                        // blank CR의 다음 LF도 같은 오류 event의 wire 상한/원본에 포함한다.
                        if let Some(size) = error_cr_boundary.take()
                            && b == b'\n'
                            && size >= 64 * 1024
                        {
                            progress.invalidate();
                            return Err(UpstreamError::StreamLimit);
                        }
                        let event = parser.byte(b);
                        if parser.native_error_in_progress() {
                            success_invalidated = true;
                            progress.invalidate();
                        }
                        let event = event.map_err(|error| {
                            let error = UpstreamError::from(error);
                            if error != UpstreamError::StreamLimit || terminal_boundary_pending {
                                progress.invalidate();
                            }
                            error
                        })?;
                        if pending && b == b'\n' && !success_invalidated {
                            progress.final_usage = reducer.finish().ok();
                        }
                        terminal_boundary_pending = false;
                        if let Some(event) = event {
                            if native_error {
                                progress.invalidate();
                                return Err(UpstreamError::Protocol);
                            }
                            if event.values.get("type").and_then(Scalar::text) == Some("error") {
                                // 오류의 shape/크기가 유효한지와 무관하게 옛 성공 final의 근거는 폐기한다.
                                success_invalidated = true;
                                progress.invalidate();
                                if event.wire_bytes > 64 * 1024 {
                                    return Err(UpstreamError::StreamLimit);
                                }
                                if (!event.name.is_empty() && event.name != "error")
                                    || event.values.get("error.type") != Some(&Scalar::StringShape)
                                    || event.values.get("error.message")
                                        != Some(&Scalar::StringShape)
                                {
                                    return Err(UpstreamError::Protocol);
                                }
                                native_error = true;
                                progress.provider_error_observed = true;
                                if parser.boundary_after_cr() {
                                    error_cr_boundary = Some(event.wire_bytes);
                                }
                            } else {
                                let terminal = event.values.get("type").and_then(Scalar::text)
                                    == Some("message_stop");
                                reducer
                                    .event(event)
                                    .inspect_err(|_| progress.invalidate())?;
                                terminal_boundary_pending = terminal && parser.boundary_after_cr();
                                if terminal
                                    && (!terminal_boundary_pending || parser.optional_lf_fits())
                                {
                                    progress.final_usage = reducer.finish().ok();
                                }
                            }
                        }
                    }
                    if let Some((head, headers, body)) = delivery.take() {
                        head(status, headers, body);
                    }
                    control.observe_parser(
                        parser.retained_peak() + super::transport::StreamProgress::RETAINED_BYTES,
                        chunk.len(),
                    );
                    if error_cr_boundary.is_some() {
                        // 마지막 blank CR을 먼저 보내면 초과한 CRLF 오류가 SDK에서 완료될 수 있다.
                        // 최대 한 작업 chunk만 보류하고 다음 LF 검사 후 원래 순서로 보낸다.
                        let retained = parser.retained_peak()
                            + part.len()
                            + super::transport::StreamProgress::RETAINED_BYTES;
                        if retained > crate::stream::RETAINED_BYTES {
                            return Err(UpstreamError::StreamLimit);
                        }
                        control.observe_parser(retained, chunk.len());
                        pending_error_tail =
                            Some(control.retain_body(Bytes::copy_from_slice(part)));
                    } else {
                        if let Some(tail) = pending_error_tail.take() {
                            queue
                                .send(&tail)
                                .await
                                .map_err(|_| UpstreamError::Cancelled)?;
                        }
                        queue
                            .send(part)
                            .await
                            .map_err(|_| UpstreamError::Cancelled)?;
                        if native_error {
                            return Ok(None);
                        }
                    }
                    tokio::task::yield_now().await;
                }
            }
            parser
                .finish()
                .map_err(UpstreamError::from)
                .inspect_err(|_| progress.invalidate())?;
            if let Some(tail) = pending_error_tail.take() {
                queue
                    .send(&tail)
                    .await
                    .map_err(|_| UpstreamError::Cancelled)?;
            }
            if native_error {
                Ok(None)
            } else {
                reducer.finish().map(Some)
            }
        }
        .await;
        // upstream은 마감 전에 해제하며 오류 event로 queue 포화 대기를 추가하지 않는다.
        drop(response);
        let (usage, limit_after_final) = match result {
            Ok(usage) => (usage, false),
            Err(UpstreamError::StreamLimit)
                if !terminal_boundary_pending && !success_invalidated =>
            {
                (
                    Some(reducer.finish().map_err(|_| UpstreamError::StreamLimit)?),
                    true,
                )
            }
            Err(error) => return Err(error),
        };
        if !limit_after_final {
            queue.finish();
        }
        Ok(FinalObservation {
            outcome: if native_error || limit_after_final {
                FinalOutcome::Failed
            } else {
                FinalOutcome::Completed
            },
            usage,
            counted_input_tokens: None,
            reason: if limit_after_final {
                "stream_limit"
            } else if native_error {
                "provider_error"
            } else {
                "completed"
            },
            http_status: Some(status.as_u16()),
            provider_request_id,
        })
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
    const STANDARD_USAGE: &str = r#""input_tokens":100,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0},"output_tokens":1,"service_tier":"standard","inference_geo":"global","speed":"standard","server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"output_tokens_details":{"thinking_tokens":0},"iterations":[]"#;
    #[test]
    fn standard_priced_usage_extensions_settle() {
        let usage = run_stream(
            STANDARD_USAGE,
            r#""output_tokens":20,"output_tokens_details":{"thinking_tokens":15},"service_tier":"standard""#,
        )
        .unwrap();
        assert_eq!((usage.input_tokens, usage.output_tokens), (100, 20));
        let nulls = r#""input_tokens":100,"output_tokens":1,"service_tier":null,"inference_geo":null,"speed":null,"server_tool_use":null,"iterations":null,"output_tokens_details":null"#;
        run_stream(nulls, r#""output_tokens":20"#).unwrap();
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
