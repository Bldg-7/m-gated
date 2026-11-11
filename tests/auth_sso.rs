#[path = "support/sso.rs"]
mod sso;
mod support;
use axum::http::StatusCode;
use m_gated::{
    auth::{AuthError, SsoVerifier, jwks::JwksCache},
    config::{self, ConfigInputs},
};
use serde_json::json;
use std::{sync::Arc, time::Duration};
fn config() -> config::ValidatedConfig {
    config::load(ConfigInputs {
        yaml: support::YAML,
        file_environment: None,
        environment: &support::environment(),
    })
    .unwrap()
}
async fn fixture() -> (Arc<sso::FakeJwks>, sso::Server, Arc<JwksCache>, SsoVerifier) {
    let fake = sso::FakeJwks::new(vec![sso::Signer::shared().jwk.clone()]);
    let server = fake.serve().await;
    let cache = Arc::new(JwksCache::loopback_for_test(server.uri.clone()).unwrap());
    let verifier = SsoVerifier::new(config().auth(), cache.clone());
    (fake, server, cache, verifier)
}
#[tokio::test]
async fn jwt_claims_are_strict_and_environment_bound() {
    let (fake, _server, _cache, verifier) = fixture().await;
    let signer = sso::Signer::shared();
    let original = sso::claims();
    let good = sso::credential(signer.sign(&original, "key1"));
    let identity = verifier.verify(&good, 1900000000).await.unwrap();
    assert_eq!(identity.issuer(), "https://auth.example.test");
    assert_eq!(identity.email(), "alice@example.test");
    assert_eq!(identity.subject(), "human-1");
    let cases = [
        ("iss", json!("https://auth.example.test/")),
        ("iss", json!("https://AUTH.example.test")),
        ("iss", json!("https://auth%2Eexample.test")),
        ("aud", json!("https://example.test/")),
        ("iss", json!("https://auth.other.test")),
        ("aud", json!("https://other.test")),
        ("sub", json!(123)),
        ("sub", json!("")),
        ("email_verified", json!(false)),
        ("email_verified", json!("true")),
        ("email", json!(123)),
        ("exp", json!(1900000000 - 30)),
        ("exp", json!(1.9e9)),
        ("exp", json!("2000000000")),
        ("nbf", json!(1900000031)),
        ("nbf", json!(null)),
    ];
    for (field, value) in cases {
        let mut c = original.clone();
        c[field] = value;
        assert!(
            matches!(
                verifier
                    .verify(&sso::credential(signer.sign(&c, "key1")), 1900000000)
                    .await,
                Err(AuthError::InvalidCredentials)
            ),
            "{field}"
        );
    }
    for field in ["exp", "iss", "aud", "sub", "email", "email_verified"] {
        let mut c = original.clone();
        c.as_object_mut().unwrap().remove(field);
        assert!(
            verifier
                .verify(&sso::credential(signer.sign(&c, "key1")), 1900000000)
                .await
                .is_err()
        );
    }
    let mut edge = original.clone();
    edge["exp"] = json!(1899999971);
    edge["nbf"] = json!(1900000030);
    assert!(
        verifier
            .verify(&sso::credential(signer.sign(&edge, "key1")), 1900000000)
            .await
            .is_ok()
    );
    let mut forged = good_token_parts(signer.sign(&original, "key1"));
    let original_signature = forged[2].clone();
    forged[2] = tamper_signature(&original_signature);
    {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        assert!(
            URL_SAFE_NO_PAD.decode(&original_signature).unwrap()
                != URL_SAFE_NO_PAD.decode(&forged[2]).unwrap(),
            "negative fixture must change actual signature bytes"
        );
    }
    assert!(
        verifier
            .verify(&sso::credential(forged.join(".")), 1900000000)
            .await
            .is_err()
    );
    let hs = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &original,
        &jsonwebtoken::EncodingKey::from_secret(b"synthetic-only"),
    )
    .unwrap();
    assert!(
        verifier
            .verify(&sso::credential(hs), 1900000000)
            .await
            .is_err()
    );
    assert_eq!(fake.count(), 1);
}
fn tamper_signature(encoded: &str) -> String {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let mut bytes = URL_SAFE_NO_PAD.decode(encoded).unwrap();
    bytes[0] ^= 1;
    URL_SAFE_NO_PAD.encode(bytes)
}
#[test]
fn signature_tamper_always_changes_decoded_bytes_including_a_prefix() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    for bytes in [vec![0u8; 256], vec![255u8; 256]] {
        let original = URL_SAFE_NO_PAD.encode(&bytes);
        let changed = URL_SAFE_NO_PAD.decode(tamper_signature(&original)).unwrap();
        assert!(
            changed != bytes,
            "signature negative fixture did not alter bytes"
        );
    }
}
fn good_token_parts(token: String) -> Vec<String> {
    token.split('.').map(str::to_owned).collect()
}

#[tokio::test]
async fn random_kid_rotation_and_expiry_are_bounded() {
    let (fake, _server, cache, verifier) = fixture().await;
    let signer = sso::Signer::shared();
    let mut now = tokio::time::Instant::now();
    assert!(cache.key_at("key1", now).await.is_ok());
    assert!(
        verifier
            .verify(
                &sso::credential(signer.sign(&sso::claims(), "key1")),
                1900000000
            )
            .await
            .is_ok()
    );
    for i in 0..1000 {
        assert!(matches!(
            cache.key_at(&format!("random-{i}"), now).await,
            Err(AuthError::InvalidCredentials)
        ));
    }
    for i in 0..1000 {
        let token = sso::Signer::shared().sign(&sso::claims(), &format!("signed-random-{i}"));
        assert!(matches!(
            verifier.verify(&sso::credential(token), 1900000000).await,
            Err(AuthError::InvalidCredentials)
        ));
    }
    assert_eq!(fake.count(), 1);
    now += Duration::from_secs(10);
    let mut rotated = signer.jwk.clone();
    rotated["kid"] = json!("key2");
    fake.keys(vec![rotated]).await;
    let mut jobs = Vec::new();
    for _ in 0..32 {
        let c = cache.clone();
        jobs.push(tokio::spawn(
            async move { c.key_at("key2", now).await.is_ok() },
        ));
    }
    for job in jobs {
        assert!(job.await.unwrap());
    }
    assert_eq!(fake.count(), 2);
    assert!(matches!(
        cache.key_at("key1", now).await,
        Err(AuthError::InvalidCredentials)
    ));
    fake.set(StatusCode::INTERNAL_SERVER_ERROR, vec![]).await;
    now += Duration::from_secs(299);
    assert!(cache.key_at("key2", now).await.is_ok());
    assert!(cache.is_healthy_at(now).await);
    now += Duration::from_secs(1);
    assert!(matches!(
        cache.key_at("key2", now).await,
        Err(AuthError::DependencyUnavailable)
    ));
    assert_eq!(fake.count(), 3);
    assert!(!cache.is_healthy_at(now).await);
    assert!(matches!(
        cache.key_at("key2", now).await,
        Err(AuthError::DependencyUnavailable)
    ));
    assert_eq!(fake.count(), 3);
    now += Duration::from_secs(10);
    fake.keys(vec![signer.jwk.clone()]).await;
    assert!(cache.key_at("key1", now).await.is_ok());
    assert_eq!(fake.count(), 4);
    for _ in 0..20 {
        assert!(cache.is_healthy_at(now).await);
    }
    assert_eq!(fake.count(), 4);
}

#[tokio::test]
async fn invalid_jwks_never_replaces_valid_set() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let key = sso::Signer::shared().jwk.clone();
    let mut even_modulus = URL_SAFE_NO_PAD.decode(key["n"].as_str().unwrap()).unwrap();
    *even_modulus.last_mut().unwrap() &= !1;
    let mut failures = Vec::new();
    for (case, field, value) in [
        ("exponent_one", "e", json!("AQ")),
        (
            "exponent_above_33_bits",
            "e",
            json!(URL_SAFE_NO_PAD.encode([2, 0, 0, 0, 1])),
        ),
        (
            "even_modulus",
            "n",
            json!(URL_SAFE_NO_PAD.encode(even_modulus)),
        ),
    ] {
        let (fake, _server, cache, verifier) = fixture().await;
        let known = sso::credential(sso::Signer::shared().sign(&sso::claims(), "key1"));
        let replacement =
            sso::credential(sso::Signer::shared().sign(&sso::claims(), "replacement"));
        assert!(verifier.verify(&known, 1900000000).await.is_ok());
        let mut bad = key.clone();
        bad["kid"] = json!("replacement");
        bad[field] = value;
        fake.keys(vec![bad]).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::time::resume();
        let mut check = |condition: bool, observation: &'static str| {
            if !condition {
                failures.push((case, observation));
            }
        };
        let error = verifier
            .verify(&replacement, 1900000000)
            .await
            .err()
            .unwrap();
        check(
            m_gated::http::authentication_error(error).status() == StatusCode::SERVICE_UNAVAILABLE,
            "잘못된 집합의 첫 재조회는503",
        );
        check(
            matches!(
                verifier.verify(&replacement, 1900000000).await,
                Err(AuthError::InvalidCredentials)
            ),
            "cooldown의 unknown은401",
        );
        check(
            verifier.verify(&known, 1900000000).await.is_ok(),
            "실패 직후 기존 서명 유지",
        );
        check(fake.count() == 2, "재조회는 한 번");
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(289)).await;
        tokio::time::resume();
        check(cache.is_healthy().await, "원래TTL299초는 유효");
        check(
            verifier.verify(&known, 1900000000).await.is_ok(),
            "원래TTL299초의 기존 서명 유지",
        );
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::time::resume();
        check(!cache.is_healthy().await, "실패로TTL연장 없음");
        check(
            matches!(
                verifier.verify(&known, 1900000000).await,
                Err(AuthError::DependencyUnavailable)
            ),
            "원래TTL300초 정각은503",
        );
        check(fake.count() == 3, "만료 재조회 한 번");
        fake.keys(vec![key.clone()]).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::time::resume();
        check(
            verifier.verify(&known, 1900000000).await.is_ok(),
            "정상 집합 복구 뒤 실제 서명 성공",
        );
        check(fake.count() == 4, "복구 재조회 한 번");
    }
    assert!(failures.is_empty(), "RSA 집합 게시 경계: {failures:?}");
    let (fake, _server, cache, _verifier) = fixture().await;
    let mut now = tokio::time::Instant::now();
    assert!(cache.key_at("key1", now).await.is_ok());
    let key = sso::Signer::shared().jwk.clone();
    let mut bad_sets = vec![
        json!({"keys":[key.clone(),key.clone()]}),
        json!({"keys":vec![key.clone();9]}),
        json!({"keys":[]}),
    ];
    for (field, value) in [
        ("use", json!("enc")),
        ("use", json!(null)),
        ("alg", json!(null)),
        ("key_ops", json!(null)),
        ("alg", json!("RS512")),
        ("key_ops", json!(["sign"])),
        ("d", json!("private")),
        ("kty", json!("EC")),
        ("n", json!("AQAB")),
        ("e", json!("Ag")),
    ] {
        let mut bad = key.clone();
        bad["kid"] = json!("replacement");
        bad[field] = value;
        bad_sets.push(json!({"keys":[bad]}));
    }
    for bad in bad_sets {
        now += Duration::from_secs(10);
        fake.set(StatusCode::OK, serde_json::to_vec(&bad).unwrap())
            .await;
        assert!(matches!(
            cache.key_at("missing", now).await,
            Err(AuthError::DependencyUnavailable)
        ));
        assert!(matches!(
            cache.key_at("missing", now).await,
            Err(AuthError::InvalidCredentials)
        ));
        assert!(cache.key_at("key1", now).await.is_ok());
    }
    now += Duration::from_secs(300);
    fake.set(StatusCode::OK, vec![b' '; 65537]).await;
    assert!(matches!(
        cache.key_at("key1", now).await,
        Err(AuthError::DependencyUnavailable)
    ));
    now += Duration::from_secs(10);
    let redirect_target = sso::FakeJwks::new(vec![sso::Signer::shared().jwk.clone()]);
    let target_server = redirect_target.serve().await;
    *fake.location.write().await = target_server.uri.to_string();
    fake.set(StatusCode::TEMPORARY_REDIRECT, vec![]).await;
    let before = fake.count();
    assert!(cache.key_at("key1", now).await.is_err());
    assert_eq!(fake.count(), before + 1);
    assert_eq!(redirect_target.count(), 0);
}

#[tokio::test]
async fn jwks_timeout_and_single_flight_are_bounded() {
    let (fake, _server, cache, _) = fixture().await;
    fake.hold.store(true, std::sync::atomic::Ordering::SeqCst);
    let c = cache.clone();
    let job = tokio::spawn(async move { c.refresh_health().await });
    fake.entered.notified().await;
    let mut jobs = Vec::new();
    for _ in 0..16 {
        let c = cache.clone();
        jobs.push(tokio::spawn(async move { c.key("random").await.is_err() }));
    }
    fake.release.notify_one();
    assert!(job.await.unwrap());
    for job in jobs {
        assert!(job.await.unwrap());
    }
    assert_eq!(fake.count(), 1);
    // 실제 HTTP 무응답의 총 fetch 2초 기한을 관찰한다. 긴 sleep으로 경합을 맞추지 않는다.
    let fake = sso::FakeJwks::new(vec![]);
    fake.hold.store(true, std::sync::atomic::Ordering::SeqCst);
    let server = fake.serve().await;
    let cache = JwksCache::loopback_for_test(server.uri.clone()).unwrap();
    let start = std::time::Instant::now();
    assert!(!cache.refresh_health().await);
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_eq!(fake.count(), 1);
}

#[tokio::test]
async fn empty_rsa_modulus_is_rejected_without_panic() {
    let fake = sso::FakeJwks::new(vec![json!({"kty":"RSA","kid":"empty","n":"","e":"AQAB"})]);
    let server = fake.serve().await;
    let cache = JwksCache::loopback_for_test(server.uri.clone()).unwrap();
    assert!(matches!(
        cache.key("empty").await,
        Err(AuthError::DependencyUnavailable)
    ));
}

#[tokio::test]
async fn jwt_header_key_references_and_duplicate_claims_never_override_trust() {
    let (fake, _server, _cache, verifier) = fixture().await;
    let shadow = sso::Signer::new(2048);
    let malicious = sso::FakeJwks::new(vec![shadow.jwk.clone()]);
    let untrusted = malicious.serve().await;
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some("key1".into());
    header.jku = Some(untrusted.uri.to_string());
    header.x5u = Some(untrusted.uri.to_string());
    header.jwk = Some(serde_json::from_value(shadow.jwk.clone()).unwrap());
    assert!(
        verifier
            .verify(
                &sso::credential(sso::Signer::shared().sign_header(&sso::claims(), &header)),
                1900000000
            )
            .await
            .is_ok()
    );
    assert_eq!(fake.count(), 1);
    assert_eq!(malicious.count(), 0);
    for kid in [None, Some("".into()), Some("a".repeat(129))] {
        header.kid = kid;
        assert!(matches!(
            verifier
                .verify(
                    &sso::credential(sso::Signer::shared().sign_header(&sso::claims(), &header)),
                    1900000000
                )
                .await,
            Err(AuthError::InvalidCredentials)
        ));
    }
    let raw = serde_json::to_string(&sso::claims()).unwrap();
    let duplicate = raw.replacen('{', "{\"sub\":\"different\",", 1);
    assert!(matches!(
        verifier
            .verify(
                &sso::credential(sso::Signer::shared().sign_raw(&duplicate)),
                1900000000
            )
            .await,
        Err(AuthError::InvalidCredentials)
    ));
    assert_eq!(fake.count(), 1);
    assert_eq!(malicious.count(), 0);
}

#[tokio::test]
async fn rsa_size_bounds_are_enforced_with_real_signatures() {
    for (bits, accepted) in [(1024, false), (4096, true)] {
        let signer = sso::Signer::new(bits);
        let fake = sso::FakeJwks::new(vec![signer.jwk.clone()]);
        let server = fake.serve().await;
        let cache = Arc::new(JwksCache::loopback_for_test(server.uri.clone()).unwrap());
        let verifier = SsoVerifier::new(config().auth(), cache);
        assert_eq!(
            verifier
                .verify(
                    &sso::credential(if bits == 1024 {
                        sso::Signer::shared().sign(&sso::claims(), "key1")
                    } else {
                        signer.sign(&sso::claims(), "key1")
                    }),
                    1900000000
                )
                .await
                .is_ok(),
            accepted
        );
        assert_eq!(fake.count(), 1);
    }
    let mut too_big = sso::Signer::shared().jwk.clone();
    use base64::Engine;
    too_big["n"] = json!(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(vec![255u8; 513]));
    let fake = sso::FakeJwks::new(vec![too_big]);
    let server = fake.serve().await;
    assert!(
        JwksCache::loopback_for_test(server.uri.clone())
            .unwrap()
            .key("key1")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn chunked_jwks_body_obeys_the_same_64kib_limit() {
    use axum::{
        Router,
        body::{Body, Bytes},
        routing::get,
    };
    use std::{
        collections::VecDeque,
        convert::Infallible,
        pin::Pin,
        task::{Context, Poll},
    };
    struct Chunks(VecDeque<Bytes>);
    impl http_body::Body for Chunks {
        type Data = Bytes;
        type Error = Infallible;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<http_body::Frame<Bytes>, Infallible>>> {
            Poll::Ready(
                self.0
                    .pop_front()
                    .map(|bytes| Ok(http_body::Frame::data(bytes))),
            )
        }
    }
    let bytes = serde_json::to_vec(&json!({"keys":[sso::Signer::shared().jwk.clone()]})).unwrap();
    for length in [65536, 65537] {
        let mut padded = bytes.clone();
        padded.resize(length, b' ');
        let router = Router::new().route(
            "/jwks",
            get(move || {
                let bytes = padded.clone();
                async move {
                    Body::new(Chunks(
                        bytes.chunks(4096).map(Bytes::copy_from_slice).collect(),
                    ))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let cache = JwksCache::loopback_for_test(format!("http://{address}/jwks").parse().unwrap())
            .unwrap();
        assert_eq!(cache.key("key1").await.is_ok(), length == 65536);
        task.abort();
        let _ = task.await;
    }
}

#[tokio::test]
async fn jwt_expiry_is_rechecked_after_jwks_wait_not_at_request_start() {
    let (fake, _server, _cache, verifier) = fixture().await;
    fake.hold.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut claims = sso::claims();
    claims["exp"] = json!(1899999971);
    let credential = sso::credential(sso::Signer::shared().sign(&claims, "key1"));
    let job = tokio::spawn(async move { verifier.verify(&credential, 1900000000).await });
    fake.entered.notified().await;
    // 연결/요청 도착 뒤에만 시각을 제어해 실제 socket의 idle 자동진행을 피한다.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::time::resume();
    fake.release.notify_one();
    assert!(matches!(
        job.await.unwrap(),
        Err(AuthError::InvalidCredentials)
    ));
    assert_eq!(fake.count(), 1);
}

#[tokio::test]
async fn unknown_kid_fetch_failure_is_503_then_cooldown_401_without_stale_extension() {
    let (fake, _server, _cache, verifier) = fixture().await;
    let signer = sso::Signer::shared();
    let known = sso::credential(signer.sign(&sso::claims(), "key1"));
    let new = sso::credential(signer.sign(&sso::claims(), "rotated"));
    assert!(verifier.verify(&known, 1900000000).await.is_ok());
    fake.set(StatusCode::SERVICE_UNAVAILABLE, vec![]).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(10)).await;
    tokio::time::resume();
    assert!(matches!(
        verifier.verify(&new, 1900000000).await,
        Err(AuthError::DependencyUnavailable)
    ));
    assert_eq!(fake.count(), 2);
    assert!(matches!(
        verifier.verify(&new, 1900000000).await,
        Err(AuthError::InvalidCredentials)
    ));
    assert_eq!(fake.count(), 2);
    assert!(verifier.verify(&known, 1900000000).await.is_ok());
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(289)).await;
    tokio::time::resume();
    assert!(verifier.verify(&known, 1900000000).await.is_ok());
    assert_eq!(fake.count(), 2);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::time::resume();
    assert!(matches!(
        verifier.verify(&known, 1900000000).await,
        Err(AuthError::DependencyUnavailable)
    ));
    assert_eq!(fake.count(), 3);
    let mut rotated = signer.jwk.clone();
    rotated["kid"] = json!("rotated");
    fake.keys(vec![rotated]).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(10)).await;
    tokio::time::resume();
    assert!(verifier.verify(&new, 1900000000).await.is_ok());
    assert_eq!(fake.count(), 4);
    assert!(matches!(
        verifier.verify(&known, 1900000000).await,
        Err(AuthError::InvalidCredentials)
    ));
    assert_eq!(fake.count(), 4);
}
