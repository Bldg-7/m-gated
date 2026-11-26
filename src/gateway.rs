use crate::{
    auth::{AuthError, SsoAuthenticator},
    config::{ModelProfile, ProviderConfig, ProviderId, ValidatedConfig},
    http,
    ledger::{
        PgLedger,
        finance::{FinalObservation, FinalOutcome, LedgerError, NoUsage, RequestSpec},
    },
    providers::{
        port::{self, ApiError, ErrorFormat},
        transport::{NativeClient, SendOptions, UpstreamError},
    },
    runtime::request::RequestPhase,
    runtime::{HttpAdmission, RequestLease, RouteClass, Runtime},
};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use http_body::Body as _;
use std::{
    collections::{BTreeMap, BTreeSet},
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::Instant;
use uuid::Uuid;

pub struct InferenceGateway {
    auth: Arc<SsoAuthenticator>,
    ledger: Arc<PgLedger>,
    clients: BTreeMap<ProviderId, NativeClient>,
    profiles: Vec<ModelProfile>,
    catalog: Option<Arc<crate::catalog::CatalogCache>>,
}
#[derive(Clone)]
struct GatewayState {
    gateway: Arc<InferenceGateway>,
    runtime: Arc<Runtime>,
}
/// 경로의 API가 허용한 query만 받는다([`port::Api::query_allowed`]). query는 upstream에 전달하지 않는다.
fn inference_query_allowed(path: &str, query: Option<&str>) -> bool {
    match query {
        None => true,
        Some(q) => port::route(path).is_some_and(|(_, api)| api.query_allowed(q)),
    }
}
/// 경로의 API가 쓰는 오류 형식. 게이트웨이 경로가 아니면 Anthropic 형식을 쓴다.
fn error_format(path: &str) -> ErrorFormat {
    port::route(path).map_or(ErrorFormat::Anthropic, |(_, api)| api.error_format())
}
/// 게이트웨이가 만든 오류를 경로의 API가 쓰는 형식으로 바꾼다.
pub(crate) fn project_error(path: &str, response: Response) -> Response {
    project(error_format(path), response)
}
fn project(format: ErrorFormat, response: Response) -> Response {
    match format {
        ErrorFormat::Anthropic => response,
        ErrorFormat::Openai => openai_error(response),
        ErrorFormat::Detail => detail_error(response),
    }
}
impl InferenceGateway {
    pub fn new(
        config: &ValidatedConfig,
        auth: Arc<SsoAuthenticator>,
        ledger: Arc<PgLedger>,
        credentials: Arc<crate::credentials::CredentialCache>,
    ) -> Result<Self, ApiError> {
        let mut clients = BTreeMap::new();
        let mut profiles = Vec::new();
        for provider in config.providers() {
            let client = NativeClient::from_config(
                provider,
                config.auth().ca_certificate.as_deref(),
                clients.values().next(),
                credentials.clone(),
            )?;
            clients.insert(provider.provider, client);
            profiles.extend(provider.profiles.clone());
        }
        Ok(Self {
            auth,
            ledger,
            clients,
            profiles,
            catalog: None,
        })
    }
    pub fn assemble(
        provider: &ProviderConfig,
        auth: Arc<SsoAuthenticator>,
        ledger: Arc<PgLedger>,
        client: NativeClient,
    ) -> Self {
        Self {
            auth,
            ledger,
            clients: BTreeMap::from([(provider.provider, client)]),
            profiles: provider.profiles.clone(),
            catalog: None,
        }
    }
    /// 관리자가 승인한 DB 카탈로그 모델도 받는다. 설정 파일 profile 이 먼저다.
    pub fn with_catalog(mut self, catalog: Arc<crate::catalog::CatalogCache>) -> Self {
        self.catalog = Some(catalog);
        self
    }
    pub fn ledger(&self) -> &Arc<PgLedger> {
        &self.ledger
    }
    pub fn paths(&self) -> Vec<&'static str> {
        self.clients
            .keys()
            .flat_map(|id| port::provider(*id).apis())
            .flat_map(|api| api.routes().iter().copied())
            .collect()
    }
    pub fn routes(self: &Arc<Self>, runtime: Arc<Runtime>) -> Router {
        self.paths()
            .into_iter()
            .fold(Router::new(), |routes, path| {
                routes.route(path, any(handle))
            })
            .with_state(GatewayState {
                gateway: self.clone(),
                runtime,
            })
    }
}
async fn handle(State(state): State<GatewayState>, request: Request) -> Response {
    let format = error_format(request.uri().path());
    let response = handle_inner(state, request).await;
    project(format, response)
}
async fn handle_inner(state: GatewayState, request: Request) -> Response {
    let format = error_format(request.uri().path());
    let id = Uuid::new_v4();
    if request.method() != Method::POST {
        let mut r = error(
            id,
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            true,
        );
        r.headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("POST"));
        return r;
    }
    let http = match request.extensions().get::<HttpAdmission>() {
        Some(http) => http.clone(),
        None => return error(id, StatusCode::SERVICE_UNAVAILABLE, "not_ready", true),
    };
    let lease = match state
        .runtime
        .try_admit_with_http(RouteClass::Inference, http)
    {
        Ok(l) => l,
        Err(e) => return native_admission(id, e),
    };
    let started = lease.started();
    let (control, registration) = state.runtime.register_request(id, started);
    let owner = crate::runtime::RequestOwner::new(lease, control.clone(), registration);
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut wait = crate::runtime::request::ResponseWait::new(rx, control.clone());
    let runtime = state.runtime.clone();
    let task = tokio::spawn(async move {
        use futures_util::FutureExt;
        let mut lease = owner.lease.lock().await;
        let mut delivery = Some(tx);
        let mut progress = ExecutionProgress::default();
        let mut interruption = None;
        let mut panicked = false;
        let result = {
            let work = std::panic::AssertUnwindSafe(execute(
                &state,
                request,
                &mut lease,
                id,
                started,
                &control,
                &owner,
                &mut delivery,
                &mut progress,
            ))
            .catch_unwind();
            tokio::select! { biased;
                cause=control.interrupted()=>{interruption=Some(cause);None},
                result=work=>match result {
                    Ok(response)=>Some(response),
                    Err(_)=>{panicked=true;None},
                },
            }
        };
        // 실행 future를 drop한 다음에만 stopped를 기록한다. Drop에는 DB/spawn이 없다.
        if progress.admitted && !progress.finalized && !progress.uncertain {
            use crate::runtime::request::RequestInterruption;
            let mut observation = failed_observation(if progress.stream.provider_error_observed {
                "provider_error"
            } else if panicked {
                "internal_error"
            } else if interruption == Some(RequestInterruption::ShutdownCancelled) {
                "shutdown_cancelled"
            } else if interruption == Some(RequestInterruption::ClientCancelled) {
                "client_cancelled"
            } else {
                "upstream_read_timeout"
            });
            // snapshot은 완전한 wire/terminal만 보존한다. 가격/alias/lease 판정은 원래 ledger 경로다.
            if !panicked && !progress.stream.provider_error_observed {
                observation.usage = progress.stream.final_usage.take();
            }
            // SDK가 native 오류를 읽고 body를 abort해도 이미 관측한 실패 terminal을 취소로 덮지 않는다.
            observation.outcome = if progress.stream.provider_error_observed
                || panicked
                || interruption == Some(RequestInterruption::TransportDeadline)
            {
                FinalOutcome::Failed
            } else {
                FinalOutcome::Cancelled
            };
            let deadline = control.begin_finalizing();
            let settlement = no_usage(&state, id, &progress, &observation, None, deadline).await;
            let _ = finalize_with(&state, id, &observation, &settlement, deadline).await;
        }
        if progress.admitted
            && !progress.uncertain
            && tokio::time::timeout_at(
                control.begin_finalizing(),
                state.gateway.ledger.record_execution_stopped(id),
            )
            .await
            .map_or(true, |r| r.is_err())
        {
            state.runtime.record_request_finalization_failure(id);
        }
        control.set_phase(RequestPhase::AwaitBodyRelease);
        if let Some(tx) = delivery {
            let response = result.unwrap_or_else(|| {
                error(
                    id,
                    if panicked {
                        StatusCode::INTERNAL_SERVER_ERROR
                    } else {
                        StatusCode::GATEWAY_TIMEOUT
                    },
                    if panicked {
                        "internal_error"
                    } else {
                        "upstream_read_timeout"
                    },
                    progress.dispatch.known_not_dispatched(),
                )
            });
            let response = project(format, response);
            let _ = tx.send(owner.bind(response));
        }
    });
    runtime.attach_request(id, task.abort_handle());
    let response = (&mut wait.receiver).await.unwrap_or_else(|_| {
        error(
            id,
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            false,
        )
    });
    wait.disarm();
    response
}
#[derive(Default)]
struct ExecutionProgress {
    admitted: bool,
    dispatch: DispatchEvidence,
    finalized: bool,
    uncertain: bool,
    /// 스트림 요청인지. 비스트림 요청은 응답 전체를 한 번에 받는다.
    streaming: bool,
    stream: crate::providers::transport::StreamProgress,
    /// 끊긴 스트림을 정산 규칙으로 정산할 수 있는 API일 때, 받은 출력 내용을 셀 때 쓰는 client와 모델.
    counter: Option<CutoffCounter>,
}
/// 요청에 고정한 Provider 키를 그대로 쓴다.
struct CutoffCounter {
    client: crate::providers::transport::NativeClient,
    model: String,
}
#[derive(Default)]
enum DispatchEvidence {
    #[default]
    NotAuthorized,
    MayBeAuthorized,
    Authorized,
}
impl DispatchEvidence {
    fn known_not_dispatched(&self) -> bool {
        matches!(self, Self::NotAuthorized)
    }
}
#[allow(clippy::too_many_arguments)]
async fn execute(
    state: &GatewayState,
    request: Request,
    lease: &mut RequestLease,
    id: Uuid,
    started: Instant,
    control: &Arc<crate::runtime::RequestControl>,
    owner: &Arc<crate::runtime::RequestOwner>,
    delivery: &mut Option<tokio::sync::oneshot::Sender<Response>>,
    progress: &mut ExecutionProgress,
) -> Response {
    if let Err(e) = lease.begin_authentication() {
        return native_admission(id, e);
    }
    let now = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(n) => n.as_secs() as i64,
        Err(_) => {
            return error(
                id,
                StatusCode::SERVICE_UNAVAILABLE,
                "auth_dependency_unavailable",
                true,
            );
        }
    };
    let human = match state
        .gateway
        .auth
        .authenticate_inference(request.headers(), now)
        .await
    {
        Ok(p) => p,
        Err(e) => return native_auth(id, e),
    };
    if let Err(e) = lease.identify(human.current.id) {
        return native_admission(id, e);
    }
    if !inference_query_allowed(request.uri().path(), request.uri().query()) {
        return error(id, StatusCode::BAD_REQUEST, "invalid_request", true);
    }
    let Some((provider, adapter)) = port::route(request.uri().path()) else {
        return error(id, StatusCode::NOT_FOUND, "invalid_request", true);
    };
    let provider = provider.id();
    let api = adapter.kind();
    let encoding = http::body_encoding(request.headers());
    if !http::json_content_type(request.headers())
        || encoding == http::BodyEncoding::Other
        || (encoding == http::BodyEncoding::Zstd && !adapter.accepts_zstd())
    {
        return error(
            id,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            true,
        );
    }
    let limits = state.runtime.limits();
    if request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n > limits.inference_body_bytes as u64)
    {
        return error(id, StatusCode::PAYLOAD_TOO_LARGE, "body_too_large", true);
    }
    control.set_phase(RequestPhase::Receiving);
    let (head, body) = request.into_parts();
    let body = match receive(
        body,
        limits.inference_body_bytes,
        Duration::from_millis(limits.body_timeout_ms),
        Duration::from_millis(limits.body_idle_timeout_ms),
    )
    .await
    {
        Ok(b) => b,
        Err((status, code)) => return error(id, status, code, true),
    };
    // 압축한 본문은 푼 크기에도 같은 상한을 쓴다. 업스트림에는 푼 본문을 보낸다.
    let body = if encoding == http::BodyEncoding::Zstd {
        match http::decode_zstd(body, limits.inference_body_bytes).await {
            Ok(b) => b,
            Err((status, code)) => return error(id, status, code, true),
        }
    } else {
        body
    };
    control.set_phase(RequestPhase::Preparing);
    let model = match request_model(&body).await {
        Ok(m) => m,
        Err(_) => return error(id, StatusCode::BAD_REQUEST, "invalid_request", true),
    };
    // 승인된 카탈로그는 요청 하나 동안 같은 묶음을 쓴다(중간에 새 버전이 승인돼도 섞이지 않는다).
    let catalog = state
        .gateway
        .catalog
        .as_ref()
        .map(|c| c.snapshot())
        .unwrap_or_default();
    let Some(profile) = state
        .gateway
        .profiles
        .iter()
        .chain(catalog.profiles.iter().map(|(_, p)| p))
        .find(|p| p.api == api && p.model == model)
    else {
        return error(id, StatusCode::BAD_REQUEST, "unsupported_model", true);
    };
    if body.len() > profile.max_body_bytes {
        return error(id, StatusCode::PAYLOAD_TOO_LARGE, "body_too_large", true);
    }
    let Some(client) = state.gateway.clients.get(&provider) else {
        return error(id, StatusCode::BAD_REQUEST, "unsupported_model", true);
    };
    // 가상 키에 이 Provider의 Provider 키가 연결돼 있지 않으면 장부 행 없이 거절한다.
    if client.uses_store() && !human.blocked(provider) && human.credential_for(provider).is_none() {
        return error(
            id,
            StatusCode::FORBIDDEN,
            "provider_credential_unbound",
            true,
        );
    }
    // 가상 키가 연결된 Provider 키의 인가가 해제됐으면 장부 행 없이 거절한다.
    if human.blocked(provider) {
        return error(
            id,
            StatusCode::FORBIDDEN,
            "provider_credential_not_authorized",
            true,
        );
    }
    // 회사 키는 장부 입장 전에 요청 하나에 고정한다. 키가 없으면 장부 행 없이 거절한다.
    let Some(client) = client.bind_credential(human.credential_for(provider)) else {
        return error(
            id,
            StatusCode::SERVICE_UNAVAILABLE,
            "provider_credential_unavailable",
            true,
        );
    };
    let client = &client;
    let validated = match adapter.validate(&body, profile).await {
        Ok(m) => m,
        Err(e) => return api_error(id, e),
    };
    if !validated.stream
        && let Err(e) = lease.acquire_nonstream()
    {
        return native_admission(id, e);
    }
    let outbound = match adapter.approved_headers(&head.headers, profile) {
        Ok(h) => h,
        Err(ApiError::AdapterFailed) => return api_error(id, ApiError::AdapterFailed),
        Err(_) => return error(id, StatusCode::BAD_REQUEST, "unsupported_option", true),
    };
    let mut spec = RequestSpec::from_profile(provider, profile, validated.output_limit);
    spec.id = id;
    spec.provider_credential_id = client.credential_id();
    spec.stream = validated.stream;
    spec.deadline = crate::runtime::RequestDeadline::new(
        control.transport_deadline,
        control.execution_deadline,
    );
    if spec.deadline.remaining_at(Instant::now()).is_none() {
        return error(
            id,
            StatusCode::REQUEST_TIMEOUT,
            "request_body_timeout",
            true,
        );
    }

    spec.api_version = outbound
        .get("anthropic-version")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    spec.approved_beta = outbound
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').map(|s| s.trim().to_owned()).collect())
        .unwrap_or_default();
    if let Err(e) = state
        .gateway
        .ledger
        .admit_scoped(&human.current, &spec, human.key, None)
        .await
    {
        return ledger_error(id, e, true);
    }
    progress.admitted = true;
    // authorize await 중 취소/COMMIT 불명에는 permit이 없어도 N을 추정하지 않는다.
    progress.dispatch = DispatchEvidence::MayBeAuthorized;
    control.set_phase(RequestPhase::Dispatching);
    let permit = match state.gateway.ledger.authorize_dispatch(id).await {
        Ok(permit) => {
            progress.dispatch = DispatchEvidence::Authorized;
            permit
        }
        Err(LedgerError::DispatchUncertain) => {
            progress.uncertain = true;
            state.runtime.record_request_finalization_failure(id);
            return error(
                id,
                StatusCode::SERVICE_UNAVAILABLE,
                "dispatch_uncertain",
                false,
            );
        }
        Err(e) => {
            progress.dispatch = DispatchEvidence::NotAuthorized;
            let obs = FinalObservation {
                outcome: FinalOutcome::Rejected,
                usage: None,
                counted_input_tokens: None,
                reason: ledger_code(e),
                http_status: None,
                provider_request_id: None,
            };
            control.begin_finalizing();
            progress.finalized = true;
            if !finalize_until(state, id, &obs, control.begin_finalizing()).await {
                return error(
                    id,
                    StatusCode::SERVICE_UNAVAILABLE,
                    "finalization_unavailable",
                    true,
                );
            }
            return ledger_error(id, e, true);
        }
    };
    let deadline = started + Duration::from_millis(limits.transport_timeout_ms);
    control.set_phase(RequestPhase::Relaying);
    progress.streaming = validated.stream;
    if validated.stream {
        if adapter.settles_cutoff() {
            progress.stream.received =
                crate::providers::transport::ReceivedOutput::with_output_limit(
                    validated.output_limit,
                );
            progress.counter = Some(CutoffCounter {
                client: client.clone(),
                model: profile.model.clone(),
            });
        }
        let observation = client
            .relay(
                permit,
                body,
                SendOptions {
                    api,
                    headers: &outbound,
                    profile,
                    deadline,
                    runtime: &state.runtime,
                },
                control.clone(),
                &mut progress.stream,
                |status, headers, body| {
                    let mut response = (status, body).into_response();
                    *response.headers_mut() = headers;
                    if let Some(tx) = delivery.take() {
                        let _ = tx.send(owner.bind(stamped(response, id)));
                    }
                },
            )
            .await;
        let deadline = control.begin_finalizing();
        progress.finalized = true;
        let (obs, failure) = match observation {
            Ok(obs) => (obs, None),
            Err(e) => {
                let mut observation = failed_observation(e.code());
                observation.usage = progress.stream.final_usage.take();
                (observation, Some(e))
            }
        };
        let settlement = no_usage(state, id, progress, &obs, failure, deadline).await;
        if !finalize_with(state, id, &obs, &settlement, deadline).await {
            return error(
                id,
                StatusCode::SERVICE_UNAVAILABLE,
                "finalization_unavailable",
                false,
            );
        }
        return error(
            id,
            if obs.reason == "upstream_read_timeout" {
                StatusCode::GATEWAY_TIMEOUT
            } else {
                StatusCode::BAD_GATEWAY
            },
            obs.reason,
            false,
        );
    }
    let upstream = tokio::time::timeout_at(deadline, async {
        let options = SendOptions {
            api,
            headers: &outbound,
            profile,
            deadline,
            runtime: &state.runtime,
        };
        client.send(permit, body, options).await
    })
    .await;
    let (response, observation, failure) = match upstream {
        Ok(Ok(result)) => {
            let mut response = (result.status, control.retain_body(result.body)).into_response();
            *response.headers_mut() = result.headers;
            (response, result.observation, None)
        }
        Err(_) => {
            let e = UpstreamError::Timeout;
            (
                error(id, e.status(), e.code(), false),
                failed_observation(e.code()),
                Some(e),
            )
        }
        Ok(Err(e)) => (
            error(id, e.status(), e.code(), false),
            failed_observation(e.code()),
            Some(e),
        ),
    };
    let deadline = control.begin_finalizing();
    progress.finalized = true;
    let settlement = no_usage(state, id, progress, &observation, failure, deadline).await;
    if !finalize_with(state, id, &observation, &settlement, deadline).await {
        return error(
            id,
            StatusCode::SERVICE_UNAVAILABLE,
            "finalization_unavailable",
            false,
        );
    }
    stamped(response, id)
}
fn failed_observation(reason: &'static str) -> FinalObservation {
    FinalObservation {
        outcome: FinalOutcome::Failed,
        usage: None,
        counted_input_tokens: None,
        reason,
        http_status: None,
        provider_request_id: None,
    }
}
async fn finalize_until(
    state: &GatewayState,
    id: Uuid,
    observation: &FinalObservation,
    deadline: Instant,
) -> bool {
    finalize_with(state, id, observation, &NoUsage::Unknown, deadline).await
}
/// 최종 usage 없이 끝난 요청을 어떻게 정산할지 정한다(결정 0005).
/// - Provider가 HTTP 오류로 답했거나 첫 응답 전에 끊겼으면 0원이다.
/// - 시작 event가 입력 토큰을 알려 주는 API([`port::Api::settles_cutoff`])의 스트림이 시작 event 뒤에
///   끊겼으면 정산 규칙을 쓴다. `received` 규칙이면 받은 내용을 센다.
/// - 알 수 없는 응답, 그 밖의 스트림의 중간 끊김은 미정산이다.
async fn no_usage(
    state: &GatewayState,
    id: Uuid,
    progress: &ExecutionProgress,
    observation: &FinalObservation,
    failure: Option<UpstreamError>,
    deadline: Instant,
) -> NoUsage {
    if observation.usage.is_some() {
        return NoUsage::Unknown;
    }
    if observation
        .http_status
        .is_some_and(|status| !(200..300).contains(&status))
        || matches!(
            failure,
            Some(
                UpstreamError::Auth
                    | UpstreamError::Redirect
                    | UpstreamError::Credential
                    | UpstreamError::ErrorResponse
            )
        )
    {
        return NoUsage::NoResponse;
    }
    if matches!(
        failure,
        Some(
            UpstreamError::Protocol
                | UpstreamError::StreamParse
                | UpstreamError::Encoding
                | UpstreamError::TooLarge
        )
    ) {
        return NoUsage::Unknown;
    }
    if !progress.streaming {
        // 비스트림 요청은 응답을 받았는데 usage가 없으면 알 수 없는 응답이다.
        return if observation.http_status.is_some() {
            NoUsage::Unknown
        } else {
            NoUsage::NoResponse
        };
    }
    // 첫 event 전, 또는 시작 event(예: Anthropic `message_start`) 전에 끊기면 입력 토큰도 모른다.
    if !progress.stream.started || (progress.counter.is_some() && progress.stream.start.is_none()) {
        return NoUsage::NoResponse;
    }
    let (Some(start), Some(counter)) = (progress.stream.start.clone(), progress.counter.as_ref())
    else {
        return NoUsage::Unknown;
    };
    // 규칙은 받을 때 기록한 값이다. 규칙이 없는 요청은 장부가 미정산으로 둔다.
    // 읽지 못하면 일단 센다. 장부가 마감 때 규칙을 다시 확인한다.
    let count = match state.gateway.ledger.cutoff_rule(id).await {
        Ok(Some(rule)) => rule == "received",
        Ok(None) => false,
        Err(_) => true,
    };
    let (received_output, count_failure) = if !count {
        (None, None)
    } else if progress.stream.received.overflow() {
        (None, Some("cutoff_content_too_large"))
    } else {
        // 장부 기록에 쓸 시간을 1초 남긴다.
        let limit = deadline
            .checked_sub(Duration::from_secs(1))
            .unwrap_or(deadline)
            .min(Instant::now() + Duration::from_secs(2));
        match counter
            .client
            .count_received(&counter.model, progress.stream.received.text(), limit)
            .await
        {
            Ok(n) => (Some(n), None),
            Err(_) => (None, Some("cutoff_count_unavailable")),
        }
    };
    NoUsage::Cutoff {
        start,
        received_output,
        count_failure,
    }
}
async fn finalize_with(
    state: &GatewayState,
    id: Uuid,
    observation: &FinalObservation,
    settlement: &NoUsage,
    deadline: Instant,
) -> bool {
    let attempt = async {
        for n in 0..3 {
            match state
                .gateway
                .ledger
                .finalize_request_with(id, observation, settlement)
                .await
            {
                Ok(_) => return true,
                Err(LedgerError::DatabaseUnavailable) => {}
                Err(_) => return false,
            }
            if n < 2 {
                tokio::time::sleep(Duration::from_millis(if n == 0 { 50 } else { 100 })).await;
            }
        }
        false
    };
    if tokio::time::timeout_at(
        deadline.min(Instant::now() + Duration::from_secs(5)),
        attempt,
    )
    .await
    .unwrap_or(false)
    {
        true
    } else {
        state.runtime.record_request_finalization_failure(id);
        false
    }
}
async fn receive(
    mut body: Body,
    max: usize,
    total: Duration,
    idle: Duration,
) -> Result<Bytes, (StatusCode, &'static str)> {
    let deadline = Instant::now() + total;
    let mut bytes = crate::runtime::BoundedBuffer::input(max);
    loop {
        let frame = tokio::time::timeout_at(
            deadline.min(Instant::now() + idle),
            std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)),
        )
        .await
        .map_err(|_| (StatusCode::REQUEST_TIMEOUT, "request_body_timeout"))?;
        match frame {
            Some(Ok(frame)) => {
                if let Ok(chunk) = frame.into_data() {
                    bytes
                        .extend(&chunk)
                        .map_err(|_| (StatusCode::PAYLOAD_TOO_LARGE, "body_too_large"))?;
                }
            }
            Some(Err(_)) => return Err((StatusCode::BAD_REQUEST, "invalid_request")),
            None => return Ok(bytes.freeze()),
        }
    }
}
async fn request_model(bytes: &[u8]) -> Result<String, crate::json::JsonError> {
    struct Model(Option<String>);
    impl crate::json::Visitor for Model {
        fn value(
            &mut self,
            path: &[String],
            kind: crate::json::Kind,
            raw: &str,
        ) -> Result<(), crate::json::JsonError> {
            if path.is_empty() && kind != crate::json::Kind::Object {
                return Err(crate::json::JsonError);
            }
            if path.len() == 1 && path[0] == "model" {
                self.0 = Some(crate::json::string(raw, 200)?);
            }
            Ok(())
        }
    }
    let mut model = Model(None);
    crate::json::scan(bytes, &mut model).await?;
    model.0.ok_or(crate::json::JsonError)
}
fn ledger_code(e: LedgerError) -> &'static str {
    match e {
        LedgerError::DatabaseUnavailable => "db_unavailable",
        LedgerError::DispatchUncertain => "dispatch_uncertain",
        LedgerError::AccessDenied => "access_denied",
        LedgerError::OwnerInactive => "owner_inactive",
        LedgerError::PolicyChanged => "policy_changed",
        LedgerError::ConfigurationConflict => "configuration_conflict",
        LedgerError::Unsettled => "unsettled_usage_block",
        LedgerError::InferencePaused => "inference_paused",
        LedgerError::BudgetExhausted => "budget_exhausted",
        LedgerError::RateLimited => "rate_limited",
        LedgerError::Conflict => "policy_changed",
        LedgerError::Expired => "policy_changed",
        LedgerError::AmountOverflow => "pricing_contract_violation",
        LedgerError::InvalidSnapshot => "price_unresolved",
        LedgerError::InvariantViolation => "invariant_violation",
    }
}
fn ledger_status(e: LedgerError) -> StatusCode {
    match e {
        LedgerError::InvariantViolation => StatusCode::INTERNAL_SERVER_ERROR,
        LedgerError::AccessDenied
        | LedgerError::OwnerInactive
        | LedgerError::Unsettled
        | LedgerError::InferencePaused => StatusCode::FORBIDDEN,
        LedgerError::DatabaseUnavailable | LedgerError::DispatchUncertain => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        LedgerError::InvalidSnapshot | LedgerError::AmountOverflow => StatusCode::BAD_REQUEST,
        LedgerError::BudgetExhausted | LedgerError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        LedgerError::PolicyChanged
        | LedgerError::ConfigurationConflict
        | LedgerError::Conflict
        | LedgerError::Expired => StatusCode::CONFLICT,
    }
}
fn ledger_error(id: Uuid, e: LedgerError, not_dispatched: bool) -> Response {
    error(id, ledger_status(e), ledger_code(e), not_dispatched)
}
fn native_auth(id: Uuid, e: AuthError) -> Response {
    let r = http::authentication_error(e);
    let code = r.headers()["x-gateway-error-code"]
        .to_str()
        .expect("고정 코드");
    let code = match code {
        "invalid_credentials" => "invalid_credentials",
        "auth_source_forbidden" => "auth_source_forbidden",
        "access_denied" => "access_denied",
        "owner_inactive" => "owner_inactive",
        "virtual_key_required" => "virtual_key_required",
        "db_unavailable" => "db_unavailable",
        _ => "auth_dependency_unavailable",
    };
    error(id, r.status(), code, true)
}
pub(crate) fn native_admission(id: Uuid, e: crate::runtime::AdmissionError) -> Response {
    let r = http::admission_error(e);
    let code = match r.headers()["x-gateway-error-code"]
        .to_str()
        .expect("고정 코드")
    {
        "concurrency_limited" => "concurrency_limited",
        "auth_dependency_unavailable" => "auth_dependency_unavailable",
        "draining" => "draining",
        "invariant_violation" => "invariant_violation",
        _ => "not_ready",
    };
    error(id, r.status(), code, true)
}
/// 요청 검사 실패를 클라이언트에 알린다. 요청은 업스트림에 보내지 않았다.
fn api_error(id: Uuid, e: ApiError) -> Response {
    let (status, code) = match e {
        ApiError::UnsupportedOption => (StatusCode::BAD_REQUEST, "unsupported_option"),
        ApiError::UnsupportedModel => (StatusCode::BAD_REQUEST, "unsupported_model"),
        ApiError::InvalidRequest => (StatusCode::BAD_REQUEST, "invalid_request"),
        ApiError::AdapterFailed => (StatusCode::INTERNAL_SERVER_ERROR, "adapter_failed"),
    };
    error(id, status, code, true)
}
fn error(id: Uuid, status: StatusCode, code: &'static str, not_dispatched: bool) -> Response {
    let kind = match status.as_u16() {
        400 | 408 | 409 | 415 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        503 => "overloaded_error",
        _ => "api_error",
    };
    let mut r=(status,Json(serde_json::json!({"type":"error","error":{"type":kind,"message":"Gateway request could not be completed"}}))).into_response();
    r.headers_mut()
        .insert("x-gateway-error-code", HeaderValue::from_static(code));
    if not_dispatched {
        r.headers_mut().insert(
            "x-gateway-dispatch-state",
            HeaderValue::from_static("not_dispatched"),
        );
    } else {
        r.headers_mut()
            .insert("x-should-retry", HeaderValue::from_static("false"));
    }
    if [
        "concurrency_limited",
        "rate_limited",
        "auth_dependency_unavailable",
        "db_unavailable",
        "draining",
        "not_ready",
    ]
    .contains(&code)
    {
        r.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    stamped(r, id)
}
fn stamped(mut r: Response, id: Uuid) -> Response {
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r.headers_mut().insert(
        "x-gateway-request-id",
        HeaderValue::from_str(&id.to_string()).expect("서버 UUID"),
    );
    r
}

/// route 선택 후 생성한 gateway 오류만 `{"detail": ...}` wire로 바꾼다. Provider native body/status는
/// 건드리지 않는다.
fn detail_error(response: Response) -> Response {
    replace_error_body(
        response,
        |_, code| serde_json::json!({"detail":format!("Gateway request could not be completed: gateway_{code}")}),
    )
}
/// 게이트웨이가 만든 오류(`x-gateway-error-code`가 있는 응답)의 본문을 한 번만 바꾼다.
fn replace_error_body(
    mut response: Response,
    body: impl FnOnce(u16, &str) -> serde_json::Value,
) -> Response {
    if response.extensions().get::<ProjectedErrorBody>().is_some() {
        return response;
    }
    if let Some(code) = response
        .headers()
        .get("x-gateway-error-code")
        .and_then(|v| v.to_str().ok())
    {
        let body = body(response.status().as_u16(), code);
        response.extensions_mut().insert(ProjectedErrorBody);
        *response.body_mut() = Body::from(serde_json::to_vec(&body).expect("고정 오류 projection"));
        for name in [
            header::CONTENT_LENGTH,
            header::CONTENT_ENCODING,
            header::ETAG,
        ] {
            response.headers_mut().remove(name);
        }
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
    }
    response
}
/// 오류 본문을 이미 바꾼 응답의 표시. 두 번 바꾸지 않는다.
#[derive(Clone)]
struct ProjectedErrorBody;
/// route 선택 후 생성한 gateway 오류만 OpenAI wire로 바꾼다. Provider native body/status는 건드리지 않는다.
pub(crate) fn openai_error(response: Response) -> Response {
    replace_error_body(response, |status, code| {
        let kind = match status {
            400 | 408 | 409 | 413 | 415 => "invalid_request_error",
            401 => "authentication_error",
            403 => "permission_error",
            429 => "rate_limit_error",
            _ => "server_error",
        };
        serde_json::json!({"error":{"message":"Gateway request could not be completed","type":kind,"param":null,"code":format!("gateway_{code}")}})
    })
}

pub fn auth_config_digest(config: &ValidatedConfig) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let profiles=config.providers().iter().flat_map(|provider|provider.profiles.iter().map(move |p|{
        let projection=serde_json::json!({"provider":crate::ledger::finance::provider_name(provider.provider),"api":crate::ledger::finance::api_name(p.api),"model":p.model,"profile_version":p.profile_version,"actual_models":p.actual_models.iter().collect::<BTreeSet<_>>(),"features":p.features.iter().collect::<BTreeSet<_>>(),"approved_beta":p.approved_beta.iter().collect::<BTreeSet<_>>(),"count_billing":p.count_billing,"max_input_tokens":p.max_input_tokens,"max_output_tokens":p.max_output_tokens,"max_body_bytes":p.max_body_bytes,"max_response_bytes":p.max_response_bytes});
        serde_json::to_string(&projection).expect("비밀 없는 유한 profile projection")
    })).collect::<BTreeSet<_>>();
    let projection = serde_json::json!({"issuer":config.auth().issuer_literal,"audience":config.auth().audience_literal,"allowed":config.allowed_emails(),"admins":config.admin_emails(),"profiles":profiles,"execution_ceiling":config.execution_ceiling()});
    Sha256::digest(serde_json::to_vec(&projection).expect("비밀 없는 유한 접근 설정 projection"))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_anthropic_sdk_beta_marker_query_is_accepted() {
        use crate::config::ApiKind;
        let anthropic = [ApiKind::Messages, ApiKind::CountTokens]
            .into_iter()
            .flat_map(|kind| port::api(ProviderId::Anthropic, kind).unwrap().routes());
        for path in anthropic {
            assert!(inference_query_allowed(path, None));
            assert!(inference_query_allowed(path, Some("beta=true")), "{path}");
            for query in [
                "beta=false",
                "beta=true&x=1",
                "x=1",
                "",
                "beta=TRUE",
                "beta=true&beta=true",
            ] {
                assert!(
                    !inference_query_allowed(path, Some(query)),
                    "{path}?{query}"
                );
            }
        }
        for path in [
            "/openai/v1/responses",
            "/openai/v1/chat/completions",
            "/openai/v1/embeddings",
            "/deepseek/v1/chat/completions",
        ] {
            assert!(inference_query_allowed(path, None));
            assert!(!inference_query_allowed(path, Some("beta=true")), "{path}");
        }
    }

    #[tokio::test]
    async fn ledger_errors_have_native_status() {
        for (failure, status, code, kind) in [
            (
                LedgerError::AccessDenied,
                403,
                "access_denied",
                "permission_error",
            ),
            (
                LedgerError::Unsettled,
                403,
                "unsettled_usage_block",
                "permission_error",
            ),
            (
                LedgerError::InferencePaused,
                403,
                "inference_paused",
                "permission_error",
            ),
            (
                LedgerError::InvalidSnapshot,
                400,
                "price_unresolved",
                "invalid_request_error",
            ),
            (
                LedgerError::AmountOverflow,
                400,
                "pricing_contract_violation",
                "invalid_request_error",
            ),
            (
                LedgerError::PolicyChanged,
                409,
                "policy_changed",
                "invalid_request_error",
            ),
            (
                LedgerError::ConfigurationConflict,
                409,
                "configuration_conflict",
                "invalid_request_error",
            ),
            (
                LedgerError::Conflict,
                409,
                "policy_changed",
                "invalid_request_error",
            ),
            (
                LedgerError::Expired,
                409,
                "policy_changed",
                "invalid_request_error",
            ),
            (
                LedgerError::BudgetExhausted,
                429,
                "budget_exhausted",
                "rate_limit_error",
            ),
            (
                LedgerError::RateLimited,
                429,
                "rate_limited",
                "rate_limit_error",
            ),
            (
                LedgerError::InvariantViolation,
                500,
                "invariant_violation",
                "api_error",
            ),
            (
                LedgerError::DatabaseUnavailable,
                503,
                "db_unavailable",
                "overloaded_error",
            ),
            (
                LedgerError::DispatchUncertain,
                503,
                "dispatch_uncertain",
                "overloaded_error",
            ),
        ] {
            let response = ledger_error(Uuid::new_v4(), failure, true);
            assert_eq!(response.status().as_u16(), status, "{code}");
            assert_eq!(response.headers()["x-gateway-error-code"], code);
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["type"], kind, "{code}");
        }
    }
}

#[cfg(test)]
#[path = "gateway/error_matrix_tests.rs"]
mod error_matrix_tests;
