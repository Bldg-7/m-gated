mod support;
use m_gated::config::{self, ConfigInputs, DurationPreset, StartupError};

fn load(yaml: &str) -> Result<config::ValidatedConfig, StartupError> {
    config::load(ConfigInputs {
        yaml,
        file_environment: None,
        environment: &support::environment(),
    })
}
fn reject(yaml: &str, code: StartupError) {
    assert_eq!(load(yaml).unwrap_err(), code);
}

#[test]
fn nonsecret_env_file_precedes_yaml_and_injected_values_precede_both() {
    let file = "PUBLIC_ORIGIN=https://file.example.test\nDB_PORT=65000\n";
    let mut env = support::environment();
    let config = config::load(ConfigInputs {
        yaml: support::YAML,
        file_environment: Some(file),
        environment: &env,
    })
    .unwrap();
    assert_eq!(config.database().port, 65000);
    assert_eq!(config.public_origin().host_str(), Some("file.example.test"));
    env.insert("DB_PORT".into(), "65001".into());
    assert_eq!(
        config::load(ConfigInputs {
            yaml: support::YAML,
            file_environment: Some(file),
            environment: &env
        })
        .unwrap()
        .database()
        .port,
        65001
    );
    for file in [
        "DB_PASSWORD=secret",
        "CSRF_SIGNING_KEY=secret",
        "ANTHROPIC_API_KEY=secret",
        "UNSUPPORTED=anything",
        "PUBLIC_ORIGIN=x\nPUBLIC_ORIGIN=y",
        "DB_HOST=${HOST}",
    ] {
        assert!(config::parse_non_secret_env(file).is_err());
    }
}

#[test]
fn sso_urls_share_issuer_origin_and_cookie_name_is_a_token() {
    for (from, to) in [
        (
            "login_url: https://auth.example.test",
            "login_url: https://other.example.test",
        ),
        (
            "jwks_uri: https://auth.example.test",
            "jwks_uri: https://other.example.test",
        ),
        (
            "refresh_url: https://auth.example.test",
            "refresh_url: https://other.example.test",
        ),
        (
            "logout_url: https://auth.example.test",
            "logout_url: https://other.example.test",
        ),
        ("mgated_access", "\"bad cookie\""),
        ("mgated_access", "\"bad;cookie\""),
        ("mgated_access", "\"\""),
    ] {
        reject(&support::YAML.replace(from, to), StartupError::InvalidAuth);
    }
    // SSO 주소와 cookie 이름은 특정 도메인에 묶이지 않는다.
    let custom = support::YAML
        .replace("https://auth.example.test", "https://sso.company.example")
        .replace("https://example.test", "https://company.example")
        .replace(
            "https://m-gated.example.test",
            "https://gateway.company.example",
        )
        .replace("/sso/refresh", "/oauth/refresh")
        .replace("mgated_access", "company_session");
    assert!(load(&custom).is_ok());
}

#[test]
fn secret_presence_redaction_and_inactive_provider() {
    let env = support::environment();
    let config = config::load(ConfigInputs {
        yaml: support::YAML,
        file_environment: None,
        environment: &env,
    })
    .unwrap();
    let debug = format!("{config:?} {:?}", config.providers());
    assert!(!debug.contains("synthetic-provider-secret"));
    assert!(!debug.contains("synthetic-db-secret"));
    assert!(!debug.contains(&env["CSRF_SIGNING_KEY"]));
    assert_eq!(
        config.providers().len(),
        1,
        "비활성 Provider secret은 요구하지 않는다"
    );
    for key in ["DB_PASSWORD", "CSRF_SIGNING_KEY", "ANTHROPIC_API_KEY"] {
        let mut missing = env.clone();
        missing.remove(key);
        assert_eq!(
            config::load(ConfigInputs {
                yaml: support::YAML,
                file_environment: None,
                environment: &missing
            })
            .unwrap_err(),
            StartupError::MissingSecret
        );
    }
    for key in ["DB_PASSWORD", "CSRF_SIGNING_KEY", "ANTHROPIC_API_KEY"] {
        let mut bad = env.clone();
        bad.insert(key.into(), "\nsecret-sentinel".into());
        let error = config::load(ConfigInputs {
            yaml: support::YAML,
            file_environment: None,
            environment: &bad,
        })
        .unwrap_err();
        assert_eq!(error, StartupError::InvalidSecret);
        assert!(!format!("{error:?} {error}").contains("sentinel"));
    }
    let mut short = env;
    short.insert("CSRF_SIGNING_KEY".into(), "01234567".into());
    assert_eq!(
        config::load(ConfigInputs {
            yaml: support::YAML,
            file_environment: None,
            environment: &short
        })
        .unwrap_err(),
        StartupError::InvalidSecret
    );
}

#[test]
fn injected_environment_wins_without_database_url_or_proxy_fallback() {
    let mut env = support::environment();
    env.insert(
        "PUBLIC_ORIGIN".into(),
        "https://override.example.test".into(),
    );
    env.insert("DB_PORT".into(), "65432".into());
    env.insert("ALLOWED_EMAILS".into(), "Upper@Example.Invalid".into());
    env.insert("ADMIN_EMAILS".into(), "Admin2@Example.Invalid".into());
    env.insert(
        "DATABASE_URL".into(),
        "postgres://do-not-use.invalid/prod".into(),
    );
    env.insert("HTTPS_PROXY".into(), "http://do-not-use.invalid".into());
    let config = config::load(ConfigInputs {
        yaml: support::YAML,
        file_environment: None,
        environment: &env,
    })
    .unwrap();
    assert_eq!(
        config.public_origin().as_str(),
        "https://override.example.test/"
    );
    assert_eq!(config.database().port, 65432);
    assert_eq!(config.database().host, "127.0.0.1");
    assert!(config.allowed_emails().contains("upper@example.invalid"));
    assert!(config.allowed_emails().contains("admin2@example.invalid"));
    assert!(!config.allowed_emails().contains("user@example.invalid"));
    assert_eq!(
        config.providers()[0].base_url.as_str(),
        "https://api.anthropic.com/"
    );
    env.insert("DB_PORT".into(), "".into());
    assert!(
        config::load(ConfigInputs {
            yaml: support::YAML,
            file_environment: None,
            environment: &env
        })
        .is_err(),
        "빈 주입값도 파일 fallback으로 감추지 않는다"
    );
}

#[test]
fn urls_reject_http_loopback_userinfo_fragment_query_and_noncanonical_paths() {
    for url in [
        "http://127.0.0.1:12345",
        "http://m-gated.example.test",
        "https://secret@m-gated.example.test",
        "https://@m-gated.example.test",
        "https://m-gated.example.test/../",
        "https://m-gated.example.test\\\\",
        "https://m-gated.example.test/#fragment",
        "https://m-gated.example.test/?token=sentinel",
        "https://m-gated.example.test/path",
        "https://m-gated.example.test:8443",
    ] {
        reject(
            &support::YAML.replace("https://m-gated.example.test", url),
            StartupError::InvalidUrl,
        );
    }
    for url in [
        "https://attacker.invalid",
        "https://api.anthropic.com/v1/messages",
        "http://127.0.0.1:12345",
        "https://api.anthropic.com?api_key=sentinel",
    ] {
        assert!(
            load(&support::YAML.replace("https://api.anthropic.com", url)).is_err(),
            "고정 upstream을 사용자 주소로 대체하지 않는다"
        );
    }
}

#[test]
fn profiles_require_exact_provider_api_versions_models_and_bounds() {
    for (from, to) in [
        ("api: messages", "api: responses"),
        ("model: synthetic-text", "model: ''"),
        ("profile_version: fixture-v1", "profile_version: ''"),
        ("pricing_version: synthetic-price-v1", "pricing_version: ''"),
        ("fixture_version: text-v1", "fixture_version: ''"),
        ("- synthetic-text", "- other-model"),
        ("max_input_tokens: 4096", "max_input_tokens: 0"),
        ("max_output_tokens: 1024", "max_output_tokens: 0"),
        ("max_body_bytes: 8388608", "max_body_bytes: 8388609"),
        (
            "max_response_bytes: 16777216",
            "max_response_bytes: 16777217",
        ),
        (
            "api: messages",
            "api: messages\n        billing_class: guaranteed_free",
        ),
    ] {
        reject(
            &support::YAML.replace(from, to),
            StartupError::InvalidProfile,
        );
    }
    assert!(load(&support::YAML.replace("        max_output_tokens: 1024\n", "")).is_err());
    assert!(
        load(&support::YAML.replace(
            "    profiles:\n",
            "    profiles: []\n    ignored_profiles:\n"
        ))
        .is_err()
    );
}

#[test]
fn rates_are_fixed_precision_nonnegative_and_complete_not_floats() {
    for value in [
        "-0.000000000001",
        "1.0000000000001",
        "1e0",
        "NaN",
        "Infinity",
        "10000000000000000.000000000000",
        " 1.000000000000",
        "+1.000000000000",
        "01.000000000000",
    ] {
        reject(
            &support::YAML.replace("1.000000000000", value),
            StartupError::InvalidPrice,
        );
    }
    reject(
        &support::YAML.replace("          output: '2.000000000000'\n", ""),
        StartupError::InvalidPrice,
    );
    reject(
        &support::YAML.replace("output: '2.000000000000'", "output: 2.0"),
        StartupError::ConfigSyntax,
    );
    reject(
        &support::YAML.replace(
            "output: '2.000000000000'",
            "unknown_category: '2.000000000000'",
        ),
        StartupError::InvalidPrice,
    );
}

#[test]
fn resource_limits_reject_zero_excess_and_inconsistent_deadlines() {
    for field in [
        "connections",
        "http2_streams",
        "http_requests",
        "inference",
        "per_principal",
        "pre_auth",
        "authentication",
        "nonstream",
        "management",
        "management_writes",
        "assets",
        "header_bytes",
        "header_count",
        "token_bytes",
        "header_timeout_ms",
        "inference_body_bytes",
        "management_body_bytes",
        "body_timeout_ms",
        "body_idle_timeout_ms",
        "management_timeout_ms",
        "asset_timeout_ms",
        "request_timeout_ms",
        "transport_timeout_ms",
        "finalization_timeout_ms",
        "health_interval_ms",
        "health_valid_ms",
    ] {
        reject(
            &format!("{}limits:\n  {field}: 0\n", support::YAML),
            StartupError::InvalidLimits,
        );
        reject(
            &format!("{}limits:\n  {field}: 999999999\n", support::YAML),
            StartupError::InvalidLimits,
        );
    }
    for fields in [
        "inference: 3\n  nonstream: 4",
        "management: 1\n  management_writes: 2",
        "request_timeout_ms: 5000",
        "health_valid_ms: 1000",
        "body_timeout_ms: 1000",
    ] {
        reject(
            &format!("{}limits:\n  {fields}\n", support::YAML),
            StartupError::InvalidLimits,
        );
    }
}

#[test]
fn retention_is_explicit_five_presets_and_missing_is_not_unlimited() {
    let c = load(support::YAML).unwrap();
    assert_eq!(c.retention().ledger, None);
    assert!(!c.retention().has_finite_selection());
    for (preset, months) in [
        ("3_months", Some(3)),
        ("6_months", Some(6)),
        ("1_year", Some(12)),
        ("3_years", Some(36)),
        ("unlimited", None),
    ] {
        let c = load(&format!(
            "{}retention:\n  ledger: {preset}\n  audit: {preset}\n  commands: {preset}\n",
            support::YAML
        ))
        .unwrap();
        assert_eq!(
            c.retention().ledger.map(DurationPreset::months),
            Some(months)
        );
        assert_eq!(c.retention().has_finite_selection(), months.is_some());
    }
    for field in ["ledger", "audit", "commands"] {
        for value in ["null", "~", "30", "30_days", "never", "''"] {
            reject(
                &format!("{}retention:\n  {field}: {value}\n", support::YAML),
                StartupError::ConfigSyntax,
            );
        }
    }
}

#[test]
fn yaml_duplicate_unknown_alias_tag_and_resource_exhaustion_are_rejected() {
    for suffix in [
        "environment: prod\n",
        "extra: true\n",
        "retention:\n  ledger: unlimited\n  ledger: 1_year\n",
        "retention: &selection\n  ledger: unlimited\n",
        "retention: !!map {}\n",
        "retention:\n  <<: *selection\n",
    ] {
        assert!(load(&format!("{}{suffix}", support::YAML)).is_err());
    }
    assert!(
        load(&format!(
            "{}retention: &selection\n  ledger: unlimited\n",
            support::YAML.replace("fixture_version: text-v1", "fixture_version: plain'quote")
        ))
        .is_err(),
        "plain scalar의 apostrophe가 alias 검사를 무효화하지 않는다"
    );
    assert!(
        load(&support::YAML.replace(
            "          output:",
            "          \"\\u0069nput\": '2.000000000000'\n          output:"
        ))
        .is_err(),
        "decode 후 같은 rates key를 덮어쓰지 않는다"
    );
    reject(&" ".repeat(256 * 1024 + 1), StartupError::ConfigInput);
    reject(
        &format!("{}{}", "[".repeat(33), "]".repeat(33)),
        StartupError::ConfigInput,
    );
}

#[test]
fn database_and_email_inputs_are_not_connection_strings_or_wildcards() {
    for (from, to) in [
        ("host: 127.0.0.1", "host: 'host dbname=elsewhere'"),
        ("port: 64603", "port: 0"),
        ("name: m_gated_test", "name: 'other/db'"),
        ("user: m_gated_test", "user: ''"),
    ] {
        reject(
            &support::YAML.replace(from, to),
            StartupError::InvalidDatabase,
        );
    }
    for value in [
        "*@example.invalid",
        "@example.invalid",
        "user@",
        "user@example.invalid,other@example.invalid",
    ] {
        reject(
            &support::YAML.replace("user@example.invalid", &format!("'{value}'")),
            StartupError::InvalidIdentity,
        );
    }
}

#[test]
fn only_declared_provider_api_profiles_are_configurable_and_free_count_is_explicit() {
    for (provider, origin, key, api, no_output) in [
        (
            "anthropic",
            "https://api.anthropic.com",
            "ANTHROPIC_API_KEY",
            "messages",
            false,
        ),
        (
            "anthropic",
            "https://api.anthropic.com",
            "ANTHROPIC_API_KEY",
            "count_tokens",
            true,
        ),
        (
            "openai",
            "https://api.openai.com",
            "OPENAI_API_KEY",
            "responses",
            false,
        ),
        (
            "openai",
            "https://api.openai.com",
            "OPENAI_API_KEY",
            "openai_chat",
            false,
        ),
        (
            "openai",
            "https://api.openai.com",
            "OPENAI_API_KEY",
            "embeddings",
            true,
        ),
        (
            "deepseek",
            "https://api.deepseek.com",
            "DEEPSEEK_API_KEY",
            "deepseek_chat",
            false,
        ),
    ] {
        let mut yaml = support::YAML
            .replace("provider: anthropic", &format!("provider: {provider}"))
            .replace("https://api.anthropic.com", origin)
            .replace("api: messages", &format!("api: {api}"));
        if provider != "anthropic" {
            yaml=yaml.replace("        input_bound_contract:\n          kind: anthropic_context_v1\n          context_tokens: 4096\n          input_categories: [input, cache_read, cache_write]\n","");
        }
        if no_output {
            yaml = yaml
                .replace("        max_output_tokens: 1024\n", "")
                .replace("          output: '2.000000000000'\n", "");
        }
        if api == "deepseek_chat" {
            yaml = yaml.replace(
                "api: deepseek_chat",
                "api: deepseek_chat\n        billing_time_rule: fixed_admission_v1",
            );
        }
        if api == "deepseek_chat" {
            yaml = yaml.replace(
                "          input: '1.000000000000'",
                "          input: '1.000000000000'\n          cache_read: '0.500000000000'",
            );
        }
        if api == "openai_chat" {
            yaml = yaml.replace(
                "api: openai_chat",
                "api: openai_chat\n        chat_output_limit_field: max_completion_tokens",
            );
        }
        if api == "embeddings" {
            yaml = yaml.replace("api: embeddings", "api: embeddings\n        embedding_dimensions: {default: 3, max: 4096}\n        embedding_encodings: [float, base64]");
            yaml = yaml.replace("          output: '2.000000000000'\n", "");
        }
        if api == "count_tokens" {
            yaml = yaml
                .replace("max_response_bytes: 16777216", "max_response_bytes: 262144")
                .replace(
                    "api: count_tokens",
                    "api: count_tokens\n        count_billing: counted_input_tokens",
                );
        }
        let mut env = support::environment();
        env.remove("ANTHROPIC_API_KEY");
        env.insert(key.into(), "synthetic-provider-secret".into());
        assert!(
            config::load(ConfigInputs {
                yaml: &yaml,
                file_environment: None,
                environment: &env
            })
            .is_ok()
        );
        if api == "count_tokens" {
            let free = yaml
                .replace("        count_billing: counted_input_tokens\n", "")
                .replace(
                    "api: count_tokens",
                    "api: count_tokens\n        billing_class: guaranteed_free",
                )
                .replace("1.000000000000", "0.000000000000")
                .replace("2.000000000000", "0.000000000000");
            assert_eq!(
                config::load(ConfigInputs {
                    yaml: &free,
                    file_environment: None,
                    environment: &env
                })
                .unwrap()
                .providers()[0]
                    .profiles[0]
                    .billing_class,
                config::BillingClass::GuaranteedFree
            );
        }
    }
    let (head, provider) = support::YAML.split_once("providers:\n").unwrap();
    reject(
        &format!("{head}providers: []\n"),
        StartupError::InvalidProfile,
    );
    reject(
        &format!("{}{}", support::YAML, provider),
        StartupError::InvalidProfile,
    );
    let profile = support::YAML.split_once("      - model:").unwrap().1;
    reject(
        &format!("{}      - model:{}", support::YAML, profile),
        StartupError::InvalidProfile,
    );
    reject(
        &support::YAML.replace(
            "          - synthetic-text",
            "          - synthetic-text\n          - synthetic-text",
        ),
        StartupError::InvalidProfile,
    );
    for scalar in [
        "plain'quote",
        "plain:\"quote",
        "plain:'quote",
        "plain\"quote",
    ] {
        assert!(
            load(&format!(
                "{}retention: &selection\n  ledger: unlimited\n",
                support::YAML.replace(
                    "fixture_version: text-v1",
                    &format!("fixture_version: {scalar}")
                )
            ))
            .is_err()
        );
    }
}

#[test]
fn jwt_identifiers_preserve_canonical_literal_not_url_normalization() {
    let cfg = load(support::YAML).unwrap();
    assert_eq!(cfg.auth().issuer_literal, "https://auth.example.test");
    assert_eq!(cfg.auth().audience_literal, "https://example.test");
    for (from, to) in [
        (
            "issuer: https://auth.example.test",
            "issuer: https://AUTH.example.test",
        ),
        (
            "issuer: https://auth.example.test",
            "issuer: https://auth.example.test/",
        ),
        (
            "audience: https://example.test",
            "audience: https://example.test/",
        ),
        (
            "audience: https://example.test",
            "audience: https://example%2Etest",
        ),
    ] {
        assert!(
            load(&support::YAML.replace(from, to)).is_err(),
            "canonical JWT 설정 식별자를 URL 정규화해서는 안 된다"
        );
    }
}

#[test]
fn anthropic_native_features_prices_and_access_digest_are_explicit() {
    use m_gated::gateway::auth_config_digest;
    let original = load(support::YAML).unwrap();
    let digest = auth_config_digest(&original);
    for change in [
        support::YAML.replace(
            "fixture_version: text-v1",
            "fixture_version: text-v1\n        approved_beta: [synthetic-beta-2026-01-01]",
        ),
        support::YAML.replace(
            "          - synthetic-text",
            "          - synthetic-text\n          - synthetic-actual-alias",
        ),
        support::YAML.replace(
            "fixture_version: text-v1",
            "fixture_version: text-v1\n        features: [client_tools]",
        ),
    ] {
        assert!(
            auth_config_digest(&load(&change).unwrap()) != digest,
            "security profile change did not alter access digest"
        );
    }
    let cache = support::YAML.replace(
        "fixture_version: text-v1",
        "fixture_version: text-v1\n        features: [cache]",
    );
    reject(&cache, StartupError::InvalidPrice);
    let valid=cache.replace("output: '2.000000000000'","output: '2.000000000000'\n          cache_read: '0.100000000000'\n          cache_write_5m: '2.500000000000'");
    load(&valid).unwrap();
    reject(
        &valid.replace(
            "cache_write_5m: '2.500000000000'",
            "cache_write_5m: '2.500000000000'\n          cache_write: '2.500000000000'",
        ),
        StartupError::InvalidPrice,
    );
    reject(
        &valid.replace(
            "cache_write_5m: '2.500000000000'",
            "cache_write_1h: '2.500000000000'",
        ),
        StartupError::InvalidPrice,
    );
    let mut secrets = support::environment();
    secrets.insert("ANTHROPIC_API_KEY".into(), "other-synthetic-secret".into());
    let changed = config::load(ConfigInputs {
        yaml: support::YAML,
        file_environment: None,
        environment: &secrets,
    })
    .unwrap();
    assert!(
        auth_config_digest(&changed) == digest,
        "secret entered access fingerprint"
    );
}

#[test]
fn execution_ceiling_is_strict_and_changes_access_digest() {
    use m_gated::gateway::auth_config_digest;
    let original = load(support::YAML).unwrap();
    for suffix in [
        "execution_ceiling: {rpm: 0}",
        "execution_ceiling: {tpm: 1000000001}",
        "execution_ceiling: {unexpected: 1}",
        "execution_ceiling: {budget: {mode: actual, limit_usd: '1.000000000000', period: {kind: calendar_month, timezone: Not/A_Zone}}}",
        "execution_ceiling: {budget: {mode: actual, limit_usd: '0.0000000000001', period: {kind: calendar_month, timezone: UTC}}}",
    ] {
        assert!(load(&format!("{}{suffix}\n", support::YAML)).is_err());
    }
    let valid=load(&format!("{}execution_ceiling: {{budget: {{mode: reserved, limit_usd: '1.000000000000', period: {{kind: calendar_month, timezone: America/New_York}}}}, rpm: 2, tpm: 4096, count_rpm: 3}}\n",support::YAML)).unwrap();
    assert_ne!(auth_config_digest(&original), auth_config_digest(&valid));
    assert_eq!(valid.execution_ceiling().rpm, Some(2));
    let without=load(&support::YAML.replace("        input_bound_contract:\n          kind: anthropic_context_v1\n          context_tokens: 4096\n          input_categories: [input, cache_read, cache_write]\n","")).unwrap();
    assert_ne!(auth_config_digest(&original), auth_config_digest(&without));
    reject(
        &support::YAML.replace("context_tokens: 4096", "context_tokens: 4095"),
        StartupError::InvalidProfile,
    );
    reject(
        &support::YAML.replace(
            "input_categories: [input, cache_read, cache_write]",
            "input_categories: [input]",
        ),
        StartupError::InvalidProfile,
    );
    reject(
        &support::YAML.replace(
            "api: messages",
            "api: messages\n        internal_count: true",
        ),
        StartupError::InvalidProfile,
    );
}

#[test]
fn responses_context_contract_is_api_specific_and_typed() {
    let yaml = support::YAML
        .replace("provider: anthropic", "provider: openai")
        .replace("https://api.anthropic.com", "https://api.openai.com")
        .replace("api: messages", "api: responses")
        .replace("anthropic_context_v1", "openai_responses_context_v1")
        .replace("[input, cache_read, cache_write]", "[input]");
    let mut environment = support::environment();
    environment.insert("OPENAI_API_KEY".into(), "synthetic-company-key".into());
    let result = config::load(ConfigInputs {
        yaml: &yaml,
        file_environment: None,
        environment: &environment,
    });
    assert!(
        result.is_ok(),
        "검증된 Responses native context 계약을 거부했다"
    );
    for bad in [
        yaml.replace("[input]", "[input, cache_read]"),
        yaml.replace("context_tokens: 4096", "context_tokens: 4095"),
        yaml.replace("openai_responses_context_v1", "anthropic_context_v1"),
    ] {
        assert!(
            config::load(ConfigInputs {
                yaml: &bad,
                file_environment: None,
                environment: &environment
            })
            .is_err()
        );
    }
}

#[test]
fn chat_and_embeddings_profiles_require_api_specific_output_dimensions_and_encodings() {
    let base = support::YAML.replace("provider: anthropic", "provider: openai").replace("https://api.anthropic.com", "https://api.openai.com").replace("        input_bound_contract:\n          kind: anthropic_context_v1\n          context_tokens: 4096\n          input_categories: [input, cache_read, cache_write]\n", "");
    let mut env = support::environment();
    env.insert("OPENAI_API_KEY".into(), "synthetic-company-key".into());
    let valid = |yaml: &str| {
        config::load(ConfigInputs {
            yaml,
            file_environment: None,
            environment: &env,
        })
        .is_ok()
    };
    let chat = base.replace(
        "api: messages",
        "api: openai_chat\n        chat_output_limit_field: max_completion_tokens",
    );
    assert!(valid(&chat));
    for replacement in [
        "",
        "\n        chat_output_limit_field: max_output_tokens",
        "\n        chat_output_limit_field: null",
        "\n        chat_output_limit_field: max_tokens\n        features: [thinking]",
        "\n        chat_output_limit_field: max_completion_tokens\n        embedding_encodings: [float]",
    ] {
        let yaml = chat.replace(
            "\n        chat_output_limit_field: max_completion_tokens",
            replacement,
        );
        assert!(!valid(&yaml));
    }
    let embedding=base.replace("api: messages","api: embeddings\n        embedding_dimensions: {default: 3, max: 4096}\n        embedding_encodings: [float, base64]").replace("        max_output_tokens: 1024\n", "").replace("          output: '2.000000000000'\n", "");
    assert!(valid(&embedding));
    for (old, new) in [
        (
            "        embedding_dimensions: {default: 3, max: 4096}\n",
            "",
        ),
        ("default: 3", "default: 0"),
        ("default: 3", "default: 4097"),
        ("max: 4096", "max: 4097"),
        ("[float, base64]", "[]"),
        ("[float, base64]", "[float, float]"),
        ("[float, base64]", "[float, int8]"),
        (
            "api: embeddings",
            "api: embeddings\n        features: [client_tools]",
        ),
        (
            "api: embeddings",
            "api: embeddings\n        chat_output_limit_field: max_tokens",
        ),
    ] {
        assert!(!valid(&embedding.replace(old, new)));
    }
}

#[test]
fn deepseek_requires_explicit_time_and_own_context_contract() {
    let base = include_str!("support/config.yaml")
        .replace("provider: anthropic", "provider: deepseek")
        .replace("https://api.anthropic.com", "https://api.deepseek.com")
        .replace("api: messages", "api: deepseek_chat");
    let base = base.replace(
        "          input: '1.000000000000'",
        "          input: '1.000000000000'\n          cache_read: '0.500000000000'",
    );
    let mut env = support::environment();
    env.insert("DEEPSEEK_API_KEY".into(), "synthetic-deepseek-key".into());
    let load = |yaml: &str| {
        config::load(ConfigInputs {
            yaml,
            file_environment: None,
            environment: &env,
        })
    };
    assert!(load(&base).is_err());
    for time in ["fixed_admission_v1", "deepseek_time_unresolved_v1"] {
        let yaml=base.replace("api: deepseek_chat",&format!("api: deepseek_chat\n        billing_time_rule: {time}\n        input_bound_contract:\n          kind: deepseek_chat_context_v1\n          context_tokens: 4096\n          input_categories: [messages]"));
        assert!(load(&yaml).is_ok());
        assert!(load(&yaml.replace("deepseek_chat_context_v1", "openai_chat_context_v1")).is_err());
        assert!(load(&yaml.replace("billing_time_rule:", "unknown_billing_rule:")).is_err());
        assert!(load(&yaml.replace(time, "unverified_utc_schedule")).is_err());
    }
    let other = include_str!("support/config.yaml").replace(
        "api: messages",
        "api: messages\n        billing_time_rule: deepseek_time_unresolved_v1",
    );
    assert!(load(&other).is_err());
}
#[test]
fn deepseek_price_categories_are_not_anthropic_rates() {
    let base = include_str!("support/config.yaml")
        .replace("provider: anthropic", "provider: deepseek")
        .replace("https://api.anthropic.com", "https://api.deepseek.com")
        .replace(
            "api: messages",
            "api: deepseek_chat\n        billing_time_rule: fixed_admission_v1",
        );
    let mut env = support::environment();
    env.insert("DEEPSEEK_API_KEY".into(), "synthetic-deepseek".into());
    assert!(
        config::load(ConfigInputs {
            yaml: &base,
            file_environment: None,
            environment: &env
        })
        .is_err(),
        "DeepSeek hit 요율 누락을 자동승계하지 않음"
    );
    let valid = base.replace(
        "          input: '1.000000000000'",
        "          input: '1.000000000000'\n          cache_read: '0.500000000000'",
    );
    assert!(
        config::load(ConfigInputs {
            yaml: &valid,
            file_environment: None,
            environment: &env
        })
        .is_ok()
    );
    let bad = valid.replace(
        "          input: '1.000000000000'",
        "          input: '1.000000000000'\n          cache_write_5m: '1.000000000000'",
    );
    assert!(
        config::load(ConfigInputs {
            yaml: &bad,
            file_environment: None,
            environment: &env
        })
        .is_err()
    );
}

#[test]
fn claude_code_sized_beta_allowlist_is_bounded_at_32() {
    // Claude Code 2.1.322는 실제 모델 이름에서 beta 8개를 보낸다. 업데이트 여유를 두되 상한은 유지한다.
    let with = |n: usize| {
        let list = (0..n)
            .map(|i| format!("synthetic-beta-{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        support::YAML.replace(
            "fixture_version: text-v1",
            &format!("fixture_version: text-v1\n        approved_beta: [{list}]"),
        )
    };
    load(&with(9)).unwrap();
    load(&with(32)).unwrap();
    reject(&with(33), StartupError::InvalidProfile);
}
