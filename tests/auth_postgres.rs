#[path = "support/postgres.rs"]
mod postgres;
#[path = "support/sso.rs"]
mod sso;
mod support;
use axum::{
    Router,
    extract::{Request, State},
    http::{Method, StatusCode},
    response::IntoResponse,
    routing::any,
};
use m_gated::{
    auth::{AccessSettings, AuthError, SsoAuthenticator, SsoVerifier, jwks::JwksCache},
    config::{self, ConfigInputs},
    http::{self, AppState},
    runtime::{Dependency, Runtime},
};
use serde_json::json;
use sqlx::Row;
use std::sync::Arc;
fn config(admin: bool) -> config::ValidatedConfig {
    let mut env = support::environment();
    env.insert("ALLOWED_EMAILS".into(), "alice@example.test".into());
    env.insert(
        "ADMIN_EMAILS".into(),
        if admin {
            "alice@example.test"
        } else {
            "other@example.test"
        }
        .into(),
    );
    config::load(ConfigInputs {
        yaml: support::YAML,
        file_environment: None,
        environment: &env,
    })
    .unwrap()
}
#[test]
fn explicit_test_database_guard_has_no_product_fallback() {
    for bad in [
        "postgres://m_gated_test@db.internal:64603/m_gated_test",
        "postgres://m_gated_test@127.0.0.1:64603/company",
        "postgres://m_gated_test@127.0.0.1:5432/m_gated_test",
        "postgres://root@127.0.0.1:64603/m_gated_test",
        "postgres://m_gated_test@127.0.0.1:64603/m_gated_test?sslmode=require",
    ] {
        assert!(postgres::test_options(bad).is_err());
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn identity_and_admin_are_not_email_inheritance() {
    let db = postgres::TestDb::new().await;
    let fake = sso::FakeJwks::new(vec![sso::Signer::shared().jwk.clone()]);
    let server = fake.serve().await;
    let verifier = SsoVerifier::new(
        config(true).auth(),
        Arc::new(JwksCache::loopback_for_test(server.uri.clone()).unwrap()),
    );
    let settings = AccessSettings::from_config(&config(true));
    let token = sso::credential(sso::Signer::shared().sign(&sso::claims(), "key1"));
    let raw = serde_json::to_string(&sso::claims())
        .unwrap()
        .replacen('{', "{\"sub\":123,", 1);
    assert!(
        verifier
            .verify(
                &sso::credential(sso::Signer::shared().sign_raw(&raw)),
                1900000000
            )
            .await
            .is_err()
    );
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_principals")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    assert_eq!(before, 0);
    let identity = verifier.verify(&token, 1900000000).await.unwrap();
    let first = db.store.current_human(&identity, &settings).await.unwrap();
    assert!(first.admin);
    assert_eq!(first.version, 1);
    let again = db.store.current_human(&identity, &settings).await.unwrap();
    assert_eq!(again.id, first.id);
    assert_eq!(again.version, 1);
    assert_eq!(again.policy_id, first.policy_id);
    let grants: serde_json::Value =
        sqlx::query_scalar("SELECT allow_rules FROM gw_policies WHERE id=$1")
            .bind(first.policy_id)
            .fetch_one(db.store.pool())
            .await
            .unwrap();
    assert_eq!(grants, json!([]));
    let mut claims = sso::claims();
    claims["sub"] = json!("new-sub-same-email");
    let other = verifier
        .verify(
            &sso::credential(sso::Signer::shared().sign(&claims, "key1")),
            1900000000,
        )
        .await
        .unwrap();
    let new = db.store.current_human(&other, &settings).await.unwrap();
    assert_ne!(new.id, first.id);
    assert_ne!(new.policy_id, first.policy_id);
    assert!(new.admin);
    assert!(
        !db.store
            .current_human(&identity, &AccessSettings::from_config(&config(false)))
            .await
            .unwrap()
            .admin
    );
    let gate = Arc::new(tokio::sync::Barrier::new(3));
    let mut jobs = Vec::new();
    for _ in 0..2 {
        let store = db.store.clone();
        let gate = gate.clone();
        let config = config(true);
        let verifier = SsoVerifier::new(config.auth(), verifier.jwks.clone());
        let token = sso::credential(sso::Signer::shared().sign(&claims, "key1"));
        jobs.push(tokio::spawn(async move {
            let identity = verifier.verify(&token, 1900000000).await.unwrap();
            gate.wait().await;
            store
                .current_human(&identity, &AccessSettings::from_config(&config))
                .await
                .unwrap()
                .id
        }));
    }
    gate.wait().await;
    for job in jobs {
        assert_eq!(job.await.unwrap(), new.id);
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_principals")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    assert_eq!(rows, 2);
    claims["sub"] = json!("human-1");
    claims["email"] = json!("ALICE@example.test");
    claims["name"] = json!("Updated Alice");
    let changed = verifier
        .verify(
            &sso::credential(sso::Signer::shared().sign(&claims, "key1")),
            1900000000,
        )
        .await
        .unwrap();
    let changed = db.store.current_human(&changed, &settings).await.unwrap();
    assert_eq!(changed.id, first.id);
    assert_eq!(changed.version, 2);
    assert_eq!(changed.policy_version, 1);
    sqlx::query("UPDATE gw_principals SET disabled_at=clock_timestamp(),disabled_reason='security_config_fence',security_fence_ref='synthetic-ref' WHERE id=$1").bind(first.id).execute(db.store.pool()).await.unwrap();
    assert!(matches!(
        db.store.current_human(&identity, &settings).await,
        Err(AuthError::AccessDenied)
    ));
    let row = sqlx::query(
        "SELECT disabled_reason,security_fence_ref,version FROM gw_principals WHERE id=$1",
    )
    .bind(first.id)
    .fetch_one(db.store.pool())
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("security_fence_ref"), "synthetic-ref");
    assert_eq!(row.get::<i64, _>("version"), 2);
    db.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn same_authentication_service_observes_wire_and_current_access() {
    let db = postgres::TestDb::new().await;
    let cfg = config(true);
    let fake = sso::FakeJwks::new(vec![sso::Signer::shared().jwk.clone()]);
    let jwks_server = fake.serve().await;
    let cache = Arc::new(JwksCache::loopback_for_test(jwks_server.uri.clone()).unwrap());
    let verifier = SsoVerifier::new(cfg.auth(), cache.clone());
    let auth = Arc::new(SsoAuthenticator::new(&cfg, verifier, db.store.clone()));
    let runtime = Runtime::new(cfg.limits().clone()).unwrap();
    runtime.start_http().unwrap();
    for dependency in [Dependency::Database, Dependency::Schema, Dependency::Ui] {
        runtime.record_health(dependency, true, tokio::time::Instant::now());
    }
    assert_eq!(runtime.lifecycle(), m_gated::runtime::Lifecycle::NotReady);
    fake.set(StatusCode::SERVICE_UNAVAILABLE, vec![]).await;
    assert!(!auth.refresh_health(&runtime).await);
    assert_eq!(runtime.lifecycle(), m_gated::runtime::Lifecycle::NotReady);
    struct AuthHarness {
        auth: Arc<SsoAuthenticator>,
        client: reqwest::Client,
        dispatch_sink: String,
    }
    async fn boundary(
        State(harness): State<Arc<AuthHarness>>,
        req: Request,
    ) -> axum::response::Response {
        if req.method() != Method::GET {
            return http::authentication_error(AuthError::AuthSourceForbidden);
        }
        let route = match req.uri().path() {
            "/test/inference" => m_gated::auth::AuthRoute::Inference,
            "/test/cookie" => m_gated::auth::AuthRoute::CookieOnly,
            _ => m_gated::auth::AuthRoute::Management,
        };
        match harness
            .auth
            .authenticate(req.headers(), route, 1900000000)
            .await
        {
            Ok(h) => {
                if req.uri().path() == "/test/admin" && !h.current.admin {
                    return http::authentication_error(AuthError::AccessDenied);
                }
                if route == m_gated::auth::AuthRoute::Inference {
                    assert_eq!(
                        harness
                            .client
                            .post(&harness.dispatch_sink)
                            .send()
                            .await
                            .unwrap()
                            .status(),
                        StatusCode::NO_CONTENT
                    );
                }
                StatusCode::NO_CONTENT.into_response()
            }
            Err(e) => http::authentication_error(e),
        }
    }
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dispatched = Arc::new(AtomicUsize::new(0));
    async fn sink(State(calls): State<Arc<AtomicUsize>>, req: Request) -> StatusCode {
        assert!(!req.headers().contains_key("authorization"));
        assert!(!req.headers().contains_key("cookie"));
        calls.fetch_add(1, Ordering::SeqCst);
        StatusCode::NO_CONTENT
    }
    let sink_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sink_address = sink_listener.local_addr().unwrap();
    let sink_router = Router::new()
        .route("/sink", axum::routing::post(sink))
        .with_state(dispatched.clone());
    let sink_task = tokio::spawn(async move {
        axum::serve(sink_listener, sink_router).await.unwrap();
    });
    let harness = Arc::new(AuthHarness {
        auth: auth.clone(),
        client: reqwest::Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .build()
            .unwrap(),
        dispatch_sink: format!("http://{sink_address}/sink"),
    });
    let router = Router::new()
        .route("/test/me", any(boundary))
        .route("/test/admin", any(boundary))
        .route("/test/inference", any(boundary))
        .route("/test/cookie", any(boundary))
        .with_state(harness);
    let app = http::router(AppState {
        runtime: runtime.clone(),
    })
    .merge(router);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(m_gated::server::serve(
        listener,
        app,
        runtime.clone(),
        std::future::pending(),
    ));
    let client = reqwest::Client::builder()
        .no_proxy()
        .retry(reqwest::retry::never())
        .build()
        .unwrap();
    let token = sso::Signer::shared().sign(&sso::claims(), "key1");
    let cookie = format!("mgated_access={token}");
    let unavailable = client
        .get(format!("http://{address}/test/me"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        unavailable.headers()["x-gateway-error-code"],
        "auth_dependency_unavailable"
    );
    assert_eq!(unavailable.headers()["retry-after"], "1");
    assert_eq!(
        client
            .get(format!("http://{address}/livez"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        client
            .get(format!("http://{address}/readyz"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_principals")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    assert_eq!(before, 0);
    fake.keys(vec![sso::Signer::shared().jwk.clone()]).await;
    cache
        .key_at(
            "key1",
            tokio::time::Instant::now() + std::time::Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(auth.refresh_health(&runtime).await);
    assert_eq!(
        client
            .get(format!("http://{address}/readyz"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    fake.set(StatusCode::SERVICE_UNAVAILABLE, vec![]).await;
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(20)).await;
    tokio::time::resume();
    let unknown = sso::Signer::shared().sign(&sso::claims(), "unknown-wire-kid");
    let first_unknown = client
        .get(format!("http://{address}/test/inference"))
        .bearer_auth(&unknown)
        .send()
        .await
        .unwrap();
    assert_eq!(first_unknown.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        first_unknown.headers()["x-gateway-error-code"],
        "auth_dependency_unavailable"
    );
    let error_body = first_unknown.text().await.unwrap();
    assert!(!error_body.contains("unknown-wire-kid"));
    assert!(!error_body.contains(&unknown));
    let cooldown = client
        .get(format!("http://{address}/test/inference"))
        .bearer_auth(&unknown)
        .send()
        .await
        .unwrap();
    assert_eq!(cooldown.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(dispatched.load(Ordering::SeqCst), 0);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_principals")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    assert_eq!(rows, 0);
    fake.keys(vec![sso::Signer::shared().jwk.clone()]).await;
    for bad in ["", "Basic x", "Bearer a,b", "Bearer bad.jwt"] {
        let r = client
            .get(format!("http://{address}/test/me"))
            .header("authorization", bad)
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(r.headers()["x-gateway-dispatch-state"], "not_dispatched");
        assert_eq!(r.headers()["x-gateway-error-code"], "invalid_credentials");
        assert_eq!(r.headers()["cache-control"], "no-store");
    }
    let conflict = client
        .get(format!("http://{address}/test/me"))
        .bearer_auth(&token)
        .header("x-api-key", "different")
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status(), StatusCode::UNAUTHORIZED);
    let repeated_cookie = client
        .get(format!("http://{address}/test/me"))
        .header("cookie", format!("{cookie}; {cookie}"))
        .send()
        .await
        .unwrap();
    assert_eq!(repeated_cookie.status(), StatusCode::UNAUTHORIZED);
    let mut denied_claims = sso::claims();
    denied_claims["email"] = json!("denied@example.test");
    let denied = client
        .get(format!("http://{address}/test/me"))
        .bearer_auth(sso::Signer::shared().sign(&denied_claims, "key1"))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    for (path, with_header, expected) in [
        ("/test/me", false, 204),
        ("/test/inference", false, 401),
        ("/test/cookie", true, 403),
        ("/test/admin", true, 204),
    ] {
        let mut req = client
            .get(format!("http://{address}{path}"))
            .header("cookie", &cookie);
        if with_header {
            req = req.bearer_auth(&token);
        }
        assert_eq!(req.send().await.unwrap().status().as_u16(), expected);
    }
    let both = client
        .get(format!("http://{address}/test/me"))
        .bearer_auth(&token)
        .header("x-api-key", &token)
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(both.status(), StatusCode::NO_CONTENT);
    let duplicate = client
        .get(format!("http://{address}/test/me"))
        .header("authorization", format!("Bearer {token}"))
        .header("authorization", format!("Bearer {token}"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(fake.count(), 3);
    fake.set(StatusCode::SERVICE_UNAVAILABLE, vec![]).await;
    assert!(auth.refresh_health(&runtime).await);
    assert_eq!(fake.count(), 3);
    fake.keys(vec![sso::Signer::shared().jwk.clone()]).await;
    assert_eq!(dispatched.load(Ordering::SeqCst), 0);
    assert_eq!(
        client
            .get(format!("http://{address}/test/inference"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(dispatched.load(Ordering::SeqCst), 1);
    sqlx::query("UPDATE gw_principals SET disabled_at=clock_timestamp(),disabled_reason='manual' WHERE subject='human-1'").execute(db.store.pool()).await.unwrap();
    assert_eq!(
        client
            .get(format!("http://{address}/test/admin"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        client
            .get(format!("http://{address}/test/inference"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(dispatched.load(Ordering::SeqCst), 1);
    sink_task.abort();
    let _ = sink_task.await;
    task.abort();
    let _ = task.await;
    drop(auth);
    db.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn principal_policy_constraints_and_migration_are_real_sql() {
    let db = postgres::TestDb::new().await;
    db.store.migrate().await.unwrap();
    let id = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO gw_principals(id,kind,issuer,subject,email,display_name) VALUES($1,'human','https://issuer.example','subject','alice@example.test','Alice')").bind(id).execute(db.store.pool()).await.unwrap();
    for statement in [
        "UPDATE gw_principals SET subject='other' WHERE id=$1",
        "UPDATE gw_principals SET issuer='https://other.example' WHERE id=$1",
        "UPDATE gw_principals SET email='Alice@example.test' WHERE id=$1",
        "UPDATE gw_principals SET kind='service',issuer=NULL,subject=NULL,email=NULL,owner_principal_id=id WHERE id=$1",
        "UPDATE gw_principals SET disabled_at=clock_timestamp(),disabled_reason=NULL WHERE id=$1",
        "UPDATE gw_principals SET security_fence_ref='fence' WHERE id=$1",
        "UPDATE gw_principals SET inference_paused_at=clock_timestamp() WHERE id=$1",
        "UPDATE gw_principals SET inference_safety_version=0 WHERE id=$1",
        "UPDATE gw_principals SET display_name=repeat('x',101) WHERE id=$1",
    ] {
        assert!(
            sqlx::query(statement)
                .bind(id)
                .execute(db.store.pool())
                .await
                .is_err(),
            "SQL CHECK/trigger must reject invalid principal state"
        );
    }
    let service = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO gw_principals(id,kind,display_name,owner_principal_id) VALUES($1,'service','service',$2)").bind(service).bind(id).execute(db.store.pool()).await.unwrap();
    assert!(sqlx::query("INSERT INTO gw_principals(id,kind,display_name,owner_principal_id) VALUES($1,'service','invalid owner',$2)").bind(uuid::Uuid::new_v4()).bind(service).execute(db.store.pool()).await.is_err());
    let scope = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO gw_policies(id,principal_id,allow_rules,budget_mode,period_kind,period_timezone,updated_by) VALUES($1,$2,'[]','actual','calendar_month','UTC',$2)").bind(scope).bind(id).execute(db.store.pool()).await.unwrap();
    for assignment in [
        "period_timezone='No/SuchZone'",
        "allow_rules='{}'",
        "rpm_limit=0",
        "budget_limit_usd='NaN'",
        "budget_limit_usd='Infinity'",
        "budget_limit_usd=-1",
        "version=0",
        "principal_id=(SELECT id FROM gw_principals WHERE kind='service' LIMIT 1)",
    ] {
        assert!(
            sqlx::query(&format!("UPDATE gw_policies SET {assignment} WHERE id=$1"))
                .bind(scope)
                .execute(db.store.pool())
                .await
                .is_err(),
            "SQL policy CHECK/trigger must reject invalid state"
        );
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_schema_migrations")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    assert_eq!(count, 7);
    sqlx::query("UPDATE gw_schema_migrations SET checksum=decode(repeat('00',32),'hex')")
        .execute(db.store.pool())
        .await
        .unwrap();
    assert!(matches!(
        db.store.migrate().await,
        Err(AuthError::DatabaseUnavailable)
    ));
    db.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn authentication_waiters_are_bounded_and_release_their_slots() {
    use m_gated::auth::{AuthRoute, AuthenticatedHuman};
    use std::{future::Future, pin::Pin, task::Poll};
    let db = postgres::TestDb::new().await;
    let cfg = config(true);
    let fake = sso::FakeJwks::new(vec![sso::Signer::shared().jwk.clone()]);
    fake.hold.store(true, std::sync::atomic::Ordering::SeqCst);
    let server = fake.serve().await;
    let verifier = SsoVerifier::new(
        cfg.auth(),
        Arc::new(JwksCache::loopback_for_test(server.uri.clone()).unwrap()),
    );
    let auth = Arc::new(SsoAuthenticator::new(&cfg, verifier, db.store.clone()));
    let token = sso::Signer::shared().sign(&sso::claims(), "key1");
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
    type Authentication =
        Pin<Box<dyn Future<Output = Result<AuthenticatedHuman, AuthError>> + Send>>;
    let mut pending: Vec<Authentication> = Vec::new();
    for _ in 0..4 {
        let auth = auth.clone();
        let headers = headers.clone();
        pending.push(Box::pin(async move {
            auth.authenticate(&headers, AuthRoute::Management, 1900000000)
                .await
        }));
    }
    std::future::poll_fn(|cx| {
        for future in &mut pending {
            assert!(future.as_mut().poll(cx).is_pending());
        }
        Poll::Ready(())
    })
    .await;
    assert!(matches!(
        auth.authenticate(&headers, AuthRoute::Management, 1900000000)
            .await,
        Err(AuthError::DependencyUnavailable)
    ));
    let jobs: Vec<_> = pending.into_iter().map(tokio::spawn).collect();
    fake.entered.notified().await;
    assert_eq!(fake.count(), 1);
    fake.release.notify_one();
    for job in jobs {
        assert!(job.await.unwrap().is_ok());
    }
    assert!(
        auth.authenticate(&headers, AuthRoute::Management, 1900000000)
            .await
            .is_ok()
    );
    assert_eq!(fake.count(), 1);
    drop(auth);
    db.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn configured_token_limit_rejects_before_jwks_or_identity_persistence() {
    let db = postgres::TestDb::new().await;
    let mut env = support::environment();
    env.insert("ALLOWED_EMAILS".into(), "alice@example.test".into());
    let yaml = format!("{}\nlimits:\n  token_bytes: 16\n", support::YAML);
    let cfg = config::load(ConfigInputs {
        yaml: &yaml,
        file_environment: None,
        environment: &env,
    })
    .unwrap();
    let fake = sso::FakeJwks::new(vec![sso::Signer::shared().jwk.clone()]);
    let server = fake.serve().await;
    let verifier = SsoVerifier::new(
        cfg.auth(),
        Arc::new(JwksCache::loopback_for_test(server.uri.clone()).unwrap()),
    );
    let auth = SsoAuthenticator::new(&cfg, verifier, db.store.clone());
    let token = sso::Signer::shared().sign(&sso::claims(), "key1");
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
    let rejected = matches!(
        auth.authenticate(&headers, m_gated::auth::AuthRoute::Management, 1900000000)
            .await,
        Err(AuthError::InvalidCredentials)
    );
    let calls = fake.count();
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_principals")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    drop(auth);
    db.close().await;
    assert!(
        rejected,
        "validated lower token_bytes must apply before verification"
    );
    assert_eq!(calls, 0);
    assert_eq!(rows, 0);
}
