//! 토큰 대조(결정 0005 23~27번)와 사후 보정(11번).
//! - 합성 Admin API가 API 키 목록과 사용량 보고서(1시간·1분 칸)를 낸다. 조회는 `TokenCheck::poll_once`로 한 번씩 돌린다.
//! - 결과는 조회가 Admin API에 보낸 요청, 관리 API(`GET /api/admin/token-check`), 장부(요청 금액, 예산 합계,
//!   usage 기록, 감사 기록)로 확인한다.
//! - Provider 키는 관리 API로 등록해 복호화된 캐시에 둔다. 요청은 장부나 게이트웨이로 만든 뒤 정한 시각으로 옮긴다.
//! - 시각은 epoch 초다. 1시간 칸은 끝난 지 10분이 지난 정시까지 받으므로 그보다 이른 칸에 둔다.
//!   사후 보정은 서버 시계로 끝난 지 10분~2시간인 요청을, 1분 칸 기준 확인은 30분이 지난 분을 보므로 그 구간에 둔다.
use super::*;
use m_gated::{
    credentials::{CredentialCache, CredentialService},
    ledger::finance::{FinalOutcome, NoUsage, StartUsage},
    providers::transport::NativeClient,
    runtime::{Dependency, Runtime},
    token_check::TokenCheck,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Mutex, time::Duration};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

/// 시험 Admin API 키. 형식(`sk-ant-admin01-`)만 맞춘 합성 값이다.
const ADMIN_KEY: &str = "sk-ant-admin01-synthetic-admin-key-0001";
const KEYS: &str = "/v1/organizations/api_keys";
const USAGE: &str = "/v1/organizations/usage_report/messages";
const SETTINGS: &str = "/api/admin/settings";
const RULE: &str = "/api/admin/settings/cutoff-rule";
/// Provider 키 원문. 앞 16글자와 끝 4글자가 서로 다르다.
const KEY_ONE: &str = "sk-ant-api03-K1aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-k1AA";
const KEY_TWO: &str = "sk-ant-api03-K2bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-k2BB";
const KEY_LONE: &str = "sk-ant-api03-K3cccccccccccccccccccccccccccccccccccccccc-k3CC";
const KEY_TWIN: &str = "sk-ant-api03-K4dddddddddddddddddddddddddddddddddddddddd-k4DD";
/// 게이트웨이에 없는 조직의 다른 API 키.
const OUTSIDE_HINT: &str = "sk-ant-api03-Zzz...zzZZ";
const MINUTE: i64 = 60;
const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;
/// 합성 API 키 목록의 한 페이지 크기.
const KEY_PAGE: usize = 2;

/// API 키 목록에 나오는 앞뒤 글자(예: `sk-ant-api03-R2D...igAA`).
fn hint(secret: &str) -> String {
    format!("{}...{}", &secret[..16], &secret[secret.len() - 4..])
}
fn rfc3339(t: i64) -> String {
    OffsetDateTime::from_unix_timestamp(t)
        .unwrap()
        .format(&Rfc3339)
        .unwrap()
}
fn epoch(s: &str) -> i64 {
    OffsetDateTime::parse(s, &Rfc3339).unwrap().unix_timestamp()
}
/// 조회가 받는 마지막 1시간 칸의 끝: 끝난 지 10분이 지난 마지막 정시(제품과 같은 계산).
fn settled_hour() -> i64 {
    (OffsetDateTime::now_utc().unix_timestamp() - 600).div_euclid(HOUR) * HOUR
}
/// 서버 시계의 지금. 사후 보정은 서버 시계로 구간(끝난 지 10분~2시간, 기준 확인은 30분)을 정한다.
fn now_utc() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}
/// `now`에서 `ago`분 전이 속한 분의 시작.
fn minute_ago(now: i64, ago: i64) -> i64 {
    (now - ago * MINUTE).div_euclid(MINUTE) * MINUTE
}
fn ended(
    outcome: FinalOutcome,
    reason: &'static str,
    http_status: Option<u16>,
) -> FinalObservation {
    FinalObservation {
        outcome,
        usage: None,
        counted_input_tokens: None,
        reason,
        http_status,
        provider_request_id: None,
    }
}

/// 시험 설정. alice는 관리자, bob은 일반 사용자다. `admin_key`이면 Admin API 키를 넣어 토큰 대조를 켠다.
/// synthetic-text 가격(백만 토큰당 USD): 입력 1, 출력 2, 캐시 읽기 0.1, 캐시 쓰기 5분 1.25, 1시간 2.
fn token_check_configuration(admin_key: bool) -> config::ValidatedConfig {
    let output = "          output: '2.000000000000'\n";
    assert!(support::YAML.contains(output));
    let yaml = support::YAML.replace(
        output,
        &format!(
            "{output}          cache_read: '0.100000000000'\n          cache_write_5m: '1.250000000000'\n          cache_write_1h: '2.000000000000'\n"
        ),
    );
    let mut env = support::environment();
    // alice가 고정 관리자다. bob은 일반 사용자로 접근 목록에 넣는다(`Check::new`).
    env.insert("ADMIN_EMAIL".into(), "alice@example.test".into());
    if admin_key {
        env.insert("ANTHROPIC_ADMIN_API_KEY".into(), ADMIN_KEY.into());
    }
    config::load(ConfigInputs {
        yaml: &yaml,
        file_environment: None,
        environment: &env,
    })
    .unwrap()
}

/// 합성 Admin API가 받은 요청 하나.
#[derive(Clone, Debug)]
struct AdminCall {
    path: String,
    query: Vec<(String, String)>,
    key: Option<String>,
    version: Option<String>,
}
impl AdminCall {
    fn one(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    fn all(&self, name: &str) -> Vec<&str> {
        self.query
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }
}
/// 합성 Anthropic Admin API. 정해 둔 API 키 목록과 사용량 보고서로 답하고, 받은 요청을 남긴다.
/// - API 키 목록은 `KEY_PAGE`개씩 나눠 `after_id`로 잇는다.
/// - 사용량 보고서는 요청한 구간의 모든 칸을 `limit`개씩 `page`로 잇는다. 빈 칸도 낸다.
/// - Provider 키 등록 확인(`/v1/models`)에는 늘 200으로 답하고 남기지 않는다.
#[derive(Default)]
struct FakeAdmin {
    calls: Mutex<Vec<AdminCall>>,
    /// (API 키 ID, 앞뒤 글자).
    keys: Mutex<Vec<(String, Option<String>)>>,
    /// (칸 너비 초, 칸 시작, 결과 한 줄).
    usage: Mutex<Vec<(i64, i64, Value)>>,
    /// 경로마다 강제할 응답 상태. 200이면 JSON이 아닌 본문을 낸다.
    failures: Mutex<BTreeMap<&'static str, u16>>,
    /// 이 시각(epoch 초)부터의 칸은 아직 채워지지 않은 것처럼 돌려주지 않는다.
    horizon: Mutex<Option<i64>>,
}
struct AdminServer {
    base: url::Url,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for AdminServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn reply(status: u16, body: Value) -> axum::response::Response {
    axum::response::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap()
}
async fn admin_api(
    axum::extract::State(s): axum::extract::State<Arc<FakeAdmin>>,
    request: axum::extract::Request,
) -> axum::response::Response {
    let path = request.uri().path().to_owned();
    if path == "/v1/models" {
        return reply(200, json!({"data": []}));
    }
    let header = |name: &str| {
        request
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let call = AdminCall {
        path: path.clone(),
        query: url::form_urlencoded::parse(request.uri().query().unwrap_or("").as_bytes())
            .into_owned()
            .collect(),
        key: header("x-api-key"),
        version: header("anthropic-version"),
    };
    s.calls.lock().unwrap().push(call.clone());
    if let Some(status) = s.failures.lock().unwrap().get(path.as_str()).copied() {
        if status == 200 {
            return axum::response::Response::new(axum::body::Body::from("not json"));
        }
        return reply(
            status,
            json!({"type":"error","error":{"type":"api_error","message":"synthetic failure"}}),
        );
    }
    match path.as_str() {
        KEYS => {
            let keys = s.keys.lock().unwrap().clone();
            let start = match call.one("after_id") {
                None => 0,
                Some(after) => keys
                    .iter()
                    .position(|(id, _)| id == after)
                    .map_or(keys.len(), |i| i + 1),
            };
            let page = &keys[start..(start + KEY_PAGE).min(keys.len())];
            reply(
                200,
                json!({
                    "data": page.iter().map(|(id, hint)| json!({"id":id,"type":"api_key","name":id,"workspace_id":null,"created_at":"2026-01-01T00:00:00Z","created_by":{"id":"user_synthetic","type":"user"},"partial_key_hint":hint,"status":"active"})).collect::<Vec<_>>(),
                    "has_more": start + page.len() < keys.len(),
                    "first_id": page.first().map(|(id, _)| id),
                    "last_id": page.last().map(|(id, _)| id),
                }),
            )
        }
        USAGE => {
            let width = match call.one("bucket_width") {
                Some("1h") => HOUR,
                Some("1m") => MINUTE,
                _ => return reply(400, json!({})),
            };
            let at = |name: &str| call.one(name).map(epoch);
            let (Some(from), Some(to), Some(limit)) = (
                at("starting_at"),
                at("ending_at"),
                call.one("limit").and_then(|s| s.parse::<usize>().ok()),
            ) else {
                return reply(400, json!({}));
            };
            let ids = call.all("api_key_ids[]");
            let mut t = match call.one("page") {
                Some(page) => page
                    .strip_prefix("page_")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(to),
                None => from,
            };
            let end = s.horizon.lock().unwrap().map_or(to, |h| to.min(h));
            let usage = s.usage.lock().unwrap();
            let mut data = Vec::new();
            while t < end && data.len() < limit {
                let results: Vec<&Value> = usage
                    .iter()
                    .filter(|(w, start, r)| {
                        *w == width
                            && *start == t
                            && ids.contains(&r["api_key_id"].as_str().unwrap_or(""))
                    })
                    .map(|(_, _, r)| r)
                    .collect();
                data.push(json!({"starting_at":rfc3339(t),"ending_at":rfc3339(t + width),"results":results}));
                t += width;
            }
            let more = t < end;
            reply(
                200,
                json!({"data":data,"has_more":more,"next_page":more.then(|| format!("page_{t}"))}),
            )
        }
        _ => reply(404, json!({})),
    }
}
impl FakeAdmin {
    async fn serve(self: &Arc<Self>) -> AdminServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let app = axum::Router::new()
            .fallback(admin_api)
            .with_state(self.clone());
        AdminServer {
            base,
            task: tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }),
        }
    }
    fn set_keys(&self, keys: &[(&str, Option<String>)]) {
        *self.keys.lock().unwrap() = keys
            .iter()
            .map(|(id, hint)| ((*id).to_owned(), hint.clone()))
            .collect();
    }
    /// 사용량 보고서 한 줄. `n`은 [캐시 아닌 입력, 캐시 쓰기 5분, 캐시 쓰기 1시간, 캐시 읽기, 출력]이다.
    fn add_at(&self, width: i64, start: i64, key: &str, model: &str, n: [u64; 5]) {
        self.usage.lock().unwrap().push((
            width,
            start,
            json!({"uncached_input_tokens":n[0],"cache_creation":{"ephemeral_5m_input_tokens":n[1],"ephemeral_1h_input_tokens":n[2]},"cache_read_input_tokens":n[3],"output_tokens":n[4],"server_tool_use":{"web_search_requests":0},"api_key_id":key,"workspace_id":null,"model":model,"service_tier":null,"context_window":null}),
        ));
    }
    /// 1시간 칸 한 줄.
    fn add(&self, start: i64, key: &str, model: &str, n: [u64; 5]) {
        self.add_at(HOUR, start, key, model, n);
    }
    /// 1분 칸 한 줄.
    fn add_minute(&self, start: i64, key: &str, model: &str, n: [u64; 5]) {
        self.add_at(MINUTE, start, key, model, n);
    }
    /// 보고서가 `horizon`(epoch 초)부터는 아직 채워지지 않은 것처럼 칸을 돌려주지 않는다. None이면 다 돌려준다.
    fn lag_until(&self, horizon: Option<i64>) {
        *self.horizon.lock().unwrap() = horizon;
    }
    fn clear_usage(&self) {
        self.usage.lock().unwrap().clear();
    }
    fn fail(&self, path: &'static str, status: u16) {
        self.failures.lock().unwrap().insert(path, status);
    }
    fn recover(&self) {
        self.failures.lock().unwrap().clear();
    }
    fn calls(&self, path: &str) -> Vec<AdminCall> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.path == path)
            .cloned()
            .collect()
    }
    fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
    /// 1분 칸 사용량 보고서 요청(사후 보정과 기준 확인).
    fn minute_calls(&self) -> Vec<AdminCall> {
        self.calls(USAGE)
            .into_iter()
            .filter(|c| c.one("bucket_width") == Some("1m"))
            .collect()
    }
}

/// 토큰 대조 시험 조립. 관리 API·게이트웨이 서버, Provider 키 저장소, 장부, 합성 Admin API, 합성 Provider가
/// 한 DB를 쓴다. Admin API 키가 있으면 `check`가 있다.
struct Check {
    db: postgres::TestDb,
    service: Arc<CredentialService>,
    ledger: Arc<PgLedger>,
    check: Option<Arc<TokenCheck>>,
    admin_api: Arc<FakeAdmin>,
    admin_server: AdminServer,
    runtime: Arc<Runtime>,
    profile: config::ModelProfile,
    base: String,
    /// 관리자 alice와 일반 사용자 bob의 SSO 토큰.
    alice: String,
    bob: String,
    principal: Uuid,
    provider: Arc<provider::Provider>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    _servers: (provider::Server, sso::Server),
}
impl Check {
    async fn new(admin_key: bool) -> Self {
        use m_gated::{
            auth::{Authenticator, SsoVerifier, jwks::JwksCache},
            bootstrap::{self, Dependencies},
            gateway::InferenceGateway,
        };
        let cfg = token_check_configuration(admin_key);
        let db = postgres::TestDb::new().await;
        postgres::grant_access(&db.store, &cfg, &[("bob@example.test", "user")]).await;
        let jwks = sso::FakeJwks::new(vec![sso::Signer::shared().jwk.clone()])
            .serve()
            .await;
        let verifier = SsoVerifier::new(
            cfg.sso().unwrap(),
            Arc::new(JwksCache::loopback_for_test(jwks.uri.clone()).unwrap()),
        );
        let alice = sso::Signer::shared().sign(&sso::claims(), "key1");
        let mut bob = sso::claims();
        bob["sub"] = json!("human-2");
        bob["email"] = json!("bob@example.test");
        let bob = sso::Signer::shared().sign(&bob, "key1");
        let identity = verifier
            .verify(&sso::credential(alice.clone()), 1900000000)
            .await
            .unwrap();
        let principal = db.store.current_human(&identity).await.unwrap().id;
        sqlx::query("UPDATE gw_policies SET allow_rules=$2 WHERE principal_id=$1")
            .bind(principal)
            .bind(json!([{"provider":"anthropic","model":"synthetic-text","capabilities":["messages","count_tokens"]}]))
            .execute(db.store.pool())
            .await
            .unwrap();
        let auth = Arc::new(Authenticator::new(&cfg, verifier, db.store.clone()));
        // Provider 키 등록 확인과 Admin API는 같은 합성 서버로 간다(제품에서는 둘 다 Anthropic 주소다).
        let admin_api = Arc::new(FakeAdmin::default());
        let admin_server = admin_api.serve().await;
        let service = CredentialService::for_test(
            &cfg.credential_encryption_key_bytes(),
            db.store.clone(),
            vec![
                NativeClient::loopback_store_for_test(
                    admin_server.base.clone(),
                    ProviderId::Anthropic,
                    Arc::new(CredentialCache::default()),
                )
                .unwrap(),
            ],
        );
        let ledger = Arc::new(
            PgLedger::new(db.store.clone(), m_gated::gateway::auth_config_digest(&cfg))
                .with_ceiling(cfg.execution_ceiling().clone()),
        );
        let check = TokenCheck::new(&cfg, ledger.clone(), service.clone());
        // 추론은 가상 키에 연결된 Provider 키를 캐시에서 고정해 합성 Provider로 보낸다.
        let provider = provider::Provider::new();
        let provider_server = provider.serve().await;
        let client = NativeClient::loopback_store_for_test(
            provider_server.base.clone(),
            ProviderId::Anthropic,
            service.cache(),
        )
        .unwrap();
        let gateway = Arc::new(InferenceGateway::assemble(
            &cfg.providers()[0],
            auth.clone(),
            ledger.clone(),
            client,
        ));
        let dashboard = Arc::new(
            m_gated::admin::DashboardApi::new(&cfg, auth.clone(), db.store.clone())
                .with_credentials(service.clone()),
        );
        let profile = cfg.providers()[0].profiles[0].clone();
        let app = bootstrap::build(
            cfg,
            Dependencies::unassembled()
                .with_authentication(auth)
                .with_dashboard(dashboard)
                .with_inference(gateway),
        )
        .unwrap();
        let runtime = app.runtime().clone();
        runtime.start_http().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(m_gated::server::serve(
            listener,
            app.router(),
            runtime.clone(),
            std::future::pending(),
        ));
        Self {
            db,
            service,
            ledger,
            check,
            admin_api,
            admin_server,
            runtime,
            profile,
            base,
            alice,
            bob,
            principal,
            provider,
            task,
            _servers: (provider_server, jwks),
        }
    }
    async fn close(self) {
        self.task.abort();
        let _ = self.task.await;
        self.db.close().await;
    }
    /// 의존성 상태를 방금 확인한 것으로 남긴다. 시험이 상태 유효 시간(10초)보다 길어도 요청을 받게 한다.
    fn fresh(&self) {
        for d in [
            Dependency::Database,
            Dependency::Schema,
            Dependency::Jwks,
            Dependency::Ui,
        ] {
            self.runtime
                .record_health(d, true, tokio::time::Instant::now());
        }
    }
    /// 관리 API를 SSO 토큰으로 부른다. (상태, 본문)을 돌려준다.
    async fn call(
        &self,
        token: &str,
        method: reqwest::Method,
        path: &str,
        command: Uuid,
        body: Option<Value>,
    ) -> (u16, Value) {
        self.fresh();
        let mut r = reqwest::Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .build()
            .unwrap()
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(token)
            .header("x-gateway-command-id", command.to_string());
        if let Some(body) = body {
            r = r
                .header("content-type", "application/json")
                .body(body.to_string());
        }
        let r = r.send().await.unwrap();
        let status = r.status().as_u16();
        let body = serde_json::from_slice(&r.bytes().await.unwrap()).unwrap_or(Value::Null);
        (status, body)
    }
    async fn api(
        &self,
        token: &str,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        self.call(token, method, path, Uuid::new_v4(), body).await
    }
    /// alice(관리자)로 읽는다. 200이어야 한다.
    async fn get(&self, path: &str) -> Value {
        let (status, body) = self
            .api(&self.alice, reqwest::Method::GET, path, None)
            .await;
        assert_eq!(status, 200, "{path} {body}");
        body
    }
    async fn view(&self, range: &str) -> Value {
        self.get(&format!("/api/admin/token-check?range={range}"))
            .await
    }
    /// 토큰 대조 화면의 행: (API 키 ID, 모델) → (연결된 Provider 키, Provider가 센 토큰).
    async fn provider_tokens(&self, range: &str) -> BTreeMap<(String, String), (Value, String)> {
        self.view(range).await["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    (
                        r["api_key_id"].as_str().unwrap().to_owned(),
                        r["model"].as_str().unwrap().to_owned(),
                    ),
                    (
                        r["credential"].clone(),
                        r["provider_tokens"].as_str().unwrap().to_owned(),
                    ),
                )
            })
            .collect()
    }
    /// 끊긴 요청 규칙과(주면) 사후 보정 스위치를 바꾼다. 지금 버전은 설정 화면에서 읽는다.
    async fn set_rule(&self, rule: &str, correction: Option<bool>) -> Value {
        let version = self.get(SETTINGS).await["cutoff_rule"]["version"].clone();
        let mut input =
            json!({"provider":"anthropic","cutoff_rule":rule,"expected_version":version});
        if let Some(on) = correction {
            input["correction"] = json!(on);
        }
        let (status, body) = self
            .api(&self.alice, reqwest::Method::PUT, RULE, Some(input))
            .await;
        assert_eq!(status, 200, "{body}");
        body
    }
    /// Provider 키를 관리 API로 등록하고 캐시에 올린다. 키 id를 돌려준다.
    async fn add_key(&self, label: &str, secret: &str) -> Uuid {
        let (status, body) = self
            .api(
                &self.alice,
                reqwest::Method::POST,
                "/api/admin/provider-credentials/anthropic",
                Some(json!({"label":label,"secret":secret})),
            )
            .await;
        assert_eq!(status, 201, "{body}");
        self.service.refresh().await.unwrap();
        body["target"]["id"].as_str().unwrap().parse().unwrap()
    }
    /// Provider 키를 해제하고 캐시에서 내린다.
    async fn retire(&self, id: Uuid) {
        let (status, body) = self
            .api(
                &self.alice,
                reqwest::Method::DELETE,
                &format!("/api/admin/provider-credentials/anthropic/{id}?expected_version=1"),
                None,
            )
            .await;
        assert_eq!(status, 200, "{body}");
        self.service.refresh().await.unwrap();
    }
    /// 장부에 Anthropic 요청 하나를 받고 송신 권한을 받는다. 요청은 이 Provider 키로 나간 것으로 남는다.
    /// 스트림이면 받을 때의 끊긴 요청 규칙을 기록한다. max_tokens는 100이다.
    async fn dispatched(&self, credential: Uuid, stream: bool) -> Uuid {
        let current = finance::current(self.db.store.pool(), self.principal).await;
        let mut spec = RequestSpec::from_profile(ProviderId::Anthropic, &self.profile, Some(100));
        spec.stream = stream;
        spec.provider_credential_id = Some(credential);
        self.ledger.admit_request(&current, &spec).await.unwrap();
        self.ledger.authorize_dispatch(spec.id).await.unwrap();
        spec.id
    }
    /// Provider가 알린 usage(입력, 출력)로 정산한 요청.
    async fn settled(&self, credential: Uuid, input: u64, output: u64) -> Uuid {
        let id = self.dispatched(credential, false).await;
        let observation = FinalObservation::completed(FinalUsage::new(
            "synthetic-text",
            [
                (m_gated::ledger::finance::UsageCategory::Input, input),
                (m_gated::ledger::finance::UsageCategory::Output, output),
            ],
            None,
        ));
        assert_eq!(
            self.ledger
                .finalize_request(id, &observation)
                .await
                .unwrap(),
            Finalized::Settled
        );
        id
    }
    /// 시작 event(입력 `input`) 뒤에 끊긴 스트림을 받은 출력 `received`로 정산한 요청(`received` 규칙).
    async fn cut_off(&self, credential: Uuid, input: u64, received: u64) -> Uuid {
        self.cut_off_with(credential, [input, 0, 0, 0], received)
            .await
    }
    /// 송신한 요청을, 응답의 usage를 알 수 없어 미정산으로 마감한다.
    /// 이 뒤로는 같은 사용자가 유료 요청을 송신할 수 없으므로 필요한 요청을 모두 송신한 뒤에 부른다.
    async fn unknown(&self, id: Uuid) {
        let observation = ended(FinalOutcome::Completed, "completed", Some(200));
        assert_eq!(
            self.ledger
                .finalize_request(id, &observation)
                .await
                .unwrap(),
            Finalized::Unknown
        );
    }
    /// 끝난 요청의 시각을 모두 같은 만큼 옮겨, 송신을 승인한 시각을 `start`, 끝난 시각을 `end`로 둔다.
    /// 접수 시각·기한·lease·실행 종료 시각도 함께 옮겨 gw_requests의 CHECK 제약
    /// (접수 ≤ 송신 승인 < 접수 기한, 접수 ≤ 끝, lease = 실행 기한)을 지킨다.
    async fn place(&self, id: Uuid, start: i64, end: i64) {
        sqlx::query("UPDATE gw_requests r SET admitted_at=r.admitted_at+s.d,admit_expires_at=r.admit_expires_at+s.d,transport_deadline=r.transport_deadline+s.d,execution_deadline=r.execution_deadline+s.d,dispatch_authorized_at=r.dispatch_authorized_at+s.d,lease_expires_at=r.lease_expires_at+s.d,execution_stopped_at=r.execution_stopped_at+s.d,finished_at=to_timestamp($3) FROM (SELECT to_timestamp($2)-dispatch_authorized_at AS d FROM gw_requests WHERE id=$1) s WHERE r.id=$1")
            .bind(id)
            .bind(start)
            .bind(end)
            .execute(self.db.store.pool())
            .await
            .unwrap();
    }
    async fn poll(&self) -> Result<(), &'static str> {
        self.check.as_ref().expect("Admin API 키").poll_once().await
    }
    /// 4분 lease가 끝난 것으로 둔다(다음 5분 주기를 흉내 낸다).
    async fn release_lease(&self) {
        sqlx::query("UPDATE gw_token_check_state SET lease_until=NULL")
            .execute(self.db.store.pool())
            .await
            .unwrap();
    }
    /// 같은 DB를 쓰는 다른 서버의 토큰 대조.
    async fn other_server(&self) -> Arc<TokenCheck> {
        let cfg = token_check_configuration(true);
        let service = CredentialService::for_test(
            &cfg.credential_encryption_key_bytes(),
            self.db.store.clone(),
            vec![
                NativeClient::loopback_store_for_test(
                    self.admin_server.base.clone(),
                    ProviderId::Anthropic,
                    Arc::new(CredentialCache::default()),
                )
                .unwrap(),
            ],
        );
        service.refresh().await.unwrap();
        let ledger = Arc::new(PgLedger::new(
            self.db.store.clone(),
            m_gated::gateway::auth_config_digest(&cfg),
        ));
        TokenCheck::new(&cfg, ledger, service).unwrap()
    }
    /// 토큰 대조 화면의 행: (API 키 ID, 모델) → (Provider가 센 토큰, 게이트웨이가 센 토큰).
    async fn rows(&self, range: &str) -> BTreeMap<(String, String), (String, String)> {
        self.view(range).await["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    (
                        r["api_key_id"].as_str().unwrap().to_owned(),
                        r["model"].as_str().unwrap().to_owned(),
                    ),
                    (
                        r["provider_tokens"].as_str().unwrap().to_owned(),
                        r["gateway_tokens"].as_str().unwrap().to_owned(),
                    ),
                )
            })
            .collect()
    }
    /// 1분 칸 기준을 센 값을 바로 넣는다(기준을 아는 상태에서 시작하는 지름길).
    async fn set_minutes(&self, start: i64, end: i64) {
        sqlx::query("UPDATE gw_token_check_state SET start_minutes=$1,end_minutes=$2")
            .bind(start)
            .bind(end)
            .execute(self.db.store.pool())
            .await
            .unwrap();
    }
    /// alice에게 Provider 키를 인가하고, 그 키에 연결한 가상 키를 발급한다. 가상 키 원문을 돌려준다.
    async fn virtual_key(&self, credential: Uuid) -> String {
        let (status, body) = self
            .api(
                &self.alice,
                reqwest::Method::PUT,
                &format!("/api/admin/principals/{}/provider-credentials", self.principal),
                Some(json!({"provider":"anthropic","credential_ids":[credential],"expected":{"credential_ids":[]}})),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let (status, body) = self
            .api(
                &self.alice,
                reqwest::Method::POST,
                "/api/keys",
                Some(json!({"name":"토큰 대조","validity":"1_year","grants":[{"provider":"anthropic","model":"synthetic-text","capabilities":["messages"]}],"limits":{"budget":null,"rpm":null,"tpm":null,"count_rpm":null},"provider_credentials":{"anthropic":credential}})),
            )
            .await;
        assert_eq!(status, 201, "{body}");
        body["key"].as_str().unwrap().to_owned()
    }
    /// 가상 키로 스트림 요청을 보낸다. 시작 event(입력 100) 뒤에 텍스트 `text`를 받은 다음 client가 끊는다.
    /// 게이트웨이가 정산을 마칠 때까지 기다리고 요청 id를 돌려준다.
    async fn cut_stream(&self, key: &str, text: &str) -> Uuid {
        self.fresh();
        self.provider.count_by_content().await;
        let event = |data: Value| {
            format!(
                "event: {}\ndata: {data}\n\n",
                data["type"].as_str().unwrap()
            )
        };
        let wire = format!(
            "{}{}{}",
            s3_contract::START,
            event(
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})
            ),
            event(
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}})
            )
        );
        let (tx, stopped) = self.provider.stream_messages().await;
        tx.send(Bytes::from(wire.clone())).await.unwrap();
        let mut r = reqwest::Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .build()
            .unwrap()
            .post(format!("{}/v1/messages", self.base))
            .bearer_auth(key)
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .body(json!({"model":"synthetic-text","max_tokens":100,"stream":true,"messages":[{"role":"user","content":"synthetic prompt sentinel"}]}).to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let id = r.headers()["x-gateway-request-id"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), async {
            while received.len() < wire.len() {
                received.extend_from_slice(&r.chunk().await.unwrap().unwrap());
            }
        })
        .await
        .unwrap();
        drop(r);
        tokio::time::timeout(Duration::from_secs(3), stopped)
            .await
            .expect("client가 끊으면 Provider 연결도 닫는다")
            .unwrap();
        drop(tx);
        tokio::time::timeout(Duration::from_secs(6), self.runtime.wait_requests())
            .await
            .unwrap();
        id
    }
    /// 시작 event가 알린 [입력, 캐시 읽기, 5분 캐시 쓰기, 1시간 캐시 쓰기] 뒤에 끊긴 스트림을 받은 출력
    /// `received`로 정산한 요청.
    /// 받을 때 규칙이 `max_tokens`였으면 max_tokens(100)로 정산한다.
    async fn cut_off_with(&self, credential: Uuid, start: [u64; 4], received: u64) -> Uuid {
        use m_gated::ledger::finance::UsageCategory::*;
        let id = self.dispatched(credential, true).await;
        let cutoff = NoUsage::Cutoff {
            start: StartUsage {
                actual_model: "synthetic-text".into(),
                tokens: [
                    (Input, start[0]),
                    (CacheRead, start[1]),
                    (CacheWrite5m, start[2]),
                    (CacheWrite1h, start[3]),
                ]
                .into_iter()
                .collect(),
            },
            received_output: Some(received),
            count_failure: None,
        };
        let observation = ended(FinalOutcome::Cancelled, "client_cancelled", None);
        assert_eq!(
            self.ledger
                .finalize_request_with(id, &observation, &cutoff)
                .await
                .unwrap(),
            Finalized::Settled
        );
        id
    }
    /// Provider가 HTTP 오류(429)로 답해 0원으로 정산한 요청.
    async fn no_response(&self, credential: Uuid) -> Uuid {
        let id = self.dispatched(credential, false).await;
        let observation = ended(FinalOutcome::Failed, "provider_error", Some(429));
        assert_eq!(
            self.ledger
                .finalize_request_with(id, &observation, &NoUsage::NoResponse)
                .await
                .unwrap(),
            Finalized::Settled
        );
        id
    }
    /// 송신한 요청을, 응답이 알린 모델(`synthetic-other`)이 profile에 없어 금액을 정하지 못한 채 마감한다.
    /// 사용량 기록(final)은 있지만 미정산이다. 이 뒤로는 같은 사용자가 유료 요청을 송신할 수 없다.
    async fn unpriced(&self, id: Uuid) {
        let observation = FinalObservation::completed(FinalUsage::new(
            "synthetic-other",
            [
                (m_gated::ledger::finance::UsageCategory::Input, 100),
                (m_gated::ledger::finance::UsageCategory::Output, 20),
            ],
            None,
        ));
        assert_eq!(
            self.ledger
                .finalize_request(id, &observation)
                .await
                .unwrap(),
            Finalized::Unknown
        );
    }
    /// 요청이 지난 예산 기간(한 달 전)에 정산된 것으로 둔다. 그 기간의 합계 행을 만들고, 이 요청의 정산 값을
    /// 지금 기간의 합계에서 옮긴다. 사용자·가상 키 범위 모두 옮긴다.
    async fn to_previous_period(&self, id: Uuid) {
        let pool = self.db.store.pool();
        sqlx::query("INSERT INTO gw_usage_totals(scope_id,period_start,period_end,timezone_snapshot,cost_usd,settled_count,input_tokens,output_tokens) SELECT scope_id,period_start-interval '1 month',period_start,timezone_snapshot,applied_cost_usd,1,applied_input_tokens,applied_output_tokens FROM gw_request_budgets WHERE request_id=$1")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("UPDATE gw_usage_totals t SET cost_usd=t.cost_usd-b.applied_cost_usd,settled_count=t.settled_count-1,input_tokens=t.input_tokens-b.applied_input_tokens,output_tokens=t.output_tokens-b.applied_output_tokens FROM gw_request_budgets b WHERE b.request_id=$1 AND t.scope_id=b.scope_id AND t.period_start=b.period_start")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("UPDATE gw_request_budgets SET period_end=period_start,period_start=period_start-interval '1 month' WHERE request_id=$1")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }
    /// 사후 보정 기록: (요청 id, 고친 출력 토큰). 요청 id 순서다.
    async fn corrections(&self) -> Vec<(Uuid, String)> {
        sqlx::query_as("SELECT request_id,normalized_usage->>'output_tokens' FROM gw_usage_events WHERE kind='correction' ORDER BY request_id")
            .fetch_all(self.db.store.pool())
            .await
            .unwrap()
    }
    /// 사후 보정 근거의 요약: (보고서의 분, 분을 고른 기준, 같은 분의 다른 요청 수).
    async fn evidence(&self, id: Uuid) -> (String, String, i64) {
        sqlx::query_as("SELECT provider_usage->>'minute',provider_usage->>'convention',(provider_usage->>'other_requests')::bigint FROM gw_usage_events WHERE request_id=$1 AND kind='correction'")
            .bind(id)
            .fetch_one(self.db.store.pool())
            .await
            .unwrap()
    }
    /// 요청 하나의 비용(문자열).
    async fn cost(&self, id: Uuid) -> String {
        sqlx::query_scalar("SELECT cost_usd::text FROM gw_requests WHERE id=$1")
            .bind(id)
            .fetch_one(self.db.store.pool())
            .await
            .unwrap()
    }
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn api_key_ids_are_matched_by_hint_across_key_list_pages() {
    let h = Check::new(true).await;
    let one = h.add_key("team-1", KEY_ONE).await;
    let two = h.add_key("team-2", KEY_TWO).await;
    let lone = h.add_key("lone", KEY_LONE).await;
    let twin = h.add_key("twin", KEY_TWIN).await;
    // 키 목록은 두 개씩 세 페이지다. 게이트웨이 밖의 키, 앞뒤 글자가 없는 키, 앞뒤 글자가 같은 키 둘이 섞여 있다.
    h.admin_api.set_keys(&[
        ("apikey_00outside", Some(OUTSIDE_HINT.into())),
        ("apikey_03twin_a", Some(hint(KEY_TWIN))),
        ("apikey_01one", Some(hint(KEY_ONE))),
        ("apikey_05nohint", None),
        ("apikey_03twin_b", Some(hint(KEY_TWIN))),
        ("apikey_02two", Some(hint(KEY_TWO))),
    ]);
    let t = settled_hour();
    for (key, n) in [
        ("apikey_01one", [10, 0, 0, 0, 1]),
        ("apikey_02two", [20, 0, 0, 0, 2]),
        ("apikey_03twin_a", [30, 0, 0, 0, 3]),
        ("apikey_00outside", [40, 0, 0, 0, 4]),
    ] {
        h.admin_api.add(t - 5 * HOUR, key, "synthetic-text", n);
    }
    h.poll().await.unwrap();
    let lists = h.admin_api.calls(KEYS);
    assert_eq!(
        lists.iter().map(|c| c.one("after_id")).collect::<Vec<_>>(),
        [None, Some("apikey_03twin_a"), Some("apikey_05nohint")]
    );
    assert!(lists.iter().all(|c| c.one("limit") == Some("1000")));
    // Admin API에는 Provider 키가 아니라 Admin API 키와 버전 헤더를 보낸다.
    for c in h.admin_api.calls.lock().unwrap().iter() {
        assert_eq!(
            (c.key.as_deref(), c.version.as_deref()),
            (Some(ADMIN_KEY), Some("2023-06-01")),
            "{}",
            c.path
        );
    }
    // 앞뒤 글자가 API 키 하나와만 맞는 Provider 키만 짝짓고, 사용량 보고서는 그 API 키 ID로만 묻는다.
    let reports = h.admin_api.calls(USAGE);
    assert!(!reports.is_empty());
    assert!(
        reports
            .iter()
            .all(|c| c.all("api_key_ids[]") == ["apikey_01one", "apikey_02two"])
    );
    // 화면의 행은 API 키 ID마다 짝지은 Provider 키를 보인다.
    let first = json!({"id":one,"label":"team-1","hint":"k1AA"});
    let second = json!({"id":two,"label":"team-2","hint":"k2BB"});
    let rows = BTreeMap::from([
        (
            ("apikey_01one".to_owned(), "synthetic-text".to_owned()),
            (first, "11".to_owned()),
        ),
        (
            ("apikey_02two".to_owned(), "synthetic-text".to_owned()),
            (second, "22".to_owned()),
        ),
    ]);
    assert_eq!(h.provider_tokens("24h").await, rows);
    // 짝이 없는 사용 중인 Provider 키는 화면에 따로 보이고 대조에서 빠진다.
    let view = h.view("24h").await;
    assert_eq!(view["enabled"], true);
    assert_eq!(view["state"]["last_error"], Value::Null);
    assert_eq!(
        view["unmatched_credentials"],
        json!([{"id":lone,"label":"lone","hint":"k3CC"},{"id":twin,"label":"twin","hint":"k4DD"}])
    );
    // 해제한 Provider 키의 짝은 남겨 지난 사용량을 계속 대조한다. 해제한 키는 짝 없는 키 목록에도 없다.
    h.retire(two).await;
    h.retire(lone).await;
    h.release_lease().await;
    h.poll().await.unwrap();
    assert_eq!(
        h.admin_api
            .calls(USAGE)
            .last()
            .unwrap()
            .all("api_key_ids[]"),
        ["apikey_01one", "apikey_02two"]
    );
    assert_eq!(h.provider_tokens("24h").await, rows);
    assert_eq!(
        h.view("24h").await["unmatched_credentials"],
        json!([{"id":twin,"label":"twin","hint":"k4DD"}])
    );
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn zero_matched_keys_stop_the_poll_before_the_usage_report() {
    let h = Check::new(true).await;
    // Provider 키가 아직 없다.
    assert_eq!(h.poll().await, Err("no_matched_keys"));
    // 등록한 키가 조직의 키 목록에 없다.
    let lone = h.add_key("lone", KEY_LONE).await;
    h.admin_api
        .set_keys(&[("apikey_00outside", Some(OUTSIDE_HINT.into()))]);
    h.release_lease().await;
    assert_eq!(h.poll().await, Err("no_matched_keys"));
    assert_eq!(h.admin_api.calls(KEYS).len(), 2);
    assert!(h.admin_api.calls(USAGE).is_empty());
    let view = h.view("24h").await;
    assert!(view["state"]["last_attempt_at"].is_string());
    assert_eq!(view["state"]["last_success_at"], Value::Null);
    assert_eq!(view["state"]["last_error"], "no_matched_keys");
    assert_eq!(view["rows"], json!([]));
    assert_eq!(
        view["unmatched_credentials"],
        json!([{"id":lone,"label":"lone","hint":"k3CC"}])
    );
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn usage_is_read_for_thirty_days_then_the_last_two_days_and_late_values_replace_old_ones() {
    let h = Check::new(true).await;
    let one = h.add_key("team-1", KEY_ONE).await;
    h.admin_api
        .set_keys(&[("apikey_01one", Some(hint(KEY_ONE)))]);
    let t = settled_hour();
    let key = "apikey_01one";
    // 70시간 전 칸과 하루 전 칸. 하루 전 칸은 같은 키·모델이 둘로 나뉘어 오고(합친다), 다른 모델 칸도 있다.
    h.admin_api
        .add(t - 70 * HOUR, key, "claude-old", [10, 1, 2, 3, 4]);
    h.admin_api
        .add(t - DAY, key, "synthetic-text", [100, 0, 0, 0, 10]);
    h.admin_api
        .add(t - DAY, key, "synthetic-text", [5, 0, 0, 0, 1]);
    h.admin_api
        .add(t - DAY, key, "claude-other", [7, 0, 0, 0, 7]);
    h.poll().await.unwrap();

    // 첫 조회는 30일(1시간 칸 720개)을 `limit`개씩 나눈 페이지로 끝까지 읽는다.
    let first = h.admin_api.calls(USAGE);
    let end = epoch(first[0].one("ending_at").unwrap());
    assert!(
        end == t || end == t + HOUR,
        "끝은 끝난 지 10분이 지난 정시다"
    );
    let limit: i64 = first[0].one("limit").unwrap().parse().unwrap();
    assert!((1..=168).contains(&limit), "1시간 칸은 한 번에 168개까지다");
    assert_eq!(first.len() as i64, (30 * 24 + limit - 1) / limit);
    for (i, c) in first.iter().enumerate() {
        assert_eq!(c.one("starting_at"), Some(rfc3339(end - 30 * DAY).as_str()));
        assert_eq!(c.one("ending_at"), Some(rfc3339(end).as_str()));
        assert_eq!(c.one("bucket_width"), Some("1h"));
        assert_eq!(c.one("limit"), Some(limit.to_string().as_str()));
        assert_eq!(c.all("group_by[]"), ["api_key_id", "model"]);
        assert_eq!(c.all("api_key_ids[]"), [key]);
        let page = (i > 0).then(|| format!("page_{}", end - 30 * DAY + i as i64 * limit * HOUR));
        assert_eq!(c.one("page"), page.as_deref());
    }
    let credential = json!({"id":one,"label":"team-1","hint":"k1AA"});
    let row = |model: &str, tokens: &str| {
        (
            (key.to_owned(), model.to_owned()),
            (credential.clone(), tokens.to_owned()),
        )
    };
    assert_eq!(
        h.provider_tokens("7d").await,
        BTreeMap::from([
            row("claude-old", "20"),
            row("claude-other", "14"),
            row("synthetic-text", "116"),
        ])
    );
    let view = h.view("7d").await;
    assert!(view["state"]["last_success_at"].is_string());
    assert_eq!(view["state"]["last_error"], Value::Null);

    // 늦게 들어온 값: 하루 전 칸이 바뀌고 다른 모델 칸은 없어졌다.
    // 70시간 전 칸도 바뀌었지만, 30일을 받아 둔 뒤의 조회는 최근 48시간만 다시 읽으므로 그대로 둔다.
    h.admin_api.clear_usage();
    h.admin_api
        .add(t - DAY, key, "synthetic-text", [120, 0, 0, 0, 30]);
    h.admin_api
        .add(t - 70 * HOUR, key, "claude-old", [99, 0, 0, 0, 99]);
    h.release_lease().await;
    h.poll().await.unwrap();
    let second = &h.admin_api.calls(USAGE)[first.len()..];
    let end2 = epoch(second[0].one("ending_at").unwrap());
    assert!(end2 == end || end2 == end + HOUR);
    assert_eq!(second.len() as i64, (48 + limit - 1) / limit);
    assert!(
        second
            .iter()
            .all(|c| c.one("starting_at") == Some(rfc3339(end2 - 2 * DAY).as_str()))
    );
    assert_eq!(
        h.provider_tokens("7d").await,
        BTreeMap::from([row("claude-old", "20"), row("synthetic-text", "150")])
    );
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn admin_api_rejection_and_outage_are_recorded_without_moving_the_last_success() {
    let h = Check::new(true).await;
    h.add_key("team-1", KEY_ONE).await;
    h.admin_api
        .set_keys(&[("apikey_01one", Some(hint(KEY_ONE)))]);
    h.poll().await.unwrap();
    let ok = h.view("24h").await["state"].clone();
    assert!(ok["last_success_at"].is_string());
    // 키 목록이나 사용량 보고서가 거절(401·403)되거나 실패(5xx, JSON이 아닌 본문)하면 그 까닭만 남긴다.
    // 마지막 성공 시각은 그대로다.
    for (path, status, code) in [
        (KEYS, 401, "admin_key_rejected"),
        (USAGE, 403, "admin_key_rejected"),
        (KEYS, 500, "admin_api_unavailable"),
        (USAGE, 529, "admin_api_unavailable"),
        (USAGE, 200, "admin_api_unavailable"),
    ] {
        h.admin_api.fail(path, status);
        h.release_lease().await;
        let calls = h.admin_api.calls(path).len();
        assert_eq!(h.poll().await, Err(code), "{path} {status}");
        assert_eq!(h.admin_api.calls(path).len(), calls + 1, "{path} {status}");
        let state = &h.view("24h").await["state"];
        assert_eq!(state["last_error"], code, "{path} {status}");
        assert_eq!(state["last_success_at"], ok["last_success_at"]);
        assert!(state["last_attempt_at"].is_string());
        h.admin_api.recover();
    }
    // 다시 성공하면 오류를 지우고 성공 시각을 옮긴다.
    h.release_lease().await;
    h.poll().await.unwrap();
    let state = &h.view("24h").await["state"];
    assert_eq!(state["last_error"], Value::Null);
    assert!(
        epoch(state["last_success_at"].as_str().unwrap())
            >= epoch(ok["last_success_at"].as_str().unwrap())
    );
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn a_poll_within_four_minutes_of_the_last_one_makes_no_admin_call() {
    let h = Check::new(true).await;
    h.add_key("team-1", KEY_ONE).await;
    h.admin_api
        .set_keys(&[("apikey_01one", Some(hint(KEY_ONE)))]);
    h.poll().await.unwrap();
    let calls = h.admin_api.count();
    let polled = h.view("24h").await["state"].clone();
    // lease는 조회를 시도한 때부터 4분이다.
    let lease: f64 = sqlx::query_scalar(
        "SELECT round(extract(epoch FROM lease_until-last_attempt_at))::float8 FROM gw_token_check_state",
    )
    .fetch_one(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(lease, 240.0);
    // 같은 서버의 다음 조회와 같은 DB를 쓰는 다른 서버의 조회는 아무것도 묻지 않고 기록도 바꾸지 않는다.
    let other = h.other_server().await;
    assert_eq!(format!("{other:?}"), "TokenCheck([REDACTED])");
    assert_eq!(h.poll().await, Ok(()));
    assert_eq!(other.poll_once().await, Ok(()));
    assert_eq!(h.admin_api.count(), calls);
    assert_eq!(h.view("24h").await["state"], polled);
    // lease가 끝나면 다른 서버도 맡을 수 있다. 둘이 함께 시도해도 한 대만 조회한다.
    h.release_lease().await;
    let (a, b) = tokio::join!(h.poll(), other.poll_once());
    assert_eq!((a, b), (Ok(()), Ok(())));
    assert_eq!(h.admin_api.calls(KEYS).len(), 2);
    assert_eq!(h.view("24h").await["state"]["last_error"], Value::Null);
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn token_check_first_polls_ten_seconds_after_start_and_stops_on_shutdown() {
    let h = Check::new(true).await;
    h.add_key("team-1", KEY_ONE).await;
    h.admin_api
        .set_keys(&[("apikey_01one", Some(hint(KEY_ONE)))]);
    let run = h.check.clone().unwrap().run(h.runtime.clone());
    let watch = async {
        tokio::time::sleep(Duration::from_secs(9)).await;
        assert_eq!(h.admin_api.count(), 0, "첫 조회는 시작하고 10초 뒤다");
        tokio::time::timeout(Duration::from_secs(10), async {
            while !h.view("24h").await["state"]["last_success_at"].is_string() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        // 다음 조회(5분 뒤)를 기다리는 중에 종료 신호가 오면 바로 끝난다.
        h.runtime.begin_draining();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(30), async { tokio::join!(run, watch) })
            .await
            .unwrap();
    result.unwrap();
    assert_eq!(h.admin_api.calls(KEYS).len(), 1);
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn view_splits_the_provider_surplus_by_reason_and_prices_both_directions() {
    let h = Check::new(true).await;
    let one = h.add_key("team-1", KEY_ONE).await;
    let two = h.add_key("team-2", KEY_TWO).await;
    h.admin_api.set_keys(&[
        ("apikey_01one", Some(hint(KEY_ONE))),
        ("apikey_02two", Some(hint(KEY_TWO))),
    ]);
    let t = settled_hour();
    let hour = |k: i64| t - k * HOUR;
    // 게이트웨이 요청은 조회 전에 만든다. 7시간 전: 끊긴 요청(입력 100, 받은 출력 5).
    let cut = h.cut_off(one, 100, 5).await;
    h.place(cut, hour(7) + 600, hour(7) + 1800).await;
    // 6시간 전: Provider usage로 정산한 요청과 사용량을 모르는 요청(아래에서 만든다).
    let known = h.settled(one, 100, 20).await;
    h.place(known, hour(6) + 600, hour(6) + 1200).await;
    // 5시간 전: 게이트웨이가 더 센 칸.
    let more = h.settled(one, 100, 20).await;
    h.place(more, hour(5) + 600, hour(5) + 1200).await;
    // 4시간 전: 끊긴 요청과 사용량을 모르는 요청(아래에서 만든다)이 함께 있다.
    let early = h.cut_off(one, 100, 5).await;
    h.place(early, hour(4) + 600, hour(4) + 1200).await;
    // 3시간 전: 게이트웨이 요청은 보고서와 같고, 가격이 없는 다른 모델 사용만 남는다.
    let same = h.settled(one, 100, 20).await;
    h.place(same, hour(3) + 600, hour(3) + 1200).await;
    // 사용량을 모르는 요청은 이 사용자의 다음 송신을 막으므로 마지막에 둘 다 송신한 뒤 마감한다.
    let unknown = [
        h.dispatched(one, false).await,
        h.dispatched(one, false).await,
    ];
    for (id, k) in unknown.into_iter().zip([6, 4]) {
        h.unknown(id).await;
        h.place(id, hour(k) + 1500, hour(k) + 1560).await;
    }
    for (k, key, model, n) in [
        (8, "apikey_02two", "synthetic-text", [50, 0, 0, 0, 10]),
        (7, "apikey_01one", "synthetic-text", [100, 0, 0, 0, 40]),
        (6, "apikey_01one", "synthetic-text", [300, 4, 6, 8, 60]),
        (5, "apikey_01one", "synthetic-text", [100, 0, 0, 0, 15]),
        (4, "apikey_01one", "synthetic-text", [100, 0, 0, 0, 30]),
        (3, "apikey_01one", "synthetic-text", [100, 0, 0, 0, 20]),
        (3, "apikey_01one", "claude-unpriced", [1000, 0, 0, 0, 100]),
    ] {
        h.admin_api.add(hour(k), key, model, n);
    }
    h.poll().await.unwrap();
    let view = h.view("24h").await;
    let to = epoch(view["range"]["to"].as_str().unwrap());
    assert_eq!(view["range"]["from"], rfc3339(to - DAY));
    let first = json!({"id":one,"label":"team-1","hint":"k1AA"});
    let second = json!({"id":two,"label":"team-2","hint":"k2BB"});
    // 칸마다 차이를 내고 (API 키, 모델)로 모은다. Provider가 더 센 몫은 칸마다 까닭 하나로 나눈다.
    // - 8시간 전(two): 그 시간에 그 키로 보낸 요청이 없다 → outside 60.
    // - 7시간 전(one): 끊긴 요청이 있다 → cutoff, 출력 35.
    // - 6시간 전(one): 사용량을 모르는 요청이 있다 → unknown. 입력 200, 캐시 쓰기 10(1시간 6부터 채운다), 캐시 읽기 8, 출력 40.
    // - 5시간 전(one): 게이트웨이가 출력 5를 더 셌다.
    // - 4시간 전(one): 끊긴 요청과 사용량을 모르는 요청이 함께 있으면 앞선 까닭 cutoff다. 출력 25.
    // - 3시간 전(one): 가격이 없는 모델 1100 → unexplained.
    assert_eq!(
        view["rows"],
        json!([
            {"credential":first,"api_key_id":"apikey_01one","model":"claude-unpriced","provider_tokens":"1100","gateway_tokens":"0","provider_more_tokens":"1100","gateway_more_tokens":"0","provider_more_usd":null,"gateway_more_usd":"0.000000000000","reasons":[{"reason":"unexplained","tokens":"1100"}]},
            {"credential":first,"api_key_id":"apikey_01one","model":"synthetic-text","provider_tokens":"883","gateway_tokens":"570","provider_more_tokens":"318","gateway_more_tokens":"5","provider_more_usd":"0.000417800000","gateway_more_usd":"0.000010000000","reasons":[{"reason":"cutoff","tokens":"60"},{"reason":"unknown","tokens":"258"}]},
            {"credential":second,"api_key_id":"apikey_02two","model":"synthetic-text","provider_tokens":"60","gateway_tokens":"0","provider_more_tokens":"60","gateway_more_tokens":"0","provider_more_usd":"0.000070000000","gateway_more_usd":"0.000000000000","reasons":[{"reason":"outside","tokens":"60"}]}
        ])
    );
    // 금액은 지금 가격이다. 6시간 전의 더 센 몫: 입력 200×1 + 캐시 쓰기 5분 4×1.25 + 1시간 6×2 + 캐시 읽기 8×0.1 + 출력 40×2.
    // 가격이 없는 모델은 금액에서 빼고 이름을 알린다. 그 모델만 있는 까닭의 금액은 null이다.
    let totals = json!({
        "provider_tokens":"2043","gateway_tokens":"570","provider_more_tokens":"1478","gateway_more_tokens":"5",
        "provider_more_usd":"0.000487800000","gateway_more_usd":"0.000010000000",
        "unpriced_models":["claude-unpriced"],"corrected_requests":"0",
        "reasons":[
            {"reason":"cutoff","tokens":"60","usd":"0.000120000000"},
            {"reason":"unknown","tokens":"258","usd":"0.000297800000"},
            {"reason":"outside","tokens":"60","usd":"0.000070000000"},
            {"reason":"unexplained","tokens":"1100","usd":null}
        ]
    });
    assert_eq!(view["totals"], totals);
    assert_eq!(view["unmatched_credentials"], json!([]));
    assert_eq!(view["enabled"], true);
    // 더 긴 기간도 같은 칸을 비교한다. 범위를 주지 않으면 24시간이다.
    for (range, length) in [("7d", 7 * DAY), ("30d", 30 * DAY)] {
        let view = h.view(range).await;
        let to = epoch(view["range"]["to"].as_str().unwrap());
        assert_eq!(view["range"]["from"], rfc3339(to - length), "{range}");
        assert_eq!(view["totals"], totals, "{range}");
    }
    let plain = h.get("/api/admin/token-check").await;
    let to = epoch(plain["range"]["to"].as_str().unwrap());
    assert_eq!(plain["range"]["from"], rfc3339(to - DAY));
    assert_eq!(plain["totals"], totals);
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn view_needs_an_admin_and_a_known_range_and_says_when_the_check_is_off() {
    let h = Check::new(false).await;
    // Admin API 키가 없으면 토큰 대조를 만들지 않는다.
    assert!(h.check.is_none());
    // 범위는 24h, 7d, 30d만 받는다.
    for query in [
        "range=1d",
        "range=24H",
        "range=",
        "range=24h&range=7d",
        "range=7d&extra=1",
        "from=2026-11-01",
    ] {
        let (status, body) = h
            .api(
                &h.alice,
                reqwest::Method::GET,
                &format!("/api/admin/token-check?{query}"),
                None,
            )
            .await;
        assert_eq!(
            (status, &body["error"]["code"]),
            (400, &json!("invalid_request")),
            "{query}"
        );
    }
    // 관리자만 본다.
    let (status, body) = h
        .api(&h.bob, reqwest::Method::GET, "/api/admin/token-check", None)
        .await;
    assert_eq!(
        (status, &body["error"]["code"]),
        (403, &json!("access_denied"))
    );
    // 대조가 꺼져 있다고 알린다. 조회한 적이 없어 비교한 값도 없다.
    let view = h.view("24h").await;
    assert_eq!(view["enabled"], false);
    for field in ["last_attempt_at", "last_success_at", "last_error"] {
        assert_eq!(view["state"][field], Value::Null, "{field}");
    }
    assert_eq!(view["rows"], json!([]));
    assert_eq!(view["unmatched_credentials"], json!([]));
    for field in [
        "provider_tokens",
        "gateway_tokens",
        "provider_more_tokens",
        "gateway_more_tokens",
    ] {
        assert_eq!(view["totals"][field], "0", "{field}");
    }
    for field in ["provider_more_usd", "gateway_more_usd"] {
        assert_eq!(view["totals"][field], "0.000000000000", "{field}");
    }
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn correction_switch_needs_the_admin_key_and_keeps_its_value_when_omitted() {
    // Admin API 키가 없는 서버에서는 스위치를 켤 수 없다.
    let h = Check::new(false).await;
    let off = json!({"provider":"anthropic","cutoff_rule":"received","correction":false,"correction_available":false,"version":"1"});
    assert_eq!(h.get(SETTINGS).await["cutoff_rule"], off);
    let (status, body) = h
        .api(
            &h.alice,
            reqwest::Method::PUT,
            RULE,
            Some(json!({"provider":"anthropic","cutoff_rule":"received","correction":true,"expected_version":"1"})),
        )
        .await;
    assert_eq!(
        (status, &body["error"]["code"]),
        (400, &json!("invalid_request"))
    );
    // 끄는 값은 받는다. 지금 값과 같아 바뀌지 않는다.
    let noop = h.set_rule("received", Some(false)).await;
    assert_eq!(
        (&noop["effect"], &noop["result_version"]),
        (&json!("noop"), &json!("1"))
    );
    // 스위치는 true나 false다.
    for bad in [json!("true"), json!(1)] {
        let (status, body) = h
            .api(
                &h.alice,
                reqwest::Method::PUT,
                RULE,
                Some(json!({"provider":"anthropic","cutoff_rule":"received","correction":bad,"expected_version":"1"})),
            )
            .await;
        assert_eq!(
            (status, &body["error"]["code"]),
            (400, &json!("invalid_request")),
            "{bad}"
        );
    }
    assert_eq!(h.get(SETTINGS).await["cutoff_rule"], off);
    h.close().await;

    // Admin API 키가 있는 서버.
    let h = Check::new(true).await;
    assert_eq!(
        h.get(SETTINGS).await["cutoff_rule"],
        json!({"provider":"anthropic","cutoff_rule":"received","correction":false,"correction_available":true,"version":"1"})
    );
    let command = Uuid::new_v4();
    let on = json!({"provider":"anthropic","cutoff_rule":"received","correction":true,"expected_version":"1"});
    let (status, changed) = h
        .call(&h.alice, reqwest::Method::PUT, RULE, command, Some(on))
        .await;
    assert_eq!(status, 200, "{changed}");
    assert_eq!(
        (&changed["effect"], &changed["result_version"]),
        (&json!("changed"), &json!("2"))
    );
    assert_eq!(
        changed["cutoff_rule"],
        json!({"provider":"anthropic","cutoff_rule":"received","correction":true,"version":"2"})
    );
    // 같은 명령 id로 스위치 값만 다르게 보내도 다른 명령이다.
    let (status, conflict) = h
        .call(
            &h.alice,
            reqwest::Method::PUT,
            RULE,
            command,
            Some(json!({"provider":"anthropic","cutoff_rule":"received","correction":false,"expected_version":"1"})),
        )
        .await;
    assert_eq!(
        (status, &conflict["error"]["code"]),
        (409, &json!("command_conflict"))
    );
    // 스위치를 빼고 규칙만 바꾸면 스위치는 그대로다.
    let rule = h.set_rule("max_tokens", None).await;
    assert_eq!(
        rule["cutoff_rule"],
        json!({"provider":"anthropic","cutoff_rule":"max_tokens","correction":true,"version":"3"})
    );
    assert_eq!(
        h.get(SETTINGS).await["cutoff_rule"],
        json!({"provider":"anthropic","cutoff_rule":"max_tokens","correction":true,"correction_available":true,"version":"3"})
    );
    // 감사 기록은 스위치 값도 남긴다.
    let audit: Vec<(Value, Value)> = sqlx::query_as(
        "SELECT before_snapshot,after_snapshot FROM gw_audit_events WHERE operation='set_settlement_rule' ORDER BY id",
    )
    .fetch_all(h.db.store.pool())
    .await
    .unwrap();
    assert_eq!(
        audit,
        vec![
            (
                json!({"provider":"anthropic","cutoff_rule":"received","correction":false,"version":"1"}),
                json!({"provider":"anthropic","cutoff_rule":"received","correction":true,"version":"2"})
            ),
            (
                json!({"provider":"anthropic","cutoff_rule":"received","correction":true,"version":"2"}),
                json!({"provider":"anthropic","cutoff_rule":"max_tokens","correction":true,"version":"3"})
            )
        ]
    );
    // 끈다.
    let off = h.set_rule("max_tokens", Some(false)).await;
    assert_eq!(off["cutoff_rule"]["correction"], false);
    assert_eq!(off["result_version"], "4");
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn calibration_counts_only_fully_known_minutes_and_each_minute_once() {
    let h = Check::new(true).await;
    let one = h.add_key("team-1", KEY_ONE).await;
    let key = "apikey_01one";
    h.admin_api.set_keys(&[(key, Some(hint(KEY_ONE)))]);
    h.set_rule("received", Some(true)).await;
    let now = now_utc();
    let m = |ago: i64| minute_ago(now, ago);
    let cell = |ago: i64, n: [u64; 5]| h.admin_api.add_minute(m(ago), key, "synthetic-text", n);
    // 보고서는 요청을 끝난 분에 넣는다. 앞 분에 보내 이 분에 끝난 요청 하나뿐인 분은 끝난 분 기준으로만 맞는다.
    for (ago, output) in [(70, 10), (60, 20), (50, 30)] {
        let id = h.settled(one, 100, output).await;
        h.place(id, m(ago + 1) + 20, m(ago) + 30).await;
        cell(ago, [100, 0, 0, 0, output]);
    }
    // 아래 분들은 걸리는 것이 없으면 끝난 분 기준으로 맞았을 분이지만 세지 않는다.
    // 끊긴 요청이 걸친 분(끝난 지 2시간이 넘어 보정 후보는 아니다).
    let before_cut = h.settled(one, 100, 40).await;
    h.place(before_cut, m(126) + 20, m(125) + 30).await;
    let cut = h.cut_off(one, 100, 5).await;
    h.place(cut, m(125) + 10, m(125) + 45).await;
    cell(125, [200, 0, 0, 0, 45]);
    // 분 경계 3초 안에 끝난 요청이 있는 분(시계 차이로 다음 분에 들어갈 수 있다).
    let inner = h.settled(one, 100, 60).await;
    h.place(inner, m(101) + 20, m(100) + 30).await;
    let edge = h.settled(one, 100, 70).await;
    h.place(edge, m(101) + 40, m(100) + 59).await;
    cell(100, [200, 0, 0, 0, 130]);
    // 보고서에 칸이 없는 분.
    let bare = h.settled(one, 100, 80).await;
    h.place(bare, m(91) + 20, m(90) + 30).await;
    // 보정 후보: 20분 전에 끊긴 요청. 칸의 입력이 맞지 않아 고치지 않고, 다음 조회도 1분 칸을 받게 한다.
    let candidate = h.cut_off(one, 100, 5).await;
    h.place(candidate, m(21) + 10, m(20) + 40).await;
    cell(20, [150, 0, 0, 0, 60]);
    // 모델을 모르는 요청(미정산)이 걸친 분. 이 사용자의 다음 송신을 막으므로 마지막에 만든다.
    let known = h.settled(one, 100, 50).await;
    h.place(known, m(111) + 20, m(110) + 30).await;
    let unknown = h.dispatched(one, false).await;
    h.unknown(unknown).await;
    h.place(unknown, m(110) + 15, m(110) + 50).await;
    cell(110, [100, 0, 0, 0, 50]);

    h.poll().await.unwrap();
    // 1분 칸은 2시간 20분 전부터 10분 전까지, 그 구간 요청의 API 키로 묻는다.
    let minutes = h.admin_api.minute_calls();
    assert_eq!(minutes.len(), 1);
    let from = epoch(minutes[0].one("starting_at").unwrap());
    let to = epoch(minutes[0].one("ending_at").unwrap());
    assert!(
        to == m(10) || to == m(10) + MINUTE,
        "끝은 10분 전이 속한 분이다"
    );
    assert_eq!(to - from, 130 * MINUTE);
    assert_eq!(
        (
            minutes[0].one("limit"),
            minutes[0].all("group_by[]"),
            minutes[0].all("api_key_ids[]")
        ),
        (Some("1440"), vec!["api_key_id", "model"], vec![key])
    );
    // 끝난 분 기준으로만 맞은 분 셋을 세어 기준을 정한다. 화면도 그 값을 보인다.
    let state = h.view("24h").await["state"].clone();
    assert_eq!(
        (
            &state["minute_convention"],
            &state["start_minutes"],
            &state["end_minutes"]
        ),
        (&json!("end"), &json!("0"), &json!("3"))
    );
    // 칸이 맞지 않는 후보는 고치지 않는다.
    assert!(h.corrections().await.is_empty());
    // 다음 조회도 1분 칸을 받지만(고칠 후보가 있다) 이미 센 분은 다시 세지 않는다.
    h.release_lease().await;
    h.poll().await.unwrap();
    assert_eq!(h.admin_api.minute_calls().len(), 2);
    let state = h.view("24h").await["state"].clone();
    assert_eq!(
        (&state["start_minutes"], &state["end_minutes"]),
        (&json!("0"), &json!("3"))
    );
    assert!(h.corrections().await.is_empty());
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn nothing_is_corrected_before_the_rule_is_known() {
    let h = Check::new(true).await;
    let one = h.add_key("team-1", KEY_ONE).await;
    let key = "apikey_01one";
    h.admin_api.set_keys(&[(key, Some(hint(KEY_ONE)))]);
    h.set_rule("received", Some(true)).await;
    let minute = minute_ago(now_utc(), 40);
    // 보낸 분과 끝난 분이 같은 끊긴 요청. 보고서 칸은 어느 기준으로 보아도 이 요청과 맞는다.
    let cut = h.cut_off(one, 100, 5).await;
    h.place(cut, minute + 10, minute + 40).await;
    h.admin_api
        .add_minute(minute, key, "synthetic-text", [100, 0, 0, 0, 50]);
    // 기준을 모르면 1분 칸을 받아 기준만 센다(끊긴 요청이 걸친 분이라 세지 않는다). 고치지 않는다.
    h.poll().await.unwrap();
    assert_eq!(h.admin_api.minute_calls().len(), 1);
    assert!(h.corrections().await.is_empty());
    assert_eq!(h.cost(cut).await, "0.000110000000");
    let state = h.view("24h").await["state"].clone();
    assert_eq!(
        (
            &state["minute_convention"],
            &state["start_minutes"],
            &state["end_minutes"]
        ),
        (&Value::Null, &json!("0"), &json!("0"))
    );
    // 기준이 끝난 분으로 정해지면(센 값을 바로 넣는다) 다음 조회에서 고친다: 입력 100×1 + 출력 50×2.
    h.set_minutes(0, 3).await;
    h.release_lease().await;
    h.poll().await.unwrap();
    assert_eq!(h.corrections().await, vec![(cut, "50".to_owned())]);
    assert_eq!(h.cost(cut).await, "0.000200000000");
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn correction_changes_cost_and_output_in_the_original_period_and_is_audited() {
    let h = Check::new(true).await;
    let one = h.add_key("team-1", KEY_ONE).await;
    let api_key = "apikey_01one";
    h.admin_api.set_keys(&[(api_key, Some(hint(KEY_ONE)))]);
    let key = h.virtual_key(one).await;
    // 시작 event(입력 100) 뒤에 "Hello"(5 token)를 받고 끊긴 스트림. `received` 규칙으로 바로 정산한다.
    let id = h.cut_stream(&key, "Hello").await;
    let pool = h.db.store.pool();
    assert_eq!(
        finance::settlement(pool, id).await,
        finance::cutoff_received("0.000110000000", 5)
    );
    // 게이트웨이는 가상 키에 연결된 Provider 키로 보냈고, 그 키를 요청에 남겼다.
    let sent: Option<Uuid> =
        sqlx::query_scalar("SELECT provider_credential_id FROM gw_requests WHERE id=$1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(sent, Some(one));
    assert!(
        h.provider
            .calls
            .lock()
            .await
            .iter()
            .all(|c| c.headers["x-api-key"] == KEY_ONE)
    );
    // 요청을 90분 전에 보내고 끝난 것으로 옮기고, 지난 예산 기간에 정산된 것으로 둔다.
    let minute = minute_ago(now_utc(), 90);
    h.place(id, minute + 10, minute + 40).await;
    h.to_previous_period(id).await;
    // 보고서의 1분 칸: 입력 100, 출력 40. 1시간 칸도 같다. 기준은 끝난 분이다(센 값을 바로 넣는다).
    let report = |output: u64| {
        h.admin_api.clear_usage();
        h.admin_api
            .add_minute(minute, api_key, "synthetic-text", [100, 0, 0, 0, output]);
        h.admin_api.add(
            minute.div_euclid(HOUR) * HOUR,
            api_key,
            "synthetic-text",
            [100, 0, 0, 0, 40],
        );
    };
    report(40);
    h.set_minutes(0, 3).await;
    let switched = h.set_rule("received", Some(true)).await;
    assert_eq!(switched["cutoff_rule"]["correction"], true);
    h.poll().await.unwrap();
    assert_eq!(h.admin_api.minute_calls().len(), 1);
    // 요청의 금액을 보고서의 출력으로 고친다: 입력 100×1 + 출력 40×2 = 0.000180. 처음 정산 기록(final)은 그대로다.
    assert_eq!(
        finance::settlement(pool, id).await,
        finance::cutoff_received("0.000180000000", 5)
    );
    // 원래(지난) 예산 기간의 합계를 고치고 지금 기간은 그대로 둔다. 사용자·가상 키 범위 모두다.
    let totals: Vec<(String, i64, i64, i64, bool)> = sqlx::query_as("SELECT cost_usd::text,settled_count,input_tokens,output_tokens,period_start<date_trunc('month',clock_timestamp() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC' FROM gw_usage_totals ORDER BY period_start,scope_id")
        .fetch_all(pool)
        .await
        .unwrap();
    let previous = ("0.000180000000".to_owned(), 1, 100, 40, true);
    let current = ("0.000000000000".to_owned(), 0, 0, 0, false);
    assert_eq!(
        totals,
        vec![previous.clone(), previous, current.clone(), current]
    );
    let budgets: Vec<(String, i64)> = sqlx::query_as(
        "SELECT applied_cost_usd::text,applied_output_tokens FROM gw_request_budgets WHERE request_id=$1",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(budgets, vec![("0.000180000000".to_owned(), 40); 2]);
    // usage 기록(correction): 고친 usage, 근거(보고서 칸), 금액.
    let (normalized, evidence, resolution, reason): (Value, Value, Value, Option<String>) =
        sqlx::query_as("SELECT normalized_usage,provider_usage,pricing_resolution,reason_code FROM gw_usage_events WHERE request_id=$1 AND kind='correction'")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(
        normalized,
        json!({"source":"provider_report","actual_model":"synthetic-text","input_tokens":"100","output_tokens":"40","cache_read_tokens":"0","cache_write_tokens":"0","cache_write_5m_tokens":"0","cache_write_1h_tokens":"0","reasoning_tokens":null,"counted_input_tokens":null,"cutoff_rule":"received","cutoff_fallback_reason":null,"previous_output_tokens":"5","previous_source":"cutoff_received"})
    );
    assert_eq!(
        evidence,
        json!({"source":"anthropic_usage_report","bucket_width":"1m","minute":rfc3339(minute),"convention":"end","api_key_id":api_key,"model":"synthetic-text","report":{"uncached_input_tokens":100,"cache_read_tokens":0,"cache_write_5m_tokens":0,"cache_write_1h_tokens":0,"output_tokens":40},"other_requests":0})
    );
    assert_eq!(
        resolution,
        json!({"pricing_version":"synthetic-price-v1","cost_usd":"0.000180000000","previous_cost_usd":"0.000110000000","rounding_version":"sum_half_up_12_v1"})
    );
    assert_eq!(reason.as_deref(), Some("usage_correction"));
    // 시스템 감사 기록(usage_correction).
    type Audit = (
        String,
        Option<Uuid>,
        Option<Uuid>,
        bool,
        Option<Uuid>,
        String,
        Uuid,
        Value,
        Value,
    );
    let audit: Vec<Audit> = sqlx::query_as("SELECT actor_kind,actor_principal_id,command_id,executor_id IS NOT NULL,request_id,target_kind,target_id,before_snapshot,after_snapshot FROM gw_audit_events WHERE operation='usage_correction'")
        .fetch_all(pool)
        .await
        .unwrap();
    assert_eq!(
        audit,
        vec![(
            "system".into(),
            None,
            None,
            true,
            Some(id),
            "request".into(),
            id,
            json!({"cost_usd":"0.000110000000","output_tokens":"5","source":"cutoff_received"}),
            json!({"cost_usd":"0.000180000000","output_tokens":"40","source":"provider_report"})
        )]
    );
    // 관리 화면의 요청 상세가 보정을 보여 준다.
    let detail = h.get(&format!("/api/admin/requests/{id}")).await;
    assert_eq!(detail["cost_usd"], "0.000180000000");
    assert_eq!(detail["usage"]["correction"], normalized);
    assert_eq!(detail["usage"]["final"]["output_tokens"], "5");
    assert_eq!(detail["usage"]["final"]["source"], "cutoff_received");

    // 다시 조회해도 다시 고치지 않는다. 보고서 값이 바뀌어도 그대로고, 고칠 후보가 없어 1분 칸도 받지 않는다.
    report(45);
    h.release_lease().await;
    h.poll().await.unwrap();
    assert_eq!(h.admin_api.minute_calls().len(), 1);
    assert_eq!(h.corrections().await, vec![(id, "40".to_owned())]);
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gw_audit_events WHERE operation='usage_correction'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(audits, 1);
    assert_eq!(h.cost(id).await, "0.000180000000");
    // 화면은 이 조회가 다시 센 게이트웨이 쪽 합계로 고친 출력을 센다. 보고서와 같아 차이가 없다.
    let view = h.view("24h").await;
    assert_eq!(view["totals"]["corrected_requests"], "1");
    assert_eq!(
        (
            &view["totals"]["provider_tokens"],
            &view["totals"]["gateway_tokens"],
            &view["totals"]["provider_more_tokens"],
            &view["totals"]["gateway_more_tokens"]
        ),
        (&json!("140"), &json!("140"), &json!("0"), &json!("0"))
    );
    // 장부도 같은 요청을 두 번 고치지 않는다.
    assert!(!h.ledger.apply_correction(id, 45, &json!({})).await.unwrap());
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn correction_is_refused_when_unsafe() {
    let h = Check::new(true).await;
    let one = h.add_key("team-1", KEY_ONE).await;
    let key = "apikey_01one";
    h.admin_api.set_keys(&[(key, Some(hint(KEY_ONE)))]);
    h.set_minutes(0, 3).await;
    let now = now_utc();
    let m = |ago: i64| minute_ago(now, ago);
    let cell = |ago: i64, n: [u64; 5]| h.admin_api.add_minute(m(ago), key, "synthetic-text", n);
    // 고칠 수 있는 요청 둘(대조군). 캐시 토큰은 캐시 읽기, 5분 캐시 쓰기, 1시간 캐시 쓰기가 모두 맞아야 한다.
    // 칸 값은 [입력, 5분 캐시 쓰기, 1시간 캐시 쓰기, 캐시 읽기, 출력]이다.
    let plain = h.cut_off(one, 100, 5).await;
    h.place(plain, m(25) + 10, m(25) + 30).await;
    cell(25, [100, 0, 0, 0, 50]);
    let cached = h.cut_off_with(one, [100, 8, 4, 6], 5).await;
    h.place(cached, m(27) + 10, m(27) + 30).await;
    cell(27, [100, 4, 6, 8, 25]);
    // 입력이 맞지 않는다(보고서 입력 120).
    let input = h.cut_off(one, 100, 5).await;
    h.place(input, m(30) + 10, m(30) + 30).await;
    cell(30, [120, 0, 0, 0, 50]);
    // 캐시 쓰기가 맞지 않는다(보고서 4+5, 요청 4+6).
    let cache = h.cut_off_with(one, [100, 8, 4, 6], 5).await;
    h.place(cache, m(32) + 10, m(32) + 30).await;
    cell(32, [100, 4, 5, 8, 25]);
    // 캐시 쓰기 합계는 같지만 5분·1시간 몫이 다르다(보고서 6+4, 요청 4+6). 따로 비교한다(결정 0012).
    let split = h.cut_off_with(one, [100, 8, 4, 6], 5).await;
    h.place(split, m(33) + 10, m(33) + 30).await;
    cell(33, [100, 6, 4, 8, 25]);
    // 같은 분에 끊긴 요청이 또 있으면 출력을 나눌 수 없다.
    let first = h.cut_off(one, 100, 5).await;
    let second = h.cut_off(one, 100, 7).await;
    h.place(first, m(35) + 10, m(35) + 20).await;
    h.place(second, m(35) + 15, m(35) + 40).await;
    cell(35, [200, 0, 0, 0, 80]);
    // 분 경계 5초 안에 끝났다(끝 경계 3초 전, 시작 경계 3초 뒤).
    let late = h.cut_off(one, 100, 5).await;
    h.place(late, m(41) + 30, m(40) + 57).await;
    cell(40, [100, 0, 0, 0, 50]);
    let early = h.cut_off(one, 100, 5).await;
    h.place(early, m(46) + 30, m(45) + 3).await;
    cell(45, [100, 0, 0, 0, 50]);
    // 같은 분에 끝난, 모델을 모르는 요청(미정산)이 있다. 이 사용자의 다음 송신을 막으므로 마지막에 만든다.
    let beside = h.cut_off(one, 100, 5).await;
    h.place(beside, m(50) + 10, m(50) + 30).await;
    cell(50, [100, 0, 0, 0, 50]);
    let unknown = h.dispatched(one, false).await;
    h.unknown(unknown).await;
    h.place(unknown, m(50) + 20, m(50) + 45).await;
    // 스위치가 꺼져 있으면(기본값) 1분 칸을 받지 않고 아무것도 고치지 않는다.
    h.poll().await.unwrap();
    assert!(h.admin_api.minute_calls().is_empty());
    assert!(h.corrections().await.is_empty());
    // 켜면 대조군만 고친다. 캐시 요청: 입력 100×1 + 캐시 읽기 8×0.1 + 5분 캐시 쓰기 4×1.25
    // + 1시간 캐시 쓰기 6×2 + 출력 25×2.
    h.set_rule("received", Some(true)).await;
    h.release_lease().await;
    h.poll().await.unwrap();
    let mut fixed = vec![(plain, "50".to_owned()), (cached, "25".to_owned())];
    fixed.sort();
    assert_eq!(h.corrections().await, fixed);
    assert_eq!(h.cost(cached).await, "0.000167800000");
    for (id, cost) in [
        (input, "0.000110000000"),
        (cache, "0.000127800000"),
        (split, "0.000127800000"),
        (first, "0.000110000000"),
        (second, "0.000114000000"),
        (late, "0.000110000000"),
        (early, "0.000110000000"),
        (beside, "0.000110000000"),
    ] {
        assert_eq!(h.cost(id).await, cost, "{id}");
    }
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn start_rule_corrects_from_the_minute_the_request_was_sent() {
    let h = Check::new(true).await;
    let one = h.add_key("team-1", KEY_ONE).await;
    let key = "apikey_01one";
    h.admin_api.set_keys(&[(key, Some(hint(KEY_ONE)))]);
    h.set_minutes(3, 0).await;
    h.set_rule("received", Some(true)).await;
    let now = now_utc();
    let m = |ago: i64| minute_ago(now, ago);
    let cell = |ago: i64, n: [u64; 5]| h.admin_api.add_minute(m(ago), key, "synthetic-text", n);
    // 40분 전에 보내 37분 전에 끊긴 요청. 보고서는 보낸 분에 넣는다(끝난 분의 칸은 다른 사용이다).
    let long = h.cut_off(one, 100, 5).await;
    h.place(long, m(40) + 10, m(37) + 20).await;
    cell(40, [100, 0, 0, 0, 60]);
    cell(37, [37, 0, 0, 0, 9]);
    // 같은 분에 보낸 정산 요청의 사용량을 빼고 남은 출력이 끊긴 요청의 것이다.
    // 그 분에 끝났지만 앞 분에 보낸 요청은 그 분의 칸에 없으므로 빼지 않는다.
    let beside = h.settled(one, 100, 20).await;
    h.place(beside, m(50) + 15, m(49) + 30).await;
    let earlier = h.settled(one, 30, 3).await;
    h.place(earlier, m(51) + 20, m(50) + 40).await;
    let cut = h.cut_off(one, 100, 5).await;
    h.place(cut, m(50) + 20, m(48) + 30).await;
    cell(50, [200, 0, 0, 0, 70]);
    h.poll().await.unwrap();
    let mut fixed = vec![(long, "60".to_owned()), (cut, "50".to_owned())];
    fixed.sort();
    assert_eq!(h.corrections().await, fixed);
    assert_eq!(
        h.evidence(long).await,
        (rfc3339(m(40)), "start".to_owned(), 0)
    );
    assert_eq!(
        h.evidence(cut).await,
        (rfc3339(m(50)), "start".to_owned(), 1)
    );
    let state = h.view("24h").await["state"].clone();
    assert_eq!(
        (
            &state["minute_convention"],
            &state["start_minutes"],
            &state["end_minutes"]
        ),
        (&json!("start"), &json!("3"), &json!("0"))
    );
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn a_key_matched_later_gets_its_own_thirty_days() {
    let h = Check::new(true).await;
    let one = h.add_key("team-1", KEY_ONE).await;
    let two = h.add_key("team-2", KEY_TWO).await;
    let (k1, k2) = ("apikey_01one", "apikey_02two");
    // 처음에는 첫 키만 조직의 키 목록에 있다.
    h.admin_api.set_keys(&[(k1, Some(hint(KEY_ONE)))]);
    let t = settled_hour();
    // 두 키 모두 8시간 전과 3시간 전에 요청이 있고, 보고서도 같은 값이다. 둘째 키는 20일 전에 다른 모델 사용도 있다.
    for (credential, key) in [(one, k1), (two, k2)] {
        for k in [8, 3] {
            let id = h.settled(credential, 100, 20).await;
            h.place(id, t - k * HOUR + 600, t - k * HOUR + 1200).await;
            h.admin_api
                .add(t - k * HOUR, key, "synthetic-text", [100, 0, 0, 0, 20]);
        }
    }
    h.admin_api
        .add(t - 20 * DAY, k2, "claude-old", [7, 0, 0, 0, 7]);
    let span =
        |c: &AdminCall| epoch(c.one("ending_at").unwrap()) - epoch(c.one("starting_at").unwrap());
    // 첫 조회는 30일, 다음 조회는 최근 48시간을 첫 키로만 묻는다.
    h.poll().await.unwrap();
    h.release_lease().await;
    h.poll().await.unwrap();
    let calls = h.admin_api.calls(USAGE);
    assert_eq!(span(&calls[0]), 30 * DAY);
    assert_eq!(span(calls.last().unwrap()), 2 * DAY);
    assert!(calls.iter().all(|c| c.all("api_key_ids[]") == [k1]));
    // 둘째 키가 목록에 나타나 짝지어진다. 보고서는 5시간 전 칸부터 아직 채워지지 않았다.
    h.admin_api
        .set_keys(&[(k1, Some(hint(KEY_ONE))), (k2, Some(hint(KEY_TWO)))]);
    h.admin_api.lag_until(Some(t - 5 * HOUR));
    h.release_lease().await;
    h.poll().await.unwrap();
    // 처음 짝지은 키가 있으면 두 키를 함께 30일 묻는다.
    let third = &h.admin_api.calls(USAGE)[calls.len()..];
    assert_eq!(span(&third[0]), 30 * DAY);
    assert!(third.iter().all(|c| c.all("api_key_ids[]") == [k1, k2]));
    // 키마다 받아 둔 시간만 비교한다. 첫 키는 3시간 전 칸까지 받아 두었고, 둘째 키는 보고서가 돌려준
    // 5시간 전까지만 받았다. 둘째 키의 20일 전 사용은 이번에 받았다.
    let row = |key: &str, model: &str, provider: &str, gateway: &str| {
        (
            (key.to_owned(), model.to_owned()),
            (provider.to_owned(), gateway.to_owned()),
        )
    };
    assert_eq!(
        h.rows("30d").await,
        BTreeMap::from([
            row(k1, "synthetic-text", "240", "240"),
            row(k2, "claude-old", "14", "0"),
            row(k2, "synthetic-text", "120", "120"),
        ])
    );
    // 보고서가 채워지면 둘째 키도 최근 48시간을 받아(30일을 다시 받지는 않는다) 3시간 전 칸을 비교한다.
    h.admin_api.lag_until(None);
    let seen = h.admin_api.calls(USAGE).len();
    h.release_lease().await;
    h.poll().await.unwrap();
    assert_eq!(span(&h.admin_api.calls(USAGE)[seen]), 2 * DAY);
    assert_eq!(
        h.rows("30d").await,
        BTreeMap::from([
            row(k1, "synthetic-text", "240", "240"),
            row(k2, "claude-old", "14", "0"),
            row(k2, "synthetic-text", "240", "240"),
        ])
    );
    h.close().await;
}

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn unknown_reason_counts_requests_with_usage_but_unknown_accounting() {
    let h = Check::new(true).await;
    let one = h.add_key("team-1", KEY_ONE).await;
    let key = "apikey_01one";
    h.admin_api.set_keys(&[(key, Some(hint(KEY_ONE)))]);
    let t = settled_hour();
    let hour = |k: i64| t - k * HOUR;
    // 4시간 전: Provider가 HTTP 오류로 답해 0원으로 정산한 요청. 규칙으로 정산했으므로 사용량을 모르는 요청이 아니다.
    // 그 시간에 Provider가 센 입력 30은 원인 모름이다.
    let failed = h.no_response(one).await;
    h.place(failed, hour(4) + 600, hour(4) + 660).await;
    h.admin_api
        .add(hour(4), key, "synthetic-text", [30, 0, 0, 0, 0]);
    // 6시간 전: 응답의 모델이 profile에 없어 금액을 정하지 못한 요청. 사용량 기록은 있지만 미정산이라
    // 게이트웨이 합계에서 빠지고, 사용량을 모르는 요청으로 센다. 이 사용자의 다음 송신을 막으므로 마지막에 만든다.
    let id = h.dispatched(one, false).await;
    h.unpriced(id).await;
    h.place(id, hour(6) + 600, hour(6) + 1200).await;
    h.admin_api
        .add(hour(6), key, "synthetic-other", [100, 0, 0, 0, 20]);
    h.poll().await.unwrap();
    let view = h.view("24h").await;
    let credential = json!({"id":one,"label":"team-1","hint":"k1AA"});
    assert_eq!(
        view["rows"],
        json!([
            {"credential":credential,"api_key_id":key,"model":"synthetic-other","provider_tokens":"120","gateway_tokens":"0","provider_more_tokens":"120","gateway_more_tokens":"0","provider_more_usd":null,"gateway_more_usd":"0.000000000000","reasons":[{"reason":"unknown","tokens":"120"}]},
            {"credential":credential,"api_key_id":key,"model":"synthetic-text","provider_tokens":"30","gateway_tokens":"0","provider_more_tokens":"30","gateway_more_tokens":"0","provider_more_usd":"0.000030000000","gateway_more_usd":"0.000000000000","reasons":[{"reason":"unexplained","tokens":"30"}]}
        ])
    );
    assert_eq!(
        view["totals"]["reasons"],
        json!([
            {"reason":"cutoff","tokens":"0","usd":"0.000000000000"},
            {"reason":"unknown","tokens":"120","usd":null},
            {"reason":"outside","tokens":"0","usd":"0.000000000000"},
            {"reason":"unexplained","tokens":"30","usd":"0.000030000000"}
        ])
    );
    assert_eq!(
        view["totals"]["unpriced_models"],
        json!(["synthetic-other"])
    );
    h.close().await;
}

/// 교체한 Provider 키의 새 원문(새 Anthropic API 키).
const KEY_NEW: &str = "sk-ant-api03-K9nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn-k9NN";

#[tokio::test]
#[ignore = "requires explicit dedicated TEST_DATABASE_URL"]
async fn replaced_provider_key_keeps_past_requests_on_its_old_api_key_id() {
    let h = Check::new(true).await;
    let one = h.add_key("team-1", KEY_ONE).await;
    let (old, new) = ("apikey_01one", "apikey_09new");
    h.admin_api.set_keys(&[(old, Some(hint(KEY_ONE)))]);
    let t = settled_hour();
    // 8시간 전: 옛 원문으로 보낸 요청. 보고서도 옛 API 키 ID로 같은 값을 알린다.
    let before = h.settled(one, 100, 20).await;
    h.place(before, t - 8 * HOUR + 600, t - 8 * HOUR + 1200)
        .await;
    h.admin_api
        .add(t - 8 * HOUR, old, "synthetic-text", [100, 0, 0, 0, 20]);
    h.poll().await.unwrap();
    // Provider 키의 원문을 바꾼다. 교체는 새 Provider 키 행을 만들고 옛 행을 해제한다.
    let (status, body) = h
        .api(
            &h.alice,
            reqwest::Method::PUT,
            &format!("/api/admin/provider-credentials/anthropic/{one}"),
            Some(json!({"expected_version":"1","secret":KEY_NEW})),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let replaced: Uuid = body["target"]["id"].as_str().unwrap().parse().unwrap();
    assert_ne!(replaced, one);
    h.service.refresh().await.unwrap();
    // 옛 API 키도 조직에 남아 있다. 3시간 전: 새 원문으로 보낸 요청.
    h.admin_api
        .set_keys(&[(old, Some(hint(KEY_ONE))), (new, Some(hint(KEY_NEW)))]);
    let after = h.settled(replaced, 100, 30).await;
    h.place(after, t - 3 * HOUR + 600, t - 3 * HOUR + 1200)
        .await;
    h.admin_api
        .add(t - 3 * HOUR, new, "synthetic-text", [100, 0, 0, 0, 30]);
    h.release_lease().await;
    h.poll().await.unwrap();
    // 바꾸기 전 요청은 옛 API 키, 바꾼 뒤 요청은 새 API 키의 사용량과 비교한다. 두 키 모두 차이가 없고,
    // 해제한 옛 Provider 키도 행에 이름을 보인다.
    let view = h.view("24h").await;
    let rows: Vec<(Value, Value, Value, Value)> = view["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["api_key_id"].clone(),
                r["credential"].clone(),
                r["provider_tokens"].clone(),
                r["gateway_tokens"].clone(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            (
                json!(old),
                json!({"id":one,"label":"team-1","hint":"k1AA"}),
                json!("120"),
                json!("120")
            ),
            (
                json!(new),
                json!({"id":replaced,"label":"team-1","hint":"k9NN"}),
                json!("130"),
                json!("130")
            ),
        ]
    );
    assert_eq!(view["totals"]["gateway_more_tokens"], "0");
    assert_eq!(view["unmatched_credentials"], json!([]));
    h.close().await;
}
