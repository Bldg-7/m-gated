//! OpenAI Embeddings API의 요청 검사와 응답 읽기. 생성/SSE와 별개인 유한 입력·벡터·usage 계약이다.
use m_gated_adapter_kit::{
    adapter::{ApiError, Profile, ReadError, Reading, RequestMetadata, Usage, UsageCategory},
    encoding,
    json::{self, Cooperate, JsonError, Kind, Visitor},
};
use std::collections::BTreeSet;
fn at(p: &[String], s: &[&str]) -> bool {
    p.iter().map(String::as_str).eq(s.iter().copied())
}
/// 10자리까지의 정수. 64비트로 읽고, `usize`를 넘으면 `usize::MAX`로 둔다. 그래야 32비트인 WASM
/// 빌드에서도 큰 값이 형식 오류가 아니라 본체 빌드와 같이 상한 초과로 거절된다.
fn int(s: &str) -> Result<usize, JsonError> {
    if s.len() > 10 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(JsonError);
    }
    let n: u64 = s.parse().map_err(|_| JsonError)?;
    Ok(usize::try_from(n).unwrap_or(usize::MAX))
}
/// 요청이 정한 응답 모양. 벡터 수, 차원, 인코딩(`float` 또는 `base64`)이다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shape {
    pub count: usize,
    pub dimensions: usize,
    pub encoding: String,
}
struct Input<'a> {
    profile: &'a Profile,
    shape: Shape,
    model: Option<String>,
}
impl Visitor for Input<'_> {
    fn value(&mut self, p: &[String], k: Kind, r: &str) -> Result<(), JsonError> {
        let want = if p.is_empty() {
            Kind::Object
        } else if at(p, &["model"]) {
            self.model = Some(json::string(r, 200)?);
            Kind::String
        } else if at(p, &["input"]) {
            if k == Kind::String {
                if r == "\"\"" {
                    return Err(JsonError);
                }
                self.shape.count = 1;
                return Ok(());
            }
            Kind::Array
        } else if at(p, &["input", "*"]) {
            if r == "\"\"" {
                return Err(JsonError);
            }
            Kind::String
        } else if at(p, &["dimensions"]) {
            self.shape.dimensions = int(r)?;
            Kind::Number
        } else if at(p, &["encoding_format"]) {
            self.shape.encoding = json::string(r, 16)?;
            Kind::String
        } else {
            return Err(JsonError);
        };
        if k == want { Ok(()) } else { Err(JsonError) }
    }
    fn object_end(
        &mut self,
        p: &[String],
        keys: &BTreeSet<String>,
        _: &str,
    ) -> Result<(), JsonError> {
        if p.is_empty() && keys.contains("model") && keys.contains("input") {
            Ok(())
        } else {
            Err(JsonError)
        }
    }
    fn array_end(&mut self, p: &[String], n: usize, _: &str) -> Result<(), JsonError> {
        if at(p, &["input"]) && (1..=256).contains(&n) {
            self.shape.count = n;
            Ok(())
        } else {
            Err(JsonError)
        }
    }
}
/// 요청 본문을 검사하고 응답 모양을 돌려준다. 긴 본문은 `cooperate`로 실행을 넘기며 읽는다.
pub async fn validate_shape_with<C: Cooperate>(
    body: &[u8],
    profile: &Profile,
    cooperate: C,
) -> Result<Shape, ApiError> {
    let dims = profile
        .embedding_dimensions
        .ok_or(ApiError::UnsupportedOption)?;
    let mut v = Input {
        profile,
        shape: Shape {
            count: 0,
            dimensions: dims.default as usize,
            encoding: "float".into(),
        },
        model: None,
    };
    json::scan_with(body, &mut v, cooperate)
        .await
        .map_err(|_| ApiError::InvalidRequest)?;
    if v.model.as_deref() != Some(&profile.model) {
        return Err(ApiError::UnsupportedModel);
    }
    // 응답 envelope 7개 + 각 vector object/index/object/array 4개 + float scalars.
    let response_tokens = v
        .shape
        .dimensions
        .checked_add(4)
        .and_then(|n| n.checked_mul(v.shape.count))
        .and_then(|n| n.checked_add(7));
    if v.shape.count == 0
        || v.shape.dimensions == 0
        || v.shape.dimensions > dims.max as usize
        || !v.profile.embedding_encodings.contains(&v.shape.encoding)
        || (v.shape.encoding == "float" && response_tokens.is_none_or(|n| n > 100_000))
    {
        return Err(ApiError::UnsupportedOption);
    }
    Ok(v.shape)
}
/// 요청 본문을 검사한다. 출력 상한은 없고 스트림도 없다.
pub async fn validate_with<C: Cooperate>(
    body: &[u8],
    profile: &Profile,
    cooperate: C,
) -> Result<RequestMetadata, ApiError> {
    validate_shape_with(body, profile, cooperate).await?;
    Ok(RequestMetadata {
        model: profile.model.clone(),
        output_limit: None,
        stream: false,
    })
}
struct Response<'a> {
    shape: &'a Shape,
    model: Option<String>,
    prompt: Option<u64>,
    total: Option<u64>,
    index: Option<usize>,
    indices: BTreeSet<usize>,
    vectors: usize,
}
impl Visitor for Response<'_> {
    fn array_limit(&self, p: &[String]) -> usize {
        if at(p, &["data", "*", "embedding"]) {
            4096
        } else {
            1024
        }
    }
    fn value(&mut self, p: &[String], k: Kind, r: &str) -> Result<(), JsonError> {
        let want = if p.is_empty() {
            Kind::Object
        } else if at(p, &["object"]) {
            return if json::string(r, 16)? == "list" {
                Ok(())
            } else {
                Err(JsonError)
            };
        } else if at(p, &["model"]) {
            self.model = Some(json::string(r, 200)?);
            Kind::String
        } else if at(p, &["data"]) {
            Kind::Array
        } else if at(p, &["data", "*"]) {
            self.index = None;
            Kind::Object
        } else if at(p, &["data", "*", "object"]) {
            return if json::string(r, 16)? == "embedding" {
                Ok(())
            } else {
                Err(JsonError)
            };
        } else if at(p, &["data", "*", "index"]) {
            self.index = Some(int(r)?);
            Kind::Number
        } else if at(p, &["data", "*", "embedding"]) {
            if self.shape.encoding == "base64" {
                let text = json::string(r, self.shape.dimensions * 8)?;
                let raw = encoding::base64_decode(&text).ok_or(JsonError)?;
                if raw.len() != self.shape.dimensions * 4
                    || raw
                        .chunks_exact(4)
                        .any(|b| !f32::from_le_bytes(b.try_into().unwrap()).is_finite())
                {
                    return Err(JsonError);
                }
                return if k == Kind::String {
                    Ok(())
                } else {
                    Err(JsonError)
                };
            }
            Kind::Array
        } else if at(p, &["data", "*", "embedding", "*"]) {
            return if k == Kind::Number
                && r.len() <= 64
                && r.parse::<f32>().is_ok_and(f32::is_finite)
            {
                Ok(())
            } else {
                Err(JsonError)
            };
        } else if at(p, &["usage"]) {
            Kind::Object
        } else if at(p, &["usage", "prompt_tokens"]) || at(p, &["usage", "total_tokens"]) {
            if k != Kind::Number || r.len() > 19 || !r.bytes().all(|b| b.is_ascii_digit()) {
                return Err(JsonError);
            }
            let n = r
                .parse::<u64>()
                .ok()
                .filter(|n| *n <= i64::MAX as u64)
                .ok_or(JsonError)?;
            if p[1] == "prompt_tokens" {
                self.prompt = Some(n)
            } else {
                self.total = Some(n)
            }
            Kind::Number
        } else {
            return Err(JsonError);
        };
        if k == want { Ok(()) } else { Err(JsonError) }
    }
    fn object_end(
        &mut self,
        p: &[String],
        keys: &BTreeSet<String>,
        _: &str,
    ) -> Result<(), JsonError> {
        let req = if p.is_empty() {
            &["object", "model", "data", "usage"][..]
        } else if at(p, &["data", "*"]) {
            let index = self.index.ok_or(JsonError)?;
            if index >= self.shape.count || index != self.vectors || !self.indices.insert(index) {
                return Err(JsonError);
            }
            self.vectors += 1;
            &["object", "index", "embedding"][..]
        } else if at(p, &["usage"]) {
            &["prompt_tokens", "total_tokens"][..]
        } else {
            return Err(JsonError);
        };
        if keys.len() == req.len() && req.iter().all(|k| keys.contains(*k)) {
            Ok(())
        } else {
            Err(JsonError)
        }
    }
    fn array_end(&mut self, p: &[String], n: usize, _: &str) -> Result<(), JsonError> {
        if (at(p, &["data"]) && n == self.shape.count)
            || (at(p, &["data", "*", "embedding"])
                && self.shape.encoding == "float"
                && n == self.shape.dimensions)
        {
            Ok(())
        } else {
            Err(JsonError)
        }
    }
}
/// 성공 응답 본문이 요청이 정한 모양인지 검사하고 사용량을 읽는다.
pub async fn response_usage_with<C: Cooperate>(
    body: &[u8],
    shape: &Shape,
    cooperate: C,
) -> Result<Option<Usage>, ReadError> {
    let mut v = Response {
        shape,
        model: None,
        prompt: None,
        total: None,
        index: None,
        indices: BTreeSet::new(),
        vectors: 0,
    };
    json::scan_with(body, &mut v, cooperate)
        .await
        .map_err(|_| ReadError::Protocol)?;
    let usage = match (v.model, v.prompt, v.total) {
        (Some(model), Some(input), Some(total))
            if input == total && v.vectors == shape.count && !model.is_empty() =>
        {
            Some(Usage {
                actual_model: model,
                tokens: vec![(UsageCategory::Input, input)],
                reasoning_tokens: None,
            })
        }
        _ => None,
    };
    Ok(usage)
}
/// 비스트림 응답 본문을 읽는다. 성공 응답은 요청이 정한 벡터 수와 차원으로 검사한다.
/// 오류 응답은 Responses와 같이 읽는다.
pub async fn read_with<C: Cooperate>(
    status: u16,
    body: &[u8],
    request: &[u8],
    profile: &Profile,
    cooperate: &mut C,
) -> Result<Reading, ReadError> {
    if status != 200 {
        return crate::responses::read_with(status, body, cooperate).await;
    }
    let shape = validate_shape_with(request, profile, &mut *cooperate)
        .await
        .map_err(|_| ReadError::Protocol)?;
    Ok(Reading {
        usage: response_usage_with(body, &shape, cooperate).await?,
        failed: false,
    })
}
