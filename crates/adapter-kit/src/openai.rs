//! OpenAI 호환 Provider(OpenAI, DeepSeek)가 같이 쓰는 규칙과 응답 읽기.
use crate::{
    adapter::{ApiError, Feature, Header, Profile, ProfileError, ReadError},
    json::Cooperate,
    stream::{JsonDecoder, Scalar},
};
use std::collections::BTreeMap;

/// 읽은 필드 값. 키는 필드 경로다.
pub type Values = BTreeMap<String, Scalar>;

/// OpenAI 호환 키의 접두사.
pub const SK_KEY_PREFIX: &str = "sk-";
/// OpenAI 호환 키로 받지 않는 접두사. Anthropic 키다.
pub const SK_REFUSED_KEY_PREFIXES: &[&str] = &["sk-ant-"];
/// `sk-`로 시작하는 키만 받는다. Anthropic 키(`sk-ant-`)는 받지 않는다.
pub fn accepts_sk_key(secret: &str) -> bool {
    secret.starts_with(SK_KEY_PREFIX)
        && !SK_REFUSED_KEY_PREFIXES
            .iter()
            .any(|p| secret.starts_with(p))
}
/// OpenAI 호환 Provider의 profile 규칙.
pub fn check_compatible_profile(profile: &Profile) -> Result<(), ProfileError> {
    if !profile.approved_beta.is_empty() {
        return Err(ProfileError::InvalidProfile);
    }
    if profile.has(Feature::Cache) && !profile.has_rate("cache_read") {
        return Err(ProfileError::InvalidPrice);
    }
    Ok(())
}

/// 헤더 값이 보이는 ASCII와 탭으로만 되어 있으면 문자열로 돌려준다.
fn visible(value: &[u8]) -> Option<&str> {
    value
        .iter()
        .all(|b| *b == b'\t' || (32..127).contains(b))
        .then(|| std::str::from_utf8(value).expect("ASCII"))
}
/// `content-type` 값이 하나뿐이고 `application/json`(선택 `charset=utf-8`)인지.
pub fn json_content_type<'a>(values: impl IntoIterator<Item = &'a [u8]>) -> bool {
    let mut values = values.into_iter();
    let Some(v) = values.next().and_then(visible) else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    let mut parts = v.split(';');
    if !parts
        .next()
        .is_some_and(|s| s.trim().eq_ignore_ascii_case("application/json"))
    {
        return false;
    }
    let parameters: Vec<_> = parts.collect();
    parameters.is_empty()
        || (parameters.len() == 1 && parameters[0].trim().eq_ignore_ascii_case("charset=utf-8"))
}
/// 업스트림에 보낼 클라이언트 헤더의 이름. JSON `content-type`만 보낸다.
/// `Connection`이 `content-type`을 지우라고 하면 받지 않는다.
pub fn approved_headers(headers: &[Header]) -> Result<Vec<&'static str>, ApiError> {
    fn named<'a>(headers: &'a [Header], name: &'a str) -> impl Iterator<Item = &'a [u8]> {
        headers
            .iter()
            .filter(move |h| h.name == name)
            .map(|h| h.value)
    }
    let named = |name| named(headers, name);
    if !json_content_type(named("content-type"))
        || named("connection").filter_map(visible).any(|v| {
            v.split(',')
                .any(|s| s.trim().eq_ignore_ascii_case("content-type"))
        })
    {
        return Err(ApiError::UnsupportedOption);
    }
    Ok(vec!["content-type"])
}

/// Chat Completions 비스트림 응답 본문을 읽는다.
///
/// - 200 응답에 `error`가 있으면 64 KiB 안의 Provider 오류여야 하고, 실패로 알린다.
/// - 200 응답의 사용량은 `final_usage`가 읽는다.
/// - 200이 아닌 응답은 오류 본문이어야 한다([`error_body_with`]).
pub async fn read_chat_with<U, C: Cooperate>(
    status: u16,
    body: &[u8],
    decoder: JsonDecoder,
    native_error_shape: impl FnOnce(&Values) -> bool,
    final_usage: impl FnOnce(&Values) -> Option<U>,
    cooperate: &mut C,
) -> Result<(Option<U>, bool), ReadError> {
    let values = decoder
        .read_with(body, cooperate)
        .await
        .map_err(|_| ReadError::Protocol)?;
    let error = status == 200 && values.get("error").is_some_and(|v| v != &Scalar::Null);
    if error {
        if body.len() > 64 * 1024 {
            return Err(ReadError::TooLarge);
        }
        if !native_error_shape(&values) {
            return Err(ReadError::Protocol);
        }
    }
    let usage = if status == 200 {
        final_usage(&values)
    } else {
        error_body_with(body, cooperate).await?;
        None
    };
    Ok((usage, error))
}
/// 200이 아닌 응답의 본문이 `{"error":{"message":"...","type":"..."}}` 모양인지 확인한다.
pub async fn error_body_with<C: Cooperate>(
    body: &[u8],
    cooperate: &mut C,
) -> Result<(), ReadError> {
    let v = JsonDecoder::with_string_shapes(&["error"], &["error.message", "error.type"])
        .read_with(body, cooperate)
        .await
        .map_err(|_| ReadError::Protocol)?;
    if v.get("error") == Some(&Scalar::Object)
        && v.get("error.message") == Some(&Scalar::StringShape)
        && v.get("error.type") == Some(&Scalar::StringShape)
    {
        Ok(())
    } else {
        Err(ReadError::Protocol)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn h<'a>(name: &'a str, value: &'a str) -> Header<'a> {
        Header {
            name,
            value: value.as_bytes(),
        }
    }
    #[test]
    fn headers_need_one_json_content_type() {
        assert_eq!(
            approved_headers(&[h("content-type", "application/json; charset=UTF-8")]),
            Ok(vec!["content-type"])
        );
        for headers in [
            vec![],
            vec![h("content-type", "text/plain")],
            vec![h("content-type", "application/json; charset=latin1")],
            vec![
                h("content-type", "application/json"),
                h("content-type", "application/json"),
            ],
            vec![
                h("content-type", "application/json"),
                h("connection", "keep-alive, Content-Type"),
            ],
        ] {
            assert_eq!(approved_headers(&headers), Err(ApiError::UnsupportedOption));
        }
        assert!(!json_content_type([b"application/json\x7f".as_slice()]));
    }
    #[test]
    fn keys() {
        assert!(accepts_sk_key("sk-proj-1"));
        assert!(!accepts_sk_key("sk-ant-1"));
        assert!(!accepts_sk_key("key"));
    }
}
