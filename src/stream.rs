//! 원본 전송과 독립적인 증분 JSON/SSE 검사. 문자열 본문은 축적하지 않는다.
use std::collections::{BTreeMap, BTreeSet};

pub const RETAINED_BYTES: usize = 256 * 1024;
pub const CHUNK_BYTES: usize = 16 * 1024;
pub const QUEUE_BYTES: usize = 64 * 1024;
const EVENT_BYTES: usize = 16 * 1024 * 1024;
const TOTAL_BYTES: usize = 64 * 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    Syntax,
    Limit,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scalar {
    Object,
    Array,
    String(String),
    StringShape,
    Number(String),
    Bool(bool),
    Null,
}
impl Scalar {
    pub fn text(&self) -> Option<&str> {
        if let Self::String(s) = self {
            Some(s)
        } else {
            None
        }
    }
    pub fn integer(&self) -> Option<u64> {
        if let Self::Number(s) = self
            && s.len() <= 19
            && s.bytes().all(|b| b.is_ascii_digit())
        {
            return s.parse().ok().filter(|n| *n <= i64::MAX as u64);
        }
        None
    }
}
#[derive(Clone, Copy)]
enum Phase {
    KeyOrEnd,
    Key,
    Colon,
    Value,
    ValueOrEnd,
    CommaOrEnd,
}
struct Container {
    object: bool,
    phase: Phase,
    keys: BTreeSet<String>,
    key: String,
    retained: usize,
    items: usize,
}
enum Token {
    None,
    String {
        key: bool,
        keep: bool,
        bytes: Vec<u8>,
        escape: Escape,
        utf8: Utf8,
    },
    Atom {
        bytes: Vec<u8>,
        keep: bool,
        number: Number,
    },
}
#[derive(Clone, Copy)]
enum Escape {
    Plain,
    Slash,
    Hex(u16, u8),
    LowSlash(u16),
    LowU(u16),
    LowHex(u16, u16, u8),
}
#[derive(Default)]
struct Utf8 {
    left: u8,
    low: u8,
    high: u8,
}
impl Utf8 {
    fn byte(&mut self, b: u8) -> Result<(), ParseError> {
        if self.left > 0 {
            if !(self.low..=self.high).contains(&b) {
                return Err(ParseError::Syntax);
            }
            self.left -= 1;
            self.low = 0x80;
            self.high = 0xbf;
            return Ok(());
        }
        match b {
            0..=0x7f => {}
            0xc2..=0xdf => {
                self.left = 1;
                self.low = 0x80;
                self.high = 0xbf;
            }
            0xe0..=0xef => {
                self.left = 2;
                self.low = if b == 0xe0 { 0xa0 } else { 0x80 };
                self.high = if b == 0xed { 0x9f } else { 0xbf };
            }
            0xf0..=0xf4 => {
                self.left = 3;
                self.low = if b == 0xf0 { 0x90 } else { 0x80 };
                self.high = if b == 0xf4 { 0x8f } else { 0xbf };
            }
            _ => return Err(ParseError::Syntax),
        }
        Ok(())
    }
}
#[derive(Clone, Copy)]
enum Number {
    Start,
    Minus,
    Zero,
    Int,
    Dot,
    Fraction,
    E,
    Sign,
    Exponent,
    Literal,
}
impl Number {
    fn byte(&mut self, b: u8) -> Result<(), ParseError> {
        *self = match (*self, b) {
            (Self::Start, b'-') => Self::Minus,
            (Self::Start | Self::Minus, b'0') => Self::Zero,
            (Self::Start | Self::Minus, b'1'..=b'9') => Self::Int,
            (Self::Int, b'0'..=b'9') => Self::Int,
            (Self::Zero | Self::Int, b'.') => Self::Dot,
            (Self::Dot | Self::Fraction, b'0'..=b'9') => Self::Fraction,
            (Self::Zero | Self::Int | Self::Fraction, b'e' | b'E') => Self::E,
            (Self::E, b'+' | b'-') => Self::Sign,
            (Self::E | Self::Sign | Self::Exponent, b'0'..=b'9') => Self::Exponent,
            (Self::Start, b't' | b'f' | b'n') => Self::Literal,
            (Self::Literal, b'a'..=b'z') => Self::Literal,
            _ => return Err(ParseError::Syntax),
        };
        Ok(())
    }
}
/// 선택 경로는 점으로 구분하며 배열 원소는 `*`다. 모든 객체의 decoded key 중복도 검사한다.
pub struct JsonDecoder {
    fields: Vec<String>,
    string_shapes: Vec<String>,
    stack: Vec<Container>,
    token: Token,
    values: BTreeMap<String, Scalar>,
    done: bool,
    tokens: usize,
    retained: usize,
    reserved: usize,
    peak: usize,
}
impl JsonDecoder {
    pub fn new(fields: &[&str]) -> Self {
        Self::with_string_shapes(fields, &[])
    }
    /// 내용 대신 문자열의 유효한 JSON shape만 보관한다. 오류 본문은 원본 전달 경로가 소유한다.
    pub fn with_string_shapes(fields: &[&str], string_shapes: &[&str]) -> Self {
        Self {
            fields: fields.iter().map(|s| s.to_string()).collect(),
            string_shapes: string_shapes.iter().map(|s| s.to_string()).collect(),
            stack: Vec::new(),
            token: Token::None,
            values: BTreeMap::new(),
            done: false,
            tokens: 0,
            retained: 0,
            reserved: 0,
            peak: 0,
        }
    }
    fn path(&self) -> String {
        self.stack
            .iter()
            .map(|c| {
                if c.object {
                    c.key.replace('\\', "\\\\").replace('.', "\\.")
                } else {
                    "*".to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(".")
    }
    fn selected(&self) -> bool {
        let path = self
            .stack
            .iter()
            .map(|c| if c.object { c.key.as_str() } else { "*" })
            .collect::<Vec<_>>();
        self.fields.iter().any(|field| {
            if let Some(prefix) = field.strip_suffix(".**") {
                let prefix = prefix.split('.').collect::<Vec<_>>();
                path.len() >= prefix.len() && path.starts_with(&prefix)
            } else {
                path.iter().copied().eq(field.split('.'))
            }
        })
    }
    fn string_shape_selected(&self) -> bool {
        self.string_shapes.iter().any(|field| {
            self.stack
                .iter()
                .map(|c| if c.object { c.key.as_str() } else { "*" })
                .eq(field.split('.'))
        })
    }
    pub fn retained_peak(&self) -> usize {
        self.peak
    }
    fn measure(&mut self, extra: usize) -> Result<(), ParseError> {
        // String/BTree node/path/lexer scratch를 보수적으로 포함한다.
        let n = 16 * 1024
            + self.retained
            + self.stack.len() * 256
            + extra
            + self
                .fields
                .iter()
                .chain(&self.string_shapes)
                .map(|s| s.capacity() + 64)
                .sum::<usize>()
            + self
                .values
                .iter()
                .map(|(k, v)| {
                    k.capacity()
                        + 128
                        + match v {
                            Scalar::String(s) | Scalar::Number(s) => s.capacity(),
                            _ => 0,
                        }
                })
                .sum::<usize>();
        self.peak = self.peak.max(n);
        if n > RETAINED_BYTES - 4096 - self.reserved {
            Err(ParseError::Limit)
        } else {
            Ok(())
        }
    }
    fn complete(&mut self) -> Result<(), ParseError> {
        if let Some(c) = self.stack.last_mut() {
            match c.phase {
                Phase::Value | Phase::ValueOrEnd => {
                    c.phase = Phase::CommaOrEnd;
                    c.items += 1;
                }
                _ => return Err(ParseError::Syntax),
            }
        } else {
            if self.done {
                return Err(ParseError::Syntax);
            }
            self.done = true;
        }
        Ok(())
    }
    fn save(&mut self, v: Scalar) -> Result<(), ParseError> {
        let path_bytes = self
            .stack
            .iter()
            .map(|c| if c.object { c.key.len() * 2 + 1 } else { 2 })
            .sum::<usize>();
        let value_bytes = match &v {
            Scalar::String(s) | Scalar::Number(s) => s.len(),
            _ => 0,
        };
        let existing = self
            .values
            .iter()
            .map(|(k, v)| {
                k.len()
                    + 128
                    + match v {
                        Scalar::String(s) | Scalar::Number(s) => s.len(),
                        _ => 0,
                    }
            })
            .sum::<usize>();
        if path_bytes > 1024 || existing + path_bytes + value_bytes + 128 > 16 * 1024 {
            return Err(ParseError::Limit);
        }
        let path = self.path();
        if self.values.insert(path, v).is_some() {
            return Err(ParseError::Syntax);
        }
        if self.values.len() > 64 {
            return Err(ParseError::Limit);
        }
        self.measure(0)
    }
    fn scalar(&mut self, v: Option<Scalar>) -> Result<(), ParseError> {
        if let Some(v) = v {
            self.save(v)?;
        }
        self.complete()
    }
    fn close(&mut self, b: u8) -> Result<(), ParseError> {
        let c = self.stack.pop().ok_or(ParseError::Syntax)?;
        if c.object != (b == b'}')
            || !matches!(
                c.phase,
                Phase::CommaOrEnd | Phase::KeyOrEnd | Phase::ValueOrEnd
            )
        {
            return Err(ParseError::Syntax);
        }
        self.retained -= c.retained;
        if !c.object {
            let key = format!("{}.$length", self.path());
            if self.fields.contains(&key) {
                self.values.insert(key, Scalar::Number(c.items.to_string()));
                self.measure(0)?;
            }
        }
        self.complete()
    }
    fn finish_atom(
        &mut self,
        bytes: Vec<u8>,
        keep: bool,
        number: Number,
    ) -> Result<(), ParseError> {
        let scalar = if matches!(number, Number::Literal) {
            match bytes.as_slice() {
                b"true" => Scalar::Bool(true),
                b"false" => Scalar::Bool(false),
                b"null" => Scalar::Null,
                _ => return Err(ParseError::Syntax),
            }
        } else {
            if !matches!(
                number,
                Number::Zero | Number::Int | Number::Fraction | Number::Exponent
            ) {
                return Err(ParseError::Syntax);
            }
            Scalar::Number(if keep {
                String::from_utf8(bytes).map_err(|_| ParseError::Syntax)?
            } else {
                String::new()
            })
        };
        self.scalar(keep.then_some(scalar))
    }
    pub fn byte(&mut self, b: u8) -> Result<(), ParseError> {
        let token = std::mem::replace(&mut self.token, Token::None);
        match token {
            Token::String {
                key,
                keep,
                mut bytes,
                mut escape,
                mut utf8,
            } => {
                let mut end = false;
                match escape {
                    Escape::Plain => {
                        if utf8.left > 0 {
                            utf8.byte(b)?;
                        } else {
                            match b {
                                b'"' => end = true,
                                b'\\' => escape = Escape::Slash,
                                0..=31 => return Err(ParseError::Syntax),
                                _ => utf8.byte(b)?,
                            }
                        }
                    }
                    Escape::Slash => {
                        escape = match b {
                            b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => Escape::Plain,
                            b'u' => Escape::Hex(0, 0),
                            _ => return Err(ParseError::Syntax),
                        };
                    }
                    Escape::Hex(n, i) => {
                        let n = n * 16 + hex(b)?;
                        escape = if i < 3 {
                            Escape::Hex(n, i + 1)
                        } else if (0xd800..=0xdbff).contains(&n) {
                            Escape::LowSlash(n)
                        } else if (0xdc00..=0xdfff).contains(&n) {
                            return Err(ParseError::Syntax);
                        } else {
                            Escape::Plain
                        };
                    }
                    Escape::LowSlash(n) => {
                        if b != b'\\' {
                            return Err(ParseError::Syntax);
                        }
                        escape = Escape::LowU(n);
                    }
                    Escape::LowU(n) => {
                        if b != b'u' {
                            return Err(ParseError::Syntax);
                        }
                        escape = Escape::LowHex(n, 0, 0);
                    }
                    Escape::LowHex(high, n, i) => {
                        let n = n * 16 + hex(b)?;
                        escape = if i < 3 {
                            Escape::LowHex(high, n, i + 1)
                        } else {
                            if !(0xdc00..=0xdfff).contains(&n) {
                                return Err(ParseError::Syntax);
                            }
                            Escape::Plain
                        };
                    }
                }
                if end {
                    if key {
                        bytes.push(b'"');
                        let s = crate::json::string(
                            std::str::from_utf8(&bytes).map_err(|_| ParseError::Syntax)?,
                            32 * 1024,
                        )
                        .map_err(|_| ParseError::Syntax)?;
                        self.measure(bytes.capacity() + s.capacity() * 2 + 128)?;
                        let c = self.stack.last_mut().ok_or(ParseError::Syntax)?;
                        let n = s.len() * 2 + 128;
                        if !c.keys.insert(s.clone()) {
                            return Err(ParseError::Syntax);
                        }
                        c.key = s;
                        c.phase = Phase::Colon;
                        c.retained += n;
                        self.retained += n;
                    } else {
                        let value = if keep {
                            bytes.push(b'"');
                            Some(Scalar::String(
                                crate::json::string(
                                    std::str::from_utf8(&bytes).map_err(|_| ParseError::Syntax)?,
                                    1024,
                                )
                                .map_err(|_| ParseError::Limit)?,
                            ))
                        } else if self.string_shape_selected() {
                            Some(Scalar::StringShape)
                        } else {
                            None
                        };
                        self.scalar(value)?;
                    }
                } else {
                    if keep || key {
                        if bytes.len() >= if key { 32 * 1024 } else { 6 * 1024 + 2 } {
                            return Err(ParseError::Limit);
                        }
                        bytes.push(b);
                        self.measure(bytes.capacity())?;
                    }
                    self.token = Token::String {
                        key,
                        keep,
                        bytes,
                        escape,
                        utf8,
                    };
                }
                return Ok(());
            }
            Token::Atom {
                mut bytes,
                keep,
                mut number,
            } => {
                if b" \r\n\t,]}".contains(&b) {
                    self.finish_atom(bytes, keep, number)?;
                } else {
                    number.byte(b)?;
                    if keep || matches!(number, Number::Literal) {
                        if bytes.len() >= 64 {
                            return Err(ParseError::Limit);
                        }
                        bytes.push(b);
                    }
                    self.token = Token::Atom {
                        bytes,
                        keep,
                        number,
                    };
                    return Ok(());
                }
            }
            Token::None => {}
        }
        if b" \r\n\t".contains(&b) {
            return Ok(());
        }
        if self.done {
            return Err(ParseError::Syntax);
        }
        if let Some(c) = self.stack.last_mut() {
            match c.phase {
                Phase::KeyOrEnd | Phase::Key => {
                    if b == b'}' && matches!(c.phase, Phase::KeyOrEnd) {
                        return self.close(b);
                    }
                    if b != b'"' {
                        return Err(ParseError::Syntax);
                    }
                    self.token = Token::String {
                        key: true,
                        keep: true,
                        bytes: vec![b'"'],
                        escape: Escape::Plain,
                        utf8: Utf8::default(),
                    };
                    return Ok(());
                }
                Phase::Colon => {
                    if b != b':' {
                        return Err(ParseError::Syntax);
                    }
                    c.phase = Phase::Value;
                    return Ok(());
                }
                Phase::CommaOrEnd => {
                    if b == b',' {
                        c.phase = if c.object { Phase::Key } else { Phase::Value };
                        return Ok(());
                    }
                    if b == b'}' || b == b']' {
                        return self.close(b);
                    }
                    return Err(ParseError::Syntax);
                }
                Phase::ValueOrEnd => {
                    if b == b']' {
                        return self.close(b);
                    }
                }
                Phase::Value => {}
            }
        }
        self.tokens += 1;
        if self.tokens > 100_000 {
            return Err(ParseError::Limit);
        }
        match b {
            b'{' | b'[' => {
                if self.selected() {
                    self.save(if b == b'{' {
                        Scalar::Object
                    } else {
                        Scalar::Array
                    })?;
                }
                if self.stack.len() >= 32 {
                    return Err(ParseError::Limit);
                }
                self.stack.push(Container {
                    object: b == b'{',
                    phase: if b == b'{' {
                        Phase::KeyOrEnd
                    } else {
                        Phase::ValueOrEnd
                    },
                    keys: BTreeSet::new(),
                    key: String::new(),
                    retained: 0,
                    items: 0,
                });
                self.measure(0)?;
            }
            b'"' => {
                let keep = self.selected() && !self.string_shape_selected();
                self.token = Token::String {
                    key: false,
                    keep,
                    bytes: if keep { vec![b'"'] } else { Vec::new() },
                    escape: Escape::Plain,
                    utf8: Utf8::default(),
                };
            }
            _ => {
                let keep = self.selected();
                let mut number = Number::Start;
                number.byte(b)?;
                self.token = Token::Atom {
                    bytes: if keep || matches!(number, Number::Literal) {
                        vec![b]
                    } else {
                        Vec::new()
                    },
                    keep,
                    number,
                };
            }
        }
        Ok(())
    }
    pub fn finish(mut self) -> Result<BTreeMap<String, Scalar>, ParseError> {
        if let Token::Atom {
            bytes,
            keep,
            number,
        } = std::mem::replace(&mut self.token, Token::None)
        {
            self.finish_atom(bytes, keep, number)?;
        } else if !matches!(self.token, Token::None) {
            return Err(ParseError::Syntax);
        }
        if !self.done || !self.stack.is_empty() {
            return Err(ParseError::Syntax);
        }
        Ok(self.values)
    }
}
fn hex(b: u8) -> Result<u16, ParseError> {
    match b {
        b'0'..=b'9' => Ok((b - b'0') as u16),
        b'a'..=b'f' => Ok((b - b'a' + 10) as u16),
        b'A'..=b'F' => Ok((b - b'A' + 10) as u16),
        _ => Err(ParseError::Syntax),
    }
}

pub struct Event {
    pub wire_bytes: usize,
    pub name: String,
    pub values: BTreeMap<String, Scalar>,
}
/// line/event를 저장하지 않는다. data line 사이의 논리 LF만 JSON decoder에 입력한다.
pub struct SseDecoder {
    fields: Vec<String>,
    string_shapes: Vec<String>,
    json: JsonDecoder,
    field: Vec<u8>,
    name: String,
    value: Vec<u8>,
    colon: bool,
    space: bool,
    data: bool,
    has_data: bool,
    line_nonempty: bool,
    cr: bool,
    bom_left: u8,
    boundary_after_cr: Option<usize>,
    event_bytes: usize,
    reserved: usize,
    total: usize,
    utf8: Utf8,
    peak: usize,
    allow_done: bool,
    sentinel: Vec<u8>,
    data_started: bool,
}
impl SseDecoder {
    pub fn new(fields: &[&str]) -> Self {
        Self::with_string_shapes(fields, &[])
    }
    pub fn with_string_shapes(fields: &[&str], string_shapes: &[&str]) -> Self {
        Self {
            fields: fields.iter().map(|s| s.to_string()).collect(),
            string_shapes: string_shapes.iter().map(|s| s.to_string()).collect(),
            json: JsonDecoder::with_string_shapes(fields, string_shapes),
            field: Vec::new(),
            name: String::new(),
            value: Vec::new(),
            colon: false,
            space: false,
            data: false,
            has_data: false,
            line_nonempty: false,
            cr: false,
            bom_left: 0,
            boundary_after_cr: None,
            event_bytes: 0,
            reserved: 0,
            total: 0,
            utf8: Utf8::default(),
            peak: 0,
            allow_done: false,
            sentinel: Vec::new(),
            data_started: false,
        }
    }
    pub fn chat(fields: &[&str]) -> Self {
        Self::chat_with_string_shapes(fields, &[])
    }
    pub fn chat_with_string_shapes(fields: &[&str], string_shapes: &[&str]) -> Self {
        let mut s = Self::with_string_shapes(fields, string_shapes);
        s.allow_done = true;
        s
    }
    fn data_byte(&mut self, b: u8) -> Result<(), ParseError> {
        if self.allow_done && ((!self.data_started && b == b'[') || !self.sentinel.is_empty()) {
            let index = self.sentinel.len();
            if b"[DONE]".get(index) != Some(&b) {
                return Err(ParseError::Syntax);
            }
            self.sentinel.push(b);
        } else {
            self.json.byte(b)?;
        }
        self.data_started = true;
        Ok(())
    }
    fn line(&mut self) -> Result<Option<Event>, ParseError> {
        if self.line_nonempty && !self.colon {
            if self.field == b"data" {
                if self.has_data {
                    self.data_byte(b'\n')?;
                }
                self.has_data = true;
            }
            if self.field == b"event" {
                self.name.clear();
            }
        }
        let event = if !self.line_nonempty {
            let fields = self.fields.iter().map(String::as_str).collect::<Vec<_>>();
            let shapes = self
                .string_shapes
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>();
            let old = std::mem::replace(
                &mut self.json,
                JsonDecoder::with_string_shapes(&fields, &shapes),
            );
            self.json.reserved = self.reserved;
            self.peak = self.peak.max(old.retained_peak() + 4096);
            let result = if self.has_data {
                Some(Event {
                    wire_bytes: self.event_bytes,
                    name: std::mem::take(&mut self.name),
                    values: if self.sentinel.is_empty() {
                        old.finish()?
                    } else if self.sentinel == b"[DONE]" {
                        BTreeMap::from([("$done".into(), Scalar::Bool(true))])
                    } else {
                        return Err(ParseError::Syntax);
                    },
                })
            } else {
                None
            };
            self.has_data = false;
            self.data_started = false;
            self.sentinel.clear();
            self.name.clear();
            self.event_bytes = 0;
            result
        } else {
            if self.colon && self.field == b"event" {
                self.name = String::from_utf8(std::mem::take(&mut self.value))
                    .map_err(|_| ParseError::Syntax)?;
            }
            None
        };
        self.line_nonempty = false;
        self.colon = false;
        self.space = false;
        self.data = false;
        self.field.clear();
        self.value.clear();
        Ok(event)
    }
    pub fn byte(&mut self, b: u8) -> Result<Option<Event>, ParseError> {
        self.total += 1;
        if self.total > TOTAL_BYTES {
            return Err(ParseError::Limit);
        }
        self.utf8.byte(b)?;
        if self.total == 1 && b == 0xef {
            self.bom_left = 2;
            return Ok(None);
        }
        if self.bom_left > 0 {
            let expected = if self.bom_left == 2 { 0xbb } else { 0xbf };
            if b != expected {
                return Err(ParseError::Syntax);
            }
            self.bom_left -= 1;
            return Ok(None);
        }
        if self.cr {
            self.cr = false;
            let boundary = self.boundary_after_cr.take();
            if b == b'\n' {
                if let Some(previous) = boundary {
                    if previous >= EVENT_BYTES {
                        return Err(ParseError::Limit);
                    }
                } else {
                    self.event_bytes += 1;
                    if self.event_bytes > EVENT_BYTES {
                        return Err(ParseError::Limit);
                    }
                }
                return Ok(None);
            }
        }
        self.event_bytes += 1;
        if self.event_bytes > EVENT_BYTES {
            return Err(ParseError::Limit);
        }
        if b == b'\r' || b == b'\n' {
            self.cr = b == b'\r';
            if self.cr && !self.line_nonempty {
                self.boundary_after_cr = Some(self.event_bytes);
            }
            return self.line();
        }
        self.line_nonempty = true;
        if !self.colon {
            if b == b':' {
                self.colon = true;
                self.space = true;
                self.data = self.field == b"data";
                if self.data {
                    if self.has_data {
                        self.data_byte(b'\n')?;
                    }
                    self.has_data = true;
                }
            } else if self.field.len() < 32 {
                self.field.push(b);
            }
            return Ok(None);
        }
        if self.space {
            self.space = false;
            if b == b' ' {
                return Ok(None);
            }
        }
        if self.data {
            self.data_byte(b)?;
        } else if self.field == b"event" {
            if self.value.len() >= 128 {
                return Err(ParseError::Limit);
            }
            self.value.push(b);
        }
        Ok(None)
    }
    pub async fn feed(
        &mut self,
        bytes: &[u8],
        mut event: impl FnMut(Event) -> Result<(), ParseError>,
    ) -> Result<(), ParseError> {
        for part in bytes.chunks(64 * 1024) {
            for &b in part {
                if let Some(value) = self.byte(b)? {
                    event(value)?;
                }
            }
            tokio::task::yield_now().await;
        }
        Ok(())
    }
    pub fn reserve_retained(&mut self, bytes: usize) {
        assert!(bytes <= 4096);
        self.reserved = bytes;
        self.json.reserved = bytes;
    }
    /// 검증된 실패를 뜻하지 않는다. 미완전 오류의 크기/문법 실패도 옛 성공 후보를 무효화한다.
    pub fn native_error_in_progress(&self) -> bool {
        self.name == "error"
            || self.json.values.get("type").and_then(Scalar::text) == Some("error")
            || self.json.values.contains_key("error")
    }
    pub fn optional_lf_fits(&self) -> bool {
        self.boundary_after_cr.is_none_or(|n| n < EVENT_BYTES) && self.total < TOTAL_BYTES
    }
    pub fn boundary_after_cr(&self) -> bool {
        self.boundary_after_cr.is_some()
    }
    pub fn finish(&self) -> Result<(), ParseError> {
        if self.has_data || self.line_nonempty || self.utf8.left != 0 {
            Err(ParseError::Syntax)
        } else {
            Ok(())
        }
    }
    pub fn retained_peak(&self) -> usize {
        self.peak.max(self.json.retained_peak() + 4096)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn large_completed_json_is_skipped_incrementally() {
        let mut parser =
            SseDecoder::new(&["type", "response.model", "response.usage.input_tokens"]);
        let mut events = Vec::new();
        let head=b"event: response.completed\r\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"text\":\"";
        for &b in head {
            if let Some(e) = parser.byte(b).unwrap() {
                events.push(e);
            }
        }
        let chunk = "합성\\\"\\uD83D\\uDE00".repeat(512); // UTF8/escape/surrogate 경계가 network chunk와 독립이다.
        for _ in 0..1024 {
            for &b in chunk.as_bytes() {
                assert!(parser.byte(b).unwrap().is_none());
            }
            tokio::task::yield_now().await;
        }
        for &b in
            b"\"}],\"model\":\"synthetic-model\",\r\ndata: \"usage\":{\"input_tokens\":7}}}\r\n\r\n"
        {
            if let Some(e) = parser.byte(b).unwrap() {
                events.push(e);
            }
        }
        parser.finish().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].values["response.model"].text(),
            Some("synthetic-model")
        );
        assert_eq!(
            events[0].values["response.usage.input_tokens"].integer(),
            Some(7)
        );
        assert!(parser.retained_peak() <= RETAINED_BYTES);
        println!(
            "S3_SPIKE retained_peak={} event_payload_bytes={} full_value_allocations=0",
            parser.retained_peak(),
            chunk.len() * 1024
        );
    }
    #[test]
    fn malformed_or_missing_terminal_never_becomes_final() {
        for text in [
            "{\"usage\":7,\"skip\":\"\\uD800x\"}",
            "{\"usage\":7,\"skip\":[1,]}",
            "{\"usage\":7,\"\\u0075sage\":8}",
            "{\"usage\":7,\"skip\":01}",
            "{\"usage\":7}x",
            "{\"usage\":7,\"skip\":\"unfinished",
            "{\"usage\":7,\"skip\":truee}",
            "{\"usage\":7,\"skip\":1e}",
        ] {
            let mut p = JsonDecoder::new(&["usage"]);
            let result = text
                .bytes()
                .try_for_each(|b| p.byte(b))
                .and_then(|_| p.finish().map(|_| ()));
            assert!(result.is_err(), "{text}");
        }
        for bytes in [
            b"{\"skip\":\"\xc0\x80\"}".as_slice(),
            b"{\"skip\":\"\xed\xa0\x80\"}",
            b"{\"skip\":\"\xf4\x90\x80\x80\"}",
        ] {
            let mut p = JsonDecoder::new(&[]);
            assert!(bytes.iter().try_for_each(|&b| p.byte(b)).is_err());
        }
    }
    #[tokio::test]
    async fn event_and_total_limits_are_independent_of_parser_retained() {
        let head = b"data: {\"skip\":\"";
        let tail = b"\"}\n\n";
        let n = EVENT_BYTES - head.len() - tail.len();
        let mut p = SseDecoder::new(&[]);
        let mut events = 0;
        p.feed(head, |_| Ok(())).await.unwrap();
        let chunk = [b'x'; 16 * 1024];
        let mut left = n;
        while left > 0 {
            let count = left.min(chunk.len());
            p.feed(&chunk[..count], |_| Ok(())).await.unwrap();
            left -= count;
        }
        p.feed(tail, |_| {
            events += 1;
            Ok(())
        })
        .await
        .unwrap();
        p.finish().unwrap();
        assert_eq!(events, 1);
        assert!(p.retained_peak() <= RETAINED_BYTES);
        let mut over = SseDecoder::new(&[]);
        over.feed(head, |_| Ok(())).await.unwrap();
        let mut left = n + 1;
        while left > 0 {
            let count = left.min(chunk.len());
            over.feed(&chunk[..count], |_| Ok(())).await.unwrap();
            left -= count;
        }
        assert!(over.feed(tail, |_| Ok(())).await.is_err());
        let mut total = SseDecoder::new(&[]);
        for _ in 0..4 {
            total.feed(head, |_| Ok(())).await.unwrap();
            let mut left = n;
            while left > 0 {
                let count = left.min(chunk.len());
                total.feed(&chunk[..count], |_| Ok(())).await.unwrap();
                left -= count;
            }
            total.feed(tail, |_| Ok(())).await.unwrap();
        }
        assert!(total.byte(b':').is_err());
    }
    #[test]
    fn string_shapes_validate_syntax_without_retaining_or_coercing_values() {
        for (value, string) in [
            (r#""합성\uD83D\uDE00\\\"""#, true),
            ("1025", false),
            ("null", false),
            ("true", false),
            ("{}", false),
            ("[]", false),
        ] {
            let wire = format!("{{\"error\":{{\"message\":{value}}}}}");
            let mut parser = JsonDecoder::with_string_shapes(&[], &["error.message"]);
            for b in wire.bytes() {
                parser.byte(b).unwrap();
            }
            let values = parser.finish().unwrap();
            assert_eq!(
                values.get("error.message"),
                string.then_some(&Scalar::StringShape)
            );
        }
        for wire in [
            r#"{"error":{"message":"\uD800x"}}"#,
            r#"{"error":{"message":"x","message":"y"}}"#,
            r#"{"error":{"message":"unfinished"#,
        ] {
            let mut parser = JsonDecoder::with_string_shapes(&[], &["error.message"]);
            assert!(
                wire.bytes()
                    .try_for_each(|b| parser.byte(b))
                    .and_then(|_| parser.finish())
                    .is_err()
            );
        }
    }
    #[test]
    fn key_paths_cannot_be_forged_with_dot_names_or_escaped_duplicates() {
        let mut p = JsonDecoder::new(&["response.model"]);
        for b in br#"{"response.model":"wrong","response":{"model":"right"}}"# {
            p.byte(*b).unwrap();
        }
        assert_eq!(p.finish().unwrap()["response.model"].text(), Some("right"));
        let mut p = JsonDecoder::new(&["usage.**"]);
        for b in br#"{"usage":{"cache_creation.ephemeral_1h_input_tokens":1}}"# {
            p.byte(*b).unwrap();
        }
        assert!(
            p.finish()
                .unwrap()
                .contains_key("usage.cache_creation\\.ephemeral_1h_input_tokens")
        );
    }
}

#[cfg(test)]
mod error_cause_tests {
    use super::*;
    #[test]
    fn sse_reserves_supervisor_observation_across_event_decoder_replacement() {
        let mut decoder = SseDecoder::new(&["type"]);
        decoder.reserve_retained(512);
        for _ in 0..3 {
            for b in b"data: {\"type\":\"ping\"}\r\n\r\n" {
                decoder.byte(*b).unwrap();
            }
            assert_eq!(decoder.json.reserved, 512);
            assert!(decoder.retained_peak() + 512 <= RETAINED_BYTES);
        }
    }
    fn sse(wire: &[u8]) -> Result<(), ParseError> {
        let mut decoder = SseDecoder::new(&[]);
        for &byte in wire {
            decoder.byte(byte)?;
        }
        decoder.finish()
    }
    #[test]
    fn syntax_and_each_structural_budget_keep_typed_causes() {
        for wire in [
            b"data: {bad}\n\n".as_slice(),
            b"data: {\"x\":1,\"x\":2}\n\n",
            b"data: {\"x\":\"\xff\"}\n\n",
            b"data: {\"x\":\"\\uD800\"}\n\n",
            b"data: {\"x\":1",
        ] {
            assert_eq!(sse(wire), Err(ParseError::Syntax));
        }
        assert_eq!(
            sse(format!("event: {}\ndata: {{}}\n\n", "x".repeat(128)).as_bytes()),
            Ok(())
        );
        assert_eq!(
            sse(format!("event: {}\ndata: {{}}\n\n", "x".repeat(129)).as_bytes()),
            Err(ParseError::Limit)
        );
        for (depth, expected) in [(32, Ok(())), (33, Err(ParseError::Limit))] {
            let wire = format!("data: {}0{}\n\n", "[".repeat(depth), "]".repeat(depth));
            assert_eq!(sse(wire.as_bytes()), expected);
        }
        for (tokens, expected) in [(100_000, Ok(())), (100_001, Err(ParseError::Limit))] {
            let wire = format!(
                "data: [{}]\n\n",
                std::iter::repeat_n("null", tokens - 1)
                    .collect::<Vec<_>>()
                    .join(",")
            );
            assert_eq!(sse(wire.as_bytes()), expected);
        }
        for (size, expected) in [(1024, Ok(())), (1025, Err(ParseError::Limit))] {
            let mut decoder = JsonDecoder::new(&["model"]);
            let wire = format!("{{\"model\":\"{}\"}}", "x".repeat(size));
            let actual = wire
                .bytes()
                .try_for_each(|b| decoder.byte(b))
                .and_then(|_| decoder.finish().map(|_| ()));
            assert_eq!(actual, expected);
        }
        let mut decoder = JsonDecoder::new(&[]);
        let wire = format!(
            "{{{}}}",
            (0..128)
                .map(|i| format!("\"{i}{}\":null", "x".repeat(2048)))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert_eq!(
            wire.bytes().try_for_each(|b| decoder.byte(b)),
            Err(ParseError::Limit),
            "보관키 예산 초과는 문법 오류가 아님"
        );
    }
    #[test]
    fn event_and_total_exact_plus_one_keep_limit_cause() {
        let head = "data: {\"x\":\"";
        let tail = "\"}\n\n";
        for size in [EVENT_BYTES, EVENT_BYTES + 1] {
            let wire = format!("{head}{}{tail}", "x".repeat(size - head.len() - tail.len()));
            assert_eq!(
                sse(wire.as_bytes()),
                if size == EVENT_BYTES {
                    Ok(())
                } else {
                    Err(ParseError::Limit)
                }
            );
        }
        let mut decoder = SseDecoder::new(&[]);
        for _ in 0..TOTAL_BYTES / 4 {
            for byte in b":x\n\n" {
                decoder.byte(*byte).unwrap();
            }
        }
        assert!(decoder.finish().is_ok());
        assert!(matches!(decoder.byte(b'\n'), Err(ParseError::Limit)));
    }
}
