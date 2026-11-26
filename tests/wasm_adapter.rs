//! DeepSeek WASM 어댑터(결정 0009). 먼저 `scripts/build-adapter.sh`로 컴포넌트를 빌드한다.
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

fn component() -> (String, [u8; 32]) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/adapters/deepseek.wasm");
    let bytes = std::fs::read(&path).expect("scripts/build-adapter.sh로 컴포넌트를 먼저 빌드한다");
    (path.to_string_lossy().into(), Sha256::digest(&bytes).into())
}
fn load(limits: Limits) -> &'static dyn Provider {
    let (path, sha) = component();
    wasm::load_with(ProviderId::Deepseek, &path, sha, limits).unwrap()
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
    for key in ["sk-1", "sk-ant-1", "key", ""] {
        assert_eq!(wasm.accepts_key(key), native.accepts_key(key), "{key}");
    }
    assert_eq!(wasm.apis().len(), native.apis().len());
    let (w, n) = (chat(wasm), chat(native));
    let p = profile(&[]);
    assert_eq!(w.kind(), n.kind());
    assert_eq!(w.routes(), n.routes());
    assert_eq!(w.upstream_path(), n.upstream_path());
    assert_eq!(w.error_format(), n.error_format());
    assert_eq!(w.token_rule(&p), n.token_rule(&p));
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
    for limits in [
        Limits {
            fuel_per_call: 1,
            ..Limits::default()
        },
        Limits {
            memory_bytes: 64 * 1024,
            ..Limits::default()
        },
    ] {
        let provider = load(limits);
        let api = chat(provider);
        assert_eq!(
            api.validate(body.as_bytes(), &p).await.err(),
            Some(ApiError::AdapterFailed)
        );
        assert_eq!(
            api.approved_headers(&headers, &p).err(),
            Some(ApiError::AdapterFailed)
        );
        assert_eq!(
            api.read(StatusCode::OK, completion().to_string().as_bytes(), b"", &p)
                .await
                .err(),
            Some(UpstreamError::Protocol)
        );
        let (steps, closed, failed) =
            run_stream(api, &p, &(wire(&chunk(true)) + "data: [DONE]\n\n"));
        assert!(steps.iter().all(|s| s == &Err(UpstreamError::Protocol)));
        assert_eq!(closed, Err(UpstreamError::Protocol));
        assert!(!failed);
        assert_eq!(
            provider.check_profile(&p),
            Err(StartupError::InvalidAdapter)
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
