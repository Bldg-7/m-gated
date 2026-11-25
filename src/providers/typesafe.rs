//! TypeSafe 어댑터. System One API(`POST /v1/systemone`) 하나만 있고 스트림이 없다.
//! 입력 토큰만 과금하고 출력 토큰은 무료다. 이미지(assets, box 질문)와 긴 문맥 헤더는 받지 않는다.
use super::{
    port::{self, Api, ApiError, BoxFuture, ErrorFormat, Reading, RequestMetadata},
    transport::UpstreamError,
};
use crate::{
    config::{ApiKind, ModelProfile, ProviderId, StartupError},
    json::{self, JsonError, Kind, Visitor},
    ledger::finance::FinalUsage,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use std::collections::{BTreeMap, BTreeSet};

/// TypeSafe 어댑터.
pub struct Typesafe;
pub static PROVIDER: Typesafe = Typesafe;
static APIS: [&dyn Api; 1] = [&SYSTEMONE];
impl port::Provider for Typesafe {
    fn id(&self) -> ProviderId {
        ProviderId::Typesafe
    }
    fn origin(&self) -> &'static str {
        "https://api.typesafe.ai"
    }
    fn apis(&self) -> &'static [&'static dyn Api] {
        &APIS
    }
    fn auth_header(&self, secret: &str) -> Option<(HeaderName, HeaderValue)> {
        super::openai::bearer(secret)
    }
    /// 키 형식이 공개되지 않았다. 잘못 붙여 넣은 키는 등록 전 확인 요청(`GET /v1/models`)이 거른다.
    fn accepts_key(&self, _secret: &str) -> bool {
        true
    }
    fn verify_path(&self) -> &'static str {
        "v1/models"
    }
    fn request_id_headers(&self) -> &'static [&'static str] {
        &["x-typesafe-request-id"]
    }
    fn response_headers(&self) -> &'static [(&'static str, port::HeaderRule)] {
        &[
            ("x-typesafe-request-id", port::HeaderRule::Id),
            ("retry-after-ms", port::HeaderRule::Number),
        ]
    }
    /// 가격은 입력과 출력 두 항목이다. 캐시와 다른 기능은 없다.
    /// 요청에 출력 상한이 없어 입장 때의 비용 상한에 출력을 넣을 수 없으므로, 출력 가격은 0이어야 한다.
    /// TypeSafe도 출력 토큰을 받지 않는다.
    fn check_profile(&self, profile: &ModelProfile) -> Result<(), StartupError> {
        if !profile.features.is_empty() || !profile.approved_beta.is_empty() {
            return Err(StartupError::InvalidProfile);
        }
        if profile.rates.len() != 2
            || !profile.rates.contains_key("input")
            || profile.rates.get("output").map(String::as_str) != Some("0.000000000000")
        {
            return Err(StartupError::InvalidPrice);
        }
        Ok(())
    }
}

/// System One API.
pub struct Systemone;
static SYSTEMONE: Systemone = Systemone;
impl Api for Systemone {
    fn kind(&self) -> ApiKind {
        ApiKind::Systemone
    }
    fn routes(&self) -> &'static [&'static str] {
        &["/typesafe/v1/systemone"]
    }
    fn upstream_path(&self) -> &'static str {
        "v1/systemone"
    }
    fn error_format(&self) -> ErrorFormat {
        ErrorFormat::Detail
    }
    /// TypeSafe SDK는 1 KiB 이상인 본문을 기본으로 zstd 압축해서 보낸다.
    fn accepts_zstd(&self) -> bool {
        true
    }
    fn validate<'a>(
        &'a self,
        body: &'a [u8],
        profile: &'a ModelProfile,
    ) -> BoxFuture<'a, Result<RequestMetadata, ApiError>> {
        Box::pin(validate(body, profile))
    }
    /// 이미지와 긴 문맥을 켜는 헤더는 받지 않는다. 업스트림에는 `content-type`만 보낸다.
    fn approved_headers(
        &self,
        incoming: &HeaderMap,
        _: &ModelProfile,
    ) -> Result<HeaderMap, ApiError> {
        if DANGER_ZONE_HEADERS
            .iter()
            .any(|name| incoming.contains_key(*name))
        {
            return Err(ApiError::UnsupportedOption);
        }
        super::openai::approved_headers(incoming)
    }
    fn read<'a>(
        &'a self,
        status: StatusCode,
        body: &'a [u8],
        _request: &'a [u8],
        _profile: &'a ModelProfile,
    ) -> BoxFuture<'a, Result<Reading, UpstreamError>> {
        Box::pin(async move {
            let usage = if status == StatusCode::OK {
                read_success(body).await?
            } else {
                read_error(body).await?;
                None
            };
            Ok(Reading {
                usage,
                counted_input_tokens: None,
                failed: false,
            })
        })
    }
    fn token_rule(&self, _profile: &ModelProfile) -> &'static str {
        "typesafe_input_output_v1"
    }
}
const DANGER_ZONE_HEADERS: [&str; 2] = [
    "typesafe-danger-zone-images",
    "typesafe-danger-zone-long-context",
];

fn at(p: &[String], parts: &[&str]) -> bool {
    p.len() == parts.len() && p.iter().zip(parts).all(|(a, b)| b == &"*" || a == b)
}
fn integer(raw: &str) -> Result<u64, JsonError> {
    if raw.is_empty() || raw.len() > 20 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(JsonError);
    }
    raw.parse().map_err(|_| JsonError)
}

/// 질문 하나에서 모은 값. 필드 순서와 무관하게 질문 object가 끝날 때 검사한다.
#[derive(Default)]
struct Question {
    kind: Option<String>,
    criteria: Option<Kind>,
    /// criteria가 object일 때의 키.
    criteria_keys: BTreeSet<String>,
    /// criteria가 array일 때의 항목 수.
    criteria_items: usize,
    /// criteria의 값 가운데 허용하지 않는 종류가 있다(score 수준은 문자열·object·array,
    /// noul과 choice의 설명은 그것에 null까지).
    criteria_bad_value: bool,
}
struct Request<'a> {
    profile: &'a ModelProfile,
    model: Option<String>,
    state: bool,
    questions: BTreeMap<String, Question>,
    question_count: Option<usize>,
    unsupported: bool,
}
impl Request<'_> {
    fn unsupported(&mut self) -> Result<(), JsonError> {
        self.unsupported = true;
        Err(JsonError)
    }
    /// 사용자가 쓰는 내용(state, instructions, criteria의 값)의 아래쪽. 문법만 검사한다.
    fn content(p: &[String]) -> bool {
        (p.len() > 1 && p[0] == "state")
            || (p.len() > 3 && p[0] == "questions" && p[2] == "instructions")
            || (p.len() > 4 && p[0] == "questions" && p[2] == "criteria")
    }
}
impl Visitor for Request<'_> {
    fn array_limit(&self, p: &[String]) -> usize {
        if at(p, &["questions", "*", "criteria"]) {
            10
        } else if !p.is_empty() && (p[0] == "state" || p[0] == "questions") {
            // 사용자 내용의 큰 목록. 본문 전체의 값 개수 상한(10만)은 검사기가 따로 지킨다.
            100_000
        } else {
            1024
        }
    }
    fn value(&mut self, p: &[String], k: Kind, r: &str) -> Result<(), JsonError> {
        if p.is_empty() {
            return if k == Kind::Object {
                Ok(())
            } else {
                Err(JsonError)
            };
        }
        if Self::content(p) {
            return Ok(());
        }
        let content = matches!(k, Kind::String | Kind::Object | Kind::Array);
        match p.len() {
            1 => match p[0].as_str() {
                "model" if k == Kind::String => {
                    self.model = Some(json::string(r, 200)?);
                    Ok(())
                }
                "state" if content => {
                    self.state = true;
                    Ok(())
                }
                "questions" if k == Kind::Object => Ok(()),
                // 이미지는 받지 않는다.
                "assets" => self.unsupported(),
                "model" | "state" | "questions" => Err(JsonError),
                _ => self.unsupported(),
            },
            2 if p[0] == "questions" => {
                if k != Kind::Object {
                    return Err(JsonError);
                }
                self.questions.insert(p[1].clone(), Question::default());
                Ok(())
            }
            3 if p[0] == "questions" => {
                let question = self.questions.get_mut(&p[1]).ok_or(JsonError)?;
                match p[2].as_str() {
                    "type" if k == Kind::String => {
                        let kind = json::string(r, 16)?;
                        match kind.as_str() {
                            "noul" | "choice" | "score" => {
                                question.kind = Some(kind);
                                Ok(())
                            }
                            // box 질문은 이미지가 있어야 한다.
                            "box" => self.unsupported(),
                            _ => Err(JsonError),
                        }
                    }
                    "instructions" if content || k == Kind::Null => Ok(()),
                    "criteria" if matches!(k, Kind::Object | Kind::Array | Kind::Null) => {
                        question.criteria = Some(k);
                        Ok(())
                    }
                    "type" | "instructions" | "criteria" => Err(JsonError),
                    _ => self.unsupported(),
                }
            }
            4 if p[0] == "questions" && p[2] == "criteria" => {
                let question = self.questions.get_mut(&p[1]).ok_or(JsonError)?;
                let allowed =
                    content || (k == Kind::Null && question.criteria == Some(Kind::Object));
                question.criteria_bad_value |= !allowed;
                Ok(())
            }
            _ => Err(JsonError),
        }
    }
    fn object_end(
        &mut self,
        p: &[String],
        keys: &BTreeSet<String>,
        _: &str,
    ) -> Result<(), JsonError> {
        if at(p, &["questions"]) {
            self.question_count = Some(keys.len());
        } else if at(p, &["questions", "*", "criteria"]) {
            self.questions
                .get_mut(&p[1])
                .ok_or(JsonError)?
                .criteria_keys = keys.clone();
        } else if at(p, &["questions", "*"]) {
            let q = self.questions.get(&p[1]).ok_or(JsonError)?;
            let valid = !q.criteria_bad_value
                && match (q.kind.as_deref(), q.criteria) {
                    (Some("noul"), None | Some(Kind::Null)) => true,
                    (Some("noul"), Some(Kind::Object)) => {
                        q.criteria_keys.iter().all(|k| k == "true" || k == "false")
                    }
                    (Some("choice"), Some(Kind::Object)) => {
                        (1..=255).contains(&q.criteria_keys.len())
                    }
                    (Some("score"), Some(Kind::Array)) => (1..=10).contains(&q.criteria_items),
                    _ => false,
                };
            if !valid {
                return Err(JsonError);
            }
        }
        Ok(())
    }
    fn array_end(&mut self, p: &[String], items: usize, _: &str) -> Result<(), JsonError> {
        if at(p, &["questions", "*", "criteria"]) {
            self.questions
                .get_mut(&p[1])
                .ok_or(JsonError)?
                .criteria_items = items;
        }
        Ok(())
    }
}
/// 요청 본문을 검사한다. `model`, `state`, `questions`만 받고, 질문은 noul, choice, score만 받는다.
pub async fn validate(body: &[u8], profile: &ModelProfile) -> Result<RequestMetadata, ApiError> {
    if profile.api != ApiKind::Systemone {
        return Err(ApiError::UnsupportedOption);
    }
    let mut v = Request {
        profile,
        model: None,
        state: false,
        questions: BTreeMap::new(),
        question_count: None,
        unsupported: false,
    };
    if json::scan(body, &mut v).await.is_err() {
        return Err(if v.unsupported {
            ApiError::UnsupportedOption
        } else {
            ApiError::InvalidRequest
        });
    }
    if !v.state || v.question_count.unwrap_or(0) == 0 {
        return Err(ApiError::InvalidRequest);
    }
    let model = v.model.ok_or(ApiError::InvalidRequest)?;
    if model != v.profile.model {
        return Err(ApiError::UnsupportedModel);
    }
    Ok(RequestMetadata {
        model,
        output_limit: None,
        stream: false,
    })
}

/// 성공 응답에서 모은 값.
#[derive(Default)]
struct Response {
    model: Option<String>,
    answers: bool,
    usage: bool,
    input: Option<u64>,
    output: Option<u64>,
    /// usage에 모르는 필드가 있거나 값이 정수가 아니다.
    invalid_usage: bool,
}
impl Visitor for Response {
    fn array_limit(&self, _: &[String]) -> usize {
        100_000
    }
    fn value(&mut self, p: &[String], k: Kind, r: &str) -> Result<(), JsonError> {
        if p.is_empty() {
            return if k == Kind::Object {
                Ok(())
            } else {
                Err(JsonError)
            };
        }
        if p.len() == 1 {
            match p[0].as_str() {
                "model" if k == Kind::String => self.model = Some(json::string(r, 200)?),
                "answers" if k == Kind::Object => self.answers = true,
                "usage" if k == Kind::Object => self.usage = true,
                "model" | "answers" | "usage" => return Err(JsonError),
                _ => {}
            }
        } else if p.len() == 2 && p[0] == "usage" {
            let n = if k == Kind::Number {
                integer(r).ok()
            } else {
                None
            };
            match (p[1].as_str(), n) {
                ("input_tokens", Some(n)) => self.input = Some(n),
                ("output_tokens", Some(n)) => self.output = Some(n),
                _ => self.invalid_usage = true,
            }
        } else if p.len() > 2 && p[0] == "usage" {
            self.invalid_usage = true;
        }
        Ok(())
    }
}
/// 성공 응답을 읽는다. `model`, `answers`, `usage`가 없으면 형식 오류다.
/// usage에 모르는 필드가 있으면 사용량을 알리지 않아 요청이 `unknown`이 된다.
async fn read_success(body: &[u8]) -> Result<Option<FinalUsage>, UpstreamError> {
    let mut v = Response::default();
    json::scan(body, &mut v)
        .await
        .map_err(|_| UpstreamError::Protocol)?;
    let model = v
        .model
        .filter(|m| !m.is_empty())
        .ok_or(UpstreamError::Protocol)?;
    if !v.answers || !v.usage {
        return Err(UpstreamError::Protocol);
    }
    if v.invalid_usage {
        return Ok(None);
    }
    Ok(match (v.input, v.output) {
        (Some(input_tokens), Some(output_tokens)) => Some(FinalUsage {
            actual_model: model,
            input_tokens,
            output_tokens,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: None,
        }),
        _ => None,
    })
}
/// 오류 응답은 JSON object여야 한다. 400과 422의 `detail` 말고는 모양이 공개되지 않았다.
async fn read_error(body: &[u8]) -> Result<(), UpstreamError> {
    struct Object;
    impl Visitor for Object {
        fn array_limit(&self, _: &[String]) -> usize {
            100_000
        }
        fn value(&mut self, p: &[String], k: Kind, _: &str) -> Result<(), JsonError> {
            if p.is_empty() && k != Kind::Object {
                Err(JsonError)
            } else {
                Ok(())
            }
        }
    }
    json::scan(body, &mut Object)
        .await
        .map_err(|_| UpstreamError::Protocol)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn profile() -> ModelProfile {
        serde_json::from_value(serde_json::json!({"model":"jev-latest","api":"systemone","profile_version":"typesafe-v1","pricing_version":"typesafe-price-v1","fixture_version":"typesafe-native-v1","actual_models":["jev-latest","jev-1.14.0"],"max_input_tokens":65536,"max_body_bytes":8388608,"max_response_bytes":1048576,"rates":{"input":"0.042000000000","output":"0.000000000000"}})).unwrap()
    }
    async fn check(body: serde_json::Value) -> Result<RequestMetadata, ApiError> {
        validate(body.to_string().as_bytes(), &profile()).await
    }
    #[tokio::test]
    async fn the_three_documented_question_types_are_accepted() {
        let body = serde_json::json!({
            "model": "jev-latest",
            "state": {"message": "I was charged twice.", "thread": [{"from": "customer", "text": "help"}]},
            "questions": {
                "billing": {"type": "noul", "instructions": "Is this message about billing?",
                    "criteria": {"true": "A charge or refund", "false": null}},
                "plain": {"type": "noul"},
                "tone": {"criteria": {"angry": "Upset", "calm": null, "excited": {"note": "eager"}},
                    "type": "choice", "instructions": null},
                "urgency": {"type": "score", "criteria": ["Can wait", {"level": "soon"}, ["today"]]}
            }
        });
        let meta = check(body).await.unwrap();
        assert_eq!(meta.model, "jev-latest");
        assert!(!meta.stream);
        assert_eq!(meta.output_limit, None);
        for state in [serde_json::json!("text"), serde_json::json!([1, 2, 3])] {
            let body = serde_json::json!({"model":"jev-latest","state":state,"questions":{"q":{"type":"noul"}}});
            assert!(check(body).await.is_ok());
        }
    }
    #[tokio::test]
    async fn images_and_unknown_fields_are_unsupported() {
        for body in [
            serde_json::json!({"model":"jev-latest","state":"photo","questions":{"q":{"type":"box"}}}),
            serde_json::json!({"model":"jev-latest","state":"photo","questions":{"q":{"type":"noul"}},
                "assets":{"photo":{"type":"image","format":"png","data_base64":"AA=="}}}),
            serde_json::json!({"model":"jev-latest","state":"s","questions":{"q":{"type":"noul"}},"stream":true}),
            serde_json::json!({"model":"jev-latest","state":"s","questions":{"q":{"type":"noul","weight":1}}}),
        ] {
            assert_eq!(check(body).await.err(), Some(ApiError::UnsupportedOption));
        }
    }
    #[tokio::test]
    async fn malformed_questions_are_invalid() {
        for body in [
            serde_json::json!({"model":"jev-latest","state":"s","questions":{}}),
            serde_json::json!({"model":"jev-latest","questions":{"q":{"type":"noul"}}}),
            serde_json::json!({"model":"jev-latest","state":1,"questions":{"q":{"type":"noul"}}}),
            serde_json::json!({"model":"jev-latest","state":null,"questions":{"q":{"type":"noul"}}}),
            serde_json::json!({"model":"jev-latest","state":"s","questions":{"q":{}}}),
            serde_json::json!({"model":"jev-latest","state":"s","questions":{"q":{"type":"other"}}}),
            serde_json::json!({"model":"jev-latest","state":"s","questions":{"q":{"type":"choice"}}}),
            serde_json::json!({"model":"jev-latest","state":"s","questions":{"q":{"type":"choice","criteria":{}}}}),
            serde_json::json!({"model":"jev-latest","state":"s","questions":{"q":{"type":"choice","criteria":["a"]}}}),
            serde_json::json!({"model":"jev-latest","state":"s","questions":{"q":{"type":"score","criteria":[]}}}),
            serde_json::json!({"model":"jev-latest","state":"s","questions":{"q":{"type":"score","criteria":["a",null]}}}),
            serde_json::json!({"model":"jev-latest","state":"s","questions":{"q":{"type":"score","criteria":(0..11).map(|n| n.to_string()).collect::<Vec<_>>()}}}),
            serde_json::json!({"model":"jev-latest","state":"s","questions":{"q":{"type":"noul","criteria":{"yes":"a"}}}}),
            serde_json::json!({"model":"jev-latest","state":"s","questions":{"q":{"type":"noul","instructions":1}}}),
            serde_json::json!({"model":"jev-latest","state":"s","questions":{"q":"noul"}}),
            serde_json::json!({"model":"jev-latest","state":"s","questions":{"q":{"type":"choice","criteria":{"a":1}}}}),
            serde_json::json!({"model":"jev-latest","state":"s","questions":{"q":{"type":"noul","criteria":{"true":false}}}}),
        ] {
            assert_eq!(
                check(body.clone()).await.err(),
                Some(ApiError::InvalidRequest),
                "{body}"
            );
        }
        let choices: serde_json::Map<String, serde_json::Value> = (0..256)
            .map(|n| (format!("c{n}"), serde_json::Value::Null))
            .collect();
        let body = serde_json::json!({"model":"jev-latest","state":"s","questions":{"q":{"type":"choice","criteria":choices}}});
        assert_eq!(check(body).await.err(), Some(ApiError::InvalidRequest));
        let body =
            serde_json::json!({"model":"jev-other","state":"s","questions":{"q":{"type":"noul"}}});
        assert_eq!(check(body).await.err(), Some(ApiError::UnsupportedModel));
    }
    #[test]
    fn profile_prices_are_input_and_a_zero_output() {
        use port::Provider;
        let mut p = profile();
        assert!(PROVIDER.check_profile(&p).is_ok());
        p.rates.insert("output".into(), "0.000000000001".into());
        assert_eq!(
            PROVIDER.check_profile(&p).err(),
            Some(StartupError::InvalidPrice)
        );
        p.rates.insert("output".into(), "0.000000000000".into());
        p.rates.insert("cache_read".into(), "0.000000000000".into());
        assert_eq!(
            PROVIDER.check_profile(&p).err(),
            Some(StartupError::InvalidPrice)
        );
        let mut p = profile();
        p.features.push(crate::config::NativeFeature::InlinePng);
        assert_eq!(
            PROVIDER.check_profile(&p).err(),
            Some(StartupError::InvalidProfile)
        );
    }
    #[test]
    fn danger_zone_headers_are_unsupported() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        assert!(SYSTEMONE.approved_headers(&headers, &profile()).is_ok());
        for name in DANGER_ZONE_HEADERS {
            let mut h = headers.clone();
            h.insert(name, HeaderValue::from_static("true"));
            assert_eq!(
                SYSTEMONE.approved_headers(&h, &profile()).err(),
                Some(ApiError::UnsupportedOption)
            );
        }
    }
    async fn read(status: u16, body: &str) -> Result<Reading, UpstreamError> {
        SYSTEMONE
            .read(
                StatusCode::from_u16(status).unwrap(),
                body.as_bytes(),
                b"",
                &profile(),
            )
            .await
    }
    #[tokio::test]
    async fn success_reports_input_and_free_output_tokens() {
        let r = read(200, r#"{"model":"jev-1.14.0","answers":{"q":{"type":"noul","noul":0.98}},"usage":{"input_tokens":120,"output_tokens":12}}"#).await.unwrap();
        let u = r.usage.unwrap();
        assert_eq!(
            (u.actual_model.as_str(), u.input_tokens, u.output_tokens),
            ("jev-1.14.0", 120, 12)
        );
        assert_eq!(
            (
                u.cache_read_tokens,
                u.cache_write_tokens,
                u.reasoning_tokens
            ),
            (0, 0, None)
        );
        assert!(!r.failed);
    }
    #[tokio::test]
    async fn unknown_or_missing_usage_is_not_settled() {
        for body in [
            r#"{"model":"jev-latest","answers":{},"usage":{"input_tokens":1,"output_tokens":1,"image_tokens":5}}"#,
            r#"{"model":"jev-latest","answers":{},"usage":{"input_tokens":1}}"#,
            r#"{"model":"jev-latest","answers":{},"usage":{"input_tokens":1.5,"output_tokens":1}}"#,
            r#"{"model":"jev-latest","answers":{},"usage":{"input_tokens":-1,"output_tokens":1}}"#,
            r#"{"model":"jev-latest","answers":{},"usage":{"input_tokens":{"text":1},"output_tokens":1}}"#,
        ] {
            assert!(read(200, body).await.unwrap().usage.is_none(), "{body}");
        }
        for body in [
            r#"{"answers":{},"usage":{"input_tokens":1,"output_tokens":1}}"#,
            r#"{"model":"jev-latest","usage":{"input_tokens":1,"output_tokens":1}}"#,
            r#"{"model":"jev-latest","answers":{}}"#,
            r#"{"model":"","answers":{},"usage":{"input_tokens":1,"output_tokens":1}}"#,
            r#"[]"#,
        ] {
            assert_eq!(
                read(200, body).await.err(),
                Some(UpstreamError::Protocol),
                "{body}"
            );
        }
    }
    #[tokio::test]
    async fn error_bodies_must_be_json_objects() {
        assert!(
            read(400, r#"{"detail":"bad"}"#)
                .await
                .unwrap()
                .usage
                .is_none()
        );
        assert!(
            read(429, r#"{"error":{"message":"slow down"}}"#)
                .await
                .is_ok()
        );
        assert_eq!(
            read(529, "overloaded").await.err(),
            Some(UpstreamError::Protocol)
        );
    }
}
