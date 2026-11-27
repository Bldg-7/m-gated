//! OpenAI 어댑터를 WASM 컴포넌트(`m-gated:adapter@0.3.0`)로 내보낸다(결정 0013).
//! WIT 타입과 어댑터 crate의 타입을 바꿔 줄 뿐이다.
//!
//! `scripts/build-adapter.sh`가 `wasm32-unknown-unknown`으로 빌드하고 컴포넌트로 감싼다.
//! 실행기가 없으므로 검사기는 실행을 넘기지 않고 한 번에 끝낸다([`json::ready`]).
//! 이 코드는 wasm32에서만 빌드한다. 다른 target에서는 WIT가 만든 import에 연결할 곳이 없다.
#![cfg(target_arch = "wasm32")]
use m_gated_adapter_kit::{
    adapter as kit,
    json::{self, NoYield},
    stream::{Event, Scalar},
};
use m_gated_adapter_openai::{self as openai, Api};
use std::cell::RefCell;

wit_bindgen::generate!({
    path: "../../../crates/adapter-kit/wit",
    world: "adapter",
});

use exports::m_gated::adapter::provider::{self as wit, Guest, GuestStreamReader};

fn strings(s: &[&str]) -> Vec<String> {
    s.iter().map(|s| s.to_string()).collect()
}
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
        chat_output_limit_field: p.chat_output_limit_field,
        embedding_dimensions: p.embedding_dimensions.map(|d| kit::EmbeddingDimensions {
            default: d.default,
            max: d.max,
        }),
        embedding_encodings: p.embedding_encodings,
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
        tokens: u
            .tokens
            .into_iter()
            .map(|(c, n)| (c.name().to_string(), n))
            .collect(),
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
            name: openai::NAME.into(),
            origin: openai::ORIGIN.into(),
            auth: wit::AuthScheme::Bearer,
            key_prefix: openai::KEY_PREFIX.into(),
            refused_key_prefixes: strings(openai::REFUSED_KEY_PREFIXES),
            verify_path: openai::VERIFY_PATH.into(),
            request_id_headers: strings(openai::REQUEST_ID_HEADERS),
            response_headers: openai::RESPONSE_ID_HEADERS
                .iter()
                .map(|name| wit::ResponseHeader {
                    name: name.to_string(),
                    rule: wit::HeaderRule::Id,
                })
                .collect(),
            apis: Api::ALL
                .into_iter()
                .map(|a| wit::ApiInfo {
                    kind: a.name().into(),
                    routes: strings(a.routes()),
                    upstream_path: a.upstream_path().into(),
                    error_format: error_format(openai::ERROR_FORMAT),
                    token_rule: a.token_rule().into(),
                    usage_categories: a
                        .usage_categories()
                        .iter()
                        .map(|c| c.name().to_string())
                        .collect(),
                    stream: a.stream(),
                    catalog: a.catalog(),
                })
                .collect(),
            models_dev_name: openai::MODELS_DEV_NAME.map(String::from),
        }
    }
    fn check_profile(api: String, p: wit::Profile) -> Result<(), wit::ProfileError> {
        if Api::from_name(&api).is_none() {
            return Err(wit::ProfileError::InvalidProfile);
        }
        openai::check_profile(&profile(p)).map_err(|e| match e {
            kit::ProfileError::InvalidPrice => wit::ProfileError::InvalidPrice,
            kit::ProfileError::InvalidProfile => wit::ProfileError::InvalidProfile,
        })
    }
    fn validate(
        api: String,
        body: Vec<u8>,
        p: wit::Profile,
    ) -> Result<wit::RequestMetadata, wit::ApiError> {
        let api = Api::from_name(&api).ok_or(wit::ApiError::UnsupportedOption)?;
        let m = json::ready(openai::validate_with(api, &body, &profile(p), NoYield))
            .map_err(api_error)?;
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
        if Api::from_name(&api).is_none() {
            return Err(wit::ApiError::UnsupportedOption);
        }
        let headers: Vec<_> = headers
            .iter()
            .map(|h| kit::Header {
                name: &h.name,
                value: &h.value,
            })
            .collect();
        openai::approved_headers(&headers)
            .map(|names| names.into_iter().map(String::from).collect())
            .map_err(api_error)
    }
    fn read(
        api: String,
        status: u16,
        body: Vec<u8>,
        request: Vec<u8>,
        p: wit::Profile,
    ) -> Result<wit::Reading, wit::ReadError> {
        let api = Api::from_name(&api).ok_or(wit::ReadError::Protocol)?;
        let r = json::ready(openai::read_with(
            api,
            status,
            &body,
            &request,
            &profile(p),
            &mut NoYield,
        ))
        .map_err(read_error)?;
        Ok(wit::Reading {
            usage: r.usage.map(usage),
            failed: r.failed,
        })
    }
}

/// 스트림 응답 하나. API 이름이 틀리거나 스트림이 없는 API면 첫 event에서 형식 오류를 낸다.
pub struct StreamReader(Option<RefCell<openai::Stream>>);
impl GuestStreamReader for StreamReader {
    fn new(api: String, p: wit::Profile) -> Self {
        Self(
            Api::from_name(&api)
                .and_then(|api| openai::Stream::new(api, &profile(p)))
                .map(RefCell::new),
        )
    }
    fn decoder(&self) -> wit::Decoder {
        let Some(inner) = &self.0 else {
            return wit::Decoder {
                chat: false,
                fields: Vec::new(),
                string_shapes: Vec::new(),
                content: Vec::new(),
            };
        };
        let rule = inner.borrow().decoder_rule();
        wit::Decoder {
            chat: rule.chat,
            fields: strings(rule.fields),
            string_shapes: strings(rule.string_shapes),
            content: Vec::new(),
        }
    }
    fn accept(&self, e: wit::Event) -> Result<wit::Step, wit::ReadError> {
        let inner = self.0.as_ref().ok_or(wit::ReadError::Protocol)?;
        let step = inner.borrow_mut().accept(event(e)?).map_err(read_error)?;
        Ok(wit::Step {
            terminal: step.terminal,
            error: step.error,
            content: step.content,
        })
    }
    fn final_usage(&self) -> Option<wit::Usage> {
        self.0.as_ref()?.borrow().final_usage().map(usage)
    }
    fn provider_error(&self) -> bool {
        self.0.as_ref().is_some_and(|s| s.borrow().provider_error())
    }
    fn close(&self) -> Result<wit::Closing, wit::ReadError> {
        let inner = self.0.as_ref().ok_or(wit::ReadError::Protocol)?;
        let (u, failed) = inner.borrow().close().map_err(read_error)?;
        Ok(wit::Closing {
            usage: u.map(usage),
            failed,
        })
    }
}

export!(Component);
