use sqlx::PgPool;
use uuid::Uuid;

pub async fn human(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO gw_principals(id,kind,issuer,subject,email,display_name) VALUES($1,'human','https://issuer.example',$2,'alice@example.test','Alice')")
        .bind(id).bind(id.to_string()).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO gw_policies(id,principal_id,allow_rules,budget_mode,period_kind,period_timezone,updated_by) VALUES($1,$2,$3,'actual','calendar_month','UTC',$2)")
        .bind(Uuid::new_v4()).bind(id).bind(serde_json::json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages","count_tokens"]}])).execute(pool).await.unwrap();
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
    sqlx::query("INSERT INTO gw_request_budgets(request_id,scope_id,policy_version,policy_snapshot,period_start,period_end,timezone_snapshot,rate_lane,rate_start,rate_end,budget_mode,reservation_usd,remaining_reservation_usd,reserved_tokens,remaining_reserved_tokens,state) SELECT $1,b.scope_id,b.policy_version,b.policy_snapshot,date_trunc('month',r.admitted_at AT TIME ZONE b.timezone_snapshot) AT TIME ZONE b.timezone_snapshot,(date_trunc('month',r.admitted_at AT TIME ZONE b.timezone_snapshot)+interval '1 month') AT TIME ZONE b.timezone_snapshot,b.timezone_snapshot,b.rate_lane,to_timestamp(floor(extract(epoch FROM r.admitted_at)/60)*60),to_timestamp(floor(extract(epoch FROM r.admitted_at)/60)*60)+interval '60 seconds',b.budget_mode,b.reservation_usd,b.remaining_reservation_usd,b.reserved_tokens,b.remaining_reserved_tokens,CASE WHEN $3='dispatching' THEN 'active' ELSE 'admitted' END FROM gw_request_budgets b JOIN gw_requests r ON r.id=$1 WHERE b.request_id=$2")
        .bind(id).bind(source).bind(phase).execute(pool).await.unwrap();
    id
}
