import { cleanup, fireEvent, render, screen, within } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../../app';
import { optionNames, pickOption, selectedValue } from '../../test-select';
import { session } from '../../lib/api';

const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
const budgetTimezone = { timezone: 'UTC', period_start: '2026-11-01T00:00:00Z', period_end: '2026-12-01T00:00:00Z', pending: null, version: '3' };
const admin = { principal: { id: 'admin1', display_name: '관리자', email: 'admin@example.test', block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' } }, role: 'admin', grants: [], budget_timezone: budgetTimezone, provider_credentials: [], sso: { refresh_url: 'https://auth.example.test/sso/refresh' } };
const meta = (id: string, label: string, hint: string) => ({ id, label, hint, version: '1', decryptable: true, bound_keys: '0', created_at: 'now', created_by: 'admin1', created_by_email: null, verified_at: 'now' });
const providerKeys = { providers: [{ provider: 'anthropic', status: 'active', credentials: [meta('a', 'team-a', '0001'), meta('b', 'team-b', '0002'), meta('c', 'team-c', '0003')], history: [] }] };
const human = { id: 'human1', kind: 'human', display_name: '사람', email: 'bob@example.test', management_owner_id: null, version: '2', policy: { id: 'p1', version: '4', disabled: false, grants: [], limits: { budget: null, rpm: null, tpm: null, count_rpm: null } }, inference_safety: { paused: false, version: '1', reason: null, paused_at: null, trigger_request_id: null }, block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' }, provider_credentials: [{ provider: 'anthropic', credentials: [{ id: 'b', label: 'team-b', hint: '0002' }] }] };
const key = { provider_credentials: [{ provider: 'anthropic', credential_id: 'b', label: 'team-b', hint: '0002', state: 'active', authorized: false }], id: 'k1', principal_id: 'human1', name: '막힌 키', key_hint: 'mg_k1', created_at: 'now', created_by: 'human1', created_command_id: 'c1', validity: '1_year', expires_at: null, revoked_at: null, version: '1', grants: [], limits: { budget: null, rpm: null, tpm: null, count_rpm: null } };
const ack = (extra: Record<string, unknown> = {}) => response({ command_id: 'cmd', operation: 'authorize_principal_credentials', target: { kind: 'principal', id: 'human1' }, result_version: '2', effect: 'changed', ...extra });
function stubDetail(principal: typeof human, keys: unknown[], write: (body: Record<string, unknown>, url: string) => Response) {
  const writes: Record<string, unknown>[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url === '/api/me') return Promise.resolve(response(admin));
    if (url === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (init?.method === 'PUT') { const body = JSON.parse(String(init.body)); writes.push({ url, ...body }); return Promise.resolve(write(body, url)); }
    if (url === '/api/admin/provider-credentials') return Promise.resolve(response(providerKeys));
    if (url === '/api/admin/principals/human1') return Promise.resolve(response({ principal, execution_ceiling: { grants: [], limits: human.policy.limits } }));
    if (url.startsWith('/api/admin/keys')) return Promise.resolve(response({ items: keys, next_cursor: null, as_of: 'now' }));
    return Promise.resolve(response({ items: [], next_cursor: null, as_of: 'now' }));
  }));
  return writes;
}
afterEach(() => { cleanup(); session.clear(); history.replaceState(null, '', '/dashboard/'); vi.unstubAllGlobals(); });

test('removing_provider_key_access_asks_what_to_do_with_bound_keys_and_blocked_keys_are_marked', async () => {
  history.replaceState(null, '', '/dashboard/admin/principals/detail?principal_id=human1');
  const writes = stubDetail(human, [key], body => body.bound_keys ? ack({ authorization: { moved_keys: '0', blocked_keys: '1' } }) : response({ error: { code: 'credential_authorization_in_use', details: { bound_keys: '1' } } }, 409));
  render(<App />);
  // 인가가 해제된 Provider 키에 연결된 가상 키는 키 목록에서 바로 알 수 있다.
  await screen.findByText('인가 해제됨');
  expect(screen.getByText(/이 키로 보내는 Anthropic 요청은 거절됩니다/)).toBeTruthy();
  const form = await screen.findByRole('form', { name: 'Anthropic Provider 키 인가' });
  expect(within(form).getByText('지금: 인가된 키 1개를 쓸 수 있습니다.')).toBeTruthy();
  expect(form.textContent).not.toContain('기본 키');
  expect(within(form).queryByRole('combobox')).toBeNull();
  expect((within(form).getByRole('button', { name: '인가 저장' }) as HTMLButtonElement).disabled).toBe(true);
  fireEvent.click(within(form).getByLabelText(/team-b/));
  fireEvent.click(within(form).getByLabelText(/team-a/));
  fireEvent.click(within(form).getByRole('button', { name: '인가 저장' }));
  // 연결된 가상 키가 있으면 서버가 거절하고, 확인 창이 옮길지 막을지 묻는다. 남는 인가 키가 하나면 미리 골라 둔다.
  const dialog = await screen.findByRole('dialog', { name: '연결된 가상 키 처리' });
  expect(dialog.textContent).toContain('가상 키 1개가 연결돼 있습니다');
  expect(selectedValue(within(dialog).getByLabelText('옮길 Provider 키'))).toBe('a');
  fireEvent.click(within(dialog).getByRole('button', { name: '옮기지 않고 막기' }));
  await screen.findByText(/가상 키 1개는 막혔습니다/);
  expect(screen.queryByRole('dialog')).toBeNull();
  expect(writes).toEqual([
    { url: '/api/admin/principals/human1/provider-credentials', provider: 'anthropic', credential_ids: ['a'], expected: { credential_ids: ['b'] } },
    { url: '/api/admin/principals/human1/provider-credentials', provider: 'anthropic', credential_ids: ['a'], expected: { credential_ids: ['b'] }, bound_keys: 'block' },
  ]);
});

test('removing_every_provider_key_says_the_user_can_no_longer_use_the_provider', async () => {
  history.replaceState(null, '', '/dashboard/admin/principals/detail?principal_id=human1');
  const writes = stubDetail(human, [], () => ack());
  render(<App />);
  const form = await screen.findByRole('form', { name: 'Anthropic Provider 키 인가' });
  fireEvent.click(within(form).getByLabelText(/team-b/));
  expect(within(form).getByText('모두 해제하면 이 사용자는 Anthropic를 쓸 수 없습니다.')).toBeTruthy();
  fireEvent.click(within(form).getByRole('button', { name: '인가 모두 해제' }));
  await screen.findByText('Anthropic Provider 키 인가를 모두 해제했습니다. 이 사용자는 Anthropic를 쓸 수 없습니다.');
  expect(writes).toEqual([{ url: '/api/admin/principals/human1/provider-credentials', provider: 'anthropic', credential_ids: [], expected: { credential_ids: ['b'] } }]);
});

test('admin_rebinds_a_virtual_key_only_to_keys_authorized_for_its_owner', async () => {
  history.replaceState(null, '', '/dashboard/admin/principals/detail?principal_id=human1');
  const owner = { ...human, provider_credentials: [{ provider: 'anthropic', credentials: [{ id: 'a', label: 'team-a', hint: '0001' }, { id: 'b', label: 'team-b', hint: '0002' }] }] };
  const bound = { ...key, name: '연결 키', provider_credentials: [{ ...key.provider_credentials[0], authorized: true }] };
  const writes = stubDetail(owner, [bound], () => ack());
  render(<App />);
  const binding = await screen.findByLabelText('연결 키 Provider 키 연결');
  expect(binding.textContent).not.toContain('인가 해제됨');
  fireEvent.click(within(binding).getByRole('button', { name: 'Provider 키 변경' }));
  const form = within(binding).getByRole('form', { name: '연결 키 Provider 키 변경' });
  const to = within(form).getByLabelText('바꿀 Provider 키');
  expect(selectedValue(to)).toBe('b');
  // 키 주인에게 인가되지 않은 team-c는 후보가 아니다.
  expect(optionNames(to)).toEqual(['team-a (…0001)', 'team-b (…0002)']);
  pickOption(to, 'team-a (…0001)');
  fireEvent.submit(form);
  await screen.findByText('연결 키 키의 Provider 키를 team-a (…0001)로 바꿨습니다.');
  expect(writes).toEqual([{ url: '/api/admin/keys/k1/provider-credential', provider: 'anthropic', credential_id: 'a', expected_credential_id: 'b' }]);
});

test('budget_time_zone_is_searched_and_chosen_on_the_settings_screen', async () => {
  history.replaceState(null, '', '/dashboard/admin/settings');
  const writes: Record<string, unknown>[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url === '/api/me') return Promise.resolve(response(admin));
    if (url === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (init?.method === 'PUT') { writes.push(JSON.parse(String(init.body))); return Promise.resolve(response({ command_id: 'cmd', effect: 'changed', result_version: '4' })); }
    if (url === '/api/admin/settings') return Promise.resolve(response({ budget_timezone: writes.length ? { ...budgetTimezone, version: '4', pending: { timezone: 'Asia/Seoul', effective_from: '2026-12-01T00:00:00.000000Z', first_period_end: '2026-12-31T15:00:00Z' } } : budgetTimezone }));
    return Promise.resolve(response({ items: [], next_cursor: null, as_of: 'now' }));
  }));
  render(<App />);
  await screen.findByRole('heading', { name: '게이트웨이 설정' });
  expect(screen.getByRole('link', { name: '설정' }).getAttribute('aria-current')).toBe('page');
  const select = await screen.findByLabelText('시간대') as HTMLInputElement;
  expect(select.getAttribute('role')).toBe('combobox');
  expect(selectedValue(select)).toBe('UTC');
  expect(select.value).toBe('UTC');
  expect(screen.getByText('2026-11-01 00:00')).toBeTruthy();
  // 글자를 치면 이름에 그 글자가 들어 있는 시간대만 남는다(대소문자 무시).
  fireEvent.click(select);
  fireEvent.change(select, { target: { value: 'seoul' } });
  expect(optionNames(select)).toEqual(['Asia/Seoul']);
  fireEvent.change(select, { target: { value: 'no-such-zone' } });
  expect(within(document.getElementById(select.getAttribute('aria-controls')!)!.parentElement!).getByText('결과 없음')).toBeTruthy();
  fireEvent.change(select, { target: { value: 'asia/seo' } });
  pickOption(select, 'Asia/Seoul');
  expect(selectedValue(select)).toBe('Asia/Seoul');
  fireEvent.click(screen.getByRole('button', { name: '시간대 저장' }));
  await screen.findByText(/2026-12-01 00:00부터 Asia\/Seoul. 바뀐 직후의 기간은 2026-12-01 00:00부터 2026-12-31 15:00까지입니다./);
  expect(writes).toEqual([{ timezone: 'Asia/Seoul', expected_version: '3' }]);
});

test('key_form_requires_an_authorized_provider_key_for_each_chosen_provider', async () => {
  history.replaceState(null, '', '/dashboard/keys');
  const grants = [{ provider: 'anthropic', model: 'synthetic-text', capabilities: ['messages'] }, { provider: 'openai', model: 'synthetic-gpt', capabilities: ['responses'] }];
  const user = { ...admin, role: 'user', grants, budget_timezone: { ...budgetTimezone, timezone: 'Asia/Seoul' }, provider_credentials: [{ provider: 'anthropic', credentials: [{ id: 'b', label: 'team-b', hint: '0002' }] }] };
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL) => Promise.resolve(response(String(input) === '/api/me' ? user : { items: [], next_cursor: null, as_of: 'now' }))));
  render(<App />);
  const form = await screen.findByRole('form', { name: '가상 키 발급' });
  // API를 고르기 전에는 Provider 키 선택칸이 없다.
  expect(within(form).queryByLabelText('Anthropic Provider 키')).toBeNull();
  fireEvent.click(within(form).getByLabelText('anthropic / synthetic-text / messages'));
  // 인가된 키가 하나뿐이면 그 키를 미리 골라 둔다.
  const provider = within(form).getByLabelText('Anthropic Provider 키') as HTMLInputElement;
  expect(selectedValue(provider)).toBe('b');
  expect(provider.value).toBe('team-b (…0002)');
  expect((within(form).getByRole('button', { name: '발급 내용 확인' }) as HTMLButtonElement).disabled).toBe(false);
  // 인가된 키가 없는 Provider의 API를 고르면 이유를 보여 주고 발급을 막는다.
  fireEvent.click(within(form).getByLabelText('openai / synthetic-gpt / responses'));
  expect(within(form).getByText('나에게 인가된 OpenAI Provider 키가 없어 OpenAI API를 쓰는 키를 발급할 수 없습니다. 관리자에게 인가를 요청하세요.')).toBeTruthy();
  expect((within(form).getByRole('button', { name: '발급 내용 확인' }) as HTMLButtonElement).disabled).toBe(true);
  expect(within(form).queryByLabelText(/timezone/)).toBeNull();
  expect(within(form).getByText(/월 예산 기준 시간대: Asia\/Seoul \(관리자 설정\). 이번 기간: 2026-11-01 00:00 ~ 2026-12-01 00:00/)).toBeTruthy();
});
