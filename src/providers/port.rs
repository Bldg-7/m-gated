//! Provider port. 게이트웨이 본체가 Provider에 대해 아는 것은 이 파일의 trait뿐이다.
//!
//! 어댑터는 요청과 응답을 읽고 그 결과를 알려 주기만 한다. 송신, 응답 크기 상한, 정산 규칙,
//! `unknown` 처리, 사용자 차단, 실행 기한은 본체가 맡는다. 그래서 어댑터가 사용량을 잘못 읽어도
//! 본체의 안전 규칙은 그대로 지켜진다.
//!
//! Provider를 더하려면 [`ProviderId`]에 이름을 더하고, [`Provider`]와 [`Api`]를 구현한 뒤
//! `NATIVE`에 넣는다. WASM 어댑터는 [`install`]로 설치한다(결정 0009).
use super::transport::UpstreamError;
use crate::{
    config::{ApiKind, ModelProfile, NativeFeature, ProviderId, StartupError},
    ledger::finance::{FinalUsage, StartUsage},
    stream::{Event, SseDecoder},
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
pub use futures_util::future::BoxFuture;
use m_gated_adapter_kit::adapter as kit;
pub use m_gated_adapter_kit::adapter::{ErrorFormat, RequestMetadata, Step};
use std::sync::RwLock;

/// 요청 검사 실패.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiError {
    InvalidRequest,
    UnsupportedOption,
    UnsupportedModel,
    /// WASM 어댑터가 멈췄거나 상한에 닿았다. 클라이언트 잘못이 아니므로 500으로 거절한다.
    AdapterFailed,
}
impl From<kit::ApiError> for ApiError {
    fn from(e: kit::ApiError) -> Self {
        match e {
            kit::ApiError::InvalidRequest => Self::InvalidRequest,
            kit::ApiError::UnsupportedOption => Self::UnsupportedOption,
            kit::ApiError::UnsupportedModel => Self::UnsupportedModel,
        }
    }
}

/// Provider 하나. 키 형식, 인증 헤더, 키 확인 경로와 이 Provider가 가진 API 목록을 알려 준다.
pub trait Provider: Sync {
    fn id(&self) -> ProviderId;
    /// 업스트림 주소. 설정 파일의 `base_url`은 이 값과 같아야 한다.
    fn origin(&self) -> &'static str;
    fn apis(&self) -> &'static [&'static dyn Api];
    /// 키를 업스트림 인증 헤더로 만든다. 헤더 값으로 쓸 수 없는 키면 None이다.
    fn auth_header(&self, secret: &str) -> Option<(HeaderName, HeaderValue)>;
    /// 키 접두사 검사. 다른 Provider의 키를 잘못 붙여 넣는 실수를 막는다.
    /// 길이와 문자 검사는 본체가 한다.
    fn accepts_key(&self, secret: &str) -> bool;
    /// 키 등록 전에 키를 확인하는 과금 없는 GET 경로(`base_url` 기준 상대 경로).
    fn verify_path(&self) -> &'static str;
    /// 키 확인과 내용 세기 요청에 붙이는 고정 헤더. 추론 요청에 보낼 헤더는 [`Api::forwarded_headers`]로 고른다.
    fn fixed_headers(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }
    /// 응답에서 Provider 요청 id를 읽을 헤더. 앞의 것이 먼저다.
    fn request_id_headers(&self) -> &'static [&'static str] {
        &["x-request-id"]
    }
    /// 클라이언트에 그대로 넘기는 응답 헤더. 값이 하나뿐이고 규칙에 맞을 때만 넘긴다.
    /// `retry-after`(1–120초)는 본체가 모든 Provider에 넘긴다.
    fn response_headers(&self) -> &'static [(&'static str, HeaderRule)] {
        &[
            ("request-id", HeaderRule::Id),
            ("x-request-id", HeaderRule::Id),
        ]
    }
    /// 캐시 기능을 쓰는 카탈로그 모델에 필요한 캐시 쓰기 가격 항목. 없으면 None이다.
    fn cache_write_rate(&self) -> Option<&'static str> {
        None
    }
    /// 이 Provider에만 있는 profile 규칙. 공통 규칙은 본체가 먼저 검사한다.
    fn check_profile(&self, _profile: &ModelProfile) -> Result<(), StartupError> {
        Ok(())
    }
    /// 공개 가격 목록(models.dev)에서 이 Provider의 모델을 찾을 이름. 없으면 카탈로그 동기화를 하지 않는다.
    fn models_dev_name(&self) -> Option<&'static str> {
        None
    }
    /// 끊긴 스트림에서 받은 출력 내용의 토큰 수를 센다(결정 0005의 `received` 규칙).
    /// 셀 수 없는 Provider는 None을 돌려준다. 송신은 `http`가 한다.
    fn count_output<'a>(
        &'a self,
        _http: &'a dyn SideRequest,
        _model: &'a str,
        _text: &'a str,
    ) -> Option<BoxFuture<'a, Result<u64, UpstreamError>>> {
        None
    }
}

/// 본체가 어댑터에 빌려주는 과금 없는 보조 요청. 주소, 키, 고정 헤더, 기한, 응답 크기 상한은 본체가 정한다.
pub trait SideRequest: Sync {
    /// `path`에 JSON `body`를 POST하고 200 응답 본문을 돌려준다. 그 밖의 응답은 오류다.
    fn post_json<'a>(
        &'a self,
        path: &'a str,
        body: String,
    ) -> BoxFuture<'a, Result<Vec<u8>, UpstreamError>>;
}

/// 응답 헤더 값의 규칙.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderRule {
    /// 1–256바이트, 제어 문자 없음.
    Id,
    /// 10진 숫자만.
    Number,
}

/// 비스트림 응답을 읽은 결과.
pub struct Reading {
    /// 정산에 쓸 사용량. 확인하지 못했으면 None이고, 본체는 이 요청을 `unknown`으로 남긴다.
    pub usage: Option<FinalUsage>,
    /// 토큰 세기 API가 알려 준 입력 토큰 수.
    pub counted_input_tokens: Option<u64>,
    /// 200 응답이지만 Provider가 실패로 알렸다.
    pub failed: bool,
}

/// Provider의 API 하나(예: Anthropic Messages).
pub trait Api: Sync {
    fn kind(&self) -> ApiKind;
    /// 게이트웨이가 이 API로 받는 경로.
    fn routes(&self) -> &'static [&'static str];
    /// 업스트림 경로(`base_url` 기준 상대 경로).
    fn upstream_path(&self) -> &'static str;
    fn error_format(&self) -> ErrorFormat;
    /// 요청 URL의 query를 받아도 되는지. query는 업스트림에 보내지 않는다.
    fn query_allowed(&self, _query: &str) -> bool {
        false
    }
    /// zstd로 압축한 요청 본문(`Content-Encoding: zstd`)을 받는지. 본체가 상한 안에서 풀어 검사하고,
    /// 업스트림에는 푼 본문을 보낸다.
    fn accepts_zstd(&self) -> bool {
        false
    }
    /// 요청 본문을 검사한다. 허용 목록에 없는 필드와 값은 거부한다.
    fn validate<'a>(
        &'a self,
        body: &'a [u8],
        profile: &'a ModelProfile,
    ) -> BoxFuture<'a, Result<RequestMetadata, ApiError>>;
    /// 클라이언트 헤더에서 업스트림에 보낼 헤더를 고른다. `content-type`은 반드시 들어 있다.
    fn approved_headers(
        &self,
        incoming: &HeaderMap,
        profile: &ModelProfile,
    ) -> Result<HeaderMap, ApiError>;
    /// `approved_headers` 가운데 `content-type` 말고 업스트림에 보낼 헤더 이름.
    fn forwarded_headers(&self) -> &'static [&'static str] {
        &[]
    }
    /// 성공 응답 본문의 크기 상한. 오류 응답의 상한은 본체가 정한다.
    fn response_limit(&self, profile: &ModelProfile) -> usize {
        profile.max_response_bytes.min(16 * 1024 * 1024)
    }
    /// 비스트림 응답 본문을 읽는다. 본체가 상태 코드, 인코딩, content-type, 크기를 먼저 확인한다.
    /// `request`는 클라이언트가 보낸 요청 본문이다.
    fn read<'a>(
        &'a self,
        status: StatusCode,
        body: &'a [u8],
        request: &'a [u8],
        profile: &'a ModelProfile,
    ) -> BoxFuture<'a, Result<Reading, UpstreamError>>;
    /// 스트림 응답을 읽는 상태 기계. 스트림이 없는 API는 None이다.
    fn stream(&self, _profile: &ModelProfile) -> Option<Box<dyn StreamReader>> {
        None
    }
    /// 시작 event가 입력 토큰을 알려 줘서, 시작 뒤에 끊긴 스트림을 정산 규칙으로 바로 정산할 수 있는지.
    fn settles_cutoff(&self) -> bool {
        false
    }
    /// 가격이 0인 보장 무료 profile을 둘 수 있는지.
    fn may_be_free(&self) -> bool {
        false
    }
    /// 가격표 snapshot에 남기는 토큰 포함 규칙 이름.
    fn token_rule(&self, profile: &ModelProfile) -> &'static str;
    /// 카탈로그 정책(모델과 가격을 DB에서 관리)을 쓸 수 있는지.
    fn catalog(&self) -> bool {
        false
    }
}

/// 스트림 응답 하나를 읽는 상태 기계. event를 차례로 받아 검사하고, 사용량을 알려 준다.
pub trait StreamReader: Send {
    fn decoder(&self) -> SseDecoder;
    /// event 하나를 받는다. 형식이 맞지 않으면 오류를 돌려주고, 그 뒤로는 사용량을 알려 주지 않는다.
    fn accept(&mut self, event: Event) -> Result<Step, UpstreamError>;
    /// 끝 event를 받은 뒤 정산에 쓸 사용량. 오류 event 뒤에는 None이다.
    fn final_usage(&self) -> Option<FinalUsage>;
    /// 시작 event가 알린 입력 사용량. 끊긴 요청의 입력 토큰이다.
    fn start(&self) -> Option<StartUsage> {
        None
    }
    /// Provider가 실패를 알렸는지(오류 event, 실패 끝 event).
    fn provider_error(&self) -> bool;
    /// 오류 event를 보낸 뒤 바로 읽기를 멈출지. Provider가 오류 뒤에 연결을 닫는 경우다.
    fn stops_at_error(&self) -> bool {
        false
    }
    /// 스트림이 끝났을 때의 사용량과 실패 여부. 끝 event 없이 끝났으면 오류다.
    fn close(&self) -> Result<(Option<FinalUsage>, bool), UpstreamError>;
}

/// 어댑터 crate(`adapter-kit`을 쓰는 crate)에 넘기는 profile.
pub fn adapter_profile(profile: &ModelProfile) -> kit::Profile {
    kit::Profile {
        model: profile.model.clone(),
        features: profile
            .features
            .iter()
            .map(|f| match f {
                NativeFeature::ClientTools => kit::Feature::ClientTools,
                NativeFeature::Thinking => kit::Feature::Thinking,
                NativeFeature::Cache => kit::Feature::Cache,
                NativeFeature::JsonSchema => kit::Feature::JsonSchema,
                NativeFeature::InlinePng => kit::Feature::InlinePng,
            })
            .collect(),
        approved_beta: profile.approved_beta.clone(),
        max_output_tokens: profile.max_output_tokens,
        rates: profile.rates.keys().cloned().collect(),
    }
}
/// 어댑터 crate에 넘기는 클라이언트 헤더. `HeaderMap`의 이름은 이미 소문자다.
pub fn adapter_headers(headers: &HeaderMap) -> Vec<kit::Header<'_>> {
    headers
        .iter()
        .map(|(name, value)| kit::Header {
            name: name.as_str(),
            value: value.as_bytes(),
        })
        .collect()
}
/// 어댑터가 고른 이름의 헤더만 남긴다. 값은 클라이언트가 보낸 그대로다.
pub fn select_headers(incoming: &HeaderMap, names: &[&str]) -> HeaderMap {
    let mut out = HeaderMap::new();
    for name in names {
        for value in incoming.get_all(*name) {
            out.append(
                HeaderName::from_bytes(name.as_bytes()).expect("받은 헤더 이름"),
                value.clone(),
            );
        }
    }
    out
}
impl From<kit::ReadError> for UpstreamError {
    fn from(e: kit::ReadError) -> Self {
        match e {
            kit::ReadError::Protocol => Self::Protocol,
            kit::ReadError::StreamLimit => Self::StreamLimit,
            kit::ReadError::TooLarge => Self::TooLarge,
        }
    }
}
impl From<kit::Usage> for FinalUsage {
    fn from(u: kit::Usage) -> Self {
        Self {
            actual_model: u.actual_model,
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_tokens: u.cache_read_tokens,
            cache_write_tokens: u.cache_write_tokens,
            reasoning_tokens: u.reasoning_tokens,
        }
    }
}
impl From<kit::ProfileError> for StartupError {
    fn from(e: kit::ProfileError) -> Self {
        match e {
            kit::ProfileError::InvalidPrice => Self::InvalidPrice,
            kit::ProfileError::InvalidProfile => Self::InvalidProfile,
        }
    }
}

/// 본체에 들어 있는 Provider 어댑터 전부.
static NATIVE: [&dyn Provider; 4] = [
    &super::anthropic::PROVIDER,
    &super::openai::PROVIDER,
    &super::deepseek::PROVIDER,
    &super::typesafe::PROVIDER,
];
/// 시작할 때 설치한 WASM 어댑터. 설치한 Provider는 본체 어댑터 대신 이것을 쓴다.
/// 한 프로세스는 Provider마다 구현을 하나만 쓴다(결정 0009).
static INSTALLED: RwLock<Vec<Installed>> = RwLock::new(Vec::new());
struct Installed {
    id: ProviderId,
    sha256: [u8; 32],
    provider: &'static dyn Provider,
}
/// WASM 어댑터를 설치한다. 같은 파일이 이미 설치돼 있으면 `load`를 부르지 않는다.
/// 다른 파일이 설치돼 있으면 거절한다.
pub fn install(
    id: ProviderId,
    sha256: [u8; 32],
    load: impl FnOnce() -> Result<&'static dyn Provider, StartupError>,
) -> Result<(), StartupError> {
    let mut installed = INSTALLED.write().expect("INSTALLED lock");
    if let Some(i) = installed.iter().find(|i| i.id == id) {
        return if i.sha256 == sha256 {
            Ok(())
        } else {
            Err(StartupError::InvalidAdapter)
        };
    }
    let provider = load()?;
    if provider.id() != id {
        return Err(StartupError::InvalidAdapter);
    }
    installed.push(Installed {
        id,
        sha256,
        provider,
    });
    Ok(())
}
/// 이 Provider에 WASM 어댑터가 설치돼 있는지.
pub fn installed(id: ProviderId) -> bool {
    INSTALLED
        .read()
        .expect("INSTALLED lock")
        .iter()
        .any(|i| i.id == id)
}
/// 이 게이트웨이가 아는 Provider 전부. 설치한 WASM 어댑터가 본체 어댑터를 대신한다.
pub fn providers() -> [&'static dyn Provider; 4] {
    NATIVE.map(|p| provider(p.id()))
}
/// 본체에 들어 있는 어댑터. WASM 어댑터가 내놓은 사실을 이것과 대조한다.
pub fn native(id: ProviderId) -> &'static dyn Provider {
    *NATIVE
        .iter()
        .find(|p| p.id() == id)
        .expect("모든 ProviderId는 NATIVE에 있다")
}
pub fn provider(id: ProviderId) -> &'static dyn Provider {
    INSTALLED
        .read()
        .expect("INSTALLED lock")
        .iter()
        .find(|i| i.id == id)
        .map_or_else(|| native(id), |i| i.provider)
}
pub fn api(id: ProviderId, kind: ApiKind) -> Option<&'static dyn Api> {
    provider(id)
        .apis()
        .iter()
        .copied()
        .find(|a| a.kind() == kind)
}
/// 게이트웨이 경로를 받는 Provider와 API.
pub fn route(path: &str) -> Option<(&'static dyn Provider, &'static dyn Api)> {
    providers().into_iter().find_map(|p| {
        p.apis()
            .iter()
            .find(|a| a.routes().contains(&path))
            .map(|a| (p, *a))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_provider_id_has_one_adapter_and_routes_are_unique() {
        for id in ProviderId::ALL {
            assert_eq!(NATIVE.iter().filter(|p| p.id() == id).count(), 1);
            assert!(!provider(id).apis().is_empty());
        }
        assert_eq!(NATIVE.len(), ProviderId::ALL.len());
        let mut seen = std::collections::BTreeSet::new();
        for p in NATIVE {
            let mut kinds = std::collections::BTreeSet::new();
            for a in p.apis() {
                assert!(kinds.insert(a.kind()));
                for r in a.routes() {
                    assert!(seen.insert(*r), "{r}");
                }
            }
        }
    }
}
