//! OpenAPI 문서가 경로 표와 문서 규칙에 맞는지 본다(결정 0014 규칙 4). DB가 없어도 돈다.
#[path = "support/openapi.rs"]
#[allow(dead_code)]
mod openapi;
use openapi::{ANNOTATIONS, Document, KEYWORDS, all_schemas};
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// 경로 표의 "GET, POST" 같은 허용 메서드를 낱낱으로 나눈다.
fn methods(allow: &str) -> impl Iterator<Item = &str> {
    allow.split(',').map(str::trim)
}

#[test]
fn document_has_exactly_the_routes_of_the_route_table() {
    let doc = Document::load();
    assert_eq!(doc.root["openapi"], "3.1.0");
    let documented: BTreeSet<(String, String)> = doc
        .operations()
        .into_iter()
        .map(|(path, method, _)| (path.to_owned(), method))
        .collect();
    let routes: BTreeSet<(String, String)> = m_gated::admin::route_table(true)
        .into_iter()
        .flat_map(|(path, allow)| methods(allow).map(move |m| (path.to_owned(), m.to_owned())))
        .collect();
    let missing: Vec<_> = routes.difference(&documented).collect();
    let extra: Vec<_> = documented.difference(&routes).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "문서에 없는 경로: {missing:?}\n경로 표에 없는 경로: {extra:?}"
    );
    // 경로 항목에는 메서드만 둔다.
    for (path, item) in doc.paths() {
        for key in item.as_object().unwrap().keys() {
            assert!(openapi::METHODS.contains(&key.as_str()), "{path}: {key}");
        }
    }
}

#[test]
fn operation_extensions_follow_the_rules() {
    let doc = Document::load();
    let password_only: BTreeSet<(&str, &str)> = m_gated::admin::ROUTES
        .iter()
        .filter(|(_, _, only)| *only)
        .flat_map(|(path, allow, _)| methods(allow).map(move |m| (*path, m)))
        .collect();
    assert!(!password_only.is_empty());
    let mut ids = BTreeSet::new();
    for (path, method, op) in doc.operations() {
        let at = format!("{method} {path}");
        for key in ["operationId", "summary", "description"] {
            assert!(
                op[key].as_str().is_some_and(|s| !s.is_empty()),
                "{at}: {key}"
            );
        }
        assert!(
            ids.insert(op["operationId"].as_str().unwrap().to_owned()),
            "{at}: operationId가 겹친다"
        );
        let write = method != "GET";
        assert_eq!(
            op["x-role"],
            if path.starts_with("/api/admin/") {
                "admin"
            } else {
                "user"
            },
            "{at}: x-role"
        );
        // 관리 토큰을 받지 않는 경로(결정 0014 규칙 2).
        let no_token = matches!(
            path,
            "/api/csrf"
                | "/api/me/password"
                | "/api/admin/management-tokens"
                | "/api/admin/management-tokens/{id}"
        );
        let scope = if no_token {
            Value::Null
        } else if write {
            json!("write")
        } else {
            json!("read")
        };
        assert_eq!(op.get("x-token-scope"), Some(&scope), "{at}: x-token-scope");
        // 쓰기는 명령 ID가 필요하다. 비밀번호 바꾸기만 명령 기록을 쓰지 않아 명령 ID가 없다.
        let command = write && path != "/api/me/password";
        assert_eq!(op["x-command-id"], json!(command), "{at}: x-command-id");
        let password = password_only.contains(&(path, method.as_str()));
        assert_eq!(
            op.get("x-auth-mode"),
            password.then(|| json!("password")).as_ref(),
            "{at}: x-auth-mode"
        );
        // 명령 ID가 필요한 쓰기는 명령 ID 헤더(필수)를, 모든 쓰기는 CSRF 헤더(선택)를 적는다.
        let header = |name: &str| {
            op["parameters"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|p| p["in"] == "header" && p["name"].as_str() == Some(name))
        };
        assert_eq!(
            header("X-Gateway-Command-ID").map(|p| &p["required"]),
            command.then_some(json!(true)).as_ref(),
            "{at}"
        );
        assert_eq!(
            header("X-CSRF-Token").map(|p| &p["required"]),
            write.then_some(json!(false)).as_ref(),
            "{at}"
        );
        // 경로의 `{이름}`마다 필수 경로 변수가 있다.
        let names: BTreeSet<&str> = path
            .split('/')
            .filter_map(|s| s.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
            .collect();
        let params: BTreeSet<&str> = op["parameters"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p["in"] == "path")
            .inspect(|p| assert_eq!(p["required"], true, "{at}"))
            .filter_map(|p| p["name"].as_str())
            .collect();
        assert_eq!(names, params, "{at}: 경로 변수");
        for p in op["parameters"].as_array().into_iter().flatten() {
            assert!(p.get("schema").is_some(), "{at}: {} schema", p["name"]);
        }
    }
}

#[test]
fn every_operation_has_one_defined_tag() {
    let doc = Document::load();
    let defined: Vec<&str> = doc.root["tags"]
        .as_array()
        .expect("tags")
        .iter()
        .map(|t| {
            assert!(
                t["description"].as_str().is_some_and(|s| !s.is_empty()),
                "{t}"
            );
            t["name"].as_str().unwrap()
        })
        .collect();
    assert_eq!(
        defined.iter().collect::<BTreeSet<_>>().len(),
        defined.len(),
        "태그 이름이 겹친다"
    );
    let mut used = BTreeSet::new();
    for (path, method, op) in doc.operations() {
        let tags = op["tags"].as_array().expect("tags");
        assert_eq!(tags.len(), 1, "{method} {path}");
        let tag = tags[0].as_str().unwrap();
        assert!(defined.contains(&tag), "{method} {path}: {tag}");
        used.insert(tag);
    }
    assert_eq!(used.len(), defined.len(), "쓰지 않는 태그가 있다");
}

#[test]
fn responses_follow_the_error_convention() {
    let doc = Document::load();
    let error = json!({"$ref": "#/components/schemas/Error"});
    for (path, method, op) in doc.operations() {
        let responses = op["responses"].as_object().expect("responses");
        assert!(
            responses.keys().any(|s| s.starts_with('2')),
            "{method} {path}: 성공 응답이 없다"
        );
        for (status, r) in responses {
            let at = format!("{method} {path} {status}");
            assert!(r["description"].as_str().is_some(), "{at}");
            let code: u16 = status.parse().expect("상태 코드");
            if code >= 400 {
                let codes = r["x-error-codes"].as_array().expect("x-error-codes");
                assert!(!codes.is_empty(), "{at}");
                assert_eq!(
                    r.as_object().unwrap().len(),
                    3,
                    "{at}: description, x-error-codes, content"
                );
                assert_eq!(
                    r["content"],
                    json!({"application/json": {"schema": error}}),
                    "{at}"
                );
            } else {
                assert!(r.get("x-error-codes").is_none(), "{at}");
            }
        }
    }
    // Error는 네 필드가 모두 필수이고 다른 필드가 없다.
    let e = doc.resolve("#/components/schemas/Error").unwrap();
    assert_eq!(e["additionalProperties"], false);
    assert_eq!(e["required"], json!(["error"]));
    let inner = &e["properties"]["error"];
    assert_eq!(inner["additionalProperties"], false);
    assert_eq!(
        inner["required"],
        json!(["code", "message", "request_id", "details"])
    );
    let ok = json!({"error":{"code":"x","message":"m","request_id":"r","details":{}}});
    doc.validate(e, &ok).unwrap();
    assert!(doc.validate(e, &json!({"error":{"code":"x"}})).is_err());
    // 공통 오류는 상태 코드마다 코드 목록이다.
    for (status, codes) in doc.root["x-common-errors"]
        .as_object()
        .expect("x-common-errors")
    {
        assert!(status.parse::<u16>().is_ok_and(|s| s >= 400), "{status}");
        assert!(
            codes
                .as_array()
                .is_some_and(|a| !a.is_empty() && a.iter().all(Value::is_string))
        );
    }
}

#[test]
fn every_ref_resolves_and_only_known_keywords_are_used() {
    let doc = Document::load();
    let schemas = all_schemas(&doc.root);
    assert!(schemas.len() > 100);
    for (at, schema) in schemas {
        let Some(o) = schema.as_object() else {
            assert!(schema.is_boolean(), "{at}: schema가 객체가 아니다");
            continue;
        };
        for key in o.keys() {
            assert!(
                KEYWORDS.contains(&key.as_str()) || ANNOTATIONS.contains(&key.as_str()),
                "{at}: 쓰지 않기로 한 낱말 `{key}`"
            );
        }
        if let Some(r) = o.get("$ref") {
            doc.resolve(r.as_str().expect("$ref"))
                .unwrap_or_else(|e| panic!("{at}: {e}"));
        }
        if let Some(t) = o.get("type") {
            let types: Vec<&Value> = match t {
                Value::Array(a) => a.iter().collect(),
                one => vec![one],
            };
            for t in types {
                assert!(
                    [
                        "null", "boolean", "object", "array", "string", "number", "integer"
                    ]
                    .contains(&t.as_str().unwrap_or_default()),
                    "{at}: type {t}"
                );
            }
        }
        if let Some(r) = o.get("required") {
            let props = o.get("properties").and_then(Value::as_object);
            for name in r.as_array().expect("required") {
                assert!(
                    props.is_some_and(|p| p.contains_key(name.as_str().unwrap())),
                    "{at}: required `{name}`에 properties가 없다"
                );
            }
        }
    }
    // components.schemas의 이름은 모두 어딘가에서 쓴다.
    let text = doc.root.to_string();
    for name in doc.root["components"]["schemas"]
        .as_object()
        .unwrap()
        .keys()
    {
        assert!(
            text.contains(&format!("\"#/components/schemas/{name}\"")),
            "쓰지 않는 schema: {name}"
        );
    }
}

#[test]
fn security_schemes_and_top_level_security_are_as_agreed() {
    let doc = Document::load();
    assert_eq!(
        doc.root["security"],
        json!([{"bearer": []}, {"cookie": []}])
    );
    let schemes = &doc.root["components"]["securitySchemes"];
    assert_eq!(schemes["bearer"]["type"], "http");
    assert_eq!(schemes["bearer"]["scheme"], "bearer");
    assert_eq!(schemes["cookie"]["type"], "apiKey");
    assert_eq!(schemes["cookie"]["in"], "cookie");
    let description = schemes["cookie"]["description"].as_str().unwrap();
    assert!(description.contains("mgated_access") && description.contains("mgated_session"));
    let info = doc.root["info"]["description"].as_str().unwrap();
    for word in [
        "Authorization: Bearer",
        "mgt_",
        "X-CSRF-Token",
        "GET /api/csrf",
        "X-Gateway-Command-ID",
        "x-gateway-command-replayed: true",
        "request_id",
    ] {
        assert!(info.contains(word), "info.description에 {word}가 없다");
    }
}

/// 검사기가 낱말마다 제대로 판정하는지 본다.
#[test]
fn validator_checks_the_keyword_subset() {
    let doc = Document::load();
    let check = |schema: Value, value: Value| doc.validate(&schema, &value);
    assert!(check(json!({"type":"string"}), json!("a")).is_ok());
    assert!(check(json!({"type":"string"}), json!(1)).is_err());
    assert!(check(json!({"type":["string","null"]}), json!(null)).is_ok());
    assert!(check(json!({"type":"integer"}), json!(1.5)).is_err());
    assert!(check(json!({"type":"integer"}), json!(-3)).is_ok());
    assert!(check(json!({"enum":["a","b"]}), json!("c")).is_err());
    assert!(check(json!({"const":true}), json!(false)).is_err());
    let o = json!({"type":"object","properties":{"a":{"type":"string"}},"required":["a"],"additionalProperties":false});
    assert!(check(o.clone(), json!({"a":"x"})).is_ok());
    assert!(check(o.clone(), json!({})).is_err());
    assert!(check(o.clone(), json!({"a":"x","b":1})).is_err());
    assert!(check(o, json!({"a":1})).is_err());
    let map = json!({"type":"object","additionalProperties":{"type":"string"}});
    assert!(check(map.clone(), json!({"x":"y"})).is_ok());
    assert!(check(map, json!({"x":1})).is_err());
    assert!(
        check(
            json!({"type":"array","items":{"type":"integer"}}),
            json!([1, "2"])
        )
        .is_err()
    );
    let one = json!({"oneOf":[{"type":"string"},{"type":"string","enum":["a"]}]});
    assert!(
        check(one.clone(), json!("a")).is_err(),
        "둘 다 맞으면 oneOf가 아니다"
    );
    assert!(check(one, json!("b")).is_ok());
    assert!(
        check(
            json!({"anyOf":[{"type":"string"},{"type":"null"}]}),
            json!(1)
        )
        .is_err()
    );
    assert!(
        check(
            json!({"allOf":[{"type":"string"},{"enum":["a"]}]}),
            json!("b")
        )
        .is_err()
    );
    assert!(check(json!({"$ref":"#/components/schemas/Error"}), json!({})).is_err());
    assert!(check(json!({"$ref":"#/components/schemas/Nope"}), json!({})).is_err());
    // 주석 낱말은 검사하지 않는다.
    assert!(
        check(
            json!({"type":"string","maxLength":1,"pattern":"^x$"}),
            json!("yy")
        )
        .is_ok()
    );
    // 모르는 낱말은 실패한다.
    assert!(check(json!({"type":"string","minProperties":1}), json!("a")).is_err());
    assert!(check(json!({"not":{"type":"string"}}), json!(1)).is_err());
}

/// 응답 검사 도우미가 경로를 찾고, 선언하지 않은 상태와 코드를 거절하는지 본다.
#[test]
fn response_check_matches_paths_and_codes() {
    let doc = Document::load();
    let id = "00000000-0000-4000-8000-000000000001";
    assert_eq!(
        doc.template(&format!("/api/keys/{id}")),
        Some("/api/keys/{id}")
    );
    assert_eq!(
        doc.template("/api/admin/provider-credentials/anthropic"),
        Some("/api/admin/provider-credentials/{provider}")
    );
    assert_eq!(
        doc.template("/api/admin/requests/models"),
        Some("/api/admin/requests/models")
    );
    assert_eq!(doc.template(&format!("/api/keys/{id}/extra")), None);
    let error =
        |code: &str| json!({"error":{"code":code,"message":"m","request_id":id,"details":{}}});
    let path = format!("/api/keys/{id}?expected_version=1");
    assert!(doc.check_path("DELETE", &path, 204, None).is_ok());
    assert!(
        doc.check_path("DELETE", &path, 204, Some(&json!({})))
            .is_err()
    );
    assert!(
        doc.check_path("DELETE", &path, 404, Some(&error("resource_not_found")))
            .is_ok()
    );
    assert!(
        doc.check_path("DELETE", &path, 404, Some(&error("route_not_found")))
            .is_ok()
    );
    assert!(
        doc.check_path("DELETE", &path, 404, Some(&error("access_exists")))
            .is_err()
    );
    assert!(
        doc.check_path("DELETE", &path, 418, Some(&error("teapot")))
            .is_err()
    );
    assert!(
        doc.check_path("OPTIONS", &path, 405, Some(&error("method_not_allowed")))
            .is_ok()
    );
    assert!(doc.check_path("OPTIONS", &path, 405, None).is_err());
    assert!(doc.check_path("HEAD", &path, 405, None).is_ok());
    assert!(
        doc.check_path("GET", "/api/nope", 404, Some(&error("route_not_found")))
            .is_ok()
    );
    assert!(
        doc.check_path("GET", "/api/nope", 400, Some(&error("invalid_request")))
            .is_err()
    );
    assert!(
        doc.check_path(
            "GET",
            "/api/csrf",
            200,
            Some(&json!({"token":"t","expires_at":null}))
        )
        .is_ok()
    );
    assert!(
        doc.check_path("GET", "/api/csrf", 200, Some(&json!({"token":"t"})))
            .is_err()
    );
}
