#[path = "support/finance.rs"]
#[allow(dead_code)]
mod finance;
#[path = "support/postgres.rs"]
mod postgres;
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
fn set(secret: &str, expected: Option<Uuid>) -> CredentialCommand {
    CredentialCommand::Set {
        provider: ProviderId::Anthropic,
        secret: Zeroizing::new(secret.to_owned()),
        expected,
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
        path: "/gateway/admin/provider-credentials/anthropic",
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
        .execute(&ctx(Uuid::new_v4()), set("sk-proj-not-anthropic-key", None))
        .await;
    assert!(matches!(bad, Err(ManagementError::Invalid)));
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    let rejected = pod_a
        .execute(
            &ctx(Uuid::new_v4()),
            set("sk-ant-synthetic-unknown-key-0009", None),
        )
        .await;
    assert!(matches!(rejected, Err(ManagementError::CredentialRejected)));
    fake.status.store(500, Ordering::SeqCst);
    let unverified = pod_a.execute(&ctx(Uuid::new_v4()), set(KEY_A, None)).await;
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
        .execute(&ctx(command), set(KEY_A, None))
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
        .execute(&ctx(command), set(KEY_A, None))
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(fake.calls.load(Ordering::SeqCst), calls);
    // 같은 command id에 다른 키는 충돌이다.
    assert!(matches!(
        pod_a.execute(&ctx(command), set(KEY_B, None)).await,
        Err(ManagementError::CommandConflict)
    ));
    // 다른 관리자가 이미 등록했다면 덮어쓰지 않는다.
    assert!(matches!(
        pod_a.execute(&ctx(Uuid::new_v4()), set(KEY_B, None)).await,
        Err(ManagementError::CredentialConflict(Some(id))) if id == first_id
    ));
    // 같은 키 재등록은 새 행 없이 noop.
    let noop = pod_a
        .execute(&ctx(Uuid::new_v4()), set(KEY_A, Some(first_id)))
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
    let active = pod_b.cache().get(ProviderId::Anthropic).unwrap();
    assert_eq!(active.id, first_id);
    assert_eq!(active.secret.as_str(), KEY_A);

    // 교체: 새 행이 active가 되고 이전 행은 암호문이 지워진다. 고정해 둔 이전 키는 계속 쓸 수 있다.
    let held = pod_b.cache().get(ProviderId::Anthropic).unwrap();
    let rotated = pod_a
        .execute(&ctx(Uuid::new_v4()), set(KEY_B, Some(first_id)))
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
            .get(ProviderId::Anthropic)
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
    assert!(wrong_root.cache().get(ProviderId::Anthropic).is_none());
    let listed = wrong_root.list().await.unwrap();
    assert_eq!(listed["providers"][0]["status"], "undecryptable");
    assert_eq!(
        pod_a.list().await.unwrap()["providers"][0]["status"],
        "active"
    );

    // 해제 후에는 모든 인스턴스가 미등록 상태가 된다.
    let retired = pod_a
        .execute(
            &ctx(Uuid::new_v4()),
            CredentialCommand::Retire {
                provider: ProviderId::Anthropic,
                expected: second_id,
            },
        )
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
            .execute(
                &ctx(Uuid::new_v4()),
                CredentialCommand::Retire {
                    provider: ProviderId::Anthropic,
                    expected: second_id,
                },
            )
            .await,
        Err(ManagementError::CredentialConflict(None))
    ));
    // 설정되지 않은 Provider는 대상이 아니다.
    assert!(matches!(
        pod_a
            .execute(
                &ctx(Uuid::new_v4()),
                CredentialCommand::Set {
                    provider: ProviderId::Openai,
                    secret: Zeroizing::new("sk-synthetic-openai-company-key".into()),
                    expected: None,
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
            "set_provider_credential",
            "set_provider_credential",
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
        pod_a.execute(&ctx(Uuid::new_v4()), set(KEY_A, None)).await,
        Err(ManagementError::Forbidden)
    ));
    db.close().await;
}
