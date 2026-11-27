//! DeepSeek WASM 어댑터(결정 0009)와 OpenAI WASM 어댑터(결정 0013). 먼저 `scripts/build-adapter.sh`로
//! 컴포넌트를 빌드한다.
//!
//! ```sh
//! scripts/build-adapter.sh && cargo test --locked --test wasm_adapter -- --ignored
//! ```
//!
//! WASM 어댑터 설치는 프로세스 전체에 남는다. 그래서 설치하는 시험은 하나뿐이고, 나머지는
//! 설치하지 않는 `wasm::load_with`를 쓴다.
mod support;

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use m_gated::{
    config::{self, ConfigInputs, ModelProfile, ProviderId, StartupError},
    providers::{
        port::{self, Api, ApiError, Provider},
        transport::UpstreamError,
        wasm::{self, Limits},
    },
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

fn component_named(name: &str) -> (String, [u8; 32]) {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("target/adapters/{name}.wasm"));
    let bytes = std::fs::read(&path).expect("scripts/build-adapter.sh로 컴포넌트를 먼저 빌드한다");
    (path.to_string_lossy().into(), Sha256::digest(&bytes).into())
}
fn component() -> (String, [u8; 32]) {
    component_named("deepseek")
}
fn load(limits: Limits) -> &'static dyn Provider {
    let (path, sha) = component();
    wasm::load_with(ProviderId::Deepseek, &path, sha, limits).unwrap()
}
fn load_openai() -> &'static dyn Provider {
    let (path, sha) = component_named("openai");
    wasm::load_with(ProviderId::Openai, &path, sha, Limits::default()).unwrap()
}
fn chat(p: &'static dyn Provider) -> &'static dyn Api {
    p.apis()[0]
}
fn profile(features: &[&str]) -> ModelProfile {
    serde_json::from_value(json!({"model":"synthetic-deepseek","api":"deepseek_chat","profile_version":"deepseek-v1","pricing_version":"deepseek-price-v1","fixture_version":"deepseek-native-v1","actual_models":["synthetic-deepseek"],"features":features,"max_input_tokens":4096,"max_output_tokens":1024,"max_body_bytes":8388608,"max_response_bytes":16777216,"rates":{"input":"1.000000000000","output":"2.000000000000","cache_read":"0.500000000000"}})).unwrap()
}
fn usage() -> Value {
    json!({"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_cache_hit_tokens":40,"prompt_cache_miss_tokens":60,"completion_tokens_details":{"reasoning_tokens":10}})
}
fn chunk(end: bool) -> Value {
    json!({"id":"ds1","object":"chat.completion.chunk","model":"synthetic-deepseek","choices":[{"index":0,"delta":{"role":"assistant","content":"x"},"finish_reason":if end{Some("stop")}else{None}}],"usage":if end{usage()}else{Value::Null}})
}
fn completion() -> Value {
    json!({"id":"ds1","object":"chat.completion","model":"synthetic-deepseek","choices":[{"index":0,"message":{"role":"assistant","content":"x"},"finish_reason":"stop"}],"usage":usage()})
}
fn wire(v: &Value) -> String {
    format!("data: {v}\n\n")
}
/// 스트림 하나를 끝까지 읽은 결과. event마다의 결과와 끝난 뒤의 값이다.
type StreamRun = (
    Vec<Result<port::Step, UpstreamError>>,
    Result<(Option<m_gated::ledger::finance::FinalUsage>, bool), UpstreamError>,
    bool,
);
fn run_stream(api: &dyn Api, p: &ModelProfile, raw: &str) -> StreamRun {
    let mut reader = api.stream(p).unwrap();
    let mut decoder = reader.decoder();
    let mut steps = Vec::new();
    for b in raw.bytes() {
        match decoder.byte(b) {
            Ok(Some(e)) => steps.push(reader.accept(e)),
            Ok(None) => {}
            Err(_) => break,
        }
    }
    (steps, reader.close(), reader.provider_error())
}

#[test]
#[ignore = "scripts/build-adapter.sh가 만든 컴포넌트가 필요하다"]
fn facts_match_native_adapter() {
    let wasm = load(Limits::default());
    let native = port::native(ProviderId::Deepseek);
    assert_eq!(wasm.id(), native.id());
    assert_eq!(wasm.origin(), native.origin());
    assert_eq!(wasm.verify_path(), native.verify_path());
    assert_eq!(wasm.request_id_headers(), native.request_id_headers());
    assert_eq!(wasm.response_headers(), native.response_headers());
    assert_eq!(wasm.models_dev_name(), native.models_dev_name());
    for key in ["sk-1", "sk-ant-1", "key", ""] {
        assert_eq!(wasm.accepts_key(key), native.accepts_key(key), "{key}");
    }
    assert_eq!(wasm.apis().len(), native.apis().len());
    let (w, n) = (chat(wasm), chat(native));
    assert_eq!(w.catalog(), n.catalog());
    let p = profile(&[]);
    assert_eq!(w.kind(), n.kind());
    assert_eq!(w.routes(), n.routes());
    assert_eq!(w.upstream_path(), n.upstream_path());
    assert_eq!(w.error_format(), n.error_format());
    assert_eq!(w.token_rule(&p), n.token_rule(&p));
    assert_eq!(w.usage_categories(), n.usage_categories());
    assert_eq!(w.response_limit(&p), n.response_limit(&p));
    assert_eq!(wasm.check_profile(&p), native.check_profile(&p));
    let mut no_cache_price = p.clone();
    no_cache_price.rates.remove("cache_read");
    assert_eq!(
        wasm.check_profile(&no_cache_price),
        Err(StartupError::InvalidPrice)
    );
}

#[tokio::test]
#[ignore = "scripts/build-adapter.sh가 만든 컴포넌트가 필요하다"]
async fn requests_and_responses_match_native_adapter() {
    let (w, n) = (
        chat(load(Limits::default())),
        chat(port::native(ProviderId::Deepseek)),
    );
    let plain = profile(&[]);
    let full = profile(&["thinking", "client_tools", "json_schema"]);
    let base = json!({"model":"synthetic-deepseek","messages":[{"role":"user","content":"x"}],"max_tokens":100,"thinking":{"type":"disabled"}});
    let mut bodies = vec![base.to_string(), "{".into(), "[]".into()];
    for (key, value) in [
        ("stream", json!(true)),
        ("stream_options", json!({"include_usage":true})),
        ("model", json!("other")),
        ("max_tokens", json!(5000)),
        ("unknown", json!(1)),
        (
            "tools",
            json!([{"type":"function","function":{"name":"f","parameters":{}}}]),
        ),
        ("thinking", json!({"type":"enabled"})),
        ("messages", json!([])),
    ] {
        let mut b = base.clone();
        b[key] = value;
        bodies.push(b.to_string());
    }
    for p in [&plain, &full] {
        for body in &bodies {
            let (a, b) = (
                w.validate(body.as_bytes(), p).await,
                n.validate(body.as_bytes(), p).await,
            );
            assert_eq!(a.as_ref().err(), b.as_ref().err(), "{body}");
            if let (Ok(a), Ok(b)) = (a, b) {
                assert_eq!(
                    (a.model, a.output_limit, a.stream),
                    (b.model, b.output_limit, b.stream)
                );
            }
        }
    }

    let header = |pairs: &[(&'static str, &'static str)]| {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_static(v));
        }
        h
    };
    for h in [
        header(&[("content-type", "application/json"), ("x-other", "1")]),
        header(&[("content-type", "text/plain")]),
        header(&[]),
        header(&[
            ("content-type", "application/json"),
            ("connection", "content-type"),
        ]),
    ] {
        assert_eq!(
            w.approved_headers(&h, &plain),
            n.approved_headers(&h, &plain)
        );
    }

    let mut error_in_success = json!({"error":{"message":"x","type":"server_error"}});
    let large_error = json!({"error":{"message":"x".repeat(70000),"type":"server_error"}});
    let mut no_usage = completion();
    no_usage.as_object_mut().unwrap().remove("usage");
    for (status, body) in [
        (200, completion().to_string()),
        (200, no_usage.to_string()),
        (200, error_in_success.to_string()),
        (200, large_error.to_string()),
        (429, error_in_success.to_string()),
        (500, "{}".into()),
        (200, "not json".into()),
        (
            200,
            json!({"x":"y".repeat(4 * 1024 * 1024), "a": completion()}).to_string(),
        ),
    ] {
        let status = StatusCode::from_u16(status).unwrap();
        let (a, b) = (
            w.read(status, body.as_bytes(), b"", &plain).await,
            n.read(status, body.as_bytes(), b"", &plain).await,
        );
        assert_eq!(a.as_ref().err(), b.as_ref().err(), "{status}");
        if let (Ok(a), Ok(b)) = (a, b) {
            assert_eq!((a.usage, a.failed), (b.usage, b.failed), "{status}");
        }
    }
    error_in_success["error"]["code"] = json!("synthetic");

    let error_event = wire(&error_in_success);
    let mut tier = chunk(false);
    tier["service_tier"] = json!("default");
    for raw in [
        wire(&chunk(false)) + &wire(&chunk(true)) + "data: [DONE]\n\n",
        wire(&chunk(true)),
        wire(&chunk(true)) + &wire(&chunk(true)) + "data: [DONE]\n\n",
        error_event.clone(),
        error_event.clone() + "data: [DONE]\n\n",
        error_event + &wire(&chunk(true)),
        wire(&tier) + &wire(&chunk(true)) + "data: [DONE]\n\n",
        "event: other\ndata: {}\n\n".into(),
    ] {
        assert_eq!(
            run_stream(w, &plain, &raw),
            run_stream(n, &plain, &raw),
            "{raw}"
        );
    }
}

#[tokio::test]
#[ignore = "scripts/build-adapter.sh가 만든 컴포넌트가 필요하다"]
async fn limits_refuse_requests_and_make_responses_malformed() {
    let p = profile(&[]);
    let body = json!({"model":"synthetic-deepseek","messages":[{"role":"user","content":"x"}],"max_tokens":100,"thinking":{"type":"disabled"}}).to_string();
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    // (상한, 비동기 호출이 실패하는지, 동기 호출이 실패하는지)
    for (limits, async_fails, sync_fails) in [
        (
            Limits {
                async_fuel: 1,
                ..Limits::default()
            },
            true,
            false,
        ),
        (
            Limits {
                sync_fuel: 1,
                ..Limits::default()
            },
            false,
            true,
        ),
        (
            Limits {
                memory_bytes: 64 * 1024,
                ..Limits::default()
            },
            true,
            true,
        ),
    ] {
        let provider = load(limits);
        let api = chat(provider);
        assert_eq!(
            api.validate(body.as_bytes(), &p).await.err(),
            async_fails.then_some(ApiError::AdapterFailed),
            "{limits:?}"
        );
        assert_eq!(
            api.read(StatusCode::OK, completion().to_string().as_bytes(), b"", &p)
                .await
                .err(),
            async_fails.then_some(UpstreamError::Protocol),
            "{limits:?}"
        );
        assert_eq!(
            api.approved_headers(&headers, &p).err(),
            sync_fails.then_some(ApiError::AdapterFailed),
            "{limits:?}"
        );
        let (steps, closed, failed) =
            run_stream(api, &p, &(wire(&chunk(true)) + "data: [DONE]\n\n"));
        assert_eq!(
            steps.iter().all(|s| s == &Err(UpstreamError::Protocol)),
            sync_fails,
            "{limits:?}"
        );
        assert_eq!(closed.is_err(), sync_fails, "{limits:?}");
        assert!(!failed);
        assert_eq!(
            provider.check_profile(&p).err(),
            sync_fails.then_some(StartupError::InvalidAdapter),
            "{limits:?}"
        );
    }
}

#[test]
#[ignore = "scripts/build-adapter.sh가 만든 컴포넌트가 필요하다"]
fn files_with_wrong_hash_imports_or_exports_are_refused() {
    let (path, sha) = component();
    let mut wrong = sha;
    wrong[0] ^= 1;
    let refused = |path: &str, sha| {
        assert_eq!(
            wasm::load_with(ProviderId::Deepseek, path, sha, Limits::default()).err(),
            Some(StartupError::InvalidAdapter)
        );
    };
    refused(&path, wrong);
    refused("/nonexistent/deepseek.wasm", sha);
    // 다른 Provider 이름으로는 쓸 수 없다.
    assert_eq!(
        wasm::load_with(ProviderId::Openai, &path, sha, Limits::default()).err(),
        Some(StartupError::InvalidAdapter)
    );
    let (openai, openai_sha) = component_named("openai");
    assert_eq!(
        wasm::load_with(ProviderId::Deepseek, &openai, openai_sha, Limits::default()).err(),
        Some(StartupError::InvalidAdapter)
    );
    let dir = std::env::temp_dir().join(format!("m-gated-wasm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, wat) in [
        ("import", r#"(component (import "clock" (func)))"#),
        ("empty", "(component)"),
        ("core", "(module)"),
    ] {
        let bytes = wat::parse_str(wat).unwrap();
        let file = dir.join(format!("{name}.wasm"));
        std::fs::write(&file, &bytes).unwrap();
        refused(file.to_str().unwrap(), Sha256::digest(&bytes).into());
    }
    // 인터페이스 0.3.0이 아닌 판으로 내보낸 컴포넌트는 받지 않는다(결정 0013). 이전 판 0.2.0과,
    // semver로 호환되는 0.3.1도 마찬가지다. 실제 컴포넌트에서 내보내는 이름의 판만 같은 길이의 다른 판으로
    // 바꿔 만든다.
    let bytes = std::fs::read(&path).unwrap();
    let new = b"m-gated:adapter/provider@0.3.0".as_slice();
    for (tag, old) in [
        ("v010", b"m-gated:adapter/provider@0.1.0".as_slice()),
        ("v020", b"m-gated:adapter/provider@0.2.0".as_slice()),
        ("v031", b"m-gated:adapter/provider@0.3.1".as_slice()),
    ] {
        let mut renamed = bytes.clone();
        let mut found = 0;
        for i in 0..renamed.len().saturating_sub(new.len()) {
            if &renamed[i..i + new.len()] == new {
                renamed[i..i + new.len()].copy_from_slice(old);
                found += 1;
            }
        }
        assert!(found > 0, "컴포넌트가 0.3.0을 내보낸다");
        let file = dir.join(format!("{tag}.wasm"));
        std::fs::write(&file, &renamed).unwrap();
        refused(file.to_str().unwrap(), Sha256::digest(&renamed).into());
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
#[ignore = "scripts/build-adapter.sh가 만든 컴포넌트가 필요하다"]
fn config_installs_one_adapter_per_provider_per_process() {
    let (path, sha) = component();
    let hex: String = sha.iter().map(|b| format!("{b:02x}")).collect();
    let deepseek = support::YAML
        .replace("provider: anthropic", "provider: deepseek")
        .replace("https://api.anthropic.com", "https://api.deepseek.com")
        .replace("api: messages", "api: deepseek_chat")
        .replace(
            "          input: '1.000000000000'",
            "          input: '1.000000000000'\n          cache_read: '0.500000000000'",
        );
    let with = |path: &str, hex: &str| {
        deepseek.replacen(
            "    base_url: https://api.deepseek.com\n",
            &format!(
                "    base_url: https://api.deepseek.com\n    adapter: {path}\n    adapter_sha256: {hex}\n"
            ),
            1,
        )
    };
    let load = |yaml: &str| {
        config::load(ConfigInputs {
            yaml,
            file_environment: None,
            environment: &support::environment(),
        })
        .map(|_| ())
    };
    assert_eq!(
        load(&with(&path, &hex[..63])),
        Err(StartupError::InvalidAdapter)
    );
    assert_eq!(
        load(&with(&path, &hex.to_uppercase())),
        Err(StartupError::InvalidAdapter)
    );
    assert!(!port::installed(ProviderId::Deepseek));
    load(&with(&path, &hex)).unwrap();
    assert!(port::installed(ProviderId::Deepseek));
    // 같은 파일은 다시 읽지 않고 받는다.
    load(&with(&path, &hex)).unwrap();
    let mut other = hex.clone();
    other.replace_range(0..1, if &hex[..1] == "0" { "1" } else { "0" });
    assert_eq!(
        load(&with(&path, &other)),
        Err(StartupError::InvalidAdapter)
    );
    assert_eq!(load(&deepseek), Err(StartupError::InvalidAdapter));
    // 다른 Provider는 본체 어댑터를 그대로 쓴다.
    load(support::YAML).unwrap();
}

fn openai_profile(api: &str) -> ModelProfile {
    let mut p = json!({"model":"synthetic-text","api":api,"profile_version":"fixture-v1","pricing_version":"price-v1","fixture_version":"native-v1","actual_models":["synthetic-text"],"features":["client_tools","thinking","cache","json_schema","inline_png"],"max_input_tokens":4096,"max_output_tokens":1024,"max_body_bytes":8388608,"max_response_bytes":16777216,"rates":{"input":"1.000000000000","output":"2.000000000000","cache_read":"0.500000000000"}});
    match api {
        "openai_chat" => p["chat_output_limit_field"] = json!("max_completion_tokens"),
        "embeddings" => {
            let p = p.as_object_mut().unwrap();
            p.insert(
                "embedding_dimensions".into(),
                json!({"default":3,"max":4096}),
            );
            p.insert("embedding_encodings".into(), json!(["float", "base64"]));
            p.insert("features".into(), json!([]));
            p.remove("max_output_tokens");
            p.insert("rates".into(), json!({"input":"1.000000000000"}));
        }
        _ => {}
    }
    serde_json::from_value(p).unwrap()
}
/// 1×1 PNG의 base64.
fn png() -> String {
    let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    b.extend(1u32.to_be_bytes());
    b.extend(1u32.to_be_bytes());
    b.extend([8, 6, 0, 0, 0, 0, 0, 0, 0, 0]);
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b)
}
/// 같은 이름의 OpenAI API 둘(컴포넌트와 본체 어댑터).
fn openai_api(p: &'static dyn Provider, kind: config::ApiKind) -> &'static dyn Api {
    *p.apis().iter().find(|a| a.kind() == kind).unwrap()
}

#[test]
#[ignore = "scripts/build-adapter.sh가 만든 컴포넌트가 필요하다"]
fn openai_facts_match_native_adapter() {
    let wasm = load_openai();
    let native = port::native(ProviderId::Openai);
    assert_eq!(wasm.id(), native.id());
    assert_eq!(wasm.origin(), native.origin());
    assert_eq!(wasm.verify_path(), native.verify_path());
    assert_eq!(wasm.request_id_headers(), native.request_id_headers());
    assert_eq!(wasm.response_headers(), native.response_headers());
    assert_eq!(wasm.models_dev_name(), native.models_dev_name());
    assert_eq!(wasm.models_dev_name(), Some("openai"));
    assert_eq!(wasm.cache_write_rate(), native.cache_write_rate());
    for key in ["sk-1", "sk-proj-1", "sk-ant-1", "key", ""] {
        assert_eq!(wasm.accepts_key(key), native.accepts_key(key), "{key}");
    }
    assert_eq!(wasm.apis().len(), native.apis().len());
    for (n, name) in native
        .apis()
        .iter()
        .zip(["responses", "openai_chat", "embeddings"])
    {
        let w = openai_api(wasm, n.kind());
        let p = openai_profile(name);
        assert_eq!(w.routes(), n.routes(), "{name}");
        assert_eq!(w.upstream_path(), n.upstream_path(), "{name}");
        assert_eq!(w.error_format(), n.error_format(), "{name}");
        assert_eq!(w.token_rule(&p), n.token_rule(&p), "{name}");
        assert_eq!(w.usage_categories(), n.usage_categories(), "{name}");
        assert_eq!(w.response_limit(&p), n.response_limit(&p), "{name}");
        assert_eq!(w.catalog(), n.catalog(), "{name}");
        assert_eq!(w.stream(&p).is_some(), n.stream(&p).is_some(), "{name}");
        assert_eq!(w.query_allowed(""), n.query_allowed(""), "{name}");
        assert_eq!(w.accepts_zstd(), n.accepts_zstd(), "{name}");
        assert_eq!(w.forwarded_headers(), n.forwarded_headers(), "{name}");
        assert_eq!(w.settles_cutoff(), n.settles_cutoff(), "{name}");
        assert_eq!(w.may_be_free(), n.may_be_free(), "{name}");
        assert_eq!(wasm.check_profile(&p), native.check_profile(&p), "{name}");
        let mut beta = p.clone();
        beta.approved_beta = vec!["x".into()];
        assert_eq!(
            wasm.check_profile(&beta),
            Err(StartupError::InvalidProfile),
            "{name}"
        );
        assert_eq!(wasm.check_profile(&beta), native.check_profile(&beta));
    }
    let mut no_cache_price = openai_profile("responses");
    no_cache_price.rates.remove("cache_read");
    assert_eq!(
        wasm.check_profile(&no_cache_price),
        Err(StartupError::InvalidPrice)
    );
}

#[tokio::test]
#[ignore = "scripts/build-adapter.sh가 만든 컴포넌트가 필요하다"]
async fn openai_requests_and_responses_match_native_adapter() {
    use config::ApiKind;
    let (wasm, native) = (load_openai(), port::native(ProviderId::Openai));
    let image = format!("data:image/png;base64,{}", png());
    let responses = json!({"model":"synthetic-text","input":[{"role":"user","content":[{"type":"input_text","text":"x"},{"type":"input_image","image_url":image}]}],"store":false,"max_output_tokens":100});
    let chat = json!({"model":"synthetic-text","messages":[{"role":"user","content":[{"type":"text","text":"x"},{"type":"image_url","image_url":{"url":image}}]}],"max_completion_tokens":100});
    let embeddings = json!({"model":"synthetic-text","input":["x","y"],"dimensions":3,"encoding_format":"float"});
    // (API, profile 이름, 기본 요청, 기본 요청에서 바꿀 필드)
    type Case = (ApiKind, &'static str, Value, Vec<(&'static str, Value)>);
    let cases: [Case; 3] = [
        (
            ApiKind::Responses,
            "responses",
            responses,
            vec![
                ("stream", json!(true)),
                ("store", json!(true)),
                ("model", json!("other")),
                ("max_output_tokens", json!(5000)),
                ("unknown", json!(1)),
                (
                    "input",
                    json!([{"role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,bm90LXBuZw=="}]}]),
                ),
                (
                    "input",
                    json!([{"role":"user","content":[{"type":"input_image","image_url":"https://example.invalid/x.png"}]}]),
                ),
                ("reasoning", json!({"effort":"low"})),
            ],
        ),
        (
            ApiKind::OpenaiChat,
            "openai_chat",
            chat,
            vec![
                ("stream", json!(true)),
                ("stream_options", json!({"include_usage":true})),
                ("max_tokens", json!(100)),
                ("model", json!("other")),
                ("n", json!(2)),
                (
                    "messages",
                    json!([{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,bm90LXBuZw=="}}]}]),
                ),
                (
                    "tools",
                    json!([{"type":"function","function":{"name":"f","parameters":{}}}]),
                ),
            ],
        ),
        (
            ApiKind::Embeddings,
            "embeddings",
            embeddings,
            vec![
                ("dimensions", json!(5000)),
                // 32비트 usize를 넘는 값도 본체 빌드와 같은 오류여야 한다.
                ("dimensions", json!(4294967296u64)),
                ("encoding_format", json!("base64")),
                ("encoding_format", json!("other")),
                ("input", json!("")),
                ("model", json!("other")),
                ("user", json!("x")),
            ],
        ),
    ];
    for (kind, name, base, changes) in cases {
        let (w, n) = (openai_api(wasm, kind), openai_api(native, kind));
        let p = openai_profile(name);
        // 기본 요청은 받는다. 이미지가 든 요청도 PNG 검사를 지난다.
        assert!(
            w.validate(base.to_string().as_bytes(), &p).await.is_ok(),
            "{name}"
        );
        let mut bodies = vec![base.to_string(), "{".into(), "[]".into()];
        for (key, value) in changes {
            let mut b = base.clone();
            b[key] = value;
            bodies.push(b.to_string());
        }
        // `stream`과 `stream_options`를 같이 준 Chat 스트림 요청.
        if kind == ApiKind::OpenaiChat {
            let mut b = base.clone();
            b["stream"] = json!(true);
            b["stream_options"] = json!({"include_usage":true});
            bodies.push(b.to_string());
        }
        for body in &bodies {
            let (a, b) = (
                w.validate(body.as_bytes(), &p).await,
                n.validate(body.as_bytes(), &p).await,
            );
            assert_eq!(a.as_ref().err(), b.as_ref().err(), "{name} {body}");
            if let (Ok(a), Ok(b)) = (a, b) {
                assert_eq!(
                    (a.model, a.output_limit, a.stream),
                    (b.model, b.output_limit, b.stream),
                    "{name}"
                );
            }
        }
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert("x-other", HeaderValue::from_static("1"));
        assert_eq!(
            w.approved_headers(&headers, &p),
            n.approved_headers(&headers, &p)
        );
    }

    // 비스트림 응답.
    let usage = json!({"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cached_tokens":40},"output_tokens_details":{"reasoning_tokens":10}});
    let response = |status: &str| json!({"id":"resp_1","object":"response","model":"synthetic-text","status":status,"output":[],"incomplete_details":null,"usage":usage});
    let chat_usage = json!({"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_tokens_details":{"cached_tokens":40},"completion_tokens_details":{"reasoning_tokens":10}});
    let completion = json!({"id":"c1","object":"chat.completion","model":"synthetic-text","choices":[{"index":0,"message":{"role":"assistant","content":"x"},"finish_reason":"stop"}],"usage":chat_usage});
    let error = json!({"error":{"message":"x","type":"server_error"}});
    let vector = |v: Value| json!({"object":"list","model":"synthetic-text","data":[{"object":"embedding","index":0,"embedding":v.clone()},{"object":"embedding","index":1,"embedding":v}],"usage":{"prompt_tokens":5,"total_tokens":5}});
    let floats = {
        use base64::Engine;
        let raw: Vec<u8> = [0.5f32, 1.0, -2.0]
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        base64::engine::general_purpose::STANDARD.encode(raw)
    };
    let float_request =
        json!({"model":"synthetic-text","input":["x","y"],"dimensions":3}).to_string();
    let base64_request =
        json!({"model":"synthetic-text","input":["x","y"],"encoding_format":"base64"}).to_string();
    let large = json!({"x":"y".repeat(4 * 1024 * 1024), "a": 1}).to_string();
    let reads: Vec<(ApiKind, &str, u16, String, String)> = vec![
        (
            ApiKind::Responses,
            "responses",
            200,
            response("completed").to_string(),
            String::new(),
        ),
        (
            ApiKind::Responses,
            "responses",
            200,
            response("failed").to_string(),
            String::new(),
        ),
        (
            ApiKind::Responses,
            "responses",
            429,
            error.to_string(),
            String::new(),
        ),
        (
            ApiKind::Responses,
            "responses",
            500,
            "{}".into(),
            String::new(),
        ),
        (
            ApiKind::Responses,
            "responses",
            200,
            "not json".into(),
            String::new(),
        ),
        (
            ApiKind::Responses,
            "responses",
            200,
            large.clone(),
            String::new(),
        ),
        (
            ApiKind::OpenaiChat,
            "openai_chat",
            200,
            completion.to_string(),
            String::new(),
        ),
        (
            ApiKind::OpenaiChat,
            "openai_chat",
            200,
            error.to_string(),
            String::new(),
        ),
        (
            ApiKind::OpenaiChat,
            "openai_chat",
            400,
            error.to_string(),
            String::new(),
        ),
        (
            ApiKind::OpenaiChat,
            "openai_chat",
            200,
            large,
            String::new(),
        ),
        (
            ApiKind::Embeddings,
            "embeddings",
            200,
            vector(json!([0.5, 1.0, -2.0])).to_string(),
            float_request.clone(),
        ),
        (
            ApiKind::Embeddings,
            "embeddings",
            200,
            vector(json!([0.5, 1.0])).to_string(),
            float_request.clone(),
        ),
        (
            ApiKind::Embeddings,
            "embeddings",
            200,
            vector(json!(floats)).to_string(),
            base64_request.clone(),
        ),
        (
            ApiKind::Embeddings,
            "embeddings",
            200,
            vector(json!(floats)).to_string(),
            float_request.clone(),
        ),
        (
            ApiKind::Embeddings,
            "embeddings",
            200,
            vector(json!([0.5, 1.0, -2.0])).to_string(),
            "{".into(),
        ),
        (
            ApiKind::Embeddings,
            "embeddings",
            400,
            error.to_string(),
            float_request,
        ),
    ];
    for (kind, name, status, body, request) in reads {
        let (w, n) = (openai_api(wasm, kind), openai_api(native, kind));
        let p = openai_profile(name);
        let status = StatusCode::from_u16(status).unwrap();
        let (a, b) = (
            w.read(status, body.as_bytes(), request.as_bytes(), &p)
                .await,
            n.read(status, body.as_bytes(), request.as_bytes(), &p)
                .await,
        );
        assert_eq!(a.as_ref().err(), b.as_ref().err(), "{name} {status}");
        let (a, b) = (a.unwrap_or_else(|_| empty()), b.unwrap_or_else(|_| empty()));
        assert_eq!(
            (&a.usage, a.failed),
            (&b.usage, b.failed),
            "{name} {status}"
        );
    }
    // 같은 사용량을 읽는지 하나는 직접 확인한다.
    let w = openai_api(wasm, ApiKind::Embeddings);
    let r = w
        .read(
            StatusCode::OK,
            vector(json!(floats)).to_string().as_bytes(),
            base64_request.as_bytes(),
            &openai_profile("embeddings"),
        )
        .await
        .unwrap();
    assert_eq!(
        r.usage
            .map(|u| u.get(m_gated::ledger::finance::UsageCategory::Input)),
        Some(5)
    );

    // 스트림 응답.
    let event = |kind: &str, n: u64, status: &str, usage: Value| {
        let response = json!({"id":"resp_1","object":"response","model":"synthetic-text","status":status,"error":null,"incomplete_details":null,"usage":usage});
        format!(
            "event: {kind}\ndata: {}\n\n",
            json!({"type":kind,"sequence_number":n,"response":response})
        )
    };
    let created = event("response.created", 0, "in_progress", Value::Null);
    let delta = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"sequence_number\":1,\"delta\":\"x\"}\n\n".to_string();
    let completed = event("response.completed", 2, "completed", usage.clone());
    let error_event =
        "event: error\ndata: {\"type\":\"error\",\"sequence_number\":1,\"message\":\"x\"}\n\n"
            .to_string();
    let p = openai_profile("responses");
    let (_, closed, _) = run_stream(
        openai_api(wasm, ApiKind::Responses),
        &p,
        &(created.clone() + &delta + &completed),
    );
    assert!(
        closed.unwrap().0.is_some(),
        "끝까지 읽은 Responses 스트림은 사용량이 있다"
    );
    for raw in [
        created.clone() + &delta + &completed,
        created.clone() + &delta,
        created.clone() + &error_event,
        created.clone() + &event("response.failed", 2, "failed", usage.clone()),
        delta.clone() + &completed,
        created.clone() + &completed + &completed,
        "event: other\ndata: {}\n\n".into(),
    ] {
        assert_eq!(
            run_stream(openai_api(wasm, ApiKind::Responses), &p, &raw),
            run_stream(openai_api(native, ApiKind::Responses), &p, &raw),
            "{raw}"
        );
    }
    let chunk = |choices: Value, usage: Value| {
        wire(
            &json!({"id":"c1","object":"chat.completion.chunk","model":"synthetic-text","choices":choices,"usage":usage}),
        )
    };
    let first = chunk(
        json!([{"index":0,"delta":{"role":"assistant","content":"x"},"finish_reason":null}]),
        Value::Null,
    );
    let finish = chunk(
        json!([{"index":0,"delta":{},"finish_reason":"stop"}]),
        Value::Null,
    );
    let usage_chunk = chunk(json!([]), chat_usage.clone());
    let chat_error =
        wire(&json!({"error":{"message":"x","type":"server_error","param":null,"code":"x"}}));
    for output in ["max_completion_tokens", "max_tokens"] {
        let mut p = openai_profile("openai_chat");
        p.chat_output_limit_field = Some(output.into());
        // `max_tokens` profile은 추론 토큰이 0일 때만 정산한다. 이 스트림의 추론 토큰은 10이다.
        let (_, closed, _) = run_stream(
            openai_api(wasm, ApiKind::OpenaiChat),
            &p,
            &(first.clone() + &finish + &usage_chunk + "data: [DONE]\n\n"),
        );
        assert_eq!(
            closed.unwrap().0.is_some(),
            output == "max_completion_tokens",
            "{output}"
        );
        for raw in [
            first.clone() + &finish + &usage_chunk + "data: [DONE]\n\n",
            first.clone() + &finish + "data: [DONE]\n\n",
            first.clone() + &finish + &usage_chunk,
            chat_error.clone() + "data: [DONE]\n\n",
            first.clone() + &chat_error,
            "event: other\ndata: {}\n\n".into(),
        ] {
            assert_eq!(
                run_stream(openai_api(wasm, ApiKind::OpenaiChat), &p, &raw),
                run_stream(openai_api(native, ApiKind::OpenaiChat), &p, &raw),
                "{output} {raw}"
            );
        }
    }
}
fn empty() -> port::Reading {
    port::Reading {
        usage: None,
        counted_input_tokens: None,
        failed: false,
    }
}
