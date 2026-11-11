use super::*;

fn json_headers() -> HeaderMap {
    HeaderMap::from_iter([(
        "content-type".parse().unwrap(),
        "application/json".parse().unwrap(),
    )])
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn count_explicit_error_is_not_evidence_for_free_or_paid_external_or_internal() {
    for free in [false, true] {
        for internal in [false, true] {
            for bad in [false, true] {
                let cfg = if free {
                    configuration_from_yaml(&finance_yaml().replace(
                        "api: messages",
                        "api: messages\n        internal_count: true",
                    ))
                } else {
                    s4::chargeable_configuration(true)
                };
                let h = Harness::with_config(cfg).await;
                sqlx::query("UPDATE gw_policies SET budget_mode='reserved',budget_limit_usd=gw_amount('1.000000000000'),tpm_limit=10000 WHERE principal_id=$1").bind(h.principal).execute(h.db.store.pool()).await.unwrap();
                h.provider.set_count(StatusCode::OK, json_headers(), Bytes::from_static(if bad {
                    br#"{"type":"error","error":{"type":"api_error","message":"synthetic failure"},"input_tokens":1}"#
                } else { br#"{"input_tokens":321}"# })).await;
                let response = h
                    .request(
                        if internal {
                            "/v1/messages"
                        } else {
                            "/v1/messages/count_tokens"
                        },
                        if internal {
                            BODY.to_owned()
                        } else {
                            BODY.replace("\"max_tokens\":100,", "")
                        },
                    )
                    .send()
                    .await
                    .unwrap();
                let status = response.status();
                assert_eq!(
                    status,
                    if bad {
                        StatusCode::BAD_GATEWAY
                    } else {
                        StatusCode::OK
                    },
                    "count 정상 전제/오류 거부 free={free} internal={internal} bad={bad}"
                );
                let _ = response.bytes().await.unwrap();
                h.runtime.wait_requests().await;
                let rows = sqlx::query("SELECT purpose,api,accounting_state,cost_usd::text AS cost,outcome,(SELECT normalized_usage->>'counted_input_tokens' FROM gw_usage_events e WHERE e.request_id=r.id AND kind='final') AS counted,(SELECT normalized_usage->>'source' FROM gw_usage_events e WHERE e.request_id=r.id AND kind='final') AS source FROM gw_requests r ORDER BY purpose").fetch_all(h.db.store.pool()).await.unwrap();
                let counted = rows[0].get::<Option<String>, _>("counted");
                let accounting = rows[0].get::<String, _>("accounting_state");
                let cost = rows[0].get::<Option<String>, _>("cost");
                let source = rows[0].get::<Option<String>, _>("source");
                let child_budget: (String, String, bool) = sqlx::query_as("SELECT reservation_usd::text,remaining_reservation_usd::text,remaining_reserved_tokens=reserved_tokens FROM gw_request_budgets b JOIN gw_requests r ON r.id=b.request_id WHERE r.api='count_tokens'").fetch_one(h.db.store.pool()).await.unwrap();
                let totals: (String, i64) =
                    sqlx::query_as("SELECT cost_usd::text,unknown_count FROM gw_usage_totals")
                        .fetch_one(h.db.store.pool())
                        .await
                        .unwrap();
                let first_calls = h.provider.calls.lock().await.len();
                // 무료 count 실패는 금전 unknown을 만들지 않는다. 후속이 실행되도록 정상 count로 교체한다.
                h.provider
                    .set_count(
                        StatusCode::OK,
                        json_headers(),
                        Bytes::from_static(br#"{"input_tokens":321}"#),
                    )
                    .await;
                let next = h.request("/v1/messages", BODY).send().await.unwrap();
                let next_status = next.status();
                let _ = next.bytes().await.unwrap();
                h.runtime.wait_requests().await;
                let calls = h.provider.calls.lock().await.clone();
                h.close().await;
                assert_eq!(
                    status,
                    if bad {
                        StatusCode::BAD_GATEWAY
                    } else {
                        StatusCode::OK
                    }
                );
                assert_eq!(first_calls, if internal && !bad { 2 } else { 1 });
                assert_eq!(rows.len(), if internal { 2 } else { 1 });
                assert_eq!(counted.as_deref(), if bad { None } else { Some("321") });
                assert_eq!(accounting, if bad && !free { "unknown" } else { "settled" });
                assert_eq!(
                    cost.as_deref(),
                    if bad && !free {
                        None
                    } else if free {
                        Some("0.000000000000")
                    } else {
                        Some("0.000321000000")
                    }
                );
                if free {
                    assert_eq!(source.as_deref(), Some("profile_no_charge"));
                }
                if internal && bad {
                    assert_eq!(rows[1].get::<String, _>("cost"), "0.000000000000");
                    assert_eq!(rows[1].get::<String, _>("source"), "gateway_no_dispatch");
                    assert_eq!(rows[1].get::<String, _>("outcome"), "rejected");
                }
                if bad && !free {
                    assert_ne!(child_budget.0, "0.000000000000");
                    assert_eq!(child_budget.0, child_budget.1);
                    assert!(child_budget.2);
                    assert_eq!(totals, ("0.000000000000".into(), 1));
                    assert_eq!(next_status, StatusCode::FORBIDDEN);
                    assert_eq!(calls.len(), first_calls);
                } else {
                    assert_eq!(child_budget.1, "0.000000000000");
                    assert_eq!(totals.1, 0);
                    assert_eq!(next_status, StatusCode::OK);
                }
                if bad {
                    assert!(
                        calls[..first_calls]
                            .iter()
                            .all(|c| c.path == "/v1/messages/count_tokens")
                    );
                }
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn messages_nonstream_error_metadata_in_both_orders_never_settles() {
    let fields = r#""type":"message","role":"assistant","model":"synthetic-text","content":[],"stop_reason":"end_turn","usage":{"input_tokens":100,"output_tokens":20}"#;
    for error in [
        None,
        Some("null"),
        Some(r#"{"type":"api_error","message":"synthetic failure"}"#),
    ] {
        for first in [false, true] {
            let h = Harness::new().await;
            let wire = match error {
                None => format!("{{{fields}}}"),
                Some(error) if first => format!("{{\"error\":{error},{fields}}}"),
                Some(error) => format!("{{{fields},\"error\":{error}}}"),
            };
            h.provider
                .set(StatusCode::OK, json_headers(), Bytes::from(wire))
                .await;
            let response = h.request("/v1/messages", BODY).send().await.unwrap();
            let status = response.status();
            let _ = response.bytes().await.unwrap();
            h.runtime.wait_requests().await;
            let state: (String, Option<String>) =
                sqlx::query_as("SELECT accounting_state,cost_usd::text FROM gw_requests")
                    .fetch_one(h.db.store.pool())
                    .await
                    .unwrap();
            let calls = h.provider.calls.lock().await.len();
            h.close().await;
            let bad = error.is_some_and(|e| e != "null");
            assert_eq!(
                status,
                if bad {
                    StatusCode::BAD_GATEWAY
                } else {
                    StatusCode::OK
                }
            );
            assert_eq!(
                state,
                if bad {
                    ("unknown".into(), None)
                } else {
                    ("settled".into(), Some("0.000140000000".into()))
                }
            );
            assert_eq!(calls, 1);
        }
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn sse_unexpected_terminal_usage_native_and_alias_same_or_split_part_stays_unknown() {
    use super::s3_contract::{END, START};
    for path in ["/anthropic/v1/messages", "/v1/messages"] {
        for split in [false, true] {
            for bad in [false, true] {
                let h = Harness::with_config(configuration_from_yaml(&finance_yaml().replace(
                    "api: messages",
                    "api: messages\n        internal_count: true",
                )))
                .await;
                h.provider
                    .set_count(
                        StatusCode::OK,
                        json_headers(),
                        Bytes::from_static(br#"{"input_tokens":321}"#),
                    )
                    .await;
                sqlx::query("UPDATE gw_policies SET budget_mode='reserved',budget_limit_usd=gw_amount('1.000000000000'),tpm_limit=10000 WHERE principal_id=$1").bind(h.principal).execute(h.db.store.pool()).await.unwrap();
                let tail = if bad {
                    END.replace(
                        r#"{"type":"message_stop"}"#,
                        r#"{"type":"message_stop","usage":{"output_tokens":99999}}"#,
                    )
                } else {
                    END.to_owned()
                };
                let (tx, stopped) = h.provider.stream_messages().await;
                let request = h.request(path, BODY.replacen('{', "{\"stream\":true,", 1));
                let response = if split {
                    let (r, sent) = tokio::join!(
                        request.send(),
                        tx.send(Bytes::from_static(START.as_bytes()))
                    );
                    sent.unwrap();
                    let r = r.unwrap();
                    assert_eq!(r.status(), 200);
                    tx.send(Bytes::from(tail)).await.unwrap();
                    r
                } else {
                    let (r, sent) = tokio::join!(
                        request.send(),
                        tx.send(Bytes::from(format!("{START}{tail}")))
                    );
                    sent.unwrap();
                    r.unwrap()
                };
                let status = response.status();
                drop(tx);
                let bytes = response.bytes().await;
                stopped.await.unwrap();
                h.runtime.wait_requests().await;
                let state: (String, Option<String>, i64) = sqlx::query_as("SELECT accounting_state,cost_usd::text,(SELECT count(*) FROM gw_usage_events e WHERE kind='final' AND e.request_id=r.id) FROM gw_requests r WHERE api='messages'").fetch_one(h.db.store.pool()).await.unwrap();
                let held: (String, String, i64, i64) = sqlx::query_as("SELECT reservation_usd::text,remaining_reservation_usd::text,reserved_tokens,remaining_reserved_tokens FROM gw_request_budgets b JOIN gw_requests r ON r.id=b.request_id WHERE r.api='messages'").fetch_one(h.db.store.pool()).await.unwrap();
                let next = h.request(path, BODY).send().await.unwrap();
                let next_status = next.status();
                let _ = next.bytes().await.unwrap();
                h.runtime.wait_requests().await;
                let calls = h
                    .provider
                    .calls
                    .lock()
                    .await
                    .iter()
                    .filter(|c| c.path == "/v1/messages")
                    .count();
                h.close().await;
                if bad {
                    assert!(status == 502 || (status == 200 && bytes.is_err()));
                    assert_eq!(state, ("unknown".into(), None, 0));
                    assert_ne!(held.0, "0.000000000000");
                    assert_eq!(held.0, held.1);
                    assert!(held.2 > 0 && held.2 == held.3);
                    assert_eq!(next_status, StatusCode::FORBIDDEN);
                    assert_eq!(calls, 1);
                } else {
                    assert_eq!(status, 200);
                    assert_eq!(bytes.unwrap(), format!("{START}{END}").as_bytes());
                    assert_eq!(state, ("settled".into(), Some("0.000140000000".into()), 1));
                    assert_eq!(held.1, "0.000000000000");
                    assert_eq!(held.3, 0);
                    assert_eq!(next_status, StatusCode::OK);
                    assert_eq!(calls, 2);
                }
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL and fixed SDK"]
async fn sdk_explicit_nonstream_error_and_conflicting_sse_have_one_attempt_without_final() {
    for free in [false, true] {
        for internal in [false, true] {
            let cfg = if free {
                configuration_from_yaml(&finance_yaml().replace(
                    "api: messages",
                    "api: messages\n        internal_count: true",
                ))
            } else {
                s4::chargeable_configuration(true)
            };
            let h = Harness::with_config(cfg).await;
            h.provider.set_count(StatusCode::OK, json_headers(), Bytes::from_static(br#"{"type":"error","error":{"type":"api_error","message":"synthetic failure"},"input_tokens":1}"#)).await;
            sdk(
                &h,
                "/anthropic",
                if internal { "messages" } else { "count_tokens" },
                502,
                &h.token,
            )
            .await;
            h.runtime.wait_requests().await;
            let states: Vec<(String, String, Option<String>)> = sqlx::query_as(
                "SELECT api,accounting_state,cost_usd::text FROM gw_requests ORDER BY api",
            )
            .fetch_all(h.db.store.pool())
            .await
            .unwrap();
            let calls = h.provider.calls.lock().await.clone();
            h.close().await;
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].path, "/v1/messages/count_tokens");
            assert_eq!(
                states[0],
                (
                    "count_tokens".into(),
                    if free { "settled" } else { "unknown" }.into(),
                    if free {
                        Some("0.000000000000".into())
                    } else {
                        None
                    }
                )
            );
            if internal {
                assert_eq!(
                    states[1],
                    (
                        "messages".into(),
                        "settled".into(),
                        Some("0.000000000000".into())
                    )
                );
            }
        }
    }
    for prefix in ["", "/anthropic"] {
        let h = Harness::new().await;
        let tail = super::s3_contract::END.replace(
            r#"{"type":"message_stop"}"#,
            r#"{"type":"message_stop","usage":{"output_tokens":99999}}"#,
        );
        // 첫 작업 part에서 H1을 열고 나서 반례 terminal이 도착하도록 유효한 확장 event를 넣는다.
        let wire = format!(
            "{}event: future_event\ndata: {{\"type\":\"future_event\",\"text\":\"{}\"}}\n\n{tail}",
            super::s3_contract::START,
            "x".repeat(32 * 1024)
        );
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
        sdk(&h, prefix, "messages_stream_interrupted", 200, &h.token).await;
        h.runtime.wait_requests().await;
        let state: (String, Option<String>, i64) = sqlx::query_as("SELECT accounting_state,cost_usd::text,(SELECT count(*) FROM gw_usage_events WHERE kind='final') FROM gw_requests").fetch_one(h.db.store.pool()).await.unwrap();
        let calls = h.provider.calls.lock().await.len();
        h.close().await;
        assert_eq!(state, ("unknown".into(), None, 0));
        assert_eq!(calls, 1);
    }
}
