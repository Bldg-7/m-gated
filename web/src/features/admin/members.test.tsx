import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../../app';
import { session } from '../../lib/api';
import { pickOption } from '../../test-select';

const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
const admin = { principal: { id: 'admin1', display_name: '관리자', email: 'admin@example.test', block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' } }, role: 'admin', grants: [], provider_credentials: [], sso: { refresh_url: 'https://auth.example.test/sso/refresh' } };
const entries = [
  { id: 'a1', email: 'admin@example.test', role: 'admin', fixed: true, added_by_email: null, added_at: '2026-11-01T00:00:00Z', version: '1', principal: { id: 'admin1', disabled: false } },
  { id: 'a2', email: 'bob@example.test', role: 'user', fixed: false, added_by_email: 'admin@example.test', added_at: '2026-11-02T00:00:00Z', version: '3', principal: null },
];
afterEach(() => { cleanup(); session.clear(); history.replaceState(null, '', '/dashboard/'); vi.unstubAllGlobals(); });
function stub() {
  const writes: { url: string; method: string; body: unknown }[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url === '/api/me') return Promise.resolve(response(admin));
    if (url === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (init?.method && init.method !== 'GET') {
      writes.push({ url, method: init.method, body: init.body === undefined ? undefined : JSON.parse(String(init.body)) });
      return Promise.resolve(response({ command_id: 'cmd', operation: 'x', target: { kind: 'access', id: 'a2' }, result_version: '4', effect: 'changed' }));
    }
    if (url === '/api/admin/access') return Promise.resolve(response({ items: entries }));
    return Promise.resolve(response({ items: [], next_cursor: null, as_of: '2026-11-01T00:00:00Z' }));
  }));
  return { writes };
}

test('access_list_shows_entries_and_locks_the_fixed_admin_and_own_entry', async () => {
  stub();
  history.replaceState(null, '', '/dashboard/admin/members'); render(<App />);
  const table = await screen.findByRole('table', { name: '접근 목록' });
  const rows = within(table).getAllByRole('row');
  // 고정 관리자는 자기 항목이기도 하다. 바꾸는 버튼이 없다.
  expect(within(rows[1]).getByText('설정 파일 관리자')).toBeTruthy();
  expect(within(rows[1]).queryByRole('button')).toBeNull();
  // 아직 로그인하지 않은 사람은 그렇게 보이고, 역할 변경과 지우기를 할 수 있다.
  expect(within(rows[2]).getByText('아직 로그인하지 않음')).toBeTruthy();
  expect(within(rows[2]).getByRole('button', { name: '관리자로 바꾸기' })).toBeTruthy();
  expect(within(rows[2]).getByRole('button', { name: '지우기' })).toBeTruthy();
});

test('adding_changing_and_removing_send_the_commands_after_confirmation', async () => {
  const { writes } = stub();
  history.replaceState(null, '', '/dashboard/admin/members'); render(<App />);
  const form = await screen.findByRole('form', { name: '사람 추가' });
  fireEvent.change(within(form).getByLabelText('이메일'), { target: { value: ' carol@example.test ' } });
  pickOption(within(form).getByLabelText('역할'), '관리자');
  fireEvent.submit(form);
  // 확인해야 보낸다.
  expect(writes).toHaveLength(0);
  fireEvent.click(within(await screen.findByRole('dialog', { name: '접근 목록에 추가' })).getByRole('button', { name: '명령 실행' }));
  await waitFor(() => expect(writes).toEqual([{ url: '/api/admin/access', method: 'POST', body: { email: 'carol@example.test', role: 'admin' } }]));

  const table = await screen.findByRole('table', { name: '접근 목록' });
  fireEvent.click(within(within(table).getAllByRole('row')[2]).getByRole('button', { name: '관리자로 바꾸기' }));
  fireEvent.click(within(await screen.findByRole('dialog', { name: '관리자로 바꾸기' })).getByRole('button', { name: '명령 실행' }));
  await waitFor(() => expect(writes[1]).toEqual({ url: '/api/admin/access/a2', method: 'PATCH', body: { role: 'admin', expected_version: '3' } }));

  const again = await screen.findByRole('table', { name: '접근 목록' });
  fireEvent.click(within(within(again).getAllByRole('row')[2]).getByRole('button', { name: '지우기' }));
  fireEvent.click(within(await screen.findByRole('dialog', { name: '접근 목록에서 지우기' })).getByRole('button', { name: '명령 실행' }));
  // DELETE는 본문 없이 버전을 query로 보낸다.
  await waitFor(() => expect(writes[2]).toEqual({ url: '/api/admin/access/a2?expected_version=3', method: 'DELETE', body: undefined }));
});
