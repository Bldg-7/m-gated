import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../../app';
import { session } from '../../lib/api';
const me = { principal: { id: 'admin1', display_name: '관리자', email: 'admin@example.test' }, role: 'admin', grants: [], sso: { refresh_url: 'https://auth.example.test/sso/refresh' } };
const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
const secret = 'sk-ant-synthetic-company-key-9f3a';
const active = { id: 'cred1', provider: 'anthropic', state: 'active', decryptable: true, hint: '9f3a', version: '1', verified_at: '2026-11-13T00:00:00Z', created_at: '2026-11-13T00:00:00Z', created_by: 'admin1', created_by_email: 'admin@example.test', retired_at: null, retired_by_email: null };
afterEach(() => { cleanup(); session.clear(); history.replaceState(null, '', '/ui/'); vi.unstubAllGlobals(); });

test('provider_key_is_sent_once_never_rendered_and_list_is_reloaded', async () => {
  history.replaceState(null, '', '/ui/admin/credentials');
  const calls: { url: string; method: string; body?: Record<string, unknown> }[] = [];
  let registered = false;
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input); calls.push({ url, method: init?.method ?? 'GET', body: init?.body ? JSON.parse(String(init.body)) : undefined });
    if (url === '/gateway/me') return Promise.resolve(response(me));
    if (url === '/gateway/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (init?.method === 'PUT') { registered = true; return Promise.resolve(response({ command_id: 'cmd', operation: 'set_provider_credential', target: { kind: 'credential', id: 'cred1' }, result_version: '1', effect: 'changed', replayed: false, credential: active }, 201)); }
    if (url === '/gateway/admin/provider-credentials') return Promise.resolve(response({ providers: [{ provider: 'anthropic', status: registered ? 'active' : 'missing', active: registered ? active : null, history: registered ? [active] : [] }] }));
    return Promise.resolve(response({ error: { code: 'resource_not_found' } }, 404));
  }));
  render(<App />);
  await screen.findByText(/미등록/);
  fireEvent.change(screen.getByLabelText('Anthropic API 키'), { target: { value: secret } });
  fireEvent.submit(screen.getByRole('form', { name: 'Anthropic 키 등록' }));
  // 확인 단계에도 원문은 없고 끝 4자리만 보인다.
  await screen.findByRole('region', { name: '회사 키 명령 확인' });
  expect(document.body.innerHTML).not.toContain(secret);
  expect(screen.getByRole('region', { name: '회사 키 명령 확인' }).textContent).toContain('…9f3a');
  fireEvent.click(screen.getByRole('button', { name: '명령 실행' }));
  await screen.findByText(/키\(…9f3a\)를 확인하고 저장했습니다/);
  await screen.findByText(/정상/);
  const put = calls.filter(c => c.method === 'PUT');
  expect(put).toHaveLength(1);
  expect(put[0].url).toBe('/gateway/admin/provider-credentials/anthropic');
  expect(put[0].body).toEqual({ secret, expected_credential_id: null });
  expect(calls.slice(calls.indexOf(put[0]) + 1).some(c => c.method === 'GET' && c.url === '/gateway/admin/provider-credentials')).toBe(true);
  expect(document.body.innerHTML).not.toContain(secret);
  expect((screen.getByLabelText('Anthropic API 키') as HTMLInputElement).value).toBe('');
});

test('rejected_key_is_not_retried_and_retire_sends_expected_id', async () => {
  history.replaceState(null, '', '/ui/admin/credentials');
  const methods: string[] = [], urls: string[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input); if (init?.method) { methods.push(init.method); urls.push(url); }
    if (url === '/gateway/me') return Promise.resolve(response(me));
    if (url === '/gateway/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (init?.method === 'PUT') return Promise.resolve(response({ error: { code: 'provider_credential_rejected', message: 'x' } }, 422));
    if (init?.method === 'DELETE') return Promise.resolve(response({ command_id: 'cmd', operation: 'retire_provider_credential', target: { kind: 'credential', id: 'cred1' }, result_version: '2', effect: 'changed', replayed: false }));
    return Promise.resolve(response({ providers: [{ provider: 'anthropic', status: 'active', active, history: [active] }] }));
  }));
  render(<App />);
  await screen.findByText(/정상/);
  fireEvent.change(screen.getByLabelText('Anthropic API 키'), { target: { value: 'sk-ant-synthetic-wrong-key-0000' } });
  fireEvent.submit(screen.getByRole('form', { name: 'Anthropic 키 등록' }));
  fireEvent.click(await screen.findByRole('button', { name: '명령 실행' }));
  await screen.findByText(/Provider가 이 키를 거절했습니다/);
  expect(methods).toEqual(['PUT']);
  fireEvent.click(screen.getByRole('button', { name: '키 해제' }));
  fireEvent.click(await screen.findByRole('button', { name: '명령 실행' }));
  await screen.findByText(/키\(…9f3a\)를 해제했습니다/);
  expect(methods).toEqual(['PUT', 'DELETE']);
  expect(urls[1]).toBe('/gateway/admin/provider-credentials/anthropic?expected_credential_id=cred1');
});
