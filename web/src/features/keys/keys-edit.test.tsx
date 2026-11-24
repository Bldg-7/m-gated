import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../../app';
import { optionNames, pickOption, selectedValue } from '../../test-select';
import { session } from '../../lib/api';

const policy = [{ provider: 'anthropic', model: 'synthetic-text', capabilities: ['messages', 'count_tokens'] }, { provider: 'openai', model: 'synthetic-gpt', capabilities: ['responses'] }];
const authorized = [{ provider: 'anthropic', credentials: [{ id: 'cred1', label: 'team-a', hint: '0001' }, { id: 'cred2', label: 'team-b', hint: '0002' }] }, { provider: 'openai', credentials: [{ id: 'cred3', label: 'oa', hint: '0003' }] }];
const me = { principal: { id: 'human1', display_name: 'Synthetic', email: 'a@example.test' }, role: 'user', grants: policy, provider_credentials: authorized, sso: { refresh_url: 'https://auth.example.test/sso/refresh' } };
const adminMe = { ...me, principal: { id: 'admin1', display_name: '관리자', email: 'admin@example.test' }, role: 'admin', grants: [], provider_credentials: [] };
const id = '00000000-0000-4000-8000-000000000001';
const bound = (credential_id: string, label: string, hint: string, isAuthorized = true) => ({ provider: 'anthropic', credential_id, label, hint, state: 'active', authorized: isAuthorized });
const key = { id, principal_id: 'human1', owner: { kind: 'human', display_name: '사람', email: 'human@example.test' }, name: '현재 키', key_hint: 'mgw_v1_x', version: '7', created_at: '2026-01-01T00:00:00Z', created_by: 'human1', created_command_id: 'original', validity: '1_year', expires_at: '2027-01-01T00:00:00Z', revoked_at: null, grants: [{ provider: 'anthropic', model: 'synthetic-text', capabilities: ['messages'] }], limits: { budget: null, rpm: 60, tpm: null, count_rpm: null }, provider_credentials: [bound('cred1', 'team-a', '0001')] };
const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
const ack = () => response({ command_id: 'cmd', operation: 'update_key', target: { kind: 'key', id }, result_version: '8', effect: 'changed', replayed: false });
function deferred<T>() { let resolve!: (value: T) => void; const promise = new Promise<T>(r => { resolve = r; }); return { promise, resolve }; }
type Call = { url: string; method: string; body?: Record<string, unknown>; commandId: string | null };
afterEach(() => { cleanup(); session.clear(); history.replaceState(null, '', '/dashboard/'); vi.unstubAllGlobals(); });

/** path의 키 목록 화면을 띄운다. 쓰기는 write가, 목록·내 정보 밖의 읽기는 read가 답한다. */
async function mount(options: { path?: string; user?: unknown; items?: () => unknown[]; write?: (call: Call) => Response; read?: (url: string) => Promise<Response> | undefined } = {}) {
  const { path = '/dashboard/keys', user = me, items = () => [key], write = () => ack(), read } = options;
  history.replaceState(null, '', path);
  const calls: Call[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const call: Call = { url: String(input), method: init?.method ?? 'GET', body: init?.body ? JSON.parse(String(init.body)) : undefined, commandId: new Headers(init?.headers).get('x-gateway-command-id') };
    calls.push(call);
    if (call.url === '/api/me') return Promise.resolve(response(user));
    if (call.url === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (call.method !== 'GET') return Promise.resolve(write(call));
    return read?.(call.url) ?? Promise.resolve(response({ items: items(), next_cursor: null, as_of: '2026-06-01T00:00:00Z' }));
  }));
  render(<App />);
  await waitFor(() => expect((screen.getByRole('button', { name: '조회' }) as HTMLButtonElement).disabled).toBe(false));
  return calls;
}
const lists = (calls: Call[], url = '/api/keys') => calls.filter(c => c.method === 'GET' && c.url === url).length;
const writes = (calls: Call[]) => calls.filter(c => c.method !== 'GET');
async function openEdit(name = '현재 키') {
  fireEvent.click(screen.getByRole('button', { name: `${name} 수정` }));
  return screen.findByRole('dialog', { name: `${name} 수정` });
}
const saveButton = (dialog: HTMLElement) => within(dialog).getByRole('button', { name: '수정 저장' }) as HTMLButtonElement;
const api = (dialog: HTMLElement, name: string) => within(dialog).getByLabelText(name) as HTMLInputElement;

test('owner_renames_a_key_and_changes_apis_with_one_patch_then_the_list_reloads', async () => {
  let saved = false;
  const calls = await mount({ write: () => { saved = true; return ack(); }, items: () => [saved ? { ...key, name: '새 이름', version: '8' } : key] });
  const dialog = await openEdit();
  // 처음 값은 지금 이름·연결·허용 API다.
  const name = within(dialog).getByLabelText('키 이름') as HTMLInputElement;
  expect(name.value).toBe('현재 키');
  expect(selectedValue(within(dialog).getByLabelText('연동할 Provider 키 (1개 이상 필수)'))).toBe('cred1');
  // Provider 키는 선택 칸 하나에서 고른다. 고른 키는 배지로 보이고, 고르지 않은 Provider(OpenAI)는 배지가 없다.
  expect(within(dialog).getAllByRole('button', { name: / 제거$/ }).map(b => b.getAttribute('aria-label'))).toEqual(['Anthropic · team-a (…0001) 제거']);
  expect(api(dialog, 'anthropic / synthetic-text / messages').checked).toBe(true);
  expect(api(dialog, 'anthropic / synthetic-text / count_tokens').checked).toBe(false);
  // 연동하지 않은 Provider의 API는 고를 수 없다.
  expect(within(dialog).queryByLabelText('openai / synthetic-gpt / responses')).toBeNull();
  // 유효기간·만료일·한도는 보여 주기만 한다.
  expect(within(dialog).getByText('유효기간 1년 · 만료 2027-01-01 00:00')).toBeTruthy();
  expect(within(dialog).getByText(/유효기간·만료일·한도는 수정할 수 없습니다\./)).toBeTruthy();
  expect(within(dialog).getAllByRole('textbox')).toEqual([name]);
  const before = lists(calls);
  fireEvent.change(name, { target: { value: '  새 이름 ' } });
  fireEvent.click(api(dialog, 'anthropic / synthetic-text / count_tokens'));
  fireEvent.click(api(dialog, 'anthropic / synthetic-text / messages'));
  expect(writes(calls)).toHaveLength(0);
  fireEvent.click(saveButton(dialog));
  expect((await screen.findByRole('status')).textContent).toBe('새 이름 키를 수정했습니다.');
  expect(screen.queryByRole('dialog')).toBeNull();
  const sent = writes(calls);
  expect(sent).toHaveLength(1);
  expect(sent[0]).toMatchObject({ url: `/api/keys/${id}`, method: 'PATCH' });
  expect(sent[0].body).toEqual({ expected_version: '7', name: '새 이름', grants: [{ provider: 'anthropic', model: 'synthetic-text', capabilities: ['count_tokens'] }], provider_credentials: { anthropic: 'cred1' } });
  expect(sent[0].commandId).toMatch(/^[0-9a-f-]{36}$/);
  // 저장한 뒤 목록을 다시 읽어 서버의 새 상태를 보여 준다.
  await screen.findByRole('button', { name: '새 이름 수정' });
  expect(lists(calls)).toBe(before + 1);
  expect(writes(calls)).toHaveLength(1);
});

test('save_stays_disabled_without_a_name_a_provider_key_or_an_api_for_each_chosen_provider', async () => {
  const calls = await mount();
  const dialog = await openEdit(), save = saveButton(dialog);
  const name = within(dialog).getByLabelText('키 이름'), anthropic = within(dialog).getByLabelText('연동할 Provider 키 (1개 이상 필수)');
  expect(save.disabled).toBe(false);
  // 이름이 비었거나 공백뿐이면 저장할 수 없다.
  fireEvent.change(name, { target: { value: '' } });
  expect(save.disabled).toBe(true);
  fireEvent.change(name, { target: { value: '   ' } });
  expect(save.disabled).toBe(true);
  fireEvent.change(name, { target: { value: '현재 키' } });
  expect(save.disabled).toBe(false);
  // Provider 키를 하나도 고르지 않으면 저장할 수 없고, 고를 API도 없다.
  fireEvent.click(within(dialog).getByRole('button', { name: / 제거$/ }));
  expect(save.disabled).toBe(true);
  expect(within(dialog).queryAllByRole('checkbox')).toHaveLength(0);
  expect(within(dialog).getByText('Provider 키를 먼저 고르면 그 Provider의 API를 고를 수 있습니다.')).toBeTruthy();
  // 연동을 풀면 그 Provider의 API도 빠진다. 다시 연동해도 API를 골라야 저장할 수 있다.
  pickOption(anthropic, 'Anthropic · team-b (…0002)');
  expect(api(dialog, 'anthropic / synthetic-text / messages').checked).toBe(false);
  expect(save.disabled).toBe(true);
  fireEvent.click(api(dialog, 'anthropic / synthetic-text / messages'));
  expect(save.disabled).toBe(false);
  // 고른 Provider마다 API가 하나 이상 있어야 한다.
  pickOption(anthropic, 'OpenAI · oa (…0003)');
  expect(save.disabled).toBe(true);
  fireEvent.click(api(dialog, 'openai / synthetic-gpt / responses'));
  expect(save.disabled).toBe(false);
  fireEvent.click(api(dialog, 'anthropic / synthetic-text / messages'));
  expect(save.disabled).toBe(true);
  // 잠긴 버튼을 눌러도 쓰기는 없다.
  fireEvent.click(save);
  expect(writes(calls)).toHaveLength(0);
});

const rejections: [string, number, string][] = [
  ['version_conflict', 409, '다른 곳에서 이 키를 먼저 바꿨습니다. 창을 닫고 목록을 다시 조회한 뒤 수정하세요.'],
  ['provider_credential_not_authorized', 403, '고른 Provider 키를 쓸 수 있는 인가가 없습니다.'],
  ['provider_credential_required', 400, '가상 키는 Provider 키를 하나 이상 연동해야 하고, 허용한 API의 Provider마다 Provider 키를 골라야 합니다.'],
];
for (const [code, status, reason] of rejections) {
  test(`rejected_edit_${code}_keeps_the_window_open_with_the_reason_and_is_not_resent`, async () => {
    const calls = await mount({ write: () => response({ error: { code } }, status) });
    const dialog = await openEdit(), before = lists(calls);
    fireEvent.change(within(dialog).getByLabelText('키 이름'), { target: { value: '바꾼 이름' } });
    fireEvent.click(saveButton(dialog));
    const alert = await within(dialog).findByRole('alert');
    const sent = writes(calls);
    expect(sent).toHaveLength(1);
    expect(alert.textContent).toBe(`${reason} 명령 ID: ${sent[0].commandId}.`);
    // 창과 입력한 값은 그대로 남고, 다시 누를 수 있다. 쓰기와 목록 조회를 스스로 다시 보내지 않는다.
    await waitFor(() => expect(saveButton(dialog).disabled).toBe(false));
    await act(async () => { await new Promise(resolve => setTimeout(resolve, 20)); });
    expect(screen.getByRole('dialog', { name: '현재 키 수정' })).toBe(dialog);
    expect((within(dialog).getByLabelText('키 이름') as HTMLInputElement).value).toBe('바꾼 이름');
    expect(screen.queryByRole('status')).toBeNull();
    expect(writes(calls)).toHaveLength(1);
    expect(lists(calls)).toBe(before);
  });
}

// 403 access_denied는 Session이 인증을 다시 확인하면서 화면을 새로 그린다. 그래서 수정 창은 닫히고 창 안의 안내는 보이지 않는다.
test('edit_denied_with_403_revalidates_the_session_closes_the_window_and_is_not_resent', async () => {
  const calls = await mount({ write: () => response({ error: { code: 'access_denied' } }, 403) });
  const dialog = await openEdit(), identityReads = lists(calls, '/api/me');
  fireEvent.click(saveButton(dialog));
  await waitFor(() => expect(lists(calls, '/api/me')).toBe(identityReads + 1));
  await screen.findByRole('button', { name: '현재 키 수정' });
  await act(async () => { await new Promise(resolve => setTimeout(resolve, 20)); });
  expect(screen.queryByRole('dialog')).toBeNull();
  expect(screen.queryByRole('status')).toBeNull();
  expect(writes(calls)).toHaveLength(1);
});

test('lost_edit_response_keeps_the_window_open_and_is_not_resent', async () => {
  const calls = await mount({ write: () => { throw new TypeError('synthetic network loss'); } });
  const dialog = await openEdit();
  fireEvent.click(saveButton(dialog));
  const alert = await within(dialog).findByRole('alert');
  expect(alert.textContent).toContain(`명령 ID: ${writes(calls)[0].commandId}.`);
  await act(async () => { await new Promise(resolve => setTimeout(resolve, 20)); });
  expect(writes(calls)).toHaveLength(1);
  expect(screen.getByRole('dialog', { name: '현재 키 수정' })).toBe(dialog);
});

test('admin_key_list_reads_the_key_owner_before_showing_the_edit_form', async () => {
  const owner = deferred<Response>();
  const ownerPolicy = [{ provider: 'anthropic', model: 'synthetic-text', capabilities: ['messages'] }];
  const principal = { id: 'svc1', kind: 'service', display_name: '배치', email: null, management_owner_id: 'human1', version: '2', policy: { id: 'p1', version: '4', disabled: false, grants: ownerPolicy, limits: { budget: null, rpm: null, tpm: null, count_rpm: null } }, provider_credentials: [{ provider: 'anthropic', credentials: [{ id: 'cred2', label: 'team-b', hint: '0002' }, { id: 'cred1', label: 'team-a', hint: '0001' }] }] };
  const calls = await mount({ path: '/dashboard/admin/keys', user: adminMe, items: () => [{ ...key, principal_id: 'svc1', owner: { kind: 'service', display_name: '배치 서비스', email: null } }], read: url => url === '/api/admin/principals/svc1' ? owner.promise : undefined });
  expect(lists(calls, '/api/admin/keys')).toBe(1);
  // 수정 창을 열기 전에는 키 주인을 읽지 않는다.
  expect(lists(calls, '/api/admin/principals/svc1')).toBe(0);
  const dialog = await openEdit();
  expect(within(dialog).getByRole('status').textContent).toBe('키 주인의 정책과 인가 조회 중…');
  expect(within(dialog).queryByRole('button', { name: '수정 저장' })).toBeNull();
  expect(within(dialog).queryByLabelText('키 이름')).toBeNull();
  expect(within(dialog).queryByRole('combobox')).toBeNull();
  expect(lists(calls, '/api/admin/principals/svc1')).toBe(1);
  await act(async () => { owner.resolve(response({ principal, execution_ceiling: { grants: ownerPolicy, limits: principal.policy.limits } })); });
  // 폼은 키 주인의 정책과 인가로 채운다(관리자 자신의 정책·인가가 아니다).
  const form = await screen.findByRole('dialog', { name: '현재 키 수정' });
  const select = await within(form).findByLabelText('연동할 Provider 키 (1개 이상 필수)');
  expect(selectedValue(select)).toBe('cred1');
  expect(optionNames(select)).toEqual(['Anthropic · team-b (…0002)', 'Anthropic · team-a (…0001)']);
  expect(within(form).queryByLabelText('OpenAI Provider 키')).toBeNull();
  // 허용할 API 표: 머리 칸의 전체 선택 하나와 API 한 줄.
  expect(within(form).getAllByRole('checkbox')).toHaveLength(2);
  pickOption(select, 'Anthropic · team-b (…0002)');
  fireEvent.click(saveButton(form));
  expect((await screen.findByRole('status')).textContent).toBe('현재 키 키를 수정했습니다.');
  const sent = writes(calls);
  expect(sent).toHaveLength(1);
  expect(sent[0]).toMatchObject({ url: `/api/keys/${id}`, method: 'PATCH', body: { expected_version: '7', name: '현재 키', grants: ownerPolicy, provider_credentials: { anthropic: 'cred2' } } });
  // 전체 Provider 키 목록은 읽지 않고, 예전 연결 변경 API도 부르지 않는다.
  expect(calls.some(c => c.url.startsWith('/api/admin/provider-credentials') || c.url.includes('/provider-credential'))).toBe(false);
  expect(lists(calls, '/api/admin/principals/svc1')).toBe(1);
  await waitFor(() => expect(lists(calls, '/api/admin/keys')).toBe(2));
});

test('admin_key_list_shows_no_edit_form_when_the_key_owner_cannot_be_read', async () => {
  const calls = await mount({ path: '/dashboard/admin/keys', user: adminMe, items: () => [{ ...key, principal_id: 'svc1', owner: { kind: 'service', display_name: '배치 서비스', email: null } }], read: url => url === '/api/admin/principals/svc1' ? Promise.resolve(response({ error: { code: 'resource_not_found' } }, 404)) : undefined });
  const dialog = await openEdit();
  await within(dialog).findByRole('alert');
  expect(within(dialog).queryByRole('button', { name: '수정 저장' })).toBeNull();
  expect(within(dialog).queryByLabelText('키 이름')).toBeNull();
  fireEvent.click(within(dialog).getByRole('button', { name: '닫기' }));
  expect(screen.queryByRole('dialog')).toBeNull();
  expect(writes(calls)).toHaveLength(0);
});

test('revoked_and_expired_keys_have_no_edit_button', async () => {
  // 조회 시각(2026-06-01)보다 만료일이 이른 키는 만료된 키다.
  await mount({ items: () => [key, { ...key, id: 'revoked-id', name: '회수된 키', revoked_at: '2026-02-01T00:00:00Z' }, { ...key, id: 'expired-id', name: '만료된 키', expires_at: '2026-05-01T00:00:00Z' }] });
  expect(screen.getByRole('button', { name: '현재 키 수정' })).toBeTruthy();
  expect(screen.getByRole('button', { name: '현재 키 회수' })).toBeTruthy();
  expect(screen.queryByRole('button', { name: '회수된 키 수정' })).toBeNull();
  expect(screen.queryByRole('button', { name: '회수된 키 회수' })).toBeNull();
  expect(screen.queryByRole('button', { name: '만료된 키 수정' })).toBeNull();
  expect(screen.getByRole('button', { name: '만료된 키 회수' })).toBeTruthy();
  expect(screen.getAllByRole('button', { name: /수정$/ })).toHaveLength(1);
});

test('binding_whose_authorization_was_removed_starts_unselected_and_dropped_apis_are_listed', async () => {
  // team-x는 더 이상 나에게 인가되지 않았고, old-model은 지금 정책에 없다.
  const stale = { ...key, grants: [...key.grants, { provider: 'anthropic', model: 'old-model', capabilities: ['messages'] }], provider_credentials: [bound('credX', 'team-x', '0009', false)] };
  const calls = await mount({ items: () => [stale] });
  const hint = screen.getByLabelText('현재 키 Provider 키 연결');
  expect(hint.textContent).toContain('인가 해제됨');
  expect(hint.textContent).toContain('수정에서 인가된 Provider 키로 다시 연결하세요.');
  const dialog = await openEdit(), select = within(dialog).getByLabelText('연동할 Provider 키 (1개 이상 필수)');
  expect(selectedValue(select)).toBe('');
  expect(optionNames(select)).toEqual(['Anthropic · team-a (…0001)', 'Anthropic · team-b (…0002)', 'OpenAI · oa (…0003)']);
  expect(saveButton(dialog).disabled).toBe(true);
  expect(within(dialog).getByText('키 주인의 지금 정책에 없어 저장하면 빠지는 API: anthropic / old-model / messages')).toBeTruthy();
  // 인가된 키를 다시 고르면 정책에 남아 있는 허용 API는 그대로 체크돼 있다.
  pickOption(select, 'Anthropic · team-b (…0002)');
  expect(api(dialog, 'anthropic / synthetic-text / messages').checked).toBe(true);
  expect(within(dialog).queryByLabelText('anthropic / old-model / messages')).toBeNull();
  expect(saveButton(dialog).disabled).toBe(false);
  fireEvent.click(saveButton(dialog));
  await screen.findByText('현재 키 키를 수정했습니다.');
  expect(writes(calls).map(c => c.body)).toEqual([{ expected_version: '7', name: '현재 키', grants: [{ provider: 'anthropic', model: 'synthetic-text', capabilities: ['messages'] }], provider_credentials: { anthropic: 'cred2' } }]);
});
