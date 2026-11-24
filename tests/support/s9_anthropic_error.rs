use super::*;
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn s9_native_error_before_limit_never_resurrects_old_success() {
    let mut violations = 0;
    for length in [32, 65536, 16 * 1024 * 1024] {
        for ending in ["\n", "\r\n", "\r"] {
            for split in [false, true] {
                let (h, path, body, native, partial, _) = s9_finality_fixture(false).await;
                let (tx, stopped) = h.provider.stream().await;
                tx.send(Bytes::from(partial.clone())).await.unwrap();
                let mut r = h.request(path, body).send().await.unwrap();
                assert_eq!(r.status(), 200);
                let mut received = Vec::new();
                while received.len() < partial.len() {
                    received.extend_from_slice(&r.chunk().await.unwrap().unwrap());
                }
                assert_eq!(received, partial.as_bytes());
                let error = format!("event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"api_error\",\"message\":\"{}\"}}}}\n\n", "x".repeat(length)).replace('\n', ending);
                let limit = format!("event: {}\n", "x".repeat(129));
                let final_part = native.strip_prefix(&partial).unwrap().replace('\n', ending);
                if split {
                    tx.send(Bytes::from(final_part.clone())).await.unwrap();
                    let mut final_received = Vec::new();
                    while final_received.len() < final_part.len() {
                        final_received.extend_from_slice(&r.chunk().await.unwrap().unwrap());
                    }
                    assert_eq!(final_received, final_part.as_bytes());
                    tx.send(Bytes::from(error + &limit)).await.unwrap();
                } else {
                    tx.send(Bytes::from(final_part + &error + &limit))
                        .await
                        .unwrap();
                }
                let _ = r.bytes().await;
                stopped.await.unwrap();
                drop(tx);
                h.runtime.wait_requests().await;
                let row:(String,bool,i64,i64)=sqlx::query_as("SELECT r.accounting_state,r.cost_usd IS NULL,t.unknown_count,(SELECT count(*) FROM gw_usage_events WHERE request_id=r.id AND kind='final') FROM gw_requests r JOIN test_user_request_budgets b ON b.request_id=r.id AND b.scope_id IN (SELECT id FROM gw_policies WHERE key_id IS NULL) JOIN test_user_usage_totals t ON t.scope_id=b.scope_id AND t.period_start=b.period_start").fetch_one(h.db.store.pool()).await.unwrap();
                let settled = finance::last_settlement(h.db.store.pool()).await;
                // 끊긴 요청은 막지 않는다. 다음 비스트림 요청은 그대로 송신한다.
                let next = h.request(path, BODY).send().await.unwrap();
                let next_status = next.status();
                let _ = next.bytes().await;
                h.runtime.wait_requests().await;
                let calls = h.provider.calls.lock().await.len();
                let available = h.runtime.available_inference();
                h.close().await;
                // 오류 뒤 limit는 시작 event 뒤의 끊김이다. 옛 성공 final(출력 20)을 되살리지 않고
                // 끊긴 요청 규칙으로 받은 출력(없음)만 정산한다.
                if row != ("settled".into(), false, 0, 1)
                    || settled != finance::cutoff_received("0.000100000000", 0)
                    || next_status != 200
                    || calls != 2
                {
                    violations += 1;
                }
                assert_eq!(available, 16);
            }
        }
    }
    assert_eq!(
        violations, 0,
        "native 오류 뒤 limit가 옛 성공 final을 복원했다"
    );
}
