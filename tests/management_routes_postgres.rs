//! 관리 API의 경로·메서드·인증·입력 조합마다 지금 응답을 기록한 표와 비교한다(결정 0010 규칙 6).
//! 응답의 상태 코드, 본문의 오류 코드, 헤더 몇 개(`Allow`, `Location`, `Content-Type`,
//! `x-gateway-error-code`, `Retry-After`)를 `tests/golden/management_routes.txt`와 맞춘다.
//! 표를 다시 쓰려면 `UPDATE_GOLDEN=1`을 준다. 표를 바꾸는 것은 응답이 바뀌는 것이므로 결정이 있어야 한다.
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
    auth::{SsoAuthenticator, SsoVerifier, jwks::JwksCache},
    catalog::CatalogService,
    config::{self, ConfigInputs},
    credentials::CredentialService,
    http::{self, AppState},
    runtime::{Dependency, Runtime},
};
use serde_json::{Value, json};
use std::{fmt::Write, sync::Arc};
use tower::ServiceExt;

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/golden/management_routes.txt"
);
/// 없는 자원을 가리키는 ID. 응답이 매번 같도록 고정한다.
const ID: &str = "00000000-0000-4000-8000-000000000001";

/// 관리 API의 모든 경로 템플릿과, 등록되지 않은 비슷한 경로 몇 개.
const PATHS: &[&str] = &[
    "/auth/login",
    "/api/me",
    "/api/usage",
    "/api/usage/models",
    "/api/csrf",
    "/api/keys",
    "/api/keys/{id}",
    "/api/admin/principals",
    "/api/admin/principals/{id}",
    "/api/admin/principals/{id}/policy",
    "/api/admin/principals/{id}/inference-pause/release",
    "/api/admin/principals/{id}/provider-credentials",
    "/api/admin/keys",
    "/api/admin/keys/{id}/provider-credential",
    "/api/admin/provider-credentials",
    "/api/admin/provider-credentials/{provider}",
    "/api/admin/provider-credentials/{provider}/{id}",
    "/api/admin/access",
    "/api/admin/access/{id}",
    "/api/admin/settings",
    "/api/admin/settings/budget-timezone",
    "/api/admin/settings/cutoff-rule",
    "/api/admin/token-check",
    "/api/admin/model-catalog",
    "/api/admin/model-catalog/sync",
    "/api/admin/model-catalog/approve",
    "/api/admin/usage",
    "/api/admin/requests",
    "/api/admin/requests/models",
    "/api/admin/requests/{id}",
    "/api/admin/requests/{id}/settlement",
    // 등록되지 않은 경로
    "/api/admin/nope",
    "/api/keys/{id}/extra",
    "/api/admin/principals/{id}/other",
    "/api/admin/settings/other",
];
const METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

/// 경로 템플릿을 실제 경로로 바꾼다. `bad`면 ID 자리에 틀린 값을 넣는다.
fn concrete(template: &str, bad: bool) -> String {
    let (id, provider) = if bad {
        ("not-a-uuid", "nope")
    } else {
        (ID, "anthropic")
    };
    template.replace("{id}", id).replace("{provider}", provider)
}

#[derive(Clone, Copy)]
enum Credential {
    None,
    AdminBearer,
    UserBearer,
    AdminCookie,
    AdminCookieCsrf,
}
impl Credential {
    const ALL: [Self; 5] = [
        Self::None,
        Self::AdminBearer,
        Self::UserBearer,
        Self::AdminCookie,
        Self::AdminCookieCsrf,
    ];
    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::AdminBearer => "admin-bearer",
            Self::UserBearer => "user-bearer",
            Self::AdminCookie => "admin-cookie",
            Self::AdminCookieCsrf => "admin-cookie-csrf",
        }
    }
}

/// 요청 입력의 결함. 정상 명령은 보내지 않는다. 어떤 조합도 데이터를 바꾸지 않게 하려는 것이다.
#[derive(Clone, Copy)]
enum Input {
    Plain,
    UnknownQuery,
    CommandOnly,
    BrokenJson,
    JsonArray,
    TextBody,
    LargeLength,
    Gzip,
    BadCommandId,
}
impl Input {
    const READ: [Self; 2] = [Self::Plain, Self::UnknownQuery];
    const WRITE: [Self; 8] = [
        Self::Plain,
        Self::CommandOnly,
        Self::BrokenJson,
        Self::JsonArray,
        Self::TextBody,
        Self::LargeLength,
        Self::Gzip,
        Self::BadCommandId,
    ];
    fn name(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::UnknownQuery => "unknown-query",
            Self::CommandOnly => "command-only",
            Self::BrokenJson => "broken-json",
            Self::JsonArray => "json-array",
            Self::TextBody => "text-body",
            Self::LargeLength => "large-length",
            Self::Gzip => "gzip",
            Self::BadCommandId => "bad-command-id",
        }
    }
}

struct Harness {
    db: postgres::TestDb,
    router: axum::Router,
    runtime: Arc<Runtime>,
    admin: String,
    user: String,
    csrf: String,
    origin: String,
    _jwks: sso::Server,
}
impl Harness {
    async fn new() -> Self {
        let mut env = support::environment();
        env.insert("ADMIN_EMAIL".into(), "alice@example.test".into());
        let cfg = config::load(ConfigInputs {
            yaml: support::YAML,
            file_environment: None,
            environment: &env,
        })
        .unwrap();
        let db = postgres::TestDb::new().await;
        // alice는 설정의 고정 관리자, bob은 접근 목록의 일반 사용자다.
        db.store
            .ensure_fixed_admin(&m_gated::auth::AccessSettings::from_config(&cfg))
            .await
            .unwrap();
        sqlx::query("INSERT INTO gw_access(id,email,role) VALUES($1,'bob@example.test','user')")
            .bind(uuid::Uuid::new_v4())
            .execute(db.store.pool())
            .await
            .unwrap();
        let jwks = sso::FakeJwks::new(vec![sso::Signer::shared().jwk.clone()])
            .serve()
            .await;
        let verifier = SsoVerifier::new(
            cfg.auth(),
            Arc::new(JwksCache::loopback_for_test(jwks.uri.clone()).unwrap()),
        );
        let admin = sso::Signer::shared().sign(&sso::claims(), "key1");
        let mut bob = sso::claims();
        bob["sub"] = json!("human-2");
        bob["email"] = json!("bob@example.test");
        let user = sso::Signer::shared().sign(&bob, "key1");
        let auth = Arc::new(SsoAuthenticator::new(&cfg, verifier, db.store.clone()));
        // 쓰이지 않는 주소. 어떤 조합도 카탈로그 동기화까지 가지 않는다.
        let source = url::Url::parse("http://127.0.0.1:9/").unwrap();
        let api = Arc::new(
            m_gated::admin::DashboardApi::new(&cfg, auth, db.store.clone())
                .with_credentials(CredentialService::for_test(
                    &[0x20; 32],
                    db.store.clone(),
                    Vec::new(),
                ))
                .with_catalog(CatalogService::for_test(&cfg, db.store.clone(), source)),
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
        let origin = cfg
            .public_origin()
            .as_str()
            .trim_end_matches('/')
            .to_owned();
        let mut h = Self {
            db,
            router,
            runtime,
            admin,
            user,
            csrf: String::new(),
            origin,
            _jwks: jwks,
        };
        let request = Request::get("/api/csrf")
            .header("cookie", format!("mgated_access={}", h.admin))
            .body(Body::empty())
            .unwrap();
        let response = h.router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        h.csrf = body["token"].as_str().unwrap().to_owned();
        h
    }

    /// 요청 하나를 보내고 기록할 한 줄을 돌려준다.
    async fn line(&self, method: &str, path: &str, credential: Credential, input: Input) -> String {
        let uri = match input {
            Input::UnknownQuery => format!("{path}?zz=1"),
            _ => path.to_owned(),
        };
        let mut r = Request::builder().method(method).uri(uri);
        r = match credential {
            Credential::None => r,
            Credential::AdminBearer => r.header("authorization", format!("Bearer {}", self.admin)),
            Credential::UserBearer => r.header("authorization", format!("Bearer {}", self.user)),
            Credential::AdminCookie => r.header("cookie", format!("mgated_access={}", self.admin)),
            Credential::AdminCookieCsrf => r
                .header("cookie", format!("mgated_access={}", self.admin))
                .header("origin", self.origin.as_str())
                .header("x-csrf-token", self.csrf.as_str()),
        };
        let command = "00000000-0000-4000-8000-0000000000c1";
        let json = "application/json";
        let (r, body) = match input {
            Input::Plain | Input::UnknownQuery => (r, Body::empty()),
            Input::CommandOnly => (r.header("x-gateway-command-id", command), Body::empty()),
            Input::BrokenJson => (
                r.header("x-gateway-command-id", command)
                    .header("content-type", json),
                Body::from("{"),
            ),
            Input::JsonArray => (
                r.header("x-gateway-command-id", command)
                    .header("content-type", json),
                Body::from("[]"),
            ),
            Input::TextBody => (
                r.header("x-gateway-command-id", command)
                    .header("content-type", "text/plain"),
                Body::from("{}"),
            ),
            Input::LargeLength => (
                r.header("x-gateway-command-id", command)
                    .header("content-type", json)
                    .header("content-length", "999999999"),
                Body::from("{}"),
            ),
            Input::Gzip => (
                r.header("x-gateway-command-id", command)
                    .header("content-type", json)
                    .header("content-encoding", "gzip"),
                Body::from("{}"),
            ),
            Input::BadCommandId => (
                r.header("x-gateway-command-id", "nope")
                    .header("content-type", json),
                Body::from("{}"),
            ),
        };
        let response = self
            .router
            .clone()
            .oneshot(r.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status().as_u16();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .map(|v| v.to_str().unwrap().to_owned())
                .unwrap_or_else(|| "-".into())
        };
        let headers = format!(
            "allow={} location={} type={} error={} retry={}",
            header("allow"),
            header("location"),
            header("content-type"),
            header("x-gateway-error-code"),
            header("retry-after")
        );
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let code = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|v| v["error"]["code"].as_str().map(str::to_owned))
            .unwrap_or_else(|| "-".into());
        format!(
            "{method} {path} {} {} => {status} {code} {headers}",
            credential.name(),
            input.name()
        )
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn every_route_answers_as_recorded() {
    let h = Harness::new().await;
    let mut table = String::new();
    for template in PATHS {
        let variants: &[bool] = if template.contains('{') {
            &[false, true]
        } else {
            &[false]
        };
        for &bad in variants {
            let path = concrete(template, bad);
            for method in METHODS {
                let inputs: &[Input] = match *method {
                    "GET" | "HEAD" => &Input::READ,
                    "OPTIONS" => &[Input::Plain],
                    _ => &Input::WRITE,
                };
                for credential in Credential::ALL {
                    for &input in inputs {
                        let line = h.line(method, &path, credential, input).await;
                        writeln!(table, "{line}").unwrap();
                    }
                }
            }
        }
    }
    h.runtime.stop();
    h.db.close().await;
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(std::path::Path::new(GOLDEN).parent().unwrap()).unwrap();
        std::fs::write(GOLDEN, &table).unwrap();
        return;
    }
    let recorded = std::fs::read_to_string(GOLDEN).expect("UPDATE_GOLDEN=1로 표를 먼저 만든다");
    let differences: Vec<String> = recorded
        .lines()
        .zip(table.lines())
        .filter(|(a, b)| a != b)
        .map(|(a, b)| format!("- {a}\n+ {b}"))
        .take(40)
        .collect();
    assert!(
        differences.is_empty() && recorded.lines().count() == table.lines().count(),
        "기록한 응답과 다르다({}줄 비교):\n{}",
        recorded.lines().count(),
        differences.join("\n")
    );
}
