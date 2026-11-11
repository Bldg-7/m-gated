use m_gated::{config::ModelProfile, providers::openai};
fn profile(api: &str) -> ModelProfile {
    serde_json::from_value(serde_json::json!({"model":"synthetic-text","api":api,"profile_version":"fixture-v1","pricing_version":"price-v1","fixture_version":"native-v1","actual_models":["synthetic-text"],"chat_output_limit_field":if api == "openai_chat" {Some("max_completion_tokens")} else {None},"embedding_dimensions":if api == "embeddings" {Some(serde_json::json!({"default":3,"max":4096}))} else {None},"embedding_encodings":if api == "embeddings" {vec!["float","base64"]} else {vec![]},"max_input_tokens":4096,"max_output_tokens":1024,"max_body_bytes":8388608,"max_response_bytes":16777216,"rates":{"input":"1.000000000000","output":"2.000000000000"}})).unwrap()
}
#[tokio::test]
async fn chat_requires_usage_and_model_output_profile() {
    let body=br#"{"model":"synthetic-text","messages":[{"role":"user","content":"synthetic"}],"max_completion_tokens":100,"stream":true,"stream_options":{"include_usage":true}}"#;
    assert!(
        openai::validate(body, &profile("openai_chat"))
            .await
            .is_ok(),
        "유효 Chat 전용 계약을 수용해야 한다"
    );
}
#[tokio::test]
async fn embeddings_bound_input_dimensions_and_response() {
    let body=br#"{"model":"synthetic-text","input":["synthetic"],"dimensions":3,"encoding_format":"float"}"#;
    assert!(
        openai::validate(body, &profile("embeddings")).await.is_ok(),
        "유효 Embeddings 전용 계약을 수용해야 한다"
    );
}

#[tokio::test]
async fn chat_usage_only_done_and_reasoning_are_independent() {
    use m_gated::{providers::openai_chat::ChatStream, stream::SseDecoder};
    let wire = concat!(
        "data: {\"object\":\"chat.completion.chunk\",\"id\":\"c1\",\"model\":\"synthetic-text\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"synthetic\"},\"finish_reason\":\"stop\"}],\"usage\":null}\r\n\r\n",
        "data: {\"object\":\"chat.completion.chunk\",\"id\":\"c1\",\"model\":\"synthetic-text\",\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":20,\"total_tokens\":120,\"prompt_tokens_details\":{\"cached_tokens\":40},\"completion_tokens_details\":{\"reasoning_tokens\":10}}}\n\n",
        "data: [DONE]\n\n"
    );
    let fields = [
        "object",
        "id",
        "model",
        "choices",
        "choices.$length",
        "choices.*",
        "choices.*.delta",
        "choices.*.index",
        "choices.*.finish_reason",
        "usage.**",
    ];
    let mut parser = SseDecoder::chat(&fields);
    let mut reducer = ChatStream::default();
    for &b in wire.as_bytes() {
        if let Some(e) = parser.byte(b).unwrap() {
            reducer.event(e).unwrap();
        }
    }
    parser.finish().unwrap();
    let usage = reducer.finish().unwrap().unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens,
            usage.reasoning_tokens
        ),
        (100, 20, 40, Some(10))
    );
    assert!(parser.retained_peak() <= 256 * 1024);
}

#[tokio::test]
async fn chat_null_content_and_schema_scalars_never_dispatch() {
    let p = profile("openai_chat");
    for message in [
        r#"{"role":"user","content":null}"#,
        r#"{"role":"tool","content":null,"tool_call_id":"c1"}"#,
    ] {
        let raw = format!(
            r#"{{"model":"synthetic-text","max_completion_tokens":100,"messages":[{message}]}}"#
        );
        assert!(
            openai::validate(raw.as_bytes(), &p).await.is_err(),
            "assistant 이외 null content 거부"
        );
    }
}

#[tokio::test]
async fn embeddings_float_structure_guard_matches_parser_counter_boundary() {
    use m_gated::providers::embeddings::{response_usage, validate_shape};
    let p = profile("embeddings");
    for dim in [3444, 3445] {
        let body=serde_json::json!({"model":"synthetic-text","input":vec!["x";29],"dimensions":dim,"encoding_format":"float"}).to_string();
        let result = validate_shape(body.as_bytes(), &p).await;
        assert_eq!(result.is_ok(), dim == 3444); // 99,999 vs100,028 실제 구조 tokens
        let shape = m_gated::providers::embeddings::Shape {
            count: 29,
            dimensions: dim,
            encoding: "float".into(),
        };
        let response=serde_json::json!({"object":"list","model":"synthetic-text","data":(0..29).map(|i|serde_json::json!({"object":"embedding","index":i,"embedding":vec![0.5;dim]})).collect::<Vec<_>>(),"usage":{"prompt_tokens":100,"total_tokens":100}}).to_string();
        assert_eq!(
            response_usage(response.as_bytes(), &shape).await.is_ok(),
            dim == 3444
        );
    }
    struct Count;
    impl m_gated::json::Visitor for Count {
        fn array_limit(&self, _: &[String]) -> usize {
            100_001
        }
        fn value(
            &mut self,
            _: &[String],
            _: m_gated::json::Kind,
            _: &str,
        ) -> Result<(), m_gated::json::JsonError> {
            Ok(())
        }
    }
    for count in [99_999, 100_000] {
        let raw = format!("[{}]", vec!["0"; count].join(","));
        assert_eq!(
            m_gated::json::scan(raw.as_bytes(), &mut Count)
                .await
                .is_ok(),
            count == 99_999
        );
    }
}
#[tokio::test]
async fn chat_feature_context_remote_resources_and_duplicate_fields_reject() {
    let mut p = profile("openai_chat");
    p.features = vec![
        m_gated::config::NativeFeature::ClientTools,
        m_gated::config::NativeFeature::InlinePng,
        m_gated::config::NativeFeature::JsonSchema,
    ];
    for message in [
        serde_json::json!({"role":"system","content":[{"type":"image_url","image_url":{"url":"https://example.invalid/x"}}]}),
        serde_json::json!({"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.invalid/x"}}]}),
        serde_json::json!({"role":"user","tool_calls":[],"content":"x"}),
        serde_json::json!({"role":"assistant","content":[{"type":"input_text","text":"x"}]}),
        serde_json::json!({"role":"tool","content":[{"type":"text","text":"x"}],"tool_call_id":"c1"}),
    ] {
        let raw=serde_json::json!({"model":"synthetic-text","messages":[message],"max_completion_tokens":100}).to_string();
        assert!(openai::validate(raw.as_bytes(), &p).await.is_err());
    }
    for extra in [
        r#", "tools":[{"type":"web_search"}]"#,
        r#", "response_format":{"type":"json_schema","json_schema":{"name":"x","schema":[]}}"#,
        r#", "tools":[{"type":"function","function":{"name":"f","parameters":[]}}]"#,
        r#", "model":"synthetic-text"#,
        r#", "previous_response_id":"r1"#,
    ] {
        let raw=r#"{"model":"synthetic-text","messages":[{"role":"user","content":"x"}],"max_completion_tokens":100"#.to_owned()+extra+"}";
        assert!(openai::validate(raw.as_bytes(), &p).await.is_err());
    }
}

#[test]
fn embeddings_input_only_price_never_requires_or_invents_output_rate() {
    use m_gated::{
        config::ProviderId,
        ledger::finance::{FinalUsage, PricingSnapshot},
    };
    let mut p = profile("embeddings");
    p.rates.remove("output");
    let price = PricingSnapshot::from_profile(ProviderId::Openai, &p);
    let u = FinalUsage {
        actual_model: "synthetic-text".into(),
        input_tokens: 100,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: None,
    };
    assert_eq!(
        price.price(&u, false).map(|v| v.to_string()),
        Ok("0.000100000000".into()),
        "Embeddings 청구 입력100·비청구 output0에 output요율은 불필요"
    );
}
#[test]
fn embeddings_price_rejects_missing_input_extra_rates_output_cache_reasoning_and_unknown_model() {
    use m_gated::{
        config::ProviderId,
        ledger::finance::{FinalUsage, PricingSnapshot},
    };
    let mut p = profile("embeddings");
    p.rates.remove("output");
    let price = PricingSnapshot::from_profile(ProviderId::Openai, &p);
    let valid = FinalUsage {
        actual_model: "synthetic-text".into(),
        input_tokens: 100,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: None,
    };
    for field in ["output", "cache_read", "cache_write", "reasoning", "model"] {
        let mut u = valid.clone();
        match field {
            "output" => u.output_tokens = 1,
            "cache_read" => u.cache_read_tokens = 1,
            "cache_write" => u.cache_write_tokens = 1,
            "reasoning" => u.reasoning_tokens = Some(0),
            _ => u.actual_model = "unknown".into(),
        };
        assert!(price.price(&u, false).is_err());
    }
    let mut extra = price.clone();
    extra.rates.push(m_gated::ledger::finance::PriceRate {
        category: "output".into(),
        usd_per_million_tokens: "0.000000000000".into(),
    });
    assert!(extra.price(&valid, false).is_err());
    let mut missing = price;
    missing.rates.clear();
    assert!(missing.price(&valid, false).is_err());
    for api in ["openai_chat", "responses"] {
        let mut p = profile(api);
        p.rates.remove("output");
        let price = PricingSnapshot::from_profile(ProviderId::Openai, &p);
        assert!(
            price.price(&valid, false).is_err(),
            "생성의 output0은 누락요율0 추정이 아니다"
        );
    }
}

#[tokio::test]
async fn chat_opaque_schema_literal_length_property_remains_supported() {
    let mut p = profile("openai_chat");
    p.features = vec![
        m_gated::config::NativeFeature::ClientTools,
        m_gated::config::NativeFeature::JsonSchema,
    ];
    let raw=br#"{"model":"synthetic-text","max_completion_tokens":100,"messages":[{"role":"user","content":"synthetic"}],"tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object","properties":{"$length":{"type":"integer"}}}}}],"response_format":{"type":"json_schema","json_schema":{"name":"answer","schema":{"type":"object","properties":{"$length":{"type":"integer"}}}}}}"#;
    assert!(
        openai::validate(raw, &p).await.is_ok(),
        "배열검증을 opaque schema의 사용자 property 금지로 확대하지 않음"
    );
}
