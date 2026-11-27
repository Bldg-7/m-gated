/**
 * 개발용 미리보기의 가짜 API. Vite 개발 서버(`npm run dev`)의 미들웨어로 `/api/*` 와 `/auth/*` 에 응답한다.
 * 실제 서버(src/admin/management.rs)의 경로·응답 모양만 흉내 낸다. 인증·권한·서버 검증은 대부분 재현하지 않는다.
 * vite.config.ts 가 개발 서버일 때만 넣으므로 운영 빌드(`npm run build`)와 시험(`vitest`)에는 들어가지 않는다.
 *
 * 오류 화면을 보기 위한 장치:
 * - Provider 키 추가·교체에서 API 키에 `reject` 가 들어 있으면 422 provider_credential_rejected
 * - Provider 키 추가·교체에서 API 키에 `unverified` 가 들어 있으면 502 provider_credential_unverified
 * - 가상 키 발급에서 키 이름이 `quota` 로 시작하면 409 key_quota_exceeded
 */
import type { Plugin } from 'vite';
import type { AuthorizedCredentials, Balance, BlockState, BudgetTimezone, Bucket, Grant, KeyMetadata, Limits, Me, Usage, Validity } from '../src/lib/types';
import type { CatalogProposal, CatalogProposalItem, CredentialList, CredentialMeta, PrincipalDetail, PrincipalSummary, RequestSummary } from '../src/features/admin/types';
import { ADMIN_ID, capabilities, catalogProviders, createState, iso, monthStart, pico, priceSource, uid, usd, type MockCredential, type MockKey, type MockPrincipal, type MockState } from './data';

/** Node 타입 패키지 없이도 타입 검사가 되도록, 쓰는 부분만 적은 요청·응답 모양. */
type Req = { method?: string; url?: string; headers: Record<string, string | string[] | undefined>; setEncoding(encoding: string): void; on(event: string, listener: (chunk: string) => void): unknown };
type Res = { statusCode: number; setHeader(name: string, value: string): void; end(body?: string): void };
type Reply = { status: number; body?: unknown };
type Json = Record<string, unknown>;

class HttpError extends Error { constructor(public status: number, public code: string, public details: Json = {}) { super(code); } }
const fail = (status: number, code: string, details: Json = {}): never => { throw new HttpError(status, code, details); };
const text = (value: unknown) => typeof value === 'string' ? value : '';
const sameSet = (a: string[], b: string[]) => a.length === b.length && a.every(x => b.includes(x));

function api(state: MockState) {
  const now = () => new Date().toISOString();
  const principal = (id: string): MockPrincipal => state.principals.find(p => p.id === id) ?? fail(404, 'resource_not_found');
  const credential = (id: string) => state.credentials.find(c => c.id === id);
  const activeCredential = (provider: string, id: string): MockCredential => state.credentials.find(c => c.id === id && c.provider === provider && c.state === 'active') ?? fail(404, 'resource_not_found');
  const authorized = (principalId: string, credentialId: string) => state.authorizations.get(principalId)?.has(credentialId) ?? false;
  const email = (id: string | null) => state.principals.find(p => p.id === id)?.email ?? null;
  /** expected_version 이 지금 버전과 다르면 서버처럼 409 version_conflict 를 낸다. */
  const expect = (expected: unknown, current: number | string) => { if (String(expected) !== String(current)) fail(409, 'version_conflict', { current_version: String(current) }); };
  const ack = (commandId: string, operation: string, kind: string, id: string, version: number | string | null, changed = true, extra: Json = {}) =>
    ({ command_id: commandId, operation, target: { kind, id }, result_version: version === null ? null : String(version), effect: changed ? 'changed' : 'noop', replayed: false, ...extra });
  const page = <T>(items: T[], query: URLSearchParams) => {
    const limit = Math.min(100, Math.max(1, Number(query.get('limit') ?? 50) || 50)), offset = Number(query.get('cursor')?.replace('offset:', '') ?? 0) || 0;
    return { items: items.slice(offset, offset + limit), next_cursor: offset + limit < items.length ? `offset:${offset + limit}` : null, as_of: now() };
  };

  // ── 화면에 내보내는 모양 ──
  const block = (p: MockPrincipal | null): BlockState => {
    const mine = state.requests.filter(r => r.accounting_state === 'unknown' && (!p || r.principal_id === p.id));
    const unknown = mine.length, expired = mine.filter(r => r.phase === 'dispatching').length;
    const reasons = p ? [...(p.policy.disabled ? ['disabled'] : []), ...(unknown ? ['unsettled'] : []), ...(p.safety.paused ? ['inference_paused'] : [])] : [];
    return { blocked: reasons.length > 0, reasons, global_unknown_count: String(unknown), expired_dispatch_count: String(expired) };
  };
  const authorizedView = (principalId: string): AuthorizedCredentials => catalogProviders.map(p => p.provider).flatMap(provider => {
    const list = state.credentials.filter(c => c.provider === provider && c.state === 'active' && authorized(principalId, c.id)).map(c => ({ id: c.id, label: c.label, hint: c.hint }));
    return list.length ? [{ provider, credentials: list }] : [];
  });
  const timezoneView = (): BudgetTimezone => {
    const at = Date.now(), tz = state.timezone, end = monthStart(tz.timezone, at, 1);
    return { timezone: tz.timezone, period_start: iso(monthStart(tz.timezone, at)), period_end: iso(end), pending: tz.pending ? { timezone: tz.pending, effective_from: iso(end), first_period_end: iso(monthStart(tz.pending, end + 1000, 1)) } : null, version: String(tz.version) };
  };
  const summary = (p: MockPrincipal): PrincipalSummary => ({ id: p.id, kind: p.kind, display_name: p.display_name, email: p.email, management_owner_id: p.management_owner_id, management_owner_name: p.management_owner_id ? principal(p.management_owner_id).display_name : null, version: String(p.version), policy_version: String(p.policy.version), disabled: p.policy.disabled, inference_safety: p.safety, block_state: block(p) });
  const ceiling = (): { grants: Grant[]; limits: Limits } => ({ grants: state.catalog.entries.map(e => ({ provider: e.provider, model: e.model, capabilities: capabilities[e.provider] ?? [] })), limits: { budget: null, rpm: 1000, tpm: 4000000, count_rpm: 1000 } });
  const detail = (p: MockPrincipal): PrincipalDetail => {
    const { policy_version: _policy, disabled: _disabled, ...rest } = summary(p);
    return { principal: { ...rest, policy: { ...p.policy, version: String(p.policy.version) }, provider_credentials: authorizedView(p.id) }, key_limit_maximum: keyMaximum(p), execution_ceiling: ceiling() };
  };
  const keyView = (k: MockKey): KeyMetadata => {
    const owner = principal(k.principal_id);
    return {
      provider_credentials: Object.entries(k.bindings).flatMap(([provider, id]) => { const c = credential(id); return c ? [{ provider, credential_id: c.id, label: c.label, hint: c.hint, state: c.state, authorized: c.state === 'active' && authorized(owner.id, c.id) }] : []; }),
      id: k.id, principal_id: k.principal_id, owner: { kind: owner.kind, display_name: owner.display_name, email: owner.email }, name: k.name, key_hint: k.key_hint, created_at: k.created_at, created_by: k.created_by, created_command_id: k.created_command_id,
      validity: k.validity, expires_at: k.expires_at, revoked_at: k.revoked_at, version: String(k.version), grants: k.grants, limits: k.limits,
    };
  };
  const boundKeys = (credentialId: string, principalId?: string) => state.keys.filter(k => !k.revoked_at && (!principalId || k.principal_id === principalId) && Object.values(k.bindings).includes(credentialId));
  const credentialView = (c: MockCredential): CredentialMeta => ({
    authorized_principal_ids: c.state === 'active' ? state.principals.filter(p => authorized(p.id, c.id)).map(p => p.id) : [],
    id: c.id, provider: c.provider, label: c.label, bound_keys: String(boundKeys(c.id).length), state: c.state, decryptable: c.decryptable, hint: c.hint, version: String(c.version),
    verified_at: c.verified_at, created_at: c.created_at, created_by: c.created_by, created_by_email: email(c.created_by), retired_at: c.retired_at, retired_by_email: email(c.retired_by),
  });
  const credentialList = (): CredentialList => ({ providers: catalogProviders.map(({ provider }) => {
    const mine = state.credentials.filter(c => c.provider === provider), active = mine.filter(c => c.state === 'active');
    return { provider, status: active.length === 0 ? 'missing' : active.some(c => !c.decryptable) ? 'undecryptable' : 'active', credentials: active.map(credentialView), history: mine.filter(c => c.state === 'retired').sort((a, b) => (b.retired_at ?? '').localeCompare(a.retired_at ?? '')).map(credentialView) };
  }) });
  /** 가상 키 한도의 최대값: 정책 한도와 실행 상한 가운데 작은 값. */
  const keyMaximum = (p: MockPrincipal): Limits => {
    const top = ceiling().limits, own = p.policy.limits, less = (a: number | null, b: number | null) => a === null ? b : b === null ? a : Math.min(a, b);
    return { budget: own.budget ?? top.budget, rpm: less(own.rpm, top.rpm), tpm: less(own.tpm, top.tpm), count_rpm: less(own.count_rpm, top.count_rpm) };
  };
  const me = (): Me => {
    const p = principal(ADMIN_ID);
    // refresh_url 은 https 여야 화면이 받아들인다. 가짜 API 는 401 을 내지 않으므로 실제로 불리지는 않는다.
    // logout_url 은 일부러 https 가 아닌 주소로 둔다. 그러면 로그아웃할 때 화면이 이 주소 대신 /auth/login(아래 안내 페이지)으로 간다.
    return { principal: { id: p.id, display_name: p.display_name, email: p.email ?? '', block_state: block(p) }, role: 'admin', grants: p.policy.grants, key_limit_maximum: keyMaximum(p), budget_timezone: timezoneView(), provider_credentials: authorizedView(p.id), auth_expires_at: iso(Date.now() + 8 * 3600000), sso: { refresh_url: 'https://sso.example.test/refresh', logout_url: 'http://localhost/auth/logout' } };
  };

  // ── 가상 키 ──
  const listKeys = (query: URLSearchParams, principalId: string | null) => {
    const at = Date.now(), status = query.get('status');
    const expired = (k: MockKey) => !k.revoked_at && !!k.expires_at && new Date(k.expires_at).getTime() <= at;
    if (status && !['active', 'revoked', 'expired'].includes(status)) fail(400, 'invalid_filter');
    const items = state.keys.filter(k => (!principalId || k.principal_id === principalId) && (!status || (status === 'revoked' ? !!k.revoked_at : status === 'expired' ? expired(k) : !k.revoked_at && !expired(k))));
    return page(items.sort((a, b) => b.created_at.localeCompare(a.created_at)).map(keyView), query);
  };
  /** 가상 키의 허용 API 와 Provider 키 연결을 서버 규칙대로 확인한다. */
  const checkBindings = (ownerId: string, grants: Grant[], bindings: Record<string, string>) => {
    if (Object.keys(bindings).length === 0 || grants.some(g => !bindings[g.provider])) fail(400, 'provider_credential_required');
    for (const [provider, id] of Object.entries(bindings)) if (!state.credentials.some(c => c.id === id && c.provider === provider && c.state === 'active') || !authorized(ownerId, id)) fail(403, 'provider_credential_not_authorized');
  };
  const issueKey = (body: Json, commandId: string): Reply => {
    const owner = body.service_principal_id === undefined ? principal(ADMIN_ID) : principal(text(body.service_principal_id));
    if (body.service_principal_id !== undefined && owner.kind !== 'service') fail(400, 'invalid_request');
    const name = text(body.name).trim(), validity = text(body.validity) as Validity, months: Record<string, number | null> = { '3_months': 3, '6_months': 6, '1_year': 12, '3_years': 36, unlimited: null };
    if (!name || !(validity in months)) fail(400, 'invalid_request');
    if (name.toLowerCase().startsWith('quota')) fail(409, 'key_quota_exceeded');
    const asked = (body.limits ?? {}) as Limits, most = keyMaximum(owner);
    if ((['rpm', 'tpm', 'count_rpm'] as const).some(f => asked[f] != null && most[f] !== null && asked[f]! > most[f]!) || (asked.budget && most.budget && Number(asked.budget.limit_usd) > Number(most.budget.limit_usd))) fail(422, 'limit_exceeds_maximum');
    const grants = (body.grants ?? []) as Grant[], bindings = (body.provider_credentials ?? {}) as Record<string, string>;
    checkBindings(owner.id, grants, bindings);
    const created = new Date(), span = months[validity], expires = span === null ? null : new Date(created);
    if (expires && span !== null) expires.setUTCMonth(expires.getUTCMonth() + span);
    const secret = `mgw_v1_fake_${crypto.randomUUID().replace(/-/g, '')}`, n = state.keys.length + 1;
    const key: MockKey = { id: crypto.randomUUID(), principal_id: owner.id, name, key_hint: secret.slice(0, 16), created_at: created.toISOString(), created_by: ADMIN_ID, created_command_id: commandId, validity, expires_at: expires?.toISOString() ?? null, revoked_at: null, version: 1, grants, limits: (body.limits ?? { budget: null, rpm: null, tpm: null, count_rpm: null }) as Limits, bindings, scope_id: uid(6, 100 + n) };
    state.keys.push(key);
    return { status: 201, body: { key: secret, metadata: keyView(key) } };
  };
  const findKey = (id: string): MockKey => state.keys.find(k => k.id === id) ?? fail(404, 'resource_not_found');
  const editKey = (id: string, body: Json, commandId: string): Reply => {
    const key = findKey(id); expect(body.expected_version, key.version);
    const name = text(body.name).trim(), grants = (body.grants ?? []) as Grant[], bindings = (body.provider_credentials ?? {}) as Record<string, string>;
    if (!name || key.revoked_at) fail(400, 'invalid_request');
    checkBindings(key.principal_id, grants, bindings);
    Object.assign(key, { name, grants, bindings, version: key.version + 1 });
    return { status: 200, body: ack(commandId, 'update_key', 'key', key.id, key.version) };
  };
  const revokeKey = (id: string, query: URLSearchParams): Reply => {
    const key = findKey(id); expect(query.get('expected_version'), key.version);
    if (!key.revoked_at) { key.revoked_at = now(); key.version++; }
    return { status: 204 };
  };

  // ── 사용자·정책 ──
  const listPrincipals = (query: URLSearchParams) => {
    const kind = query.get('kind'), disabled = query.get('disabled'), needle = query.get('email')?.toLowerCase();
    if ((kind && kind !== 'human' && kind !== 'service') || (disabled && disabled !== 'true' && disabled !== 'false')) fail(400, 'invalid_filter');
    return page(state.principals.filter(p => (!kind || p.kind === kind) && (!disabled || String(p.policy.disabled) === disabled) && (!needle || (p.email ?? '').toLowerCase().includes(needle))).map(summary), query);
  };
  const humanOwner = (id: unknown) => { const owner = state.principals.find(p => p.id === id && p.kind === 'human'); if (!owner) fail(400, 'invalid_request'); return owner!.id; };
  const createService = (body: Json, commandId: string): Reply => {
    const name = text(body.display_name).trim(), n = state.principals.length + 1;
    if (!name) fail(400, 'invalid_request');
    const created: MockPrincipal = { id: crypto.randomUUID(), kind: 'service', display_name: name, email: null, management_owner_id: humanOwner(body.management_owner_id), version: 1, policy: { id: uid(5, n), version: 1, disabled: false, grants: [], limits: { budget: null, rpm: null, tpm: null, count_rpm: null } }, safety: { paused: false, version: '1', reason: null, paused_at: null, trigger_request_id: null }, scope_id: uid(6, n) };
    state.principals.push(created);
    return { status: 201, body: ack(commandId, 'create_service', 'principal', created.id, 1) };
  };
  const updateService = (id: string, body: Json, commandId: string): Reply => {
    const p = principal(id); expect(body.expected_version, p.version);
    const name = text(body.display_name).trim();
    if (p.kind !== 'service' || !name) fail(400, 'invalid_request');
    const owner = humanOwner(body.management_owner_id), changed = name !== p.display_name || owner !== p.management_owner_id;
    if (changed) Object.assign(p, { display_name: name, management_owner_id: owner, version: p.version + 1 });
    return { status: 200, body: ack(commandId, 'update_service', 'principal', p.id, p.version, changed) };
  };
  const replacePolicy = (id: string, body: Json, commandId: string): Reply => {
    const p = principal(id); expect(body.expected_version, p.policy.version);
    if (!text(body.reason).trim()) fail(400, 'invalid_request');
    // 자기 자신을 정지하면 서버가 거절한다.
    if (id === ADMIN_ID && body.disabled === true) fail(409, 'security_fence_conflict');
    p.policy = { ...p.policy, version: p.policy.version + 1, disabled: body.disabled === true, grants: (body.grants ?? []) as Grant[], limits: body.limits as Limits };
    return { status: 200, body: ack(commandId, 'replace_policy', 'principal', p.id, p.policy.version) };
  };
  const releasePause = (id: string, body: Json, commandId: string): Reply => {
    const p = principal(id); expect(body.expected_version, p.safety.version);
    if (!text(body.reason).trim()) fail(400, 'invalid_request');
    const changed = p.safety.paused;
    if (changed) p.safety = { paused: false, version: String(Number(p.safety.version) + 1), reason: null, paused_at: null, trigger_request_id: null };
    return { status: 200, body: ack(commandId, 'release_inference_pause', 'principal', p.id, p.safety.version, changed) };
  };
  /** 사용자 하나의 Provider 키 인가를 바꾼다. 빠지는 키에 연결된 가상 키가 있으면 옮기거나 막을지 물어야 한다. */
  const authorize = (id: string, body: Json, commandId: string): Reply => {
    const p = principal(id), provider = text(body.provider), ids = (body.credential_ids ?? []) as string[];
    const current = state.credentials.filter(c => c.provider === provider && c.state === 'active' && authorized(p.id, c.id)).map(c => c.id);
    const expected = (body.expected as { credential_ids?: string[] } | undefined)?.credential_ids ?? [];
    if (!sameSet(expected, current)) fail(409, 'credential_authorization_conflict', { credential_ids: current });
    for (const credentialId of ids) activeCredential(provider, credentialId);
    const affected = current.filter(c => !ids.includes(c)).flatMap(c => boundKeys(c, p.id));
    let moved = 0, blocked = 0;
    if (affected.length) {
      const choice = body.bound_keys as undefined | 'block' | { move_to?: string };
      if (choice === undefined) fail(409, 'credential_authorization_in_use', { bound_keys: String(affected.length) });
      if (choice === 'block') blocked = affected.length;
      else { const to = text(choice?.move_to); if (!ids.includes(to)) fail(400, 'invalid_request'); for (const key of affected) { key.bindings = { ...key.bindings, [provider]: to }; key.version++; } moved = affected.length; }
    }
    const set = state.authorizations.get(p.id) ?? new Set<string>();
    for (const c of current) set.delete(c);
    for (const c of ids) set.add(c);
    state.authorizations.set(p.id, set);
    const changed = !sameSet(ids, current) || moved > 0;
    if (changed) state.authorizationVersion++;
    return { status: 200, body: ack(commandId, 'authorize_principal_credentials', 'principal', p.id, state.authorizationVersion, changed, { authorization: { provider, credential_ids: ids, moved_keys: String(moved), blocked_keys: String(blocked) } }) };
  };

  // ── Provider 키 ──
  /** Provider 에 키를 확인하는 단계 대신, 키 값의 모양과 오류 장치만 본다. */
  const verifySecret = (provider: string, secret: string) => {
    if (secret.length < 20 || !secret.startsWith(provider === 'anthropic' ? 'sk-ant-' : 'sk-')) fail(400, 'invalid_request');
    if (secret.includes('reject')) fail(422, 'provider_credential_rejected');
    if (secret.includes('unverified')) fail(502, 'provider_credential_unverified');
  };
  const checkLabel = (provider: string, label: string, except?: string) => {
    if (!label || label !== label.trim() || label.length > 60) fail(400, 'invalid_request');
    if (state.credentials.some(c => c.provider === provider && c.state === 'active' && c.label === label && c.id !== except)) fail(409, 'credential_label_taken');
  };
  const addCredential = (provider: string, body: Json, commandId: string): Reply => {
    if (!catalogProviders.some(p => p.provider === provider)) fail(404, 'resource_not_found');
    const label = text(body.label), secret = text(body.secret);
    checkLabel(provider, label); verifySecret(provider, secret);
    const created: MockCredential = { id: crypto.randomUUID(), provider, label, hint: secret.slice(-4), version: 1, state: 'active', decryptable: true, verified_at: now(), created_at: now(), created_by: ADMIN_ID, retired_at: null, retired_by: null };
    state.credentials.push(created);
    return { status: 201, body: ack(commandId, 'add_provider_credential', 'credential', created.id, 1, true, { credential: credentialView(created) }) };
  };
  const retire = (c: MockCredential) => { Object.assign(c, { state: 'retired', retired_at: now(), retired_by: ADMIN_ID, version: c.version + 1 }); for (const set of state.authorizations.values()) set.delete(c.id); };
  const rebind = (from: string, to: string) => { for (const key of state.keys) for (const [provider, id] of Object.entries(key.bindings)) if (id === from) key.bindings = { ...key.bindings, [provider]: to }; };
  /** 키 교체: 새 Provider 키가 이름·인가·가상 키 연결을 이어받고, 이전 키는 해제 이력으로 간다. */
  const replaceCredential = (provider: string, id: string, body: Json, commandId: string): Reply => {
    const old = activeCredential(provider, id); expect(body.expected_version, old.version);
    const secret = text(body.secret); verifySecret(provider, secret);
    const created: MockCredential = { ...old, id: crypto.randomUUID(), hint: secret.slice(-4), version: 1, verified_at: now(), created_at: now(), created_by: ADMIN_ID };
    for (const set of state.authorizations.values()) if (set.has(old.id)) set.add(created.id);
    rebind(old.id, created.id); retire(old);
    state.credentials.splice(state.credentials.indexOf(old), 0, created);
    return { status: 201, body: ack(commandId, 'replace_provider_credential', 'credential', created.id, 1, true, { credential: credentialView(created) }) };
  };
  const renameCredential = (provider: string, id: string, body: Json, commandId: string): Reply => {
    const c = activeCredential(provider, id); expect(body.expected_version, c.version);
    const label = text(body.label); checkLabel(provider, label, c.id);
    const changed = label !== c.label;
    if (changed) Object.assign(c, { label, version: c.version + 1 });
    return { status: 200, body: ack(commandId, 'update_provider_credential', 'credential', c.id, c.version, changed, { credential: credentialView(c) }) };
  };
  const retireCredential = (provider: string, id: string, query: URLSearchParams, commandId: string): Reply => {
    const c = activeCredential(provider, id); expect(query.get('expected_version'), c.version);
    const bound = boundKeys(c.id).length, to = query.get('move_to');
    if (bound > 0 && !to) fail(409, 'credential_in_use', { bound_keys: String(bound) });
    if (to) { if (to === c.id) fail(400, 'invalid_request'); rebind(c.id, activeCredential(provider, to).id); }
    retire(c);
    return { status: 200, body: ack(commandId, 'retire_provider_credential', 'credential', c.id, c.version, true, { credential: credentialView(c) }) };
  };
  /** Provider 키 하나를 여러 사용자에게 한 번에 인가한다. 인가를 더하기만 한다. */
  const grantCredential = (provider: string, id: string, body: Json, commandId: string): Reply => {
    const c = activeCredential(provider, id), ids = (body.principal_ids ?? []) as string[];
    if (ids.length === 0 || ids.length > 500) fail(400, 'invalid_request');
    for (const principalId of ids) principal(principalId);
    let added = 0;
    for (const principalId of ids) { const set = state.authorizations.get(principalId) ?? new Set<string>(); if (!set.has(c.id)) added++; set.add(c.id); state.authorizations.set(principalId, set); }
    if (added) state.authorizationVersion++;
    return { status: 200, body: ack(commandId, 'authorize_principal_credentials', 'credential', c.id, state.authorizationVersion, added > 0, { credential: credentialView(c) }) };
  };

  // ── 요청·수동 정산 ──
  const requestSummary = (r: MockState['requests'][number]): RequestSummary => ({ id: r.id, parent_request_id: r.parent_request_id, purpose: r.purpose, principal_id: r.principal_id, key_id: r.key_id, provider: r.provider, model: r.model, api: r.api, admitted_at: r.admitted_at, finished_at: r.finished_at, phase: r.phase, outcome: r.outcome, accounting_state: r.accounting_state, cost_usd: r.cost_usd, version: r.version, reason_code: r.reason_code, provider_credential: r.provider_credential });
  const listRequests = (query: URLSearchParams) => {
    // 서버 기본값과 같이 회계 상태를 지정하지 않으면 미정산(unknown)만 본다.
    const accounting = query.get('accounting_state') ?? 'unknown', from = query.get('from'), to = query.get('to');
    const equal = (name: string, value: string | null) => !query.get(name) || query.get(name) === value;
    const items = state.requests.filter(r => r.accounting_state === accounting && equal('principal_id', r.principal_id) && equal('key_id', r.key_id) && equal('provider', r.provider) && equal('model', r.model) && equal('phase', r.phase) && equal('outcome', r.outcome) && (!from || r.admitted_at >= from) && (!to || r.admitted_at < to));
    return page(items.sort((a, b) => b.admitted_at.localeCompare(a.admitted_at)).map(requestSummary), query);
  };
  const settle = (id: string, body: Json, commandId: string): Reply => {
    const r = state.requests.find(x => x.id === id) ?? fail(404, 'resource_not_found');
    expect(body.expected_version, r.version);
    const decision = text(body.decision), cost = text(body.cost_usd);
    if (!['charged', 'no_charge'].includes(decision) || !/^(0|[1-9][0-9]{0,15})\.[0-9]{12}$/.test(cost) || !text(body.reason).trim()) fail(400, 'invalid_request');
    if (r.accounting_state !== 'unknown') fail(409, 'settlement_conflict');
    // runtime 종료 기록이 없으면 관리자의 종료 확인 진술이 있어야 한다.
    if (!r.execution_stopped_at && body.execution_stopped_confirmed !== true) fail(409, 'execution_not_final');
    Object.assign(r, { accounting_state: 'settled', cost_usd: cost, phase: 'finished', outcome: r.outcome ?? 'failed', finished_at: r.finished_at ?? now(), version: String(Number(r.version) + 1),
      settlement: { source: 'manual_entry', decision, cost_usd: cost, reason: text(body.reason), actor_principal_id: ADMIN_ID, settled_at: now(), executor_id: r.executor_id, execution_stop_source: r.execution_stopped_at ? 'runtime' : 'admin_confirmation' } });
    if (pico(cost) > 0n) state.usage.push({ at: Date.now(), principal_id: r.principal_id, key_id: r.key_id, provider: r.provider, model: r.model, cost: pico(cost), input: 0, output: 0 });
    return { status: 200, body: ack(commandId, 'settle_unknown', 'request', r.id, r.version) };
  };

  // ── 사용량 ──
  const usage = (query: URLSearchParams, principalId: string | null): Usage => {
    const at = Date.now(), zone = state.timezone.timezone, periodStart = monthStart(zone, at), periodEnd = monthStart(zone, at, 1);
    const from = query.get('from') ? new Date(query.get('from')!).getTime() : periodStart, to = query.get('to') ? new Date(query.get('to')!).getTime() : periodEnd;
    if (Number.isNaN(from) || Number.isNaN(to) || from >= to) fail(400, 'invalid_filter');
    const groupBy = query.get('group_by'), keyId = query.get('key_id');
    const matches = (x: { principal_id: string; key_id: string | null; provider: string; model: string }) => (!principalId || x.principal_id === principalId) && (!keyId || x.key_id === keyId) && (!query.get('provider') || x.provider === query.get('provider')) && (!query.get('model') || x.model === query.get('model'));
    const rows = state.usage.filter(u => u.at >= from && u.at < to && matches(u));
    const open = state.requests.filter(r => matches(r));
    const inRange = (r: { admitted_at: string }) => { const t = new Date(r.admitted_at).getTime(); return t >= from && t < to; };
    const sum = (list: typeof rows) => list.reduce((total, u) => total + u.cost, 0n);
    const groupOf = (x: { key_id: string | null; provider: string; model: string }): Bucket['group'] | null => groupBy === 'provider' ? { kind: 'provider', provider: x.provider } : groupBy === 'model' ? { kind: 'model', provider: x.provider, model: x.model } : groupBy === 'key' ? { kind: 'key', key_id: x.key_id } : null;
    const groups = new Map<string, Bucket['group']>();
    for (const x of [...rows, ...open.filter(inRange)]) { const g = groupOf(x); if (g) groups.set(JSON.stringify(g), g); }
    const buckets: Bucket[] = [...groups].map(([id, group]) => {
      const mine = rows.filter(u => JSON.stringify(groupOf(u)) === id), requests = open.filter(r => inRange(r) && JSON.stringify(groupOf(r)) === id);
      const unknown = requests.filter(r => r.accounting_state === 'unknown').length;
      // 미정산이 남은 그룹은 token 합계를 단정할 수 없다.
      return { group, settled_usd: usd(sum(mine)), unknown_count: String(unknown), input_tokens: unknown ? null : String(mine.reduce((n, u) => n + u.input, 0)), output_tokens: unknown ? null : String(mine.reduce((n, u) => n + u.output, 0)), tokens_complete: unknown === 0 };
    }).sort((a, b) => Number(b.settled_usd) - Number(a.settled_usd));
    const owner = principalId ? principal(principalId) : null, key = keyId ? state.keys.find(k => k.id === keyId) : undefined;
    const balance = (scope: { scope_id: string; kind: 'principal' | 'key'; key_id: string | null; limits: Limits; policy_version: number }, p: MockPrincipal): Balance => {
      const scoped = (x: { principal_id: string; key_id: string | null }) => x.principal_id === p.id && (scope.kind === 'principal' || x.key_id === scope.key_id);
      const settled = sum(state.usage.filter(u => u.at >= periodStart && u.at < periodEnd && scoped(u)));
      const unknown = state.requests.filter(r => r.accounting_state === 'unknown' && scoped(r)).length, limit = scope.limits.budget ? pico(scope.limits.budget.limit_usd) : null;
      const state_ = limit === null ? 'unlimited' : unknown ? 'unresolved' : 'limited', rest = limit === null ? null : limit - settled;
      const reasons = [...block(p).reasons, ...(state_ === 'limited' && rest !== null && rest <= 0n ? ['budget_exhausted'] : [])];
      return { scope_id: scope.scope_id, scope_kind: scope.kind, key_id: scope.key_id, period_start: iso(periodStart), period_end: iso(periodEnd), timezone: zone, policy_version: String(scope.policy_version), config_digest: 'mock0f3a9c1e', limit_usd: limit === null ? null : usd(limit), settled_usd: usd(settled), scope_unknown_count: String(unknown), principal_unknown_count: block(p).global_unknown_count, balance_state: state_, remaining_usd: state_ === 'limited' && rest !== null ? usd(rest > 0n ? rest : 0n) : null, blocked: reasons.length > 0, reasons, as_of: iso(at) };
    };
    const balances = owner ? [balance({ scope_id: owner.scope_id, kind: 'principal', key_id: null, limits: owner.policy.limits, policy_version: owner.policy.version }, owner), ...(key && key.principal_id === owner.id ? [balance({ scope_id: key.scope_id, kind: 'key', key_id: key.id, limits: key.limits, policy_version: owner.policy.version }, owner)] : [])] : [];
    const scopeBlock = block(owner);
    return { currency: 'USD', from: iso(from), to: iso(to), as_of: iso(at), detail_available_from: iso(at - 59 * 86400000), aggregation: 'detail', settled_usd: usd(sum(rows)), period_unknown_count: String(open.filter(r => inRange(r) && r.accounting_state === 'unknown').length), global_unknown_count: scopeBlock.global_unknown_count, global_expired_dispatch_count: scopeBlock.expired_dispatch_count, block_state: owner ? scopeBlock : null, balance_context: !owner ? 'aggregate_no_balance' : balances.length > 1 ? 'current_principal_and_key' : 'current_principal', budget_balances: balances, buckets: { items: buckets, next_cursor: null, as_of: iso(at) } };
  };

  // ── 모델·가격 카탈로그 ──
  const catalogView = () => ({ version: String(state.catalog.version), approved_at: state.catalog.approved_at, approved_by_email: state.catalog.approved_by_email, entries: state.catalog.entries, providers: catalogProviders });
  /** 지금 카탈로그와 가짜 가격 목록을 비교해 변경안을 만든다. 승인 전에는 아무것도 바뀌지 않는다. */
  const syncCatalog = (commandId: string): Reply => {
    const same = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b);
    const items: CatalogProposalItem[] = priceSource.map(({ provider, model, proposed }) => {
      const current = state.catalog.entries.find(e => e.provider === provider && e.model === model) ?? null;
      return { provider, model, status: !proposed ? 'incomplete' : !current ? 'new' : same(current, proposed) ? 'unchanged' : 'changed', current, proposed };
    });
    for (const current of state.catalog.entries) if (!priceSource.some(s => s.provider === current.provider && s.model === current.model)) items.push({ provider: current.provider, model: current.model, status: 'missing', current, proposed: null });
    const proposal: CatalogProposal = { base_version: state.catalog.version, source: { source: 'models_dev', models: priceSource.length }, items }, id = crypto.randomUUID();
    state.proposals.set(id, proposal);
    return { status: 201, body: ack(commandId, 'sync_model_catalog', 'catalog', id, state.catalog.version, true, { proposal_id: id, proposal }) };
  };
  const approveCatalog = (body: Json, commandId: string): Reply => {
    const id = text(body.proposal_id), proposal = state.proposals.get(id);
    if (!proposal || proposal.base_version !== state.catalog.version) fail(409, 'catalog_proposal_stale');
    expect(body.expected_version, state.catalog.version);
    const picked = (body.items ?? []) as { provider: string; model: string }[];
    if (picked.length === 0) fail(400, 'invalid_request');
    for (const pick of picked) {
      const item = proposal!.items.find(i => i.provider === pick.provider && i.model === pick.model) ?? fail(400, 'invalid_request');
      const index = state.catalog.entries.findIndex(e => e.provider === item.provider && e.model === item.model);
      if (item.status === 'missing') { if (index >= 0) state.catalog.entries.splice(index, 1); }
      else if (item.proposed && (item.status === 'changed' || item.status === 'new')) { if (index >= 0) state.catalog.entries[index] = item.proposed; else state.catalog.entries.push(item.proposed); }
    }
    state.catalog.entries.sort((a, b) => catalogProviders.findIndex(p => p.provider === a.provider) - catalogProviders.findIndex(p => p.provider === b.provider));
    Object.assign(state.catalog, { version: state.catalog.version + 1, approved_at: now(), approved_by_email: email(ADMIN_ID) });
    state.proposals.clear();
    return { status: 200, body: ack(commandId, 'approve_model_catalog', 'catalog', id, state.catalog.version, true, { proposal_id: id }) };
  };

  // ── 설정 ──
  // 미리보기 서버에는 Admin API 키가 있다고 친다(사후 보정 스위치를 켤 수 있다).
  const cutoffRuleView = () => ({ provider: 'anthropic', cutoff_rule: state.cutoffRule.cutoff_rule, correction: state.cutoffRule.correction, correction_available: true, version: String(state.cutoffRule.version) });
  const setCutoffRule = (body: Json, commandId: string): Reply => {
    expect(body.expected_version, state.cutoffRule.version);
    const rule = text(body.cutoff_rule);
    if (text(body.provider) !== 'anthropic' || (rule !== 'received' && rule !== 'max_tokens')) fail(400, 'invalid_request');
    if (body.correction !== undefined && typeof body.correction !== 'boolean') fail(400, 'invalid_request');
    const correction = typeof body.correction === 'boolean' ? body.correction : state.cutoffRule.correction;
    const changed = rule !== state.cutoffRule.cutoff_rule || correction !== state.cutoffRule.correction;
    if (changed) Object.assign(state.cutoffRule, { cutoff_rule: rule, correction, version: state.cutoffRule.version + 1 });
    return { status: 200, body: ack(commandId, 'set_settlement_rule', 'settings', '00000000-0000-0000-0000-000000000000', state.cutoffRule.version, changed, { cutoff_rule: cutoffRuleView() }) };
  };
  // ── 토큰 대조: 미리보기는 9일 전부터 사용량 보고서를 받아 두었고, 첫 Anthropic 키만 API 키 목록에서 찾았다고 친다 ──
  const tokenCheck = (query: URLSearchParams) => {
    const hours = ({ '24h': 24, '7d': 168, '30d': 720 } as Record<string, number>)[query.get('range') ?? '24h'] ?? fail(400, 'invalid_request');
    const to = Math.floor((Date.now() - 600000) / 3600000) * 3600000, from = to - hours * 3600000, coveredFrom = to - 9 * 24 * 3600000;
    const scale = (Math.min(to - from, to - coveredFrom) / 3600000) / 24, n = (v: number) => Math.round(v * scale);
    const anthropic = state.credentials.filter(c => c.provider === 'anthropic' && c.state === 'active');
    const credential = anthropic[0] ? { id: anthropic[0].id, label: anthropic[0].label, hint: anthropic[0].hint } : null;
    // 금액: 토큰 × 백만 토큰당 USD. 미리보기는 출력 가격 하나로 어림한다. 가격을 모르는 모델은 null.
    const price: Record<string, number> = { 'claude-opus-5-5': 25, 'claude-haiku-5-5': 5 };
    const money = (model: string, tokens: number) => price[model] === undefined ? null : usd(BigInt(tokens) * BigInt(price[model]) * 1000000n);
    type Reason = { reason: string; tokens: number };
    const row = (model: string, provider: number, gateway: number, more: Reason[], less: number) => ({ model, provider, gateway, more, less });
    const raw = [
      row('claude-opus-5-5', n(1204880), n(1179900), [{ reason: 'cutoff', tokens: n(24500) }, { reason: 'unexplained', tokens: n(7320) }], n(6840)),
      row('claude-haiku-5-5', n(402000), n(396800), [{ reason: 'unknown', tokens: n(5200) }], 0),
      row('claude-sonnet-5-5', n(88000), 0, [{ reason: 'outside', tokens: n(88000) }], 0),
    ];
    const sum = (list: number[]) => list.reduce((a, b) => a + b, 0);
    const moreOf = (r: typeof raw[number]) => sum(r.more.map(x => x.tokens));
    const usdSum = (list: (string | null)[]) => usd(list.reduce<bigint>((a, b) => a + (b === null ? 0n : pico(b)), 0n));
    const rows = raw.map(r => ({ credential, api_key_id: 'apikey_01PreviewPlatform', model: r.model, provider_tokens: String(r.provider), gateway_tokens: String(r.gateway), provider_more_tokens: String(moreOf(r)), gateway_more_tokens: String(r.less), provider_more_usd: money(r.model, moreOf(r)), gateway_more_usd: money(r.model, r.less), reasons: r.more.map(x => ({ reason: x.reason, tokens: String(x.tokens) })) }));
    const reasonTotal = (reason: string) => raw.flatMap(r => r.more.filter(x => x.reason === reason).map(x => ({ model: r.model, tokens: x.tokens })));
    return {
      enabled: true,
      range: { from: iso(from), to: iso(to) },
      compared: { from: iso(Math.max(from, coveredFrom)), to: iso(to) },
      state: { last_attempt_at: iso(Date.now() - 120000), last_success_at: iso(Date.now() - 120000), last_error: null, minute_convention: state.cutoffRule.correction ? 'end' : null, start_minutes: '0', end_minutes: state.cutoffRule.correction ? '14' : '0' },
      totals: {
        provider_tokens: String(sum(raw.map(r => r.provider))), gateway_tokens: String(sum(raw.map(r => r.gateway))),
        provider_more_tokens: String(sum(raw.map(moreOf))), gateway_more_tokens: String(sum(raw.map(r => r.less))),
        provider_more_usd: usdSum(rows.map(r => r.provider_more_usd)), gateway_more_usd: usdSum(rows.map(r => r.gateway_more_usd)),
        unpriced_models: raw.filter(r => price[r.model] === undefined).map(r => r.model), corrected_requests: String(Math.max(1, Math.round(scale))),
        reasons: ['cutoff', 'unknown', 'outside', 'unexplained'].map(reason => { const parts = reasonTotal(reason); return { reason, tokens: String(sum(parts.map(p => p.tokens))), usd: parts.length > 0 && parts.every(p => price[p.model] === undefined) ? null : usdSum(parts.map(p => money(p.model, p.tokens))) }; }),
      },
      rows,
      unmatched_credentials: anthropic.slice(1).map(c => ({ id: c.id, label: c.label, hint: c.hint })),
    };
  };
  const setTimezone = (body: Json, commandId: string): Reply => {
    expect(body.expected_version, state.timezone.version);
    const zone = text(body.timezone);
    try { new Intl.DateTimeFormat('en-US', { timeZone: zone }); } catch { fail(400, 'invalid_request'); }
    const pending = zone === state.timezone.timezone ? null : zone, changed = pending !== state.timezone.pending;
    Object.assign(state.timezone, { pending, version: state.timezone.version + 1 });
    return { status: 200, body: ack(commandId, 'set_budget_timezone', 'settings', '00000000-0000-0000-0000-000000000000', state.timezone.version, changed, { budget_timezone: timezoneView() }) };
  };

  // ── 접근 목록 ──
  const FIXED = 'admin@example.test';
  const accessView = (a: MockState['access'][number]) => {
    const p = state.principals.find(x => x.kind === 'human' && x.email === a.email);
    return { ...a, version: String(a.version), fixed: a.email === FIXED, principal: p ? { id: p.id, disabled: Boolean(p.policy.disabled) } : null };
  };
  const accessEntry = (id: string) => state.access.find(a => a.id === id) ?? fail(404, 'resource_not_found');
  const changeable = (id: string, expected: unknown) => {
    const a = accessEntry(id);
    if (a.email === FIXED) fail(409, 'access_protected');
    expect(expected, a.version);
    return a;
  };
  const addAccess = (body: Json, commandId: string): Reply => {
    const email = text(body.email).trim().toLowerCase(), role = text(body.role);
    if (!email.includes('@') || (role !== 'user' && role !== 'admin')) fail(400, 'invalid_request');
    if (state.access.some(a => a.email === email)) fail(409, 'access_exists');
    const a = { id: `acc-${Date.now()}`, email, role: role as 'user' | 'admin', added_by_email: FIXED, added_at: now(), version: 1 };
    state.access.push(a); state.access.sort((x, y) => x.email.localeCompare(y.email));
    return { status: 201, body: ack(commandId, 'add_access', 'access', a.id, 1, true, { access: accessView(a) }) };
  };
  const updateAccess = (id: string, body: Json, commandId: string): Reply => {
    const a = changeable(id, body.expected_version), role = text(body.role);
    if (role !== 'user' && role !== 'admin') fail(400, 'invalid_request');
    Object.assign(a, { role, version: a.version + 1 });
    return { status: 200, body: ack(commandId, 'update_access', 'access', a.id, a.version, true, { access: accessView(a) }) };
  };
  const removeAccess = (id: string, query: URLSearchParams, commandId: string): Reply => {
    const a = changeable(id, query.get('expected_version'));
    state.access = state.access.filter(x => x.id !== a.id);
    return { status: 200, body: ack(commandId, 'remove_access', 'access', a.id, null) };
  };

  /** 경로와 메서드로 처리기를 고른다. 모르는 경로는 404 resource_not_found. */
  return (method: string, path: string, query: URLSearchParams, body: Json, commandId: string): Reply => {
    const s = path.split('/').filter(Boolean).map(decodeURIComponent), ok = (value: unknown): Reply => ({ status: 200, body: value }), at = `${method} /${s.slice(0, 3).join('/')}`;
    if (s[1] !== 'admin') {
      if (method === 'GET' && path === '/api/me') return ok(me());
      if (method === 'GET' && path === '/api/csrf') return ok({ token: 'mock-csrf-token', expires_at: iso(Date.now() + 600000) });
      if (method === 'GET' && path === '/api/usage') return ok(usage(query, ADMIN_ID));
      if (method === 'GET' && path === '/api/usage/models') return ok({ items: [...new Map(state.requests.filter(r => r.principal_id === ADMIN_ID).map(r => [`${r.provider}/${r.model}`, { provider: r.provider, model: r.model }])).values()] });
      if (method === 'GET' && path === '/api/keys') return ok(listKeys(query, ADMIN_ID));
      if (method === 'POST' && path === '/api/keys') return issueKey(body, commandId);
      if (s[1] === 'keys' && s.length === 3) { if (method === 'PATCH') return editKey(s[2], body, commandId); if (method === 'DELETE') return revokeKey(s[2], query); }
      return fail(404, 'resource_not_found');
    }
    switch (at) {
      case 'GET /api/admin/keys': if (s.length === 3) return ok(listKeys(query, query.get('principal_id'))); break;
      case 'GET /api/admin/usage': return ok(usage(query, query.get('principal_id')));
      case 'GET /api/admin/principals': return s.length === 3 ? ok(listPrincipals(query)) : ok(detail(principal(s[3])));
      case 'POST /api/admin/principals': if (s.length === 3) return createService(body, commandId); if (s[4] === 'inference-pause' && s[5] === 'release') return releasePause(s[3], body, commandId); break;
      case 'PATCH /api/admin/principals': if (s.length === 4) return updateService(s[3], body, commandId); break;
      case 'PUT /api/admin/principals': if (s[4] === 'policy') return replacePolicy(s[3], body, commandId); if (s[4] === 'provider-credentials') return authorize(s[3], body, commandId); break;
      case 'GET /api/admin/provider-credentials': if (s.length === 3) return ok(credentialList()); break;
      case 'POST /api/admin/provider-credentials': if (s.length === 4) return addCredential(s[3], body, commandId); if (s.length === 5) return grantCredential(s[3], s[4], body, commandId); break;
      case 'PUT /api/admin/provider-credentials': if (s.length === 5) return replaceCredential(s[3], s[4], body, commandId); break;
      case 'PATCH /api/admin/provider-credentials': if (s.length === 5) return renameCredential(s[3], s[4], body, commandId); break;
      case 'DELETE /api/admin/provider-credentials': if (s.length === 5) return retireCredential(s[3], s[4], query, commandId); break;
      case 'GET /api/admin/requests':
        if (s.length === 3) return ok(listRequests(query));
        if (s[3] === 'models') return ok({ items: [...new Map(state.requests.map(r => [`${r.provider}/${r.model}`, { provider: r.provider, model: r.model }])).values()] });
        return ok(state.requests.find(r => r.id === s[3]) ?? fail(404, 'resource_not_found'));
      case 'PUT /api/admin/requests': if (s[4] === 'settlement') return settle(s[3], body, commandId); break;
      case 'GET /api/admin/model-catalog': if (s.length === 3) return ok(catalogView()); break;
      case 'POST /api/admin/model-catalog': if (s[3] === 'sync') return syncCatalog(commandId); if (s[3] === 'approve') return approveCatalog(body, commandId); break;
      case 'GET /api/admin/access': if (s.length === 3) return ok({ items: state.access.map(accessView) }); break;
      case 'POST /api/admin/access': if (s.length === 3) return addAccess(body, commandId); break;
      case 'PATCH /api/admin/access': if (s.length === 4) return updateAccess(s[3], body, commandId); break;
      case 'DELETE /api/admin/access': if (s.length === 4) return removeAccess(s[3], query, commandId); break;
      case 'GET /api/admin/settings': if (s.length === 3) return ok({ budget_timezone: timezoneView(), cutoff_rule: cutoffRuleView() }); break;
      case 'GET /api/admin/token-check': if (s.length === 3) return ok(tokenCheck(query)); break;
      case 'PUT /api/admin/settings': if (s[3] === 'budget-timezone') return setTimezone(body, commandId); if (s[3] === 'cutoff-rule') return setCutoffRule(body, commandId); break;
    }
    return fail(404, 'resource_not_found');
  };
}

const loginPage = '<!doctype html><html lang="ko"><head><meta charset="UTF-8"><title>m-gated 개발용 미리보기</title></head><body style="font-family:system-ui;margin:3rem"><h1>개발용 미리보기</h1><p>실제 SSO 가 없는 가짜 로그인 화면입니다. 대시보드는 항상 관리자(admin@example.test)로 로그인된 상태로 열립니다.</p><p><a href="/dashboard/">대시보드로 돌아가기</a></p></body></html>';

/** `/api/*`, `/auth/*` 를 메모리 안의 가짜 데이터로 응답하는 Vite 플러그인. 개발 서버에서만 동작한다. */
export function mockApi(): Plugin {
  return {
    name: 'm-gated-mock-api',
    apply: 'serve',
    configureServer(server) {
      const route = api(createState());
      const send = (res: Res, status: number, body?: unknown, type = 'application/json') => {
        res.statusCode = status; res.setHeader('Cache-Control', 'no-store');
        if (body === undefined) { res.end(); return; }
        res.setHeader('Content-Type', `${type}; charset=utf-8`); res.end(typeof body === 'string' ? body : JSON.stringify(body));
      };
      const read = (req: Req) => new Promise<string>((resolve, reject) => { let raw = ''; req.setEncoding('utf8'); req.on('data', chunk => { raw += chunk; }); req.on('end', () => resolve(raw)); req.on('error', () => reject(new Error('body'))); });
      const handle = async (req: Req, res: Res, next: () => void) => {
        const url = new URL(req.url ?? '/', 'http://mock.local'), method = req.method ?? 'GET', path = url.pathname;
        if (path.startsWith('/auth/')) {
          // 실제 SSO 대신 안내 페이지를 보여 준다. refresh 는 성공한 것으로 답한다.
          if (path === '/auth/refresh') send(res, 204); else send(res, 200, loginPage, 'text/html');
          return;
        }
        if (!path.startsWith('/api/')) { next(); return; }
        const failure = (status: number, code: string, details: Json = {}) => send(res, status, { error: { code, message: '관리 요청을 처리할 수 없습니다.', request_id: crypto.randomUUID(), details } });
        try {
          let body: Json = {}, commandId = '';
          if (method !== 'GET') {
            // 실제 서버처럼 쓰기에는 CSRF 토큰과 명령 ID 헤더가 있어야 한다.
            commandId = String(req.headers['x-gateway-command-id'] ?? '');
            if (!req.headers['x-csrf-token']) { failure(403, 'csrf_invalid'); return; }
            if (!/^[0-9a-f-]{36}$/i.test(commandId)) { failure(400, 'invalid_request'); return; }
            const raw = await read(req);
            if (raw) { const parsed: unknown = JSON.parse(raw); if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) { failure(400, 'invalid_request'); return; } body = parsed as Json; }
          }
          // 조회 중 표시가 잠깐 보이도록 조금 늦게 답한다.
          await new Promise(resolve => setTimeout(resolve, 80));
          const reply = route(method, path, url.searchParams, body, commandId);
          send(res, reply.status, reply.body);
        } catch (error) {
          if (error instanceof HttpError) failure(error.status, error.code, error.details);
          else if (error instanceof SyntaxError) failure(400, 'invalid_request');
          else { server.config.logger.error(`[mock] ${method} ${path} 처리 실패: ${String(error)}`); failure(500, 'invariant_violation'); }
        }
      };
      server.middlewares.use((req: unknown, res: unknown, next: () => void) => { void handle(req as Req, res as Res, next); });
      server.config.logger.info('  [mock] 가짜 API 사용 중: /api/*, /auth/* 는 메모리 안의 가짜 데이터로 응답합니다.');
    },
  };
}
