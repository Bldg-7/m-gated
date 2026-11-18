import { cleanup, fireEvent, render, screen, within } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../../app';
import { session } from '../../lib/api';

const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
const budgetTimezone = { timezone: 'UTC', period_start: '2026-11-01T00:00:00Z', period_end: '2026-12-01T00:00:00Z', pending: null, version: '3' };
const admin = { principal: { id: 'admin1', display_name: '관리자', email: 'admin@example.test', block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' } }, role: 'admin', grants: [], budget_timezone: budgetTimezone, provider_credentials: [], sso: { refresh_url: 'https://auth.example.test/sso/refresh' } };
const meta = (id: string, label: string, hint: string, isDefault: boolean) => ({ id, label, hint, default: isDefault, version: '1', decryptable: true, bound_keys: '0', created_at: 'now', created_by: 'admin1', created_by_email: null, verified_at: 'now' });
const companyKeys = { providers: [{ provider: 'anthropic', status: 'active', credentials: [meta('a', 'team-a', '0001', true), meta('b', 'team-b', '0002', false)], history: [] }] };
const human = { id: 'human1', kind: 'human', display_name: '사람', email: 'bob@example.test', management_owner_id: null, version: '2', policy: { id: 'p1', version: '4', disabled: false, grants: [], limits: { budget: null, rpm: null, tpm: null, count_rpm: null } }, inference_safety: { paused: false, version: '1', reason: null, paused_at: null, trigger_request_id: null }, block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' }, provider_credentials: [{ provider: 'anthropic', credentials: [{ id: 'b', label: 'team-b', hint: '0002', default: true }] }] };
const key = { provider_credentials: [{ provider: 'anthropic', credential_id: 'b', label: 'team-b', hint: '0002', state: 'active', authorized: false }], id: 'k1', principal_id: 'human1', name: '막힌 키', key_hint: 'mg_k1', created_at: 'now', created_by: 'human1', created_command_id: 'c1', validity: '1_year', expires_at: null, revoked_at: null, version: '1', grants: [], limits: { budget: null, rpm: null, tpm: null, count_rpm: null } };
afterEach(() => { cleanup(); session.clear(); history.replaceState(null, '', '/dashboard/'); vi.unstubAllGlobals(); });

test('removing_company_key_access_asks_what_to_do_with_bound_keys_and_blocked_keys_are_marked', async () => {
  history.replaceState(null, '', '/dashboard/admin/principals/detail?principal_id=human1');
  const writes: Record<string, unknown>[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url === '/api/me') return Promise.resolve(response(admin));
    if (url === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (init?.method === 'PUT') {
      const body = JSON.parse(String(init.body)); writes.push(body);
      return Promise.resolve(body.bound_keys
        ? response({ command_id: 'cmd', operation: 'authorize_principal_credentials', target: { kind: 'principal', id: 'human1' }, result_version: '2', effect: 'changed', authorization: { moved_keys: '0', blocked_keys: '1' } })
        : response({ error: { code: 'credential_authorization_in_use', details: { bound_keys: '1' } } }, 409));
    }
    if (url === '/api/admin/provider-credentials') return Promise.resolve(response(companyKeys));
    if (url === '/api/admin/principals/human1') return Promise.resolve(response({ principal: human, execution_ceiling: { grants: [], limits: human.policy.limits } }));
    if (url.startsWith('/api/admin/keys')) return Promise.resolve(response({ items: [key], next_cursor: null, as_of: 'now' }));
    return Promise.resolve(response({ items: [], next_cursor: null, as_of: 'now' }));
  }));
  render(<App />);
  // 인가가 해제된 회사 키에 연결된 가상 키는 키 목록에서 바로 알 수 있다.
  await screen.findByText('인가 해제됨');
  expect(screen.getByText(/이 키로 보내는 Anthropic 요청은 거절됩니다/)).toBeTruthy();
  const form = await screen.findByRole('form', { name: 'Anthropic 회사 키 인가' });
  expect(within(form).getByText(/지금: 고른 키 1개만 쓸 수 있습니다/)).toBeTruthy();
  fireEvent.click(within(form).getByLabelText(/team-b/));
  fireEvent.click(within(form).getByLabelText(/team-a/));
  fireEvent.click(within(form).getByRole('button', { name: '인가 저장' }));
  // 연결된 가상 키가 있으면 서버가 거절하고, 화면이 옮길지 막을지 묻는다.
  await screen.findByText(/가상 키 1개가 연결돼 있습니다/);
  fireEvent.click(screen.getByRole('button', { name: '옮기지 않고 막기' }));
  await screen.findByText(/가상 키 1개는 막혔습니다/);
  expect(writes).toEqual([
    { provider: 'anthropic', credential_ids: ['a'], default_credential_id: 'a', expected: { credential_ids: ['b'], default_credential_id: 'b' } },
    { provider: 'anthropic', credential_ids: ['a'], default_credential_id: 'a', expected: { credential_ids: ['b'], default_credential_id: 'b' }, bound_keys: 'block' },
  ]);
});

test('budget_time_zone_is_chosen_from_a_list_on_the_settings_screen', async () => {
  history.replaceState(null, '', '/dashboard/admin/settings');
  const writes: Record<string, unknown>[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url === '/api/me') return Promise.resolve(response(admin));
    if (url === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (init?.method === 'PUT') { writes.push(JSON.parse(String(init.body))); return Promise.resolve(response({ command_id: 'cmd', effect: 'changed', result_version: '4' })); }
    if (url === '/api/admin/settings') return Promise.resolve(response({ budget_timezone: writes.length ? { ...budgetTimezone, version: '4', pending: { timezone: 'Asia/Seoul', effective_from: '2026-12-01T00:00:00Z', first_period_end: '2026-12-31T15:00:00Z' } } : budgetTimezone }));
    return Promise.resolve(response({ items: [], next_cursor: null, as_of: 'now' }));
  }));
  render(<App />);
  await screen.findByRole('heading', { name: '게이트웨이 설정' });
  expect(screen.getByRole('link', { name: '설정' }).getAttribute('aria-current')).toBe('page');
  const select = await screen.findByLabelText('시간대');
  expect((select as HTMLSelectElement).value).toBe('UTC');
  expect(within(select).getByRole('option', { name: 'Asia/Seoul' })).toBeTruthy();
  fireEvent.change(select, { target: { value: 'Asia/Seoul' } });
  fireEvent.click(screen.getByRole('button', { name: '시간대 저장' }));
  await screen.findByText(/2026-12-01T00:00:00Z부터 Asia\/Seoul/);
  expect(writes).toEqual([{ timezone: 'Asia/Seoul', expected_version: '3' }]);
});

test('key_form_shows_the_authorized_company_key_and_the_budget_time_zone_instead_of_an_input', async () => {
  history.replaceState(null, '', '/dashboard/keys');
  const user = { ...admin, role: 'user', budget_timezone: { ...budgetTimezone, timezone: 'Asia/Seoul' }, provider_credentials: [{ provider: 'anthropic', credentials: [{ id: 'b', label: 'team-b', hint: '0002', default: true }] }] };
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL) => Promise.resolve(response(String(input) === '/api/me' ? user : { items: [], next_cursor: null, as_of: 'now' }))));
  render(<App />);
  const form = await screen.findByRole('form', { name: '개인 키 발급' });
  // 인가된 회사 키가 하나뿐이어도 어느 키에 연결되는지 보여 준다(고를 수는 없다).
  const company = within(form).getByLabelText('Anthropic 회사 키') as HTMLSelectElement;
  expect(company.disabled).toBe(true);
  expect(company.selectedOptions[0].textContent).toContain('team-b (…0002)');
  expect(within(form).queryByLabelText(/timezone/)).toBeNull();
  expect(within(form).getByText(/월 예산 기준 시간대: Asia\/Seoul \(관리자 설정\)/)).toBeTruthy();
});
