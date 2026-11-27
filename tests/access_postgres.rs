//! 접근 목록(결정 0001)의 관리 API와 규칙. 실제 라우터에 HTTP 요청을 보내 확인한다.
#[path = "support/openapi.rs"]
#[allow(dead_code)]
mod openapi;
#[path = "support/postgres.rs"]
mod postgres;
#[path = "support/sso.rs"]
#[allow(dead_code)]
mod sso;
mod support;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use m_gated::{
    auth::{Authenticator, SsoVerifier, jwks::JwksCache},
    config::{self, ConfigInputs},
    http::{self, AppState},
    runtime::{Dependency, Runtime},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

struct Harness {
    db: postgres::TestDb,
    router: axum::Router,
    _runtime: Arc<Runtime>,
    _jwks: sso::Server,
}
impl Harness {
    /// alice가 설정의 고정 관리자다. 다른 사람은 목록에 없다.
    async fn new() -> Self {
        let mut env = support::environment();
        env.insert("ADMIN_EMAIL".into(), "Alice@Example.test".into());
        let cfg = config::load(ConfigInputs {
            yaml: support::YAML,
            file_environment: None,
            environment: &env,
        })
        .unwrap();
        let db = postgres::TestDb::new().await;
        postgres::grant_access(&db.store, &cfg, &[]).await;
        let jwks = sso::FakeJwks::new(vec![sso::Signer::shared().jwk.clone()])
            .serve()
            .await;
        let verifier = SsoVerifier::new(
            cfg.sso().unwrap(),
            Arc::new(JwksCache::loopback_for_test(jwks.uri.clone()).unwrap()),
        );
        let auth = Arc::new(Authenticator::new(&cfg, verifier, db.store.clone()));
        let api = Arc::new(m_gated::admin::DashboardApi::new(
            &cfg,
            auth,
            db.store.clone(),
        ));
        let runtime = Runtime::new(cfg.limits().clone()).unwrap();
        runtime.start_http().unwrap();
        for d in [
            Dependency::Database,
            Dependency::Jwks,
            Dependency::Schema,
            Dependency::Ui,
        ] {
            runtime.record_health(d, true, tokio::time::Instant::now());
        }
        let router = http::router_with_components(
            AppState {
                runtime: runtime.clone(),
            },
            None,
            api.routes(runtime.clone()),
        );
        Self {
            db,
            router,
            _runtime: runtime,
            _jwks: jwks,
        }
    }
    /// 이 사람의 SSO 토큰. alice는 sso::claims()의 기본 사람이다.
    fn token(name: &str) -> String {
        let mut claims = sso::claims();
        if name != "alice" {
            claims["sub"] = json!(format!("human-{name}"));
            claims["email"] = json!(format!("{name}@example.test"));
        }
        sso::Signer::shared().sign(&claims, "key1")
    }
    async fn send(
        &self,
        who: &str,
        method: &str,
        path: &str,
        command: Option<Uuid>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut r = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {}", Self::token(who)));
        if let Some(c) = command {
            r = r.header("x-gateway-command-id", c.to_string());
        }
        let request = match body {
            Some(b) => r
                .header("content-type", "application/json")
                .body(Body::from(b.to_string())),
            None => r.body(Body::empty()),
        }
        .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        // 관리 API의 응답은 OpenAPI 문서(결정 0014 규칙 4)에 맞아야 한다.
        if path.starts_with("/api/") {
            let parsed: Option<serde_json::Value> = serde_json::from_slice(&bytes).ok();
            if let Err(e) =
                openapi::Document::shared().check_path(method, path, status, parsed.as_ref())
            {
                panic!("OpenAPI 문서와 다른 응답: {e}\n{parsed:?}");
            }
        }
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    async fn entry(&self, email: &str) -> Value {
        let (status, list) = self
            .send("alice", "GET", "/api/admin/access", None, None)
            .await;
        assert_eq!(status, 200, "{list}");
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["email"] == email)
            .cloned()
            .unwrap_or(Value::Null)
    }
    async fn add(&self, email: &str, role: &str) -> (u16, Value) {
        self.send(
            "alice",
            "POST",
            "/api/admin/access",
            Some(Uuid::new_v4()),
            Some(json!({"email": email, "role": role})),
        )
        .await
    }
}

impl Harness {
    /// 시험용 schema를 지운다.
    async fn finish(self) {
        self.db.close().await;
    }
}

fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn fixed_admin_is_on_the_list_and_cannot_be_changed() {
    let h = Harness::new().await;
    // 설정의 이메일은 소문자로 바뀌어 들어간다.
    let alice = h.entry("alice@example.test").await;
    assert_eq!(alice["role"], "admin");
    assert_eq!(alice["fixed"], true);
    assert_eq!(alice["added_by_email"], Value::Null);
    let id = alice["id"].as_str().unwrap();
    let version = alice["version"].as_str().unwrap();
    let (status, body) = h
        .send(
            "alice",
            "PATCH",
            &format!("/api/admin/access/{id}"),
            Some(Uuid::new_v4()),
            Some(json!({"role": "user", "expected_version": version})),
        )
        .await;
    assert_eq!((status, code(&body)), (409, "access_protected"));
    let (status, body) = h
        .send(
            "alice",
            "DELETE",
            &format!("/api/admin/access/{id}?expected_version={version}"),
            Some(Uuid::new_v4()),
            None,
        )
        .await;
    assert_eq!((status, code(&body)), (409, "access_protected"));
    // 다시 시작해도 한 행이고 역할은 관리자다.
    h.db.store
        .ensure_fixed_admin(&m_gated::auth::AccessSettings::from_config(
            &config::load(ConfigInputs {
                yaml: support::YAML,
                file_environment: None,
                environment: &{
                    let mut env = support::environment();
                    env.insert("ADMIN_EMAIL".into(), "alice@example.test".into());
                    env
                },
            })
            .unwrap(),
        ))
        .await
        .unwrap();
    let (_, list) = h
        .send("alice", "GET", "/api/admin/access", None, None)
        .await;
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    h.finish().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn only_listed_people_sign_in_and_roles_apply_on_the_next_request() {
    let h = Harness::new().await;
    // 목록에 없으면 로그인할 수 없다.
    let (status, body) = h.send("bob", "GET", "/api/me", None, None).await;
    assert_eq!((status, code(&body)), (403, "access_denied"));

    let (status, ack) = h.add(" Bob@Example.test ", "user").await;
    assert_eq!(status, 201, "{ack}");
    assert_eq!(ack["operation"], "add_access");
    assert_eq!(ack["access"]["email"], "bob@example.test");
    assert_eq!(ack["access"]["added_by_email"], "alice@example.test");
    let (status, body) = h.add("bob@example.test", "admin").await;
    assert_eq!((status, code(&body)), (409, "access_exists"));

    let (status, me) = h.send("bob", "GET", "/api/me", None, None).await;
    assert_eq!(status, 200, "{me}");
    assert_eq!(me["role"], "user");
    let (status, _) = h.send("bob", "GET", "/api/admin/access", None, None).await;
    assert_eq!(status, 403);
    // 로그인하면 목록에 계정이 보인다.
    let bob = h.entry("bob@example.test").await;
    assert_eq!(bob["principal"]["disabled"], false);

    // 관리자로 바꾸면 다음 요청부터 관리자다. 예전 버전으로 보내면 거절한다.
    let id = bob["id"].as_str().unwrap().to_owned();
    let (status, ack) = h
        .send(
            "alice",
            "PATCH",
            &format!("/api/admin/access/{id}"),
            Some(Uuid::new_v4()),
            Some(json!({"role": "admin", "expected_version": "1"})),
        )
        .await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(ack["result_version"], "2");
    let (status, body) = h
        .send(
            "alice",
            "PATCH",
            &format!("/api/admin/access/{id}"),
            Some(Uuid::new_v4()),
            Some(json!({"role": "user", "expected_version": "1"})),
        )
        .await;
    assert_eq!((status, code(&body)), (409, "version_conflict"));
    let (status, me) = h.send("bob", "GET", "/api/me", None, None).await;
    assert_eq!((status, me["role"].as_str()), (200, Some("admin")));

    // 관리자도 자기 항목은 바꿀 수 없다.
    let (status, body) = h
        .send(
            "bob",
            "DELETE",
            &format!("/api/admin/access/{id}?expected_version=2"),
            Some(Uuid::new_v4()),
            None,
        )
        .await;
    assert_eq!((status, code(&body)), (409, "access_protected"));

    // 지우면 다음 요청부터 로그인할 수 없다. 계정은 남는다.
    let (status, ack) = h
        .send(
            "alice",
            "DELETE",
            &format!("/api/admin/access/{id}?expected_version=2"),
            Some(Uuid::new_v4()),
            None,
        )
        .await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(ack["operation"], "remove_access");
    let (status, body) = h.send("bob", "GET", "/api/me", None, None).await;
    assert_eq!((status, code(&body)), (403, "access_denied"));
    assert_eq!(h.entry("bob@example.test").await, Value::Null);
    let principals: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gw_principals WHERE email='bob@example.test'")
            .fetch_one(h.db.store.pool())
            .await
            .unwrap();
    assert_eq!(principals, 1);
    h.finish().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn commands_replay_and_leave_command_and_audit_records() {
    let h = Harness::new().await;
    let command = Uuid::new_v4();
    let body = json!({"email": "carol@example.test", "role": "user"});
    let first = h
        .send(
            "alice",
            "POST",
            "/api/admin/access",
            Some(command),
            Some(body.clone()),
        )
        .await;
    assert_eq!(first.0, 201, "{}", first.1);
    // 같은 명령 ID와 같은 입력은 같은 결과를 돌려준다. 행은 하나다.
    let again = h
        .send(
            "alice",
            "POST",
            "/api/admin/access",
            Some(command),
            Some(body),
        )
        .await;
    assert_eq!(again.0, 201);
    assert_eq!(again.1["target"], first.1["target"]);
    // 같은 명령 ID에 다른 입력은 충돌이다.
    let other = h
        .send(
            "alice",
            "POST",
            "/api/admin/access",
            Some(command),
            Some(json!({"email": "dave@example.test", "role": "user"})),
        )
        .await;
    assert_eq!((other.0, code(&other.1)), (409, "command_conflict"));
    let (status, list) = h
        .send("alice", "GET", "/api/admin/access", None, None)
        .await;
    assert_eq!(status, 200);
    assert_eq!(list["items"].as_array().unwrap().len(), 2);
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gw_audit_events WHERE operation='add_access' AND target_kind='access'",
    )
    .fetch_one(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(audits, 1);
    let commands: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gw_commands WHERE operation='add_access' AND target_kind='access'",
    )
    .fetch_one(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(commands, 1);
    h.finish().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn bad_input_is_rejected() {
    let h = Harness::new().await;
    for body in [
        json!({"email": "not-an-email", "role": "user"}),
        json!({"email": "e@example.test", "role": "owner"}),
        json!({"email": "e@example.test", "role": "user", "extra": 1}),
    ] {
        let (status, answer) = h
            .send(
                "alice",
                "POST",
                "/api/admin/access",
                Some(Uuid::new_v4()),
                Some(body),
            )
            .await;
        assert_eq!((status, code(&answer)), (400, "invalid_request"));
    }
    let (status, answer) = h
        .send(
            "alice",
            "DELETE",
            &format!("/api/admin/access/{}", Uuid::new_v4()),
            Some(Uuid::new_v4()),
            None,
        )
        .await;
    assert_eq!((status, code(&answer)), (400, "invalid_request"));
    let (status, answer) = h
        .send(
            "alice",
            "DELETE",
            &format!("/api/admin/access/{}?expected_version=1", Uuid::new_v4()),
            Some(Uuid::new_v4()),
            None,
        )
        .await;
    assert_eq!((status, code(&answer)), (404, "resource_not_found"));
    h.finish().await;
}
