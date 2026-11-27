//! 어댑터가 받고 돌려주는 값. 본체의 HTTP·설정·장부 타입에 기대지 않는다.
//! 본체는 이 값을 자기 타입으로 바꿔 쓴다.

/// 요청 검사 실패.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiError {
    InvalidRequest,
    UnsupportedOption,
    UnsupportedModel,
}
/// 요청 검사가 알려 주는 값.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestMetadata {
    pub model: String,
    pub output_limit: Option<u64>,
    pub stream: bool,
}
/// 게이트웨이가 직접 만든 오류 응답의 형식.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorFormat {
    /// `{"type":"error","error":{...}}`
    Anthropic,
    /// `{"error":{"message":...,"type":...,"code":...}}`
    Openai,
    /// `{"detail":"..."}`
    Detail,
}

/// 응답을 읽지 못한 까닭.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// 응답 형식이 계약과 다르다.
    Protocol,
    /// 스트림 event 하나가 상한을 넘었다.
    StreamLimit,
    /// 응답 본문이 상한을 넘었다.
    TooLarge,
}
/// 게이트웨이의 사용량 칸(결정 0012). 가격 칸과 같은 목록이다. 새 칸은 게이트웨이를 고쳐야 생긴다.
/// 추론 토큰은 칸이 아니다. `output`에 들어 있고 따로 값을 매기지 않는다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UsageCategory {
    /// 캐시가 아닌 입력.
    Input,
    Output,
    CacheRead,
    /// 캐시가 사는 시간과 관계없이 값이 같은 캐시 쓰기.
    CacheWrite,
    /// 5분 동안 사는 캐시 쓰기.
    CacheWrite5m,
    /// 1시간 동안 사는 캐시 쓰기.
    CacheWrite1h,
}
impl UsageCategory {
    pub const ALL: [Self; 6] = [
        Self::Input,
        Self::Output,
        Self::CacheRead,
        Self::CacheWrite,
        Self::CacheWrite5m,
        Self::CacheWrite1h,
    ];
    /// 칸 이름. 가격 항목 이름과 같다.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
            Self::CacheRead => "cache_read",
            Self::CacheWrite => "cache_write",
            Self::CacheWrite5m => "cache_write_5m",
            Self::CacheWrite1h => "cache_write_1h",
        }
    }
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }
    /// 캐시 칸인지. 분당 토큰 한도의 입력 토큰은 `input`과 모든 캐시 칸의 합이다.
    pub const fn is_cache(self) -> bool {
        !matches!(self, Self::Input | Self::Output)
    }
}
/// Provider가 알린 사용량(결정 0012).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Usage {
    pub actual_model: String,
    /// 칸과 토큰 수. 한 칸은 한 번만 나온다. API가 알린 칸만 쓴다. 없는 칸은 0이다.
    pub tokens: Vec<(UsageCategory, u64)>,
    pub reasoning_tokens: Option<u64>,
}
impl Usage {
    /// 한 칸의 토큰 수. 없으면 0이다.
    pub fn get(&self, category: UsageCategory) -> u64 {
        self.tokens
            .iter()
            .find(|(c, _)| *c == category)
            .map_or(0, |(_, n)| *n)
    }
}
/// 비스트림 응답을 읽은 결과.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reading {
    /// 정산에 쓸 사용량. 읽지 못했으면 None이다.
    pub usage: Option<Usage>,
    /// Provider가 실패를 알렸는지.
    pub failed: bool,
}
/// 스트림 event 하나를 읽은 결과.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Step {
    /// 이 event가 스트림의 끝을 알린다.
    pub terminal: bool,
    /// 이 event가 Provider의 오류 event다.
    pub error: bool,
    /// 끊긴 요청의 `received` 정산에 쓰는 출력 내용 조각. None은 조각이 상한을 넘어 받지 못한 것이다.
    pub content: Vec<Option<String>>,
}

/// 설정 파일의 model profile이 켠 Provider 기능.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    ClientTools,
    Thinking,
    Cache,
    JsonSchema,
    InlinePng,
}
/// 어댑터가 보는 model profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub model: String,
    pub features: Vec<Feature>,
    pub approved_beta: Vec<String>,
    pub max_output_tokens: Option<u64>,
    /// 가격 항목 이름(`input`, `output`, `cache_read` 등).
    pub rates: Vec<String>,
}
impl Profile {
    pub fn has(&self, feature: Feature) -> bool {
        self.features.contains(&feature)
    }
    pub fn has_rate(&self, name: &str) -> bool {
        self.rates.iter().any(|r| r == name)
    }
}
/// profile 규칙 위반.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileError {
    InvalidPrice,
    InvalidProfile,
}

/// 클라이언트 요청 헤더 하나. 이름은 소문자다.
#[derive(Clone, Copy, Debug)]
pub struct Header<'a> {
    pub name: &'a str,
    pub value: &'a [u8],
}
