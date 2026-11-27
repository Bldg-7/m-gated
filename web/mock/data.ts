/**
 * 개발용 미리보기의 가짜 데이터. `npm run dev` 에서만 쓰이고 운영 빌드와 시험에는 들어가지 않는다.
 * 모든 값은 지어낸 것이다. 이메일은 예시 도메인(example.test)만 쓰고, 키 값은 `mgw_v1_fake…` 처럼 명백한 가짜만 쓴다.
 * 상태는 메모리에만 있고 개발 서버를 다시 띄우면 처음으로 돌아간다.
 */
import type { Grant, Limits, Validity } from '../src/lib/types';
import type { CatalogEntry, CatalogProposal, RequestDetail, Safety } from '../src/features/admin/types';
import type { ManagementToken } from '../src/features/admin/tokens';

export type MockPrincipal = { id: string; kind: 'human' | 'service'; display_name: string; email: string | null; management_owner_id: string | null; version: number; policy: { id: string; version: number; disabled: boolean; grants: Grant[]; limits: Limits }; safety: Safety; scope_id: string };
export type MockCredential = { id: string; provider: string; label: string; hint: string; version: number; state: 'active' | 'retired'; decryptable: boolean; verified_at: string; created_at: string; created_by: string; retired_at: string | null; retired_by: string | null };
/** bindings: Provider 이름 → 연결된 Provider 키 ID. */
export type MockKey = { id: string; principal_id: string; name: string; key_hint: string; created_at: string; created_by: string; created_command_id: string; validity: Validity; expires_at: string | null; revoked_at: string | null; version: number; grants: Grant[]; limits: Limits; bindings: Record<string, string>; scope_id: string };
/** 하루 단위 사용량 한 줄. 금액은 1e-12 USD 정수다. */
export type UsageRow = { at: number; principal_id: string; key_id: string | null; provider: string; model: string; cost: bigint; input: number; output: number };
export type MockState = {
  principals: MockPrincipal[]; credentials: MockCredential[];
  /** 사용자 ID → 인가된 Provider 키 ID. */
  authorizations: Map<string, Set<string>>;
  keys: MockKey[]; requests: RequestDetail[]; usage: UsageRow[];
  catalog: { version: number; approved_at: string | null; approved_by_email: string | null; entries: CatalogEntry[] };
  proposals: Map<string, CatalogProposal>;
  timezone: { timezone: string; pending: string | null; version: number };
  /** 끊긴 요청의 정산 규칙(Anthropic). */
  cutoffRule: { cutoff_rule: 'received' | 'max_tokens'; correction: boolean; version: number };
  authorizationVersion: number;
  /** 접근 목록(결정 0001). 고정 관리자는 admin@example.test 다. password_set: 비밀번호를 정했는지(결정 0011, 비밀번호 방식에서만 화면에 보인다). */
  access: { id: string; email: string; role: 'user' | 'admin'; added_by_email: string | null; added_at: string; version: number; password_set: boolean }[];
  /** 관리 토큰(결정 0014). 비밀값은 두지 않는다. */
  tokens: ManagementToken[];
};

const DAY = 86400000;
/** 읽기 쉬운 고정 UUID. group 으로 종류를 나눈다(1 사용자, 2 Provider 키, 3 가상 키, 4 요청, 5 정책, 6 예산 scope, 7 executor). */
export const uid = (group: number, n: number) => `${group.toString(16).padStart(8, '0')}-0000-4000-8000-${n.toString(16).padStart(12, '0')}`;
export const ADMIN_ID = uid(1, 1);

// ── 금액: 소수점 12자리 USD 문자열 ↔ 1e-12 USD 정수 ──
const UNIT = 10n ** 12n;
export const usd = (value: bigint) => `${value / UNIT}.${(value % UNIT).toString().padStart(12, '0')}`;
export const pico = (value: string) => { const [whole, fraction = ''] = value.split('.'); return BigInt(whole) * UNIT + BigInt(fraction.padEnd(12, '0').slice(0, 12)); };

// ── 예산 기간: 기준 시간대의 달력 한 달 ──
function zoneOffset(zone: string, at: number) {
  const parts = Object.fromEntries(new Intl.DateTimeFormat('en-US', { timeZone: zone, hourCycle: 'h23', year: 'numeric', month: 'numeric', day: 'numeric', hour: 'numeric', minute: 'numeric', second: 'numeric' }).formatToParts(at).map(p => [p.type, Number(p.value)]));
  return Date.UTC(parts.year, parts.month - 1, parts.day, parts.hour, parts.minute, parts.second) - Math.floor(at / 1000) * 1000;
}
/** at 이 속한 달에서 months 만큼 떨어진 달의 1일 0시(zone 기준)를 UTC ms 로 돌려준다. */
export function monthStart(zone: string, at: number, months = 0) {
  const local = new Date(at + zoneOffset(zone, at));
  const wall = Date.UTC(local.getUTCFullYear(), local.getUTCMonth() + months, 1);
  return wall - zoneOffset(zone, wall - zoneOffset(zone, wall));
}
export const iso = (at: number) => new Date(at).toISOString();

// ── 모델·가격 ──
const entry = (provider: string, model: string, rates: Record<string, string>, max_input_tokens: number, max_output_tokens: number, note: string | null = null): CatalogEntry => ({ provider, model, rates: Object.fromEntries(Object.entries(rates).map(([k, v]) => [k, usd(pico(v))])), max_input_tokens, max_output_tokens, source: 'models_dev', note });
/** usage_categories: 그 API가 알린 사용량 칸(결정 0012). 단가는 이 칸에만 둔다. */
export const catalogProviders = [
  { provider: 'anthropic', api: 'messages', model_prefixes: ['claude-'], required_rates: ['input', 'output', 'cache_read', 'cache_write_5m'], usage_categories: ['input', 'output', 'cache_read', 'cache_write_5m', 'cache_write_1h'] },
  { provider: 'openai', api: 'responses', model_prefixes: ['gpt-'], required_rates: ['input', 'output', 'cache_read'], usage_categories: ['input', 'output', 'cache_read'] },
  { provider: 'deepseek', api: 'chat_completions', model_prefixes: ['deepseek-'], required_rates: ['input', 'output', 'cache_read'], usage_categories: ['input', 'output', 'cache_read'] },
];
/** Provider 마다 정책에서 고를 수 있는 API. */
export const capabilities: Record<string, string[]> = { anthropic: ['messages', 'count_tokens'], openai: ['responses', 'chat_completions'], deepseek: ['chat_completions'] };
const OPUS = 'claude-opus-5-5', HAIKU = 'claude-haiku-5-5', GPT = 'gpt-5.5-mini';
const approved: CatalogEntry[] = [
  entry('anthropic', OPUS, { input: '5', output: '25', cache_read: '0.5', cache_write_5m: '6.25', cache_write_1h: '10' }, 1000000, 128000),
  entry('anthropic', HAIKU, { input: '1', output: '5', cache_read: '0.1', cache_write_5m: '1.25' }, 200000, 64000, 'tier_input_limit:200000'),
  entry('openai', GPT, { input: '0.25', output: '2', cache_read: '0.025' }, 400000, 128000),
  entry('openai', 'gpt-4.1-legacy', { input: '2', output: '8', cache_read: '0.5' }, 1000000, 32000),
  entry('deepseek', 'deepseek-chat', { input: '0.28', output: '0.42', cache_read: '0.028' }, 128000, 8000),
];
/** 가짜 "공개 가격 목록". 동기화는 지금 카탈로그와 이 목록을 비교해 변경안을 만든다. proposed 가 null 이면 가격 항목이 모자란 모델이다. */
export const priceSource: { provider: string; model: string; proposed: CatalogEntry | null }[] = [
  { provider: 'anthropic', model: OPUS, proposed: entry('anthropic', OPUS, { input: '4.5', output: '22.5', cache_read: '0.45', cache_write_5m: '5.625' }, 1000000, 128000) },
  { provider: 'anthropic', model: HAIKU, proposed: approved[1] },
  { provider: 'anthropic', model: 'claude-sonnet-5-5', proposed: entry('anthropic', 'claude-sonnet-5-5', { input: '3', output: '15', cache_read: '0.3', cache_write_5m: '3.75' }, 200000, 64000, 'tier_input_limit:200000') },
  { provider: 'openai', model: GPT, proposed: approved[2] },
  { provider: 'openai', model: 'gpt-5.5', proposed: entry('openai', 'gpt-5.5', { input: '1.25', output: '10', cache_read: '0.125' }, 400000, 128000) },
  { provider: 'deepseek', model: 'deepseek-chat', proposed: approved[4] },
  { provider: 'deepseek', model: 'deepseek-reasoner', proposed: null },
];

const noLimits = (): Limits => ({ budget: null, rpm: null, tpm: null, count_rpm: null });
const budget = (amount: string): Limits['budget'] => ({ limit_usd: usd(pico(amount)), period: { kind: 'calendar_month' } });
const grant = (provider: string, model: string, ...caps: string[]): Grant => ({ provider, model, capabilities: caps });
/** 시드가 고정된 난수. 서버를 다시 띄워도 같은 모양의 데이터가 나온다. */
function random(seed: number) { let s = seed; return () => { s = (s * 1664525 + 1013904223) % 4294967296; return s / 4294967296; }; }

export function createState(): MockState {
  const now = Date.now(), ago = (days: number, hours = 0) => iso(now - days * DAY - hours * 3600000);
  const safe = (version = 1): Safety => ({ paused: false, version: String(version), reason: null, paused_at: null, trigger_request_id: null });
  const P = { admin: ADMIN_ID, backend: uid(1, 2), analyst: uid(1, 3), designer: uid(1, 4), qa: uid(1, 5), intern: uid(1, 6), batch: uid(1, 7), bot: uid(1, 8) };
  const C = { platform: uid(2, 1), research: uid(2, 2), openai: uid(2, 3), oldShared: uid(2, 4), oldOpenai: uid(2, 5) };
  const K = { local: uid(3, 1), ci: uid(3, 2), experiment: uid(3, 3), demo: uid(3, 4), oldTeam: uid(3, 5), backend: uid(3, 6), blocked: uid(3, 7), notebook: uid(3, 8), unlinked: uid(3, 9), batchProd: uid(3, 10), batchStaging: uid(3, 11), bot: uid(3, 12) };

  const person = (n: number, id: string, kind: 'human' | 'service', display_name: string, email: string | null, owner: string | null, policy: { version: number; disabled?: boolean; grants: Grant[]; limits: Limits }, safety = safe()): MockPrincipal =>
    ({ id, kind, display_name, email, management_owner_id: owner, version: n + 2, policy: { id: uid(5, n), disabled: false, ...policy }, safety, scope_id: uid(6, n) });
  const principals: MockPrincipal[] = [
    person(1, P.admin, 'human', '미리보기 관리자', 'admin@example.test', null, { version: 12, grants: [grant('anthropic', OPUS, 'messages', 'count_tokens'), grant('anthropic', HAIKU, 'messages', 'count_tokens'), grant('openai', GPT, 'responses')], limits: { budget: budget('200'), rpm: 120, tpm: null, count_rpm: null } }),
    person(2, P.backend, 'human', '백엔드 개발자', 'backend-dev@example.test', null, { version: 7, grants: [grant('anthropic', OPUS, 'messages', 'count_tokens'), grant('anthropic', HAIKU, 'messages')], limits: { budget: budget('50'), rpm: 60, tpm: 200000, count_rpm: 30 } }),
    person(3, P.analyst, 'human', '데이터 분석가', 'analyst@example.test', null, { version: 4, grants: [grant('openai', GPT, 'responses'), grant('anthropic', HAIKU, 'messages')], limits: { budget: budget('30'), rpm: null, tpm: null, count_rpm: null } }),
    person(4, P.designer, 'human', '디자이너', 'designer@example.test', null, { version: 2, grants: [grant('anthropic', HAIKU, 'messages')], limits: noLimits() }),
    person(5, P.qa, 'human', 'QA 담당자', 'qa@example.test', null, { version: 3, grants: [grant('anthropic', OPUS, 'messages'), grant('anthropic', HAIKU, 'messages')], limits: { budget: budget('20'), rpm: 30, tpm: null, count_rpm: null } },
      { paused: true, version: '4', reason: 'cost_bound_violation', paused_at: ago(0, 5), trigger_request_id: uid(4, 4) }),
    person(6, P.intern, 'human', '인턴 (정지된 계정)', 'intern@example.test', null, { version: 5, disabled: true, grants: [grant('anthropic', HAIKU, 'messages')], limits: noLimits() }),
    person(7, P.batch, 'service', 'batch-summarizer', null, P.admin, { version: 9, grants: [grant('anthropic', HAIKU, 'messages'), grant('openai', GPT, 'responses')], limits: { budget: budget('500'), rpm: 600, tpm: 2000000, count_rpm: null } }),
    person(8, P.bot, 'service', 'support-bot', null, P.backend, { version: 2, grants: [grant('anthropic', HAIKU, 'messages')], limits: { budget: budget('80'), rpm: 240, tpm: null, count_rpm: null } }),
  ];

  const credential = (id: string, provider: string, label: string, hint: string, createdDays: number, retiredDays: number | null = null): MockCredential =>
    ({ id, provider, label, hint, version: retiredDays === null ? 2 : 3, state: retiredDays === null ? 'active' : 'retired', decryptable: true, verified_at: ago(createdDays), created_at: ago(createdDays), created_by: P.admin, retired_at: retiredDays === null ? null : ago(retiredDays), retired_by: retiredDays === null ? null : P.admin });
  const credentials = [
    credential(C.platform, 'anthropic', 'team-platform', 'a1f3', 150),
    credential(C.research, 'anthropic', 'team-research', '9k2d', 45),
    credential(C.openai, 'openai', 'openai-main', 'Zx07', 120),
    credential(C.oldShared, 'anthropic', 'old-shared', '0b7c', 300, 60),
    credential(C.oldOpenai, 'openai', 'openai-trial', 'Qp55', 280, 121),
  ];
  // 인가된 사용자 수와 연결된 가상 키 수가 서로 다르게 나오도록 나눴다.
  const authorizations = new Map<string, Set<string>>([
    [P.admin, new Set([C.platform, C.research, C.openai])], [P.backend, new Set([C.platform])], [P.analyst, new Set([C.research, C.openai])],
    [P.designer, new Set([C.platform])], [P.qa, new Set([C.platform])], [P.batch, new Set([C.platform, C.openai])], [P.bot, new Set([C.platform])],
  ]);

  const months: Record<Validity, number | null> = { '3_months': 3, '6_months': 6, '1_year': 12, '3_years': 36, unlimited: null };
  const key = (n: number, id: string, principal_id: string, name: string, validity: Validity, createdDays: number, bindings: Record<string, string>, grants: Grant[], extra: { revokedDays?: number; limits?: Limits } = {}): MockKey => {
    const created = new Date(now - createdDays * DAY), span = months[validity];
    const expires = span === null ? null : new Date(created); if (expires && span !== null) expires.setUTCMonth(expires.getUTCMonth() + span);
    return { id, principal_id, name, key_hint: `mgw_v1_fake${n.toString().padStart(2, '0')}`, created_at: created.toISOString(), created_by: principals.find(p => p.id === principal_id)?.kind === 'service' ? P.admin : principal_id, created_command_id: uid(8, n), validity, expires_at: expires?.toISOString() ?? null, revoked_at: extra.revokedDays === undefined ? null : ago(extra.revokedDays), version: extra.revokedDays === undefined ? 1 : 2, grants, limits: extra.limits ?? noLimits(), bindings, scope_id: uid(6, 100 + n) };
  };
  const keys = [
    key(1, K.local, P.admin, '로컬 개발', '1_year', 40, { anthropic: C.platform, openai: C.openai }, [grant('anthropic', OPUS, 'messages', 'count_tokens'), grant('openai', GPT, 'responses')]),
    key(2, K.ci, P.admin, 'CI 파이프라인', 'unlimited', 120, { anthropic: C.research }, [grant('anthropic', HAIKU, 'messages')], { limits: { budget: budget('25'), rpm: 30, tpm: null, count_rpm: null } }),
    key(3, K.experiment, P.admin, '지난 분기 실험', '3_months', 100, { anthropic: C.platform }, [grant('anthropic', OPUS, 'messages')], { revokedDays: 20 }),
    key(4, K.demo, P.admin, '만료된 데모 키', '3_months', 97, { openai: C.openai }, [grant('openai', GPT, 'responses')]),
    key(5, K.oldTeam, P.admin, '이전 팀 키', '6_months', 70, { anthropic: C.oldShared }, [grant('anthropic', HAIKU, 'messages')]),
    key(6, K.backend, P.backend, '백엔드 통합 시험', '6_months', 30, { anthropic: C.platform }, [grant('anthropic', OPUS, 'messages'), grant('anthropic', HAIKU, 'messages')]),
    key(7, K.blocked, P.backend, '연구 키 (인가 해제됨)', '3_years', 44, { anthropic: C.research }, [grant('anthropic', HAIKU, 'messages')]),
    key(8, K.notebook, P.analyst, '분석 노트북', '1_year', 25, { openai: C.openai, anthropic: C.research }, [grant('openai', GPT, 'responses'), grant('anthropic', HAIKU, 'messages')]),
    key(9, K.unlinked, P.designer, '예전 키 (연결 없음)', '1_year', 200, {}, [grant('anthropic', HAIKU, 'messages')]),
    key(10, K.batchProd, P.batch, 'batch-prod', 'unlimited', 90, { anthropic: C.platform, openai: C.openai }, [grant('anthropic', HAIKU, 'messages'), grant('openai', GPT, 'responses')], { limits: { budget: budget('300'), rpm: 300, tpm: 1000000, count_rpm: null } }),
    key(11, K.batchStaging, P.batch, 'batch-staging', '6_months', 85, { anthropic: C.platform }, [grant('anthropic', HAIKU, 'messages')], { revokedDays: 9 }),
    key(12, K.bot, P.bot, 'support-bot-main', '1_year', 14, { anthropic: C.platform }, [grant('anthropic', HAIKU, 'messages')]),
  ];

  // ── 사용량: 최근 60일의 일별·모델별 값 ──
  const rate = (provider: string, model: string, kind: string) => pico(approved.find(e => e.provider === provider && e.model === model)?.rates[kind] ?? '0');
  const price = (provider: string, model: string, input: number, output: number) => (BigInt(input) * rate(provider, model, 'input') + BigInt(output) * rate(provider, model, 'output')) / 1000000n;
  const next = random(20261120), usage: UsageRow[] = [];
  const streams: [string, string | null, string, string, number][] = [
    [P.admin, K.local, 'anthropic', OPUS, 3], [P.admin, K.local, 'openai', GPT, 0.6], [P.admin, K.ci, 'anthropic', HAIKU, 1.5], [P.admin, null, 'anthropic', HAIKU, 0.2],
    [P.backend, K.backend, 'anthropic', OPUS, 0.8], [P.backend, K.backend, 'anthropic', HAIKU, 1], [P.analyst, K.notebook, 'openai', GPT, 2], [P.analyst, K.notebook, 'anthropic', HAIKU, 0.5],
    [P.batch, K.batchProd, 'anthropic', HAIKU, 4], [P.batch, K.batchProd, 'openai', GPT, 3], [P.bot, K.bot, 'anthropic', HAIKU, 1.2],
  ];
  for (let day = 0; day < 60; day++) for (const [principal_id, key_id, provider, model, weight] of streams) {
    const input = Math.floor(weight * (20000 + next() * 180000)), output = Math.floor(input * (0.1 + next() * 0.3));
    // 오늘 줄은 한 시간 전으로 두어 방금 시작한 달에도 이번 기간 사용량이 비지 않게 한다.
    usage.push({ at: now - day * DAY - 3600000, principal_id, key_id, provider, model, cost: price(provider, model, input, output), input, output });
  }

  // ── 요청: 미정산·정산 대기·확정, 완료·실패·거절·취소가 섞여 있다 ──
  const periodStart = iso(monthStart('Asia/Seoul', now)), periodEnd = iso(monthStart('Asia/Seoul', now, 1));
  type Seed = { n: number; principal: string; key: string | null; provider: string; model: string; api: string; minutes: number; phase: string; outcome: string | null; accounting: string; tokens?: [number, number]; reason?: string; stopped?: boolean; http?: number | null; parent?: number; manual?: boolean; credential?: string | null; settledBy?: 'cutoff_received' | 'cutoff_max_tokens' | 'no_response'; /** 사후 보정한 출력 토큰. */ corrected?: number };
  const request = (s: Seed): RequestDetail => {
    const admitted = now - s.minutes * 60000, finished = s.phase === 'finished' ? iso(admitted + 4200) : null;
    const credentialId = s.credential === undefined ? keys.find(k => k.id === s.key)?.bindings[s.provider] ?? null : s.credential, bound = credentials.find(c => c.id === credentialId);
    const owner = principals.find(p => p.id === s.principal)!, [input, output] = s.tokens ?? [0, 0];
    const cost = s.accounting !== 'settled' ? null : s.outcome === 'completed' || s.manual || s.settledBy?.startsWith('cutoff') ? usd(price(s.provider, s.model, input, s.corrected ?? output)) : usd(0n);
    const observed = (source: string) => ({ source, actual_model: s.model, input_tokens: String(input), output_tokens: String(output), cache_read_tokens: '0', cache_write_tokens: '0', cache_write_5m_tokens: '0', cache_write_1h_tokens: '0', reasoning_tokens: null, counted_input_tokens: null });
    return {
      id: uid(4, s.n), parent_request_id: s.parent ? uid(4, s.parent) : null, purpose: s.parent ? 'internal_count' : 'root', principal_id: s.principal, key_id: s.key, provider: s.provider, model: s.model, api: s.api,
      admitted_at: iso(admitted), finished_at: finished, phase: s.phase, outcome: s.outcome, accounting_state: s.accounting, cost_usd: cost, version: s.phase === 'finished' ? '3' : s.phase === 'dispatching' ? '2' : '1', reason_code: s.reason ?? null,
      provider_credential: bound ? { id: bound.id, label: bound.label, hint: bound.hint } : null,
      provider_request_id: s.outcome === 'completed' ? `req_fake_${s.n.toString().padStart(4, '0')}` : null, http_status: s.http === undefined ? (s.outcome === 'completed' ? 200 : null) : s.http,
      pricing_snapshot: { catalog_version: '4', currency: 'USD', unit: 'per_1m_tokens', rates: approved.find(e => e.provider === s.provider && e.model === s.model)?.rates ?? {} },
      execution_snapshot: { policy_version: String(owner.policy.version), max_output_tokens: 8000, stream: s.api !== 'count_tokens' },
      budgets: owner.policy.limits.budget ? [{ scope_id: owner.scope_id, policy_version: String(owner.policy.version), period_start: periodStart, period_end: periodEnd, state: s.accounting === 'settled' ? 'settled' : s.accounting === 'unknown' ? 'unknown' : 'active' }] : [],
      usage: { partial: s.tokens && s.accounting === 'unknown' ? observed('provider') : null, final: s.outcome === 'completed' ? observed('provider') : s.settledBy ? { ...observed(s.settledBy), cutoff_rule: s.settledBy === 'cutoff_received' ? 'received' : s.settledBy === 'cutoff_max_tokens' ? 'max_tokens' : null, cutoff_fallback_reason: null } : null, late_final: null, correction: s.corrected === undefined ? null : { ...observed('provider_report'), output_tokens: String(s.corrected), previous_output_tokens: String(output), previous_source: s.settledBy ?? 'provider' } },
      executor_id: uid(7, (s.n % 3) + 1), execution_stopped_at: s.stopped ?? s.phase === 'finished' ? finished ?? iso(admitted + 9000) : null,
      settlement: s.manual ? { source: 'manual_entry', decision: 'charged', cost_usd: cost!, reason: '공급사 사용량 화면에서 확인한 금액을 반영했습니다.', actor_principal_id: P.admin, settled_at: iso(admitted + 6 * 3600000), executor_id: uid(7, (s.n % 3) + 1), execution_stop_source: 'runtime' } : null,
    };
  };
  const requests = [
    request({ n: 1, principal: P.analyst, key: K.notebook, provider: 'openai', model: GPT, api: 'responses', minutes: 180, phase: 'finished', outcome: 'failed', accounting: 'unknown', tokens: [41200, 0], reason: 'provider_response_lost', http: null }),
    request({ n: 2, principal: P.analyst, key: K.notebook, provider: 'anthropic', model: HAIKU, api: 'messages', minutes: 1560, phase: 'dispatching', outcome: null, accounting: 'unknown', stopped: false }),
    request({ n: 3, principal: P.batch, key: K.batchProd, provider: 'anthropic', model: HAIKU, api: 'messages', minutes: 2900, phase: 'finished', outcome: 'failed', accounting: 'unknown', tokens: [88000, 1200], reason: 'stream_interrupted', http: 200 }),
    request({ n: 4, principal: P.qa, key: null, provider: 'anthropic', model: OPUS, api: 'messages', minutes: 300, phase: 'finished', outcome: 'cancelled', accounting: 'unknown', tokens: [15000, 0], reason: 'client_disconnected', credential: C.platform }),
    request({ n: 5, principal: P.admin, key: K.local, provider: 'anthropic', model: OPUS, api: 'messages', minutes: 1, phase: 'dispatching', outcome: null, accounting: 'pending' }),
    request({ n: 6, principal: P.batch, key: K.batchProd, provider: 'openai', model: GPT, api: 'responses', minutes: 2, phase: 'dispatching', outcome: null, accounting: 'pending' }),
    request({ n: 7, principal: P.backend, key: K.backend, provider: 'anthropic', model: HAIKU, api: 'messages', minutes: 0, phase: 'admitted', outcome: null, accounting: 'pending' }),
    request({ n: 8, principal: P.admin, key: K.local, provider: 'anthropic', model: OPUS, api: 'messages', minutes: 20, phase: 'finished', outcome: 'completed', accounting: 'settled', tokens: [52340, 3180] }),
    request({ n: 9, principal: P.admin, key: K.ci, provider: 'anthropic', model: HAIKU, api: 'messages', minutes: 60, phase: 'finished', outcome: 'completed', accounting: 'settled', tokens: [12800, 940] }),
    request({ n: 10, principal: P.backend, key: K.backend, provider: 'anthropic', model: OPUS, api: 'messages', minutes: 120, phase: 'finished', outcome: 'completed', accounting: 'settled', tokens: [98000, 6400] }),
    request({ n: 11, principal: P.batch, key: K.batchProd, provider: 'anthropic', model: HAIKU, api: 'messages', minutes: 240, phase: 'finished', outcome: 'completed', accounting: 'settled', tokens: [154000, 21000] }),
    request({ n: 12, principal: P.bot, key: K.bot, provider: 'anthropic', model: HAIKU, api: 'messages', minutes: 360, phase: 'finished', outcome: 'completed', accounting: 'settled', tokens: [4200, 380] }),
    request({ n: 13, principal: P.analyst, key: K.notebook, provider: 'openai', model: GPT, api: 'responses', minutes: 480, phase: 'finished', outcome: 'completed', accounting: 'settled', tokens: [230000, 12000] }),
    request({ n: 14, principal: P.admin, key: K.local, provider: 'openai', model: GPT, api: 'responses', minutes: 540, phase: 'finished', outcome: 'failed', accounting: 'settled', reason: 'upstream_error', http: 500 }),
    request({ n: 15, principal: P.backend, key: K.blocked, provider: 'anthropic', model: HAIKU, api: 'messages', minutes: 600, phase: 'finished', outcome: 'rejected', accounting: 'settled', reason: 'provider_credential_not_authorized', credential: null }),
    request({ n: 16, principal: P.designer, key: K.unlinked, provider: 'anthropic', model: HAIKU, api: 'messages', minutes: 700, phase: 'finished', outcome: 'rejected', accounting: 'settled', reason: 'rate_limited', credential: null }),
    request({ n: 17, principal: P.batch, key: K.batchProd, provider: 'anthropic', model: HAIKU, api: 'messages', minutes: 4400, phase: 'finished', outcome: 'failed', accounting: 'settled', tokens: [61000, 900], reason: 'stream_interrupted', manual: true }),
    request({ n: 18, principal: P.admin, key: K.local, provider: 'anthropic', model: OPUS, api: 'count_tokens', minutes: 20, phase: 'finished', outcome: 'completed', accounting: 'settled', tokens: [52340, 0] }),
    request({ n: 19, principal: P.backend, key: K.backend, provider: 'anthropic', model: OPUS, api: 'messages', minutes: 35, phase: 'finished', outcome: 'cancelled', accounting: 'settled', tokens: [24000, 1200], reason: 'client_cancelled', settledBy: 'cutoff_received' }),
    request({ n: 20, principal: P.qa, key: null, provider: 'anthropic', model: HAIKU, api: 'messages', minutes: 50, phase: 'finished', outcome: 'failed', accounting: 'settled', reason: 'provider_error', http: 529, settledBy: 'no_response' }),
    request({ n: 21, principal: P.backend, key: K.backend, provider: 'anthropic', model: OPUS, api: 'messages', minutes: 95, phase: 'finished', outcome: 'cancelled', accounting: 'settled', tokens: [31000, 640], reason: 'client_cancelled', settledBy: 'cutoff_received', corrected: 2210 }),
  ];

  return { principals, credentials, authorizations, keys, requests, usage, catalog: { version: 4, approved_at: ago(12), approved_by_email: 'admin@example.test', entries: approved.map(e => ({ ...e })) }, proposals: new Map(), timezone: { timezone: 'Asia/Seoul', pending: null, version: 3 }, cutoffRule: { cutoff_rule: 'received', correction: false, version: 1 }, authorizationVersion: 20,
    access: [
      ...principals.filter(p => p.kind === 'human' && p.email).map((p, i) => ({ id: `acc-${i + 1}`, email: p.email!, role: (p.email === 'admin@example.test' ? 'admin' : 'user') as 'user' | 'admin', added_by_email: p.email === 'admin@example.test' ? null : 'admin@example.test', added_at: ago(30), version: 1, password_set: true })),
      { id: 'acc-new', email: 'new-hire@example.test', role: 'user' as const, added_by_email: 'admin@example.test', added_at: ago(1), version: 1, password_set: false },
    ],
    tokens: [
      { id: uid(9, 1), name: '배포 스크립트', scope: 'write', validity: '90_days', owner: { id: ADMIN_ID, email: 'admin@example.test', display_name: '관리자' }, created_at: ago(20), expires_at: iso(now + 70 * DAY), revoked_at: null, revoked_by_email: null, last_used_at: ago(0, 2), active: true },
      { id: uid(9, 2), name: '사용량 대시보드', scope: 'read', validity: '1_year', owner: { id: ADMIN_ID, email: 'admin@example.test', display_name: '관리자' }, created_at: ago(40), expires_at: iso(now + 325 * DAY), revoked_at: null, revoked_by_email: null, last_used_at: null, active: true },
      { id: uid(9, 3), name: '옛 CI 작업', scope: 'write', validity: '30_days', owner: { id: ADMIN_ID, email: 'admin@example.test', display_name: '관리자' }, created_at: ago(25), expires_at: iso(now + 5 * DAY), revoked_at: ago(3), revoked_by_email: 'admin@example.test', last_used_at: ago(4), active: false },
    ] };
}
