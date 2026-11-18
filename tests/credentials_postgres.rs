#[path = "support/finance.rs"]
#[allow(dead_code)]
mod finance;
#[path = "support/postgres.rs"]
mod postgres;
#[path = "support/provider.rs"]
#[allow(dead_code)]
mod provider;
#[path = "support/sso.rs"]
#[allow(dead_code)]
mod sso;
mod support;
use m_gated::{
    auth::AccessSettings,
    config::{self, ConfigInputs, ProviderId},
    credentials::{CredentialService, CredentialStatus},
    ledger::{
        commands::CommandContext, credentials::CredentialCommand, management::ManagementError,
    },
    providers::transport::NativeClient,
};
use serde_json::json;
use sqlx::Row;
use std::sync::{
    Arc,
    atomic::{AtomicU16, AtomicUsize, Ordering},
};
use uuid::Uuid;
use zeroize::Zeroizing;

const KEY_A: &str = "sk-ant-synthetic-company-key-0001";
const KEY_B: &str = "sk-ant-synthetic-company-key-0002";
const ROOT: [u8; 32] = [0x20; 32];

/// 등록 확인용 합성 Provider. 허용 키만 200, 그 밖의 키는 401, `status`를 바꾸면 그 상태를 낸다.
struct FakeModels {
    uri: url::Url,
    calls: Arc<AtomicUsize>,
    status: Arc<AtomicU16>,
    _task: tokio::task::JoinHandle<()>,
}
async fn fake_models() -> FakeModels {
    use axum::{extract::State, http::HeaderMap, routing::get};
    let calls = Arc::new(AtomicUsize::new(0));
    let status = Arc::new(AtomicU16::new(0));
    let state = (calls.clone(), status.clone());
    let app = axum::Router::new()
        .route(
            "/v1/models",
            get(
                |State((calls, status)): State<(Arc<AtomicUsize>, Arc<AtomicU16>)>,
                 headers: HeaderMap| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(headers["anthropic-version"], "2023-06-01");
                    let forced = status.load(Ordering::SeqCst);
                    let code = if forced != 0 {
                        forced
                    } else if [KEY_A, KEY_B]
                        .iter()
                        .any(|k| headers.get("x-api-key").is_some_and(|v| v == k))
                    {
                        200
                    } else {
                        401
                    };
                    (axum::http::StatusCode::from_u16(code).unwrap(), "{}")
                },
            ),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}/", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    FakeModels {
        uri,
        calls,
        status,
        _task: task,
    }
}

fn settings() -> AccessSettings {
    let mut env = support::environment();
    env.insert("ALLOWED_EMAILS".into(), "alice@example.test".into());
    env.insert("ADMIN_EMAILS".into(), "alice@example.test".into());
    AccessSettings::from_config(
        &config::load(ConfigInputs {
            yaml: support::YAML,
            file_environment: None,
            environment: &env,
        })
        .unwrap(),
    )
}
fn service(root: &[u8; 32], db: &postgres::TestDb, fake: &FakeModels) -> Arc<CredentialService> {
    let cache = Arc::new(m_gated::credentials::CredentialCache::default());
    let verifier =
        NativeClient::loopback_store_for_test(fake.uri.clone(), ProviderId::Anthropic, cache)
            .unwrap();
    CredentialService::for_test(root, db.store.clone(), vec![verifier])
}
fn add(label: &str, secret: &str) -> CredentialCommand {
    CredentialCommand::Add {
        provider: ProviderId::Anthropic,
        label: label.to_owned(),
        secret: Zeroizing::new(secret.to_owned()),
    }
}
fn replace(id: Uuid, expected: i64, secret: &str) -> CredentialCommand {
    CredentialCommand::Replace {
        provider: ProviderId::Anthropic,
        id,
        expected,
        secret: Zeroizing::new(secret.to_owned()),
    }
}
fn retire(id: Uuid, expected: i64) -> CredentialCommand {
    CredentialCommand::Retire {
        provider: ProviderId::Anthropic,
        id,
        expected,
        move_to: None,
    }
}
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn provider_credentials_rotate_replay_shred_and_refresh_across_instances() {
    let db = postgres::TestDb::new().await;
    let fake = fake_models().await;
    let actor = finance::human(db.store.pool()).await;
    let settings = settings();
    let grants = json!([]);
    let ceiling = m_gated::ledger::policy::PolicyLimits::default();
    let ctx = |command_id| CommandContext {
        actor,
        command_id,
        method: "PUT",
        path: "/api/admin/provider-credentials/anthropic",
        settings: &settings,
        grants: &grants,
        digest: [1; 32],
        ceiling: &ceiling,
        execution_running: false,
    };
    let pod_a = service(&ROOT, &db, &fake);
    let pod_b = service(&ROOT, &db, &fake);
    pod_a.refresh().await.unwrap();
    assert_eq!(
        pod_a.cache().status(ProviderId::Anthropic),
        CredentialStatus::Missing
    );

    // 형식이 틀리면 Provider를 부르지 않고, Provider가 거절하면 저장하지 않는다.
    let bad = pod_a
        .execute(
            &ctx(Uuid::new_v4()),
            add("default", "sk-proj-not-anthropic-key"),
        )
        .await;
    assert!(matches!(bad, Err(ManagementError::Invalid)));
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    let rejected = pod_a
        .execute(
            &ctx(Uuid::new_v4()),
            add("default", "sk-ant-synthetic-unknown-key-0009"),
        )
        .await;
    assert!(matches!(rejected, Err(ManagementError::CredentialRejected)));
    fake.status.store(500, Ordering::SeqCst);
    let unverified = pod_a
        .execute(&ctx(Uuid::new_v4()), add("default", KEY_A))
        .await;
    assert!(matches!(
        unverified,
        Err(ManagementError::CredentialUnverified)
    ));
    fake.status.store(0, Ordering::SeqCst);
    let count = |db: &postgres::TestDb| {
        let pool = db.store.pool().clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM gw_provider_credentials")
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(count(&db).await, 0);

    // 등록 → 같은 명령 재전송은 Provider 재호출 없이 같은 결과.
    let command = Uuid::new_v4();
    let first = pod_a
        .execute(&ctx(command), add("default", KEY_A))
        .await
        .unwrap();
    assert_eq!(first.status, 201);
    let first_id: Uuid = first.body.as_ref().unwrap()["target"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let calls = fake.calls.load(Ordering::SeqCst);
    let replay = pod_a
        .execute(&ctx(command), add("default", KEY_A))
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(fake.calls.load(Ordering::SeqCst), calls);
    // 같은 command id에 다른 키는 충돌이다.
    assert!(matches!(
        pod_a.execute(&ctx(command), add("default", KEY_B)).await,
        Err(ManagementError::CommandConflict)
    ));
    // 같은 원문을 다른 이름으로 또 등록할 수 없고, 쓰고 있는 이름(대소문자 무시)도 다시 쓸 수 없다.
    assert!(matches!(
        pod_a.execute(&ctx(Uuid::new_v4()), add("other", KEY_A)).await,
        Err(ManagementError::CredentialDuplicate(id)) if id == first_id
    ));
    assert!(matches!(
        pod_a
            .execute(&ctx(Uuid::new_v4()), add("DEFAULT", KEY_B))
            .await,
        Err(ManagementError::CredentialLabelTaken)
    ));
    assert!(matches!(
        pod_a
            .execute(&ctx(Uuid::new_v4()), add(" padded", KEY_B))
            .await,
        Err(ManagementError::Invalid)
    ));
    // 버전이 다르면 교체하지 않는다. 같은 키로 교체하면 새 행 없이 noop.
    assert!(matches!(
        pod_a
            .execute(&ctx(Uuid::new_v4()), replace(first_id, 7, KEY_B))
            .await,
        Err(ManagementError::Version(1))
    ));
    let noop = pod_a
        .execute(&ctx(Uuid::new_v4()), replace(first_id, 1, KEY_A))
        .await
        .unwrap();
    assert_eq!(noop.status, 200);
    assert_eq!(noop.body.as_ref().unwrap()["effect"], "noop");
    assert_eq!(count(&db).await, 1);

    // 다른 인스턴스는 자기 refresh 때 반영한다.
    pod_a.refresh().await.unwrap();
    assert_eq!(
        pod_b.cache().status(ProviderId::Anthropic),
        CredentialStatus::Missing
    );
    pod_b.refresh().await.unwrap();
    let active = pod_b.cache().get(ProviderId::Anthropic, None).unwrap();
    assert_eq!(active.id, first_id);
    assert_eq!(active.secret.as_str(), KEY_A);

    // 교체: 새 행이 active가 되고 이전 행은 암호문이 지워진다. 고정해 둔 이전 키는 계속 쓸 수 있다.
    let held = pod_b.cache().get(ProviderId::Anthropic, None).unwrap();
    let rotated = pod_a
        .execute(&ctx(Uuid::new_v4()), replace(first_id, 1, KEY_B))
        .await
        .unwrap();
    assert_eq!(rotated.status, 201);
    let second_id: Uuid = rotated.body.as_ref().unwrap()["target"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let old = sqlx::query("SELECT state,nonce IS NULL AS n,ciphertext IS NULL AS c,retired_by FROM gw_provider_credentials WHERE id=$1")
        .bind(first_id)
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    assert_eq!(old.get::<String, _>("state"), "retired");
    assert!(old.get::<bool, _>("n") && old.get::<bool, _>("c"));
    assert_eq!(old.get::<Uuid, _>("retired_by"), actor);
    pod_b.refresh().await.unwrap();
    assert_eq!(held.secret.as_str(), KEY_A);
    assert_eq!(
        pod_b
            .cache()
            .get(ProviderId::Anthropic, None)
            .unwrap()
            .secret
            .as_str(),
        KEY_B
    );

    // 다른 root key로 기동한 인스턴스는 풀지 못하고 그 상태를 드러낸다(추론은 거절).
    let wrong_root = service(&[0x40; 32], &db, &fake);
    wrong_root.refresh().await.unwrap();
    assert_eq!(
        wrong_root.cache().status(ProviderId::Anthropic),
        CredentialStatus::Undecryptable
    );
    assert!(
        wrong_root
            .cache()
            .get(ProviderId::Anthropic, None)
            .is_none()
    );
    let listed = wrong_root.list().await.unwrap();
    assert_eq!(listed["providers"][0]["status"], "undecryptable");
    assert_eq!(
        pod_a.list().await.unwrap()["providers"][0]["status"],
        "active"
    );

    // 해제 후에는 모든 인스턴스가 미등록 상태가 된다.
    let retired = pod_a
        .execute(&ctx(Uuid::new_v4()), retire(second_id, 1))
        .await
        .unwrap();
    assert_eq!(retired.status, 200);
    pod_b.refresh().await.unwrap();
    assert_eq!(
        pod_b.cache().status(ProviderId::Anthropic),
        CredentialStatus::Missing
    );
    assert!(matches!(
        pod_a
            .execute(&ctx(Uuid::new_v4()), retire(second_id, 1))
            .await,
        Err(ManagementError::Version(2))
    ));
    // 설정되지 않은 Provider는 대상이 아니다.
    assert!(matches!(
        pod_a
            .execute(
                &ctx(Uuid::new_v4()),
                CredentialCommand::Add {
                    provider: ProviderId::Openai,
                    label: "default".into(),
                    secret: Zeroizing::new("sk-synthetic-openai-company-key".into()),
                },
            )
            .await,
        Err(ManagementError::NotFound)
    ));

    // 명령 기록·감사·키 테이블 어디에도 원문(문자열·hex)이 없다.
    let dump: String = sqlx::query_scalar("SELECT coalesce(string_agg(t,'|'),'') FROM (SELECT row_to_json(c)::text AS t FROM gw_provider_credentials c UNION ALL SELECT row_to_json(m)::text FROM gw_commands m UNION ALL SELECT row_to_json(a)::text FROM gw_audit_events a) x")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    for key in [KEY_A, KEY_B] {
        let hex: String = key.bytes().map(|b| format!("{b:02x}")).collect();
        assert!(!dump.contains(key) && !dump.contains(&hex));
    }
    let audits: Vec<String> = sqlx::query_scalar(
        "SELECT operation FROM gw_audit_events WHERE target_kind='credential' ORDER BY id",
    )
    .fetch_all(db.store.pool())
    .await
    .unwrap();
    assert_eq!(
        audits,
        [
            "add_provider_credential",
            "replace_provider_credential",
            "retire_provider_credential"
        ]
    );
    // 관리자가 아니면 거부한다.
    sqlx::query("UPDATE gw_principals SET email='bob@example.test' WHERE id=$1")
        .bind(actor)
        .execute(db.store.pool())
        .await
        .unwrap();
    assert!(matches!(
        pod_a
            .execute(&ctx(Uuid::new_v4()), add("default", KEY_A))
            .await,
        Err(ManagementError::Forbidden)
    ));
    db.close().await;
}

/// HTTP 경로 전체(관리 API → 가상 키 발급 → 추론)로 회사 키 여러 개·이름·연결을 확인한다.
struct Http {
    db: postgres::TestDb,
    router: axum::Router,
    service: Arc<CredentialService>,
    provider: Arc<provider::Provider>,
    alice: String,
    bob: String,
    _servers: (provider::Server, sso::Server, FakeModels),
}
impl Http {
    async fn new() -> Self {
        use m_gated::{
            auth::{SsoAuthenticator, SsoVerifier, jwks::JwksCache},
            http::{self, AppState},
            ledger::PgLedger,
            runtime::{Dependency, Runtime},
        };
        let mut env = support::environment();
        env.insert(
            "ALLOWED_EMAILS".into(),
            "alice@example.test,bob@example.test".into(),
        );
        env.insert("ADMIN_EMAILS".into(), "alice@example.test".into());
        let cfg = config::load(ConfigInputs {
            yaml: support::YAML,
            file_environment: None,
            environment: &env,
        })
        .unwrap();
        let db = postgres::TestDb::new().await;
        let jwks = sso::FakeJwks::new(vec![sso::Signer::shared().jwk.clone()])
            .serve()
            .await;
        let verifier = SsoVerifier::new(
            cfg.auth(),
            Arc::new(JwksCache::loopback_for_test(jwks.uri.clone()).unwrap()),
        );
        let alice = sso::Signer::shared().sign(&sso::claims(), "key1");
        let mut bob_claims = sso::claims();
        bob_claims["sub"] = json!("human-2");
        bob_claims["email"] = json!("bob@example.test");
        let bob = sso::Signer::shared().sign(&bob_claims, "key1");
        let auth = Arc::new(SsoAuthenticator::new(&cfg, verifier, db.store.clone()));
        let fake = fake_models().await;
        let service = service(&ROOT, &db, &fake);
        let api = Arc::new(
            m_gated::admin::DashboardApi::new(&cfg, auth.clone(), db.store.clone())
                .with_credentials(service.clone()),
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
        let p = provider::Provider::new();
        let provider_server = p.serve().await;
        let client = NativeClient::loopback_store_for_test(
            provider_server.base.clone(),
            ProviderId::Anthropic,
            service.cache(),
        )
        .unwrap();
        let ledger = Arc::new(PgLedger::new(
            db.store.clone(),
            AccessSettings::from_config(&cfg),
            m_gated::gateway::auth_config_digest(&cfg),
        ));
        let gateway = Arc::new(m_gated::gateway::InferenceGateway::assemble(
            &cfg.providers()[0],
            auth,
            ledger,
            client,
        ));
        let router = http::router_with_components(
            AppState {
                runtime: runtime.clone(),
            },
            Some(gateway),
            api.routes(runtime),
        );
        Self {
            db,
            router,
            service,
            provider: p,
            alice,
            bob,
            _servers: (provider_server, jwks, fake),
        }
    }
    async fn call(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<serde_json::Value>,
    ) -> (u16, serde_json::Value) {
        use axum::body::{Body, to_bytes};
        use tower::ServiceExt;
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {token}"))
            .header("anthropic-version", "2023-06-01")
            .header("x-gateway-command-id", Uuid::new_v4().to_string());
        if body.is_some() {
            request = request.header("content-type", "application/json");
        }
        let response = self
            .router
            .clone()
            .oneshot(
                request
                    .body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status().as_u16();
        let code = response
            .headers()
            .get("x-gateway-error-code")
            .map(|v| v.to_str().unwrap().to_owned());
        let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        if let (Some(code), serde_json::Value::Object(map)) = (code, &mut value) {
            map.insert("gateway_error_code".into(), json!(code));
        }
        (status, value)
    }
    /// 추론 한 번을 보내고, Provider가 받은 회사 키 원문과 장부에 남은 회사 키 id를 돌려준다.
    async fn infer(&self, token: &str) -> (u16, Option<String>, Option<Uuid>) {
        self.provider.calls.lock().await.clear();
        let body = json!({"model":"synthetic-text","max_tokens":100,"messages":[{"role":"user","content":"x"}]});
        let (status, _) = self.call("POST", "/v1/messages", token, Some(body)).await;
        let sent = self.provider.calls.lock().await.last().and_then(|c| {
            c.headers
                .get("x-api-key")
                .map(|v| v.to_str().unwrap().to_owned())
        });
        let used: Option<Option<Uuid>> = sqlx::query_scalar(
            "SELECT provider_credential_id FROM gw_requests ORDER BY admitted_at DESC LIMIT 1",
        )
        .fetch_optional(self.db.store.pool())
        .await
        .unwrap();
        (status, sent, used.flatten())
    }
}
fn id_of(v: &serde_json::Value) -> Uuid {
    v.as_str().unwrap().parse().unwrap()
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn several_labeled_keys_bind_to_virtual_keys_and_route_requests() {
    let h = Http::new().await;
    const CREDS: &str = "/api/admin/provider-credentials";
    // 주체를 만들고 둘 다 synthetic-text를 쓸 수 있게 한다.
    for token in [&h.alice, &h.bob] {
        assert_eq!(h.call("GET", "/api/me", token, None).await.0, 200);
    }
    sqlx::query("UPDATE gw_policies SET allow_rules=$1 WHERE key_id IS NULL")
        .bind(
            json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}]),
        )
        .execute(h.db.store.pool())
        .await
        .unwrap();

    // 이름을 붙여 키 두 개를 등록한다. 첫 키가 기본 키가 된다.
    let add = |label: &str, secret: &str| json!({"label":label,"secret":secret});
    let (status, a) = h
        .call(
            "POST",
            &format!("{CREDS}/anthropic"),
            &h.alice,
            Some(add("team-a", KEY_A)),
        )
        .await;
    assert_eq!(status, 201, "{a}");
    assert_eq!(a["credential"]["label"], "team-a");
    assert_eq!(a["credential"]["default"], true);
    let a_id = id_of(&a["target"]["id"]);
    let (status, err) = h
        .call(
            "POST",
            &format!("{CREDS}/anthropic"),
            &h.alice,
            Some(add("Team-A", KEY_B)),
        )
        .await;
    assert_eq!(
        (status, err["error"]["code"].clone()),
        (409, json!("credential_label_taken"))
    );
    let (status, err) = h
        .call(
            "POST",
            &format!("{CREDS}/anthropic"),
            &h.alice,
            Some(add("team-b", KEY_A)),
        )
        .await;
    assert_eq!(
        (status, err["error"]["code"].clone()),
        (409, json!("credential_duplicate"))
    );
    let (status, b) = h
        .call(
            "POST",
            &format!("{CREDS}/anthropic"),
            &h.alice,
            Some(add("team-b", KEY_B)),
        )
        .await;
    assert_eq!(status, 201, "{b}");
    assert_eq!(b["credential"]["default"], false);
    let b_id = id_of(&b["target"]["id"]);
    // 관리자가 아니면 목록도 볼 수 없다.
    assert_eq!(h.call("GET", CREDS, &h.bob, None).await.0, 403);
    let (_, list) = h.call("GET", CREDS, &h.alice, None).await;
    let listed: Vec<_> = list["providers"][0]["credentials"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["label"].as_str().unwrap().to_owned(),
                c["default"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(listed, [("team-a".into(), true), ("team-b".into(), false)]);
    h.service.refresh().await.unwrap();

    // 일반 사용자는 회사 키를 고를 수 없고, 발급하면 기본 키에 연결된다.
    let issue = |chosen: Option<Uuid>| {
        let mut body = json!({"name":"k","validity":"1_year","grants":[{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}],"limits":{"budget":null,"rpm":null,"tpm":null,"count_rpm":null}});
        if let Some(id) = chosen {
            body["provider_credentials"] = json!({"anthropic": id});
        }
        body
    };
    let (status, _) = h
        .call("POST", "/api/keys", &h.bob, Some(issue(Some(b_id))))
        .await;
    assert_eq!(status, 403);
    let (status, bob_key) = h.call("POST", "/api/keys", &h.bob, Some(issue(None))).await;
    assert_eq!(status, 201, "{bob_key}");
    assert_eq!(
        bob_key["metadata"]["provider_credentials"][0]["label"],
        "team-a"
    );
    let bob_secret = bob_key["key"].as_str().unwrap().to_owned();
    let bob_key_id = id_of(&bob_key["metadata"]["id"]);
    // 관리자는 발급할 때 회사 키를 고른다. 해제됐거나 다른 Provider 키는 거부한다.
    let (status, _) = h
        .call(
            "POST",
            "/api/keys",
            &h.alice,
            Some(issue(Some(Uuid::new_v4()))),
        )
        .await;
    assert_eq!(status, 400);
    let (status, alice_key) = h
        .call("POST", "/api/keys", &h.alice, Some(issue(Some(b_id))))
        .await;
    assert_eq!(status, 201, "{alice_key}");
    assert_eq!(
        alice_key["metadata"]["provider_credentials"][0]["label"],
        "team-b"
    );
    let alice_secret = alice_key["key"].as_str().unwrap().to_owned();
    let alice_key_id = id_of(&alice_key["metadata"]["id"]);
    // 키 목록에도 연결된 회사 키 이름이 보인다.
    let (_, keys) = h.call("GET", "/api/keys", &h.bob, None).await;
    assert_eq!(
        keys["items"][0]["provider_credentials"][0]["label"],
        "team-a"
    );
    assert_eq!(keys["items"][0]["provider_credentials"][0]["hint"], "0001");

    // 요청은 가상 키에 연결된 회사 키로 나가고, 장부에 그 키가 남는다. SSO 직접 요청은 기본 키를 쓴다.
    assert_eq!(
        h.infer(&bob_secret).await,
        (200, Some(KEY_A.into()), Some(a_id))
    );
    assert_eq!(
        h.infer(&alice_secret).await,
        (200, Some(KEY_B.into()), Some(b_id))
    );
    assert_eq!(
        h.infer(&h.alice).await,
        (200, Some(KEY_A.into()), Some(a_id))
    );
    let request: Uuid = sqlx::query_scalar("SELECT id FROM gw_requests WHERE provider_credential_id=$1 ORDER BY admitted_at DESC LIMIT 1")
        .bind(b_id)
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    let (_, detail) = h
        .call(
            "GET",
            &format!("/api/admin/requests/{request}"),
            &h.alice,
            None,
        )
        .await;
    assert_eq!(detail["provider_credential"]["label"], "team-b", "{detail}");

    // 이름 변경과 기본 키 지정. 연결이 있는 가상 키는 그대로 자기 키를 쓴다.
    let (status, renamed) = h
        .call(
            "PATCH",
            &format!("{CREDS}/anthropic/{b_id}"),
            &h.alice,
            Some(json!({"expected_version":"1","label":"team-b-prod","default":true})),
        )
        .await;
    assert_eq!(status, 200, "{renamed}");
    assert_eq!(
        (
            renamed["credential"]["label"].clone(),
            renamed["credential"]["default"].clone()
        ),
        (json!("team-b-prod"), json!(true))
    );
    let (status, stale) = h
        .call(
            "PATCH",
            &format!("{CREDS}/anthropic/{b_id}"),
            &h.alice,
            Some(json!({"expected_version":"1","label":"x"})),
        )
        .await;
    assert_eq!(
        (status, stale["error"]["code"].clone()),
        (409, json!("version_conflict"))
    );
    h.service.refresh().await.unwrap();
    assert_eq!(h.infer(&h.alice).await.1.as_deref(), Some(KEY_B));
    assert_eq!(h.infer(&bob_secret).await.1.as_deref(), Some(KEY_A));

    // 가상 키가 연결된 키는 옮길 키 없이 해제할 수 없다.
    let (status, busy) = h
        .call(
            "DELETE",
            &format!("{CREDS}/anthropic/{a_id}?expected_version=2"),
            &h.alice,
            None,
        )
        .await;
    assert_eq!(
        (status, busy["error"]["code"].clone()),
        (409, json!("credential_in_use")),
        "{busy}"
    );
    assert_eq!(busy["error"]["details"]["bound_keys"], "1");

    // 관리자는 가상 키 하나의 연결을 바꾼다. 지금 연결을 잘못 알고 있으면 거부한다.
    let bind = |credential: Uuid, expected: Uuid| json!({"provider":"anthropic","credential_id":credential,"expected_credential_id":expected});
    let path = format!("/api/admin/keys/{alice_key_id}/provider-credential");
    let (status, err) = h.call("PUT", &path, &h.alice, Some(bind(a_id, a_id))).await;
    assert_eq!(
        (status, err["error"]["code"].clone()),
        (409, json!("credential_binding_conflict"))
    );
    assert_eq!(
        h.call("PUT", &path, &h.bob, Some(bind(a_id, b_id))).await.0,
        403
    );
    let (status, rebound) = h.call("PUT", &path, &h.alice, Some(bind(a_id, b_id))).await;
    assert_eq!(status, 200, "{rebound}");
    assert_eq!(rebound["key"]["provider_credentials"][0]["label"], "team-a");
    assert_eq!(h.infer(&alice_secret).await.1.as_deref(), Some(KEY_A));

    // 옮길 키를 정하면 해제되고, 연결된 가상 키 두 개가 함께 옮겨진다.
    let (status, retired) = h
        .call(
            "DELETE",
            &format!("{CREDS}/anthropic/{a_id}?expected_version=2&move_to={b_id}"),
            &h.alice,
            None,
        )
        .await;
    assert_eq!(status, 200, "{retired}");
    assert_eq!(retired["credential"]["moved_keys"], "2");
    h.service.refresh().await.unwrap();
    assert_eq!(
        h.infer(&bob_secret).await,
        (200, Some(KEY_B.into()), Some(b_id))
    );
    assert_eq!(h.infer(&alice_secret).await.1.as_deref(), Some(KEY_B));

    // 원문 교체: 이름·기본 여부·연결이 새 키로 옮겨진다.
    let (status, replaced) = h
        .call(
            "PUT",
            &format!("{CREDS}/anthropic/{b_id}"),
            &h.alice,
            Some(json!({"secret":KEY_A,"expected_version":"2"})),
        )
        .await;
    assert_eq!(status, 201, "{replaced}");
    assert_eq!(
        (
            replaced["credential"]["label"].clone(),
            replaced["credential"]["default"].clone(),
            replaced["credential"]["moved_keys"].clone()
        ),
        (json!("team-b-prod"), json!(true), json!("2"))
    );
    let c_id = id_of(&replaced["target"]["id"]);
    h.service.refresh().await.unwrap();
    assert_eq!(
        h.infer(&bob_secret).await,
        (200, Some(KEY_A.into()), Some(c_id))
    );
    let (_, keys) = h.call("GET", "/api/keys", &h.bob, None).await;
    assert_eq!(
        keys["items"][0]["provider_credentials"][0]["credential_id"],
        json!(c_id)
    );

    // 마지막 키는 옮길 곳 없이 해제할 수 있다. 그 뒤 요청은 송신 전에 거절한다.
    let (status, _) = h
        .call(
            "DELETE",
            &format!("{CREDS}/anthropic/{c_id}?expected_version=1"),
            &h.alice,
            None,
        )
        .await;
    assert_eq!(status, 200);
    h.service.refresh().await.unwrap();
    let (status, _, _) = h.infer(&bob_secret).await;
    assert_eq!(status, 503);
    assert!(h.provider.calls.lock().await.is_empty());
    // 해제한 이름은 다시 쓸 수 있고, 연결이 없던 가상 키는 새 기본 키를 쓴다.
    let (status, again) = h
        .call(
            "POST",
            &format!("{CREDS}/anthropic"),
            &h.alice,
            Some(add("team-a", KEY_B)),
        )
        .await;
    assert_eq!(status, 201, "{again}");
    h.service.refresh().await.unwrap();
    assert_eq!(h.infer(&bob_secret).await.1.as_deref(), Some(KEY_B));
    let audits: Vec<String> = sqlx::query_scalar(
        "SELECT operation FROM gw_audit_events WHERE operation LIKE '%credential%' ORDER BY id",
    )
    .fetch_all(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(
        audits,
        [
            "add_provider_credential",
            "add_provider_credential",
            "update_provider_credential",
            "bind_key_credential",
            "retire_provider_credential",
            "replace_provider_credential",
            "retire_provider_credential",
            "add_provider_credential"
        ]
    );
    let _ = bob_key_id;
    h.db.close().await;
}

/// 사용자별 회사 키 인가: 인가된 키 안에서만 고르고, 인가를 풀면 연결된 가상 키를 막거나 옮긴다.
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn user_authorization_limits_company_keys_and_blocks_deauthorized_keys() {
    let h = Http::new().await;
    const CREDS: &str = "/api/admin/provider-credentials";
    for token in [&h.alice, &h.bob] {
        assert_eq!(h.call("GET", "/api/me", token, None).await.0, 200);
    }
    sqlx::query("UPDATE gw_policies SET allow_rules=$1 WHERE key_id IS NULL")
        .bind(
            json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}]),
        )
        .execute(h.db.store.pool())
        .await
        .unwrap();
    let (_, me) = h.call("GET", "/api/me", &h.bob, None).await;
    let bob = id_of(&me["principal"]["id"]);
    assert_eq!(me["provider_credentials"], json!([]));
    let add = |label: &str, secret: &str| json!({"label":label,"secret":secret});
    let (_, a) = h
        .call(
            "POST",
            &format!("{CREDS}/anthropic"),
            &h.alice,
            Some(add("team-a", KEY_A)),
        )
        .await;
    let a_id = id_of(&a["target"]["id"]);
    let (_, b) = h
        .call(
            "POST",
            &format!("{CREDS}/anthropic"),
            &h.alice,
            Some(add("team-b", KEY_B)),
        )
        .await;
    let b_id = id_of(&b["target"]["id"]);
    h.service.refresh().await.unwrap();

    let path = format!("/api/admin/principals/{bob}/provider-credentials");
    let authorize = |ids: &[Uuid],
                     default: Option<Uuid>,
                     expected: &[Uuid],
                     expected_default: Option<Uuid>| {
        json!({"provider":"anthropic","credential_ids":ids,"default_credential_id":default,"expected":{"credential_ids":expected,"default_credential_id":expected_default}})
    };
    // 관리자만 인가를 바꾼다. 기본 키는 인가된 키 중 하나여야 한다.
    assert_eq!(
        h.call(
            "PUT",
            &path,
            &h.bob,
            Some(authorize(&[b_id], Some(b_id), &[], None))
        )
        .await
        .0,
        403
    );
    assert_eq!(
        h.call(
            "PUT",
            &path,
            &h.alice,
            Some(authorize(&[b_id], Some(a_id), &[], None))
        )
        .await
        .0,
        400
    );
    let (status, granted) = h
        .call(
            "PUT",
            &path,
            &h.alice,
            Some(authorize(&[b_id], Some(b_id), &[], None)),
        )
        .await;
    assert_eq!(status, 200, "{granted}");
    assert_eq!(
        granted["authorization"]["provider_credentials"][0]["credentials"][0]["label"],
        "team-b"
    );
    // 다른 관리자가 먼저 바꿨으면(지금 인가를 잘못 알면) 덮어쓰지 않는다.
    let (status, stale) = h
        .call(
            "PUT",
            &path,
            &h.alice,
            Some(authorize(&[a_id], Some(a_id), &[], None)),
        )
        .await;
    assert_eq!(
        (status, stale["error"]["code"].clone()),
        (409, json!("credential_authorization_conflict"))
    );
    assert_eq!(stale["error"]["details"]["credential_ids"], json!([b_id]));

    // 사용자는 /api/me 에서 자기에게 인가된 회사 키를 본다.
    let (_, me) = h.call("GET", "/api/me", &h.bob, None).await;
    assert_eq!(
        me["provider_credentials"][0]["credentials"][0]["label"],
        "team-b"
    );
    assert_eq!(
        me["provider_credentials"][0]["credentials"][0]["default"],
        true
    );

    // 인가된 키는 일반 사용자도 고를 수 있다. 인가 밖의 키는 거절한다. 고르지 않으면 사용자의 기본 키다.
    let issue = |chosen: Option<Uuid>| {
        let mut body = json!({"name":"k","validity":"1_year","grants":[{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}],"limits":{"budget":null,"rpm":null,"tpm":null,"count_rpm":null}});
        if let Some(id) = chosen {
            body["provider_credentials"] = json!({"anthropic": id});
        }
        body
    };
    let (status, denied) = h
        .call("POST", "/api/keys", &h.bob, Some(issue(Some(a_id))))
        .await;
    assert_eq!(
        (status, denied["error"]["code"].clone()),
        (403, json!("provider_credential_not_authorized"))
    );
    let (status, chosen) = h
        .call("POST", "/api/keys", &h.bob, Some(issue(Some(b_id))))
        .await;
    assert_eq!(status, 201, "{chosen}");
    let (status, plain) = h.call("POST", "/api/keys", &h.bob, Some(issue(None))).await;
    assert_eq!(status, 201, "{plain}");
    assert_eq!(
        plain["metadata"]["provider_credentials"][0]["label"],
        "team-b"
    );
    let key1 = chosen["key"].as_str().unwrap().to_owned();
    let key2 = plain["key"].as_str().unwrap().to_owned();
    let key1_id = id_of(&chosen["metadata"]["id"]);
    // 요청은 인가된 키로 나간다. SSO 직접 요청도 사용자의 기본 키(Provider 기본 키가 아니다)를 쓴다.
    assert_eq!(h.infer(&key1).await, (200, Some(KEY_B.into()), Some(b_id)));
    assert_eq!(h.infer(&h.bob).await, (200, Some(KEY_B.into()), Some(b_id)));
    assert_eq!(h.infer(&h.alice).await.1.as_deref(), Some(KEY_A));
    // 관리자도 인가 밖의 키로는 연결을 옮기지 못한다.
    let (status, err) = h
        .call(
            "PUT",
            &format!("/api/admin/keys/{key1_id}/provider-credential"),
            &h.alice,
            Some(
                json!({"provider":"anthropic","credential_id":a_id,"expected_credential_id":b_id}),
            ),
        )
        .await;
    assert_eq!(
        (status, err["error"]["code"].clone()),
        (403, json!("provider_credential_not_authorized"))
    );

    // 연결된 가상 키가 있는 회사 키의 인가를 풀려면 막을지 옮길지 골라야 한다.
    let (status, busy) = h
        .call(
            "PUT",
            &path,
            &h.alice,
            Some(authorize(&[a_id], Some(a_id), &[b_id], Some(b_id))),
        )
        .await;
    assert_eq!(
        (status, busy["error"]["code"].clone()),
        (409, json!("credential_authorization_in_use"))
    );
    assert_eq!(busy["error"]["details"]["bound_keys"], "2");
    let mut block = authorize(&[a_id], Some(a_id), &[b_id], Some(b_id));
    block["bound_keys"] = json!("block");
    let (status, blocked) = h.call("PUT", &path, &h.alice, Some(block)).await;
    assert_eq!(status, 200, "{blocked}");
    assert_eq!(blocked["authorization"]["blocked_keys"], "2");
    // 막힌 키의 요청은 송신 전에 403으로 거절된다. 키 목록에 인가 해제가 보인다.
    let (status, _, _) = h.infer(&key1).await;
    assert_eq!(status, 403);
    assert!(h.provider.calls.lock().await.is_empty());
    let (_, body) = h
        .call(
            "POST",
            "/v1/messages",
            &key2,
            Some(json!({"model":"synthetic-text","max_tokens":100,"messages":[{"role":"user","content":"x"}]})),
        )
        .await;
    assert_eq!(
        body["gateway_error_code"],
        "provider_credential_not_authorized"
    );
    let (_, keys) = h.call("GET", "/api/keys", &h.bob, None).await;
    for item in keys["items"].as_array().unwrap() {
        assert_eq!(
            item["provider_credentials"][0]["authorized"], false,
            "{item}"
        );
    }
    let (_, keys) = h.call("GET", "/api/keys", &h.alice, None).await;
    assert!(
        keys["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|k| k["provider_credentials"][0]["authorized"] == true)
    );
    // SSO 직접 요청은 새 기본 키를 쓴다.
    assert_eq!(h.infer(&h.bob).await.1.as_deref(), Some(KEY_A));

    // 다시 인가하면 막힌 키가 원래 회사 키로 다시 나간다.
    let (status, _) = h
        .call(
            "PUT",
            &path,
            &h.alice,
            Some(authorize(&[a_id, b_id], Some(a_id), &[a_id], Some(a_id))),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(h.infer(&key1).await.1.as_deref(), Some(KEY_B));
    // 옮기기를 고르면 연결된 가상 키가 인가된 키로 옮겨진다.
    let mut moving = authorize(&[a_id], Some(a_id), &[a_id, b_id], Some(a_id));
    moving["bound_keys"] = json!({"move_to": a_id});
    let (status, moved) = h.call("PUT", &path, &h.alice, Some(moving)).await;
    assert_eq!(status, 200, "{moved}");
    assert_eq!(moved["authorization"]["moved_keys"], "2");
    assert_eq!(h.infer(&key1).await, (200, Some(KEY_A.into()), Some(a_id)));

    // 회사 키를 해제하면 인가도 옮길 키로 넘어간다.
    let (status, _) = h
        .call(
            "PUT",
            &path,
            &h.alice,
            Some(authorize(&[b_id], Some(b_id), &[a_id], Some(a_id))).map(|mut v| {
                v["bound_keys"] = json!({"move_to": b_id});
                v
            }),
        )
        .await;
    assert_eq!(status, 200);
    let version: String =
        h.call("GET", CREDS, &h.alice, None).await.1["providers"][0]["credentials"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == json!(b_id))
            .unwrap()["version"]
            .as_str()
            .unwrap()
            .to_owned();
    let (status, retired) = h
        .call(
            "DELETE",
            &format!("{CREDS}/anthropic/{b_id}?expected_version={version}&move_to={a_id}"),
            &h.alice,
            None,
        )
        .await;
    assert_eq!(status, 200, "{retired}");
    let (_, detail) = h
        .call(
            "GET",
            &format!("/api/admin/principals/{bob}"),
            &h.alice,
            None,
        )
        .await;
    assert_eq!(
        detail["principal"]["provider_credentials"],
        json!([{"provider":"anthropic","credentials":[{"id":a_id,"label":"team-a","hint":"0001","default":true}]}])
    );
    // 목록을 비우면 제한이 풀린다.
    let (status, cleared) = h
        .call(
            "PUT",
            &path,
            &h.alice,
            Some(authorize(&[], None, &[a_id], Some(a_id))),
        )
        .await;
    assert_eq!(status, 200, "{cleared}");
    assert_eq!(cleared["authorization"]["provider_credentials"], json!([]));
    h.db.close().await;
}

/// 예산 시간대는 관리자가 한 곳에서 정하고, 바꾸면 다음 기간부터 적용한다.
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn budget_timezone_is_global_and_changes_from_next_period() {
    let h = Http::new().await;
    for token in [&h.alice, &h.bob] {
        assert_eq!(h.call("GET", "/api/me", token, None).await.0, 200);
    }
    let pool = h.db.store.pool();
    // 기간 계산: 바뀐 직후의 기간 하나만 이전 기간 끝부터 새 시간대의 다음 달 1일 0시까지다.
    let period = |t: &'static str| async move {
        let r = sqlx::query("SELECT period_start::text AS s,period_end::text AS e,timezone FROM gw_budget_period($1::timestamptz)")
            .bind(t)
            .fetch_one(pool)
            .await
            .unwrap();
        (
            r.get::<String, _>("s"),
            r.get::<String, _>("e"),
            r.get::<String, _>("timezone"),
        )
    };
    sqlx::query("SET TIME ZONE 'UTC'")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO gw_budget_timezones(effective_from,timezone) VALUES('2030-12-01T00:00:00Z','Asia/Seoul'),('2031-03-01T00:00:00+09:00','UTC')")
        .execute(pool)
        .await
        .unwrap();
    let tests = [
        // UTC 기간: 11월
        (
            "2030-11-20T00:00:00Z",
            "2030-11-01 00:00:00+00",
            "2030-12-01 00:00:00+00",
            "UTC",
        ),
        // UTC→서울: 12월 1일 09:00(서울)부터 1월 1일 00:00(서울)까지(9시간 짧다)
        (
            "2030-12-05T00:00:00Z",
            "2030-12-01 00:00:00+00",
            "2030-12-31 15:00:00+00",
            "Asia/Seoul",
        ),
        (
            "2031-01-05T00:00:00Z",
            "2030-12-31 15:00:00+00",
            "2031-01-31 15:00:00+00",
            "Asia/Seoul",
        ),
        // 서울 2월 기간은 다음 변경 시각(3월 1일 00:00 서울)에서 끝난다.
        (
            "2031-02-10T00:00:00Z",
            "2031-01-31 15:00:00+00",
            "2031-02-28 15:00:00+00",
            "Asia/Seoul",
        ),
        // 서울→UTC: 3월 1일 00:00(서울)부터 4월 1일 00:00(UTC)까지(9시간 길다)
        (
            "2031-03-01T00:00:00Z",
            "2031-02-28 15:00:00+00",
            "2031-04-01 00:00:00+00",
            "UTC",
        ),
        (
            "2031-04-02T00:00:00Z",
            "2031-04-01 00:00:00+00",
            "2031-05-01 00:00:00+00",
            "UTC",
        ),
    ];
    for (t, s, e, tz) in tests {
        assert_eq!(period(t).await, (s.into(), e.into(), tz.into()), "{t}");
    }
    sqlx::query("DELETE FROM gw_budget_timezones WHERE effective_from>'-infinity'")
        .execute(pool)
        .await
        .unwrap();

    // 관리자만 보고 바꾼다.
    assert_eq!(
        h.call("GET", "/api/admin/settings", &h.bob, None).await.0,
        403
    );
    let (status, view) = h.call("GET", "/api/admin/settings", &h.alice, None).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["budget_timezone"]["timezone"], "UTC");
    assert_eq!(view["budget_timezone"]["pending"], json!(null));
    assert_eq!(view["budget_timezone"]["version"], "0");
    const SET: &str = "/api/admin/settings/budget-timezone";
    assert_eq!(
        h.call(
            "PUT",
            SET,
            &h.bob,
            Some(json!({"timezone":"Asia/Seoul","expected_version":"0"}))
        )
        .await
        .0,
        403
    );
    assert_eq!(
        h.call(
            "PUT",
            SET,
            &h.alice,
            Some(json!({"timezone":"KST","expected_version":"0"}))
        )
        .await
        .0,
        400
    );
    let (status, set) = h
        .call(
            "PUT",
            SET,
            &h.alice,
            Some(json!({"timezone":"Asia/Seoul","expected_version":"0"})),
        )
        .await;
    assert_eq!(status, 200, "{set}");
    // 이번 기간은 그대로 UTC이고, 다음 기간(이번 기간이 끝나는 시각)부터 서울 시간이다.
    let tz = &set["budget_timezone"];
    assert_eq!(tz["timezone"], "UTC");
    assert_eq!(tz["pending"]["timezone"], "Asia/Seoul");
    assert_eq!(tz["pending"]["effective_from"], tz["period_end"]);
    assert_eq!(tz["version"], "1");
    // 다른 화면에서 먼저 바꿨으면 덮어쓰지 않는다.
    let (status, stale) = h
        .call(
            "PUT",
            SET,
            &h.alice,
            Some(json!({"timezone":"Europe/London","expected_version":"0"})),
        )
        .await;
    assert_eq!(
        (status, stale["error"]["code"].clone()),
        (409, json!("version_conflict"))
    );
    // 일반 사용자도 /api/me 에서 기준 시간대와 예약된 변경을 본다.
    let (_, me) = h.call("GET", "/api/me", &h.bob, None).await;
    assert_eq!(me["budget_timezone"]["pending"]["timezone"], "Asia/Seoul");
    // 지금 시간대로 되돌리면 예약이 취소된다.
    let (status, back) = h
        .call(
            "PUT",
            SET,
            &h.alice,
            Some(json!({"timezone":"UTC","expected_version":"1"})),
        )
        .await;
    assert_eq!(status, 200, "{back}");
    assert_eq!(back["effect"], "changed");
    assert_eq!(back["budget_timezone"]["pending"], json!(null));
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gw_audit_events WHERE operation='set_budget_timezone'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(audits, 2);
    h.db.close().await;
}
