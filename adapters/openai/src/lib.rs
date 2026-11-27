//! OpenAI 어댑터(결정 0013). Responses, Chat Completions, Embeddings 세 API의 요청 검사,
//! 비스트림 응답 읽기, 스트림 읽기를 맡는다.
//!
//! 본체의 HTTP·설정·장부 타입에 기대지 않는다. 본체는 `src/providers/openai.rs`,
//! `openai_chat.rs`, `embeddings.rs`에서 이 crate를 port에 연결한다. WASM 컴포넌트
//! (`adapters/openai/component`)도 이 crate를 쓴다.
pub mod chat;
pub mod embeddings;
pub mod responses;

use m_gated_adapter_kit::{
    adapter::{
        ApiError, ErrorFormat, Header, Profile, ProfileError, ReadError, Reading, RequestMetadata,
        Step, Usage, UsageCategory,
    },
    json::Cooperate,
    openai,
    stream::{Event, SseDecoder},
};

/// Provider 이름. 본체의 `ProviderId`와 같다.
pub const NAME: &str = "openai";
/// 업스트림 주소.
pub const ORIGIN: &str = "https://api.openai.com";
/// 키 확인 경로.
pub const VERIFY_PATH: &str = "v1/models";
/// 키는 이 접두사로 시작해야 한다.
pub const KEY_PREFIX: &str = openai::SK_KEY_PREFIX;
/// 키가 이 접두사로 시작하면 받지 않는다.
pub const REFUSED_KEY_PREFIXES: &[&str] = openai::SK_REFUSED_KEY_PREFIXES;
/// 응답에서 Provider 요청 id를 읽을 헤더. 앞의 것이 먼저다.
pub const REQUEST_ID_HEADERS: &[&str] = &["x-request-id"];
/// 클라이언트에 그대로 넘기는 요청 id 응답 헤더.
pub const RESPONSE_ID_HEADERS: &[&str] = &["request-id", "x-request-id"];
/// 공개 가격 목록(models.dev)에서 OpenAI 모델을 찾을 이름. 카탈로그 동기화가 쓴다.
pub const MODELS_DEV_NAME: Option<&str> = Some("openai");
/// 세 API 모두 OpenAI 오류 형식을 쓴다.
pub const ERROR_FORMAT: ErrorFormat = ErrorFormat::Openai;

/// `sk-` 키만 받는다. Anthropic 키(`sk-ant-`)는 받지 않는다.
pub fn accepts_key(secret: &str) -> bool {
    openai::accepts_sk_key(secret)
}
/// OpenAI 호환 Provider의 profile 규칙.
pub fn check_profile(profile: &Profile) -> Result<(), ProfileError> {
    openai::check_compatible_profile(profile)
}
/// 업스트림에 보낼 클라이언트 헤더의 이름. JSON `content-type`만 보낸다.
pub fn approved_headers(headers: &[Header]) -> Result<Vec<&'static str>, ApiError> {
    openai::approved_headers(headers)
}

/// OpenAI의 API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    Responses,
    Chat,
    Embeddings,
}
impl Api {
    pub const ALL: [Self; 3] = [Self::Responses, Self::Chat, Self::Embeddings];
    /// 본체의 API 이름. 설정 파일의 `api`와 같다.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::Chat => "openai_chat",
            Self::Embeddings => "embeddings",
        }
    }
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.name() == name)
    }
    /// 게이트웨이가 받는 경로.
    pub const fn routes(self) -> &'static [&'static str] {
        match self {
            Self::Responses => &["/openai/v1/responses"],
            Self::Chat => &["/openai/v1/chat/completions"],
            Self::Embeddings => &["/openai/v1/embeddings"],
        }
    }
    /// 업스트림 경로(`base_url` 기준 상대 경로).
    pub const fn upstream_path(self) -> &'static str {
        match self {
            Self::Responses => "v1/responses",
            Self::Chat => "v1/chat/completions",
            Self::Embeddings => "v1/embeddings",
        }
    }
    /// 가격표 snapshot에 남기는 토큰 포함 규칙.
    pub const fn token_rule(self) -> &'static str {
        match self {
            Self::Responses => "openai_responses_input_includes_cache_output_includes_reasoning_v1",
            Self::Chat => "openai_chat_prompt_includes_cache_completion_includes_reasoning_v1",
            Self::Embeddings => "openai_embeddings_prompt_v1",
        }
    }
    /// 사용량 칸(결정 0012). `input`은 캐시가 아닌 입력이고, `cache_read`는 캐시에서 읽은 입력이다.
    pub const fn usage_categories(self) -> &'static [UsageCategory] {
        match self {
            Self::Responses | Self::Chat => &[
                UsageCategory::Input,
                UsageCategory::Output,
                UsageCategory::CacheRead,
            ],
            Self::Embeddings => &[UsageCategory::Input],
        }
    }
    /// 카탈로그 정책(모델과 가격을 DB에서 관리)을 쓸 수 있는지.
    pub const fn catalog(self) -> bool {
        matches!(self, Self::Responses | Self::Chat)
    }
    /// 스트림 응답을 읽을 수 있는지. Embeddings는 스트림이 없다.
    pub const fn stream(self) -> bool {
        matches!(self, Self::Responses | Self::Chat)
    }
}

/// 요청 본문을 검사한다. 긴 본문은 `cooperate`로 실행을 넘기며 읽는다.
pub async fn validate_with<C: Cooperate>(
    api: Api,
    body: &[u8],
    profile: &Profile,
    cooperate: C,
) -> Result<RequestMetadata, ApiError> {
    match api {
        Api::Responses => responses::validate_with(body, profile, cooperate).await,
        Api::Chat => chat::validate_with(body, profile, cooperate).await,
        Api::Embeddings => embeddings::validate_with(body, profile, cooperate).await,
    }
}
/// 비스트림 응답 본문을 읽는다. 본체가 상태 코드, 인코딩, content-type, 크기를 먼저 확인한다.
/// `request`는 클라이언트가 보낸 요청 본문이다. Embeddings만 쓴다.
pub async fn read_with<C: Cooperate>(
    api: Api,
    status: u16,
    body: &[u8],
    request: &[u8],
    profile: &Profile,
    cooperate: &mut C,
) -> Result<Reading, ReadError> {
    match api {
        Api::Responses => responses::read_with(status, body, cooperate).await,
        Api::Chat => chat::read_with(status, body, profile, cooperate).await,
        Api::Embeddings => embeddings::read_with(status, body, request, profile, cooperate).await,
    }
}

/// 스트림 event에서 읽을 필드의 규칙. WASM 컴포넌트는 이 규칙을 본체에 알리고, 본체가 같은
/// 디코더를 만든다.
#[derive(Clone, Copy, Debug)]
pub struct DecoderRule {
    /// `data: [DONE]`을 끝 event로 받는지(Chat Completions).
    pub chat: bool,
    /// event에서 읽을 필드 경로.
    pub fields: &'static [&'static str],
    /// 내용 없이 문자열인지만 알릴 필드 경로.
    pub string_shapes: &'static [&'static str],
}
impl DecoderRule {
    pub fn decoder(&self) -> SseDecoder {
        if self.chat {
            SseDecoder::chat_with_string_shapes(self.fields, self.string_shapes)
        } else {
            SseDecoder::with_string_shapes(self.fields, self.string_shapes)
        }
    }
}
/// 스트림 응답 하나를 읽는 상태 기계.
pub enum Stream {
    Responses(responses::Stream),
    Chat(chat::Reader),
}
impl Stream {
    /// 스트림이 없는 API(Embeddings)는 None이다.
    pub fn new(api: Api, profile: &Profile) -> Option<Self> {
        match api {
            Api::Responses => Some(Self::Responses(responses::Stream::default())),
            Api::Chat => Some(Self::Chat(chat::Reader::new(profile))),
            Api::Embeddings => None,
        }
    }
    /// 스트림 event에서 읽을 필드의 규칙.
    pub fn decoder_rule(&self) -> DecoderRule {
        match self {
            Self::Responses(_) => DecoderRule {
                chat: false,
                fields: responses::STREAM_FIELDS,
                string_shapes: &[],
            },
            Self::Chat(_) => DecoderRule {
                chat: true,
                fields: chat::FIELDS,
                string_shapes: chat::STRING_SHAPES,
            },
        }
    }
    /// 스트림 event에서 읽을 필드.
    pub fn decoder(&self) -> SseDecoder {
        self.decoder_rule().decoder()
    }
    /// event 하나를 받는다. 형식이 맞지 않으면 오류를 돌려주고, 그 뒤로는 사용량을 알려 주지 않는다.
    pub fn accept(&mut self, event: Event) -> Result<Step, ReadError> {
        match self {
            Self::Responses(s) => s.accept(event),
            Self::Chat(s) => s.accept(event),
        }
    }
    /// 끝 event를 받은 뒤 정산에 쓸 사용량. 오류 event 뒤에는 None이다.
    pub fn final_usage(&self) -> Option<Usage> {
        match self {
            Self::Responses(s) => s.final_usage(),
            Self::Chat(s) => s.final_usage(),
        }
    }
    /// Provider가 실패를 알렸는지.
    pub fn provider_error(&self) -> bool {
        match self {
            Self::Responses(s) => s.provider_error(),
            Self::Chat(s) => s.provider_error(),
        }
    }
    /// 스트림이 끝났을 때의 사용량과 실패 여부. 끝 event 없이 끝났으면 오류다.
    pub fn close(&self) -> Result<(Option<Usage>, bool), ReadError> {
        match self {
            Self::Responses(s) => s.close(),
            Self::Chat(s) => s.close(),
        }
    }
}
