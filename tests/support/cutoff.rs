//! 최종 usage 없이 끝난 요청의 정산(결정 0005 6~14번).
//! - 시작 event 뒤에 끊긴 Anthropic 스트림은 받을 때 기록한 끊긴 요청 규칙으로 바로 정산한다.
//! - 첫 event 전에 끊긴 스트림, 응답 전에 끊긴 비스트림 요청, Provider HTTP 오류는 0원이다.
//! - 어느 경우든 사용자를 막지 않는다.
use super::*;
use m_gated::ledger::finance::{FinalOutcome, NoUsage, StartUsage};
use serde_json::{Value, json};
use std::time::Duration;

/// 시험 스트림 요청 본문. 끊긴 요청의 `max_tokens` 규칙과 받은 내용 상한(max_tokens × 8바이트)이 이 값을 쓴다.
fn stream_body(max_tokens: u64) -> String {
    json!({"model":"synthetic-text","max_tokens":max_tokens,"stream":true,"messages":[{"role":"user","content":"synthetic prompt sentinel"}]}).to_string()
}
/// SSE event 하나. event 이름은 data의 type과 같다.
fn event(data: Value) -> String {
    format!(
        "event: {}\ndata: {data}\n\n",
        data["type"].as_str().unwrap()
    )
}
/// 시작 event 뒤에 이어 받은 event들의 wire.
fn after_start(events: Vec<Value>) -> String {
    events
        .into_iter()
        .map(event)
        .fold(s3_contract::START.to_owned(), |wire, e| wire + &e)
}
/// thinking·텍스트·도구 인자를 하나씩 받다가 끊긴 스트림. 서명(signature)은 출력 내용이 아니다.
fn received_wire() -> String {
    after_start(vec![
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think."}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"c2lnbmF0dXJl"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hello"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"weather","input":{}}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}),
    ])
}
/// `received_wire`에서 받은 출력 내용. 받은 순서대로 이어 붙인다(26글자).
const RECEIVED: &str = "Let me think.Hello{\"city\":";
/// 텍스트 하나를 받다가 끊긴 스트림.
fn text_wire(text: &str) -> String {
    after_start(vec![
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
    ])
}
/// 정상 비스트림 응답(입력 100, 출력 20 token).
const MESSAGE: &[u8] = br#"{"id":"msg_fixture","type":"message","role":"assistant","model":"synthetic-text","content":[{"type":"text","text":"synthetic response"}],"stop_reason":"end_turn","usage":{"input_tokens":100,"output_tokens":20}}"#;
fn json_headers() -> HeaderMap {
    HeaderMap::from_iter([(
        "content-type".parse().unwrap(),
        "application/json".parse().unwrap(),
    )])
}
type Upstream = (
    tokio::sync::mpsc::Sender<Bytes>,
    tokio::sync::oneshot::Receiver<()>,
);
/// 스트림 요청을 보내고 Provider가 보낸 wire를 client가 모두 읽을 때까지 기다린다. 끊지는 않는다.
async fn open_stream(
    h: &Harness,
    body: String,
    wire: &str,
) -> (reqwest::Response, uuid::Uuid, Upstream) {
    let (tx, stopped) = h.provider.stream_messages().await;
    tx.send(Bytes::from(wire.to_owned())).await.unwrap();
    let mut r = h.request("/v1/messages", body).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let id = r.headers()["x-gateway-request-id"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let mut received = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), async {
        while received.len() < wire.len() {
            received.extend_from_slice(&r.chunk().await.unwrap().unwrap());
        }
    })
    .await
    .unwrap();
    assert_eq!(received, wire.as_bytes());
    (r, id, (tx, stopped))
}
/// client가 연결을 끊고, gateway가 Provider 연결을 닫고 정산을 마칠 때까지 기다린다.
async fn cut(h: &Harness, r: reqwest::Response, (tx, stopped): Upstream) {
    drop(r);
    tokio::time::timeout(Duration::from_secs(3), stopped)
        .await
        .expect("client가 끊으면 Provider 연결도 닫는다")
        .unwrap();
    drop(tx);
    tokio::time::timeout(Duration::from_secs(6), h.runtime.wait_requests())
        .await
        .unwrap();
}
/// 스트림 요청을 보내 wire를 다 받은 뒤 client가 끊는다. 요청 id를 돌려준다.
async fn cut_after(h: &Harness, body: String, wire: &str) -> uuid::Uuid {
    let (r, id, upstream) = open_stream(h, body, wire).await;
    cut(h, r, upstream).await;
    id
}
/// Provider가 받은 count_tokens 요청의 본문. 받은 내용을 세는 두 요청은 동시에 가므로 정렬해서 본다.
/// 사용자 요청과 같은 회사 키와 Anthropic 버전으로 보냈는지도 확인한다.
async fn count_calls(h: &Harness) -> Vec<Value> {
    let calls = h.provider.calls.lock().await;
    let mut bodies = calls
        .iter()
        .filter(|c| c.path == "/v1/messages/count_tokens")
        .map(|c| {
            assert_eq!(c.method, "POST");
            assert_eq!(c.headers["x-api-key"], "synthetic-company-key");
            assert_eq!(c.headers["anthropic-version"], "2023-06-01");
            serde_json::from_slice::<Value>(&c.body).unwrap()
        })
        .collect::<Vec<_>>();
    bodies.sort_by_key(Value::to_string);
    bodies
}
/// 받은 내용을 세는 두 요청: 한 글자짜리 기준 메시지와 받은 내용 전체.
fn counted(content: &str) -> Vec<Value> {
    let mut bodies = [".", content]
        .map(|c| json!({"model":"synthetic-text","messages":[{"role":"user","content":c}]}))
        .to_vec();
    bodies.sort_by_key(Value::to_string);
    bodies
}
/// 끊긴 요청 뒤에도 사용자는 막히지 않는다. 다음 비스트림 요청은 Provider까지 가서 정산된다.
async fn next_is_not_blocked(h: &Harness) {
    h.provider
        .set(StatusCode::OK, json_headers(), Bytes::from_static(MESSAGE))
        .await;
    let sent = h.provider.calls.lock().await.len();
    let r = h.request("/v1/messages", BODY).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let _ = r.bytes().await.unwrap();
    h.runtime.wait_requests().await;
    assert_eq!(h.provider.calls.lock().await.len(), sent + 1);
    let unknown: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gw_requests WHERE accounting_state='unknown'")
            .fetch_one(h.db.store.pool())
            .await
            .unwrap();
    assert_eq!(unknown, 0);
}
/// 끊긴 요청의 실행 기록과 정산 근거: (기록한 규칙, 결과, 사유, usage event의 규칙, 입력 token, Provider 입력 usage).
async fn cutoff_detail(
    h: &Harness,
    id: uuid::Uuid,
) -> (
    Option<String>,
    String,
    String,
    Option<String>,
    Option<String>,
    Value,
) {
    sqlx::query_as("SELECT r.execution_snapshot->>'cutoff_rule',r.outcome,r.reason_code,e.normalized_usage->>'cutoff_rule',e.normalized_usage->>'input_tokens',e.provider_usage FROM gw_requests r JOIN gw_usage_events e ON e.request_id=r.id AND e.kind='final' WHERE r.id=$1")
        .bind(id)
        .fetch_one(h.db.store.pool())
        .await
        .unwrap()
}
/// 끊긴 요청을 `max_tokens` 규칙(요청의 max_tokens만큼)으로 정산한 결과. 대체 정산이면 그 까닭이 있다.
fn max_tokens(cost: &str, output: u64, fallback: Option<&str>) -> finance::Settlement {
    (
        "settled".into(),
        Some(cost.into()),
        Some("cutoff_max_tokens".into()),
        Some(output.to_string()),
        fallback.map(str::to_owned),
    )
}
/// 관리 API를 SSO 토큰으로 부른다. 재생을 확인하려고 명령 id를 받는다.
/// (상태, `x-gateway-command-replayed` 헤더, 본문)을 돌려준다.
async fn admin(
    h: &Harness,
    method: reqwest::Method,
    path: &str,
    command: uuid::Uuid,
    input: Option<Value>,
) -> (u16, Option<String>, Value) {
    let mut r = reqwest::Client::builder()
        .no_proxy()
        .retry(reqwest::retry::never())
        .build()
        .unwrap()
        .request(method, format!("{}{path}", h.base))
        .bearer_auth(&h.sso)
        .header("x-gateway-command-id", command.to_string());
    if let Some(input) = input {
        r = r
            .header("content-type", "application/json")
            .body(input.to_string());
    }
    let r = r.send().await.unwrap();
    let status = r.status().as_u16();
    let replayed = r
        .headers()
        .get("x-gateway-command-replayed")
        .map(|v| v.to_str().unwrap().to_owned());
    let body = serde_json::from_slice(&r.bytes().await.unwrap()).unwrap_or(Value::Null);
    (status, replayed, body)
}
const RULE: &str = "/api/admin/settings/cutoff-rule";
/// 끊긴 요청 규칙을 바꾼다. 바꾼 뒤의 버전을 돌려준다.
async fn set_rule(h: &Harness, rule: &str, expected: &str) -> String {
    let (status, _, body) = admin(
        h,
        reqwest::Method::PUT,
        RULE,
        uuid::Uuid::new_v4(),
        Some(json!({"provider":"anthropic","cutoff_rule":rule,"expected_version":expected})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["cutoff_rule"]["cutoff_rule"], rule);
    body["cutoff_rule"]["version"].as_str().unwrap().to_owned()
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn received_rule_counts_received_output_and_does_not_block() {
    let h = Harness::new().await;
    h.provider.count_by_content().await;
    let id = cut_after(&h, stream_body(100), &received_wire()).await;
    // 받은 내용과 기준 메시지를 같은 회사 키로 세고, 그 차이+1(26 token)을 출력으로 쓴다.
    assert_eq!(count_calls(&h).await, counted(RECEIVED));
    assert_eq!(
        finance::settlement(h.db.store.pool(), id).await,
        finance::cutoff_received("0.000152000000", 26)
    );
    // 입력 token은 시작 event가 알린 값이다. 결과는 client 취소로 남는다.
    assert_eq!(
        cutoff_detail(&h, id).await,
        (
            Some("received".into()),
            "cancelled".into(),
            "client_cancelled".into(),
            Some("received".into()),
            Some("100".into()),
            json!({"input_tokens":100,"cache_read_input_tokens":0,"cache_creation_input_tokens":0})
        )
    );
    // 정산한 token은 정산한 1분 칸과 예산 합계에 쌓이고 미정산은 없다.
    let totals:(i64,i64,i64,String,i64)=sqlx::query_as("SELECT t.unknown_count,t.input_tokens,t.output_tokens,t.cost_usd::text,(SELECT COALESCE(sum(w.settled_tokens),0)::bigint FROM test_user_rate_windows w WHERE w.scope_id=t.scope_id) FROM test_user_usage_totals t").fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(totals, (0, 100, 26, "0.000152000000".into(), 126));
    next_is_not_blocked(&h).await;
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn max_tokens_rule_set_by_admin_charges_max_tokens_without_counting() {
    let h = Harness::with_config(admin_configuration()).await;
    h.provider.count_by_content().await;
    assert_eq!(set_rule(&h, "max_tokens", "1").await, "2");
    let id = cut_after(&h, stream_body(100), &received_wire()).await;
    // 출력은 요청의 max_tokens(100)이다. 받은 내용을 세지 않는다.
    assert!(count_calls(&h).await.is_empty());
    assert_eq!(h.provider.calls.lock().await.len(), 1);
    assert_eq!(
        finance::settlement(h.db.store.pool(), id).await,
        max_tokens("0.000300000000", 100, None)
    );
    let detail = cutoff_detail(&h, id).await;
    assert_eq!(
        (
            detail.0.as_deref(),
            detail.3.as_deref(),
            detail.4.as_deref()
        ),
        (Some("max_tokens"), Some("max_tokens"), Some("100"))
    );
    next_is_not_blocked(&h).await;
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn received_rule_falls_back_to_max_tokens_when_the_count_fails() {
    for (status, body) in [
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            br#"{"type":"error","error":{"type":"api_error","message":"synthetic count failure"}}"#
                .as_slice(),
        ),
        (StatusCode::OK, br#"{"input_tokens":"many"}"#.as_slice()),
    ] {
        let h = Harness::with_config(admin_configuration()).await;
        h.provider
            .set_count(status, json_headers(), Bytes::copy_from_slice(body))
            .await;
        let id = cut_after(&h, stream_body(100), &received_wire()).await;
        // 두 count 요청을 모두 보냈지만 세지 못했다. max_tokens 규칙으로 정산하고 그 까닭을 남긴다.
        assert_eq!(count_calls(&h).await, counted(RECEIVED));
        assert_eq!(
            finance::settlement(h.db.store.pool(), id).await,
            max_tokens("0.000300000000", 100, Some("cutoff_count_unavailable"))
        );
        // 기록한 규칙은 그대로 `received`다.
        assert_eq!(cutoff_detail(&h, id).await.3.as_deref(), Some("received"));
        // 관리 화면의 요청 상세도 정산 방식, 규칙, 대체 사유를 보여 준다.
        let (status, _, detail) = admin(
            &h,
            reqwest::Method::GET,
            &format!("/api/admin/requests/{id}"),
            uuid::Uuid::new_v4(),
            None,
        )
        .await;
        assert_eq!(status, 200, "{detail}");
        assert_eq!(detail["accounting_state"], "settled");
        assert_eq!(
            (
                &detail["usage"]["final"]["source"],
                &detail["usage"]["final"]["cutoff_rule"],
                &detail["usage"]["final"]["cutoff_fallback_reason"],
                &detail["usage"]["final"]["output_tokens"]
            ),
            (
                &json!("cutoff_max_tokens"),
                &json!("received"),
                &json!("cutoff_count_unavailable"),
                &json!("100")
            )
        );
        next_is_not_blocked(&h).await;
        h.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn received_rule_falls_back_to_max_tokens_when_the_content_is_too_large() {
    // max_tokens 2이면 받은 내용은 16바이트까지 담는다. 17바이트를 받으면 내용을 버린다.
    // max_tokens 1024이면 8KiB까지 담지만, 6KiB를 넘는 한 조각은 decoder가 담지 않고 버린다.
    for (limit, text, cost) in [
        (2, "0123456789abcdefg".to_owned(), "0.000104000000"),
        (1024, "x".repeat(7000), "0.002148000000"),
    ] {
        let h = Harness::new().await;
        h.provider.count_by_content().await;
        let id = cut_after(&h, stream_body(limit), &text_wire(&text)).await;
        assert!(count_calls(&h).await.is_empty());
        assert_eq!(
            finance::settlement(h.db.store.pool(), id).await,
            max_tokens(cost, limit, Some("cutoff_content_too_large"))
        );
        next_is_not_blocked(&h).await;
        h.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn large_content_delta_is_relayed_and_settled_from_the_final_usage() {
    // 출력 내용 한 조각이 내용 상한(6KiB)을 넘어도 스트림은 끊기지 않는다. 끝까지 받으면 Provider final로 정산한다.
    let h = Harness::new().await;
    let wire = format!(
        "{}{}{}",
        text_wire(&"x".repeat(16 * 1024)),
        event(json!({"type":"content_block_stop","index":0})),
        s3_contract::END
    );
    h.provider
        .set_chunked(
            StatusCode::OK,
            HeaderMap::from_iter([(
                "content-type".parse().unwrap(),
                "text/event-stream".parse().unwrap(),
            )]),
            Bytes::from(wire.clone()),
        )
        .await;
    let r = h
        .request("/v1/messages", stream_body(1024))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.bytes().await.unwrap(), wire.as_bytes());
    h.runtime.wait_requests().await;
    let (state, cost, source, output, _) = finance::last_settlement(h.db.store.pool()).await;
    assert_eq!(
        (
            state.as_str(),
            cost.as_deref(),
            source.as_deref(),
            output.as_deref()
        ),
        (
            "settled",
            Some("0.000140000000"),
            Some("provider"),
            Some("20")
        )
    );
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn error_event_and_broken_connection_after_start_are_cut_offs() {
    for broken in [false, true] {
        let h = Harness::new().await;
        h.provider.count_by_content().await;
        let id = if broken {
            // Provider 연결이 chunked 종료 없이 끊긴다.
            let (tx, stopped) = h.provider.stream_broken().await;
            let wire = text_wire("Hello");
            tx.send(Bytes::from(wire.clone())).await.unwrap();
            let mut r = h
                .request("/v1/messages", stream_body(100))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            let id: uuid::Uuid = r.headers()["x-gateway-request-id"]
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            let mut received = Vec::new();
            while received.len() < wire.len() {
                received.extend_from_slice(&r.chunk().await.unwrap().unwrap());
            }
            drop(tx);
            assert!(
                r.bytes().await.is_err(),
                "끊긴 연결을 정상 종료로 바꾸지 않는다"
            );
            tokio::time::timeout(Duration::from_secs(3), stopped)
                .await
                .unwrap()
                .unwrap();
            id
        } else {
            // 시작 event 뒤의 오류 event는 client에 그대로 전달하고 스트림을 끝낸다.
            let wire = text_wire("Hello")
                + &event(
                    json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}),
                );
            let (tx, stopped) = h.provider.stream_messages().await;
            tx.send(Bytes::from(wire.clone())).await.unwrap();
            let r = h
                .request("/v1/messages", stream_body(100))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            let id: uuid::Uuid = r.headers()["x-gateway-request-id"]
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(r.bytes().await.unwrap(), wire.as_bytes());
            tokio::time::timeout(Duration::from_secs(3), stopped)
                .await
                .unwrap()
                .unwrap();
            drop(tx);
            id
        };
        h.runtime.wait_requests().await;
        // 받은 "Hello"(5 token)를 세어 정산한다. 미정산으로 남기지 않는다.
        assert_eq!(count_calls(&h).await, counted("Hello"));
        assert_eq!(
            finance::settlement(h.db.store.pool(), id).await,
            finance::cutoff_received("0.000110000000", 5)
        );
        let detail = cutoff_detail(&h, id).await;
        assert_eq!(
            (detail.1.as_str(), detail.2.as_str()),
            (
                "failed",
                if broken {
                    "upstream_transport_error"
                } else {
                    "provider_error"
                }
            )
        );
        next_is_not_blocked(&h).await;
        h.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn stream_cut_before_the_first_event_is_zero_and_does_not_block() {
    // client가 첫 event 전에 끊는다. 응답 헤더도 아직 받지 않았다.
    let h = Harness::new().await;
    h.provider.count_by_content().await;
    let (tx, stopped) = h.provider.stream_messages().await;
    let request = tokio::spawn(h.request("/v1/messages", stream_body(100)).send());
    tokio::time::timeout(Duration::from_secs(3), async {
        while h.provider.calls.lock().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    request.abort();
    let _ = request.await;
    tokio::time::timeout(Duration::from_secs(3), stopped)
        .await
        .expect("client가 끊으면 Provider 연결도 닫는다")
        .unwrap();
    drop(tx);
    tokio::time::timeout(Duration::from_secs(6), h.runtime.wait_requests())
        .await
        .unwrap();
    assert_eq!(
        finance::last_settlement(h.db.store.pool()).await,
        finance::no_response()
    );
    let reason: (String, String) = sqlx::query_as("SELECT outcome,reason_code FROM gw_requests")
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    assert_eq!(reason, ("cancelled".into(), "client_cancelled".into()));
    assert!(count_calls(&h).await.is_empty());
    next_is_not_blocked(&h).await;
    h.close().await;

    // Provider가 헤더만 보내고 기한까지 첫 event를 보내지 않는다.
    let h = Harness::with_config(configuration_from_yaml(&format!(
        "{}\nlimits:\n  transport_timeout_ms: 1500\n  request_timeout_ms: 6500\n  body_timeout_ms: 1500\n  body_idle_timeout_ms: 1500\n",
        support::YAML
    )))
    .await;
    let (tx, stopped) = h.provider.stream_messages().await;
    // 기한에는 504 응답 또는 연결 종료만 허용한다(실제 송신 뒤의 hard transport 기한).
    match h.request("/v1/messages", stream_body(100)).send().await {
        Ok(r) => {
            assert_eq!(r.status(), 504);
            assert_eq!(r.headers()["x-gateway-error-code"], "upstream_read_timeout");
            let _ = r.bytes().await;
        }
        Err(error) => assert!(
            error.is_request() && !error.is_timeout() && !error.is_connect(),
            "기한의 연결 종료만 허용한다"
        ),
    }
    tokio::time::timeout(Duration::from_secs(3), stopped)
        .await
        .unwrap()
        .unwrap();
    drop(tx);
    h.runtime.wait_requests().await;
    assert_eq!(
        finance::last_settlement(h.db.store.pool()).await,
        finance::no_response()
    );
    let reason: (String, String) = sqlx::query_as("SELECT outcome,reason_code FROM gw_requests")
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    assert_eq!(reason, ("failed".into(), "upstream_read_timeout".into()));
    next_is_not_blocked(&h).await;
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn provider_http_errors_are_zero_and_do_not_block() {
    for status in [400, 429, 529] {
        for stream in [false, true] {
            let h = Harness::new().await;
            let native = json!({"type":"error","error":{"type":"overloaded_error","message":"synthetic provider error"}}).to_string();
            h.provider
                .set(
                    StatusCode::from_u16(status).unwrap(),
                    json_headers(),
                    Bytes::from(native.clone()),
                )
                .await;
            let body = if stream {
                stream_body(100)
            } else {
                BODY.to_owned()
            };
            let r = h.request("/v1/messages", body).send().await.unwrap();
            // 오류 응답은 client에 그대로 전달한다.
            assert_eq!(r.status().as_u16(), status);
            assert_eq!(r.bytes().await.unwrap(), native.as_bytes());
            h.runtime.wait_requests().await;
            assert_eq!(
                finance::last_settlement(h.db.store.pool()).await,
                finance::no_response(),
                "status={status} stream={stream}"
            );
            let row: (String, String, i16) =
                sqlx::query_as("SELECT outcome,reason_code,http_status FROM gw_requests")
                    .fetch_one(h.db.store.pool())
                    .await
                    .unwrap();
            assert_eq!(
                row,
                ("failed".into(), "provider_error".into(), status as i16)
            );
            next_is_not_blocked(&h).await;
            h.close().await;
        }
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn unreadable_provider_error_responses_are_zero_and_do_not_block() {
    // Provider가 HTTP 오류로 답했지만 본문을 읽거나 확인할 수 없다(HTML 오류 페이지, 깨진 JSON).
    // client에는 형식 오류(502)로 알리지만, 오류 응답이므로 0원으로 정산하고 사용자를 막지 않는다.
    for (status, content_type, body) in [
        (502, "text/html", "<html><body>bad gateway</body></html>"),
        (500, "application/json", "not json"),
    ] {
        for stream in [false, true] {
            let h = Harness::new().await;
            h.provider
                .set(
                    StatusCode::from_u16(status).unwrap(),
                    HeaderMap::from_iter([(
                        "content-type".parse().unwrap(),
                        content_type.parse().unwrap(),
                    )]),
                    Bytes::from_static(body.as_bytes()),
                )
                .await;
            let request = if stream {
                stream_body(100)
            } else {
                BODY.to_owned()
            };
            let r = h.request("/v1/messages", request).send().await.unwrap();
            assert_eq!(r.status(), 502, "status={status} stream={stream}");
            assert_eq!(
                r.headers()["x-gateway-error-code"],
                "upstream_protocol_error"
            );
            let _ = r.bytes().await;
            h.runtime.wait_requests().await;
            assert_eq!(
                finance::last_settlement(h.db.store.pool()).await,
                finance::no_response(),
                "status={status} stream={stream}"
            );
            let row: (String, String) =
                sqlx::query_as("SELECT outcome,reason_code FROM gw_requests")
                    .fetch_one(h.db.store.pool())
                    .await
                    .unwrap();
            assert_eq!(row, ("failed".into(), "upstream_protocol_error".into()));
            next_is_not_blocked(&h).await;
            h.close().await;
        }
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn anthropic_stream_cut_after_ping_but_before_message_start_is_zero() {
    // 시작 event(message_start) 전에 ping만 받고 끊겼다. 입력 토큰도 모르므로 0원이다.
    let h = Harness::new().await;
    h.provider.count_by_content().await;
    let wire = event(json!({"type":"ping"}));
    cut_after(&h, stream_body(100), &wire).await;
    assert_eq!(
        finance::last_settlement(h.db.store.pool()).await,
        finance::no_response()
    );
    assert!(count_calls(&h).await.is_empty());
    next_is_not_blocked(&h).await;
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn nonstream_request_cut_before_its_response_is_zero_and_does_not_block() {
    let h = Harness::new().await;
    let (started, release) = h.provider.hold_response().await;
    let request = tokio::spawn(h.request("/v1/messages", BODY).send());
    tokio::time::timeout(Duration::from_secs(3), started)
        .await
        .unwrap()
        .unwrap();
    // Provider가 요청을 받았지만 아직 답하지 않았다. client가 끊는다.
    request.abort();
    let _ = request.await;
    tokio::time::timeout(Duration::from_secs(6), h.runtime.wait_requests())
        .await
        .unwrap();
    drop(release);
    assert_eq!(
        finance::last_settlement(h.db.store.pool()).await,
        finance::no_response()
    );
    let row: (String, String, Option<String>) = sqlx::query_as(
        "SELECT outcome,reason_code,execution_snapshot->>'cutoff_rule' FROM gw_requests",
    )
    .fetch_one(h.db.store.pool())
    .await
    .unwrap();
    // 비스트림 요청에는 끊긴 요청 규칙을 기록하지 않는다.
    assert_eq!(row, ("cancelled".into(), "client_cancelled".into(), None));
    next_is_not_blocked(&h).await;
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn cutoff_rule_setting_is_versioned_replayed_audited_and_fixed_at_admission() {
    let h = Harness::with_config(admin_configuration()).await;
    h.provider.count_by_content().await;
    let get = || {
        admin(
            &h,
            reqwest::Method::GET,
            "/api/admin/settings",
            uuid::Uuid::new_v4(),
            None,
        )
    };
    let (status, _, view) = get().await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(
        view["cutoff_rule"],
        json!({"provider":"anthropic","cutoff_rule":"received","correction":false,"correction_available":false,"version":"1"})
    );
    assert!(view["budget_timezone"].is_object());
    // 이번 단계에서는 Anthropic 규칙만, 두 규칙 중 하나로만 바꾼다.
    for bad in [
        json!({"provider":"openai","cutoff_rule":"max_tokens","expected_version":"1"}),
        json!({"provider":"deepseek","cutoff_rule":"max_tokens","expected_version":"1"}),
        json!({"provider":"anthropic","cutoff_rule":"estimate","expected_version":"1"}),
        json!({"provider":"anthropic","cutoff_rule":"max_tokens","expected_version":"one"}),
        json!({"provider":"anthropic","cutoff_rule":"max_tokens"}),
        json!({"provider":"anthropic","cutoff_rule":"max_tokens","expected_version":"1","extra":1}),
    ] {
        let (status, _, body) = admin(
            &h,
            reqwest::Method::PUT,
            RULE,
            uuid::Uuid::new_v4(),
            Some(bad.clone()),
        )
        .await;
        assert_eq!(
            (status, &body["error"]["code"]),
            (400, &json!("invalid_request")),
            "{bad}"
        );
    }
    // 다른 화면이 먼저 바꾼 뒤의 오래된 버전은 덮어쓰지 않는다.
    let (status, _, stale) = admin(
        &h,
        reqwest::Method::PUT,
        RULE,
        uuid::Uuid::new_v4(),
        Some(json!({"provider":"anthropic","cutoff_rule":"max_tokens","expected_version":"0"})),
    )
    .await;
    assert_eq!(
        (status, &stale["error"]["code"]),
        (409, &json!("version_conflict"))
    );

    // `received` 규칙으로 받은 스트림이 진행 중일 때 규칙을 `max_tokens`로 바꾼다.
    let (before, before_id, upstream) =
        open_stream(&h, stream_body(100), &text_wire("Hello")).await;
    let command = uuid::Uuid::new_v4();
    let input = json!({"provider":"anthropic","cutoff_rule":"max_tokens","expected_version":"1"});
    let (status, replayed, changed) =
        admin(&h, reqwest::Method::PUT, RULE, command, Some(input.clone())).await;
    assert_eq!(status, 200, "{changed}");
    assert_eq!(replayed.as_deref(), Some("false"));
    assert_eq!(changed["operation"], "set_settlement_rule");
    assert_eq!(changed["effect"], "changed");
    assert_eq!(changed["result_version"], "2");
    assert_eq!(
        changed["cutoff_rule"],
        json!({"provider":"anthropic","cutoff_rule":"max_tokens","correction":false,"version":"2"})
    );
    // 같은 명령 id를 다시 보내면 결과를 재생하고 다시 바꾸지 않는다. 같은 id로 다른 입력은 충돌이다.
    let (status, replayed, again) =
        admin(&h, reqwest::Method::PUT, RULE, command, Some(input)).await;
    assert_eq!(status, 200);
    assert_eq!(replayed.as_deref(), Some("true"));
    assert_eq!(again["replayed"], true);
    assert_eq!(again["result_version"], "2");
    let (status, _, conflict) = admin(
        &h,
        reqwest::Method::PUT,
        RULE,
        command,
        Some(json!({"provider":"anthropic","cutoff_rule":"received","expected_version":"2"})),
    )
    .await;
    assert_eq!(
        (status, &conflict["error"]["code"]),
        (409, &json!("command_conflict"))
    );
    // 지금 규칙과 같은 값은 바꾸지 않는다(버전 유지, 감사 기록 없음).
    let (status, _, noop) = admin(
        &h,
        reqwest::Method::PUT,
        RULE,
        uuid::Uuid::new_v4(),
        Some(json!({"provider":"anthropic","cutoff_rule":"max_tokens","expected_version":"2"})),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        (&noop["effect"], &noop["result_version"]),
        (&json!("noop"), &json!("2"))
    );
    let (_, _, view) = get().await;
    assert_eq!(
        view["cutoff_rule"],
        json!({"provider":"anthropic","cutoff_rule":"max_tokens","correction":false,"correction_available":false,"version":"2"})
    );

    // 바꾸기 전에 받은 요청은 받을 때의 규칙(`received`)으로 정산한다.
    cut(&h, before, upstream).await;
    assert_eq!(count_calls(&h).await, counted("Hello"));
    assert_eq!(
        finance::settlement(h.db.store.pool(), before_id).await,
        finance::cutoff_received("0.000110000000", 5)
    );
    // 바꾼 뒤에 받은 요청은 새 규칙(`max_tokens`)으로 정산하고 세지 않는다.
    let after_id = cut_after(&h, stream_body(100), &text_wire("Hello")).await;
    assert_eq!(count_calls(&h).await.len(), 2);
    assert_eq!(
        finance::settlement(h.db.store.pool(), after_id).await,
        max_tokens("0.000300000000", 100, None)
    );
    let rules: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT execution_snapshot->>'cutoff_rule' FROM gw_requests WHERE id=ANY($1) ORDER BY admitted_at",
    )
    .bind(vec![before_id, after_id])
    .fetch_all(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(
        rules,
        vec![Some("received".into()), Some("max_tokens".into())]
    );

    // 감사 기록은 실제로 바꾼 한 번만 남는다. 명령 기록은 바꾼 명령과 같은 값 명령 둘이다.
    let audit: Vec<(Value, Value)> = sqlx::query_as(
        "SELECT before_snapshot,after_snapshot FROM gw_audit_events WHERE operation='set_settlement_rule'",
    )
    .fetch_all(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(
        audit,
        vec![(
            json!({"provider":"anthropic","cutoff_rule":"received","correction":false,"version":"1"}),
            json!({"provider":"anthropic","cutoff_rule":"max_tokens","correction":false,"version":"2"})
        )]
    );
    let commands: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gw_commands WHERE operation='set_settlement_rule'",
    )
    .fetch_one(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(commands, 2);
    let updated_by: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT updated_by FROM gw_settlement_rules WHERE provider='anthropic'")
            .fetch_one(h.db.store.pool())
            .await
            .unwrap();
    assert_eq!(updated_by, Some(h.principal));
    h.close().await;
}

/// 장부 시험용: 시작 event가 알린 입력 100 token.
fn start() -> StartUsage {
    StartUsage {
        actual_model: "synthetic-text".into(),
        input_tokens: 100,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    }
}
fn cutoff(received_output: Option<u64>, count_failure: Option<&'static str>) -> NoUsage {
    NoUsage::Cutoff {
        start: start(),
        received_output,
        count_failure,
    }
}
fn ended(
    outcome: FinalOutcome,
    reason: &'static str,
    http_status: Option<u16>,
) -> FinalObservation {
    FinalObservation {
        outcome,
        usage: None,
        counted_input_tokens: None,
        reason,
        http_status,
        provider_request_id: None,
    }
}
/// 장부에 스트림 요청 하나를 받고 송신 권한을 받는다. 받을 때의 규칙이 실행 기록에 남는다.
async fn dispatched(
    ledger: &PgLedger,
    current: &m_gated::ledger::CurrentHuman,
    stream: bool,
) -> uuid::Uuid {
    let mut spec = specification();
    spec.stream = stream;
    ledger.admit_request(current, &spec).await.unwrap();
    ledger.authorize_dispatch(spec.id).await.unwrap();
    spec.id
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn ledger_replays_the_same_settlement_as_noop_and_rejects_a_different_one() {
    let db = postgres::TestDb::new().await;
    postgres::grant_access(&db.store, &configuration(), ACCESS).await;
    let principal = finance::human(db.store.pool()).await;
    let current = finance::current(db.store.pool(), principal).await;
    let ledger = PgLedger::new(db.store.clone(), [1; 32]);
    let cancelled = ended(FinalOutcome::Cancelled, "client_cancelled", None);
    // 시작 event 뒤에 끊긴 스트림: 같은 관측·같은 정산은 noop, 정산이 다르면 충돌이다.
    let id = dispatched(&ledger, &current, true).await;
    assert_eq!(
        ledger.cutoff_rule(id).await.unwrap().as_deref(),
        Some("received")
    );
    let settlement = cutoff(Some(7), None);
    assert_eq!(
        ledger
            .finalize_request_with(id, &cancelled, &settlement)
            .await
            .unwrap(),
        Finalized::Settled
    );
    assert_eq!(
        ledger
            .finalize_request_with(id, &cancelled, &settlement)
            .await
            .unwrap(),
        Finalized::Noop
    );
    for different in [
        cutoff(Some(8), None),
        cutoff(None, Some("cutoff_count_unavailable")),
        NoUsage::NoResponse,
        NoUsage::Unknown,
    ] {
        assert!(
            matches!(
                ledger
                    .finalize_request_with(id, &cancelled, &different)
                    .await,
                Err(LedgerError::Conflict)
            ),
            "{different:?}"
        );
    }
    // 같은 정산이라도 관측이 다르면 다른 마감이다.
    let shutdown = ended(FinalOutcome::Cancelled, "shutdown_cancelled", None);
    assert!(matches!(
        ledger
            .finalize_request_with(id, &shutdown, &settlement)
            .await,
        Err(LedgerError::Conflict)
    ));
    assert_eq!(
        finance::settlement(db.store.pool(), id).await,
        finance::cutoff_received("0.000114000000", 7)
    );

    // 0원 정산도 같은 방식이다.
    let id = dispatched(&ledger, &current, true).await;
    let failed = ended(FinalOutcome::Failed, "provider_error", Some(429));
    assert_eq!(
        ledger
            .finalize_request_with(id, &failed, &NoUsage::NoResponse)
            .await
            .unwrap(),
        Finalized::Settled
    );
    assert_eq!(
        ledger
            .finalize_request_with(id, &failed, &NoUsage::NoResponse)
            .await
            .unwrap(),
        Finalized::Noop
    );
    for different in [settlement, NoUsage::Unknown] {
        assert!(matches!(
            ledger.finalize_request_with(id, &failed, &different).await,
            Err(LedgerError::Conflict)
        ));
    }
    assert_eq!(
        finance::settlement(db.store.pool(), id).await,
        finance::no_response()
    );
    // 미정산으로 남은 것이 없으므로 사용자를 막지 않는다.
    let totals: (i64, i64, String) = sqlx::query_as(
        "SELECT unknown_count,settled_count,cost_usd::text FROM test_user_usage_totals",
    )
    .fetch_one(db.store.pool())
    .await
    .unwrap();
    assert_eq!(totals, (0, 2, "0.000114000000".into()));
    db.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn ledger_settles_cut_offs_by_the_rule_recorded_at_admission() {
    let db = postgres::TestDb::new().await;
    postgres::grant_access(&db.store, &configuration(), ACCESS).await;
    let principal = finance::human(db.store.pool()).await;
    let current = finance::current(db.store.pool(), principal).await;
    let ledger = PgLedger::new(db.store.clone(), [1; 32]);
    let cancelled = ended(FinalOutcome::Cancelled, "client_cancelled", None);
    let pool = db.store.pool();
    let settle = |id, settlement| {
        let ledger = &ledger;
        let cancelled = &cancelled;
        async move {
            ledger
                .finalize_request_with(id, cancelled, &settlement)
                .await
                .unwrap();
            finance::settlement(pool, id).await
        }
    };
    // `received`: 센 출력은 max_tokens(100)를 넘지 않는다. 세지 못했으면 그 까닭과 함께 max_tokens로 정산한다.
    let id = dispatched(&ledger, &current, true).await;
    assert_eq!(
        settle(id, cutoff(Some(500), None)).await,
        finance::cutoff_received("0.000300000000", 100)
    );
    let id = dispatched(&ledger, &current, true).await;
    assert_eq!(
        settle(id, cutoff(None, Some("cutoff_content_too_large"))).await,
        max_tokens("0.000300000000", 100, Some("cutoff_content_too_large"))
    );
    // 규칙을 바꾸기 전에 받은 요청은 바꾼 뒤에 마감해도 받을 때의 규칙을 쓴다.
    let early = dispatched(&ledger, &current, true).await;
    sqlx::query("UPDATE gw_settlement_rules SET cutoff_rule='max_tokens',version=version+1 WHERE provider='anthropic'")
        .execute(db.store.pool())
        .await
        .unwrap();
    assert_eq!(
        settle(early, cutoff(Some(7), None)).await,
        finance::cutoff_received("0.000114000000", 7)
    );
    // `max_tokens`: 센 값이 있어도 max_tokens로 정산하고 대체 사유는 없다.
    let id = dispatched(&ledger, &current, true).await;
    assert_eq!(
        ledger.cutoff_rule(id).await.unwrap().as_deref(),
        Some("max_tokens")
    );
    assert_eq!(
        settle(id, cutoff(Some(7), None)).await,
        max_tokens("0.000300000000", 100, None)
    );
    // 규칙을 기록하지 않은 요청(비스트림)은 끊긴 요청 정산을 받아도 미정산으로 남긴다(최종 usage 없음).
    let id = dispatched(&ledger, &current, false).await;
    assert_eq!(ledger.cutoff_rule(id).await.unwrap(), None);
    assert_eq!(
        settle(id, cutoff(Some(7), None)).await,
        ("unknown".into(), None, None, None, None)
    );
    db.close().await;
}
