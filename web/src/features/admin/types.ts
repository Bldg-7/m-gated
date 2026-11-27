import type { AuthorizedCredentials, BlockState, Grant, Limits } from '../../lib/types';
export type Safety = { paused: boolean; version: string; reason: string | null; paused_at: string | null; trigger_request_id: string | null };
export type Policy = { id: string; version: string; disabled: boolean; grants: Grant[]; limits: Limits };
export type PrincipalSummary = { id: string; kind: 'human' | 'service'; display_name: string; email: string | null; management_owner_id: string | null; management_owner_name: string | null; version: string; policy_version: string; disabled: boolean; inference_safety: Safety; block_state: BlockState };
export type Principal = Omit<PrincipalSummary, 'policy_version' | 'disabled'> & { policy: Policy; provider_credentials: AuthorizedCredentials };
export type PrincipalDetail = { principal: Principal; key_limit_maximum?: Limits | null; execution_ceiling: { grants: Grant[]; limits: Limits } };
/** invitation: 비밀번호 방식에서 사람을 추가한 뒤 초대 메일을 보냈는지. link: 링크 보내기로 메일을 보낸 대상과 종류. */
export type CommandAck = { command_id: string; operation: string; target: { kind: string; id: string }; result_version: string | null; effect: 'changed' | 'noop'; replayed: boolean; invitation?: 'sent' | 'failed'; link?: { email: string; kind: 'invitation' | 'reset' | 'setup' } };
export type RequestSummary = { id: string; parent_request_id: string | null; purpose: string; principal_id: string; key_id: string | null; provider: string; model: string; api: string; admitted_at: string; finished_at: string | null; phase: string; outcome: string | null; accounting_state: string; cost_usd: string | null; version: string; reason_code: string | null; provider_credential: { id: string; label: string | null; hint: string | null } | null };
/** 장부의 사용량 기록. 사용량 칸(결정 0012)마다 `<칸>_tokens`가 있고, 토큰을 모르면 null이다. */
export type SelectedUsage = { source: string; actual_model: string | null; input_tokens: string | null; output_tokens: string | null; cache_read_tokens: string | null; cache_write_tokens: string | null; cache_write_5m_tokens: string | null; cache_write_1h_tokens: string | null; reasoning_tokens: string | null; counted_input_tokens: string | null; /** 끊긴 요청을 정산한 규칙과, 받은 내용을 세지 못해 최대 출력으로 정산한 까닭. */ cutoff_rule?: string | null; cutoff_fallback_reason?: string | null };
export type SettlementRecord = { source: 'manual_entry'; decision: string; cost_usd: string; reason: string; actor_principal_id: string; settled_at: string; executor_id: string; execution_stop_source: 'runtime' | 'admin_confirmation' };
export type RequestDetail = RequestSummary & { provider_request_id: string | null; http_status: number | null; pricing_snapshot: unknown; execution_snapshot: unknown; budgets: unknown[]; usage: { partial: SelectedUsage | null; final: SelectedUsage | null; late_final: SelectedUsage | null; /** 사후 보정한 사용량. 고치기 전 출력 토큰(previous_output_tokens)과 출처(previous_source)가 함께 있다. */ correction?: (SelectedUsage & { previous_output_tokens?: string; previous_source?: string }) | null }; executor_id: string; execution_stopped_at: string | null; settlement: SettlementRecord | null };
export type CredentialMeta = { authorized_principal_ids?: string[]; id: string; provider: string; label: string; bound_keys: string; state: 'active' | 'retired'; decryptable: boolean; hint: string; version: string; verified_at: string; created_at: string; created_by: string; created_by_email: string | null; retired_at: string | null; retired_by_email: string | null };
export type CredentialList = { providers: { provider: string; status: 'active' | 'missing' | 'undecryptable'; credentials: CredentialMeta[]; history: CredentialMeta[] }[] };
export type CatalogEntry = { provider: string; model: string; rates: Record<string, string>; max_input_tokens: number; max_output_tokens: number; source: string; note: string | null };
export type CatalogView = { version: string; approved_at: string | null; approved_by_email: string | null; entries: CatalogEntry[]; providers: { provider: string; api: string; model_prefixes: string[]; required_rates: string[]; /** 그 API가 알린 사용량 칸. 단가는 이 칸에만 둘 수 있다(결정 0012). */ usage_categories: string[] }[] };
export type CatalogProposalItem = { provider: string; model: string; status: 'unchanged' | 'changed' | 'new' | 'missing' | 'incomplete'; current: CatalogEntry | null; proposed: CatalogEntry | null };
export type CatalogProposal = { base_version: number; source: { source: string; models: number }; items: CatalogProposalItem[] };
/** 토큰 대조에서 Provider가 더 많이 센 토큰의 까닭. */
export type TokenCheckReason = 'cutoff' | 'unknown' | 'outside' | 'unexplained';
export type TokenCheckCredential = { id: string; label: string; hint: string };
/** 토큰 대조 화면 값. 토큰 수는 정수 문자열, 금액은 소수점 12자리 USD 문자열이다. 금액이 null이면 가격을 모르는 모델이다. */
export type TokenCheckView = {
  /** 이 서버에 Admin API 키가 있어 조회하는지. */
  enabled: boolean;
  range: { from: string | null; to: string | null };
  /** 요청한 기간 가운데 사용량 보고서를 받아 둔 구간. 없으면 아직 비교할 데이터가 없다. */
  compared: { from: string | null; to: string | null } | null;
  /** minute_convention: 사용량 보고서가 1분 칸에 요청을 넣는 기준(보낸 분 start, 끝난 분 end). 확인 전이면 null.
   * start_minutes·end_minutes: 그 기준으로만 맞은 분의 수. 사후 보정은 기준을 확인한 뒤에만 한다. */
  state: { last_attempt_at: string | null; last_success_at: string | null; last_error: string | null; minute_convention: 'start' | 'end' | null; start_minutes: string; end_minutes: string };
  totals: {
    provider_tokens: string; gateway_tokens: string; provider_more_tokens: string; gateway_more_tokens: string;
    provider_more_usd: string; gateway_more_usd: string; unpriced_models: string[]; corrected_requests: string;
    /** usd가 null이면 그 까닭의 토큰이 모두 가격을 모르는 모델의 것이다. */
    reasons: { reason: TokenCheckReason; tokens: string; usd: string | null }[];
  };
  rows: {
    credential: TokenCheckCredential | null; api_key_id: string; model: string;
    provider_tokens: string; gateway_tokens: string; provider_more_tokens: string; gateway_more_tokens: string;
    provider_more_usd: string | null; gateway_more_usd: string | null;
    reasons: { reason: TokenCheckReason; tokens: string }[];
  }[];
  unmatched_credentials: TokenCheckCredential[];
};
