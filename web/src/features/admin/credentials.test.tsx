import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../../app';
import { session } from '../../lib/api';
const me = { principal: { id: 'admin1', display_name: '관리자', email: 'admin@example.test' }, role: 'admin', grants: [{ provider: 'anthropic', model: 'synthetic-text', capabilities: ['messages'] }], sso: { refresh_url: 'https://auth.example.test/sso/refresh' } };
const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
const secret = 'sk-ant-synthetic-company-key-9f3a';
const meta = (id: string, label: string, hint: string, extra: Record<string, unknown> = {}) => ({ id, provider: 'anthropic', label, default: false, bound_keys: '0', state: 'active', decryptable: true, hint, version: '1', verified_at: '2026-11-13T00:00:00Z', created_at: '2026-11-13T00:00:00Z', created_by: 'admin1', created_by_email: 'admin@example.test', retired_at: null, retired_by_email: null, ...extra });
const teamA = meta('credA', 'team-a', '0001', { default: true, bound_keys: '3', version: '2' });
const teamB = meta('credB', 'team-b', '0002');
type Call = { url: string; method: string; body?: Record<string, unknown> };
function stub(handle: (call: Call) => Response | undefined) {
  const calls: Call[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const call = { url: String(input), method: init?.method ?? 'GET', body: init?.body ? JSON.parse(String(init.body)) : undefined };
    calls.push(call);
    if (call.url === '/gateway/me') return Promise.resolve(response(me));
    if (call.url === '/gateway/csrf') return Promise.resolve(response({ token: 'csrf' }));
    return Promise.resolve(handle(call) ?? response({ error: { code: 'resource_not_found' } }, 404));
  }));
  return calls;
}
afterEach(() => { cleanup(); session.clear(); history.replaceState(null, '', '/ui/'); vi.unstubAllGlobals(); });

test('first_key_is_added_with_a_label_and_the_secret_is_never_rendered', async () => {
  history.replaceState(null, '', '/ui/admin/credentials');
  let registered = false;
  const calls = stub(c => {
    if (c.method === 'POST') { registered = true; return response({ command_id: 'cmd', operation: 'add_provider_credential', target: { kind: 'credential', id: 'credA' }, result_version: '1', effect: 'changed', replayed: false, credential: teamA }, 201); }
    if (c.url === '/gateway/admin/provider-credentials') return response({ providers: [{ provider: 'anthropic', status: registered ? 'active' : 'missing', credentials: registered ? [teamA] : [], history: [] }] });
  });
  render(<App />);
  await screen.findByText(/미등록/);
  expect(screen.getByText('처음 등록하는 키는 기본 키가 됩니다.')).toBeTruthy();
  fireEvent.change(screen.getByLabelText('이름 (예: 팀·용도)'), { target: { value: 'team-a' } });
  fireEvent.change(screen.getByLabelText('Anthropic API 키'), { target: { value: secret } });
  fireEvent.submit(screen.getByRole('form', { name: 'Anthropic 키 추가' }));
  // 확인 단계에도 원문은 없고 끝 4자리와 이름만 보인다.
  const confirm = await screen.findByRole('region', { name: '회사 키 명령 확인' });
  expect(document.body.innerHTML).not.toContain(secret);
  expect(confirm.textContent).toContain('…9f3a');
  expect(confirm.textContent).toContain('team-a');
  fireEvent.click(screen.getByRole('button', { name: '명령 실행' }));
  await screen.findByText(/키 team-a\(…9f3a\)를 확인하고 저장했습니다/);
  const item = await screen.findByRole('listitem', { name: '회사 키 team-a' });
  expect(item.textContent).toContain('기본 키');
  expect(item.textContent).toContain('연결된 가상 키 3개');
  const post = calls.filter(c => c.method === 'POST');
  expect(post).toHaveLength(1);
  expect(post[0].url).toBe('/gateway/admin/provider-credentials/anthropic');
  expect(post[0].body).toEqual({ label: 'team-a', secret });
  expect(calls.slice(calls.indexOf(post[0]) + 1).some(c => c.method === 'GET' && c.url === '/gateway/admin/provider-credentials')).toBe(true);
  expect(document.body.innerHTML).not.toContain(secret);
});

test('rejected_key_is_not_retried_and_default_key_retire_requires_a_key_to_move_to', async () => {
  history.replaceState(null, '', '/ui/admin/credentials');
  const calls = stub(c => {
    if (c.method === 'POST') return response({ error: { code: 'provider_credential_rejected', message: 'x' } }, 422);
    if (c.method === 'PATCH' || c.method === 'DELETE') return response({ command_id: 'cmd', operation: 'x', target: { kind: 'credential', id: 'credA' }, result_version: '3', effect: 'changed', replayed: false });
    if (c.url === '/gateway/admin/provider-credentials') return response({ providers: [{ provider: 'anthropic', status: 'active', credentials: [teamA, teamB], history: [] }] });
  });
  render(<App />);
  await screen.findByRole('listitem', { name: '회사 키 team-b' });
  fireEvent.change(screen.getByLabelText('이름 (예: 팀·용도)'), { target: { value: 'team-c' } });
  fireEvent.change(screen.getByLabelText('Anthropic API 키'), { target: { value: 'sk-ant-synthetic-wrong-key-0000' } });
  fireEvent.submit(screen.getByRole('form', { name: 'Anthropic 키 추가' }));
  fireEvent.click(await screen.findByRole('button', { name: '명령 실행' }));
  await screen.findByText(/Provider가 이 키를 거절했습니다/);
  expect(calls.filter(c => c.method !== 'GET').map(c => c.method)).toEqual(['POST']);

  // 기본 키 지정은 지금 버전을 함께 보낸다.
  fireEvent.click(within(screen.getByRole('listitem', { name: '회사 키 team-b' })).getByRole('button', { name: '기본 키로 지정' }));
  await screen.findByText('team-b (…0002)를 기본 키로 지정했습니다.');
  expect(calls.find(c => c.method === 'PATCH')).toMatchObject({ url: '/gateway/admin/provider-credentials/anthropic/credB', body: { expected_version: '1', default: true } });

  // 기본 키(가상 키 3개 연결)를 해제하려면 옮길 키를 골라야 한다.
  const a = screen.getByRole('listitem', { name: '회사 키 team-a' });
  fireEvent.click(within(a).getByRole('button', { name: '해제' }));
  const form = await screen.findByRole('form', { name: 'team-a 해제' });
  expect(form.textContent).toContain('기본 키를 해제하면 고른 키가 새 기본 키가 됩니다.');
  const select = within(form).getByLabelText('연결을 옮길 키 (필수)') as HTMLSelectElement;
  expect(select.required).toBe(true);
  fireEvent.change(select, { target: { value: 'credB' } });
  fireEvent.submit(form);
  const confirm = await screen.findByRole('region', { name: '회사 키 명령 확인' });
  expect(confirm.textContent).toContain('team-b (…0002)로 옮겨집니다');
  fireEvent.click(screen.getByRole('button', { name: '명령 실행' }));
  await screen.findByText('team-a (…0001)를 해제하고 연결을 team-b (…0002)로 옮겼습니다.');
  expect(calls.find(c => c.method === 'DELETE')?.url).toBe('/gateway/admin/provider-credentials/anthropic/credA?expected_version=2&move_to=credB');
});

test('admin_chooses_a_company_key_when_issuing_and_keys_show_their_binding', async () => {
  history.replaceState(null, '', '/ui/keys');
  const issued = { id: 'key1', principal_id: 'admin1', name: '새 키', key_hint: 'mgw_v1_x', created_at: '2026-11-13T00:00:00Z', created_by: 'admin1', created_command_id: 'c1', validity: 'unlimited', expires_at: null, revoked_at: null, version: '1', grants: [], limits: {}, provider_credentials: [{ provider: 'anthropic', credential_id: 'credB', label: 'team-b', hint: '0002', state: 'active' }] };
  let items: unknown[] = [];
  const calls = stub(c => {
    if (c.url === '/gateway/admin/provider-credentials') return response({ providers: [{ provider: 'anthropic', status: 'active', credentials: [teamA, teamB], history: [] }] });
    if (c.method === 'POST' && c.url === '/gateway/keys') { items = [issued]; return response({ key: 'mgw_v1_secret', metadata: issued }, 201); }
    if (c.url.startsWith('/gateway/keys')) return response({ items, next_cursor: null, as_of: '2026-11-13T00:00:00Z' });
  });
  render(<App />);
  const select = await screen.findByLabelText('Anthropic 회사 키') as HTMLSelectElement;
  await waitFor(() => expect(select.value).toBe('credA'));
  fireEvent.change(screen.getByLabelText('키 이름'), { target: { value: '새 키' } });
  fireEvent.change(screen.getByLabelText('유효기간'), { target: { value: 'unlimited' } });
  fireEvent.change(select, { target: { value: 'credB' } });
  fireEvent.submit(screen.getByRole('form', { name: '개인 키 발급' }));
  expect((await screen.findByRole('region', { name: '쓰기 확인' })).textContent).toContain('Anthropic · team-b (…0002)');
  fireEvent.click(screen.getByRole('button', { name: '발급 확인' }));
  const binding = await screen.findByLabelText('새 키 회사 키 연결');
  expect(binding.textContent).toContain('회사 키: Anthropic · team-b (…0002)');
  expect(calls.find(c => c.method === 'POST' && c.url === '/gateway/keys')?.body?.provider_credentials).toEqual({ anthropic: 'credB' });
});
