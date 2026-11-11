use super::*;

// 원문 Provider/SQL 예외가 아닌 실제 생성기의 고정 envelope와 dispatch 사실을 대조한다.
#[tokio::test]
async fn each_gateway_error_has_native_envelope_and_dispatch_headers() {
    let cases = [
        (400, "invalid_request", false),
        (400, "unsupported_option", false),
        (400, "unsupported_model", false),
        (400, "unsupported_strict_policy", false),
        (401, "invalid_credentials", false),
        (403, "auth_source_forbidden", false),
        (403, "access_denied", false),
        (403, "owner_inactive", false),
        (408, "request_body_timeout", false),
        (413, "body_too_large", false),
        (415, "unsupported_media_type", false),
        (429, "concurrency_limited", true),
        (429, "rate_limited", true),
        (429, "budget_exhausted", false),
        (403, "unsettled_usage_block", false),
        (403, "inference_paused", false),
        (409, "configuration_conflict", false),
        (409, "policy_changed", false),
        (503, "auth_dependency_unavailable", true),
        (503, "db_unavailable", true),
        (503, "draining", true),
        (503, "dispatch_uncertain", false),
        (502, "upstream_transport_error", false),
        (504, "upstream_read_timeout", false),
        (502, "upstream_auth_error", false),
        (502, "upstream_redirect", false),
        (502, "upstream_encoding_error", false),
        (502, "upstream_protocol_error", false),
        (502, "stream_parse_error", false),
        (502, "stream_limit", false),
        (502, "upstream_response_too_large", false),
        (503, "finalization_unavailable", false),
        (500, "invariant_violation", false),
        (500, "internal_error", false),
    ];
    for (status, code, retry_after) in cases {
        for not_dispatched in [true, false] {
            for openai in [false, true] {
                let id = Uuid::new_v4();
                let mut r = error(
                    id,
                    StatusCode::from_u16(status).unwrap(),
                    code,
                    not_dispatched,
                );
                if openai {
                    r = openai_error(r);
                }
                assert_eq!(r.status().as_u16(), status, "{code}");
                assert_eq!(r.headers()["x-gateway-error-code"], code);
                assert_eq!(r.headers()["x-gateway-request-id"], id.to_string());
                assert_eq!(r.headers()["cache-control"], "no-store");
                assert_eq!(
                    r.headers()
                        .get("x-gateway-dispatch-state")
                        .map(|v| v.as_bytes()),
                    not_dispatched.then_some(b"not_dispatched".as_slice())
                );
                assert_eq!(
                    r.headers().get("x-should-retry").map(|v| v.as_bytes()),
                    (!not_dispatched).then_some(b"false".as_slice())
                );
                assert_eq!(
                    r.headers().get("retry-after").map(|v| v.as_bytes()),
                    retry_after.then_some(b"1".as_slice())
                );
                for forbidden in [
                    "location",
                    "set-cookie",
                    "www-authenticate",
                    "content-encoding",
                    "etag",
                ] {
                    assert!(!r.headers().contains_key(forbidden));
                }
                let body = axum::body::to_bytes(r.into_body(), 2048).await.unwrap();
                let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let kind = match (openai, status) {
                    (_, 400 | 408 | 409 | 415) | (true, 413) => "invalid_request_error",
                    (_, 401) => "authentication_error",
                    (_, 403) => "permission_error",
                    (_, 429) => "rate_limit_error",
                    (false, 413) => "request_too_large",
                    (false, 503) => "overloaded_error",
                    (false, _) => "api_error",
                    (true, _) => "server_error",
                };
                assert_eq!(value["error"]["type"], kind);
                assert_eq!(
                    value["error"]["message"],
                    "Gateway request could not be completed"
                );
                if openai {
                    assert_eq!(
                        value,
                        serde_json::json!({"error":{"message":"Gateway request could not be completed","type":kind,"param":null,"code":format!("gateway_{code}")}})
                    );
                } else {
                    assert_eq!(
                        value,
                        serde_json::json!({"type":"error","error":{"type":kind,"message":"Gateway request could not be completed"}})
                    );
                }
            }
        }
    }
}

#[test]
fn only_certain_no_authorization_may_emit_not_dispatched() {
    assert!(DispatchEvidence::default().known_not_dispatched());
    assert!(!DispatchEvidence::MayBeAuthorized.known_not_dispatched());
    assert!(!DispatchEvidence::Authorized.known_not_dispatched());
}
