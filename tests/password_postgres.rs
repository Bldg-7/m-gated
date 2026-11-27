//! 이메일/비밀번호 로그인(결정 0011). 실제 라우터에 HTTP 요청을 보내 확인한다. 메일은 메모리에 모은다.
#[path = "support/openapi.rs"]
#[allow(dead_code)]
mod openapi;
#[path = "support/postgres.rs"]
mod postgres;
mod support;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use m_gated::{
    auth::{
        Authenticator,
        password::{LinkKind, MemoryMailer, link_token, token_hash},
    },
    config::{self, ConfigInputs},
    http::{self, AppState},
    runtime::{Dependency, Runtime},
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::Ordering};
use tower::ServiceExt;
use uuid::Uuid;

const ORIGIN: &str = "https://m-gated.example.test";

struct Harness {
    db: postgres::TestDb,
    router: axum::Router,
    mailer: Arc<MemoryMailer>,
    _runtime: Arc<Runtime>,
}
/// 응답 하나. 상태, JSON 본문, `Set-Cookie`의 세션 토큰.
struct Answer {
    status: u16,
    body: Value,
    cookie: Option<String>,
    retry_after: Option<String>,
}
fn code(a: &Answer) -> &str {
    a.body["error"]["code"].as_str().unwrap_or("")
}

impl Harness {
    /// alice가 설정의 고정 관리자다. 아직 비밀번호가 없다.
    async fn new() -> Self {
        let mut env = support::environment();
        env.insert("ADMIN_EMAIL".into(), "alice@example.test".into());
        env.insert("SMTP_PASSWORD".into(), "synthetic-smtp-secret".into());
        let yaml = support::password_yaml();
        let cfg = config::load(ConfigInputs {
            yaml: &yaml,
            file_environment: None,
            environment: &env,
        })
        .unwrap();
        assert_eq!(cfg.auth_mode(), config::AuthMode::Password);
        let db = postgres::TestDb::new().await;
        postgres::grant_access(&db.store, &cfg, &[]).await;
        let mailer = Arc::new(MemoryMailer::default());
        let auth = Arc::new(Authenticator::password(&cfg, db.store.clone()));
        let api = Arc::new(
            m_gated::admin::DashboardApi::new(&cfg, auth, db.store.clone())
                .with_mailer(mailer.clone()),
        );
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
            mailer,
            _runtime: runtime,
        }
    }
    async fn finish(self) {
        self.db.close().await;
    }
    /// 요청 하나. `session`이 있으면 쿠키로 보내고, 쓰기면 Origin과 CSRF 토큰을 붙인다.
    async fn send(
        &self,
        method: &str,
        path: &str,
        session: Option<&str>,
        command: bool,
        body: Option<Value>,
    ) -> Answer {
        let mut r = Request::builder().method(method).uri(path);
        if let Some(s) = session {
            r = r.header("cookie", format!("mgated_session={s}"));
            if method != "GET" && path.starts_with("/api/") {
                let csrf = self.csrf(s).await;
                r = r.header("origin", ORIGIN).header("x-csrf-token", csrf);
            }
        }
        if command {
            r = r.header("x-gateway-command-id", Uuid::new_v4().to_string());
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
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let cookie = header("set-cookie").and_then(|c| {
            let value = c
                .strip_prefix("mgated_session=")?
                .split(';')
                .next()?
                .to_owned();
            Some(value)
        });
        let retry_after = header("retry-after");
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
        Answer {
            status,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            cookie,
            retry_after,
        }
    }
    async fn csrf(&self, session: &str) -> String {
        let request = Request::get("/api/csrf")
            .header("cookie", format!("mgated_session={session}"))
            .body(Body::empty())
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1 << 16).await.unwrap())
                .unwrap();
        body["token"].as_str().unwrap().to_owned()
    }
    /// 첫 관리자 설정 링크를 만든다(시작할 때 게이트웨이가 하는 일).
    async fn setup_link(&self) -> String {
        let token = link_token().unwrap();
        self.db
            .store
            .create_link(
                "alice@example.test",
                LinkKind::Setup,
                &token_hash(&token),
                None,
            )
            .await
            .unwrap();
        token
    }
    async fn set_password(&self, token: &str, password: &str, name: Option<&str>) -> Answer {
        let mut body = json!({"token": token, "password": password});
        if let Some(n) = name {
            body["display_name"] = json!(n);
        }
        self.send("POST", "/auth/password/set", None, false, Some(body))
            .await
    }
    async fn sign_in(&self, email: &str, password: &str) -> Answer {
        self.send(
            "POST",
            "/auth/password",
            None,
            false,
            Some(json!({"email": email, "password": password})),
        )
        .await
    }
    /// alice가 비밀번호를 정하고 받은 세션.
    async fn alice(&self) -> String {
        let token = self.setup_link().await;
        let a = self
            .set_password(&token, "alice password 1", Some("Alice"))
            .await;
        assert_eq!(a.status, 204, "{}", a.body);
        a.cookie.unwrap()
    }
    /// 메일 본문의 링크에서 토큰을 꺼낸다.
    fn mailed_token(&self, to: &str) -> String {
        let sent = self.mailer.sent.lock().unwrap();
        let mail = sent.iter().rev().find(|m| m.to == to).expect("mail");
        let start = mail.body.find("#token=").unwrap() + "#token=".len();
        mail.body[start..]
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned()
    }
    async fn count(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sql)
            .fetch_one(self.db.store.pool())
            .await
            .unwrap()
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn first_admin_sets_a_password_from_the_setup_link() {
    let h = Harness::new().await;
    // 시작할 때마다 새 설정 링크를 만들어도 예전 설정 링크는 유효하다.
    let first = h.setup_link().await;
    let second = h.setup_link().await;
    let info = h
        .send(
            "POST",
            "/auth/password/link",
            None,
            false,
            Some(json!({"token": first})),
        )
        .await;
    assert_eq!(info.status, 200, "{}", info.body);
    assert_eq!(info.body["email"], "alice@example.test");
    assert_eq!(info.body["kind"], "setup");
    assert_eq!(info.body["password_set"], false);
    // 처음에는 표시 이름이 있어야 하고, 비밀번호는 12자 이상이어야 한다.
    assert_eq!(
        h.set_password(&first, "long enough pw", None).await.status,
        400
    );
    let short = h.set_password(&first, "short", Some("Alice")).await;
    assert_eq!((short.status, code(&short)), (400, "password_invalid"));
    let ok = h
        .set_password(&first, "alice password 1", Some("Alice"))
        .await;
    assert_eq!(ok.status, 204, "{}", ok.body);
    let session = ok.cookie.unwrap();
    // 비밀번호를 정하면 그 사람의 링크는 모두 끝난다.
    for token in [&first, &second] {
        let gone = h
            .send(
                "POST",
                "/auth/password/link",
                None,
                false,
                Some(json!({"token": token})),
            )
            .await;
        assert_eq!((gone.status, code(&gone)), (404, "link_invalid"));
    }
    let me = h.send("GET", "/api/me", Some(&session), false, None).await;
    assert_eq!(me.status, 200, "{}", me.body);
    assert_eq!(me.body["role"], "admin");
    assert_eq!(me.body["principal"]["display_name"], "Alice");
    assert_eq!(me.body["auth"]["mode"], "password");
    assert_eq!(me.body["auth"]["refresh_url"], Value::Null);
    // 다시 로그인할 수 있다.
    let again = h.sign_in(" Alice@Example.test ", "alice password 1").await;
    assert_eq!(again.status, 204);
    assert!(again.cookie.is_some());
    h.finish().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn adding_a_person_sends_an_invitation_and_the_link_signs_them_in() {
    let h = Harness::new().await;
    let alice = h.alice().await;
    let added = h
        .send(
            "POST",
            "/api/admin/access",
            Some(&alice),
            true,
            Some(json!({"email": "bob@example.test", "role": "user"})),
        )
        .await;
    assert_eq!(added.status, 201, "{}", added.body);
    assert_eq!(added.body["invitation"], "sent");
    let mail = h.mailer.sent.lock().unwrap().last().cloned().unwrap();
    assert_eq!(mail.to, "bob@example.test");
    assert!(
        mail.body
            .contains(&format!("{ORIGIN}/dashboard/password#token="))
    );
    let token = h.mailed_token("bob@example.test");
    let info = h
        .send(
            "POST",
            "/auth/password/link",
            None,
            false,
            Some(json!({"token": token})),
        )
        .await;
    assert_eq!(info.body["kind"], "invitation");
    let set = h.set_password(&token, "bob password 12", Some("Bob")).await;
    assert_eq!(set.status, 204, "{}", set.body);
    let me = h
        .send("GET", "/api/me", set.cookie.as_deref(), false, None)
        .await;
    assert_eq!((me.status, me.body["role"].as_str()), (200, Some("user")));
    // 목록에 비밀번호를 정했는지가 보인다.
    let list = h
        .send("GET", "/api/admin/access", Some(&alice), false, None)
        .await;
    let bob = list.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["email"] == "bob@example.test")
        .unwrap()
        .clone();
    assert_eq!(bob["password_set"], true);
    h.finish().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn five_failures_lock_an_address_and_unknown_addresses_get_the_same_answer() {
    let h = Harness::new().await;
    h.alice().await;
    let unknown = h.sign_in("nobody@example.test", "whatever password").await;
    assert_eq!(
        (unknown.status, code(&unknown)),
        (401, "invalid_credentials")
    );
    for _ in 0..5 {
        let wrong = h.sign_in("alice@example.test", "wrong password!!").await;
        assert_eq!((wrong.status, code(&wrong)), (401, "invalid_credentials"));
        assert!(wrong.cookie.is_none());
    }
    // 잠긴 동안은 맞는 비밀번호도 받지 않는다.
    let locked = h.sign_in("alice@example.test", "alice password 1").await;
    assert_eq!((locked.status, code(&locked)), (429, "sign_in_locked"));
    let wait: i64 = locked.retry_after.unwrap().parse().unwrap();
    assert!((1..=900).contains(&wait));
    h.finish().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn reset_links_replace_earlier_links_and_a_mail_failure_leaves_no_link() {
    let h = Harness::new().await;
    let alice = h.alice().await;
    let added = h
        .send(
            "POST",
            "/api/admin/access",
            Some(&alice),
            true,
            Some(json!({"email": "bob@example.test", "role": "user"})),
        )
        .await;
    let id = added.body["target"]["id"].as_str().unwrap().to_owned();
    let invitation = h.mailed_token("bob@example.test");
    h.set_password(&invitation, "bob password 12", Some("Bob"))
        .await;
    // 비밀번호가 있는 사람에게는 재설정 링크가 간다.
    let link = h
        .send(
            "POST",
            &format!("/api/admin/access/{id}/link"),
            Some(&alice),
            true,
            None,
        )
        .await;
    assert_eq!(link.status, 200, "{}", link.body);
    assert_eq!(link.body["link"]["kind"], "reset");
    let first = h.mailed_token("bob@example.test");
    h.send(
        "POST",
        &format!("/api/admin/access/{id}/link"),
        Some(&alice),
        true,
        None,
    )
    .await;
    let second = h.mailed_token("bob@example.test");
    assert_ne!(first, second);
    let old = h
        .send(
            "POST",
            "/auth/password/link",
            None,
            false,
            Some(json!({"token": first})),
        )
        .await;
    assert_eq!(old.status, 404);
    // 재설정은 표시 이름 없이 된다. 예전 세션은 끝난다.
    let reset = h.set_password(&second, "bob new password", None).await;
    assert_eq!(reset.status, 204, "{}", reset.body);
    assert_eq!(
        h.sign_in("bob@example.test", "bob password 12")
            .await
            .status,
        401
    );
    assert_eq!(
        h.sign_in("bob@example.test", "bob new password")
            .await
            .status,
        204
    );
    // 메일을 보내지 못하면 502이고, 링크는 남지 않는다.
    h.mailer.fail.store(true, Ordering::SeqCst);
    let failed = h
        .send(
            "POST",
            &format!("/api/admin/access/{id}/link"),
            Some(&alice),
            true,
            None,
        )
        .await;
    assert_eq!((failed.status, code(&failed)), (502, "mail_failed"));
    assert_eq!(
        h.count("SELECT count(*) FROM gw_auth_links WHERE email='bob@example.test'")
            .await,
        0
    );
    assert_eq!(
        h.count("SELECT count(*) FROM gw_commands WHERE operation='send_access_link'")
            .await,
        2
    );
    h.finish().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn sessions_end_on_logout_password_change_and_removal() {
    let h = Harness::new().await;
    let alice = h.alice().await;
    // 로그아웃하면 그 세션은 끝난다.
    let other = h
        .sign_in("alice@example.test", "alice password 1")
        .await
        .cookie
        .unwrap();
    let out = h
        .send("POST", "/auth/logout", Some(&other), false, None)
        .await;
    assert_eq!(out.status, 204);
    assert_eq!(
        h.send("GET", "/api/me", Some(&other), false, None)
            .await
            .status,
        401
    );
    assert_eq!(
        h.send("GET", "/api/me", Some(&alice), false, None)
            .await
            .status,
        200
    );
    // 지금 비밀번호가 틀리면 바꾸지 않는다. 바꾸면 모든 세션이 끝난다.
    let wrong = h
        .send(
            "POST",
            "/api/me/password",
            Some(&alice),
            false,
            Some(json!({"current_password": "nope nope nope", "new_password": "alice password 2"})),
        )
        .await;
    assert_eq!(
        (wrong.status, code(&wrong)),
        (400, "current_password_invalid")
    );
    let changed = h
        .send(
            "POST",
            "/api/me/password",
            Some(&alice),
            false,
            Some(
                json!({"current_password": "alice password 1", "new_password": "alice password 2"}),
            ),
        )
        .await;
    assert_eq!(changed.status, 204, "{}", changed.body);
    assert_eq!(
        h.send("GET", "/api/me", Some(&alice), false, None)
            .await
            .status,
        401
    );
    let alice = h
        .sign_in("alice@example.test", "alice password 2")
        .await
        .cookie
        .unwrap();
    // 목록에서 지우면 세션과 비밀번호가 사라진다.
    let added = h
        .send(
            "POST",
            "/api/admin/access",
            Some(&alice),
            true,
            Some(json!({"email": "carol@example.test", "role": "user"})),
        )
        .await;
    let id = added.body["target"]["id"].as_str().unwrap().to_owned();
    let token = h.mailed_token("carol@example.test");
    let carol = h
        .set_password(&token, "carol password 1", Some("Carol"))
        .await
        .cookie
        .unwrap();
    let removed = h
        .send(
            "DELETE",
            &format!("/api/admin/access/{id}?expected_version=1"),
            Some(&alice),
            true,
            None,
        )
        .await;
    assert_eq!(removed.status, 200, "{}", removed.body);
    assert_eq!(
        h.send("GET", "/api/me", Some(&carol), false, None)
            .await
            .status,
        401
    );
    assert_eq!(
        h.sign_in("carol@example.test", "carol password 1")
            .await
            .status,
        401
    );
    assert_eq!(
        h.count("SELECT count(*) FROM gw_passwords WHERE email='carol@example.test'")
            .await,
        0
    );
    h.finish().await;
}
