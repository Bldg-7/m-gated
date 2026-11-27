import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../../app';
import { session } from '../../lib/api';

const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
const admin = { principal: { id: 'admin1', display_name: '관리자', email: 'admin@example.test', block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' } }, role: 'admin', grants: [], provider_credentials: [], auth: { mode: 'sso', refresh_url: 'https://auth.example.test/sso/refresh', logout_url: 'https://auth.example.test/sso/logout' } };
const token = (id: string, more: Record<string, unknown> = {}) => ({ id, name: `토큰 ${id}`, scope: 'read', validity: '90_days', owner: { id: 'admin1', email: 'admin@example.test', display_name: '관리자' }, created_at: '2026-11-01T00:00:00Z', expires_at: '2027-01-30T00:00:00Z', revoked_at: null, revoked_by_email: null, last_used_at: null, active: true, ...more });
const items = [
  token('t1', { scope: 'write', last_used_at: '2026-11-20T00:00:00Z' }),
  token('t2', { revoked_at: '2026-11-10T00:00:00Z', revoked_by_email: 'bob@example.test', active: false }),
  token('t3', { active: false, owner: { id: 'bob1', email: 'bob@example.test', display_name: 'Bob' } }),
];
afterEach(() => { cleanup(); session.clear(); history.replaceState(null, '', '/dashboard/'); vi.unstubAllGlobals(); });
function stub(list: unknown[] = items) {
  const writes: { url: string; method: string; body: unknown }[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url === '/api/me') return Promise.resolve(response(admin));
    if (url === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (init?.method && init.method !== 'GET') {
      writes.push({ url, method: init.method, body: init.body === undefined ? undefined : JSON.parse(String(init.body)) });
      const created = init.method === 'POST';
      return Promise.resolve(response({ command_id: 'cmd', operation: created ? 'create_management_token' : 'revoke_management_token', target: { kind: 'management_token', id: 't9' }, result_version: null, effect: 'changed', replayed: false, token: token('t9', { name: '배포' }), ...(created ? { secret: 'mgt_synthetic-secret' } : {}) }, created ? 201 : 200));
    }
    if (url === '/api/admin/management-tokens') return Promise.resolve(response({ items: list }));
    return Promise.resolve(response({ items: [], next_cursor: null, as_of: '2026-11-01T00:00:00Z' }));
  }));
  return { writes };
}

test('token_list_shows_state_owner_and_last_use_and_only_active_tokens_can_be_revoked', async () => {
  stub();
  history.replaceState(null, '', '/dashboard/admin/tokens'); render(<App />);
  const table = await screen.findByRole('table', { name: '관리 토큰' });
  const rows = within(table).getAllByRole('row');
  expect(within(rows[1]).getByText('쓰기')).toBeTruthy();
  expect(within(rows[1]).getByText('활성')).toBeTruthy();
  expect(within(rows[1]).getByRole('button', { name: '토큰 t1 토큰 폐기' })).toBeTruthy();
  // 폐기한 사람과 시각은 배지의 말풍선에 있다.
  const revoked = within(rows[2]).getByText('폐기됨');
  expect(revoked.closest('.badge')?.getAttribute('aria-describedby')).toBeTruthy();
  expect(within(rows[2]).getByText('쓴 적 없음')).toBeTruthy();
  expect(within(rows[2]).queryByRole('button')).toBeNull();
  expect(within(rows[3]).getByText('만료됨')).toBeTruthy();
  expect(within(rows[3]).getByText('Bob')).toBeTruthy();
  expect(screen.getByText('내 활성 토큰 1/30개')).toBeTruthy();
});

test('issuing_shows_the_secret_once_and_revoking_sends_delete_without_body', async () => {
  const { writes } = stub();
  history.replaceState(null, '', '/dashboard/admin/tokens'); render(<App />);
  const form = await screen.findByRole('form', { name: '관리 토큰 발급' });
  fireEvent.change(within(form).getByLabelText('이름'), { target: { value: ' 배포 ' } });
  fireEvent.click(within(form).getByLabelText('쓰기'));
  fireEvent.click(within(form).getByLabelText('1년'));
  fireEvent.submit(form);
  expect(writes).toHaveLength(0);
  fireEvent.click(within(await screen.findByRole('dialog', { name: '관리 토큰 발급' })).getByRole('button', { name: '명령 실행' }));
  await waitFor(() => expect(writes).toEqual([{ url: '/api/admin/management-tokens', method: 'POST', body: { name: '배포', scope: 'write', validity: '1_year' } }]));
  const secret = await screen.findByRole('dialog', { name: '발급된 관리 토큰' });
  expect(within(secret).getByText('mgt_synthetic-secret')).toBeTruthy();
  expect(within(secret).getByRole('button', { name: '토큰 복사' })).toBeTruthy();
  fireEvent.click(within(secret).getByRole('button', { name: '닫기' }));
  await waitFor(() => expect(screen.queryByText('mgt_synthetic-secret')).toBeNull());
  // 발급한 뒤에는 이름 칸이 비어 있다.
  expect((within(screen.getByRole('form', { name: '관리 토큰 발급' })).getByLabelText('이름') as HTMLInputElement).value).toBe('');

  const table = await screen.findByRole('table', { name: '관리 토큰' });
  fireEvent.click(within(table).getByRole('button', { name: '토큰 t1 토큰 폐기' }));
  const confirm = await screen.findByRole('dialog', { name: '관리 토큰 폐기' });
  // 되돌릴 수 없는 명령이라 처음 초점은 취소다.
  await waitFor(() => expect(document.activeElement?.textContent).toBe('취소'));
  fireEvent.click(within(confirm).getByRole('button', { name: '명령 실행' }));
  await waitFor(() => expect(writes[1]).toEqual({ url: '/api/admin/management-tokens/t1', method: 'DELETE', body: undefined }));
});

test('at_the_active_limit_the_issue_button_is_replaced_by_the_next_step', async () => {
  stub(Array.from({ length: 30 }, (_, i) => token(`a${i}`)));
  history.replaceState(null, '', '/dashboard/admin/tokens'); render(<App />);
  await screen.findByText('활성 토큰이 30개라 더 발급할 수 없습니다. 쓰지 않는 토큰을 아래 목록에서 폐기하세요.');
  expect(within(screen.getByRole('form', { name: '관리 토큰 발급' })).queryByRole('button', { name: '발급' })).toBeNull();
});
