//! 업스트림 응답을 읽는 본체. 상태 코드, 인코딩, content-type, 크기 상한, 스트림 전달 순서는 여기서 정하고,
//! 본문과 event의 뜻은 어댑터([`port::Api`], [`port::StreamReader`])가 해석한다.
use super::{
    port,
    transport::{
        NativeClient, SendOptions, StreamProgress, UpstreamError, UpstreamResponse,
        classify_transport, keep_error_status, response_headers,
    },
};
use crate::{
    ledger::finance::{FinalObservation, FinalOutcome, SendPermit},
    runtime::{RequestControl, request::byte_queue},
};
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, HeaderValue, StatusCode, header},
};
use std::sync::Arc;

/// 오류 응답 본문의 크기 상한.
const ERROR_BYTES: usize = 64 * 1024;

impl NativeClient {
    /// 비스트림 요청을 한 번 보내고 응답을 읽는다.
    pub async fn send(
        &self,
        permit: SendPermit,
        body: impl Into<Bytes>,
        options: SendOptions<'_>,
    ) -> Result<UpstreamResponse, UpstreamError> {
        let body = body.into();
        let api = self.api(options.api)?;
        let response = self.begin_send(permit, body.clone(), options).await?;
        self.read_response(api, response, &body, options).await
    }
    /// HTTP 오류 응답의 본문을 읽지 못해도 오류 응답이라는 사실은 남긴다([`keep_error_status`]).
    async fn read_response(
        &self,
        api: &dyn port::Api,
        response: reqwest::Response,
        request: &[u8],
        options: SendOptions<'_>,
    ) -> Result<UpstreamResponse, UpstreamError> {
        let status = response.status();
        keep_error_status(
            status,
            self.read_body(api, response, request, options).await,
        )
    }
    async fn read_body(
        &self,
        api: &dyn port::Api,
        mut response: reqwest::Response,
        request: &[u8],
        options: SendOptions<'_>,
    ) -> Result<UpstreamResponse, UpstreamError> {
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
        if !crate::http::json_content_type(response.headers()) {
            return Err(UpstreamError::Protocol);
        }
        let limit = if status.is_success() {
            api.response_limit(options.profile)
        } else {
            ERROR_BYTES
        };
        if response.content_length().is_some_and(|n| n > limit as u64) {
            return Err(UpstreamError::TooLarge);
        }
        let mut bytes = crate::runtime::BoundedBuffer::new(limit);
        while let Some(chunk) = response.chunk().await.map_err(classify_transport)? {
            bytes.extend(&chunk).map_err(|_| UpstreamError::TooLarge)?;
        }
        let bytes = bytes.freeze();
        let reading = api.read(status, &bytes, request, options.profile).await?;
        let mut headers = response_headers(response.headers(), self.provider());
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert("x-should-retry", HeaderValue::from_static("false"));
        let completed = status == StatusCode::OK && !reading.failed;
        let observation = FinalObservation {
            outcome: if completed {
                FinalOutcome::Completed
            } else {
                FinalOutcome::Failed
            },
            usage: reading.usage,
            counted_input_tokens: reading.counted_input_tokens,
            reason: if completed {
                "completed"
            } else {
                "provider_error"
            },
            http_status: Some(status.as_u16()),
            provider_request_id: self.request_id(&headers),
        };
        Ok(UpstreamResponse {
            status,
            headers,
            body: bytes,
            observation,
        })
    }
    fn request_id(&self, headers: &HeaderMap) -> Option<String> {
        port::provider(self.provider())
            .request_id_headers()
            .iter()
            .find_map(|name| headers.get(*name))
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }
    /// 스트림 요청을 한 번 보내고, 받은 바이트를 그대로 클라이언트에 넘기면서 event를 어댑터로 읽는다.
    /// `progress`에는 감독자가 relay future를 버려도 남겨야 할 관측만 둔다.
    pub async fn relay(
        &self,
        permit: SendPermit,
        body: Bytes,
        options: SendOptions<'_>,
        control: Arc<RequestControl>,
        progress: &mut StreamProgress,
        head: impl FnOnce(StatusCode, HeaderMap, Body),
    ) -> Result<FinalObservation, UpstreamError> {
        let api = self.api(options.api)?;
        let mut reader = api.stream(options.profile).ok_or(UpstreamError::Protocol)?;
        let mut response = self.begin_send(permit, body.clone(), options).await?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(UpstreamError::Auth);
        }
        if status.is_redirection() {
            return Err(UpstreamError::Redirect);
        }
        if !status.is_success() {
            let result = self.read_response(api, response, &body, options).await?;
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
        let mut headers = response_headers(response.headers(), self.provider());
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        headers.insert("x-should-retry", HeaderValue::from_static("false"));
        let provider_request_id = self.request_id(&headers);
        let (queue, out) = byte_queue(control.clone());
        let mut delivery = Some((head, headers, out));
        let mut parser = reader.decoder();
        parser.reserve_retained(StreamProgress::RETAINED_BYTES);
        // 오류 event가 시작됐거나 왔으면, 그 앞의 성공 끝 event로 정산 근거를 되살리지 않는다.
        let mut invalidated = false;
        // Provider가 오류 뒤에 연결을 닫는 경우, 오류 event를 보낸 뒤 바로 읽기를 멈췄다.
        let mut stopped = false;
        let mut error_cr_boundary = None;
        let mut pending_error_tail = None;
        // blank CR의 terminal 후보는 같은 event의 선택 LF 상한 확인 전에는 정산 근거가 아니다.
        let mut terminal_boundary_pending = false;
        let result = async {
            'read: while let Some(chunk) = response.chunk().await.map_err(classify_transport)? {
                for part in chunk.chunks(crate::stream::CHUNK_BYTES) {
                    for &b in part {
                        let pending = terminal_boundary_pending;
                        if b != b'\n' {
                            terminal_boundary_pending = false;
                            if pending && !invalidated {
                                progress.final_usage = reader.final_usage();
                            }
                        }
                        // blank CR의 다음 LF도 같은 오류 event의 wire 상한/원본에 포함한다.
                        if let Some(size) = error_cr_boundary.take()
                            && b == b'\n'
                            && size >= ERROR_BYTES
                        {
                            progress.invalidate();
                            return Err(UpstreamError::StreamLimit);
                        }
                        let event = parser.byte(b);
                        if parser.native_error_in_progress() {
                            invalidated = true;
                            progress.invalidate();
                        }
                        let event = event.map_err(|error| {
                            let error = UpstreamError::from(error);
                            if error != UpstreamError::StreamLimit || terminal_boundary_pending {
                                progress.invalidate();
                            }
                            error
                        })?;
                        if pending && b == b'\n' && !invalidated {
                            progress.final_usage = reader.final_usage();
                        }
                        terminal_boundary_pending = false;
                        if let Some(event) = event {
                            // 첫 event를 받은 뒤에 끊기면 첫 응답 전 끊김(0원)이 아니다.
                            progress.started = true;
                            // 새 event의 의미 검증이 실패하면 이전 final은 절대로 복원하지 않는다.
                            progress.invalidate();
                            let size = event.wire_bytes;
                            let step = reader.accept(event)?;
                            progress.provider_error_observed = reader.provider_error();
                            invalidated |= step.error;
                            if progress.start.is_none() {
                                progress.start = reader.start();
                            }
                            for piece in &step.content {
                                match piece {
                                    Some(text) => progress.received.push(text),
                                    None => progress.received.drop_content(),
                                }
                            }
                            terminal_boundary_pending = step.terminal && parser.boundary_after_cr();
                            if step.terminal
                                && !invalidated
                                && (!terminal_boundary_pending || parser.optional_lf_fits())
                            {
                                progress.final_usage = reader.final_usage();
                            }
                            if step.error && parser.boundary_after_cr() {
                                error_cr_boundary = Some(size);
                            }
                        }
                    }
                    if let Some((head, headers, out)) = delivery.take() {
                        head(status, headers, out);
                    }
                    control.observe_parser(
                        parser.retained_peak()
                            + StreamProgress::RETAINED_BYTES
                            + progress.received.len(),
                        chunk.len(),
                    );
                    if error_cr_boundary.is_some() {
                        // 마지막 blank CR을 먼저 보내면 초과한 CRLF 오류가 SDK에서 완료될 수 있다.
                        // 최대 한 작업 chunk만 보류하고 다음 LF 검사 후 원래 순서로 보낸다.
                        let retained =
                            parser.retained_peak() + part.len() + StreamProgress::RETAINED_BYTES;
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
                        if reader.provider_error() && reader.stops_at_error() {
                            stopped = true;
                            break 'read;
                        }
                    }
                    tokio::task::yield_now().await;
                }
            }
            if !stopped {
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
            }
            Ok(())
        }
        .await;
        // upstream은 마감 전에 해제하며 오류 event로 queue 포화 대기를 추가하지 않는다.
        drop(response);
        let limit_after_final = match result {
            Ok(()) => false,
            Err(UpstreamError::StreamLimit)
                if !terminal_boundary_pending && !invalidated && progress.final_usage.is_some() =>
            {
                true
            }
            Err(error) => return Err(error),
        };
        let (usage, failed) = if limit_after_final {
            let (usage, _) = reader.close().map_err(|_| UpstreamError::StreamLimit)?;
            (Some(usage.ok_or(UpstreamError::StreamLimit)?), true)
        } else {
            reader.close()?
        };
        if !limit_after_final {
            queue.finish();
        }
        Ok(FinalObservation {
            outcome: if failed {
                FinalOutcome::Failed
            } else {
                FinalOutcome::Completed
            },
            usage,
            counted_input_tokens: None,
            reason: if limit_after_final {
                "stream_limit"
            } else if failed {
                "provider_error"
            } else {
                "completed"
            },
            http_status: Some(status.as_u16()),
            provider_request_id,
        })
    }
}
