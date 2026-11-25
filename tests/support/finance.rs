use sqlx::PgPool;
use uuid::Uuid;

pub async fn human(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO gw_principals(id,kind,issuer,subject,email,display_name) VALUES($1,'human','https://issuer.example',$2,'alice@example.test','Alice')")
        .bind(id).bind(id.to_string()).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO gw_policies(id,principal_id,allow_rules,period_kind,period_timezone,updated_by) VALUES($1,$2,$3,'calendar_month','UTC',$2)")
        .bind(Uuid::new_v4()).bind(id).bind(serde_json::json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages","count_tokens"]}])).execute(pool).await.unwrap();
    allow(pool, id).await;
    id
}

pub async fn current(pool: &PgPool, id: Uuid) -> m_gated::ledger::CurrentHuman {
    use sqlx::Row;
    let p=sqlx::query("SELECT p.version,q.id AS policy_id,q.version AS policy_version FROM gw_principals p JOIN gw_policies q ON q.principal_id=p.id AND q.key_id IS NULL WHERE p.id=$1").bind(id).fetch_one(pool).await.unwrap();
    m_gated::ledger::CurrentHuman {
        id,
        version: p.get("version"),
        policy_id: p.get("policy_id"),
        policy_version: p.get("policy_version"),
        admin: false,
    }
}

/// DB가 실제로 검증하는 과거 시각 fixture다. 제품 시계/실행 경로에 test clock을 추가하지 않는다.
pub async fn expired_copy(pool: &PgPool, source: Uuid, phase: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO gw_requests(id,principal_id,key_id,purpose,provider,requested_model,api,admitted_at,admit_expires_at,transport_deadline,execution_deadline,dispatch_authorized_at,lease_expires_at,executor_id,phase,accounting_state,pricing_snapshot,execution_snapshot,auth_config_digest) SELECT $1,r.principal_id,r.key_id,'root',r.provider,r.requested_model,r.api,n.t,n.t+interval '60 seconds',n.t+interval '895 seconds',n.t+interval '900 seconds',CASE WHEN $3='dispatching' THEN n.t+interval '1 second' END,CASE WHEN $3='dispatching' THEN n.t+interval '900 seconds' END,r.executor_id,$3,'pending',r.pricing_snapshot,r.execution_snapshot,r.auth_config_digest FROM gw_requests r CROSS JOIN (SELECT clock_timestamp()-interval '2 months'-interval '901 seconds' AS t) n WHERE r.id=$2")
        .bind(id).bind(source).bind(phase).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO gw_usage_totals(scope_id,period_start,period_end,timezone_snapshot) SELECT b.scope_id,date_trunc('month',r.admitted_at AT TIME ZONE b.timezone_snapshot) AT TIME ZONE b.timezone_snapshot,(date_trunc('month',r.admitted_at AT TIME ZONE b.timezone_snapshot)+interval '1 month') AT TIME ZONE b.timezone_snapshot,b.timezone_snapshot FROM gw_request_budgets b JOIN gw_requests r ON r.id=$1 WHERE b.request_id=$2 ON CONFLICT DO NOTHING")
        .bind(id).bind(source).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO gw_rate_windows(scope_id,lane,window_start,window_end,admitted_count) SELECT b.scope_id,b.rate_lane,to_timestamp(floor(extract(epoch FROM r.admitted_at)/60)*60),to_timestamp(floor(extract(epoch FROM r.admitted_at)/60)*60)+interval '60 seconds',1 FROM gw_request_budgets b JOIN gw_requests r ON r.id=$1 WHERE b.request_id=$2 ON CONFLICT DO NOTHING")
        .bind(id).bind(source).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO gw_request_budgets(request_id,scope_id,policy_version,policy_snapshot,period_start,period_end,timezone_snapshot,rate_lane,rate_start,rate_end,state) SELECT $1,b.scope_id,b.policy_version,b.policy_snapshot,date_trunc('month',r.admitted_at AT TIME ZONE b.timezone_snapshot) AT TIME ZONE b.timezone_snapshot,(date_trunc('month',r.admitted_at AT TIME ZONE b.timezone_snapshot)+interval '1 month') AT TIME ZONE b.timezone_snapshot,b.timezone_snapshot,b.rate_lane,to_timestamp(floor(extract(epoch FROM r.admitted_at)/60)*60),to_timestamp(floor(extract(epoch FROM r.admitted_at)/60)*60)+interval '60 seconds',CASE WHEN $3='dispatching' THEN 'active' ELSE 'admitted' END FROM gw_request_budgets b JOIN gw_requests r ON r.id=$1 WHERE b.request_id=$2")
        .bind(id).bind(source).bind(phase).execute(pool).await.unwrap();
    id
}

/// 시험 고정 Provider 키 id. 시험 client는 고정 키를 쓰므로, 이 행은 가상 키 연결과 인가 확인에만 쓰인다.
/// 암호문은 가짜라 풀 수 없다(Provider 키 캐시를 쓰는 시험에서는 이 행을 만들지 않는다).
#[allow(dead_code)]
pub fn synthetic_credential(provider: &str) -> Uuid {
    match provider {
        "anthropic" => Uuid::from_u128(0xa0),
        "openai" => Uuid::from_u128(0xb0),
        "deepseek" => Uuid::from_u128(0xd0),
        "typesafe" => Uuid::from_u128(0xe0),
        other => panic!("알 수 없는 Provider {other}"),
    }
}

/// 모든 Provider의 시험 고정 Provider 키를 (없으면) 만들고 이 주체에게 인가한다.
#[allow(dead_code)]
pub async fn allow(pool: &PgPool, principal: Uuid) {
    for provider in ["anthropic", "openai", "deepseek", "typesafe"] {
        let id = synthetic_credential(provider);
        sqlx::query("INSERT INTO gw_provider_credentials(id,provider,state,kek_id,nonce,ciphertext,fingerprint,hint,verified_at,created_by,label) VALUES($1,$2,'active',decode(repeat('00',8),'hex'),decode(repeat('00',12),'hex'),decode(repeat('00',36),'hex'),decode(repeat('5a',32),'hex'),'test',clock_timestamp(),$3,'synthetic') ON CONFLICT(id) DO NOTHING")
            .bind(id).bind(provider).bind(principal).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO gw_principal_credentials(principal_id,provider,credential_id,granted_by) VALUES($1,$2,$3,$1) ON CONFLICT DO NOTHING")
            .bind(principal).bind(provider).bind(id).execute(pool).await.unwrap();
    }
}

/// 가상 키 발급 입력에 허용한 API의 Provider마다 시험 고정 Provider 키를 고른다.
#[allow(dead_code)]
pub fn with_credentials(mut input: serde_json::Value) -> serde_json::Value {
    let providers: std::collections::BTreeSet<String> = input["grants"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|g| g["provider"].as_str().map(str::to_owned))
        .collect();
    input["provider_credentials"] = providers
        .iter()
        .map(|p| (p.clone(), serde_json::json!(synthetic_credential(p))))
        .collect::<serde_json::Map<_, _>>()
        .into();
    input
}

/// 추론용 가상 키를 발급해 원문을 돌려준다. 추론은 SSO 토큰이 아니라 가상 키로만 받는다.
/// 키는 주체의 지금 정책 grant 전체를 허용하고, Provider마다 시험 고정 Provider 키에 연결된다.
#[allow(dead_code)]
pub async fn virtual_key(
    store: &std::sync::Arc<m_gated::ledger::PgIdentityStore>,
    cfg: &m_gated::config::ValidatedConfig,
    principal: Uuid,
) -> String {
    use m_gated::ledger::{commands::CommandContext, management::Command};
    allow(store.pool(), principal).await;
    let grants: serde_json::Value = sqlx::query_scalar(
        "SELECT allow_rules FROM gw_policies WHERE principal_id=$1 AND key_id IS NULL",
    )
    .bind(principal)
    .fetch_one(store.pool())
    .await
    .unwrap();
    let server: Vec<serde_json::Value> = cfg
        .providers()
        .iter()
        .flat_map(|p| {
            p.profiles.iter().map(move |profile| {
                serde_json::json!({"provider":m_gated::ledger::finance::provider_name(p.provider),"model":profile.model,"capabilities":[m_gated::ledger::finance::api_name(profile.api)]})
            })
        })
        .collect();
    let server = serde_json::Value::Array(server);
    // 정책과 서버 설정이 모두 허용하는 API만 키에 담는다.
    let grants: Vec<serde_json::Value> = grants
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|g| {
            let caps: Vec<&serde_json::Value> = g["capabilities"]
                .as_array()?
                .iter()
                .filter(|c| {
                    server.as_array().unwrap().iter().any(|s| {
                        s["provider"] == g["provider"]
                            && s["model"] == g["model"]
                            && s["capabilities"].as_array().unwrap().contains(c)
                    })
                })
                .collect();
            (!caps.is_empty()).then(|| serde_json::json!({"provider":g["provider"],"model":g["model"],"capabilities":caps}))
        })
        .collect();
    let settings = m_gated::auth::AccessSettings::from_config(cfg);
    let ledger = m_gated::ledger::PgLedger::new(
        store.clone(),
        settings.clone(),
        m_gated::gateway::auth_config_digest(cfg),
    );
    let ctx = CommandContext {
        actor: principal,
        command_id: Uuid::new_v4(),
        method: "POST",
        path: "/api/keys",
        settings: &settings,
        grants: &server,
        digest: m_gated::gateway::auth_config_digest(cfg),
        ceiling: cfg.execution_ceiling(),
        execution_running: false,
    };
    let input = with_credentials(
        serde_json::json!({"name":"시험 추론 키","validity":"unlimited","grants":grants,"limits":{"budget":null,"rpm":null,"tpm":null,"count_rpm":null}}),
    );
    let issued = ledger
        .management_command(&ctx, Command::Issue(serde_json::from_value(input).unwrap()))
        .await
        .unwrap();
    issued.body.unwrap()["key"].as_str().unwrap().to_owned()
}

/// 요청 하나의 정산 결과: 회계 상태, 비용, 최종 usage의 정산 방식(source), 출력 토큰, 대체 사유.
/// 최종 usage 없이 끝난 요청의 정산(결정 0005)을 확인할 때 쓴다. 최종 usage가 없으면 뒤의 셋은 None이다.
#[allow(dead_code)]
pub type Settlement = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);
#[allow(dead_code)]
pub async fn settlement(pool: &PgPool, id: Uuid) -> Settlement {
    sqlx::query_as("SELECT r.accounting_state,r.cost_usd::text,e.normalized_usage->>'source',e.normalized_usage->>'output_tokens',e.normalized_usage->>'cutoff_fallback_reason' FROM gw_requests r LEFT JOIN gw_usage_events e ON e.request_id=r.id AND e.kind='final' WHERE r.id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}
/// 가장 늦게 받은 요청의 정산 결과.
#[allow(dead_code)]
pub async fn last_settlement(pool: &PgPool) -> Settlement {
    let id: Uuid =
        sqlx::query_scalar("SELECT id FROM gw_requests ORDER BY admitted_at DESC LIMIT 1")
            .fetch_one(pool)
            .await
            .unwrap();
    settlement(pool, id).await
}
/// 첫 응답 전에 끊겼거나 Provider가 HTTP 오류로 답한 요청의 정산: 0원, 토큰 0, 정산 방식 `no_response`.
#[allow(dead_code)]
pub fn no_response() -> Settlement {
    (
        "settled".into(),
        Some("0.000000000000".into()),
        Some("no_response".into()),
        Some("0".into()),
        None,
    )
}
/// 시작 event 뒤에 끊긴 Anthropic 스트림을 `received` 규칙(받은 출력만큼)으로 정산한 결과.
#[allow(dead_code)]
pub fn cutoff_received(cost: &str, output: u64) -> Settlement {
    (
        "settled".into(),
        Some(cost.into()),
        Some("cutoff_received".into()),
        Some(output.to_string()),
        None,
    )
}
