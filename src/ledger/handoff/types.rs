use super::super::{
    finance::PricingSnapshot,
    management::{Grant, Validity},
    policy::PolicyLimits,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub kind: String,
    pub issuer: Option<String>,
    pub subject: Option<String>,
    pub owner_principal_id: Option<Uuid>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Safety {
    pub inference_paused_at: Option<String>,
    pub inference_pause_reason: Option<String>,
    pub inference_pause_request_id: Option<Uuid>,
    pub inference_safety_version: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub id: Uuid,
    pub key_id: Option<Uuid>,
    pub version: String,
    pub grants: Vec<Grant>,
    pub limits: PolicyLimits,
    pub budget_mode: String,
    pub period_kind: String,
    pub period_timezone: String,
    pub created_at: String,
    pub updated_at: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Key {
    pub id: Uuid,
    pub public_prefix: String,
    pub name: String,
    pub grant_snapshot: Vec<Grant>,
    pub validity_preset: Validity,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
    pub created_at: String,
    pub version: String,
    pub original_created_by: Uuid,
    pub original_created_command_id: Uuid,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Period {
    pub scope_id: Uuid,
    pub period_start: String,
    pub period_end: String,
    pub timezone_snapshot: String,
    pub cost_usd: String,
    pub reserved_usd: String,
    pub carried_cost_usd: String,
    pub unknown_count: String,
    pub settled_count: String,
    pub input_tokens: String,
    pub output_tokens: String,
    pub carried_settled_count: String,
    pub carried_input_tokens: String,
    pub carried_output_tokens: String,
    pub settled_unknown_tokens_count: String,
    pub carried_settled_unknown_tokens_count: String,
    pub detail_deleted_through: Option<String>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub scope_id: Uuid,
    pub lane: String,
    pub window_start: String,
    pub window_end: String,
    pub admitted_count: String,
    pub reserved_tokens: String,
    pub settled_tokens: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub id: Uuid,
    pub key_id: Option<Uuid>,
    pub parent_request_id: Option<Uuid>,
    pub purpose: String,
    pub provider: String,
    pub requested_model: String,
    pub api: String,
    pub admitted_at: String,
    pub admit_expires_at: String,
    pub transport_deadline: String,
    pub execution_deadline: String,
    pub dispatch_authorized_at: Option<String>,
    pub lease_expires_at: Option<String>,
    pub finished_at: String,
    pub executor_id: Uuid,
    pub execution_stopped_at: Option<String>,
    pub outcome: String,
    pub accounting_state: String,
    pub cost_usd: Option<String>,
    pub pricing_snapshot: PricingSnapshot,
    pub execution_snapshot: Value,
    pub auth_config_digest: String,
    pub safety_violation_recorded: bool,
    pub provider_request_id: Option<String>,
    pub http_status: Option<i16>,
    pub reason_code: Option<String>,
    pub result_digest: Option<String>,
    pub settlement_digest: Option<String>,
    pub version: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicySnapshot {
    pub scope_id: Uuid,
    pub version: String,
    pub grants: Vec<Grant>,
    pub limits: PolicyLimits,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub request_id: Uuid,
    pub scope_id: Uuid,
    pub policy_version: String,
    pub policy_snapshot: PolicySnapshot,
    pub period_start: String,
    pub period_end: String,
    pub timezone_snapshot: String,
    pub rate_lane: String,
    pub rate_start: String,
    pub rate_end: String,
    pub budget_mode: String,
    pub reservation_usd: String,
    pub remaining_reservation_usd: String,
    pub reserved_tokens: String,
    pub remaining_reserved_tokens: String,
    pub applied_cost_usd: Option<String>,
    pub applied_input_tokens: Option<String>,
    pub applied_output_tokens: Option<String>,
    pub state: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub request_id: Uuid,
    pub kind: String,
    pub observed_at: String,
    pub actual_model: Option<String>,
    pub provider_usage: Value,
    pub normalized_usage: Value,
    pub pricing_resolution: Value,
    pub reason_code: Option<String>,
    pub content_digest: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FinancialBundle {
    pub identity: Identity,
    pub original_disabled: bool,
    pub original_disabled_reason: Option<String>,
    pub original_security_fence_ref: Option<String>,
    pub source_principal_version: String,
    pub safety: Safety,
    pub keys: Vec<Key>,
    pub scopes: Vec<Scope>,
    pub periods: Vec<Period>,
    pub windows: Vec<Window>,
    pub requests: Vec<Request>,
    pub budgets: Vec<Budget>,
    pub events: Vec<Event>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Import {
    pub target_id: Uuid,
    pub expected_version: String,
    pub source_id: Uuid,
    pub source_ref: String,
    pub previous_writer_fenced_ref: String,
    pub previous_executors_stopped_ref: String,
    pub cutoff: String,
    pub bundle: FinancialBundle,
}
