use super::*;
use m_gated::config::ApiKind;
use std::time::Duration;
pub(super) const START: &str = "event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_fixture\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"synthetic-text\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":100,\"output_tokens\":0}}}\r\n\r\n";
pub(super) const END: &str = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\ndata: \"usage\":{\"output_tokens\":20}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
fn stream_body() -> String {
    BODY.replacen('{', "{\"stream\":true,", 1)
}
async fn idle(h: &Harness) {
    tokio::time::timeout(Duration::from_secs(6), h.runtime.wait_requests())
        .await
        .expect("감독 task/body owner 모두 해제");
    assert_eq!(h.runtime.available_inference(), 16);
    assert_eq!(h.runtime.active_principals(), 0);
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn native_sse_and_alias_preserve_original_bytes_and_settle_once() {
    let h = Harness::new().await;
    let wire = format!(
        "{START}: ping 합성\r\n\r\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"합성\\uD83D\\uDE00\"}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n{END}"
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
    for path in ["/anthropic/v1/messages", "/v1/messages"] {
        let response = h.request(path, stream_body()).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let id: uuid::Uuid = response.headers()["x-gateway-request-id"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(response.bytes().await.unwrap(), wire.as_bytes());
        idle(&h).await;
        let row=sqlx::query("SELECT accounting_state,cost_usd::text AS cost,execution_stopped_at IS NOT NULL AS stopped,execution_snapshot->>'stream' AS stream FROM gw_requests WHERE id=$1").bind(id).fetch_one(h.db.store.pool()).await.unwrap();
        assert_eq!(row.get::<String, _>("accounting_state"), "settled");
        assert_eq!(row.get::<String, _>("cost"), "0.000140000000");
        assert!(row.get::<bool, _>("stopped"));
        assert_eq!(row.get::<String, _>("stream"), "true");
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM gw_usage_events WHERE request_id=$1 AND kind='final'"
            )
            .bind(id)
            .fetch_one(h.db.store.pool())
            .await
            .unwrap(),
            1
        );
    }
    let calls = h.provider.calls.lock().await;
    assert_eq!(calls.len(), 2);
    for c in calls.iter() {
        assert_eq!(c.body, stream_body());
        assert_eq!(c.headers["x-api-key"], "synthetic-company-key");
        assert_eq!(c.path, "/v1/messages");
    }
    drop(calls);
    h.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn malformed_missing_conflicting_sse_terminal_stays_unknown_and_blocks_paid() {
    for tail in [
        "".to_string(),
        END.replace("\"output_tokens\":20", "\"output_tokens\":-1"),
        format!("{END}event: ping\ndata: {{\"type\":\"ping\",\"broken\":[1,]}}\n\n"),
        format!("{END}{END}"),
        END.replace(
            "\"output_tokens\":20",
            "\"output_tokens\":20,\"output_tokens\":21",
        ),
    ] {
        let h = Harness::new().await;
        let (tx, stopped) = h.provider.stream().await;
        let pending = h.request("/v1/messages", stream_body()).send();
        let producer = async {
            tx.send(Bytes::from_static(START.as_bytes())).await.unwrap();
        };
        let (response, ()) = tokio::join!(pending, producer);
        let response = response.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        // 실제200을 받은 뒤에만 terminal/error/EOF를 해제한다. H1과 H0 경합을 섞지 않는다.
        if !tail.is_empty() {
            tx.send(Bytes::from(tail)).await.unwrap();
        }
        drop(tx);
        assert!(response.bytes().await.is_err());
        tokio::time::timeout(Duration::from_secs(2), stopped)
            .await
            .unwrap()
            .unwrap();
        idle(&h).await;
        let row=sqlx::query("SELECT accounting_state,cost_usd::text AS cost,execution_stopped_at IS NOT NULL AS stopped FROM gw_requests").fetch_one(h.db.store.pool()).await.unwrap();
        assert_eq!(row.get::<String, _>("accounting_state"), "unknown");
        assert!(row.get::<Option<String>, _>("cost").is_none());
        assert!(row.get::<bool, _>("stopped"));
        let next = h.request("/v1/messages", BODY).send().await.unwrap();
        assert_eq!(next.status(), StatusCode::FORBIDDEN);
        drop(next);
        idle(&h).await;
        assert_eq!(h.provider.calls.lock().await.len(), 1);
        h.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn full_queue_cancel_finalizes_real_sql_without_task_or_permit_leak() {
    let h = Harness::new().await;
    let (tx, upstream_stopped) = h.provider.stream().await;
    let addr = h.base.trim_start_matches("http://");
    let socket = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut builder = h2::client::Builder::new();
    builder.initial_window_size(0);
    let (mut sender, connection) = builder.handshake::<_, Bytes>(socket).await.unwrap();
    let connection = tokio::spawn(connection);
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(format!("{}/v1/messages", h.base))
        .header("authorization", format!("Bearer {}", h.token))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(())
        .unwrap();
    let (response, mut send) = sender.send_request(request, false).unwrap();
    send.send_data(Bytes::from(stream_body()), true).unwrap();
    tx.send(Bytes::from_static(START.as_bytes())).await.unwrap();
    let response = response.await.unwrap();
    assert_eq!(response.status(), 200);
    let producer = tokio::spawn(async move {
        let wire = format!(
            "event: future_event\ndata: {{\"type\":\"future_event\",\"text\":\"{}\"}}\n\n",
            "x".repeat(16 * 1024)
        );
        for _ in 0..256 {
            if tx.send(Bytes::from(wire.clone())).await.is_err() {
                break;
            }
        }
        std::future::pending::<()>().await;
    });
    let mut changes = h.runtime.resource_changes();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if h.runtime.request_resources().queue_waiters > 0 {
                break;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("실제64KiB byte 상한에서 생산자 대기");
    assert_eq!(h.runtime.available_inference(), 15);
    assert_eq!(h.runtime.active_requests(), 1);
    send.send_reset(h2::Reason::CANCEL);
    drop(response);
    tokio::time::timeout(Duration::from_secs(2), upstream_stopped)
        .await
        .expect("upstream 즉시 drop")
        .unwrap();
    idle(&h).await;
    let metrics = h.runtime.request_resources();
    assert_eq!(metrics.queue_bytes, 0);
    assert!(metrics.queue_peak > 48 * 1024 && metrics.queue_peak <= 64 * 1024);
    assert_eq!(metrics.queue_waiters, 0);
    assert!(metrics.parser_peak <= 256 * 1024);
    assert_eq!(metrics.finished, 1);
    let row = sqlx::query(
        "SELECT accounting_state,execution_stopped_at IS NOT NULL AS stopped FROM gw_requests",
    )
    .fetch_one(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("accounting_state"), "unknown");
    assert!(row.get::<bool, _>("stopped"));
    assert_eq!(h.provider.calls.lock().await.len(), 1);
    producer.abort();
    let _ = producer.await;
    drop(sender);
    connection.abort();
    let _ = connection.await;
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn absolute_transport_deadline_cancels_upstream_and_reserves_finalization_budget() {
    let yaml = format!(
        "{}\nlimits:\n  transport_timeout_ms: 1500\n  request_timeout_ms: 6500\n  body_timeout_ms: 1500\n  body_idle_timeout_ms: 1500\n",
        support::YAML
    );
    let cfg = configuration_from_yaml(&yaml);
    let h = Harness::with_config(cfg).await;
    let (tx, stopped) = h.provider.stream().await;
    tx.send(Bytes::from_static(START.as_bytes())).await.unwrap();
    let response = h
        .request("/v1/messages", stream_body())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let id: uuid::Uuid = response.headers()["x-gateway-request-id"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let result = response.bytes().await;
    assert!(
        result.is_err(),
        "기한 종료를 정상 chunked EOF로 합성하지 않는다"
    );
    tokio::time::timeout(Duration::from_secs(1), stopped)
        .await
        .unwrap()
        .unwrap();
    idle(&h).await;
    let row=sqlx::query("SELECT accounting_state,execution_stopped_at IS NOT NULL AS stopped,extract(epoch from execution_deadline-transport_deadline)::bigint AS gap FROM gw_requests WHERE id=$1").bind(id).fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(row.get::<String, _>("accounting_state"), "unknown");
    assert!(row.get::<bool, _>("stopped"));
    assert_eq!(row.get::<i64, _>("gap"), 5);
    assert_eq!(h.provider.calls.lock().await.len(), 1);
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn immediate_malformed_sse_before_headers_has_native_502_and_unknown() {
    let h = Harness::new().await;
    h.provider
        .set(
            StatusCode::OK,
            HeaderMap::from_iter([(
                "content-type".parse().unwrap(),
                "text/event-stream".parse().unwrap(),
            )]),
            Bytes::from_static(b"data: {bad}\n\n"),
        )
        .await;
    let response = h
        .request("/v1/messages", stream_body())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    assert_eq!(
        response.headers()["x-gateway-error-code"],
        "stream_parse_error"
    );
    assert!(!response.headers().contains_key("x-gateway-dispatch-state"));
    let body = response.bytes().await.unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["type"],
        "error"
    );
    idle(&h).await;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT accounting_state FROM gw_requests")
            .fetch_one(h.db.store.pool())
            .await
            .unwrap(),
        "unknown"
    );
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn slow_nonstream_bodies_hold_four_slots_until_transport_deadline_and_release_bytes() {
    let yaml = format!(
        "{}\nlimits:\n  transport_timeout_ms: 3000\n  request_timeout_ms: 8000\n  body_timeout_ms: 3000\n  body_idle_timeout_ms: 3000\n",
        finance_yaml()
    );
    let h = Harness::with_config(configuration_from_yaml(&yaml)).await;
    let prefix = "{\"id\":\"msg_fixture\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"synthetic-text\",\"content\":[{\"type\":\"text\",\"text\":\"";
    let suffix =
        "\"}],\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":100,\"output_tokens\":20}}";
    let wire = format!(
        "{prefix}{}{suffix}",
        "x".repeat(16 * 1024 * 1024 - prefix.len() - suffix.len())
    );
    h.provider
        .set(
            StatusCode::OK,
            HeaderMap::from_iter([(
                "content-type".parse().unwrap(),
                "application/json".parse().unwrap(),
            )]),
            Bytes::from(wire),
        )
        .await;
    let mut builder = h2::client::Builder::new();
    builder.initial_window_size(0);
    let (mut sender, connection) = builder
        .handshake::<_, Bytes>(
            tokio::net::TcpStream::connect(h.base.trim_start_matches("http://"))
                .await
                .unwrap(),
        )
        .await
        .unwrap();
    let connection = tokio::spawn(connection);
    let mut bodies = Vec::new();
    for _ in 0..4 {
        let request = axum::http::Request::builder()
            .method("POST")
            .uri(format!("{}/v1/messages", h.base))
            .header("authorization", format!("Bearer {}", h.token))
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .body(())
            .unwrap();
        let (response, mut send) = sender.send_request(request, false).unwrap();
        send.send_data(Bytes::from_static(BODY.as_bytes()), true)
            .unwrap();
        let response = response.await.unwrap();
        assert_eq!(response.status(), 200);
        bodies.push(response.into_body());
    }
    assert_eq!(h.runtime.available_nonstream(), 0);
    assert_eq!(h.runtime.available_inference(), 12);
    assert_eq!(h.runtime.request_resources().body_bytes, 64 * 1024 * 1024);
    let response = h.request("/v1/messages", BODY).send().await.unwrap();
    assert_eq!(response.status(), 429);
    drop(response);
    assert_eq!(h.provider.calls.lock().await.len(), 4);
    let (response, _) = sender
        .send_request(
            axum::http::Request::builder()
                .uri(format!("{}/livez", h.base))
                .body(())
                .unwrap(),
            true,
        )
        .unwrap();
    assert_eq!(response.await.unwrap().status(), 200);
    for body in &mut bodies {
        let result = tokio::time::timeout(Duration::from_secs(3), body.data())
            .await
            .unwrap();
        assert!(result.unwrap().is_err(), "기한 뒤 개별reset");
    }
    idle(&h).await;
    assert_eq!(h.runtime.available_nonstream(), 4);
    assert_eq!(h.runtime.request_resources().body_bytes, 0);
    assert_eq!(h.runtime.request_resources().body_peak, 64 * 1024 * 1024);
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT accounting_state FROM gw_requests WHERE dispatch_authorized_at IS NOT NULL",
    )
    .fetch_all(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(rows, vec!["settled"; 4]);
    assert_eq!(h.provider.calls.lock().await.len(), 4);
    drop(bodies);
    drop(sender);
    connection.abort();
    let _ = connection.await;
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn body_eof_does_not_release_permit_before_real_sql_finalization_finishes() {
    let h = Harness::new().await;
    let (tx, _) = h.provider.stream().await;
    tx.send(Bytes::from_static(START.as_bytes())).await.unwrap();
    let response = h
        .request("/v1/messages", stream_body())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut barrier = h.db.store.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM gw_principals WHERE id=$1 FOR UPDATE")
        .bind(h.principal)
        .execute(&mut *barrier)
        .await
        .unwrap();
    tx.send(Bytes::from_static(END.as_bytes())).await.unwrap();
    drop(tx);
    response.bytes().await.unwrap();
    assert_eq!(
        h.runtime.available_inference(),
        15,
        "body EOF도 DB 마감 완료 전 permit을 반환하지 않는다"
    );
    assert_eq!(h.runtime.active_requests(), 1);
    assert_eq!(h.runtime.request_resources().queue_bytes, 0);
    barrier.rollback().await.unwrap();
    idle(&h).await;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT accounting_state FROM gw_requests")
            .fetch_one(h.db.store.pool())
            .await
            .unwrap(),
        "settled"
    );
    h.close().await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn real_sql_finalization_failure_keeps_only_root_id_latch_and_releases_resources() {
    let h = Harness::new().await;
    let (tx, _) = h.provider.stream().await;
    tx.send(Bytes::from_static(START.as_bytes())).await.unwrap();
    let response = h
        .request("/v1/messages", stream_body())
        .send()
        .await
        .unwrap();
    let id: uuid::Uuid = response.headers()["x-gateway-request-id"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    sqlx::raw_sql("CREATE FUNCTION gw_test_s3_fail_final() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic finalization failure'; END $$; CREATE TRIGGER gw_test_s3_fail_final BEFORE UPDATE ON gw_requests FOR EACH ROW WHEN (NEW.phase='finished') EXECUTE FUNCTION gw_test_s3_fail_final();").execute(h.db.store.pool()).await.unwrap();
    tx.send(Bytes::from_static(END.as_bytes())).await.unwrap();
    drop(tx);
    let _ = response.bytes().await;
    idle(&h).await;
    assert_eq!(h.runtime.failed_request_ids(), vec![id]);
    assert_eq!(h.runtime.lifecycle(), m_gated::runtime::Lifecycle::NotReady);
    assert_eq!(h.runtime.request_resources().body_bytes, 0);
    assert_eq!(h.runtime.request_resources().queue_bytes, 0);
    let row = sqlx::query(
        "SELECT phase,accounting_state,cost_usd IS NULL AS no_cost FROM gw_requests WHERE id=$1",
    )
    .bind(id)
    .fetch_one(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("phase"), "dispatching");
    assert_eq!(row.get::<String, _>("accounting_state"), "pending");
    assert!(row.get::<bool, _>("no_cost"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM gw_usage_events")
            .fetch_one(h.db.store.pool())
            .await
            .unwrap(),
        0
    );
    let next = h.request("/v1/messages", BODY).send().await.unwrap();
    assert_eq!(next.status(), 503);
    drop(next);
    assert_eq!(h.provider.calls.lock().await.len(), 1);
    sqlx::raw_sql(
        "DROP TRIGGER gw_test_s3_fail_final ON gw_requests; DROP FUNCTION gw_test_s3_fail_final();",
    )
    .execute(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(
        h.runtime.lifecycle(),
        m_gated::runtime::Lifecycle::NotReady,
        "DB 회복만으로 root durable 마감/latch 해제를 주장하지 않는다"
    );
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn idle_upstream_h2_client_reset_cancels_even_while_body_poll_is_pending() {
    let h = Harness::new().await;
    let (tx, stopped) = h.provider.stream().await;
    tx.send(Bytes::from_static(START.as_bytes())).await.unwrap();
    let (mut client, connection) = h2::client::handshake(
        tokio::net::TcpStream::connect(h.base.trim_start_matches("http://"))
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    let connection = tokio::spawn(connection);
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(format!("{}/v1/messages", h.base))
        .header("authorization", format!("Bearer {}", h.token))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(())
        .unwrap();
    let (response, mut send) = client.send_request(request, false).unwrap();
    send.send_data(Bytes::from(stream_body()), true).unwrap();
    let response = response.await.unwrap();
    let mut body = response.into_body();
    assert!(
        body.data()
            .await
            .unwrap()
            .unwrap()
            .starts_with(b"event: message_start")
    );
    send.send_reset(h2::Reason::CANCEL);
    drop(body);
    tokio::time::timeout(Duration::from_secs(2), stopped)
        .await
        .expect("body Pending 중에도 client RST가 upstream 취소를 깨워야 한다")
        .unwrap();
    idle(&h).await;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT accounting_state FROM gw_requests")
            .fetch_one(h.db.store.pool())
            .await
            .unwrap(),
        "unknown"
    );
    drop(tx);
    drop(client);
    connection.abort();
    let _ = connection.await;
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn streaming_native_features_preserve_image_tool_thinking_schema_and_cache_usage() {
    let yaml=support::YAML.replace("fixture_version: text-v1","fixture_version: native-v1\n        features: [client_tools, thinking, cache, json_schema, inline_png]\n        approved_beta: [synthetic-native-2026-01-01]").replace("max_output_tokens: 1024","max_output_tokens: 4096").replace("output: '2.000000000000'","output: '2.000000000000'\n          cache_read: '0.100000000000'\n          cache_write: '2.500000000000'");
    let h = Harness::with_config(configuration_from_yaml(&yaml)).await;
    let mut request = serde_json::json!({"model":"synthetic-text","max_tokens":2048,"system":[{"type":"text","text":"synthetic instruction","cache_control":{"type":"ephemeral","ttl":"5m"}}],"messages":[{"role":"user","content":[{"type":"text","text":"synthetic text"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScLbtAAAAABJRU5ErkJggg=="}}]},{"role":"assistant","content":[{"type":"tool_use","id":"tool_1","name":"synthetic_tool","input":{"schema":"not a schema position","nested":{"value":1}}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool_1","content":"synthetic result","is_error":false}]}],"tools":[{"name":"synthetic_tool","description":"synthetic client tool","input_schema":{"type":"object","properties":{"value":{"type":"number"}}}}],"tool_choice":{"type":"auto","disable_parallel_tool_use":true},"thinking":{"type":"enabled","budget_tokens":1024},"output_config":{"format":{"type":"json_schema","schema":{"type":"object","properties":{"answer":{"type":"string"}}}}}});
    request["stream"] = serde_json::json!(true);
    let body = serde_json::to_vec_pretty(&request).unwrap();
    let start=START.replace("\"output_tokens\":0","\"output_tokens\":0,\"cache_read_input_tokens\":4,\"cache_creation_input_tokens\":8,\"cache_creation\":{\"ephemeral_5m_input_tokens\":8,\"ephemeral_1h_input_tokens\":0}");
    let mut wire = start;
    for (index, block, deltas) in [
        (
            0,
            serde_json::json!({"type":"thinking","thinking":"","signature":""}),
            vec![
                serde_json::json!({"type":"thinking_delta","thinking":"synthetic thinking"}),
                serde_json::json!({"type":"signature_delta","signature":"synthetic-signature"}),
            ],
        ),
        (
            1,
            serde_json::json!({"type":"text","text":"","citations":null}),
            vec![serde_json::json!({"type":"text_delta","text":"{\"answer\":\"synthetic\"}"})],
        ),
        (
            2,
            serde_json::json!({"type":"tool_use","id":"tool_2","name":"synthetic_tool","input":{}}),
            vec![
                serde_json::json!({"type":"input_json_delta","partial_json":"{\"value\":"}),
                serde_json::json!({"type":"input_json_delta","partial_json":"1}"}),
            ],
        ),
    ] {
        let block =
            serde_json::json!({"type":"content_block_start","index":index,"content_block":block});
        wire.push_str(&format!("event: content_block_start\ndata: {block}\n\n"));
        for delta in deltas {
            let event =
                serde_json::json!({"type":"content_block_delta","index":index,"delta":delta});
            wire.push_str(&format!("event: content_block_delta\ndata: {event}\n\n"));
        }
        let stop = serde_json::json!({"type":"content_block_stop","index":index});
        wire.push_str(&format!("event: content_block_stop\ndata: {stop}\n\n"));
    }
    wire.push_str(&END.replace("end_turn", "tool_use"));
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
    let response = h
        .request("/anthropic/v1/messages", body.clone())
        .header("anthropic-beta", "synthetic-native-2026-01-01")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(
        response.bytes().await.unwrap() == wire.as_bytes(),
        "정상 native tool/thinking 원본 불일치"
    );
    idle(&h).await;
    assert_eq!(h.provider.calls.lock().await[0].body, body);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT cost_usd::text FROM gw_requests")
            .fetch_one(h.db.store.pool())
            .await
            .unwrap(),
        "0.000160400000"
    );
    sdk(&h, "/anthropic", "messages_stream_features", 200, &h.token).await;
    idle(&h).await;
    assert_eq!(h.provider.calls.lock().await.len(), 2);
    let costs: Vec<String> =
        sqlx::query_scalar("SELECT cost_usd::text FROM gw_requests ORDER BY admitted_at")
            .fetch_all(h.db.store.pool())
            .await
            .unwrap();
    assert_eq!(costs, vec!["0.000160400000", "0.000160400000"]);
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn streaming_total_is_not_limited_by_nonstream_model_json_bytes() {
    use sha2::{Digest, Sha256};
    for chunked in [false, true] {
        let h = Harness::new().await;
        let mut wire = START.to_string();
        for _ in 0..2 {
            wire.push_str("event: future_event\ndata: {\"type\":\"future_event\",\"output\":\"");
            wire.push_str(&"x".repeat(9 * 1024 * 1024));
            wire.push_str("\"}\n\n");
        }
        wire.push_str(END);
        let wire = Bytes::from(wire);
        let expected = Sha256::digest(&wire);
        let length = wire.len();
        let producer = if chunked {
            let (tx, _) = h.provider.stream().await;
            Some(tokio::spawn(async move {
                for chunk in wire.chunks(16 * 1024) {
                    if tx.send(Bytes::copy_from_slice(chunk)).await.is_err() {
                        break;
                    }
                }
            }))
        } else {
            h.provider
                .set(
                    StatusCode::OK,
                    HeaderMap::from_iter([(
                        "content-type".parse().unwrap(),
                        "text/event-stream".parse().unwrap(),
                    )]),
                    wire,
                )
                .await;
            None
        };
        let response = h
            .request("/v1/messages", stream_body())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let received = response
            .bytes()
            .await
            .expect("정상 다중event SSE를 nonstream16MiB JSON 한도로 자르면 안 된다");
        assert_eq!(received.len(), length);
        assert_eq!(Sha256::digest(&received), expected);
        drop(received);
        if let Some(producer) = producer {
            producer.await.unwrap();
        }
        idle(&h).await;
        assert!(h.runtime.request_resources().parser_peak <= 256 * 1024);
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT accounting_state FROM gw_requests")
                .fetch_one(h.db.store.pool())
                .await
                .unwrap(),
            "settled"
        );
        assert_eq!(h.provider.calls.lock().await.len(), 1);
        h.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn sse_event_and_total_wire_boundaries_have_distinct_accounting_outcomes() {
    for case in ["event_exact", "event_over", "total_exact", "total_over"] {
        let h = Harness::new().await;
        let start = START.replace("\r\n", "\n");
        let head = "event: future_event\ndata: {\"type\":\"future_event\",\"text\":\"";
        let tail = "\"}\n\n";
        let mut wire = start.clone();
        if case.starts_with("event") {
            let size = 16 * 1024 * 1024 + usize::from(case == "event_over");
            wire.push_str(head);
            wire.push_str(&"x".repeat(size - head.len() - tail.len()));
            wire.push_str(tail);
        } else {
            let remaining = 64 * 1024 * 1024 - start.len() - END.len();
            let mut left = remaining;
            for i in 0..4 {
                let size = if i == 3 { left } else { remaining / 4 };
                left -= size;
                wire.push_str(head);
                wire.push_str(&"x".repeat(size - head.len() - tail.len()));
                wire.push_str(tail);
            }
        }
        wire.push_str(END);
        if case == "total_over" {
            wire.push('\n');
        }
        let expected_bytes = wire.len();
        let (tx, _) = h.provider.stream().await;
        let producer = tokio::spawn(async move {
            for part in wire.as_bytes().chunks(16 * 1024) {
                if tx.send(Bytes::copy_from_slice(part)).await.is_err() {
                    break;
                }
            }
        });
        let response = h
            .request("/v1/messages", stream_body())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let received = response.bytes().await;
        if case.ends_with("exact") {
            assert_eq!(received.unwrap().len(), expected_bytes);
        } else {
            assert!(received.is_err(), "한도 초과를 성공EOF로 바꾸지 않는다");
        }
        producer.await.unwrap();
        idle(&h).await;
        let state = sqlx::query_scalar::<_, String>("SELECT accounting_state FROM gw_requests")
            .fetch_one(h.db.store.pool())
            .await
            .unwrap();
        assert_eq!(
            state,
            if case.ends_with("exact") || case == "total_over" {
                "settled"
            } else {
                "unknown"
            }
        );
        assert_eq!(h.provider.calls.lock().await.len(), 1);
        assert!(h.runtime.request_resources().parser_peak <= 256 * 1024);
        assert_eq!(h.runtime.request_resources().queue_bytes, 0);
        h.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn http1_client_drop_cancels_idle_upstream_and_joins_before_stopped() {
    let h = Harness::new().await;
    let (tx, stopped) = h.provider.stream().await;
    tx.send(Bytes::from_static(START.as_bytes())).await.unwrap();
    let mut response = h
        .request("/v1/messages", stream_body())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(
        response
            .chunk()
            .await
            .unwrap()
            .unwrap()
            .starts_with(b"event: message_start")
    );
    drop(response);
    tokio::time::timeout(Duration::from_secs(2), stopped)
        .await
        .expect("H1 client close가 idle upstream을 즉시 해제")
        .unwrap();
    idle(&h).await;
    let row = sqlx::query(
        "SELECT accounting_state,execution_stopped_at IS NOT NULL AS stopped FROM gw_requests",
    )
    .fetch_one(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("accounting_state"), "unknown");
    assert!(row.get::<bool, _>("stopped"));
    drop(tx);
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn admit_lock_wait_does_not_extend_original_transport_or_execution_deadline() {
    let db = postgres::TestDb::new().await;
    let id = finance::human(db.store.pool()).await;
    let current = finance::current(db.store.pool(), id).await;
    let ledger = Arc::new(PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    ));
    let mut lock = db.store.pool().begin().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM gw_principals WHERE id=$1 FOR UPDATE")
        .bind(id)
        .execute(&mut *lock)
        .await
        .unwrap();
    let origin: String = sqlx::query_scalar("SELECT clock_timestamp()::text")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    let mut spec = specification();
    let observed = tokio::time::Instant::now();
    spec.deadline = m_gated::runtime::RequestDeadline::new(
        observed + Duration::from_secs(2),
        observed + Duration::from_secs(7),
    );
    let request_id = spec.id;
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let b = barrier.clone();
    let job = tokio::spawn(async move {
        b.wait().await;
        ledger.admit_request(&current, &spec).await
    });
    barrier.wait().await;
    // 기한 예산을 소비하는 유한 timer다. 경합 정렬은 뒤의 실제 자기 blocker 확인으로 입증한다.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let waiting:i64=sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid))").bind(blocker).fetch_one(db.store.pool()).await.unwrap();
    assert_eq!(
        waiting, 1,
        "자기 principal row의 실제 대기를 확인한 뒤 barrier 해제"
    );
    lock.rollback().await.unwrap();
    assert_eq!(job.await.unwrap(), Ok(request_id));
    let row=sqlx::query("SELECT ceil(extract(epoch FROM transport_deadline-$2::timestamptz)*1000)::bigint AS transport_ms,ceil(extract(epoch FROM execution_deadline-$2::timestamptz)*1000)::bigint AS execution_ms,extract(epoch FROM execution_deadline-transport_deadline)::bigint AS gap FROM gw_requests WHERE id=$1").bind(request_id).bind(origin).fetch_one(db.store.pool()).await.unwrap();
    let transport = row.get::<i64, _>("transport_ms");
    let execution = row.get::<i64, _>("execution_ms");
    assert!(
        transport <= 2020,
        "DB 잠금 대기가 transport 기한을 연장했다: {transport}ms"
    );
    assert!(
        execution <= 7020,
        "DB 잠금 대기가 execution 기한을 연장했다: {execution}ms"
    );
    assert_eq!(row.get::<i64, _>("gap"), 5);
    db.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn consumed_admit_deadline_rolls_back_rate_totals_and_start_row() {
    let db = postgres::TestDb::new().await;
    let id = finance::human(db.store.pool()).await;
    let current = finance::current(db.store.pool(), id).await;
    let ledger = Arc::new(PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    ));
    let mut lock = db.store.pool().begin().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM gw_principals WHERE id=$1 FOR UPDATE")
        .bind(id)
        .execute(&mut *lock)
        .await
        .unwrap();
    let mut spec = specification();
    let observed = tokio::time::Instant::now();
    spec.deadline = m_gated::runtime::RequestDeadline::new(
        observed + Duration::from_millis(100),
        observed + Duration::from_millis(5100),
    );
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let b = barrier.clone();
    let job = tokio::spawn(async move {
        b.wait().await;
        ledger.admit_request(&current, &spec).await
    });
    barrier.wait().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let waiting:i64=sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid))").bind(blocker).fetch_one(db.store.pool()).await.unwrap();
    assert_eq!(waiting, 1);
    lock.rollback().await.unwrap();
    assert_eq!(job.await.unwrap(), Err(LedgerError::Expired));
    for table in [
        "gw_requests",
        "gw_usage_totals",
        "gw_rate_windows",
        "gw_request_budgets",
    ] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(db.store.pool())
            .await
            .unwrap();
        assert_eq!(n, 0, "소진한 admit가 {table}을 남겼다");
    }
    db.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn runtime_deadline_during_blocked_policy_admit_sends_no_provider_bytes() {
    let yaml = format!(
        "{}\nlimits:\n  transport_timeout_ms: 350\n  request_timeout_ms: 5350\n  body_timeout_ms: 350\n  body_idle_timeout_ms: 350\n",
        finance_yaml()
    );
    let h = Harness::with_config(configuration_from_yaml(&yaml)).await;
    let mut lock = h.db.store.pool().begin().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM gw_policies WHERE principal_id=$1 FOR UPDATE")
        .bind(h.principal)
        .execute(&mut *lock)
        .await
        .unwrap();
    let request = h.request("/v1/messages", BODY);
    let job = tokio::spawn(async move { request.send().await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let waiting:i64=sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid))").bind(blocker).fetch_one(h.db.store.pool()).await.unwrap();
    assert_eq!(
        waiting, 1,
        "실제 자기 policy 대기 뒤 고정 deadline을 관찰한다"
    );
    let result = tokio::time::timeout(Duration::from_secs(2), job)
        .await
        .unwrap()
        .unwrap();
    assert!(
        result.is_err(),
        "헤더 이전 전체 transport 기한은 자기 연결을 닫는다"
    );
    lock.rollback().await.unwrap();
    idle(&h).await;
    assert_eq!(h.provider.calls.lock().await.len(), 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM gw_requests")
            .fetch_one(h.db.store.pool())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM gw_rate_windows")
            .fetch_one(h.db.store.pool())
            .await
            .unwrap(),
        0
    );
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn stream_and_nonstream_share_bounded_response_header_allowlist() {
    const RATE: [&str; 6] = [
        "anthropic-ratelimit-requests-limit",
        "anthropic-ratelimit-requests-remaining",
        "anthropic-ratelimit-input-tokens-limit",
        "anthropic-ratelimit-input-tokens-remaining",
        "anthropic-ratelimit-output-tokens-limit",
        "anthropic-ratelimit-output-tokens-remaining",
    ];
    for (retry, rate, duplicate, accepted) in [
        ("120", "123", false, true),
        ("1", "0", false, true),
        ("121", "bad", false, false),
        ("0", "-1", false, false),
        ("+1", "+1", false, false),
        ("1", "123", true, false),
    ] {
        for stream in [false, true] {
            let h = Harness::new().await;
            let mut headers = HeaderMap::new();
            headers.insert(
                "content-type",
                if stream {
                    "text/event-stream"
                } else {
                    "application/json"
                }
                .parse()
                .unwrap(),
            );
            for (name, value) in [
                ("request-id", "provider-synthetic"),
                ("x-request-id", "provider-other"),
                ("retry-after", retry),
                ("set-cookie", "synthetic=not-forwarded"),
                ("location", "https://not-forwarded.invalid"),
                ("www-authenticate", "not-forwarded"),
            ] {
                headers.insert(name, value.parse().unwrap());
            }
            for name in RATE {
                headers.insert(name, rate.parse().unwrap());
            }
            if duplicate {
                for name in RATE
                    .into_iter()
                    .chain(["retry-after", "request-id", "x-request-id"])
                {
                    let value = headers[name].clone();
                    headers.append(name, value);
                }
            }
            let wire = if stream {
                format!("{START}{END}")
            } else {
                r#"{"id":"msg_fixture","type":"message","role":"assistant","model":"synthetic-text","content":[],"stop_reason":"end_turn","usage":{"input_tokens":100,"output_tokens":20}}"#.to_string()
            };
            h.provider
                .set(StatusCode::OK, headers, Bytes::from(wire.clone()))
                .await;
            let response = h
                .request(
                    "/v1/messages",
                    if stream {
                        stream_body()
                    } else {
                        BODY.to_string()
                    },
                )
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let headers = response.headers();
            assert_eq!(
                headers.get("retry-after").is_some(),
                accepted,
                "stream={stream} retry={retry} duplicate={duplicate}"
            );
            for name in RATE {
                assert_eq!(
                    headers.get(name).is_some(),
                    accepted,
                    "stream={stream} name={name}"
                );
            }
            for name in ["set-cookie", "location", "www-authenticate"] {
                assert!(headers.get(name).is_none());
            }
            for name in ["request-id", "x-request-id"] {
                assert_eq!(headers.get(name).is_some(), !duplicate);
            }
            assert_ne!(headers["x-gateway-request-id"], "provider-synthetic");
            assert_eq!(response.bytes().await.unwrap(), wire.as_bytes());
            idle(&h).await;
            assert_eq!(h.provider.calls.lock().await.len(), 1);
            h.close().await;
        }
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL; waits for actual DB minute boundary"]
async fn saturated_previous_minute_retries_after_authoritative_anchor_for_both_lanes() {
    let db = postgres::TestDb::new().await;
    let ledger = Arc::new(PgLedger::new(
        db.store.clone(),
        AccessSettings::from_config(&configuration()),
        [1; 32],
    ));
    let mut cases = Vec::new();
    for free in [false, true] {
        let id = finance::human(db.store.pool()).await;
        sqlx::query("UPDATE gw_policies SET rpm_limit=1,count_rpm_limit=1 WHERE principal_id=$1")
            .bind(id)
            .execute(db.store.pool())
            .await
            .unwrap();
        let current = finance::current(db.store.pool(), id).await;
        let spec = if free {
            RequestSpec::from_profile(
                ProviderId::Anthropic,
                &configuration().providers()[0].profiles[1],
                None,
            )
        } else {
            specification()
        };
        cases.push((current, spec));
    }
    // DB가 알려 준 다음 경계의 200ms 전까지 기다린다. 경합은 실제 row lock/barrier로 만든다.
    let observed = tokio::time::Instant::now();
    let timing = sqlx::query("WITH n AS MATERIALIZED (SELECT clock_timestamp() t) SELECT (to_timestamp((floor(extract(epoch FROM t)/60)+1)*60))::text AS boundary, ceil(extract(epoch FROM (to_timestamp((floor(extract(epoch FROM t)/60)+1)*60)-t))*1000)::bigint AS ms FROM n").fetch_one(db.store.pool()).await.unwrap();
    let mut boundary: String = timing.get("boundary");
    let mut ms: i64 = timing.get("ms");
    if ms < 1000 {
        boundary = sqlx::query_scalar("SELECT ($1::timestamptz+interval '60 seconds')::text")
            .bind(&boundary)
            .fetch_one(db.store.pool())
            .await
            .unwrap();
        ms += 60_000;
    }
    tokio::time::sleep_until(observed + Duration::from_millis((ms - 200) as u64)).await;
    for (current, spec) in &cases {
        let lane = if spec.api == ApiKind::CountTokens {
            "token_count"
        } else {
            "inference"
        };
        sqlx::query("INSERT INTO gw_rate_windows(scope_id,lane,window_start,window_end,admitted_count) VALUES($1,$2,$3::timestamptz-interval '60 seconds',$3::timestamptz,1)").bind(current.policy_id).bind(lane).bind(&boundary).execute(db.store.pool()).await.unwrap();
    }
    let mut lock = db.store.pool().begin().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    let ids = cases.iter().map(|(c, _)| c.id).collect::<Vec<_>>();
    sqlx::query("SELECT id FROM gw_principals WHERE id=ANY($1) ORDER BY id FOR UPDATE")
        .bind(&ids)
        .execute(&mut *lock)
        .await
        .unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let origin: String = sqlx::query_scalar("SELECT clock_timestamp()::text")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    let start = tokio::time::Instant::now();
    let mut jobs = Vec::new();
    for (current, mut spec) in cases {
        spec.deadline = m_gated::runtime::RequestDeadline::new(
            start + Duration::from_secs(2),
            start + Duration::from_secs(7),
        );
        let b = barrier.clone();
        let ledger = ledger.clone();
        jobs.push(tokio::spawn(async move {
            b.wait().await;
            let result = ledger.admit_request(&current, &spec).await;
            (spec.id, result)
        }));
    }
    barrier.wait().await;
    tokio::time::sleep_until(observed + Duration::from_millis((ms + 40) as u64)).await;
    let waiting:i64=sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid)) AND xact_start < $2::timestamptz").bind(blocker).bind(&boundary).fetch_one(db.store.pool()).await.unwrap();
    let crossed: bool = sqlx::query_scalar("SELECT clock_timestamp()>=$1::timestamptz")
        .bind(&boundary)
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    let began_before_boundary: bool =
        sqlx::query_scalar("SELECT $1::timestamptz < $2::timestamptz")
            .bind(&origin)
            .bind(&boundary)
            .fetch_one(db.store.pool())
            .await
            .unwrap();
    lock.rollback().await.unwrap();
    let mut results = Vec::new();
    for job in jobs {
        results.push(job.await.unwrap());
    }
    let admitted = results.iter().filter(|(id, r)| *r == Ok(*id)).count();
    println!(
        "S6_CLOCK_BOUNDARY admitted={admitted} waiting={waiting} began_before_boundary={began_before_boundary} crossed={crossed} elapsed_ms={}",
        start.elapsed().as_millis()
    );
    for (_, result) in &results {
        if let Err(error) = result {
            // LedgerError는 고정 enum이다. UUID/SQL 예외/본문을 출력하지 않는다.
            println!("S6_CLOCK_ADMIT error={error:?}");
        }
    }
    if admitted != 2 {
        db.close().await;
        assert_eq!(
            admitted, 2,
            "권위 anchor의 실제 두 lane 입장 실패; 고정 enum 진단 참조"
        );
        return;
    }
    let rows=sqlx::query("SELECT r.id,ceil(extract(epoch FROM r.transport_deadline-$1::timestamptz)*1000)::bigint AS transport_ms,extract(epoch FROM r.execution_deadline-r.transport_deadline)::bigint AS gap,b.rate_start >= $2::timestamptz AS new_window FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id").bind(origin).bind(&boundary).fetch_all(db.store.pool()).await.unwrap();
    let requests_valid = rows.len() == 2
        && rows.iter().all(|row| {
            row.get::<bool, _>("new_window")
                && row.get::<i64, _>("transport_ms") <= 2020
                && row.get::<i64, _>("gap") == 5
        });
    let windows=sqlx::query("SELECT window_start >= $1::timestamptz AS new_window,admitted_count FROM gw_rate_windows ORDER BY scope_id,window_start").bind(&boundary).fetch_all(db.store.pool()).await.unwrap();
    let windows_valid = windows.len() == 4
        && windows
            .iter()
            .filter(|r| r.get::<bool, _>("new_window"))
            .count()
            == 2
        && windows
            .iter()
            .all(|r| r.get::<i64, _>("admitted_count") == 1);
    db.close().await;
    assert_eq!(waiting, 2, "실제 자기 principal 잠금 대기를 확인");
    assert!(
        began_before_boundary,
        "두 admit은 실제 DB 분 경계 전에 시작해야 한다"
    );
    assert!(crossed);
    assert!(requests_valid, "새 창/원래2초 transport/마감5초 gap");
    assert!(windows_valid, "원래 포화 창과 새 두 lane의 각 admission1");
}

pub(super) fn native_error_wire(size: usize, crlf: bool) -> String {
    let mut wire = "event: error\ndata: ".to_string();
    let error = serde_json::json!({"type":"error","error":{"type":"overloaded_error","message":"합성 \" \\ \n 😀"}});
    wire.push_str(&error.to_string());
    wire.push_str("\n\n");
    if crlf {
        wire = wire.replace('\n', "\r\n");
    }
    assert!(size >= wire.len());
    let at = wire.find("합성").unwrap();
    wire.insert_str(at, &"x".repeat(size - wire.len()));
    assert_eq!(wire.len(), size);
    wire
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn native_error_event_boundaries_preserve_wire_without_retaining_messages() {
    for started in [false, true] {
        for (size, crlf, truncated, terminal_cr) in [
            (1110, false, false, false),
            (1111, false, false, false),
            (65536, false, false, false),
            (65537, false, false, false),
            (65536, true, false, false),
            (65537, true, false, false),
            (1111, false, true, false),
            (65536, true, false, true),
            (65537, true, false, true),
        ] {
            let h = Harness::new().await;
            let mut error = native_error_wire(size + usize::from(terminal_cr), crlf);
            if terminal_cr {
                assert_eq!(error.pop(), Some('\n'));
            }
            if truncated {
                error.truncate(error.len() - 5);
            }
            let expected = format!("{}{}", if started { START } else { "" }, error);
            let (tx, stopped) = h.provider.stream().await;
            let job = tokio::spawn(async move {
                // UTF8/escape/JSON token의 모든 분할을 실제 upstream chunk 경계에서 실행한다.
                if started {
                    tx.send(Bytes::from_static(START.as_bytes())).await.unwrap();
                }
                for byte in error.bytes() {
                    if tx.send(Bytes::copy_from_slice(&[byte])).await.is_err() {
                        break;
                    }
                }
            });
            let response = h
                .request("/v1/messages", stream_body())
                .send()
                .await
                .unwrap();
            let status = response.status();
            let bytes = response.bytes().await;
            let accepted = size <= 65536 && !truncated;
            if accepted {
                assert_eq!(status, 200);
                let received = bytes.expect("정상 bounded native 오류 원본 전달");
                let first_difference = received
                    .iter()
                    .zip(expected.bytes())
                    .position(|(a, b)| *a != b);
                assert!(
                    received == expected.as_bytes(),
                    "native 오류 원본 불일치 size={size} crlf={crlf} terminal_cr={terminal_cr} started={started} received_len={} expected_len={} first_difference={first_difference:?}",
                    received.len(),
                    expected.len()
                );
            } else {
                assert!(
                    status == 502 || bytes.is_err(),
                    "상한 초과/절단 native 오류를 정상 종료했다 size={size} crlf={crlf} started={started}"
                );
            }
            job.await.unwrap();
            tokio::time::timeout(Duration::from_secs(2), stopped)
                .await
                .unwrap()
                .unwrap();
            idle(&h).await;
            let r = h.runtime.request_resources();
            assert_eq!((r.queue_bytes, r.queue_waiters, r.body_bytes), (0, 0, 0));
            assert!(r.parser_peak <= 256 * 1024);
            assert!(r.body_peak <= 16 * 1024);
            let row=sqlx::query("SELECT accounting_state,cost_usd,execution_stopped_at IS NOT NULL AS stopped,reason_code FROM gw_requests").fetch_one(h.db.store.pool()).await.unwrap();
            assert_eq!(row.get::<String, _>("accounting_state"), "unknown");
            assert!(row.try_get_raw("cost_usd").unwrap().is_null());
            assert!(row.get::<bool, _>("stopped"));
            if accepted {
                assert_eq!(row.get::<String, _>("reason_code"), "provider_error");
            }
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT unknown_count FROM gw_usage_totals")
                    .fetch_one(h.db.store.pool())
                    .await
                    .unwrap(),
                1
            );
            let blocked = h.request("/v1/messages", BODY).send().await.unwrap();
            assert_eq!(blocked.status(), 403);
            drop(blocked);
            idle(&h).await;
            assert_eq!(h.provider.calls.lock().await.len(), 1);
            h.close().await;
        }
    }
}

#[tokio::test]
#[ignore = "requires dedicated TEST_DATABASE_URL, fixed SDK and explicit TEST_FNM_DIR"]
async fn sdk_native_error_message_and_event_boundaries_are_not_projection_limits() {
    for started in [false, true] {
        let messages = [1024,1025].map(|length| {
            let event = serde_json::json!({"type":"error","error":{"type":"overloaded_error","message":"x".repeat(length)}});
            format!("event: error\ndata: {event}\n\n")
        });
        let mut cases = messages
            .into_iter()
            .map(|wire| (wire, true))
            .collect::<Vec<_>>();
        cases.push((native_error_wire(65536, true), true));
        cases.push((native_error_wire(65537, true), false));
        let mut cr_only = native_error_wire(65537, true);
        cr_only.pop();
        cases.push((cr_only, true));
        let mut truncated = native_error_wire(1111, false);
        truncated.truncate(truncated.len() - 5);
        cases.push((truncated, false));
        for (error, valid) in cases {
            let h = Harness::new().await;
            let length = if valid {
                let data = error.split("data: ").nth(1).unwrap().trim_end();
                let value: serde_json::Value = serde_json::from_str(data).unwrap();
                Some(value["error"]["message"].as_str().unwrap().len())
            } else {
                None
            };
            let wire = format!("{}{}", if started { START } else { "" }, error);
            h.provider
                .set_chunked(
                    StatusCode::OK,
                    HeaderMap::from_iter([(
                        "content-type".parse().unwrap(),
                        "text/event-stream".parse().unwrap(),
                    )]),
                    Bytes::from(wire),
                )
                .await;
            println!(
                "S3_NATIVE_ERROR_SDK started={started} valid={valid} wire_bytes={} message_bytes={length:?}",
                error.len()
            );
            sdk_with_error_length(
                &h,
                "/anthropic",
                if valid {
                    "messages_stream_error_bounded"
                } else {
                    "messages_stream_interrupted"
                },
                200,
                &h.token,
                length,
            )
            .await;
            idle(&h).await;
            let r = h.runtime.request_resources();
            assert_eq!((r.queue_bytes, r.queue_waiters, r.body_bytes), (0, 0, 0));
            assert!(r.parser_peak <= 256 * 1024);
            assert_eq!(h.provider.calls.lock().await.len(), 1);
            let row=sqlx::query("SELECT accounting_state,cost_usd,execution_stopped_at IS NOT NULL AS stopped FROM gw_requests").fetch_one(h.db.store.pool()).await.unwrap();
            assert_eq!(row.get::<String, _>("accounting_state"), "unknown");
            assert!(row.try_get_raw("cost_usd").unwrap().is_null());
            assert!(row.get::<bool, _>("stopped"));
            h.close().await;
        }
    }
}
