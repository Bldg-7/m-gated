use crate::{
    auth::{AuthError, SsoAuthenticator},
    config::{ApiKind, ModelProfile, ProviderConfig, ProviderId, ValidatedConfig},
    http,
    ledger::{
        PgLedger,
        finance::{FinalObservation, FinalOutcome, LedgerError, RequestSpec},
    },
    providers::{
        anthropic::{self, ApiError},
        transport::NativeClient,
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
}
#[derive(Clone)]
struct GatewayState {
    gateway: Arc<InferenceGateway>,
    runtime: Arc<Runtime>,
}
pub(crate) const ANTHROPIC_ROUTES: [&str; 4] = [
    "/anthropic/v1/messages",
    "/v1/messages",
    "/anthropic/v1/messages/count_tokens",
    "/v1/messages/count_tokens",
];
/// Anthropic SDK의 beta client는 `?beta=true`를 붙인다. 이 표식만 허용하며 upstream에는 전달하지 않는다
/// (upstream 경로는 서버 고정, beta 기능은 승인된 anthropic-beta 헤더로만 결정된다). 그 외 query는 거부한다.
fn inference_query_allowed(path: &str, query: Option<&str>) -> bool {
    match query {
        None => true,
        Some(q) => q == "beta=true" && ANTHROPIC_ROUTES.contains(&path),
    }
}
impl InferenceGateway {
    pub fn new(
        config: &ValidatedConfig,
        auth: Arc<SsoAuthenticator>,
        ledger: Arc<PgLedger>,
    ) -> Result<Self, ApiError> {
        let mut clients = BTreeMap::new();
        let mut profiles = Vec::new();
        for provider in config.providers() {
            let client = NativeClient::from_config(
                provider,
                config.auth().ca_certificate.as_deref(),
                clients.values().next(),
            )?;
            clients.insert(provider.provider, client);
            profiles.extend(provider.profiles.clone());
        }
        Ok(Self {
            auth,
            ledger,
            clients,
            profiles,
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
        }
    }
    pub fn ledger(&self) -> &Arc<PgLedger> {
        &self.ledger
    }
    pub fn paths(&self) -> Vec<&'static str> {
        let mut paths = Vec::new();
        if self.clients.contains_key(&ProviderId::Anthropic) {
            paths.extend(ANTHROPIC_ROUTES);
        }
        if self.clients.contains_key(&ProviderId::Openai) {
            paths.extend([
                "/openai/v1/responses",
                "/openai/v1/chat/completions",
                "/openai/v1/embeddings",
            ]);
        }
        if self.clients.contains_key(&ProviderId::Deepseek) {
            paths.push("/deepseek/v1/chat/completions");
        }
        paths
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
    let responses = request.uri().path().starts_with("/openai/v1/")
        || request.uri().path().starts_with("/deepseek/v1/");
    let response = handle_inner(state, request).await;
    if responses {
        openai_error(response)
    } else {
        response
    }
}
async fn handle_inner(state: GatewayState, request: Request) -> Response {
    let responses = request.uri().path().starts_with("/openai/v1/")
        || request.uri().path().starts_with("/deepseek/v1/");
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
        if let Some(child) = &progress.child {
            let deadline = control.begin_finalizing();
            if child.uncertain {
                state
                    .runtime
                    .record_family_finalization_failure(id, Some(child.id));
            } else {
                let observation = failed_observation("client_cancelled");
                if !child.finalized
                    && !finalize_family_until(&state, id, child.id, &observation, deadline).await
                {
                    state
                        .runtime
                        .record_family_finalization_failure(id, Some(child.id));
                }
                if tokio::time::timeout_at(
                    deadline,
                    state.gateway.ledger.record_execution_stopped(child.id),
                )
                .await
                .map_or(true, |r| r.is_err())
                {
                    state
                        .runtime
                        .record_family_finalization_failure(id, Some(child.id));
                }
            }
        }

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
            let _ = finalize_until(&state, id, &observation, control.begin_finalizing()).await;
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
            let response = if responses {
                openai_error(response)
            } else {
                response
            };
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
    stream: crate::providers::transport::StreamProgress,
    child: Option<ChildProgress>,
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
struct ChildProgress {
    id: Uuid,
    finalized: bool,
    uncertain: bool,
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
    let api = if request.uri().path() == "/deepseek/v1/chat/completions" {
        ApiKind::DeepseekChat
    } else if request.uri().path() == "/openai/v1/chat/completions" {
        ApiKind::OpenaiChat
    } else if request.uri().path() == "/openai/v1/embeddings" {
        ApiKind::Embeddings
    } else if request.uri().path().starts_with("/openai/v1/") {
        ApiKind::Responses
    } else if request.uri().path().ends_with("/count_tokens") {
        ApiKind::CountTokens
    } else {
        ApiKind::Messages
    };
    if !anthropic::json_content_type(request.headers())
        || request
            .headers()
            .get_all(header::CONTENT_ENCODING)
            .iter()
            .any(|v| v.as_bytes() != b"identity")
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
    control.set_phase(RequestPhase::Preparing);
    let model = match request_model(&body).await {
        Ok(m) => m,
        Err(_) => return error(id, StatusCode::BAD_REQUEST, "invalid_request", true),
    };
    let Some(profile) = state
        .gateway
        .profiles
        .iter()
        .find(|p| p.api == api && p.model == model)
    else {
        return error(id, StatusCode::BAD_REQUEST, "unsupported_model", true);
    };
    if body.len() > profile.max_body_bytes {
        return error(id, StatusCode::PAYLOAD_TOO_LARGE, "body_too_large", true);
    }
    let provider = if api == ApiKind::DeepseekChat {
        ProviderId::Deepseek
    } else if matches!(
        api,
        ApiKind::Responses | ApiKind::OpenaiChat | ApiKind::Embeddings
    ) {
        ProviderId::Openai
    } else {
        ProviderId::Anthropic
    };
    let Some(client) = state.gateway.clients.get(&provider) else {
        return error(id, StatusCode::BAD_REQUEST, "unsupported_model", true);
    };
    let validation = if api == ApiKind::DeepseekChat {
        crate::providers::deepseek::validate(&body, profile).await
    } else if matches!(
        api,
        ApiKind::Responses | ApiKind::OpenaiChat | ApiKind::Embeddings
    ) {
        crate::providers::openai::validate(&body, profile).await
    } else {
        anthropic::validate(&body, api, profile).await
    };
    let validated = match validation {
        Ok(m) => m,
        Err(e) => {
            return error(
                id,
                StatusCode::BAD_REQUEST,
                match e {
                    ApiError::UnsupportedOption => "unsupported_option",
                    ApiError::UnsupportedModel => "unsupported_model",
                    ApiError::InvalidRequest => "invalid_request",
                },
                true,
            );
        }
    };
    if !validated.stream
        && let Err(e) = lease.acquire_nonstream()
    {
        return native_admission(id, e);
    }
    let outbound = match if matches!(
        api,
        ApiKind::Responses | ApiKind::OpenaiChat | ApiKind::Embeddings | ApiKind::DeepseekChat
    ) {
        crate::providers::openai::approved_headers(&head.headers)
    } else {
        anthropic::approved_headers(&head.headers, profile)
    } {
        Ok(h) => h,
        Err(_) => return error(id, StatusCode::BAD_REQUEST, "unsupported_option", true),
    };
    let mut spec = RequestSpec::from_profile(provider, profile, validated.output_limit);
    spec.id = id;
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
    if profile.internal_count
        && let Err(failure) = internal_count(
            state,
            &human.current,
            human.key,
            &spec,
            profile.max_input_tokens,
            &body,
            &outbound,
            control,
            progress,
        )
        .await
    {
        let observation = FinalObservation {
            outcome: FinalOutcome::Rejected,
            usage: None,
            counted_input_tokens: None,
            reason: failure.code(),
            http_status: None,
            provider_request_id: None,
        };
        progress.finalized = true;
        if !finalize_until(state, id, &observation, control.begin_finalizing()).await {
            return error(
                id,
                StatusCode::SERVICE_UNAVAILABLE,
                "finalization_unavailable",
                true,
            );
        }
        return error(id, failure.status(), failure.code(), true);
    }
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
    if validated.stream {
        let observation = client
            .relay(
                permit,
                body,
                anthropic::SendOptions {
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
        control.begin_finalizing();
        progress.finalized = true;
        let obs = match observation {
            Ok(obs) => obs,
            Err(e) => {
                let mut observation = failed_observation(e.code());
                observation.usage = progress.stream.final_usage.take();
                observation
            }
        };
        if !finalize_until(state, id, &obs, control.begin_finalizing()).await {
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
        let options = anthropic::SendOptions {
            api,
            headers: &outbound,
            profile,
            deadline,
            runtime: &state.runtime,
        };
        if api == ApiKind::Embeddings {
            client.send_embeddings(permit, body, options).await
        } else {
            client.send(permit, body, options).await
        }
    })
    .await;
    let (response, observation) = match upstream {
        Ok(Ok(result)) => {
            let mut response = (result.status, control.retain_body(result.body)).into_response();
            *response.headers_mut() = result.headers;
            (response, result.observation)
        }
        Err(_) => {
            let e = crate::providers::anthropic::UpstreamError::Timeout;
            (
                error(id, e.status(), e.code(), false),
                failed_observation(e.code()),
            )
        }
        Ok(Err(e)) => (
            error(id, e.status(), e.code(), false),
            failed_observation(e.code()),
        ),
    };
    control.begin_finalizing();
    progress.finalized = true;
    if !finalize_until(state, id, &observation, control.begin_finalizing()).await {
        return error(
            id,
            StatusCode::SERVICE_UNAVAILABLE,
            "finalization_unavailable",
            false,
        );
    }
    stamped(response, id)
}
enum InternalCountError {
    Ledger(LedgerError),
    Count(&'static str),
}
impl InternalCountError {
    fn code(&self) -> &'static str {
        match self {
            Self::Ledger(e) => ledger_code(*e),
            Self::Count(code) => code,
        }
    }
    fn status(&self) -> StatusCode {
        match self {
            Self::Ledger(e) => ledger_status(*e),
            Self::Count("unsupported_strict_policy") => StatusCode::BAD_REQUEST,
            Self::Count("finalization_unavailable") => StatusCode::SERVICE_UNAVAILABLE,
            Self::Count(_) => StatusCode::BAD_GATEWAY,
        }
    }
}
#[allow(clippy::too_many_arguments)]
async fn internal_count(
    state: &GatewayState,
    human: &crate::ledger::CurrentHuman,
    key: Option<crate::ledger::finance::KeyScope>,
    root: &RequestSpec,
    input_limit: u64,
    body: &Bytes,
    headers: &axum::http::HeaderMap,
    control: &Arc<crate::runtime::RequestControl>,
    progress: &mut ExecutionProgress,
) -> Result<(), InternalCountError> {
    let profile = state
        .gateway
        .profiles
        .iter()
        .find(|p| p.model == root.pricing.requested_model && p.api == ApiKind::CountTokens)
        .ok_or(InternalCountError::Count("unsupported_strict_policy"))?;
    if body.len() > profile.max_body_bytes {
        return Err(InternalCountError::Count("unsupported_strict_policy"));
    }
    let projection = anthropic::count_projection(body.clone())
        .await
        .map_err(|_| InternalCountError::Count("unsupported_strict_policy"))?;
    let mut child = RequestSpec::from_profile(ProviderId::Anthropic, profile, None);
    child.deadline = root.deadline;
    child.api_version = root.api_version.clone();
    child.approved_beta = root.approved_beta.clone();
    // count profile의 헤더 허용 목록도 부모와 독립적으로 검사한다.
    let approved = anthropic::approved_headers(headers, profile)
        .map_err(|_| InternalCountError::Count("unsupported_strict_policy"))?;
    state
        .gateway
        .ledger
        .admit_scoped(human, &child, key, Some(root.id))
        .await
        .map_err(InternalCountError::Ledger)?;
    progress.child = Some(ChildProgress {
        id: child.id,
        finalized: false,
        uncertain: false,
    });
    let permit = match state.gateway.ledger.authorize_dispatch(child.id).await {
        Ok(p) => p,
        Err(e) => {
            if e == LedgerError::DispatchUncertain {
                progress.child.as_mut().expect("등록한 자식").uncertain = true;
                state
                    .runtime
                    .record_family_finalization_failure(root.id, Some(child.id));
            }
            return Err(InternalCountError::Ledger(e));
        }
    };
    let result = tokio::time::timeout_at(
        control.transport_deadline,
        state.gateway.clients[&ProviderId::Anthropic].send(
            permit,
            projection,
            anthropic::SendOptions {
                api: ApiKind::CountTokens,
                headers: &approved,
                profile,
                deadline: control.transport_deadline,
                runtime: &state.runtime,
            },
        ),
    )
    .await;
    let (observation, count) = match result {
        Ok(Ok(response)) => {
            let count = response.observation.counted_input_tokens;
            (response.observation, count)
        }
        Ok(Err(e)) => (failed_observation(e.code()), None),
        Err(_) => (failed_observation("upstream_read_timeout"), None),
    };
    let deadline = control
        .execution_deadline
        .min(Instant::now() + Duration::from_secs(5));
    if !finalize_family_until(state, root.id, child.id, &observation, deadline).await {
        state
            .runtime
            .record_family_finalization_failure(root.id, Some(child.id));
        return Err(InternalCountError::Count("finalization_unavailable"));
    }
    progress.child.as_mut().expect("등록한 자식").finalized = true;
    if state
        .gateway
        .ledger
        .record_execution_stopped(child.id)
        .await
        .is_err()
    {
        state
            .runtime
            .record_family_finalization_failure(root.id, Some(child.id));
        return Err(InternalCountError::Count("finalization_unavailable"));
    }
    // 최종 count만 입력 상한 증거다. 결과 유실을 0 token으로 추정하지 않는다.
    if count.is_none() {
        return Err(InternalCountError::Count(
            if observation.reason == "completed" {
                "unsupported_strict_policy"
            } else {
                observation.reason
            },
        ));
    }
    if count.is_some_and(|n| n > input_limit) {
        return Err(InternalCountError::Count("unsupported_strict_policy"));
    }
    Ok(())
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
    finalize_family_until(state, id, id, observation, deadline).await
}
async fn finalize_family_until(
    state: &GatewayState,
    root: Uuid,
    id: Uuid,
    observation: &FinalObservation,
    deadline: Instant,
) -> bool {
    let attempt = async {
        for n in 0..3 {
            match state.gateway.ledger.finalize_request(id, observation).await {
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
        state
            .runtime
            .record_family_finalization_failure(root, if root == id { None } else { Some(id) });
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
        LedgerError::UnsupportedStrictPolicy => "unsupported_strict_policy",
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
        LedgerError::UnsupportedStrictPolicy
        | LedgerError::InvalidSnapshot
        | LedgerError::AmountOverflow => StatusCode::BAD_REQUEST,
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

/// route 선택 후 생성한 gateway 오류만 OpenAI wire로 바꾼다. Provider native body/status는 건드리지 않는다.
#[derive(Clone)]
struct OpenaiErrorBody;
pub(crate) fn openai_error(mut response: Response) -> Response {
    if response.extensions().get::<OpenaiErrorBody>().is_some() {
        return response;
    }
    if let Some(code) = response
        .headers()
        .get("x-gateway-error-code")
        .and_then(|v| v.to_str().ok())
    {
        let status = response.status().as_u16();
        let kind = match status {
            400 | 408 | 409 | 413 | 415 => "invalid_request_error",
            401 => "authentication_error",
            403 => "permission_error",
            429 => "rate_limit_error",
            _ => "server_error",
        };
        let body = serde_json::json!({"error":{"message":"Gateway request could not be completed","type":kind,"param":null,"code":format!("gateway_{code}")}});
        response.extensions_mut().insert(OpenaiErrorBody);
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

pub fn auth_config_digest(config: &ValidatedConfig) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let profiles=config.providers().iter().flat_map(|provider|provider.profiles.iter().map(move |p|{
        let mut projection=serde_json::json!({"provider":crate::ledger::finance::provider_name(provider.provider),"api":crate::ledger::finance::api_name(p.api),"model":p.model,"profile_version":p.profile_version,"actual_models":p.actual_models.iter().collect::<BTreeSet<_>>(),"features":p.features.iter().collect::<BTreeSet<_>>(),"approved_beta":p.approved_beta.iter().collect::<BTreeSet<_>>(),"input_bound_contract":p.input_bound_contract,"count_billing":p.count_billing,"internal_count":p.internal_count,"max_input_tokens":p.max_input_tokens,"max_output_tokens":p.max_output_tokens,"max_body_bytes":p.max_body_bytes,"max_response_bytes":p.max_response_bytes});
        if provider.provider == ProviderId::Deepseek { projection["billing_time_rule"] = serde_json::json!(p.billing_time_rule.map(|r|r.as_str())); }
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
        for path in ANTHROPIC_ROUTES {
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
    async fn internal_count_and_direct_ledger_errors_share_native_status() {
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
                LedgerError::UnsupportedStrictPolicy,
                400,
                "unsupported_strict_policy",
                "invalid_request_error",
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
            let id = Uuid::new_v4();
            let internal = InternalCountError::Ledger(failure);
            let responses = [
                ledger_error(id, failure, true),
                error(id, internal.status(), internal.code(), true),
            ];
            for response in responses {
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

    #[test]
    fn internal_count_nonledger_errors_keep_their_own_classification() {
        for (code, status) in [
            ("unsupported_strict_policy", StatusCode::BAD_REQUEST),
            ("finalization_unavailable", StatusCode::SERVICE_UNAVAILABLE),
            ("upstream_auth_error", StatusCode::BAD_GATEWAY),
            ("upstream_transport_error", StatusCode::BAD_GATEWAY),
            ("upstream_protocol_error", StatusCode::BAD_GATEWAY),
        ] {
            let failure = InternalCountError::Count(code);
            assert_eq!(failure.status(), status);
            assert_eq!(failure.code(), code);
        }
    }
}

#[cfg(test)]
#[path = "gateway/error_matrix_tests.rs"]
mod error_matrix_tests;
