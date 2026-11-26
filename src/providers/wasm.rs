//! WASM 어댑터 컴포넌트를 port 뒤에 붙인다(결정 0009).
//!
//! - 컴포넌트는 import를 가질 수 없다. 그래서 네트워크, 파일, 시계, 난수에 닿지 못한다.
//! - 시작할 때 한 번 컴파일하고 인스턴스를 준비한다. 호출마다 store를 새로 만들어 요청 사이에
//!   상태를 나누지 않는다. 스트림은 요청 하나가 store 하나를 끝까지 쓴다.
//! - store마다 메모리 상한과 CPU 상한(fuel)이 있다. 긴 호출은 tokio에 실행을 넘긴다.
//! - 요청 검사 중에 멈추면 요청을 보내지 않고 거절한다. 응답을 읽다가 멈추면 형식 오류로
//!   처리하므로 요청은 `unknown`이 된다.
use super::{
    port::{
        self, Api, ApiError, BoxFuture, ErrorFormat, HeaderRule, Reading, RequestMetadata, Step,
        StreamReader,
    },
    transport::UpstreamError,
};
use crate::{
    config::{ApiKind, ModelProfile, ProviderId, StartupError},
    ledger::finance::FinalUsage,
    stream::{Event, SseDecoder},
};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    future::Future,
    io::Read,
    pin::pin,
    task::{Context, Poll, Waker},
};
use wasmtime::{
    Config, Engine, Store, StoreLimits, StoreLimitsBuilder,
    component::{Component, Linker, ResourceAny},
};

mod bindings {
    wasmtime::component::bindgen!({
        path: "crates/adapter-kit/wit",
        world: "adapter",
        exports: { default: async },
    });
}
use bindings::{Adapter, AdapterPre, exports::m_gated::adapter::provider as wit};

/// 컴포넌트 파일의 크기 상한.
const FILE_BYTES: u64 = 64 * 1024 * 1024;
/// 이만큼 fuel을 쓸 때마다 tokio에 실행을 넘긴다.
const YIELD_FUEL: u64 = 10_000_000;
/// store 하나의 상한.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// 메모리 상한. 응답 본문(최대 16 MiB)을 복사하고 읽을 만큼이다.
    pub memory_bytes: usize,
    /// 호출 하나(스트림은 event 하나)의 CPU 상한.
    pub fuel_per_call: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: 128 * 1024 * 1024,
            fuel_per_call: 20_000_000_000,
        }
    }
}
/// `decoder`가 고를 수 있는 필드 수와 경로 길이의 상한.
const DECODER_PATHS: usize = 256;
const PATH_BYTES: usize = 256;

/// 이 future는 fuel이 다 쓰여 실행을 넘길 때만 기다린다. 동기 port 메서드에서 끝날 때까지 poll한다.
/// 컴포넌트는 import가 없어 다른 것을 기다리지 않고, fuel 상한이 전체 시간을 막는다.
fn block<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
    }
}
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// 컴파일한 컴포넌트. 호출마다 여기서 store와 인스턴스를 만든다.
struct Shared {
    engine: Engine,
    pre: AdapterPre<StoreLimits>,
    limits: Limits,
}
impl Shared {
    fn store(&self) -> wasmtime::Result<Store<StoreLimits>> {
        let mut store = Store::new(
            &self.engine,
            StoreLimitsBuilder::new()
                .memory_size(self.limits.memory_bytes)
                // 컴포넌트 하나는 core 인스턴스 몇 개로 이루어진다.
                .instances(32)
                .trap_on_grow_failure(true)
                .build(),
        );
        store.limiter(|limits| limits);
        store.set_fuel(self.limits.fuel_per_call)?;
        store.fuel_async_yield_interval(Some(YIELD_FUEL))?;
        Ok(store)
    }
    async fn instance(&self) -> wasmtime::Result<(Store<StoreLimits>, Adapter)> {
        let mut store = self.store()?;
        let adapter = self.pre.instantiate_async(&mut store).await?;
        Ok((store, adapter))
    }
}

fn profile(p: &ModelProfile) -> wit::Profile {
    let p = port::adapter_profile(p);
    wit::Profile {
        model: p.model,
        features: p
            .features
            .into_iter()
            .map(|f| match f {
                m_gated_adapter_kit::adapter::Feature::ClientTools => wit::Feature::ClientTools,
                m_gated_adapter_kit::adapter::Feature::Thinking => wit::Feature::Thinking,
                m_gated_adapter_kit::adapter::Feature::Cache => wit::Feature::Cache,
                m_gated_adapter_kit::adapter::Feature::JsonSchema => wit::Feature::JsonSchema,
                m_gated_adapter_kit::adapter::Feature::InlinePng => wit::Feature::InlinePng,
            })
            .collect(),
        approved_beta: p.approved_beta,
        max_output_tokens: p.max_output_tokens,
        rates: p.rates,
    }
}
fn api_error(e: wit::ApiError) -> ApiError {
    match e {
        wit::ApiError::InvalidRequest => ApiError::InvalidRequest,
        wit::ApiError::UnsupportedOption => ApiError::UnsupportedOption,
        wit::ApiError::UnsupportedModel => ApiError::UnsupportedModel,
    }
}
fn read_error(e: wit::ReadError) -> UpstreamError {
    match e {
        wit::ReadError::Protocol => UpstreamError::Protocol,
        wit::ReadError::StreamLimit => UpstreamError::StreamLimit,
        wit::ReadError::TooLarge => UpstreamError::TooLarge,
    }
}
fn usage(u: wit::Usage) -> FinalUsage {
    FinalUsage {
        actual_model: u.actual_model,
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_read_tokens: u.cache_read_tokens,
        cache_write_tokens: u.cache_write_tokens,
        reasoning_tokens: u.reasoning_tokens,
    }
}
fn event(e: Event) -> wit::Event {
    use crate::stream::Scalar;
    wit::Event {
        name: e.name,
        wire_bytes: e.wire_bytes as u64,
        values: e
            .values
            .into_iter()
            .map(|(k, v)| {
                let v = match v {
                    Scalar::Object => wit::Scalar::Object,
                    Scalar::Array => wit::Scalar::Array,
                    Scalar::String(s) => wit::Scalar::String(s),
                    Scalar::StringShape => wit::Scalar::StringShape,
                    Scalar::Number(s) => wit::Scalar::Number(s),
                    Scalar::Bool(b) => wit::Scalar::Bool(b),
                    Scalar::Null => wit::Scalar::Null,
                };
                (k, v)
            })
            .collect(),
    }
}

/// WASM 어댑터 하나. 시작할 때 만들어 프로세스가 끝날 때까지 둔다.
pub struct WasmProvider {
    id: ProviderId,
    shared: &'static Shared,
    origin: &'static str,
    key_prefix: String,
    refused_key_prefixes: Vec<String>,
    verify_path: &'static str,
    request_id_headers: &'static [&'static str],
    response_headers: &'static [(&'static str, HeaderRule)],
    apis: &'static [&'static dyn Api],
}
impl port::Provider for WasmProvider {
    fn id(&self) -> ProviderId {
        self.id
    }
    fn origin(&self) -> &'static str {
        self.origin
    }
    fn apis(&self) -> &'static [&'static dyn Api] {
        self.apis
    }
    fn auth_header(&self, secret: &str) -> Option<(HeaderName, HeaderValue)> {
        super::openai::bearer(secret)
    }
    fn accepts_key(&self, secret: &str) -> bool {
        secret.starts_with(&self.key_prefix)
            && !self
                .refused_key_prefixes
                .iter()
                .any(|p| secret.starts_with(p.as_str()))
    }
    fn verify_path(&self) -> &'static str {
        self.verify_path
    }
    fn request_id_headers(&self) -> &'static [&'static str] {
        self.request_id_headers
    }
    fn response_headers(&self) -> &'static [(&'static str, HeaderRule)] {
        self.response_headers
    }
    fn check_profile(&self, p: &ModelProfile) -> Result<(), StartupError> {
        let api = self
            .apis
            .iter()
            .find(|a| a.kind() == p.api)
            .ok_or(StartupError::InvalidProfile)?;
        let native = port::api(self.id, p.api).ok_or(StartupError::InvalidProfile)?;
        if native.token_rule(p) != api.token_rule(p) {
            return Err(StartupError::InvalidAdapter);
        }
        let name = api_name(api.kind());
        let result = block(async {
            let (mut store, adapter) = self.shared.instance().await?;
            adapter
                .m_gated_adapter_provider()
                .call_check_profile(&mut store, name, &profile(p))
                .await
        });
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(wit::ProfileError::InvalidPrice)) => Err(StartupError::InvalidPrice),
            Ok(Err(wit::ProfileError::InvalidProfile)) => Err(StartupError::InvalidProfile),
            Err(_) => Err(StartupError::InvalidAdapter),
        }
    }
}

/// 설정 파일의 API 이름. WIT 함수에 넘긴다.
fn api_name(kind: ApiKind) -> &'static str {
    kind.name()
}

struct WasmApi {
    shared: &'static Shared,
    kind: ApiKind,
    routes: &'static [&'static str],
    upstream_path: &'static str,
    error_format: ErrorFormat,
    token_rule: &'static str,
    stream: bool,
}
impl Api for WasmApi {
    fn kind(&self) -> ApiKind {
        self.kind
    }
    fn routes(&self) -> &'static [&'static str] {
        self.routes
    }
    fn upstream_path(&self) -> &'static str {
        self.upstream_path
    }
    fn error_format(&self) -> ErrorFormat {
        self.error_format
    }
    fn validate<'a>(
        &'a self,
        body: &'a [u8],
        p: &'a ModelProfile,
    ) -> BoxFuture<'a, Result<RequestMetadata, ApiError>> {
        Box::pin(async move {
            let result = async {
                let (mut store, adapter) = self.shared.instance().await?;
                adapter
                    .m_gated_adapter_provider()
                    .call_validate(&mut store, api_name(self.kind), body, &profile(p))
                    .await
            }
            .await;
            match result {
                Ok(Ok(m)) => Ok(RequestMetadata {
                    model: m.model,
                    output_limit: m.output_limit,
                    stream: m.stream,
                }),
                Ok(Err(e)) => Err(api_error(e)),
                Err(_) => Err(ApiError::AdapterFailed),
            }
        })
    }
    /// 컴포넌트가 고른 이름은 클라이언트가 보낸 헤더여야 하고, `content-type`이 들어 있어야 한다.
    fn approved_headers(
        &self,
        incoming: &HeaderMap,
        _: &ModelProfile,
    ) -> Result<HeaderMap, ApiError> {
        let headers: Vec<wit::Header> = incoming
            .iter()
            .map(|(name, value)| wit::Header {
                name: name.as_str().to_owned(),
                value: value.as_bytes().to_vec(),
            })
            .collect();
        let result = block(async {
            let (mut store, adapter) = self.shared.instance().await?;
            adapter
                .m_gated_adapter_provider()
                .call_approved_headers(&mut store, api_name(self.kind), &headers)
                .await
        });
        let names = match result {
            Ok(Ok(names)) => names,
            Ok(Err(e)) => return Err(api_error(e)),
            Err(_) => return Err(ApiError::AdapterFailed),
        };
        if !names.iter().any(|n| n == "content-type")
            || names
                .iter()
                .any(|n| !incoming.keys().any(|k| k.as_str() == n))
        {
            return Err(ApiError::AdapterFailed);
        }
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        Ok(port::select_headers(incoming, &names))
    }
    fn read<'a>(
        &'a self,
        status: axum::http::StatusCode,
        body: &'a [u8],
        _request: &'a [u8],
        p: &'a ModelProfile,
    ) -> BoxFuture<'a, Result<Reading, UpstreamError>> {
        Box::pin(async move {
            let result = async {
                let (mut store, adapter) = self.shared.instance().await?;
                adapter
                    .m_gated_adapter_provider()
                    .call_read(
                        &mut store,
                        api_name(self.kind),
                        status.as_u16(),
                        body,
                        &profile(p),
                    )
                    .await
            }
            .await;
            match result {
                Ok(Ok(r)) => Ok(Reading {
                    usage: r.usage.map(usage),
                    counted_input_tokens: None,
                    failed: r.failed,
                }),
                Ok(Err(e)) => Err(read_error(e)),
                Err(_) => Err(UpstreamError::Protocol),
            }
        })
    }
    fn stream(&self, p: &ModelProfile) -> Option<Box<dyn StreamReader>> {
        if !self.stream {
            return None;
        }
        let opened: wasmtime::Result<_> = block(async {
            let (mut store, adapter) = self.shared.instance().await?;
            let reader = adapter
                .m_gated_adapter_provider()
                .stream_reader()
                .call_constructor(&mut store, api_name(self.kind), &profile(p))
                .await?;
            let decoder = adapter
                .m_gated_adapter_provider()
                .stream_reader()
                .call_decoder(&mut store, reader)
                .await?;
            Ok((store, adapter, reader, decoder))
        });
        Some(Box::new(match opened {
            Ok((store, adapter, reader, decoder)) if decoder_from(&decoder).is_some() => {
                WasmStream {
                    open: RefCell::new(Some(Open {
                        fuel_per_call: self.shared.limits.fuel_per_call,
                        store,
                        adapter,
                        reader,
                    })),
                    decoder: Some(decoder),
                }
            }
            _ => WasmStream::broken(),
        }))
    }
    fn token_rule(&self, _profile: &ModelProfile) -> &'static str {
        self.token_rule
    }
}

fn strs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}
/// 컴포넌트가 고른 필드 규칙으로 스트림 디코더를 만든다. 규칙이 상한을 넘으면 None이다.
fn decoder_from(d: &wit::Decoder) -> Option<SseDecoder> {
    let paths = [&d.fields, &d.string_shapes, &d.content];
    if paths.iter().map(|p| p.len()).sum::<usize>() > DECODER_PATHS
        || paths
            .iter()
            .flat_map(|p| p.iter())
            .any(|s| s.is_empty() || s.len() > PATH_BYTES || s.chars().any(char::is_control))
    {
        return None;
    }
    let decoder = if d.chat {
        SseDecoder::chat_with_string_shapes(&strs(&d.fields), &strs(&d.string_shapes))
    } else {
        SseDecoder::with_string_shapes(&strs(&d.fields), &strs(&d.string_shapes))
    };
    Some(if d.content.is_empty() {
        decoder
    } else {
        decoder.with_content(&strs(&d.content))
    })
}

struct Open {
    fuel_per_call: u64,
    store: Store<StoreLimits>,
    adapter: Adapter,
    reader: ResourceAny,
}
type Call<T> = Box<
    dyn for<'a> FnOnce(
        &'a mut Store<StoreLimits>,
        &'a Adapter,
        ResourceAny,
    ) -> BoxFuture<'a, wasmtime::Result<T>>,
>;
/// 스트림 응답 하나. 컴포넌트가 한 번 멈추면 그 뒤로는 형식 오류만 알린다.
/// port의 몇몇 메서드가 `&self`만 받으므로 store를 `RefCell`에 둔다.
struct WasmStream {
    open: RefCell<Option<Open>>,
    decoder: Option<wit::Decoder>,
}
impl WasmStream {
    fn broken() -> Self {
        Self {
            open: RefCell::new(None),
            decoder: None,
        }
    }
    /// 컴포넌트 함수 하나를 부른다. 호출마다 fuel을 다시 채운다. 멈추면 store를 버린다.
    fn call<T>(&self, f: Call<T>) -> Option<T> {
        let mut open = self.open.borrow_mut();
        let o = open.as_mut()?;
        let result = block(async {
            o.store.set_fuel(o.fuel_per_call)?;
            f(&mut o.store, &o.adapter, o.reader).await
        });
        if result.is_err() {
            *open = None;
        }
        result.ok()
    }
}
impl StreamReader for WasmStream {
    fn decoder(&self) -> SseDecoder {
        self.decoder
            .as_ref()
            .and_then(decoder_from)
            .unwrap_or_else(|| SseDecoder::chat(&[]))
    }
    fn accept(&mut self, e: Event) -> Result<Step, UpstreamError> {
        let e = event(e);
        let result = self.call(Box::new(move |store, adapter, reader| {
            Box::pin(async move {
                adapter
                    .m_gated_adapter_provider()
                    .stream_reader()
                    .call_accept(store, reader, &e)
                    .await
            })
        }));
        match result {
            Some(Ok(step)) => Ok(Step {
                terminal: step.terminal,
                error: step.error,
                content: step.content,
            }),
            Some(Err(e)) => Err(read_error(e)),
            None => Err(UpstreamError::Protocol),
        }
    }
    fn final_usage(&self) -> Option<FinalUsage> {
        self.call(Box::new(|store, adapter, reader| {
            Box::pin(async move {
                adapter
                    .m_gated_adapter_provider()
                    .stream_reader()
                    .call_final_usage(store, reader)
                    .await
            })
        }))
        .flatten()
        .map(usage)
    }
    fn provider_error(&self) -> bool {
        self.call(Box::new(|store, adapter, reader| {
            Box::pin(async move {
                adapter
                    .m_gated_adapter_provider()
                    .stream_reader()
                    .call_provider_error(store, reader)
                    .await
            })
        }))
        .unwrap_or(false)
    }
    fn close(&self) -> Result<(Option<FinalUsage>, bool), UpstreamError> {
        let result = self.call(Box::new(|store, adapter, reader| {
            Box::pin(async move {
                adapter
                    .m_gated_adapter_provider()
                    .stream_reader()
                    .call_close(store, reader)
                    .await
            })
        }));
        match result {
            Some(Ok(c)) => Ok((c.usage.map(usage), c.failed)),
            Some(Err(e)) => Err(read_error(e)),
            None => Err(UpstreamError::Protocol),
        }
    }
}

/// 컴포넌트 파일을 읽고, 해시를 확인하고, 컴파일하고, 사실을 읽어 Provider를 만든다.
pub fn load(
    id: ProviderId,
    path: &str,
    sha256: [u8; 32],
) -> Result<&'static dyn port::Provider, StartupError> {
    load_with(id, path, sha256, Limits::default())
}
/// [`load`]와 같지만 상한을 정할 수 있다. 시험에서 상한에 닿는 경우를 만들 때 쓴다.
/// 돌려준 Provider는 설치하지 않는다.
pub fn load_with(
    id: ProviderId,
    path: &str,
    sha256: [u8; 32],
    limits: Limits,
) -> Result<&'static dyn port::Provider, StartupError> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|_| StartupError::InvalidAdapter)?
        .take(FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| StartupError::InvalidAdapter)?;
    if bytes.len() as u64 > FILE_BYTES || <[u8; 32]>::from(Sha256::digest(&bytes)) != sha256 {
        return Err(StartupError::InvalidAdapter);
    }
    let mut config = Config::new();
    config.consume_fuel(true);
    let engine = Engine::new(&config).map_err(|_| StartupError::InvalidAdapter)?;
    let component = Component::new(&engine, &bytes).map_err(|_| StartupError::InvalidAdapter)?;
    if component.component_type().imports(&engine).next().is_some() {
        return Err(StartupError::InvalidAdapter);
    }
    let pre = Linker::new(&engine)
        .instantiate_pre(&component)
        .and_then(AdapterPre::new)
        .map_err(|_| StartupError::InvalidAdapter)?;
    // describe는 기본 상한으로 부른다. 시험용 작은 상한은 그 뒤의 호출에만 쓴다.
    let shared: &'static Shared = Box::leak(Box::new(Shared {
        engine,
        pre,
        limits: Limits::default(),
    }));
    let d = block(async {
        let (mut store, adapter) = shared.instance().await?;
        adapter
            .m_gated_adapter_provider()
            .call_describe(&mut store)
            .await
    })
    .map_err(|_| StartupError::InvalidAdapter)?;
    let shared: &'static Shared = Box::leak(Box::new(Shared {
        engine: shared.engine.clone(),
        pre: shared.pre.clone(),
        limits,
    }));
    describe(id, shared, d)
}

/// 컴포넌트가 알린 사실을 검사하고 Provider로 만든다.
///
/// 시험 단계에서는 Provider 이름과 API 이름이 본체 어댑터와 같아야 한다. 토큰 규칙은 profile 검사 때
/// 대조한다. 이 값들은 DB 검사, 장부, 대시보드에 목록으로 들어 있기 때문이다.
fn describe(
    id: ProviderId,
    shared: &'static Shared,
    d: wit::Description,
) -> Result<&'static dyn port::Provider, StartupError> {
    let native = port::native(id);
    let header = |s: &str| HeaderName::from_bytes(s.as_bytes()).is_ok() && s.len() <= 128;
    if d.name != id.name()
        || d.apis.is_empty()
        || d.key_prefix.is_empty()
        || url::Url::parse(&d.origin)
            .ok()
            .is_none_or(|u| u.scheme() != "https" || u.as_str() != format!("{}/", d.origin))
        || d.verify_path.is_empty()
        || d.verify_path.starts_with('/')
        || !d.request_id_headers.iter().all(|h| header(h))
        || !d.response_headers.iter().all(|h| header(&h.name))
    {
        return Err(StartupError::InvalidAdapter);
    }
    let mut apis: Vec<&'static dyn Api> = Vec::new();
    for a in d.apis {
        let kind = native
            .apis()
            .iter()
            .map(|n| n.kind())
            .find(|k| api_name(*k) == a.kind)
            .ok_or(StartupError::InvalidAdapter)?;
        let token_rule = leak(a.token_rule);
        if apis.iter().any(|x| x.kind() == kind)
            || a.routes.is_empty()
            || !a
                .routes
                .iter()
                .all(|r| r.starts_with('/') && r.len() <= 256)
            || a.upstream_path.starts_with('/')
        {
            return Err(StartupError::InvalidAdapter);
        }
        let routes: Vec<&'static str> = a.routes.into_iter().map(leak).collect();
        apis.push(Box::leak(Box::new(WasmApi {
            shared,
            kind,
            routes: Box::leak(routes.into_boxed_slice()),
            upstream_path: leak(a.upstream_path),
            error_format: match a.error_format {
                wit::ErrorFormat::Anthropic => ErrorFormat::Anthropic,
                wit::ErrorFormat::Openai => ErrorFormat::Openai,
                wit::ErrorFormat::Detail => ErrorFormat::Detail,
            },
            token_rule,
            stream: a.stream,
        })));
    }
    let request_id_headers: Vec<&'static str> =
        d.request_id_headers.into_iter().map(leak).collect();
    let response_headers: Vec<(&'static str, HeaderRule)> = d
        .response_headers
        .into_iter()
        .map(|h| {
            (
                leak(h.name),
                match h.rule {
                    wit::HeaderRule::Id => HeaderRule::Id,
                    wit::HeaderRule::Number => HeaderRule::Number,
                },
            )
        })
        .collect();
    let wit::AuthScheme::Bearer = d.auth;
    Ok(Box::leak(Box::new(WasmProvider {
        id,
        shared,
        origin: leak(d.origin),
        key_prefix: d.key_prefix,
        refused_key_prefixes: d.refused_key_prefixes,
        verify_path: leak(d.verify_path),
        request_id_headers: Box::leak(request_id_headers.into_boxed_slice()),
        response_headers: Box::leak(response_headers.into_boxed_slice()),
        apis: Box::leak(apis.into_boxed_slice()),
    })))
}
