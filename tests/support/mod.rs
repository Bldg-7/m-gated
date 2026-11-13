use std::collections::BTreeMap;

pub const YAML: &str = "public_origin: https://m-gated.example.test
auth:
  issuer: https://auth.example.test
  jwks_uri: https://auth.example.test/.well-known/jwks.json
  audience: https://example.test
  access_cookie_name: mgated_access
  login_url: https://auth.example.test/sso/login
  refresh_url: https://auth.example.test/sso/refresh
  logout_url: https://auth.example.test/sso/logout
allowed_emails:
  - user@example.invalid
admin_emails:
  - admin@example.invalid
database:
  host: 127.0.0.1
  port: 64603
  name: m_gated_test
  user: m_gated_test
providers:
  - provider: anthropic
    base_url: https://api.anthropic.com
    profiles:
      - model: synthetic-text
        api: messages
        profile_version: fixture-v1
        pricing_version: synthetic-price-v1
        fixture_version: text-v1
        actual_models:
          - synthetic-text
        input_bound_contract:
          kind: anthropic_context_v1
          context_tokens: 4096
          input_categories: [input, cache_read, cache_write]
        max_input_tokens: 4096
        max_output_tokens: 1024
        max_body_bytes: 8388608
        max_response_bytes: 16777216
        rates:
          input: '1.000000000000'
          output: '2.000000000000'
";

pub fn environment() -> BTreeMap<String, String> {
    [
        ("DB_PASSWORD", "synthetic-db-secret"),
        (
            "CSRF_SIGNING_KEY",
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        ),
        (
            "CREDENTIAL_ENCRYPTION_KEY",
            "202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f",
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}
