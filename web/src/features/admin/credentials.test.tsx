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
  expect(within(screen.getByRole('region', { name: 'Anthropic Provider 키 상태' })).getByText('활성 Provider 키가 없어 이 Provider 요청은 503으로 거절됩니다.')).toBeTruthy();
  expect(document.body.textContent).not.toContain('기본 키');
  // 키 추가는 창에서 Provider를 고르고 이름과 키를 넣는다. 확인 창 없이 바로 보낸다.
  fireEvent.click(screen.getByRole('button', { name: 'Provider 키 추가' }));
  const add = await screen.findByRole('dialog', { name: 'Provider 키 추가' });
  expect(within(add).getByText(/저장한 뒤 이 키의 「사용자 인가」로 여러 사용자에게 한 번에 인가하거나/)).toBeTruthy();
  expect(selectedValue(within(add).getByLabelText('Provider'))).toBe('');
  pickOption(within(add).getByLabelText('Provider'), 'Anthropic');
  fireEvent.change(within(add).getByLabelText('이름 (예: 팀·용도)'), { target: { value: 'team-a' } });
  fireEvent.change(within(add).getByLabelText('API 키'), { target: { value: secret } });
  // 입력한 키 값은 화면 HTML에 나오지 않는다.
  expect(document.body.innerHTML).not.toContain(secret);
  fireEvent.submit(within(add).getByRole('form', { name: 'Provider 키 추가' }));
  // 결과 문구에도 키 값은 없고 끝 4자리와 이름만 보인다.
  const done = await screen.findByText(/키 team-a\(…9f3a\)를 확인하고 저장했습니다/);
  expect(done.getAttribute('role')).toBe('status');
  expect(screen.queryByRole('dialog')).toBeNull();
  const row = await screen.findByRole('row', { name: 'Provider 키 team-a' });
  expect(screen.queryByRole('region', { name: 'Anthropic Provider 키 상태' })).toBeNull();
  expect(row.textContent).not.toContain('기본 키');
  expect(within(row).queryByRole('button', { name: /기본 키/ })).toBeNull();
  // 열: Provider, 이름, 키 끝자리, 인가된 사용자, 연결된 가상 키, 등록, Provider 확인, 동작.
  expect(within(screen.getByRole('table', { name: 'Provider 키' })).getAllByRole('columnheader').map(h => h.textContent)).toEqual(['Provider', '이름', '키 끝자리', '인가된 사용자', '연결된 가상 키', '등록', 'Provider 확인', '동작']);
  const cells = within(row).getAllByRole('cell').map(cell => cell.textContent);
  expect(cells.slice(0, 3)).toEqual(['Anthropic', 'team-a', '••••0001']);
  expect(cells[4]).toBe('3개');
  expect(cells[5]).toBe('2026-11-13 00:00admin@example.test');
  expect(cells[6]).toBe('2026-11-13 00:00');
  // 키 교체 창은 이름과 연결이 그대로라는 점을 알린다.
  fireEvent.click(within(row).getByRole('button', { name: 'team-a 키 교체' }));
  const replace = await screen.findByRole('dialog', { name: 'team-a 키 교체' });
  expect(replace.textContent).toContain('이름과 연결은 그대로 두고 키만 바꿉니다');
  expect(within(replace).getByRole('button', { name: '교체 확인' })).toBeTruthy();
  fireEvent.click(within(replace).getByRole('button', { name: '취소' }));
  const post = calls.filter(c => c.method === 'POST');
  expect(post).toHaveLength(1);
  expect(post[0].url).toBe('/api/admin/provider-credentials/anthropic');
  expect(post[0].body).toEqual({ label: 'team-a', secret });
  expect(calls.slice(calls.indexOf(post[0]) + 1).some(c => c.method === 'GET' && c.url === '/api/admin/provider-credentials')).toBe(true);
  expect(calls.filter(c => c.method !== 'GET')).toHaveLength(1);
  expect(document.body.innerHTML).not.toContain(secret);
});

test('rejected_key_is_not_retried_and_retiring_a_bound_key_requires_a_key_to_move_to', async () => {
  history.replaceState(null, '', '/dashboard/admin/credentials');
  const calls = stub(c => {
    if (c.method === 'POST') return response({ error: { code: 'provider_credential_rejected', message: 'x' } }, 422);
    if (c.method === 'PATCH' || c.method === 'DELETE') return response({ command_id: 'cmd', operation: 'x', target: { kind: 'credential', id: 'credA' }, result_version: '3', effect: 'changed', replayed: false });
    if (c.url === '/api/admin/provider-credentials') return response({ providers: [{ provider: 'anthropic', status: 'active', credentials: [teamA, teamB], history: [] }, { provider: 'openai', status: 'active', credentials: [meta('credO', 'oa-main', '0009', { provider: 'openai' })], history: [] }] });
  });
  render(<App />);
  await screen.findByRole('row', { name: 'Provider 키 team-b' });
  // Provider를 고르면 그 Provider의 키만 남는다.
  const rowNames = () => within(screen.getByRole('table', { name: 'Provider 키' })).getAllByRole('row').slice(1).map(r => r.getAttribute('aria-label'));
  expect(rowNames()).toEqual(['Provider 키 team-a', 'Provider 키 team-b', 'Provider 키 oa-main']);
  pickOption(screen.getByLabelText('Provider'), 'OpenAI');
  expect(rowNames()).toEqual(['Provider 키 oa-main']);
  pickOption(screen.getByLabelText('Provider'), 'Anthropic');
  expect(rowNames()).toEqual(['Provider 키 team-a', 'Provider 키 team-b']);
  // 키 추가 창은 걸러 보던 Provider를 미리 골라 둔다. 거절되면 창이 열린 채로 사유를 보여 주고 다시 보내지 않는다.
  fireEvent.click(screen.getByRole('button', { name: 'Provider 키 추가' }));
  const add = await screen.findByRole('dialog', { name: 'Provider 키 추가' });
  expect(selectedValue(within(add).getByLabelText('Provider'))).toBe('anthropic');
  fireEvent.change(within(add).getByLabelText('이름 (예: 팀·용도)'), { target: { value: 'team-c' } });
  fireEvent.change(within(add).getByLabelText('API 키'), { target: { value: 'sk-ant-synthetic-wrong-key-0000' } });
  fireEvent.submit(within(add).getByRole('form', { name: 'Provider 키 추가' }));
  expect((await within(add).findByRole('alert')).textContent).toMatch(/Provider가 이 키를 거절했습니다/);
  expect(screen.getByRole('dialog', { name: 'Provider 키 추가' })).toBe(add);
  expect(calls.filter(c => c.method !== 'GET').map(c => c.method)).toEqual(['POST']);
  expect(calls.find(c => c.method === 'POST')).toMatchObject({ url: '/api/admin/provider-credentials/anthropic', body: { label: 'team-c', secret: 'sk-ant-synthetic-wrong-key-0000' } });
  fireEvent.click(within(add).getByRole('button', { name: '취소' }));
  expect(screen.queryByRole('dialog')).toBeNull();
  expect(calls.filter(c => c.method !== 'GET').map(c => c.method)).toEqual(['POST']);
  pickOption(screen.getByLabelText('Provider'), '전체');
  expect(rowNames()).toHaveLength(3);

  // 이름 변경은 지금 버전과 이름만 보낸다(기본 키 개념은 없다). 확인 창 없이 바로 보낸다.
  fireEvent.click(screen.getByRole('button', { name: 'team-b 이름 변경' }));
  const rename = await screen.findByRole('dialog', { name: 'team-b 이름 변경' });
  fireEvent.change(within(rename).getByLabelText('새 이름'), { target: { value: 'team-b2' } });
  expect(within(rename).getByRole('button', { name: '저장' })).toBeTruthy();
  fireEvent.submit(within(rename).getByRole('form', { name: 'team-b 이름 변경' }));
  await screen.findByText('이름을 team-b2로 바꿨습니다.');
  expect(screen.queryByRole('dialog')).toBeNull();
  expect(calls.find(c => c.method === 'PATCH')).toMatchObject({ url: '/api/admin/provider-credentials/anthropic/credB', body: { expected_version: '1', label: 'team-b2' } });
  expect(Object.keys(calls.find(c => c.method === 'PATCH')!.body!)).toEqual(['expected_version', 'label']);

  // 연결된 가상 키가 없는 키는 옮길 키를 묻지 않는다.
  fireEvent.click(screen.getByRole('button', { name: 'team-b 해제' }));
  const plain = await screen.findByRole('dialog', { name: 'team-b 해제' });
  expect(within(plain).queryByRole('combobox')).toBeNull();
  expect(plain.textContent).toContain('연결된 가상 키가 없어 바로 해제할 수 있습니다.');
  fireEvent.click(within(plain).getByRole('button', { name: '취소' }));

  // 가상 키 3개가 연결된 키를 해제하려면 옮길 키를 골라야 한다. 후보는 같은 Provider의 키뿐이고, 하나뿐이면 미리 골라 둔다.
  fireEvent.click(screen.getByRole('button', { name: 'team-a 해제' }));
  const retire = await screen.findByRole('dialog', { name: 'team-a 해제' });
  const select = within(retire).getByLabelText('연결을 옮길 키 (필수)');
  expect(select.getAttribute('aria-required')).toBe('true');
  expect(selectedValue(select)).toBe('credB');
  expect(optionNames(select)).toEqual(['team-b (…0002)']);
  pickOption(select, 'team-b (…0002)');
  expect((within(retire).getByRole('button', { name: '해제 확인' }) as HTMLButtonElement).disabled).toBe(false);
  fireEvent.submit(within(retire).getByRole('form', { name: 'team-a 해제' }));
  const confirm = await screen.findByRole('dialog', { name: 'team-a 해제 확인' });
  expect(calls.some(c => c.method === 'DELETE')).toBe(false);
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
  fireEvent.click(within(await screen.findByRole('row', { name: 'Provider 키 team-a' })).getByRole('button', { name: 'team-a 해제' }));
  const dialog = await screen.findByRole('dialog', { name: 'team-a 해제' });
  expect(within(dialog).getByRole('form', { name: 'team-a 해제' }).textContent).toContain('옮길 다른 키가 없어 해제할 수 없습니다');
  expect(within(dialog).queryByRole('combobox')).toBeNull();
  expect((within(dialog).getByRole('button', { name: '해제 확인' }) as HTMLButtonElement).disabled).toBe(true);
  // 버튼을 거치지 않고 폼을 직접 보내도 확인 창이 뜨지 않고 아무것도 보내지 않는다.
  fireEvent.submit(within(dialog).getByRole('form', { name: 'team-a 해제' }));
  expect(screen.queryByRole('dialog', { name: 'team-a 해제 확인' })).toBeNull();
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
  // 표의 Provider 키 칸은 열 이름과 겹치는 앞말과 끝 4자리 없이 Provider와 이름만 보여 준다.
  expect(binding.textContent).toContain('Anthropic · team-b');
  expect(binding.textContent).not.toContain('…0002');
  expect(binding.textContent).not.toContain('Provider 키:');
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
