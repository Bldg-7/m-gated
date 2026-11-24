import { cleanup, fireEvent, render, screen, within } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../../app';
import { pickOption } from '../../test-select';
import { session } from '../../lib/api';
const principal = { id: 'service1', kind: 'service', display_name: '서비스', email: null, management_owner_id: 'owner1', version: '7', policy: { id: 'policy1', version: '13', disabled: false, grants: [], limits: { budget: null, rpm: null, tpm: null, count_rpm: null } }, inference_safety: { paused: true, version: '29', reason: 'cost_bound_violation', paused_at: '2026-01-01Z', trigger_request_id: 'request1' }, block_state: { blocked: true, reasons: ['inference_paused'], global_unknown_count: '0', expired_dispatch_count: '0' } };
const me = { principal: { id: 'admin1', display_name: '관리자', email: 'admin@example.test' }, role: 'admin', grants: [], sso: { refresh_url: 'https://auth.example.test/sso/refresh' } };
const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
afterEach(() => { cleanup(); session.clear(); history.replaceState(null, '', '/dashboard/'); vi.unstubAllGlobals(); });
test('admin_direct_route_uses_separate_principal_policy_safety_cas_and_ack_get', async () => {
  history.replaceState(null, '', '/dashboard/admin/principals?principal_id=service1');
  const calls: { url: string; method: string; body?: Record<string, unknown> }[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input); calls.push({ url, method: init?.method ?? 'GET', body: init?.body ? JSON.parse(String(init.body)) : undefined });
    if (url === '/api/me') return Promise.resolve(response(me));
    if (url.startsWith('/api/usage')) return Promise.resolve(response({ error: { code: 'db_unavailable' } }, 503));
    if (url === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (init?.method) return Promise.resolve(response({ command_id: 'cmd', target: { id: principal.id, kind: 'principal' }, effect: 'changed', result_version: '30', replayed: false }));
    if (url.startsWith('/api/admin/principals/')) return Promise.resolve(response({ principal, execution_ceiling: { grants: [], limits: principal.policy.limits } }));
    return Promise.resolve(response({ items: [], next_cursor: null, as_of: 'now' }));
  }));
  render(<App />);
  await screen.findByRole('heading', { name: '사용자 상세' });
  // 예전 주소는 사용자 상세 하위 페이지로 옮겨지고, 목록으로 돌아가는 링크가 있다.
  expect(location.pathname + location.search).toBe('/dashboard/admin/principals/detail?principal_id=service1');
  const back = screen.getByRole('link', { name: '사용자 목록으로 돌아가기' });
  expect(back.getAttribute('href')).toBe('/dashboard/admin/principals');
  expect(back.textContent).toBe('사용자 목록');
  // 돌아가는 링크는 페이지 제목 바로 위에 있다.
  expect(back.nextElementSibling).toBe(screen.getByRole('heading', { level: 1, name: '사용자 상세' }));
  expect(screen.getByRole('link', { name: '사용자·정책·키 관리' }).getAttribute('aria-current')).toBe('page');
  fireEvent.change(screen.getByLabelText('서비스 이름'), { target: { value: '새 서비스' } });
  fireEvent.submit(screen.getByRole('form', { name: '서비스 변경' }));
  // 관리 명령은 확인 창(모달)에서 실행한다. 창이 열리면 실행 버튼에 초점이 간다.
  const confirm = screen.getByRole('dialog', { name: '서비스 변경 확인' });
  expect(document.activeElement).toBe(within(confirm).getByRole('button', { name: '명령 실행' }));
  expect(confirm.textContent).toContain('새 서비스');
  fireEvent.click(within(confirm).getByRole('button', { name: '명령 실행' }));
  expect(screen.queryByRole('dialog')).toBeNull();
  await screen.findByText(/서버 재조회 완료/);
  expect(calls.find(c => c.method === 'PATCH')?.body?.expected_version).toBe('7');
  fireEvent.change(screen.getByLabelText('정책 변경 사유'), { target: { value: '정책 확인' } });
  fireEvent.submit(screen.getByRole('form', { name: '정책 편집' }));
  fireEvent.click(screen.getByRole('button', { name: '명령 실행' }));
  await screen.findByText(/서버 재조회 완료/);
  expect(calls.find(c => c.method === 'PUT')?.body?.expected_version).toBe('13');
  // 차단 중이면 옅은 붉은 배경의 자동과금 차단 섹션에서 명시 해제한다.
  const safety = screen.getByRole('region', { name: '자동과금 차단됨' });
  expect(safety.className).toContain('card-alert');
  expect(safety.textContent).toContain('이 사용자의 유료 요청을 막고 있습니다.');
  fireEvent.change(within(safety).getByLabelText('안전차단 해제 사유'), { target: { value: '원인 확인' } });
  fireEvent.click(screen.getByLabelText('과금 원인과 남은 차단을 확인했습니다'));
  fireEvent.submit(screen.getByRole('form', { name: '명시 안전차단 해제' }));
  fireEvent.click(screen.getByRole('button', { name: '명령 실행' }));
  await screen.findByText(/서버 재조회 완료/);
  expect(calls.find(c => c.method === 'POST')?.body?.expected_version).toBe('29');
  for (let i = 0; i < calls.length; i++) if (['PATCH', 'PUT', 'POST'].includes(calls[i].method)) expect(calls.slice(i + 1).some(c => c.method === 'GET' && c.url.endsWith('/service1'))).toBe(true);
});

test('safety_section_appears_only_while_inference_is_paused', async () => {
  history.replaceState(null, '', '/dashboard/admin/principals/detail?principal_id=service1');
  const released = { ...principal, inference_safety: { paused: false, version: '30', reason: null, paused_at: null, trigger_request_id: null }, block_state: { ...principal.block_state, blocked: false, reasons: [] } };
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL) => {
    const url = String(input);
    if (url === '/api/me') return Promise.resolve(response(me));
    if (url === '/api/admin/principals/service1') return Promise.resolve(response({ principal: released, execution_ceiling: { grants: [], limits: principal.policy.limits } }));
    return Promise.resolve(response({ items: [], next_cursor: null, as_of: 'now' }));
  }));
  render(<App />);
  await screen.findByLabelText('서비스 이름');
  // 해제 상태에서는 자동과금 차단 섹션 자체가 없다.
  expect(screen.queryByRole('region', { name: '자동과금 차단됨' })).toBeNull();
  expect(screen.queryByRole('form', { name: '명시 안전차단 해제' })).toBeNull();
  expect(screen.queryByLabelText('안전차단 해제 사유')).toBeNull();
});

function deferred<T>() { let resolve!: (value: T) => void; const promise = new Promise<T>(r => { resolve = r; }); return { promise, resolve }; }
test('admin_ack_waits_for_target_get_and_409_clears_old_form_before_reconfirmation', async () => {
  history.replaceState(null, '', '/dashboard/admin/principals?principal_id=service1');
  const get = deferred<Response>(); let reads = 0, writes = 0;
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url === '/api/me') return Promise.resolve(response(me));
    if (url === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (init?.method === 'PATCH') { writes++; return Promise.resolve(response({ error: { code: 'version_conflict', details: { current_version: '8' } } }, 409)); }
    if (url === '/api/admin/principals/service1') return ++reads === 1 ? Promise.resolve(response({ principal, execution_ceiling: { grants: [], limits: principal.policy.limits } })) : get.promise;
    return Promise.resolve(response({ items: [], next_cursor: null, as_of: 'now' }));
  }));
  render(<App />); await screen.findByLabelText('서비스 이름');
  fireEvent.submit(screen.getByRole('form', { name: '서비스 변경' })); fireEvent.click(screen.getByRole('button', { name: '명령 실행' }));
  await screen.findByText('명령 처리·서버 확인 중…');
  // 처리 중 알림은 다시 보내지 않는다는 것과 서버 처리가 이어질 수 있다는 것을 함께 말한다.
  expect(screen.getByText('자동으로 다시 보내지 않습니다. 이 창을 닫아도 서버 처리는 계속될 수 있습니다.')).toBeTruthy();
  await vi.waitFor(() => expect(reads).toBe(2));
  expect(screen.queryByLabelText('서비스 이름')).toBeNull();
  expect(screen.queryByText(/서버 재조회 완료/)).toBeNull();
  get.resolve(response({ principal: { ...principal, version: '8' }, execution_ceiling: { grants: [], limits: principal.policy.limits } }));
  await screen.findByText(/버전이 변경되었습니다/); await screen.findByLabelText('서비스 이름');
  expect(writes).toBe(1); expect(screen.queryByRole('button', { name: '명령 실행' })).toBeNull();
});

test('admin_direct_user_route_never_fetches_administrator_resources', async () => {
  history.replaceState(null, '', '/dashboard/admin/requests?request_id=private-id');
  const fetcher = vi.fn(() => Promise.resolve(response({ ...me, role: 'user' })));
  vi.stubGlobal('fetch', fetcher); render(<App />);
  await screen.findByRole('alert'); expect(screen.getByRole('alert').textContent).toContain('관리자 권한이 없습니다');
  expect(fetcher.mock.calls).toHaveLength(1); expect(screen.queryByRole('heading', { name: '요청 상세' })).toBeNull();
});

test('policy_form_turns_checkboxes_and_limit_fields_into_the_server_policy', async () => {
  history.replaceState(null, '', '/dashboard/admin/principals/detail?principal_id=service1');
  const ceiling = [{ provider: 'anthropic', model: 'claude-opus-5-5', capabilities: ['messages', 'count_tokens'] }, { provider: 'anthropic', model: 'claude-haiku-5-5', capabilities: ['messages'] }];
  const current = { ...principal, policy: { ...principal.policy, grants: [{ provider: 'anthropic', model: 'claude-old-1', capabilities: ['messages'] }], limits: { budget: { limit_usd: '100.500000000000', period: { kind: 'calendar_month' } }, rpm: 5, tpm: null, count_rpm: null } } };
  const writes: Record<string, unknown>[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url === '/api/me') return Promise.resolve(response(me));
    if (url === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (init?.method) { writes.push(JSON.parse(String(init.body))); return Promise.resolve(response({ command_id: 'cmd', target: { id: 'service1', kind: 'principal' }, effect: 'changed', result_version: '14', replayed: false })); }
    if (url === '/api/admin/principals/service1') return Promise.resolve(response({ principal: current, execution_ceiling: { grants: ceiling, limits: principal.policy.limits } }));
    return Promise.resolve(response({ items: [], next_cursor: null, as_of: 'now' }));
  }));
  render(<App />);
  const group = await screen.findByRole('group', { name: '사용할 수 있는 모델·API' });
  // 지금 정책에만 남은 모델도 보여 주어 뺄 수 있게 한다. 현재 값은 입력칸에 짧게 보인다.
  const old = within(group).getByLabelText(/claude-old-1/) as HTMLInputElement;
  expect(old.checked).toBe(true);
  // 모델·API는 표로 고른다. 한 줄이 (Provider, 모델, API) 하나이고, 머리 칸 체크박스가 전체 선택이다.
  expect(within(group).getAllByRole('columnheader').map(h => h.textContent)).toEqual(['', 'Provider', '모델', 'API']);
  expect(within(group).getAllByRole('row')).toHaveLength(5);
  const all = within(group).getByLabelText('전체 선택') as HTMLInputElement;
  expect(all.indeterminate).toBe(true);
  const boxes = () => within(group).getAllByRole('checkbox').filter(box => box !== all) as HTMLInputElement[];
  fireEvent.click(all);
  expect(boxes().every(box => box.checked)).toBe(true);
  fireEvent.click(all);
  expect(boxes().some(box => box.checked)).toBe(false);
  // 줄의 글자를 눌러도 고른다.
  fireEvent.click(within(group).getByText('claude-old-1'));
  expect(old.checked).toBe(true);
  expect(group.textContent).toContain('지금은 쓸 수 없는 모델');
  expect((screen.getByLabelText('월 예산 (USD)') as HTMLInputElement).value).toBe('100.5');
  fireEvent.click(old);
  fireEvent.click(within(group).getByLabelText(/claude-opus-5-5.*\/ messages/));
  fireEvent.click(within(group).getByLabelText(/claude-opus-5-5.*count_tokens/));
  fireEvent.change(screen.getByLabelText('월 예산 (USD)'), { target: { value: '20' } });
  fireEvent.change(screen.getByLabelText('분당 요청 수 한도 (RPM)'), { target: { value: '' } });
  fireEvent.change(screen.getByLabelText('분당 토큰 수 한도 (TPM)'), { target: { value: '50000' } });
  fireEvent.change(screen.getByLabelText('정책 변경 사유'), { target: { value: '모델 추가' } });
  fireEvent.submit(screen.getByRole('form', { name: '정책 편집' }));
  fireEvent.click(screen.getByRole('button', { name: '명령 실행' }));
  await screen.findByText(/서버 재조회 완료/);
  expect(writes[0]).toEqual({ expected_version: '13', disabled: false, reason: '모델 추가', grants: [{ provider: 'anthropic', model: 'claude-opus-5-5', capabilities: ['messages', 'count_tokens'] }], limits: { budget: { limit_usd: '20.000000000000', period: { kind: 'calendar_month' } }, rpm: null, tpm: 50000, count_rpm: null } });
});

test('policy_form_rejects_a_malformed_budget_without_sending', async () => {
  history.replaceState(null, '', '/dashboard/admin/principals/detail?principal_id=service1');
  let writes = 0;
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url === '/api/me') return Promise.resolve(response(me));
    if (init?.method) writes++;
    if (url === '/api/admin/principals/service1') return Promise.resolve(response({ principal, execution_ceiling: { grants: [], limits: principal.policy.limits } }));
    return Promise.resolve(response({ items: [], next_cursor: null, as_of: 'now' }));
  }));
  render(<App />);
  expect((await screen.findByRole('group', { name: '사용할 수 있는 모델·API' })).textContent).toContain('고를 수 있는 모델이 없습니다');
  fireEvent.change(screen.getByLabelText('월 예산 (USD)'), { target: { value: '1.5.0' } });
  fireEvent.change(screen.getByLabelText('정책 변경 사유'), { target: { value: 'x' } });
  fireEvent.submit(screen.getByRole('form', { name: '정책 편집' }));
  await screen.findByText(/월 예산은 USD 금액으로 입력하세요/);
  expect(screen.queryByRole('button', { name: '명령 실행' })).toBeNull();
  expect(writes).toBe(0);
});

test('losing_admin_role_or_session_says_the_screen_was_cleared_and_offers_sso_sign_in_again', async () => {
  history.replaceState(null, '', '/dashboard/admin/principals');
  let current: unknown = me, meStatus = 200;
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL) => {
    const url = String(input);
    if (url === '/api/me') return Promise.resolve(response(current, meStatus));
    return Promise.resolve(response({ items: [], next_cursor: null, as_of: 'now' }));
  }));
  render(<App />);
  await screen.findByRole('form', { name: '사용자 조회' });
  // 관리자 권한을 잃으면 화면을 지웠다고 알리고 다시 로그인하게 한다.
  current = { ...me, role: 'user' };
  await session.validate();
  expect((await screen.findByRole('alert')).textContent).toBe('현재 관리자 권한이 없습니다. 입력 중이던 내용과 원문은 지웠습니다.');
  expect(screen.getByRole('link', { name: 'SSO 다시 로그인' }).getAttribute('href')).toBe('/auth/login');
  // 세션을 확인할 수 없게 돼도 같은 안내를 붙인다.
  meStatus = 403; current = { error: { code: 'access_denied' } };
  await session.validate();
  expect((await screen.findByRole('alert')).textContent).toBe('접근 권한이 없습니다. 입력 중이던 내용과 원문은 지웠습니다.');
  expect(screen.getByRole('link', { name: 'SSO 다시 로그인' })).toBeTruthy();
});

test('first_sign_in_failure_does_not_claim_anything_was_cleared', async () => {
  vi.stubGlobal('fetch', vi.fn(() => Promise.resolve(response({ error: { code: 'access_denied' } }, 403))));
  render(<App />);
  expect((await screen.findByRole('alert')).textContent).toBe('접근 권한이 없습니다.');
  expect(screen.getByRole('link', { name: 'SSO 로그인' })).toBeTruthy();
});
