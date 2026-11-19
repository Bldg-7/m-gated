import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../../app';
import { optionNames, pickOption, selectedValue } from '../../test-select';
import { session } from '../../lib/api';
const me = { principal: { id: 'admin1', display_name: '관리자', email: 'admin@example.test' }, role: 'admin', grants: [{ provider: 'anthropic', model: 'synthetic-text', capabilities: ['messages'] }], sso: { refresh_url: 'https://auth.example.test/sso/refresh' } };
const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
const secret = 'sk-ant-synthetic-company-key-9f3a';
const meta = (id: string, label: string, hint: string, extra: Record<string, unknown> = {}) => ({ id, provider: 'anthropic', label, bound_keys: '0', state: 'active', decryptable: true, hint, version: '1', verified_at: '2026-11-13T00:00:00Z', created_at: '2026-11-13T00:00:00Z', created_by: 'admin1', created_by_email: 'admin@example.test', retired_at: null, retired_by_email: null, ...extra });
const teamA = meta('credA', 'team-a', '0001', { bound_keys: '3', version: '2' });
const teamB = meta('credB', 'team-b', '0002');
type Call = { url: string; method: string; body?: Record<string, unknown> };
function stub(handle: (call: Call) => Response | undefined) {
  const calls: Call[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const call = { url: String(input), method: init?.method ?? 'GET', body: init?.body ? JSON.parse(String(init.body)) : undefined };
    calls.push(call);
    if (call.url === '/api/me') return Promise.resolve(response(me));
    if (call.url === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
    return Promise.resolve(handle(call) ?? response({ error: { code: 'resource_not_found' } }, 404));
  }));
  return calls;
}
afterEach(() => { cleanup(); session.clear(); history.replaceState(null, '', '/dashboard/'); vi.unstubAllGlobals(); });

test('first_key_is_added_with_a_label_and_the_secret_is_never_rendered', async () => {
  history.replaceState(null, '', '/dashboard/admin/credentials');
  let registered = false;
  const calls = stub(c => {
    if (c.method === 'POST') { registered = true; return response({ command_id: 'cmd', operation: 'add_provider_credential', target: { kind: 'credential', id: 'credA' }, result_version: '1', effect: 'changed', replayed: false, credential: teamA }, 201); }
    if (c.url === '/api/admin/provider-credentials') return response({ providers: [{ provider: 'anthropic', status: registered ? 'active' : 'missing', credentials: registered ? [teamA] : [], history: [] }] });
  });
  render(<App />);
  await screen.findByText(/미등록/);
  expect(screen.getByRole('heading', { level: 1, name: 'Provider 키' })).toBeTruthy();
  expect(screen.getByText('활성 Provider 키가 없어 이 Provider 요청은 503으로 거절됩니다.')).toBeTruthy();
  expect(screen.getByText(/저장한 뒤 이 키의 「사용자 인가」로 여러 사용자에게 한 번에 인가하거나/)).toBeTruthy();
  expect(document.body.textContent).not.toContain('기본 키');
  fireEvent.change(screen.getByLabelText('이름 (예: 팀·용도)'), { target: { value: 'team-a' } });
  fireEvent.change(screen.getByLabelText('Anthropic API 키'), { target: { value: secret } });
  fireEvent.submit(screen.getByRole('form', { name: 'Anthropic 키 추가' }));
  // 확인 창에도 키 값은 없고 끝 4자리와 이름만 보인다.
  const confirm = await screen.findByRole('dialog', { name: 'Anthropic 키 추가 확인' });
  expect(document.body.innerHTML).not.toContain(secret);
  expect(confirm.textContent).toContain('…9f3a');
  expect(confirm.textContent).toContain('team-a');
  fireEvent.click(within(confirm).getByRole('button', { name: '명령 실행' }));
  await screen.findByText(/키 team-a\(…9f3a\)를 확인하고 저장했습니다/);
  const item = await screen.findByRole('listitem', { name: 'Provider 키 team-a' });
  expect(item.textContent).not.toContain('기본 키');
  expect(within(item).queryByRole('button', { name: '기본 키로 지정' })).toBeNull();
  expect(within(item).getByRole('button', { name: '키 교체' }).getAttribute('title')).toBe('이름과 연결은 그대로 두고 키만 바꿉니다');
  expect(item.textContent).toContain('연결된 가상 키 3개');
  expect(item.textContent).toContain('등록 2026-11-13 00:00 (admin@example.test) · Provider 확인 2026-11-13 00:00');
  const post = calls.filter(c => c.method === 'POST');
  expect(post).toHaveLength(1);
  expect(post[0].url).toBe('/api/admin/provider-credentials/anthropic');
  expect(post[0].body).toEqual({ label: 'team-a', secret });
  expect(calls.slice(calls.indexOf(post[0]) + 1).some(c => c.method === 'GET' && c.url === '/api/admin/provider-credentials')).toBe(true);
  expect(document.body.innerHTML).not.toContain(secret);
});

test('rejected_key_is_not_retried_and_retiring_a_bound_key_requires_a_key_to_move_to', async () => {
  history.replaceState(null, '', '/dashboard/admin/credentials');
  const calls = stub(c => {
    if (c.method === 'POST') return response({ error: { code: 'provider_credential_rejected', message: 'x' } }, 422);
    if (c.method === 'PATCH' || c.method === 'DELETE') return response({ command_id: 'cmd', operation: 'x', target: { kind: 'credential', id: 'credA' }, result_version: '3', effect: 'changed', replayed: false });
    if (c.url === '/api/admin/provider-credentials') return response({ providers: [{ provider: 'anthropic', status: 'active', credentials: [teamA, teamB], history: [] }] });
  });
  render(<App />);
  await screen.findByRole('listitem', { name: 'Provider 키 team-b' });
  fireEvent.change(screen.getByLabelText('이름 (예: 팀·용도)'), { target: { value: 'team-c' } });
  fireEvent.change(screen.getByLabelText('Anthropic API 키'), { target: { value: 'sk-ant-synthetic-wrong-key-0000' } });
  fireEvent.submit(screen.getByRole('form', { name: 'Anthropic 키 추가' }));
  fireEvent.click(within(await screen.findByRole('dialog', { name: 'Anthropic 키 추가 확인' })).getByRole('button', { name: '명령 실행' }));
  await screen.findByText(/Provider가 이 키를 거절했습니다/);
  expect(calls.filter(c => c.method !== 'GET').map(c => c.method)).toEqual(['POST']);

  // 이름 변경은 지금 버전과 이름만 보낸다(기본 키 개념은 없다).
  const b = screen.getByRole('listitem', { name: 'Provider 키 team-b' });
  fireEvent.click(within(b).getByRole('button', { name: '이름 변경' }));
  fireEvent.change(within(b).getByLabelText('새 이름'), { target: { value: 'team-b2' } });
  fireEvent.submit(within(b).getByRole('form', { name: 'team-b 이름 변경' }));
  await screen.findByText('이름을 team-b2로 바꿨습니다.');
  expect(calls.find(c => c.method === 'PATCH')).toMatchObject({ url: '/api/admin/provider-credentials/anthropic/credB', body: { expected_version: '1', label: 'team-b2' } });
  expect(Object.keys(calls.find(c => c.method === 'PATCH')!.body!)).toEqual(['expected_version', 'label']);

  // 연결된 가상 키가 없는 키는 옮길 키를 묻지 않는다.
  fireEvent.click(within(screen.getByRole('listitem', { name: 'Provider 키 team-b' })).getByRole('button', { name: '해제' }));
  const plain = await screen.findByRole('form', { name: 'team-b 해제' });
  expect(within(plain).queryByRole('combobox')).toBeNull();
  expect(plain.textContent).toContain('연결된 가상 키가 없어 바로 해제할 수 있습니다.');
  fireEvent.click(within(plain).getByRole('button', { name: '취소' }));

  // 가상 키 3개가 연결된 키를 해제하려면 옮길 키를 골라야 한다. 후보가 하나뿐이면 미리 골라 둔다.
  const a = screen.getByRole('listitem', { name: 'Provider 키 team-a' });
  fireEvent.click(within(a).getByRole('button', { name: '해제' }));
  const form = await screen.findByRole('form', { name: 'team-a 해제' });
  const select = within(form).getByLabelText('연결을 옮길 키 (필수)');
  expect(select.getAttribute('aria-required')).toBe('true');
  expect(selectedValue(select)).toBe('credB');
  pickOption(select, 'team-b (…0002)');
  fireEvent.submit(form);
  const confirm = await screen.findByRole('dialog', { name: 'team-a 해제 확인' });
  expect(confirm.textContent).toContain('team-b (…0002)로 옮겨집니다');
  expect(within(confirm).getByRole('button', { name: '명령 실행' }).className).toBe('btn-danger');
  fireEvent.click(within(confirm).getByRole('button', { name: '명령 실행' }));
  await screen.findByText('team-a (…0001)를 해제하고 연결을 team-b (…0002)로 옮겼습니다.');
  expect(calls.find(c => c.method === 'DELETE')?.url).toBe('/api/admin/provider-credentials/anthropic/credA?expected_version=2&move_to=credB');
});

test('last_key_with_bound_virtual_keys_cannot_be_retired_without_another_key', async () => {
  history.replaceState(null, '', '/dashboard/admin/credentials');
  const calls = stub(c => {
    if (c.url === '/api/admin/provider-credentials') return response({ providers: [{ provider: 'anthropic', status: 'active', credentials: [teamA], history: [] }] });
  });
  render(<App />);
  fireEvent.click(within(await screen.findByRole('listitem', { name: 'Provider 키 team-a' })).getByRole('button', { name: '해제' }));
  const form = await screen.findByRole('form', { name: 'team-a 해제' });
  expect(form.textContent).toContain('옮길 다른 키가 없어 해제할 수 없습니다');
  expect((within(form).getByRole('button', { name: '해제 확인' }) as HTMLButtonElement).disabled).toBe(true);
  expect(calls.filter(c => c.method !== 'GET')).toHaveLength(0);
});

test('admin_chooses_only_from_own_authorized_provider_keys_when_issuing', async () => {
  history.replaceState(null, '', '/dashboard/keys/new');
  const authorizedMe = { ...me, provider_credentials: [{ provider: 'anthropic', credentials: [{ id: 'credA', label: 'team-a', hint: '0001' }, { id: 'credB', label: 'team-b', hint: '0002' }] }] };
  const issued = { id: 'key1', principal_id: 'admin1', name: '새 키', key_hint: 'mgw_v1_x', created_at: '2026-11-13T00:00:00.000000Z', created_by: 'admin1', created_command_id: 'c1', validity: 'unlimited', expires_at: null, revoked_at: null, version: '1', grants: [], limits: {}, provider_credentials: [{ provider: 'anthropic', credential_id: 'credB', label: 'team-b', hint: '0002', state: 'active', authorized: true }] };
  let items: unknown[] = [];
  const calls: { url: string; method: string; body?: Record<string, unknown> }[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const c = { url: String(input), method: init?.method ?? 'GET', body: init?.body ? JSON.parse(String(init.body)) : undefined };
    calls.push(c);
    if (c.url === '/api/me') return Promise.resolve(response(authorizedMe));
    if (c.url === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (c.method === 'POST' && c.url === '/api/keys') { items = [issued]; return Promise.resolve(response({ key: 'mgw_v1_secret', metadata: issued }, 201)); }
    if (c.url.startsWith('/api/keys')) return Promise.resolve(response({ items, next_cursor: null, as_of: '2026-11-13T00:00:00Z' }));
    return Promise.resolve(response({ error: { code: 'resource_not_found' } }, 404));
  }));
  render(<App />);
  fireEvent.change(await screen.findByLabelText('키 이름'), { target: { value: '새 키' } });
  pickOption(screen.getByLabelText('유효기간'), '무제한 (만료 없음, 회수·정지는 적용)');
  // Provider 키를 고르기 전에는 고를 API가 없다.
  expect(screen.queryByLabelText('anthropic / synthetic-text / messages')).toBeNull();
  // Provider 키는 미리 고르지 않고, 고르지 않으면 확인 창을 열지 않는다.
  const select = screen.getByLabelText('Anthropic Provider 키') as HTMLInputElement;
  expect(selectedValue(select)).toBe('');
  expect(optionNames(select)).toEqual(['연동 안 함', 'team-a (…0001)', 'team-b (…0002)']);
  expect((screen.getByRole('button', { name: '발급 내용 확인' }) as HTMLButtonElement).disabled).toBe(true);
  fireEvent.submit(screen.getByRole('form', { name: '가상 키 발급' }));
  expect(screen.getByRole('alert').textContent).toBe('연동할 Provider 키를 하나 이상 고르세요.');
  expect(screen.queryByRole('dialog')).toBeNull();
  // 글자를 쳐서 좁힌 뒤 고른다.
  fireEvent.click(select);
  fireEvent.change(select, { target: { value: 'TEAM-B' } });
  pickOption(select, 'team-b (…0002)');
  expect(select.value).toBe('team-b (…0002)');
  fireEvent.click(screen.getByLabelText('anthropic / synthetic-text / messages'));
  fireEvent.submit(screen.getByRole('form', { name: '가상 키 발급' }));
  const confirm = await screen.findByRole('dialog', { name: '발급 확인' });
  expect(confirm.textContent).toContain('Anthropic · team-b (…0002)');
  fireEvent.click(within(confirm).getByRole('button', { name: '발급 확인' }));
  fireEvent.click(within(await screen.findByRole('dialog', { name: '발급된 키' })).getByRole('button', { name: '닫기' }));
  // 키 창을 닫으면 목록 페이지로 돌아가 새 키와 그 Provider 키 연결을 보여 준다.
  const binding = await screen.findByLabelText('새 키 Provider 키 연결');
  expect(location.pathname).toBe('/dashboard/keys');
  expect(binding.textContent).toContain('Provider 키: Anthropic · team-b (…0002)');
  expect(calls.find(c => c.method === 'POST' && c.url === '/api/keys')?.body?.provider_credentials).toEqual({ anthropic: 'credB' });
  // 관리자도 발급할 때 전체 Provider 키 목록을 읽지 않는다.
  expect(calls.some(c => c.url === '/api/admin/provider-credentials')).toBe(false);
  expect(screen.getAllByText('2026-11-13 00:00').length).toBeGreaterThan(0);

  // 내 가상 키는 주인이 수정 창에서 Provider 키 연결을 바꾼다. 후보는 나에게 인가된 키뿐이고, 전체 Provider 키 목록은 읽지 않는다.
  expect(within(binding).queryByRole('button')).toBeNull();
  fireEvent.click(screen.getByRole('button', { name: '새 키 수정' }));
  const edit = within(await screen.findByRole('dialog', { name: '새 키 수정' })).getByLabelText('Anthropic Provider 키');
  expect(selectedValue(edit)).toBe('credB');
  expect(optionNames(edit)).toEqual(['연동 안 함', 'team-a (…0001)', 'team-b (…0002)']);
  expect(calls.some(c => c.url.startsWith('/api/admin/'))).toBe(false);
});
