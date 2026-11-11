use super::*;
use std::time::Duration;

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s9_complete_final_survives_client_cancellation() {
    cancellation(true, false).await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s9_partial_and_unpriced_final_cancel_keep_unknown() {
    cancellation(false, false).await;
    cancellation(false, true).await;
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s9_complete_final_survives_shutdown_cancellation() {
    cancellation(true, true).await;
}
async fn cancellation(complete: bool, shutdown: bool) {
    for variant in [false, true] {
        for ending in if complete {
            &["\n", "\r\n", "\r"][..]
        } else {
            &["\n"][..]
        } {
            let (h, path, body, native, partial, cost) = s9_finality_fixture(variant).await;
            let wire = if complete {
                native.replace("\r\n", "\n").replace('\n', ending)
            } else if variant {
                native.replace("synthetic-text", "synthetic-unpriced")
            } else {
                partial
            };
            let (tx, stopped) = h.provider.stream().await;
            tx.send(Bytes::from(wire.clone())).await.unwrap();
            let mut r = h.request(path, body.clone()).send().await.unwrap();
            assert_eq!(r.status(), 200);
            assert_eq!(r.headers()["x-should-retry"], "false");
            assert!(!r.headers().contains_key("x-gateway-dispatch-state"));
            let id: uuid::Uuid = r.headers()["x-gateway-request-id"]
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            // EOF 없는 upstream의 완전 원본을 실제 client가 읽은 뒤에만 취소한다.
            let mut received = Vec::new();
            tokio::time::timeout(Duration::from_secs(3), async {
                while received.len() < wire.len() {
                    received.extend_from_slice(&r.chunk().await.unwrap().unwrap());
                }
            })
            .await
            .unwrap();
            assert_eq!(received, wire.as_bytes());
            if shutdown {
                h.runtime.begin_draining();
                h.runtime.cancel_requests();
                assert!(r.bytes().await.is_err(), "shutdown H1은 성공 EOF가 아니다");
            } else {
                drop(r);
            }
            tokio::time::timeout(Duration::from_secs(3), stopped)
                .await
                .unwrap()
                .unwrap();
            drop(tx);
            tokio::time::timeout(Duration::from_secs(6), h.runtime.wait_requests())
                .await
                .unwrap();
            let row:(String,String,Option<String>,bool,String)=sqlx::query_as("SELECT accounting_state,reason_code,cost_usd::text,execution_stopped_at IS NOT NULL,outcome FROM gw_requests WHERE id=$1").bind(id).fetch_one(h.db.store.pool()).await.unwrap();
            let budget:(String,i64,i64,i64,i64,i64)=sqlx::query_as("SELECT b.remaining_reservation_usd::text,b.remaining_reserved_tokens,t.unknown_count,(SELECT count(*) FROM gw_usage_events WHERE request_id=$1 AND kind='final' AND normalized_usage->>'source'='provider'),w.reserved_tokens,w.settled_tokens FROM gw_request_budgets b JOIN gw_usage_totals t ON t.scope_id=b.scope_id AND t.period_start=b.period_start JOIN gw_rate_windows w ON w.scope_id=b.scope_id AND w.lane=b.rate_lane AND w.window_start=b.rate_start WHERE b.request_id=$1").bind(id).fetch_one(h.db.store.pool()).await.unwrap();
            let calls = h.provider.calls.lock().await.len();
            let resources = h.runtime.request_resources();
            let available = h.runtime.available_inference();
            // assertion Red도 자신의 schema를 정리하고 나서 실패한다.
            let mut next_status = None;
            if !shutdown {
                h.provider
                    .set(
                        StatusCode::OK,
                        HeaderMap::from_iter([(
                            "content-type".parse().unwrap(),
                            "text/event-stream".parse().unwrap(),
                        )]),
                        Bytes::from(native),
                    )
                    .await;
                let next = h.request(path, body).send().await.unwrap();
                let status = next.status();
                let _ = next.bytes().await;
                h.runtime.wait_requests().await;
                next_status = Some(status.as_u16());
            }
            h.close().await;
            if let Some(status) = next_status {
                assert_eq!(status, if complete { 200 } else { 403 });
            }
            assert_eq!(calls, 1);
            assert_eq!(available, 16);
            assert_eq!(resources.body_bytes, 0);
            assert_eq!(resources.queue_bytes, 0);
            assert_eq!(resources.queue_waiters, 0);
            assert!(resources.parser_peak <= 256 * 1024);
            assert!(row.3);
            assert_eq!(row.4, "cancelled");
            assert_eq!(row.0, if complete { "settled" } else { "unknown" });
            assert_eq!(
                row.1,
                if shutdown {
                    "shutdown_cancelled"
                } else {
                    "client_cancelled"
                }
            );
            assert_eq!(row.2.as_deref(), complete.then_some(cost));
            if complete {
                assert_eq!(budget, ("0.000000000000".into(), 0, 0, 1, 0, 120));
            } else {
                // 완전한 관측의 존재와 가격 확정은 다르다. 미가격 final도 비용은 계속 unknown이다.
                assert_eq!(
                    budget,
                    if cost == "0.000140000000" {
                        ("0.000000000000".into(), 0, 1, i64::from(variant), 0, 0)
                    } else {
                        (
                            "0.004296000000".into(),
                            4196,
                            1,
                            i64::from(variant),
                            4196,
                            0,
                        )
                    }
                );
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s9_failure_after_final_discards_snapshot_before_supervised_finalization() {
    for split in [false, true] {
        for kind in ["syntax", "native", "oversized_native", "duplicate_terminal"] {
            let (h, path, body, native, partial, cost) = s9_finality_fixture(false).await;
            let (tx, stopped) = h.provider.stream().await;
            tx.send(Bytes::from(partial.clone())).await.unwrap();
            let mut r = h.request(path, body.clone()).send().await.unwrap();
            assert_eq!(r.status(), 200);
            let mut received = Vec::new();
            while received.len() < partial.len() {
                received.extend_from_slice(&r.chunk().await.unwrap().unwrap());
            }
            assert_eq!(received, partial.as_bytes());
            let tail = match kind {
                "syntax" => "data: {bad}\n\n".to_owned(),
                "native" => s9_native_error_tail(32),
                "oversized_native" => s9_native_error_tail(65536),
                _ => native.clone(),
            };
            let final_part = native.strip_prefix(&partial).unwrap();
            if split {
                tx.send(Bytes::copy_from_slice(final_part.as_bytes()))
                    .await
                    .unwrap();
                let mut received = Vec::new();
                while received.len() < final_part.len() {
                    received.extend_from_slice(&r.chunk().await.unwrap().unwrap());
                }
                assert_eq!(received, final_part.as_bytes());
                tx.send(Bytes::from(tail + &format!("event: {}\n", "x".repeat(129))))
                    .await
                    .unwrap();
            } else {
                tx.send(Bytes::from(
                    final_part.to_owned() + &tail + &format!("event: {}\n", "x".repeat(129)),
                ))
                .await
                .unwrap();
            }
            assert!(
                r.bytes().await.is_err(),
                "H1 오류를 성공 EOF로 바꾸지 않는다"
            );
            tokio::time::timeout(Duration::from_secs(3), stopped)
                .await
                .unwrap()
                .unwrap();
            drop(tx);
            h.runtime.wait_requests().await;
            let row:(String,bool,i64,i64,String,i64)=sqlx::query_as("SELECT r.accounting_state,r.cost_usd IS NULL,t.unknown_count,(SELECT count(*) FROM gw_usage_events WHERE request_id=r.id AND kind='final'),b.remaining_reservation_usd::text,b.remaining_reserved_tokens FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id JOIN gw_usage_totals t ON t.scope_id=b.scope_id AND t.period_start=b.period_start").fetch_one(h.db.store.pool()).await.unwrap();
            let next = h.request(path, body).send().await.unwrap();
            let next_status = next.status();
            let _ = next.bytes().await;
            h.runtime.wait_requests().await;
            let calls = h.provider.calls.lock().await.len();
            let available = h.runtime.available_inference();
            h.close().await;
            assert_eq!(next_status, 403);
            assert_eq!(calls, 1);
            assert_eq!(available, 16);
            assert_eq!(
                row,
                (
                    "unknown".into(),
                    true,
                    1,
                    0,
                    if cost == "0.000140000000" {
                        "0.000000000000"
                    } else {
                        "0.004296000000"
                    }
                    .into(),
                    if cost == "0.000140000000" { 0 } else { 4196 }
                )
            );
        }
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s9_incomplete_native_error_before_cancel_invalidates_success_without_claiming_failure() {
    for shutdown in [false, true] {
        let (h, path, body, native, _, cost) = s9_finality_fixture(false).await;
        let (tx, stopped) = h.provider.stream().await;
        tx.send(Bytes::from(native.clone())).await.unwrap();
        let mut r = h.request(path, body.clone()).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let mut received = Vec::new();
        while received.len() < native.len() {
            received.extend_from_slice(&r.chunk().await.unwrap().unwrap());
        }
        assert_eq!(received, native.as_bytes());
        // 이름만 관측하고 JSON/blank line이 아직 없는 오류. 검증된 native failure는 아니다.
        let tail = "event: error\ndata: {";
        tx.send(Bytes::from_static(tail.as_bytes())).await.unwrap();
        let mut received = Vec::new();
        while received.len() < tail.len() {
            received.extend_from_slice(&r.chunk().await.unwrap().unwrap());
        }
        assert_eq!(received, tail.as_bytes());
        if shutdown {
            h.runtime.cancel_requests();
        }
        drop(r);
        stopped.await.unwrap();
        drop(tx);
        h.runtime.wait_requests().await;
        let row:(String,String,String,bool,i64,String,i64)=sqlx::query_as("SELECT r.accounting_state,r.outcome,r.reason_code,r.cost_usd IS NULL,(SELECT count(*) FROM gw_usage_events WHERE request_id=r.id AND kind='final'),b.remaining_reservation_usd::text,b.remaining_reserved_tokens FROM gw_requests r JOIN gw_request_budgets b ON b.request_id=r.id").fetch_one(h.db.store.pool()).await.unwrap();
        let next = h.request(path, body).send().await.unwrap();
        let status = next.status();
        let _ = next.bytes().await;
        h.runtime.wait_requests().await;
        let calls = h.provider.calls.lock().await.len();
        let resources = h.runtime.request_resources();
        h.close().await;
        assert_eq!(status, 403);
        assert_eq!(calls, 1);
        assert_eq!(resources.body_bytes, 0);
        assert_eq!(resources.queue_bytes, 0);
        assert_eq!(
            row,
            (
                "unknown".into(),
                "cancelled".into(),
                if shutdown {
                    "shutdown_cancelled"
                } else {
                    "client_cancelled"
                }
                .into(),
                true,
                0,
                if cost == "0.000140000000" {
                    "0.000000000000"
                } else {
                    "0.004296000000"
                }
                .into(),
                if cost == "0.000140000000" { 0 } else { 4196 }
            )
        );
    }
}
