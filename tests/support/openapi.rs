//! 관리 API의 OpenAPI 문서(`api/management.openapi.json`, 결정 0014 규칙 4)를 읽고 응답을 문서와 맞춘다.
//!
//! 문서가 쓰는 JSON Schema 낱말은 아래 `KEYWORDS`와 `ANNOTATIONS`뿐이다. 검사기는 `KEYWORDS`만
//! 검사하고, `ANNOTATIONS`(설명, 형식, 길이·범위·패턴, 예시, 기본값)는 검사하지 않고 넘긴다.
//! 그 밖의 낱말을 만나면 실패한다. 검사하지 않는 낱말에 기대는 일이 없게 하려는 것이다.
use serde_json::{Map, Value};
use std::sync::OnceLock;

/// 검사기가 검사하는 낱말.
pub const KEYWORDS: &[&str] = &[
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "oneOf",
    "anyOf",
    "allOf",
    "$ref",
];
/// 문서에 써도 되지만 검사기가 검사하지 않는 낱말.
pub const ANNOTATIONS: &[&str] = &[
    "description",
    "title",
    "format",
    "examples",
    "default",
    "deprecated",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "pattern",
    "minItems",
    "maxItems",
];
/// 문서의 HTTP 메서드 낱말.
pub const METHODS: &[&str] = &[
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

pub struct Document {
    pub root: Value,
}

impl Document {
    /// 바이너리에 들어 있는 문서를 한 번만 읽는다.
    pub fn shared() -> &'static Document {
        static DOCUMENT: OnceLock<Document> = OnceLock::new();
        DOCUMENT.get_or_init(Document::load)
    }
    pub fn load() -> Self {
        Self {
            root: serde_json::from_str(m_gated::admin::OPENAPI).expect("OpenAPI 문서는 JSON이다"),
        }
    }
    pub fn paths(&self) -> &Map<String, Value> {
        self.root["paths"].as_object().expect("paths")
    }
    /// 문서의 모든 (경로 템플릿, 대문자 메서드, operation).
    pub fn operations(&self) -> Vec<(&str, String, &Value)> {
        let mut out = Vec::new();
        for (path, item) in self.paths() {
            for (method, operation) in item.as_object().expect("path item") {
                if METHODS.contains(&method.as_str()) {
                    out.push((path.as_str(), method.to_uppercase(), operation));
                }
            }
        }
        out
    }
    /// 실제 경로(조회 문자열 없이)에 맞는 경로 템플릿. `{name}` 자리는 비지 않은 한 마디와 맞는다.
    pub fn template(&self, path: &str) -> Option<&str> {
        let actual: Vec<&str> = path.split('/').collect();
        self.paths().keys().map(String::as_str).find(|template| {
            let parts: Vec<&str> = template.split('/').collect();
            parts.len() == actual.len()
                && parts.iter().zip(&actual).all(|(t, a)| {
                    (t.starts_with('{') && t.ends_with('}') && !a.is_empty()) || t == a
                })
        })
    }
    pub fn operation(&self, template: &str, method: &str) -> Option<&Value> {
        self.paths()
            .get(template)?
            .get(method.to_lowercase())
            .filter(|_| METHODS.contains(&method.to_lowercase().as_str()))
    }
    /// 어느 경로든 낼 수 있는 오류 코드.
    pub fn common_codes(&self, status: u16) -> Vec<&str> {
        self.root["x-common-errors"][status.to_string()]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }
    /// `#/components/schemas/<이름>`을 찾는다.
    pub fn resolve(&self, reference: &str) -> Result<&Value, String> {
        let name = reference
            .strip_prefix("#/components/schemas/")
            .ok_or_else(|| format!("지역 schema가 아닌 $ref: {reference}"))?;
        self.root["components"]["schemas"]
            .get(name)
            .ok_or_else(|| format!("없는 schema: {reference}"))
    }

    /// 값을 schema로 검사한다. 오류에는 값 안의 위치를 적는다.
    pub fn validate(&self, schema: &Value, value: &Value) -> Result<(), String> {
        self.check(schema, value, "$")
    }
    fn check(&self, schema: &Value, value: &Value, at: &str) -> Result<(), String> {
        let schema = match schema {
            Value::Bool(true) => return Ok(()),
            Value::Bool(false) => return Err(format!("{at}: 허용하지 않는 값")),
            Value::Object(o) => o,
            _ => return Err(format!("{at}: schema가 객체가 아니다")),
        };
        for key in schema.keys() {
            if !KEYWORDS.contains(&key.as_str()) && !ANNOTATIONS.contains(&key.as_str()) {
                return Err(format!("{at}: 검사기가 모르는 낱말 `{key}`"));
            }
        }
        if let Some(r) = schema.get("$ref") {
            let target = self.resolve(r.as_str().ok_or("$ref는 문자열이다")?)?;
            self.check(target, value, at)?;
        }
        if let Some(t) = schema.get("type") {
            let types: Vec<&str> = match t {
                Value::String(s) => vec![s.as_str()],
                Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
                _ => return Err(format!("{at}: type이 틀렸다")),
            };
            if !types.iter().any(|t| type_matches(t, value)) {
                return Err(format!("{at}: {types:?}가 아니다: {}", short(value)));
            }
        }
        if let Some(e) = schema.get("enum") {
            let options = e.as_array().ok_or("enum은 배열이다")?;
            if !options.contains(value) {
                return Err(format!(
                    "{at}: {options:?} 가운데 하나가 아니다: {}",
                    short(value)
                ));
            }
        }
        if let Some(c) = schema.get("const")
            && c != value
        {
            return Err(format!("{at}: {c}가 아니다: {}", short(value)));
        }
        if let Value::Object(fields) = value {
            let properties = schema.get("properties").and_then(Value::as_object);
            if let Some(required) = schema.get("required") {
                for name in required.as_array().ok_or("required는 배열이다")? {
                    let name = name.as_str().ok_or("required의 이름은 문자열이다")?;
                    if !fields.contains_key(name) {
                        return Err(format!("{at}: `{name}`이 없다"));
                    }
                }
            }
            for (name, field) in fields {
                let path = format!("{at}.{name}");
                match properties.and_then(|p| p.get(name)) {
                    Some(s) => self.check(s, field, &path)?,
                    None => match schema.get("additionalProperties") {
                        None => {}
                        Some(extra) => self.check(extra, field, &path).map_err(|e| {
                            if extra == &Value::Bool(false) {
                                format!("{at}: 문서에 없는 필드 `{name}`")
                            } else {
                                e
                            }
                        })?,
                    },
                }
            }
        }
        if let (Some(items), Value::Array(values)) = (schema.get("items"), value) {
            for (i, item) in values.iter().enumerate() {
                self.check(items, item, &format!("{at}[{i}]"))?;
            }
        }
        if let Some(all) = schema.get("allOf") {
            for s in all.as_array().ok_or("allOf는 배열이다")? {
                self.check(s, value, at)?;
            }
        }
        if let Some(any) = schema.get("anyOf") {
            let results: Vec<_> = any
                .as_array()
                .ok_or("anyOf는 배열이다")?
                .iter()
                .map(|s| self.check(s, value, at))
                .collect();
            if !results.iter().any(Result::is_ok) {
                return Err(format!("{at}: anyOf 가운데 맞는 것이 없다: {results:?}"));
            }
        }
        if let Some(one) = schema.get("oneOf") {
            let results: Vec<_> = one
                .as_array()
                .ok_or("oneOf는 배열이다")?
                .iter()
                .map(|s| self.check(s, value, at))
                .collect();
            let matched = results.iter().filter(|r| r.is_ok()).count();
            if matched != 1 {
                return Err(format!(
                    "{at}: oneOf 가운데 {matched}개가 맞는다(하나여야 한다): {results:?}"
                ));
            }
        }
        Ok(())
    }

    /// 응답 하나를 문서와 맞춘다. `template`은 문서의 경로 템플릿이고, 문서에 없는 경로면 그대로 준다.
    /// `body`는 비어 있으면 `None`이다. `HEAD` 응답은 본문이 없으므로 상태 코드만 본다.
    ///
    /// - 상태 코드는 그 operation이 선언했거나 `x-common-errors`에 있어야 한다.
    /// - 선언한 응답에 본문이 있으면 그 schema에 맞아야 하고, 없으면 본문이 비어야 한다.
    /// - 오류 코드는 그 상태의 `x-error-codes`나 `x-common-errors[상태]`에 있어야 한다.
    pub fn check_response(
        &self,
        method: &str,
        template: &str,
        status: u16,
        body: Option<&Value>,
    ) -> Result<(), String> {
        let declared = self
            .operation(template, method)
            .and_then(|o| o["responses"].get(status.to_string()));
        let common = self.common_codes(status);
        if method == "HEAD" {
            return if declared.is_some() || !common.is_empty() {
                Ok(())
            } else {
                Err(format!("HEAD {template}: 문서에 없는 상태 {status}"))
            };
        }
        let at = format!("{method} {template} {status}");
        let Some(response) = declared else {
            if common.is_empty() {
                return Err(format!("{at}: 문서에 없는 상태"));
            }
            let body = body.ok_or_else(|| format!("{at}: 오류 본문이 없다"))?;
            self.validate(self.resolve("#/components/schemas/Error")?, body)
                .map_err(|e| format!("{at}: {e}"))?;
            let code = body["error"]["code"].as_str().unwrap_or_default();
            return if common.contains(&code) {
                Ok(())
            } else {
                Err(format!("{at}: `{code}`가 x-common-errors에 없다"))
            };
        };
        match (response["content"]["application/json"].get("schema"), body) {
            (Some(schema), Some(body)) => {
                self.validate(schema, body)
                    .map_err(|e| format!("{at}: {e}"))?;
            }
            (Some(_), None) => return Err(format!("{at}: 문서에는 본문이 있는데 응답은 비었다")),
            (None, Some(body)) => {
                return Err(format!(
                    "{at}: 문서에는 본문이 없는데 응답에 있다: {}",
                    short(body)
                ));
            }
            (None, None) => {}
        }
        if let Some(codes) = response.get("x-error-codes") {
            let code = body
                .and_then(|b| b["error"]["code"].as_str())
                .unwrap_or_default();
            let listed = codes
                .as_array()
                .is_some_and(|a| a.iter().any(|c| c == code));
            if !listed && !common.contains(&code) {
                return Err(format!(
                    "{at}: `{code}`가 x-error-codes와 x-common-errors에 없다"
                ));
            }
        }
        Ok(())
    }

    /// 실제 경로(조회 문자열이 있어도 된다)로 응답을 맞춘다. 경로 템플릿은 문서에서 찾는다.
    pub fn check_path(
        &self,
        method: &str,
        path: &str,
        status: u16,
        body: Option<&Value>,
    ) -> Result<(), String> {
        let path = path.split('?').next().unwrap_or_default();
        let template = self.template(path).unwrap_or(path);
        self.check_response(method, template, status, body)
    }
}

fn type_matches(t: &str, value: &Value) -> bool {
    match t {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        _ => false,
    }
}

fn short(value: &Value) -> String {
    let s = value.to_string();
    if s.chars().count() > 200 {
        format!("{}…", s.chars().take(200).collect::<String>())
    } else {
        s
    }
}

/// 문서 안의 모든 schema를 (위치, schema)로 모은다. 하위 schema까지 포함한다.
pub fn all_schemas(root: &Value) -> Vec<(String, &Value)> {
    let mut out = Vec::new();
    if let Some(schemas) = root["components"]["schemas"].as_object() {
        for (name, s) in schemas {
            collect(s, format!("#/components/schemas/{name}"), &mut out);
        }
    }
    if let Some(paths) = root["paths"].as_object() {
        for (path, item) in paths {
            for (method, operation) in item.as_object().into_iter().flatten() {
                let at = format!("{method} {path}");
                for (i, p) in operation["parameters"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    if let Some(s) = p.get("schema") {
                        collect(s, format!("{at} parameters[{i}]"), &mut out);
                    }
                }
                for (media, c) in operation["requestBody"]["content"]
                    .as_object()
                    .into_iter()
                    .flatten()
                {
                    if let Some(s) = c.get("schema") {
                        collect(s, format!("{at} requestBody {media}"), &mut out);
                    }
                }
                for (status, r) in operation["responses"].as_object().into_iter().flatten() {
                    for (media, c) in r["content"].as_object().into_iter().flatten() {
                        if let Some(s) = c.get("schema") {
                            collect(s, format!("{at} {status} {media}"), &mut out);
                        }
                    }
                }
            }
        }
    }
    out
}
fn collect<'a>(schema: &'a Value, at: String, out: &mut Vec<(String, &'a Value)>) {
    out.push((at.clone(), schema));
    let Some(o) = schema.as_object() else {
        return;
    };
    if let Some(p) = o.get("properties").and_then(Value::as_object) {
        for (name, s) in p {
            collect(s, format!("{at}.properties.{name}"), out);
        }
    }
    if let Some(s) = o.get("additionalProperties").filter(|s| s.is_object()) {
        collect(s, format!("{at}.additionalProperties"), out);
    }
    if let Some(s) = o.get("items") {
        collect(s, format!("{at}.items"), out);
    }
    for key in ["oneOf", "anyOf", "allOf"] {
        for (i, s) in o
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            collect(s, format!("{at}.{key}[{i}]"), out);
        }
    }
}
