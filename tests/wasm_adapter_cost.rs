//! DeepSeek WASM 어댑터의 비용 측정(결정 0009의 5단계). 값을 단정하지 않고 출력만 한다.
//! 먼저 `scripts/build-adapter.sh`로 컴포넌트를 빌드하고, release로 돌린다.
//!
//! ```sh
//! scripts/build-adapter.sh
//! cargo test --release --locked --test wasm_adapter_cost -- --ignored --nocapture --test-threads=1
//! ```
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use m_gated::{
    config::{ModelProfile, ProviderId},
    providers::{
        port::{self, Api},
        wasm::{self, Limits},
    },
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

fn wasm_api() -> &'static dyn Api {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/adapters/deepseek.wasm");
    let bytes = std::fs::read(&path).expect("scripts/build-adapter.sh로 컴포넌트를 먼저 빌드한다");
    let sha = Sha256::digest(&bytes).into();
    wasm::load_with(
        ProviderId::Deepseek,
        &path.to_string_lossy(),
        sha,
        Limits::default(),
    )
    .unwrap()
    .apis()[0]
}
fn native_api() -> &'static dyn Api {
    port::native(ProviderId::Deepseek).apis()[0]
}
fn profile() -> ModelProfile {
    serde_json::from_value(json!({"model":"synthetic-deepseek","api":"deepseek_chat","profile_version":"deepseek-v1","pricing_version":"deepseek-price-v1","fixture_version":"deepseek-native-v1","actual_models":["synthetic-deepseek"],"features":["thinking","client_tools","json_schema"],"max_input_tokens":4096,"max_output_tokens":1024,"max_body_bytes":8388608,"max_response_bytes":16777216,"rates":{"input":"1.000000000000","output":"2.000000000000","cache_read":"0.500000000000"}})).unwrap()
}
fn usage() -> Value {
    json!({"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_cache_hit_tokens":40,"prompt_cache_miss_tokens":60,"completion_tokens_details":{"reasoning_tokens":10}})
}
fn chunk(end: bool) -> Value {
    json!({"id":"ds1","object":"chat.completion.chunk","model":"synthetic-deepseek","choices":[{"index":0,"delta":{"role":"assistant","content":"synthetic text"},"finish_reason":if end{Some("stop")}else{None}}],"usage":if end{usage()}else{Value::Null}})
}
/// 메시지 `turns`개, 각 메시지 `size`바이트인 요청 본문.
fn request(turns: usize, size: usize) -> Vec<u8> {
    let messages: Vec<Value> = (0..turns)
        .map(|i| json!({"role": if i % 2 == 0 {"user"} else {"assistant"}, "content": "x".repeat(size)}))
        .collect();
    json!({"model":"synthetic-deepseek","messages":messages,"max_tokens":100,"stream":true,"thinking":{"type":"disabled"}})
        .to_string()
        .into_bytes()
}

struct Times(Vec<Duration>);
impl Times {
    fn show(mut self, name: &str) -> Duration {
        self.0.sort();
        let at = |q: f64| self.0[((self.0.len() - 1) as f64 * q) as usize];
        println!(
            "{name:<44} median {:>10.1?}  p99 {:>10.1?}  (n={})",
            at(0.5),
            at(0.99),
            self.0.len()
        );
        at(0.5)
    }
}
async fn time_async<F, Fut>(n: usize, mut f: F) -> Times
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    for _ in 0..n / 10 {
        f().await;
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let start = Instant::now();
        f().await;
        out.push(start.elapsed());
    }
    Times(out)
}
fn time_sync(n: usize, mut f: impl FnMut() -> Duration) -> Times {
    for _ in 0..n / 10 {
        f();
    }
    Times((0..n).map(|_| f()).collect())
}

#[tokio::test]
#[ignore = "release로 돌리는 측정이다"]
async fn request_and_event_costs() {
    let start = Instant::now();
    let w = wasm_api();
    println!(
        "load (read, compile, describe)               {:?}",
        start.elapsed()
    );
    let n = native_api();
    let p = profile();
    for (name, body, count) in [
        ("validate 1 message (~200 B)", request(1, 10), 2000),
        (
            "validate 20 messages x 4 KiB (~80 KiB)",
            request(20, 4096),
            500,
        ),
        (
            "validate 8 messages x 1 MiB (~8 MiB)",
            request(8, 1 << 20),
            10,
        ),
    ] {
        for (kind, api) in [("native", n), ("wasm", w)] {
            time_async(count, || async {
                api.validate(&body, &p).await.unwrap();
            })
            .await
            .show(&format!("{name} {kind}"));
        }
    }
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert("user-agent", HeaderValue::from_static("synthetic"));
    for (kind, api) in [("native", n), ("wasm", w)] {
        time_sync(2000, || {
            let start = Instant::now();
            api.approved_headers(&headers, &p).unwrap();
            start.elapsed()
        })
        .show(&format!("approved_headers {kind}"));
    }
    let completion = json!({"id":"ds1","object":"chat.completion","model":"synthetic-deepseek","choices":[{"index":0,"message":{"role":"assistant","content":"x".repeat(4096)},"finish_reason":"stop"}],"usage":usage()}).to_string();
    for (kind, api) in [("native", n), ("wasm", w)] {
        time_async(2000, || async {
            api.read(StatusCode::OK, completion.as_bytes(), b"", &p)
                .await
                .unwrap();
        })
        .await
        .show(&format!("read 4 KiB completion {kind}"));
    }

    // 스트림: 여는 비용과 event 하나의 비용. event는 시간을 재기 전에 잘라 둔다.
    let events = 200;
    let mut raw = String::new();
    for i in 0..events {
        raw += &format!("data: {}\n\n", chunk(i + 1 == events));
    }
    raw += "data: [DONE]\n\n";
    for (kind, api) in [("native", n), ("wasm", w)] {
        time_sync(2000, || {
            let start = Instant::now();
            let reader = api.stream(&p).unwrap();
            drop(reader.decoder());
            start.elapsed()
        })
        .show(&format!("stream open (stream + decoder) {kind}"));
        let mut per_event = Vec::new();
        for _ in 0..50 {
            let mut reader = api.stream(&p).unwrap();
            let mut decoder = reader.decoder();
            let decoded: Vec<_> = raw
                .bytes()
                .filter_map(|b| decoder.byte(b).unwrap())
                .collect();
            let count = decoded.len() as u32;
            let start = Instant::now();
            for e in decoded {
                reader.accept(e).unwrap();
            }
            reader.close().unwrap();
            per_event.push(start.elapsed() / count);
        }
        Times(per_event).show(&format!("stream accept, per event {kind}"));
    }
}

/// 컴포넌트가 끝없이 돌면서 fuel을 다 쓸 때까지 걸리는 시간. fuel은 명령 수를 세므로 명령에 따라
/// 시간이 다르다. 동기 호출은 이 시간 동안 tokio 작업 스레드를 붙잡는다.
#[test]
#[ignore = "release로 돌리는 측정이다"]
fn fuel_limit_time() {
    use wasmtime::{Config, Engine, Instance, Module, Store, Trap};
    let mut config = Config::new();
    config.consume_fuel(true);
    let engine = Engine::new(&config).unwrap();
    let module = Module::new(
        &engine,
        wat::parse_str(
            r#"(module
             (memory 1)
             (func (export "integer") (local i32)
               (loop
                 (local.set 0 (i32.add (local.get 0) (i32.load (i32.const 0))))
                 (i32.store (i32.const 4) (local.get 0))
                 (br 0)))
             (func (export "float_division") (local f64)
               (local.set 0 (f64.const 1e300))
               (loop
                 (local.set 0 (f64.sqrt (f64.div (local.get 0) (f64.const 1.0000001))))
                 (br 0))))"#,
        )
        .unwrap(),
    )
    .unwrap();
    let limits = Limits::default();
    for (limit, fuel) in [("sync", limits.sync_fuel), ("async", limits.async_fuel)] {
        for name in ["integer", "float_division"] {
            let mut store = Store::new(&engine, ());
            store.set_fuel(fuel).unwrap();
            let instance = Instance::new(&mut store, &module, &[]).unwrap();
            let spin = instance.get_typed_func::<(), ()>(&mut store, name).unwrap();
            let start = Instant::now();
            let error = spin.call(&mut store, ()).unwrap_err();
            assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
            println!(
                "{limit} limit ({fuel} fuel), {name:<14} spent in {:?}",
                start.elapsed()
            );
        }
    }
}

/// 상한 안에서 가장 큰 정상 입력이 쓰는 fuel. 늘려 가는 상한 가운데 처음 통과하는 값을 출력한다.
#[tokio::test]
#[ignore = "release로 돌리는 측정이다"]
async fn largest_inputs_fuel() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/adapters/deepseek.wasm");
    let sha: [u8; 32] = Sha256::digest(std::fs::read(&path).unwrap()).into();
    let fuels: Vec<u64> = (0..40)
        .map(|i| 100_000 * 10u64.pow(i / 8) * (1 + i as u64 % 8))
        .collect();
    let apis: Vec<(u64, &'static dyn Api)> = fuels
        .iter()
        .map(|&fuel| {
            let limits = Limits {
                async_fuel: fuel,
                sync_fuel: fuel,
                ..Limits::default()
            };
            let provider =
                wasm::load_with(ProviderId::Deepseek, &path.to_string_lossy(), sha, limits);
            (fuel, provider.unwrap().apis()[0])
        })
        .collect();
    macro_rules! first {
        ($name:expr, $a:ident => $ok:expr) => {{
            let mut found = None;
            for &(fuel, $a) in &apis {
                if $ok {
                    found = Some(fuel);
                    break;
                }
            }
            println!("{:<44} needs at most {found:?} fuel", $name);
        }};
    }
    let p = profile();
    // 본문 상한(8 MiB) 가까운 요청. 문자열 하나와, 받을 수 있는 가장 많은 메시지 1,000개.
    let one = request(1, 8 * 1024 * 1024 - 200);
    first!("validate 1 message x 8 MiB", a => a.validate(&one, &p).await.is_ok());
    let many = request(1000, 8300);
    first!("validate 1,000 messages x 8 KiB", a => a.validate(&many, &p).await.is_ok());
    // 응답 상한(16 MiB) 가까운 응답.
    let completion = json!({"id":"ds1","object":"chat.completion","model":"synthetic-deepseek","choices":[{"index":0,"message":{"role":"assistant","content":"x".repeat(16 * 1024 * 1024 - 400)},"finish_reason":"stop"}],"usage":usage()}).to_string();
    first!("read 16 MiB completion", a => a
        .read(StatusCode::OK, completion.as_bytes(), b"", &p)
        .await
        .is_ok());
    // 내용 1 MiB인 event 하나.
    let mut big = chunk(false);
    big["choices"][0]["delta"]["content"] = json!("x".repeat(1 << 20));
    let raw = format!("data: {big}\n\n");
    first!("stream accept, 1 MiB content event", a => {
        let mut reader = a.stream(&p).unwrap();
        let mut decoder = reader.decoder();
        let events: Vec<_> = raw.bytes().filter_map(|b| decoder.byte(b).unwrap()).collect();
        events.len() == 1 && events.into_iter().all(|e| reader.accept(e).is_ok())
    });
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    first!("approved_headers", a => a.approved_headers(&headers, &p).is_ok());
}
