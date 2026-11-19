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
    if (init?.method === 'PUT' || init?.method === 'PATCH') { const body = JSON.parse(String(init.body)); writes.push({ url, ...body }); return Promise.resolve(write(body, url)); }
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

const editGrants = [{ provider: 'anthropic', model: 'synthetic-text', capabilities: ['messages'] }];
/** 정책이 Anthropic API를 허용하고 team-a·team-b가 인가된 키 주인. team-c는 인가되지 않았다. */
const editOwner = { ...human, policy: { ...human.policy, grants: editGrants as never[] }, provider_credentials: [{ provider: 'anthropic', credentials: [{ id: 'a', label: 'team-a', hint: '0001' }, { id: 'b', label: 'team-b', hint: '0002' }] }] };

test('admin_edits_a_virtual_key_only_to_keys_authorized_for_its_owner', async () => {
  history.replaceState(null, '', '/dashboard/admin/principals/detail?principal_id=human1');
  const bound = { ...key, name: '연결 키', grants: editGrants, provider_credentials: [{ ...key.provider_credentials[0], authorized: true }] };
  const writes = stubDetail(editOwner, [bound], () => ack());
  render(<App />);
  const binding = await screen.findByLabelText('연결 키 Provider 키 연결');
  expect(binding.textContent).not.toContain('인가 해제됨');
  // 연결마다 따로 두던 「Provider 키 변경」은 없고, 수정 창에서 바꾼다.
  expect(screen.queryByRole('button', { name: 'Provider 키 변경' })).toBeNull();
  fireEvent.click(screen.getByRole('button', { name: '연결 키 수정' }));
  const dialog = await screen.findByRole('dialog', { name: '연결 키 수정' });
  const to = within(dialog).getByLabelText('Anthropic Provider 키');
  expect(selectedValue(to)).toBe('b');
  // 키 주인에게 인가되지 않은 team-c는 후보가 아니다.
  expect(optionNames(to)).toEqual(['연동 안 함', 'team-a (…0001)', 'team-b (…0002)']);
  pickOption(to, 'team-a (…0001)');
  fireEvent.click(within(dialog).getByRole('button', { name: '수정 저장' }));
  expect((await screen.findByRole('status')).textContent).toBe('연결 키 키를 수정했습니다.');
  expect(screen.queryByRole('dialog')).toBeNull();
  expect(writes).toEqual([{ url: '/api/keys/k1', expected_version: '1', name: '연결 키', grants: editGrants, provider_credentials: { anthropic: 'a' } }]);
});

test('admin_fixes_a_blocked_binding_by_choosing_a_key_authorized_for_the_owner', async () => {
  history.replaceState(null, '', '/dashboard/admin/principals/detail?principal_id=human1');
  // 인가가 풀린 team-c에 연결돼 막힌 가상 키.
  const blocked = { ...key, grants: editGrants, provider_credentials: [{ provider: 'anthropic', credential_id: 'c', label: 'team-c', hint: '0003', state: 'active', authorized: false }] };
  const writes = stubDetail(editOwner, [blocked], () => ack());
  render(<App />);
  const binding = await screen.findByLabelText('막힌 키 Provider 키 연결');
  expect(binding.textContent).toContain('인가 해제됨');
  expect(binding.textContent).toContain('수정에서 인가된 Provider 키로 다시 연결하세요.');
  expect(within(binding).queryByRole('button')).toBeNull();
  fireEvent.click(screen.getByRole('button', { name: '막힌 키 수정' }));
  const dialog = await screen.findByRole('dialog', { name: '막힌 키 수정' });
  const to = within(dialog).getByLabelText('Anthropic Provider 키'), save = within(dialog).getByRole('button', { name: '수정 저장' }) as HTMLButtonElement;
  // 인가가 풀린 연결은 처음 값이 아니다. 인가된 키를 다시 골라야 저장할 수 있다.
  expect(selectedValue(to)).toBe('');
  expect(optionNames(to)).toEqual(['연동 안 함', 'team-a (…0001)', 'team-b (…0002)']);
  expect(save.disabled).toBe(true);
  pickOption(to, 'team-b (…0002)');
  expect((within(dialog).getByLabelText('anthropic / synthetic-text / messages') as HTMLInputElement).checked).toBe(true);
  expect(save.disabled).toBe(false);
  fireEvent.click(save);
  expect((await screen.findByRole('status')).textContent).toBe('막힌 키 키를 수정했습니다.');
  expect(writes).toEqual([{ url: '/api/keys/k1', expected_version: '1', name: '막힌 키', grants: editGrants, provider_credentials: { anthropic: 'b' } }]);
});

test('budget_time_zone_is_searched_and_chosen_on_the_settings_screen', async () => {
  history.replaceState(null, '', '/dashboard/admin/settings');
  const writes: Record<string, unknown>[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url === '/api/me') return Promise.resolve(response(admin));
    if (url === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (init?.method === 'PUT' || init?.method === 'PATCH') { writes.push(JSON.parse(String(init.body))); return Promise.resolve(response({ command_id: 'cmd', effect: 'changed', result_version: '4' })); }
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

test('key_form_offers_only_authorized_provider_keys_and_only_the_apis_of_a_linked_provider', async () => {
  history.replaceState(null, '', '/dashboard/keys/new');
  const grants = [{ provider: 'anthropic', model: 'synthetic-text', capabilities: ['messages'] }, { provider: 'openai', model: 'synthetic-gpt', capabilities: ['responses'] }];
  const user = { ...admin, role: 'user', grants, budget_timezone: { ...budgetTimezone, timezone: 'Asia/Seoul' }, provider_credentials: [{ provider: 'anthropic', credentials: [{ id: 'b', label: 'team-b', hint: '0002' }] }] };
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL) => Promise.resolve(response(String(input) === '/api/me' ? user : { items: [], next_cursor: null, as_of: 'now' }))));
  render(<App />);
  const form = await screen.findByRole('form', { name: '가상 키 발급' });
  const submit = within(form).getByRole('button', { name: '발급 내용 확인' }) as HTMLButtonElement;
  // 인가된 키가 하나뿐이어도 미리 고르지 않는다. Provider 키를 고르기 전에는 고를 API가 없다.
  const provider = within(form).getByLabelText('Anthropic Provider 키') as HTMLInputElement;
  expect(selectedValue(provider)).toBe('');
  expect(optionNames(provider)).toEqual(['연동 안 함', 'team-b (…0002)']);
  expect(within(form).queryAllByRole('checkbox')).toHaveLength(0);
  expect(submit.disabled).toBe(true);
  // 인가된 키가 없는 Provider는 이유만 보여 주고 선택칸을 두지 않는다.
  expect(within(form).queryByLabelText('OpenAI Provider 키')).toBeNull();
  expect(within(form).getByText('나에게 인가된 OpenAI Provider 키가 없습니다.')).toBeTruthy();
  pickOption(provider, 'team-b (…0002)');
  expect(selectedValue(provider)).toBe('b');
  expect(provider.value).toBe('team-b (…0002)');
  // 연동한 Provider의 API만 고를 수 있다. 인가된 키가 없는 Provider의 API는 고를 수 없다.
  expect(within(form).queryByLabelText('openai / synthetic-gpt / responses')).toBeNull();
  expect(submit.disabled).toBe(true);
  fireEvent.click(within(form).getByLabelText('anthropic / synthetic-text / messages'));
  expect(submit.disabled).toBe(false);
  expect(within(form).queryByLabelText(/timezone/)).toBeNull();
  expect(within(form).getByText(/월 예산 기준 시간대: Asia\/Seoul \(관리자 설정\). 이번 기간: 2026-11-01 00:00 ~ 2026-12-01 00:00/)).toBeTruthy();
});

test('service_account_key_is_issued_on_its_own_page_from_the_keys_authorized_for_the_account', async () => {
  const grants = [{ provider: 'anthropic', model: 'synthetic-text', capabilities: ['messages'] }];
  const service = { ...human, kind: 'service', display_name: '배치', email: null, policy: { ...human.policy, grants } };
  history.replaceState(null, '', '/dashboard/admin/principals/detail?principal_id=human1');
  const posts: Record<string, unknown>[] = [], reads: string[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (!init?.method) reads.push(url);
    if (url === '/api/me') return Promise.resolve(response(admin));
    if (url === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (init?.method === 'POST' && url === '/api/keys') { posts.push(JSON.parse(String(init.body))); return Promise.resolve(response({ key: 'service-secret', metadata: { ...key, id: 'k2' } }, 201)); }
    if (url === '/api/admin/provider-credentials') return Promise.resolve(response(providerKeys));
    if (url === '/api/admin/principals/human1') return Promise.resolve(response({ principal: service, execution_ceiling: { grants, limits: human.policy.limits } }));
    return Promise.resolve(response({ items: [], next_cursor: null, as_of: 'now' }));
  }));
  render(<App />);
  // 사용자 상세에는 발급 폼이 없고, 발급 페이지로 가는 링크만 있다.
  const link = await screen.findByRole('link', { name: '서비스 계정의 가상 키 발급' });
  expect(link.getAttribute('href')).toBe('/dashboard/admin/principals/key?principal_id=human1');
  expect(screen.queryByRole('form', { name: '서비스 계정의 가상 키 발급' })).toBeNull();
  fireEvent.click(link);
  const form = await screen.findByRole('form', { name: '서비스 계정의 가상 키 발급' });
  expect(location.pathname + location.search).toBe('/dashboard/admin/principals/key?principal_id=human1');
  expect(screen.getByRole('heading', { level: 2, name: '서비스 계정의 가상 키 발급' })).toBeTruthy();
  expect(screen.getByRole('link', { name: '← 사용자 상세' }).getAttribute('href')).toBe('/dashboard/admin/principals/detail?principal_id=human1');
  expect(within(form).getByRole('link', { name: '취소' }).getAttribute('href')).toBe('/dashboard/admin/principals/detail?principal_id=human1');
  expect(screen.queryByRole('form', { name: '키 조회' })).toBeNull();
  // 관리자가 아니라 서비스 계정에 인가된 Provider 키 안에서만 고른다.
  const provider = within(form).getByLabelText('Anthropic Provider 키');
  expect(optionNames(provider)).toEqual(['연동 안 함', 'team-b (…0002)']);
  fireEvent.change(within(form).getByLabelText('키 이름'), { target: { value: '배치 키' } });
  pickOption(within(form).getByLabelText('유효기간'), '1년');
  pickOption(provider, 'team-b (…0002)');
  fireEvent.click(within(form).getByLabelText('anthropic / synthetic-text / messages'));
  fireEvent.submit(form);
  fireEvent.click(within(await screen.findByRole('dialog', { name: '발급 확인' })).getByRole('button', { name: '발급 확인' }));
  const secret = await screen.findByRole('dialog', { name: '발급된 키' });
  expect(posts).toEqual([expect.objectContaining({ service_principal_id: 'human1', name: '배치 키', validity: '1_year', grants, provider_credentials: { anthropic: 'b' } })]);
  expect(reads.some(url => url === '/api/admin/keys?principal_id=human1')).toBe(true);
  // 키 창을 닫으면 사용자 상세로 돌아간다.
  fireEvent.click(within(secret).getByRole('button', { name: '닫기' }));
  await screen.findByRole('link', { name: '서비스 계정의 가상 키 발급' });
  expect(location.pathname + location.search).toBe('/dashboard/admin/principals/detail?principal_id=human1');
  expect(screen.queryByText('service-secret')).toBeNull();
});

test('service_account_without_an_authorized_provider_key_and_human_users_get_no_issue_form', async () => {
  const grants = [{ provider: 'anthropic', model: 'synthetic-text', capabilities: ['messages'] }];
  history.replaceState(null, '', '/dashboard/admin/principals/key?principal_id=human1');
  stubDetail({ ...human, kind: 'service', policy: { ...human.policy, grants: grants as never[] }, provider_credentials: [] }, [], () => ack());
  render(<App />);
  await screen.findByText('이 서비스 계정에 인가된 Provider 키가 없어 가상 키를 발급할 수 없습니다. 사용자 상세의 Provider 키 인가에서 먼저 인가하세요.');
  expect(screen.queryByRole('form', { name: '서비스 계정의 가상 키 발급' })).toBeNull();
  cleanup(); session.clear();
  // 사람 사용자의 가상 키는 관리자가 대신 발급하지 않는다.
  stubDetail(human, [], () => ack());
  render(<App />);
  expect((await screen.findByRole('alert')).textContent).toBe('사람 사용자의 가상 키는 본인이 내 가상 키에서 발급합니다.');
  expect(screen.queryByRole('form', { name: '서비스 계정의 가상 키 발급' })).toBeNull();
  expect(screen.getByRole('link', { name: '← 사용자 상세' }).getAttribute('href')).toBe('/dashboard/admin/principals/detail?principal_id=human1');
});
