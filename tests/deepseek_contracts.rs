use m_gated::{
    config::{ModelProfile, ProviderId},
    ledger::finance::{FinalUsage, PricingSnapshot},
};
fn profile() -> ModelProfile {
    serde_json::from_value(serde_json::json!({"model":"synthetic-deepseek","api":"deepseek_chat","billing_time_rule":"fixed_admission_v1","profile_version":"deepseek-v1","pricing_version":"deepseek-price-v1","fixture_version":"deepseek-native-v1","actual_models":["synthetic-deepseek"],"max_input_tokens":4096,"max_output_tokens":1024,"max_body_bytes":8388608,"max_response_bytes":16777216,"rates":{"input":"1.000000000000","output":"2.000000000000","cache_read":"0.500000000000"}})).unwrap()
}
#[test]
fn deepseek_price_does_not_inherit_anthropic_cache_sum() {
    let price = PricingSnapshot::from_profile(ProviderId::Deepseek, &profile());
    let usage = FinalUsage {
        actual_model: "synthetic-deepseek".into(),
        input_tokens: 100,
        output_tokens: 20,
        cache_read_tokens: 40,
        cache_write_tokens: 0,
        reasoning_tokens: Some(10),
    };
    assert_eq!(
        price.price(&usage, false).unwrap().to_string(),
        "0.000120000000"
    );
    assert_eq!(
        price.included_token_rules,
        "deepseek_prompt_includes_hit_completion_includes_reasoning_v1"
    );
}
#[tokio::test]
async fn deepseek_native_request_is_not_responses_or_openai_chat() {
    let body=br#"{"model":"synthetic-deepseek","messages":[{"role":"user","content":"synthetic"}],"max_tokens":100,"stream":true,"thinking":{"type":"disabled"}}"#;
    assert!(
        m_gated::providers::deepseek::validate(body, &profile())
            .await
            .is_ok(),
        "DeepSeek 전용 native body 수용 경로 필요"
    );
}
#[tokio::test]
async fn deepseek_thinking_does_not_accept_forced_tool_choices() {
    let mut p = profile();
    p.features = vec![
        m_gated::config::NativeFeature::Thinking,
        m_gated::config::NativeFeature::ClientTools,
    ];
    for thinking in [None, Some("enabled")] {
        for choice in [
            serde_json::json!("required"),
            serde_json::json!({"type":"function","function":{"name":"f"}}),
        ] {
            let mut body = serde_json::json!({"model":"synthetic-deepseek","messages":[{"role":"user","content":"synthetic"}],"max_tokens":100,"tool_choice":choice});
            if let Some(t) = thinking {
                body["thinking"] = serde_json::json!({"type":t});
            }
            assert!(
                m_gated::providers::deepseek::validate(body.to_string().as_bytes(), &p)
                    .await
                    .is_err(),
                "thinking mode와 강제 tool choice 조합 거부"
            );
        }
    }
}
#[tokio::test]
async fn deepseek_native_feature_and_rejection_matrix() {
    use m_gated::{config::NativeFeature, providers::deepseek};
    let mut p = profile();
    p.features = vec![
        NativeFeature::Thinking,
        NativeFeature::ClientTools,
        NativeFeature::JsonSchema,
        NativeFeature::Cache,
    ];
    let base = serde_json::json!({"model":"synthetic-deepseek","messages":[{"role":"user","content":"synthetic"}],"max_tokens":100});
    for extra in [
        serde_json::json!({"thinking":{"type":"enabled"}}),
        serde_json::json!({"thinking":{"type":"disabled"},"tool_choice":"required"}),
        serde_json::json!({"response_format":{"type":"json_object"}}),
        serde_json::json!({"tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object","properties":{"model":{"type":"string"}}}}}],"tool_choice":"auto"}),
        serde_json::json!({"messages":[{"role":"assistant","content":null,"reasoning_content":"synthetic","tool_calls":[{"id":"c1","type":"function","function":{"name":"f","arguments":"{not parsed JSON}"}}]},{"role":"tool","content":"synthetic result","tool_call_id":"c1"}]}),
    ] {
        let mut b = base.clone();
        b.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(
            deepseek::validate(b.to_string().as_bytes(), &p)
                .await
                .is_ok()
        );
    }
    for extra in [
        serde_json::json!({"max_completion_tokens":100}),
        serde_json::json!({"store":false}),
        serde_json::json!({"reasoning_effort":"high"}),
        serde_json::json!({"n":1}),
        serde_json::json!({"parallel_tool_calls":true}),
        serde_json::json!({"response_format":{"type":"json_schema","json_schema":{"name":"x","schema":{}}}}),
        serde_json::json!({"thinking":{"type":"enabled","budget_tokens":5}}),
        serde_json::json!({"messages":[{"role":"user","content":[{"type":"text","text":"x"}]}]}),
        serde_json::json!({"messages":[{"role":"assistant","content":"x","prefix":true}]}),
        serde_json::json!({"tools":[{"type":"web_search"}]}),
        serde_json::json!({"stream_options":{"include_usage":true}}),
        serde_json::json!({"messages":[{"role":"developer","content":"x"}]}),
        serde_json::json!({"messages":[{"role":"user","content":null}]}),
        serde_json::json!({"messages":[{"role":"user","content":"x","reasoning_content":"x"}]}),
    ] {
        let mut b = base.clone();
        b.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(
            deepseek::validate(b.to_string().as_bytes(), &p)
                .await
                .is_err()
        );
    }
    for tail in [
        r#", "model":"synthetic-deepseek"}"#,
        r#", "tools":[{"type":"function","function":{"name":"f","parameters":{"x":1,"x":2}}}]}"#,
    ] {
        let b = base.to_string();
        assert!(
            deepseek::validate(format!("{}{tail}", &b[..b.len() - 1]).as_bytes(), &p)
                .await
                .is_err()
        );
    }
}
#[test]
fn deepseek_time_unresolved_has_no_fixed_price_or_reservation_proof() {
    use m_gated::{config::BillingTimeRule, ledger::finance::RequestSpec};
    let mut p = profile();
    p.billing_time_rule = Some(BillingTimeRule::DeepseekTimeUnresolvedV1);
    let spec = RequestSpec::from_profile(ProviderId::Deepseek, &p, Some(100));
    assert!(spec.cost_bound.is_none());
    let u = FinalUsage {
        actual_model: p.model.clone(),
        input_tokens: 100,
        output_tokens: 20,
        cache_read_tokens: 40,
        cache_write_tokens: 0,
        reasoning_tokens: Some(10),
    };
    assert!(spec.pricing.price(&u, false).is_err());
    assert!(spec.pricing.price(&u, true).is_err());
    let mut wrong = spec.pricing.clone();
    wrong.provider = "openai".into();
    assert!(wrong.price(&u, false).is_err());
    let mut fixed = PricingSnapshot::from_profile(ProviderId::Deepseek, &profile());
    fixed.included_token_rules =
        "openai_chat_prompt_includes_cache_completion_includes_reasoning_v1".into();
    assert!(fixed.price(&u, false).is_err());
}

#[tokio::test]
async fn deepseek_review_r1_effective_thinking_feature_and_field_order() {
    use m_gated::{config::NativeFeature, providers::deepseek};
    let mut mismatches = Vec::new();
    for proof in [false, true] {
        let mut p = profile();
        p.features = vec![NativeFeature::ClientTools];
        if proof {
            p.features.push(NativeFeature::Thinking);
        }
        for mode in [None, Some("enabled"), Some("disabled")] {
            for reversed in [false, true] {
                let thinking = mode
                    .map(|m| format!(r#", "thinking":{{"type":"{m}"}}"#))
                    .unwrap_or_default();
                let body = if reversed {
                    format!(
                        r#"{{"messages":[{{"role":"user","content":"synthetic"}}],"max_tokens":100{thinking},"model":"synthetic-deepseek"}}"#
                    )
                } else {
                    format!(
                        r#"{{"model":"synthetic-deepseek"{thinking},"max_tokens":100,"messages":[{{"role":"user","content":"synthetic"}}]}}"#
                    )
                };
                if deepseek::validate(body.as_bytes(), &p).await.is_ok()
                    != (proof || mode == Some("disabled"))
                {
                    mismatches.push((proof, mode, "omitted", reversed));
                }
            }
            for choice in [
                r#""auto""#,
                r#""none""#,
                r#""required""#,
                r#"{"type":"function","function":{"name":"f"}}"#,
            ] {
                let forced = choice != r#""auto""# && choice != r#""none""#;
                let allowed =
                    (mode == Some("disabled") || proof) && (!forced || mode == Some("disabled"));
                for reversed in [false, true] {
                    let fields = format!(
                        r#""tool_choice":{choice}{}"#,
                        mode.map(|m| format!(r#", "thinking":{{"type":"{m}"}}"#))
                            .unwrap_or_default()
                    );
                    let fields = if reversed {
                        mode.map(|m| {
                            format!(r#""thinking":{{"type":"{m}"}},"tool_choice":{choice}"#)
                        })
                        .unwrap_or(fields)
                    } else {
                        fields
                    };
                    let body = format!(
                        r#"{{{fields},"model":"synthetic-deepseek","max_tokens":100,"messages":[{{"role":"user","content":"synthetic"}}]}}"#
                    );
                    if deepseek::validate(body.as_bytes(), &p).await.is_ok() != allowed {
                        mismatches.push((proof, mode, choice, reversed));
                    }
                }
            }
        }
        let history = br#"{"model":"synthetic-deepseek","max_tokens":100,"thinking":{"type":"disabled"},"messages":[{"role":"assistant","content":"synthetic","reasoning_content":"synthetic thought"}]}"#;
        assert_eq!(
            deepseek::validate(history, &p).await.is_ok(),
            proof,
            "history Thinking proof는 현재 disabled와 독립"
        );
    }
    assert!(
        mismatches.is_empty(),
        "실효 thinking/feature/순서 불일치: {mismatches:?}"
    );
}

#[tokio::test]
async fn deepseek_review_r2_top_p_native_open_lower_boundary() {
    use m_gated::{config::NativeFeature, providers::deepseek};
    let mut p = profile();
    p.features = vec![NativeFeature::Thinking];
    let mut accepted_invalid = Vec::new();
    for value in [
        "-0", "0", "-0.0", "-0.1", "1.00001", r#""0.5""#, "true", "null", "{}", "[]",
    ] {
        let body = format!(
            r#"{{"model":"synthetic-deepseek","max_tokens":100,"messages":[{{"role":"user","content":"synthetic"}}],"thinking":{{"type":"disabled"}},"top_p":{value}}}"#
        );
        if deepseek::validate(body.as_bytes(), &p).await.is_ok() {
            accepted_invalid.push(value);
        }
    }
    assert!(
        accepted_invalid.is_empty(),
        "불법 top_p 수용: {accepted_invalid:?}"
    );
    for value in ["0.000001", "1", "1.0", "1e-3"] {
        let body = format!(
            r#"{{"model":"synthetic-deepseek","max_tokens":100,"messages":[{{"role":"user","content":"synthetic"}}],"thinking":{{"type":"disabled"}},"top_p":{value}}}"#
        );
        assert!(
            deepseek::validate(body.as_bytes(), &p).await.is_ok(),
            "합법 top_p {value}"
        );
    }
    for value in ["-0", "0", "2"] {
        let body = format!(
            r#"{{"model":"synthetic-deepseek","max_tokens":100,"messages":[{{"role":"user","content":"synthetic"}}],"thinking":{{"type":"disabled"}},"temperature":{value}}}"#
        );
        assert!(
            deepseek::validate(body.as_bytes(), &p).await.is_ok(),
            "temperature 기존 닫힌구간 {value}"
        );
    }
}
