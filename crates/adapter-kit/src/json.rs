//! 원본을 보존하는 bounded JSON 문법/중복 검사. 본문 문자열과 트리를 복제하지 않는다.
//! 검사기는 64 KiB를 읽을 때마다 [`Cooperate::cooperate`]를 기다린다. 본체는 이때 tokio에 실행을 넘기고,
//! WASM 어댑터는 넘기지 않고([`NoYield`]) 한 번에 끝낸다([`scan`]).
use std::{
    collections::BTreeSet,
    future::Future,
    pin::pin,
    task::{Context, Poll, Waker},
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Object,
    Array,
    String,
    Number,
    Bool,
    Null,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JsonError;
pub trait Visitor {
    fn array_limit(&self, _path: &[String]) -> usize {
        1024
    }
    fn value(&mut self, path: &[String], kind: Kind, raw: &str) -> Result<(), JsonError>;
    fn object_end(
        &mut self,
        _path: &[String],
        _keys: &BTreeSet<String>,
        _raw: &str,
    ) -> Result<(), JsonError> {
        Ok(())
    }
    fn array_end(&mut self, _path: &[String], _items: usize, _raw: &str) -> Result<(), JsonError> {
        Ok(())
    }
}
/// 긴 검사 중에 실행을 넘기는 방법.
pub trait Cooperate {
    fn cooperate(&mut self) -> impl Future<Output = ()> + Send;
}
/// 빌린 [`Cooperate`]. 검사 하나가 같은 방법으로 JSON 검사와 PNG 검사를 차례로 할 때 쓴다.
impl<C: Cooperate> Cooperate for &mut C {
    fn cooperate(&mut self) -> impl Future<Output = ()> + Send {
        (**self).cooperate()
    }
}
/// 실행을 넘기지 않는다. 런타임이 없는 곳(WASM 어댑터)에서 쓴다.
pub struct NoYield;
impl Cooperate for NoYield {
    fn cooperate(&mut self) -> impl Future<Output = ()> + Send {
        std::future::ready(())
    }
}
/// 실행을 넘기지 않고 한 번에 검사한다.
pub fn scan(bytes: &[u8], visitor: &mut impl Visitor) -> Result<(), JsonError> {
    ready(scan_with(bytes, visitor, NoYield))
}
/// [`NoYield`]만 기다리는 future를 한 번 poll해 끝낸다. 실행기가 없는 곳(WASM 어댑터)에서 쓴다.
///
/// # Panics
/// future가 다른 것을 기다리면 panic한다.
pub fn ready<F: Future>(future: F) -> F::Output {
    match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("NoYield 말고 다른 것을 기다리는 future"),
    }
}
/// 64 KiB마다 `cooperate`를 기다리며 검사한다.
pub async fn scan_with<V: Visitor, C: Cooperate>(
    bytes: &[u8],
    visitor: &mut V,
    cooperate: C,
) -> Result<(), JsonError> {
    let text = std::str::from_utf8(bytes).map_err(|_| JsonError)?;
    let mut parser = Parser {
        text,
        pos: 0,
        tokens: 0,
        key_bytes: 0,
        path: Vec::new(),
        visitor,
        last_yield: 0,
        cooperate,
    };
    parser.value(0).await?;
    parser.whitespace().await;
    if parser.pos != text.len() {
        return Err(JsonError);
    }
    Ok(())
}
pub fn string(raw: &str, max: usize) -> Result<String, JsonError> {
    if raw.len() > max.saturating_mul(6).saturating_add(2)
        || !raw.starts_with('"')
        || !raw.ends_with('"')
        || raw.len() < 2
    {
        return Err(JsonError);
    }
    // Deserialize의 scratch/String 할당 전에 실제 decoded bytes 상한을 검사한다.
    let bytes = raw.as_bytes();
    let mut pos = 1;
    let mut decoded = 0usize;
    let hex = |part: &[u8]| -> Result<u16, JsonError> {
        if part.len() != 4 || !part.iter().all(u8::is_ascii_hexdigit) {
            return Err(JsonError);
        }
        u16::from_str_radix(std::str::from_utf8(part).map_err(|_| JsonError)?, 16)
            .map_err(|_| JsonError)
    };
    while pos < bytes.len() - 1 {
        let b = bytes[pos];
        pos += 1;
        let count = if b == b'\\' {
            let escape = *bytes.get(pos).ok_or(JsonError)?;
            pos += 1;
            if escape == b'u' {
                let n = hex(bytes.get(pos..pos + 4).ok_or(JsonError)?)?;
                pos += 4;
                if (0xd800..=0xdbff).contains(&n) {
                    if bytes.get(pos..pos + 2) != Some(b"\\u") {
                        return Err(JsonError);
                    }
                    pos += 2;
                    let low = hex(bytes.get(pos..pos + 4).ok_or(JsonError)?)?;
                    pos += 4;
                    if !(0xdc00..=0xdfff).contains(&low) {
                        return Err(JsonError);
                    }
                    4
                } else if (0xdc00..=0xdfff).contains(&n) {
                    return Err(JsonError);
                } else if n <= 0x7f {
                    1
                } else if n <= 0x7ff {
                    2
                } else {
                    3
                }
            } else if b"\"\\/bfnrt".contains(&escape) {
                1
            } else {
                return Err(JsonError);
            }
        } else {
            1
        };
        decoded = decoded.checked_add(count).ok_or(JsonError)?;
        if decoded > max {
            return Err(JsonError);
        }
    }
    serde_json::from_str(raw).map_err(|_| JsonError)
}
const KEY_RETAINED_BYTES: usize = 192 * 1024; // 64KiB는 유한 parser frame/경로/임시 scratch 여유다.

struct Parser<'a, V, C> {
    text: &'a str,
    pos: usize,
    tokens: usize,
    key_bytes: usize,
    path: Vec<String>,
    visitor: &'a mut V,
    last_yield: usize,
    cooperate: C,
}
impl<'a, V: Visitor, C: Cooperate> Parser<'a, V, C> {
    async fn whitespace(&mut self) {
        while self.pos < self.text.len() && b" \r\n\t".contains(&self.text.as_bytes()[self.pos]) {
            self.pos += 1;
            self.cooperate().await;
        }
    }
    async fn cooperate(&mut self) {
        if self.pos.saturating_sub(self.last_yield) >= 64 * 1024 {
            self.last_yield = self.pos;
            self.cooperate.cooperate().await;
        }
    }
    fn byte(&self) -> Result<u8, JsonError> {
        self.text.as_bytes().get(self.pos).copied().ok_or(JsonError)
    }
    async fn value(&mut self, depth: usize) -> Result<(), JsonError> {
        self.whitespace().await;
        self.cooperate().await;
        self.tokens += 1;
        if depth > 32 || self.tokens > 100_000 {
            return Err(JsonError);
        }
        let start = self.pos;
        match self.byte()? {
            b'{' => {
                self.visitor.value(&self.path, Kind::Object, "")?;
                self.pos += 1;
                self.whitespace().await;
                let mut keys = BTreeSet::new();
                let mut retained = 0;
                if self.byte()? != b'}' {
                    loop {
                        self.whitespace().await;
                        let raw = self.read_string().await?;
                        let room = KEY_RETAINED_BYTES
                            .saturating_sub(self.key_bytes)
                            .saturating_sub(128);
                        // escaped-string scratch와 소유 String이 겹치는 순간도 같은 예산 안이다.
                        let key = string(raw, (room / 4).min(32 * 1024))?;
                        // BTree node/String/경로 복사까지 보수적으로 계산한다.
                        let bytes = key.len() * 2 + 128;
                        self.key_bytes = self.key_bytes.checked_add(bytes).ok_or(JsonError)?;
                        retained += bytes;
                        if self.key_bytes > KEY_RETAINED_BYTES || !keys.insert(key.clone()) {
                            return Err(JsonError);
                        }
                        self.whitespace().await;
                        if self.byte()? != b':' {
                            return Err(JsonError);
                        }
                        self.pos += 1;
                        self.path.push(key);
                        Box::pin(self.value(depth + 1)).await?;
                        self.path.pop();
                        self.whitespace().await;
                        match self.byte()? {
                            b',' => self.pos += 1,
                            b'}' => break,
                            _ => return Err(JsonError),
                        }
                    }
                }
                self.pos += 1;
                self.visitor
                    .object_end(&self.path, &keys, &self.text[start..self.pos])?;
                self.key_bytes -= retained;
            }
            b'[' => {
                self.visitor.value(&self.path, Kind::Array, "")?;
                self.pos += 1;
                self.whitespace().await;
                let mut items = 0;
                if self.byte()? != b']' {
                    loop {
                        items += 1;
                        if items > self.visitor.array_limit(&self.path) {
                            return Err(JsonError);
                        }
                        self.path.push("*".into());
                        Box::pin(self.value(depth + 1)).await?;
                        self.path.pop();
                        self.whitespace().await;
                        match self.byte()? {
                            b',' => self.pos += 1,
                            b']' => break,
                            _ => return Err(JsonError),
                        }
                    }
                }
                self.pos += 1;
                self.visitor
                    .array_end(&self.path, items, &self.text[start..self.pos])?;
            }
            b'"' => {
                let raw = self.read_string().await?;
                self.visitor.value(&self.path, Kind::String, raw)?;
            }
            b't' | b'f' | b'n' => {
                let (literal, kind) = match self.byte()? {
                    b't' => ("true", Kind::Bool),
                    b'f' => ("false", Kind::Bool),
                    _ => ("null", Kind::Null),
                };
                if !self.text[self.pos..].starts_with(literal) {
                    return Err(JsonError);
                }
                self.pos += literal.len();
                self.visitor.value(&self.path, kind, literal)?;
            }
            b'-' | b'0'..=b'9' => {
                if self.byte()? == b'-' {
                    self.pos += 1;
                }
                match self.byte()? {
                    b'0' => self.pos += 1,
                    b'1'..=b'9' => self.digits().await?,
                    _ => return Err(JsonError),
                }
                if self.text.as_bytes().get(self.pos) == Some(&b'.') {
                    self.pos += 1;
                    self.digits().await?;
                }
                if self
                    .text
                    .as_bytes()
                    .get(self.pos)
                    .is_some_and(|b| b"eE".contains(b))
                {
                    self.pos += 1;
                    if self
                        .text
                        .as_bytes()
                        .get(self.pos)
                        .is_some_and(|b| b"+-".contains(b))
                    {
                        self.pos += 1;
                        self.cooperate().await;
                    }
                    self.digits().await?;
                }
                self.visitor
                    .value(&self.path, Kind::Number, &self.text[start..self.pos])?;
            }
            _ => return Err(JsonError),
        }
        Ok(())
    }
    async fn digits(&mut self) -> Result<(), JsonError> {
        let start = self.pos;
        while self.pos < self.text.len() && self.text.as_bytes()[self.pos].is_ascii_digit() {
            self.pos += 1;
            self.cooperate().await;
        }
        if start == self.pos {
            Err(JsonError)
        } else {
            Ok(())
        }
    }
    fn hex4(&mut self) -> Result<u16, JsonError> {
        let end = self.pos.checked_add(4).ok_or(JsonError)?;
        let raw = self.text.as_bytes().get(self.pos..end).ok_or(JsonError)?;
        if !raw.iter().all(u8::is_ascii_hexdigit) {
            return Err(JsonError);
        }
        let n = u16::from_str_radix(std::str::from_utf8(raw).map_err(|_| JsonError)?, 16)
            .map_err(|_| JsonError)?;
        self.pos = end;
        Ok(n)
    }
    async fn read_string(&mut self) -> Result<&'a str, JsonError> {
        let start = self.pos;
        if self.byte()? != b'"' {
            return Err(JsonError);
        }
        self.pos += 1;
        loop {
            self.cooperate().await;
            match self.byte()? {
                b'"' => {
                    self.pos += 1;
                    return Ok(&self.text[start..self.pos]);
                }
                b'\\' => {
                    self.pos += 1;
                    match self.byte()? {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => self.pos += 1,
                        b'u' => {
                            self.pos += 1;
                            let code = self.hex4()?;
                            if (0xd800..=0xdbff).contains(&code) {
                                if self.text.as_bytes().get(self.pos..self.pos + 2) != Some(b"\\u")
                                {
                                    return Err(JsonError);
                                }
                                self.pos += 2;
                                if !(0xdc00..=0xdfff).contains(&self.hex4()?) {
                                    return Err(JsonError);
                                }
                            } else if (0xdc00..=0xdfff).contains(&code) {
                                return Err(JsonError);
                            }
                        }
                        _ => return Err(JsonError),
                    }
                }
                0..=31 => return Err(JsonError),
                _ => self.pos += 1,
            }
        }
    }
}
/// 재귀 검사 frame 하나의 크기. 본체가 자기 `Cooperate`로 고정 예산을 확인할 때 쓴다.
#[doc(hidden)]
pub fn parser_frame_size<C: Cooperate>(cooperate: C) -> usize {
    struct Check;
    impl Visitor for Check {
        fn value(&mut self, _: &[String], _: Kind, _: &str) -> Result<(), JsonError> {
            Ok(())
        }
    }
    let mut visitor = Check;
    let mut parser = Parser {
        text: "{}",
        pos: 0,
        tokens: 0,
        key_bytes: 0,
        path: Vec::new(),
        visitor: &mut visitor,
        last_yield: 0,
        cooperate,
    };
    let frame = parser.value(0);
    std::mem::size_of_val(&frame)
}
#[cfg(test)]
mod tests {
    use super::*;
    struct Check;
    impl Visitor for Check {
        fn value(&mut self, _: &[String], _: Kind, _: &str) -> Result<(), JsonError> {
            Ok(())
        }
    }
    /// 한 번 Pending을 돌려주고 깨운다. tokio 없이 양보 지점을 확인한다.
    struct PendOnce;
    impl Cooperate for PendOnce {
        fn cooperate(&mut self) -> impl Future<Output = ()> + Send {
            let mut done = false;
            std::future::poll_fn(move |cx| {
                if done {
                    Poll::Ready(())
                } else {
                    done = true;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
        }
    }
    #[test]
    fn decoded_string_bounds_precede_scratch_allocation() {
        assert_eq!(string(r#""\u0061""#, 1).unwrap(), "a");
        assert_eq!(string(r#""\ud83d\ude00""#, 4).unwrap(), "😀");
        assert!(string(r#""aaaaa\u0061""#, 5).is_err());
        assert!(string(r#""\ud83d\ude00""#, 3).is_err());
        assert!(string(r#""\ud800""#, 32).is_err());
        assert!(string("123", 32).is_err());
    }
    #[test]
    fn parser_frames_fit_the_reserved_fixed_budget() {
        assert!(
            parser_frame_size(NoYield) <= 1024,
            "recursive parser frame exceeded fixed budget"
        );
    }
    #[test]
    fn large_string_and_whitespace_yield_before_completion() {
        for raw in [
            format!(r#"{{"text":"{}"}}"#, "a".repeat(1024 * 1024)),
            format!("{}{{}}", " ".repeat(1024 * 1024)),
        ] {
            let mut visitor = Check;
            let mut future = Box::pin(scan_with(raw.as_bytes(), &mut visitor, PendOnce));
            let mut cx = Context::from_waker(Waker::noop());
            assert!(
                future.as_mut().poll(&mut cx).is_pending(),
                "large scan monopolized first poll"
            );
            while future.as_mut().poll(&mut cx).is_pending() {}
        }
        // 실행을 넘기지 않는 검사는 한 번에 끝난다.
        let raw = format!(r#"{{"text":"{}"}}"#, "a".repeat(1024 * 1024));
        scan(raw.as_bytes(), &mut Check).unwrap();
    }
    #[test]
    fn integer_digits_yield_before_number_visitor() {
        use std::cell::Cell;
        struct Numbers<'a>(&'a Cell<usize>);
        impl Visitor for Numbers<'_> {
            fn value(&mut self, _: &[String], kind: Kind, _: &str) -> Result<(), JsonError> {
                if kind == Kind::Number {
                    self.0.set(self.0.get() + 1);
                }
                Ok(())
            }
        }
        for sign in ["", "-"] {
            let text = format!("{sign}{}", "7".repeat(1024 * 1024));
            let seen = Cell::new(0);
            let mut visitor = Numbers(&seen);
            let mut parser = Parser {
                text: &text,
                pos: 0,
                tokens: 0,
                key_bytes: 0,
                path: Vec::new(),
                visitor: &mut visitor,
                last_yield: 0,
                cooperate: PendOnce,
            };
            {
                let mut future = Box::pin(parser.value(0));
                let pending = future
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending();
                assert_eq!(seen.get(), 0, "number visitor ran before cooperative yield");
                assert!(pending, "integer scanner monopolized first poll");
            }
            assert!(
                (64 * 1024..text.len()).contains(&parser.pos),
                "not an intermediate digit yield"
            );
            scan(text.as_bytes(), &mut Check).unwrap();
        }
        for text in ["0", "1", "-1", "-0", "1.25", "1e2", "1e+2", "1e-2"] {
            scan(text.as_bytes(), &mut Check).unwrap();
        }
        for text in ["01", "-01", "-", "1.", "1e", "1e+"] {
            assert!(scan(text.as_bytes(), &mut Check).is_err());
        }
    }
    #[test]
    fn escaped_duplicates_and_every_nested_value_are_checked() {
        for raw in [
            r#"{"model":"a","\u006dodel":"b"}"#,
            r#"{"x":{"a":1,"a":2}}"#,
            r#"{"x":[{"a":1,"a":2}]}"#,
            r#"{"x":"\ud800"}"#,
            r#"{"x":"\udc00"}"#,
            r#"{"x":01}"#,
            r#"{"x":1.}"#,
            r#"{"x":1e}"#,
            r#"{"x":NaN}"#,
            r#"{"x":true}false"#,
            r#"{"x":[1,]}"#,
            r#"{"x":{},}"#,
        ] {
            assert!(
                scan(raw.as_bytes(), &mut Check).is_err(),
                "invalid JSON accepted"
            );
        }
        for raw in [
            r#"{"x":"\ud83d\ude00"}"#,
            r#"{"x":"한글", "array":[1,true,null,-1.2e+1]}"#,
        ] {
            scan(raw.as_bytes(), &mut Check).unwrap();
        }
        let large = format!(r#"{{"text":"{}"}}"#, "a".repeat(2 * 1024 * 1024));
        scan(large.as_bytes(), &mut Check).unwrap();
        assert!(
            scan(
                format!("{}0{}", "[".repeat(33), "]".repeat(33)).as_bytes(),
                &mut Check
            )
            .is_err()
        );
    }
}
