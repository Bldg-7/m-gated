//! DeepSeek 어댑터를 WASM 컴포넌트로 내보낸다(결정 0009). WIT 타입과 어댑터 crate의 타입을 바꿔 줄 뿐이다.
//!
//! `scripts/build-adapter.sh`가 `wasm32-unknown-unknown`으로 빌드하고 컴포넌트로 감싼다.
//! 실행기가 없으므로 검사기는 실행을 넘기지 않고 한 번에 끝낸다([`json::ready`]).
//! 이 코드는 wasm32에서만 빌드한다. 다른 target에서는 WIT가 만든 import에 연결할 곳이 없다.
#![cfg(target_arch = "wasm32")]
use m_gated_adapter_deepseek as deepseek;
use m_gated_adapter_kit::{
    adapter as kit,
    json::{self, NoYield},
    stream::{Event, Scalar},
};
use std::cell::RefCell;

wit_bindgen::generate!({
    path: "../../../crates/adapter-kit/wit",
    world: "adapter",
});

use exports::m_gated::adapter::provider::{self as wit, Guest, GuestStreamReader};

/// 본체의 API 이름.
const CHAT: &str = "deepseek_chat";

fn profile(p: wit::Profile) -> kit::Profile {
    kit::Profile {
        model: p.model,
        features: p
            .features
            .into_iter()
            .map(|f| match f {
                wit::Feature::ClientTools => kit::Feature::ClientTools,
                wit::Feature::Thinking => kit::Feature::Thinking,
                wit::Feature::Cache => kit::Feature::Cache,
                wit::Feature::JsonSchema => kit::Feature::JsonSchema,
                wit::Feature::InlinePng => kit::Feature::InlinePng,
            })
            .collect(),
        approved_beta: p.approved_beta,
        max_output_tokens: p.max_output_tokens,
        rates: p.rates,
    }
}
fn api_error(e: kit::ApiError) -> wit::ApiError {
    match e {
        kit::ApiError::InvalidRequest => wit::ApiError::InvalidRequest,
        kit::ApiError::UnsupportedOption => wit::ApiError::UnsupportedOption,
        kit::ApiError::UnsupportedModel => wit::ApiError::UnsupportedModel,
    }
}
fn read_error(e: kit::ReadError) -> wit::ReadError {
    match e {
        kit::ReadError::Protocol => wit::ReadError::Protocol,
        kit::ReadError::StreamLimit => wit::ReadError::StreamLimit,
        kit::ReadError::TooLarge => wit::ReadError::TooLarge,
    }
}
fn error_format(f: kit::ErrorFormat) -> wit::ErrorFormat {
    match f {
        kit::ErrorFormat::Anthropic => wit::ErrorFormat::Anthropic,
        kit::ErrorFormat::Openai => wit::ErrorFormat::Openai,
        kit::ErrorFormat::Detail => wit::ErrorFormat::Detail,
    }
}
fn usage(u: kit::Usage) -> wit::Usage {
    wit::Usage {
        actual_model: u.actual_model,
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_read_tokens: u.cache_read_tokens,
        cache_write_tokens: u.cache_write_tokens,
        reasoning_tokens: u.reasoning_tokens,
    }
}
fn event(e: wit::Event) -> Result<Event, wit::ReadError> {
    Ok(Event {
        wire_bytes: usize::try_from(e.wire_bytes).map_err(|_| wit::ReadError::StreamLimit)?,
        name: e.name,
        values: e
            .values
            .into_iter()
            .map(|(k, v)| {
                let v = match v {
                    wit::Scalar::Object => Scalar::Object,
                    wit::Scalar::Array => Scalar::Array,
                    wit::Scalar::String(s) => Scalar::String(s),
                    wit::Scalar::StringShape => Scalar::StringShape,
                    wit::Scalar::Number(s) => Scalar::Number(s),
                    wit::Scalar::Bool(b) => Scalar::Bool(b),
                    wit::Scalar::Null => Scalar::Null,
                };
                (k, v)
            })
            .collect(),
    })
}

struct Component;
impl Guest for Component {
    type StreamReader = StreamReader;
    fn describe() -> wit::Description {
        wit::Description {
            name: deepseek::NAME.into(),
            origin: deepseek::ORIGIN.into(),
            auth: wit::AuthScheme::Bearer,
            key_prefix: deepseek::KEY_PREFIX.into(),
            refused_key_prefixes: deepseek::REFUSED_KEY_PREFIXES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            verify_path: deepseek::VERIFY_PATH.into(),
            request_id_headers: deepseek::REQUEST_ID_HEADERS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            response_headers: deepseek::RESPONSE_ID_HEADERS
                .iter()
                .map(|name| wit::ResponseHeader {
                    name: name.to_string(),
                    rule: wit::HeaderRule::Id,
                })
                .collect(),
            apis: vec![wit::ApiInfo {
                kind: CHAT.into(),
                routes: vec![deepseek::ROUTE.into()],
                upstream_path: deepseek::UPSTREAM_PATH.into(),
                error_format: error_format(deepseek::ERROR_FORMAT),
                token_rule: deepseek::TOKEN_RULE.into(),
                stream: true,
            }],
        }
    }
    fn check_profile(api: String, p: wit::Profile) -> Result<(), wit::ProfileError> {
        if api != CHAT {
            return Err(wit::ProfileError::InvalidProfile);
        }
        deepseek::check_profile(&profile(p)).map_err(|e| match e {
            kit::ProfileError::InvalidPrice => wit::ProfileError::InvalidPrice,
            kit::ProfileError::InvalidProfile => wit::ProfileError::InvalidProfile,
        })
    }
    fn validate(
        api: String,
        body: Vec<u8>,
        p: wit::Profile,
    ) -> Result<wit::RequestMetadata, wit::ApiError> {
        if api != CHAT {
            return Err(wit::ApiError::UnsupportedOption);
        }
        let m =
            json::ready(deepseek::validate_with(&body, &profile(p), NoYield)).map_err(api_error)?;
        Ok(wit::RequestMetadata {
            model: m.model,
            output_limit: m.output_limit,
            stream: m.stream,
        })
    }
    fn approved_headers(
        api: String,
        headers: Vec<wit::Header>,
    ) -> Result<Vec<String>, wit::ApiError> {
        if api != CHAT {
            return Err(wit::ApiError::UnsupportedOption);
        }
        let headers: Vec<_> = headers
            .iter()
            .map(|h| kit::Header {
                name: &h.name,
                value: &h.value,
            })
            .collect();
        deepseek::approved_headers(&headers)
            .map(|names| names.into_iter().map(String::from).collect())
            .map_err(api_error)
    }
    fn read(
        api: String,
        status: u16,
        body: Vec<u8>,
        _profile: wit::Profile,
    ) -> Result<wit::Reading, wit::ReadError> {
        if api != CHAT {
            return Err(wit::ReadError::Protocol);
        }
        let r =
            json::ready(deepseek::read_with(status, &body, &mut NoYield)).map_err(read_error)?;
        Ok(wit::Reading {
            usage: r.usage.map(usage),
            failed: r.failed,
        })
    }
}

/// 스트림 응답 하나. API 이름이 틀리면 첫 event에서 형식 오류를 낸다.
pub struct StreamReader {
    known: bool,
    inner: RefCell<deepseek::Stream>,
}
impl GuestStreamReader for StreamReader {
    fn new(api: String, _profile: wit::Profile) -> Self {
        Self {
            known: api == CHAT,
            inner: RefCell::default(),
        }
    }
    fn decoder(&self) -> wit::Decoder {
        let strings = |s: &[&str]| s.iter().map(|s| s.to_string()).collect();
        wit::Decoder {
            chat: true,
            fields: strings(deepseek::FIELDS),
            string_shapes: strings(deepseek::STRING_SHAPES),
            content: Vec::new(),
        }
    }
    fn accept(&self, e: wit::Event) -> Result<wit::Step, wit::ReadError> {
        if !self.known {
            return Err(wit::ReadError::Protocol);
        }
        let step = self
            .inner
            .borrow_mut()
            .accept(event(e)?)
            .map_err(read_error)?;
        Ok(wit::Step {
            terminal: step.terminal,
            error: step.error,
            content: step.content,
        })
    }
    fn final_usage(&self) -> Option<wit::Usage> {
        self.inner.borrow().final_usage().map(usage)
    }
    fn provider_error(&self) -> bool {
        self.inner.borrow().provider_error()
    }
    fn close(&self) -> Result<wit::Closing, wit::ReadError> {
        if !self.known {
            return Err(wit::ReadError::Protocol);
        }
        let (u, failed) = self.inner.borrow().close().map_err(read_error)?;
        Ok(wit::Closing {
            usage: u.map(usage),
            failed,
        })
    }
}

export!(Component);
