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
    // 이 시험은 Provider 키가 하나도 없는 상태에서 시작한다(finance::human 이 만든 시험 고정 키를 지운다).
    sqlx::query("DELETE FROM gw_principal_credentials")
        .execute(db.store.pool())
        .await
        .unwrap();
    sqlx::query("DELETE FROM gw_provider_credentials")
        .execute(db.store.pool())
        .await
        .unwrap();
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
    let active = pod_b
        .cache()
        .get(ProviderId::Anthropic, Some(first_id))
        .unwrap();
    assert_eq!(active.id, first_id);
    assert_eq!(active.secret.as_str(), KEY_A);

    // 교체: 새 행이 active가 되고 이전 행은 암호문이 지워진다. 고정해 둔 이전 키는 계속 쓸 수 있다.
    let held = pod_b
        .cache()
        .get(ProviderId::Anthropic, Some(first_id))
        .unwrap();
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
            .get(ProviderId::Anthropic, Some(second_id))
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
            .get(ProviderId::Anthropic, Some(second_id))
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

/// 가상 키 발급 입력. 고른 Provider 키가 있으면 넣는다.
fn issue(chosen: Option<Uuid>) -> serde_json::Value {
    let mut body = json!({"name":"k","validity":"1_year","grants":[{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}],"limits":{"budget":null,"rpm":null,"tpm":null,"count_rpm":null}});
    if let Some(id) = chosen {
        body["provider_credentials"] = json!({"anthropic": id});
    }
    body
}
fn authorize(ids: &[Uuid], expected: &[Uuid]) -> serde_json::Value {
    json!({"provider":"anthropic","credential_ids":ids,"expected":{"credential_ids":expected}})
}
const CREDS: &str = "/api/admin/provider-credentials";

/// Provider 키 여러 개와 이름. 가상 키는 발급할 때 인가된 Provider 키를 반드시 직접 고르고, 요청은 그 키로 나간다.
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn labeled_keys_are_chosen_explicitly_and_route_requests() {
    let h = Http::new().await;
    let mut ids = Vec::new();
    for token in [&h.alice, &h.bob] {
        let (status, me) = h.call("GET", "/api/me", token, None).await;
        assert_eq!(status, 200);
        ids.push(id_of(&me["principal"]["id"]));
    }
    let (alice, bob) = (ids[0], ids[1]);
    sqlx::query("UPDATE gw_policies SET allow_rules=$1 WHERE key_id IS NULL")
        .bind(
            json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}]),
        )
        .execute(h.db.store.pool())
        .await
        .unwrap();

    // 이름을 붙여 키 두 개를 등록한다. 기본 키라는 개념은 없다.
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
    assert!(a["credential"].get("default").is_none());
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
    let b_id = id_of(&b["target"]["id"]);
    assert_eq!(h.call("GET", CREDS, &h.bob, None).await.0, 403);
    let (_, list) = h.call("GET", CREDS, &h.alice, None).await;
    assert_eq!(list["providers"][0]["status"], "active");
    let labels: Vec<_> = list["providers"][0]["credentials"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["label"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(labels, ["team-a", "team-b"]);
    h.service.refresh().await.unwrap();

    // 인가가 없으면 아무 키도 고를 수 없고, 키를 고르지 않으면 발급하지 않는다.
    let (status, err) = h
        .call("POST", "/api/keys", &h.bob, Some(issue(Some(a_id))))
        .await;
    assert_eq!(
        (status, err["error"]["code"].clone()),
        (403, json!("provider_credential_not_authorized"))
    );
    let (status, err) = h.call("POST", "/api/keys", &h.bob, Some(issue(None))).await;
    assert_eq!(
        (status, err["error"]["code"].clone()),
        (400, json!("provider_credential_required"))
    );
    // 허용한 API가 없는 가상 키는 연동할 Provider 키도 없다. 아무 요청도 보낼 수 없는 키이므로 발급하지 않는다.
    let mut empty = issue(None);
    empty["grants"] = json!([]);
    let (status, err) = h.call("POST", "/api/keys", &h.bob, Some(empty)).await;
    assert_eq!(
        (status, err["error"]["code"].clone()),
        (400, json!("provider_credential_required"))
    );
    let (status, list) = h.call("GET", "/api/keys", &h.bob, None).await;
    assert_eq!((status, list["items"].as_array().unwrap().len()), (200, 0));
    for (who, keys) in [(bob, vec![a_id]), (alice, vec![a_id, b_id])] {
        let (status, v) = h
            .call(
                "PUT",
                &format!("/api/admin/principals/{who}/provider-credentials"),
                &h.alice,
                Some(authorize(&keys, &[])),
            )
            .await;
        assert_eq!(status, 200, "{v}");
    }
    // 관리자도 인가 밖의 키는 고를 수 없다.
    let (status, _) = h
        .call("POST", "/api/keys", &h.bob, Some(issue(Some(b_id))))
        .await;
    assert_eq!(status, 403);
    let (status, bob_key) = h
        .call("POST", "/api/keys", &h.bob, Some(issue(Some(a_id))))
        .await;
    assert_eq!(status, 201, "{bob_key}");
    assert_eq!(
        bob_key["metadata"]["provider_credentials"][0]["label"],
        "team-a"
    );
    assert_eq!(
        bob_key["metadata"]["provider_credentials"][0]["authorized"],
        true
    );
    let bob_secret = bob_key["key"].as_str().unwrap().to_owned();
    let (status, alice_key) = h
        .call("POST", "/api/keys", &h.alice, Some(issue(Some(b_id))))
        .await;
    assert_eq!(status, 201, "{alice_key}");
    let alice_secret = alice_key["key"].as_str().unwrap().to_owned();
    let alice_key_id = id_of(&alice_key["metadata"]["id"]);
    let (_, keys) = h.call("GET", "/api/keys", &h.bob, None).await;
    assert_eq!(
        keys["items"][0]["provider_credentials"][0]["label"],
        "team-a"
    );
    assert_eq!(keys["items"][0]["provider_credentials"][0]["hint"], "0001");

    // 요청은 가상 키에 연결된 Provider 키로 나가고, 장부에 그 키가 남는다. SSO 토큰으로는 추론하지 않는다.
    assert_eq!(
        h.infer(&bob_secret).await,
        (200, Some(KEY_A.into()), Some(a_id))
    );
    assert_eq!(
        h.infer(&alice_secret).await,
        (200, Some(KEY_B.into()), Some(b_id))
    );
    let body = json!({"model":"synthetic-text","max_tokens":100,"messages":[{"role":"user","content":"x"}]});
    h.provider.calls.lock().await.clear();
    let (status, err) = h.call("POST", "/v1/messages", &h.alice, Some(body)).await;
    assert_eq!(
        (status, err["gateway_error_code"].clone()),
        (403, json!("virtual_key_required"))
    );
    assert!(h.provider.calls.lock().await.is_empty());
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

    // 이름 변경. 기본 키 지정은 없다.
    let path_b = format!("{CREDS}/anthropic/{b_id}");
    let (status, renamed) = h
        .call(
            "PATCH",
            &path_b,
            &h.alice,
            Some(json!({"expected_version":"1","label":"team-b-prod"})),
        )
        .await;
    assert_eq!(status, 200, "{renamed}");
    assert_eq!(renamed["credential"]["label"], "team-b-prod");
    let (status, _) = h
        .call(
            "PATCH",
            &path_b,
            &h.alice,
            Some(json!({"expected_version":"2","default":true})),
        )
        .await;
    assert_eq!(status, 400);
    let (status, stale) = h
        .call(
            "PATCH",
            &path_b,
            &h.alice,
            Some(json!({"expected_version":"1","label":"x"})),
        )
        .await;
    assert_eq!(
        (status, stale["error"]["code"].clone()),
        (409, json!("version_conflict"))
    );

    // 쓰고 있는 가상 키가 연결된 키는 옮길 키 없이 해제할 수 없다.
    let (status, busy) = h
        .call(
            "DELETE",
            &format!("{CREDS}/anthropic/{a_id}?expected_version=1"),
            &h.alice,
            None,
        )
        .await;
    assert_eq!(
        (status, busy["error"]["code"].clone()),
        (409, json!("credential_in_use")),
        "{busy}"
    );
    assert_eq!(busy["error"]["details"], json!({"bound_keys":"1"}));

    // 관리자는 가상 키 하나의 연결을 키 주인에게 인가된 키로 바꾼다.
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

    // 옮길 키를 정하면 해제되고, 연결된 가상 키 두 개와 인가가 함께 옮겨진다.
    let (status, retired) = h
        .call(
            "DELETE",
            &format!("{CREDS}/anthropic/{a_id}?expected_version=1&move_to={b_id}"),
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
    let (_, me) = h.call("GET", "/api/me", &h.bob, None).await;
    assert_eq!(
        me["provider_credentials"],
        json!([{"provider":"anthropic","credentials":[{"id":b_id,"label":"team-b-prod","hint":"0002"}]}])
    );

    // 키 교체: 이름·연결·인가가 새 행으로 옮겨진다.
    let (status, replaced) = h
        .call(
            "PUT",
            &path_b,
            &h.alice,
            Some(json!({"secret":KEY_A,"expected_version":"2"})),
        )
        .await;
    assert_eq!(status, 201, "{replaced}");
    assert_eq!(
        (
            replaced["credential"]["label"].clone(),
            replaced["credential"]["moved_keys"].clone()
        ),
        (json!("team-b-prod"), json!("2"))
    );
    let c_id = id_of(&replaced["target"]["id"]);
    h.service.refresh().await.unwrap();
    assert_eq!(
        h.infer(&bob_secret).await,
        (200, Some(KEY_A.into()), Some(c_id))
    );

    // 마지막 키는 옮길 곳 없이 해제할 수 있다. 연결이 지워져 그 뒤 요청은 송신 전에 거절한다.
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
    let (_, list) = h.call("GET", CREDS, &h.alice, None).await;
    assert_eq!(list["providers"][0]["status"], "missing");
    h.provider.calls.lock().await.clear();
    let body = json!({"model":"synthetic-text","max_tokens":100,"messages":[{"role":"user","content":"x"}]});
    let (status, err) = h
        .call("POST", "/v1/messages", &bob_secret, Some(body))
        .await;
    assert_eq!(
        (status, err["gateway_error_code"].clone()),
        (403, json!("provider_credential_unbound"))
    );
    assert!(h.provider.calls.lock().await.is_empty());
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
            "authorize_principal_credentials",
            "authorize_principal_credentials",
            "update_provider_credential",
            "bind_key_credential",
            "retire_provider_credential",
            "replace_provider_credential",
            "retire_provider_credential",
        ]
    );
    h.db.close().await;
}

/// Provider 키 인가: 인가된 키 안에서만 고르고, 인가를 풀면 연결된 가상 키를 막거나 옮긴다.
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn authorization_limits_provider_keys_and_blocks_deauthorized_keys() {
    let h = Http::new().await;
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
    // 관리자만 인가를 바꾼다.
    assert_eq!(
        h.call("PUT", &path, &h.bob, Some(authorize(&[b_id], &[])))
            .await
            .0,
        403
    );
    let (status, granted) = h
        .call("PUT", &path, &h.alice, Some(authorize(&[b_id], &[])))
        .await;
    assert_eq!(status, 200, "{granted}");
    assert_eq!(
        granted["authorization"]["provider_credentials"],
        json!([{"provider":"anthropic","credentials":[{"id":b_id,"label":"team-b","hint":"0002"}]}])
    );
    // 다른 관리자가 먼저 바꿨으면 덮어쓰지 않는다.
    let (status, stale) = h
        .call("PUT", &path, &h.alice, Some(authorize(&[a_id], &[])))
        .await;
    assert_eq!(
        (status, stale["error"]["code"].clone()),
        (409, json!("credential_authorization_conflict"))
    );
    assert_eq!(stale["error"]["details"], json!({"credential_ids":[b_id]}));
    let (_, me) = h.call("GET", "/api/me", &h.bob, None).await;
    assert_eq!(
        me["provider_credentials"][0]["credentials"][0]["label"],
        "team-b"
    );

    // 인가된 키로만 발급한다.
    let (status, denied) = h
        .call("POST", "/api/keys", &h.bob, Some(issue(Some(a_id))))
        .await;
    assert_eq!(
        (status, denied["error"]["code"].clone()),
        (403, json!("provider_credential_not_authorized"))
    );
    let mut secrets = Vec::new();
    let mut key_ids = Vec::new();
    for _ in 0..2 {
        let (status, issued) = h
            .call("POST", "/api/keys", &h.bob, Some(issue(Some(b_id))))
            .await;
        assert_eq!(status, 201, "{issued}");
        secrets.push(issued["key"].as_str().unwrap().to_owned());
        key_ids.push(id_of(&issued["metadata"]["id"]));
    }
    assert_eq!(
        h.infer(&secrets[0]).await,
        (200, Some(KEY_B.into()), Some(b_id))
    );
    let (status, err) = h
        .call(
            "PUT",
            &format!("/api/admin/keys/{}/provider-credential", key_ids[0]),
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

    // 연결된 가상 키가 있는 키의 인가를 풀려면 막을지 옮길지 골라야 한다.
    let (status, busy) = h
        .call("PUT", &path, &h.alice, Some(authorize(&[a_id], &[b_id])))
        .await;
    assert_eq!(
        (status, busy["error"]["code"].clone()),
        (409, json!("credential_authorization_in_use"))
    );
    assert_eq!(busy["error"]["details"]["bound_keys"], "2");
    let mut block = authorize(&[a_id], &[b_id]);
    block["bound_keys"] = json!("block");
    let (status, blocked) = h.call("PUT", &path, &h.alice, Some(block)).await;
    assert_eq!(status, 200, "{blocked}");
    assert_eq!(blocked["authorization"]["blocked_keys"], "2");
    let (status, _, _) = h.infer(&secrets[0]).await;
    assert_eq!(status, 403);
    assert!(h.provider.calls.lock().await.is_empty());
    let (_, body) = h
        .call("POST", "/v1/messages", &secrets[1], Some(json!({"model":"synthetic-text","max_tokens":100,"messages":[{"role":"user","content":"x"}]})))
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

    // 다시 인가하면 막힌 키가 원래 Provider 키로 다시 나간다.
    let (status, _) = h
        .call(
            "PUT",
            &path,
            &h.alice,
            Some(authorize(&[a_id, b_id], &[a_id])),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(h.infer(&secrets[0]).await.1.as_deref(), Some(KEY_B));
    // 옮기기를 고르면 연결된 가상 키가 인가된 키로 옮겨진다.
    let mut moving = authorize(&[a_id], &[a_id, b_id]);
    moving["bound_keys"] = json!({"move_to": a_id});
    let (status, moved) = h.call("PUT", &path, &h.alice, Some(moving)).await;
    assert_eq!(status, 200, "{moved}");
    assert_eq!(moved["authorization"]["moved_keys"], "2");
    assert_eq!(
        h.infer(&secrets[0]).await,
        (200, Some(KEY_A.into()), Some(a_id))
    );

    // Provider 키를 해제하면 인가도 옮길 키로 넘어간다.
    let mut back = authorize(&[b_id], &[a_id]);
    back["bound_keys"] = json!({"move_to": b_id});
    assert_eq!(h.call("PUT", &path, &h.alice, Some(back)).await.0, 200);
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
        json!([{"provider":"anthropic","credentials":[{"id":a_id,"label":"team-a","hint":"0001"}]}])
    );
    // 목록을 비우면 그 Provider를 쓸 수 없다. 연결된 가상 키가 있으면 막을지 옮길지 골라야 한다.
    let (status, busy) = h
        .call("PUT", &path, &h.alice, Some(authorize(&[], &[a_id])))
        .await;
    assert_eq!(
        (status, busy["error"]["code"].clone()),
        (409, json!("credential_authorization_in_use"))
    );
    let mut clear = authorize(&[], &[a_id]);
    clear["bound_keys"] = json!("block");
    let (status, cleared) = h.call("PUT", &path, &h.alice, Some(clear)).await;
    assert_eq!(status, 200, "{cleared}");
    assert_eq!(cleared["authorization"]["provider_credentials"], json!([]));
    let (status, _) = h
        .call("POST", "/api/keys", &h.bob, Some(issue(Some(a_id))))
        .await;
    assert_eq!(status, 403);
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
        let r = sqlx::query(&format!(
            "SELECT period_start::text AS s,period_end::text AS e,timezone FROM {} b",
            m_gated::ledger::settings::budget_period_sql("$1::timestamptz")
        ))
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

/// 「막기」로 막아 둔 가상 키는 그 Provider 키를 해제해도 대신 쓸 키로 옮겨지지 않고 계속 막힌다.
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn retiring_a_key_keeps_blocked_virtual_keys_blocked() {
    let h = Http::new().await;
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
    assert_eq!(
        h.call("PUT", &path, &h.alice, Some(authorize(&[b_id], &[])))
            .await
            .0,
        200
    );
    let (status, issued) = h
        .call("POST", "/api/keys", &h.bob, Some(issue(Some(b_id))))
        .await;
    assert_eq!(status, 201, "{issued}");
    let secret = issued["key"].as_str().unwrap().to_owned();
    // b의 인가를 풀면서 연결된 가상 키를 막는다. bob은 a만 쓸 수 있다.
    let mut block = authorize(&[a_id], &[b_id]);
    block["bound_keys"] = json!("block");
    assert_eq!(h.call("PUT", &path, &h.alice, Some(block)).await.0, 200);
    assert_eq!(h.infer(&secret).await.0, 403);
    // b를 해제하면서 a를 대신 쓸 키로 골라도, 막힌 가상 키는 옮겨지지 않는다.
    // 옮길 연결이 없으므로 대신 쓸 키 없이도 해제할 수 있다.
    let (status, retired) = h
        .call(
            "DELETE",
            &format!("{CREDS}/anthropic/{b_id}?expected_version=1&move_to={a_id}"),
            &h.alice,
            None,
        )
        .await;
    assert_eq!(status, 200, "{retired}");
    assert_eq!(retired["credential"]["moved_keys"], "0");
    h.service.refresh().await.unwrap();
    let (status, _, _) = h.infer(&secret).await;
    assert_eq!(
        status, 403,
        "막아 둔 가상 키가 해제를 거쳐 다시 살아나면 안 된다"
    );
    assert!(h.provider.calls.lock().await.is_empty());
    let (_, keys) = h.call("GET", "/api/keys", &h.bob, None).await;
    let binding = &keys["items"][0]["provider_credentials"][0];
    assert_eq!(
        (
            binding["credential_id"].clone(),
            binding["state"].clone(),
            binding["authorized"].clone()
        ),
        (json!(b_id), json!("retired"), json!(false))
    );
    // 관리자가 연결을 a로 바꾸면 다시 쓸 수 있다.
    let (status, rebound) = h
        .call(
            "PUT",
            &format!(
                "/api/admin/keys/{}/provider-credential",
                id_of(&issued["metadata"]["id"])
            ),
            &h.alice,
            Some(
                json!({"provider":"anthropic","credential_id":a_id,"expected_credential_id":b_id}),
            ),
        )
        .await;
    assert_eq!(status, 200, "{rebound}");
    assert_eq!(
        h.infer(&secret).await,
        (200, Some(KEY_A.into()), Some(a_id))
    );
    h.db.close().await;
}

/// Provider 키 하나를 여러 사용자에게 한 번에 인가한다. 기존 인가는 그대로 두고 이 키만 더하며,
/// 하나라도 잘못되면 아무도 인가하지 않는다. 막아 둔 가상 키는 그 키가 다시 인가되면 다시 쓸 수 있다.
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn one_key_is_granted_to_many_principals_at_once() {
    let h = Http::new().await;
    let mut ids = Vec::new();
    for token in [&h.alice, &h.bob] {
        let (status, me) = h.call("GET", "/api/me", token, None).await;
        assert_eq!(status, 200);
        ids.push(id_of(&me["principal"]["id"]));
    }
    let (alice, bob) = (ids[0], ids[1]);
    sqlx::query("UPDATE gw_policies SET allow_rules=$1 WHERE key_id IS NULL")
        .bind(
            json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}]),
        )
        .execute(h.db.store.pool())
        .await
        .unwrap();
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
    // bob은 b만 인가받은 상태에서 시작한다.
    let bob_path = format!("/api/admin/principals/{bob}/provider-credentials");
    assert_eq!(
        h.call("PUT", &bob_path, &h.alice, Some(authorize(&[b_id], &[])))
            .await
            .0,
        200
    );
    let grant_path = format!("{CREDS}/anthropic/{a_id}");
    let grant = |who: &[Uuid]| json!({"principal_ids": who});
    let holders = |list: &serde_json::Value, id: Uuid| -> Vec<Uuid> {
        list["providers"][0]["credentials"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| id_of(&c["id"]) == id)
            .unwrap()["authorized_principal_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(id_of)
            .collect()
    };
    let mut both = vec![alice, bob];
    both.sort();

    // 관리자가 아니면 인가할 수 없다. 빈 목록·중복·없는 사용자는 거절하고 아무도 인가하지 않는다.
    assert_eq!(
        h.call("POST", &grant_path, &h.bob, Some(grant(&[bob])))
            .await
            .0,
        403
    );
    for (body, expected) in [
        (grant(&[]), 400),
        (grant(&[bob, bob]), 400),
        (grant(&[alice, Uuid::new_v4()]), 404),
        (json!({"principal_ids":[alice],"extra":1}), 400),
    ] {
        let (status, v) = h.call("POST", &grant_path, &h.alice, Some(body)).await;
        assert_eq!(status, expected, "{v}");
    }
    let (_, list) = h.call("GET", CREDS, &h.alice, None).await;
    assert!(holders(&list, a_id).is_empty());

    // a를 alice와 bob에게 한 번에 인가한다. bob의 기존 인가(b)는 그대로다.
    let (status, v) = h
        .call("POST", &grant_path, &h.alice, Some(grant(&[alice, bob])))
        .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["effect"], "changed");
    assert_eq!(v["target"]["kind"], "credential");
    assert_eq!(v["credential"]["granted_principal_ids"], json!(both));
    let (_, list) = h.call("GET", CREDS, &h.alice, None).await;
    assert_eq!(holders(&list, a_id), both);
    assert_eq!(holders(&list, b_id), vec![bob]);
    let (_, me) = h.call("GET", "/api/me", &h.bob, None).await;
    let mut mine: Vec<Uuid> = me["provider_credentials"][0]["credentials"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| id_of(&c["id"]))
        .collect();
    mine.sort();
    let mut keys = vec![a_id, b_id];
    keys.sort();
    assert_eq!(mine, keys);

    // 이미 인가된 사용자만 다시 보내면 바뀌는 것이 없다.
    let (status, v) = h
        .call("POST", &grant_path, &h.alice, Some(grant(&[bob])))
        .await;
    assert_eq!((status, v["effect"].clone()), (200, json!("noop")));
    let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_audit_events WHERE operation='authorize_principal_credentials' AND target_kind='credential' AND target_id=$1")
        .bind(a_id)
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    assert_eq!(audits, 1);

    // 해제된 Provider 키는 인가할 수 없다.
    let (status, _) = h
        .call(
            "DELETE",
            &format!("{CREDS}/anthropic/{b_id}?expected_version=1"),
            &h.alice,
            None,
        )
        .await;
    assert_eq!(status, 200);
    let (status, _) = h
        .call(
            "POST",
            &format!("{CREDS}/anthropic/{b_id}"),
            &h.alice,
            Some(grant(&[alice])),
        )
        .await;
    assert_eq!(status, 404);
}

/// 관리자의 사용자 목록은 이메일 일부로 찾을 수 있다. 대소문자를 가리지 않고 %와 _는 글자 그대로 본다.
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn principal_list_is_searched_by_part_of_the_email() {
    let h = Http::new().await;
    let mut people = Vec::new();
    for token in [&h.alice, &h.bob] {
        let (status, me) = h.call("GET", "/api/me", token, None).await;
        assert_eq!(status, 200);
        people.push((
            id_of(&me["principal"]["id"]),
            me["principal"]["email"].as_str().unwrap().to_owned(),
        ));
    }
    let (bob, bob_email) = people[1].clone();
    assert_ne!(people[0].1, bob_email);
    let found = |v: &serde_json::Value| -> Vec<Uuid> {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| id_of(&p["id"]))
            .collect()
    };
    // 이메일 전체, 대문자로 쓴 앞부분(@ 앞) 모두 bob 하나만 찾는다.
    let local = bob_email.split('@').next().unwrap().to_uppercase();
    for needle in [bob_email.as_str(), local.as_str()] {
        let (status, v) = h
            .call(
                "GET",
                &format!("/api/admin/principals?email={needle}"),
                &h.alice,
                None,
            )
            .await;
        assert_eq!(status, 200, "{v}");
        assert_eq!(found(&v), vec![bob], "{needle}");
    }
    // %와 _는 아무 글자나 뜻하지 않는다. 맞는 이메일이 없으면 빈 목록이다.
    for needle in ["%25", "_", "no-such-user"] {
        let (status, v) = h
            .call(
                "GET",
                &format!("/api/admin/principals?email={needle}"),
                &h.alice,
                None,
            )
            .await;
        assert_eq!(status, 200, "{v}");
        assert!(found(&v).is_empty(), "{needle}");
    }
    // 빈 값은 거절한다. 관리자가 아니면 목록을 볼 수 없다.
    let (status, _) = h
        .call("GET", "/api/admin/principals?email=", &h.alice, None)
        .await;
    assert_eq!(status, 400);
    let (status, _) = h
        .call(
            "GET",
            &format!("/api/admin/principals?email={local}"),
            &h.bob,
            None,
        )
        .await;
    assert_eq!(status, 403);
}

/// 가상 키 수정: 이름·허용 API·Provider 키 연결을 키 주인과 관리자가 바꾼다. 유효기간·만료일·한도는 바꿀 수 없다.
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn virtual_key_name_grants_and_provider_keys_are_edited_after_issue() {
    let h = Http::new().await;
    let mut ids = Vec::new();
    for token in [&h.alice, &h.bob] {
        let (status, me) = h.call("GET", "/api/me", token, None).await;
        assert_eq!(status, 200);
        ids.push(id_of(&me["principal"]["id"]));
    }
    let (alice, bob) = (ids[0], ids[1]);
    let messages =
        json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}]);
    sqlx::query("UPDATE gw_policies SET allow_rules=$1 WHERE key_id IS NULL")
        .bind(&messages)
        .execute(h.db.store.pool())
        .await
        .unwrap();
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
    let (status, _) = h
        .call(
            "POST",
            &format!("{CREDS}/anthropic/{a_id}"),
            &h.alice,
            Some(json!({"principal_ids":[alice, bob]})),
        )
        .await;
    assert_eq!(status, 200);
    let (status, issued) = h
        .call("POST", "/api/keys", &h.bob, Some(issue(Some(a_id))))
        .await;
    assert_eq!(status, 201, "{issued}");
    let key = id_of(&issued["metadata"]["id"]);
    let secret = issued["key"].as_str().unwrap().to_owned();
    let path = format!("/api/keys/{key}");
    let edit = |version: &str, name: &str, grants: &serde_json::Value, cred: Uuid| json!({"expected_version":version,"name":name,"grants":grants,"provider_credentials":{"anthropic":cred}});
    let fixed = |pool: sqlx::PgPool| async move {
        sqlx::query_as::<_, (String, Option<String>, Option<i64>)>("SELECT k.validity_preset,k.expires_at::text,p.rpm_limit FROM gw_virtual_keys k JOIN gw_policies p ON p.key_id=k.id WHERE k.id=$1")
            .bind(key)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let before = fixed(h.db.store.pool().clone()).await;

    // 키 주인이 이름만 바꾼다. 버전이 오르고 요청은 그대로 된다.
    let (status, v) = h
        .call(
            "PATCH",
            &path,
            &h.bob,
            Some(edit("1", "새 이름", &messages, a_id)),
        )
        .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        (v["effect"].clone(), v["result_version"].clone()),
        (json!("changed"), json!("2"))
    );
    assert_eq!(h.infer(&secret).await.0, 200);
    // 같은 내용을 다시 보내면 바뀌는 것이 없다. 옛 버전으로 보내면 충돌이다.
    let (status, v) = h
        .call(
            "PATCH",
            &path,
            &h.bob,
            Some(edit("2", "새 이름", &messages, a_id)),
        )
        .await;
    assert_eq!((status, v["effect"].clone()), (200, json!("noop")));
    let (status, v) = h
        .call(
            "PATCH",
            &path,
            &h.bob,
            Some(edit("1", "다른 이름", &messages, a_id)),
        )
        .await;
    assert_eq!(
        (status, v["error"]["code"].clone()),
        (409, json!("version_conflict"))
    );

    // 허용 API를 고치면 가상 키 행과 정책 행에 함께 저장되고, 그 즉시 요청에 적용된다.
    // 저장된 허용 API를 다른 모델로 바꿔 두면 요청이 거절되고, 수정으로 되돌리면 다시 된다.
    let other =
        json!([{"provider":"anthropic","model":"retired-model","capabilities":["messages"]}]);
    sqlx::query("UPDATE gw_virtual_keys SET grant_snapshot=$2 WHERE id=$1")
        .bind(key)
        .bind(&other)
        .execute(h.db.store.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE gw_policies SET allow_rules=$2 WHERE key_id=$1")
        .bind(key)
        .bind(&other)
        .execute(h.db.store.pool())
        .await
        .unwrap();
    assert_eq!(h.infer(&secret).await.0, 403);
    let (status, v) = h
        .call(
            "PATCH",
            &path,
            &h.bob,
            Some(edit("2", "새 이름", &messages, a_id)),
        )
        .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["result_version"], "3");
    let stored: (serde_json::Value, serde_json::Value) = sqlx::query_as("SELECT k.grant_snapshot,p.allow_rules FROM gw_virtual_keys k JOIN gw_policies p ON p.key_id=k.id WHERE k.id=$1")
        .bind(key)
        .fetch_one(h.db.store.pool())
        .await
        .unwrap();
    assert_eq!(stored, (messages.clone(), messages.clone()));
    assert_eq!(h.infer(&secret).await.0, 200);

    // 인가받지 않은 Provider 키로는 바꿀 수 없다. 허용 API가 없거나, Provider 키를 고르지 않거나, 정책 밖의 API면 거절한다.
    let (status, v) = h
        .call(
            "PATCH",
            &path,
            &h.bob,
            Some(edit("3", "새 이름", &messages, b_id)),
        )
        .await;
    assert_eq!(
        (status, v["error"]["code"].clone()),
        (403, json!("provider_credential_not_authorized"))
    );
    for (body, expected) in [
        (
            json!({"expected_version":"3","name":"새 이름","grants":[],"provider_credentials":{}}),
            (400, "provider_credential_required"),
        ),
        (
            json!({"expected_version":"3","name":"새 이름","grants":messages,"provider_credentials":{}}),
            (400, "provider_credential_required"),
        ),
        (
            json!({"expected_version":"3","name":"새 이름","grants":[{"provider":"anthropic","model":"no-such-model","capabilities":["messages"]}],"provider_credentials":{"anthropic":a_id}}),
            (403, "access_denied"),
        ),
        (
            json!({"expected_version":"3","name":"","grants":messages,"provider_credentials":{"anthropic":a_id}}),
            (400, "invalid_request"),
        ),
        (
            json!({"expected_version":"3","name":"새 이름","grants":messages,"provider_credentials":{"anthropic":a_id},"limits":{"rpm":1}}),
            (400, "invalid_request"),
        ),
        (
            json!({"expected_version":"3","name":"새 이름","grants":messages,"provider_credentials":{"anthropic":a_id},"validity":"3_years"}),
            (400, "invalid_request"),
        ),
    ] {
        let (status, v) = h.call("PATCH", &path, &h.bob, Some(body)).await;
        assert_eq!(
            (status, v["error"]["code"].as_str().unwrap()),
            expected,
            "{v}"
        );
    }
    // 거절된 수정은 아무것도 바꾸지 않는다.
    assert_eq!(h.infer(&secret).await.0, 200);

    // 관리자는 다른 사람의 가상 키도 고친다. bob에게 b를 인가한 뒤 연결을 b로 바꾼다.
    let (status, _) = h
        .call(
            "POST",
            &format!("{CREDS}/anthropic/{b_id}"),
            &h.alice,
            Some(json!({"principal_ids":[bob]})),
        )
        .await;
    assert_eq!(status, 200);
    let (status, v) = h
        .call(
            "PATCH",
            &path,
            &h.alice,
            Some(edit("3", "관리자가 고친 이름", &messages, b_id)),
        )
        .await;
    assert_eq!(status, 200, "{v}");
    let (_, list) = h.call("GET", "/api/keys", &h.bob, None).await;
    let item = &list["items"][0];
    assert_eq!(item["name"], "관리자가 고친 이름");
    assert_eq!(item["version"], "4");
    assert_eq!(item["grants"], messages);
    assert_eq!(
        id_of(&item["provider_credentials"][0]["credential_id"]),
        b_id
    );
    assert_eq!(h.infer(&secret).await.0, 200);
    // 유효기간·만료일·한도는 수정으로 바뀌지 않는다.
    assert_eq!(fixed(h.db.store.pool().clone()).await, before);

    // 다른 사용자의 가상 키는 관리자가 아니면 고칠 수 없다. 회수된 가상 키는 고치지 않는다.
    let (status, own) = h
        .call("POST", "/api/keys", &h.alice, Some(issue(Some(a_id))))
        .await;
    assert_eq!(status, 201, "{own}");
    let alice_key = id_of(&own["metadata"]["id"]);
    let (status, _) = h
        .call(
            "PATCH",
            &format!("/api/keys/{alice_key}"),
            &h.bob,
            Some(edit("1", "남의 키", &messages, a_id)),
        )
        .await;
    assert_eq!(status, 404);
    let (status, _) = h
        .call(
            "DELETE",
            &format!("{path}?expected_version=4"),
            &h.bob,
            None,
        )
        .await;
    assert_eq!(status, 204);
    let (status, _) = h
        .call(
            "PATCH",
            &path,
            &h.bob,
            Some(edit("5", "회수된 키", &messages, b_id)),
        )
        .await;
    assert_eq!(status, 400);
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gw_audit_events WHERE operation='update_key' AND target_id=$1",
    )
    .bind(key)
    .fetch_one(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(audits, 3);
}

/// 관리자 화면이 쓰는 두 값: 요청 기록에 있는 모델 이름 목록과, 가상 키 목록의 키 주인 정보.
#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn admin_reads_request_models_and_key_owner() {
    let h = Http::new().await;
    let (_, me) = h.call("GET", "/api/me", &h.bob, None).await;
    let (bob, bob_email) = (
        id_of(&me["principal"]["id"]),
        me["principal"]["email"].as_str().unwrap().to_owned(),
    );
    assert_eq!(h.call("GET", "/api/me", &h.alice, None).await.0, 200);
    let messages =
        json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}]);
    sqlx::query("UPDATE gw_policies SET allow_rules=$1 WHERE key_id IS NULL")
        .bind(&messages)
        .execute(h.db.store.pool())
        .await
        .unwrap();
    let (_, a) = h
        .call(
            "POST",
            &format!("{CREDS}/anthropic"),
            &h.alice,
            Some(json!({"label":"team-a","secret":KEY_A})),
        )
        .await;
    let a_id = id_of(&a["target"]["id"]);
    h.service.refresh().await.unwrap();
    let (status, _) = h
        .call(
            "POST",
            &format!("{CREDS}/anthropic/{a_id}"),
            &h.alice,
            Some(json!({"principal_ids":[bob]})),
        )
        .await;
    assert_eq!(status, 200);

    // 요청 기록이 없으면 빈 목록이다. 관리자만 읽고, 조회 조건은 받지 않는다.
    let models = "/api/admin/requests/models";
    let (status, v) = h.call("GET", models, &h.alice, None).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["items"], json!([]));
    assert_eq!(h.call("GET", models, &h.bob, None).await.0, 403);
    let with_query = format!("{models}?limit=1");
    assert_eq!(h.call("GET", &with_query, &h.alice, None).await.0, 400);

    let (status, issued) = h
        .call("POST", "/api/keys", &h.bob, Some(issue(Some(a_id))))
        .await;
    assert_eq!(status, 201, "{issued}");
    let secret = issued["key"].as_str().unwrap().to_owned();
    assert_eq!(h.infer(&secret).await.0, 200);
    assert_eq!(h.infer(&secret).await.0, 200);

    // 같은 모델로 온 요청이 여러 개여도 한 번만 나온다.
    let (status, v) = h.call("GET", models, &h.alice, None).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        v["items"],
        json!([{"provider":"anthropic","model":"synthetic-text"}])
    );

    // 내 사용량의 모델 후보는 내 요청 기록만 본다. 요청을 보낸 사용자에게만 나오고, 조회 조건은 받지 않는다.
    let mine = "/api/usage/models";
    let (status, v) = h.call("GET", mine, &h.bob, None).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(
        v["items"],
        json!([{"provider":"anthropic","model":"synthetic-text"}])
    );
    let (status, v) = h.call("GET", mine, &h.alice, None).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["items"], json!([]));
    let with_query = format!("{mine}?limit=1");
    assert_eq!(h.call("GET", &with_query, &h.bob, None).await.0, 400);

    // 가상 키 목록의 키 주인은 ID와 함께 이름·이메일로도 온다.
    let (status, v) = h.call("GET", "/api/admin/keys", &h.alice, None).await;
    assert_eq!(status, 200, "{v}");
    let owner = &v["items"][0]["owner"];
    assert_eq!(v["items"][0]["principal_id"], json!(bob));
    assert_eq!(owner["kind"], "human");
    assert_eq!(owner["email"], json!(bob_email));
    assert!(
        owner["display_name"]
            .as_str()
            .is_some_and(|n| !n.is_empty())
    );

    // 가상 키의 한도는 키 주인의 정책 한도를 넘을 수 없다. 최대값은 /api/me 로 알려 주고, 넘으면 발급을 거절한다.
    sqlx::query("UPDATE gw_policies SET rpm_limit=5 WHERE key_id IS NULL")
        .execute(h.db.store.pool())
        .await
        .unwrap();
    let (_, me) = h.call("GET", "/api/me", &h.bob, None).await;
    assert_eq!(me["key_limit_maximum"]["rpm"], json!(5), "{me}");
    assert_eq!(me["key_limit_maximum"]["tpm"], json!(null));
    let mut over = issue(Some(a_id));
    over["limits"]["rpm"] = json!(6);
    let (status, v) = h.call("POST", "/api/keys", &h.bob, Some(over)).await;
    assert_eq!(status, 422, "{v}");
    assert_eq!(v["error"]["code"], "limit_exceeds_maximum");
    let mut within = issue(Some(a_id));
    within["limits"]["rpm"] = json!(5);
    let (status, v) = h.call("POST", "/api/keys", &h.bob, Some(within)).await;
    assert_eq!(status, 201, "{v}");
    h.db.close().await;
}
