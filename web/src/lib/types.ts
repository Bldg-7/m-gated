export type BlockState = { blocked: boolean; reasons: string[]; global_unknown_count: string; expired_dispatch_count: string };
export type Grant = { provider: string; model: string; capabilities: string[] };
export type Limits = { budget: null | { mode: 'actual' | 'reserved'; limit_usd: string; period: { kind: 'calendar_month' } }; rpm: number | null; tpm: number | null; count_rpm: number | null };
export type Validity = '3_months' | '6_months' | '1_year' | '3_years' | 'unlimited';
/** authorized=false: 키 주인에게 이 Provider 키의 사용 인가가 해제됐다. 이 Provider로 가는 요청은 거절된다. */
export type KeyCredential = { provider: string; credential_id: string; label: string; hint: string; state: 'active' | 'retired'; authorized: boolean };
/** 사용자에게 인가된 Provider 키. 인가가 있는 Provider만 담긴다. 인가가 없는 Provider는 그 사용자가 쓸 수 없다. */
export type AuthorizedCredentials = { provider: string; credentials: { id: string; label: string; hint: string }[] }[];
/** 예산 기간의 기준 시간대(관리자 설정). pending은 다음 기간부터 바뀔 시간대다. */
export type BudgetTimezone = { timezone: string; period_start: string; period_end: string; pending: null | { timezone: string; effective_from: string; first_period_end: string }; version: string };
/** owner: 키 주인. 서비스 계정은 email이 없다. */
export type KeyMetadata = { provider_credentials: KeyCredential[]; id: string; principal_id: string; owner: { kind: 'human' | 'service'; display_name: string; email: string | null }; name: string; key_hint: string; created_at: string; created_by: string; created_command_id: string; validity: Validity; expires_at: string | null; revoked_at: string | null; version: string; grants: Grant[]; limits: Limits };
export type Page<T> = { items: T[]; next_cursor: string | null; as_of: string };
export type IssuedKey = { key: string; metadata: KeyMetadata };
export type Me = { principal: { id: string; display_name: string; email: string; block_state: BlockState }; role: 'user' | 'admin'; grants: Grant[]; budget_timezone?: BudgetTimezone | null; provider_credentials?: AuthorizedCredentials; auth_expires_at: string; sso: { refresh_url: string; logout_url: string } };
export type Balance = { scope_id: string; scope_kind: 'principal' | 'key'; key_id: string | null; period_start: string; period_end: string; timezone: string; policy_version: string; config_digest: string; mode: 'actual' | 'reserved'; limit_usd: string | null; settled_usd: string; reserved_usd: string; scope_unknown_count: string; principal_unknown_count: string; balance_state: 'limited' | 'unlimited' | 'unresolved'; remaining_usd: string | null; blocked: boolean; reasons: string[]; as_of: string };
export type Bucket = { group: { kind: 'provider' | 'model' | 'key'; provider?: string; model?: string; key_id?: string | null }; settled_usd: string; held_reservation_usd: string; unknown_count: string; input_tokens: string | null; output_tokens: string | null; tokens_complete: boolean };
export type Usage = { currency: 'USD'; from: string; to: string; as_of: string; detail_available_from: string | null; aggregation: 'detail' | 'period_totals'; settled_usd: string; held_reservation_usd: string; period_unknown_count: string; global_unknown_count: string; global_expired_dispatch_count: string; block_state: BlockState | null; balance_context: 'current_principal' | 'current_principal_and_key' | 'aggregate_no_balance'; budget_balances: Balance[]; buckets: { items: Bucket[]; next_cursor: string | null; as_of: string } };
