import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../../app';
import { session } from '../../lib/api';
import { pickOption } from '../../test-select';

const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
const admin = { principal: { id: 'admin1', display_name: '관리자', email: 'admin@example.test', block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' } }, role: 'admin', grants: [], provider_credentials: [], auth: { mode: 'sso', refresh_url: 'https://auth.example.test/sso/refresh', logout_url: 'https://auth.example.test/sso/logout' } };
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

// 비밀번호 방식(결정 0011): 비밀번호 열과 링크 보내기.
const passwordAdmin = { ...admin, auth: { mode: 'password', refresh_url: null, logout_url: '/dashboard/signin' } };
const passwordEntries = [
  { ...entries[0], password_set: true },
  { ...entries[1], password_set: false },
  { id: 'a3', email: 'carol@example.test', role: 'user', fixed: false, added_by_email: 'admin@example.test', added_at: '2026-11-03T00:00:00Z', version: '2', principal: { id: 'human3', disabled: false }, password_set: true },
];
/** reply: 쓰기의 답. 기본은 성공 ack다. */
function passwordStub(reply: (url: string) => Response = () => response({ command_id: 'cmd', operation: 'x', target: { kind: 'access', id: 'a2' }, result_version: '3', effect: 'changed' })) {
  const writes: { url: string; method: string; body: unknown; headers: Record<string, string> }[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url === '/api/me') return Promise.resolve(response(passwordAdmin));
    if (url === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (init?.method && init.method !== 'GET') {
      writes.push({ url, method: init.method, body: init.body, headers: init.headers as Record<string, string> });
      return Promise.resolve(reply(url));
    }
    if (url === '/api/admin/access') return Promise.resolve(response({ items: passwordEntries }));
    return Promise.resolve(response({ items: [], next_cursor: null, as_of: '2026-11-01T00:00:00Z' }));
  }));
  return { writes };
}

test('password_mode_shows_whether_each_person_set_a_password_and_sends_links_without_a_body', async () => {
  const { writes } = passwordStub();
  history.replaceState(null, '', '/dashboard/admin/members'); render(<App />);
  const table = await screen.findByRole('table', { name: '접근 목록' });
  expect(within(table).getAllByRole('columnheader').map(h => h.textContent)).toEqual(['이메일', '역할', '계정', '비밀번호', '추가한 사람', '추가 시각', '작업']);
  const rows = within(table).getAllByRole('row');
  // 자기 항목에는 링크 보내기가 없다.
  expect(within(rows[1]).getByText('정함')).toBeTruthy();
  expect(within(rows[1]).queryByRole('button')).toBeNull();
  // 비밀번호가 없으면 초대를, 있으면 재설정 링크를 다시 보낸다.
  expect(within(rows[2]).getByText('초대 대기')).toBeTruthy();
  expect(within(rows[3]).getByRole('button', { name: '재설정 링크 보내기' })).toBeTruthy();
  fireEvent.click(within(rows[2]).getByRole('button', { name: '초대 다시 보내기' }));
  // 확인해야 보낸다.
  const dialog = await screen.findByRole('dialog', { name: '초대 다시 보내기' });
  expect(writes).toHaveLength(0);
  fireEvent.click(within(dialog).getByRole('button', { name: '명령 실행' }));
  await waitFor(() => expect(writes.map(w => [w.url, w.method, w.body])).toEqual([['/api/admin/access/a2/link', 'POST', undefined]]));
  // 본문이 없으므로 Content-Type도 없다. 명령 ID와 CSRF 토큰은 붙인다.
  expect(writes[0].headers['Content-Type']).toBeUndefined();
  expect(writes[0].headers['X-Gateway-Command-ID']).toBeTruthy();
  expect(writes[0].headers['X-CSRF-Token']).toBe('csrf');
});

test('password_mode_tells_when_the_invitation_or_link_mail_was_not_sent', async () => {
  passwordStub(url => url === '/api/admin/access'
    ? response({ command_id: 'cmd', operation: 'add_access', target: { kind: 'access', id: 'a4' }, result_version: '1', effect: 'changed', invitation: 'failed' }, 201)
    : response({ error: { code: 'mail_failed' } }, 502));
  history.replaceState(null, '', '/dashboard/admin/members'); render(<App />);
  const form = await screen.findByRole('form', { name: '사람 추가' });
  expect(within(form).getByText(/추가하면 초대 메일을 보냅니다\./)).toBeTruthy();
  fireEvent.change(within(form).getByLabelText('이메일'), { target: { value: 'dan@example.test' } });
  fireEvent.submit(form);
  fireEvent.click(within(await screen.findByRole('dialog', { name: '접근 목록에 추가' })).getByRole('button', { name: '명령 실행' }));
  expect(await screen.findByText('초대 메일을 보내지 못했습니다. 목록의 초대 다시 보내기 버튼으로 다시 보내세요.')).toBeTruthy();

  const table = await screen.findByRole('table', { name: '접근 목록' });
  fireEvent.click(within(within(table).getAllByRole('row')[3]).getByRole('button', { name: '재설정 링크 보내기' }));
  fireEvent.click(within(await screen.findByRole('dialog', { name: '재설정 링크 보내기' })).getByRole('button', { name: '명령 실행' }));
  expect(await screen.findByText(/메일을 보내지 못했습니다\. 잠시 뒤 다시 보내세요\./)).toBeTruthy();
  // 다음 명령을 보내면 초대 실패 알림은 지운다.
  expect(screen.queryByText('초대 메일을 보내지 못했습니다. 목록의 초대 다시 보내기 버튼으로 다시 보내세요.')).toBeNull();
});

test('sso_mode_has_no_password_column_or_link_buttons', async () => {
  stub();
  history.replaceState(null, '', '/dashboard/admin/members'); render(<App />);
  const table = await screen.findByRole('table', { name: '접근 목록' });
  expect(within(table).queryByRole('columnheader', { name: '비밀번호' })).toBeNull();
  expect(screen.queryByRole('button', { name: '초대 다시 보내기' })).toBeNull();
});
