import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../../app';
import { pickOption } from '../../test-select';
import { session } from '../../lib/api';

const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
const admin = { principal: { id: 'admin1', display_name: '관리자', email: 'admin@example.test', block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' } }, role: 'admin', grants: [], provider_credentials: [], sso: { refresh_url: 'https://auth.example.test/sso/refresh' } };
const meta = (id: string, label: string, hint: string, holders: string[]) => ({ id, label, hint, version: '1', decryptable: true, bound_keys: '0', created_at: '2026-11-01T00:00:00Z', created_by: 'admin1', created_by_email: null, verified_at: '2026-11-01T00:00:00Z', authorized_principal_ids: holders });
const person = (id: string, kind: 'human' | 'service', name: string, email: string | null) => ({ id, kind, display_name: name, email, management_owner_id: null, version: '1', policy_version: '1', disabled: false, inference_safety: { paused: false, version: '1', reason: null, paused_at: null, trigger_request_id: null }, block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' } });
const people = [person('h1', 'human', '가람', 'garam@example.test'), person('h2', 'human', '나래', 'narae@example.test'), person('h3', 'human', '다온', 'daon@example.test'), person('s1', 'service', '배치 서비스', null)];
afterEach(() => { cleanup(); session.clear(); history.replaceState(null, '', '/dashboard/'); vi.unstubAllGlobals(); });

/** 사용자 목록은 두 페이지로 나눠 준다. 창이 끝 페이지까지 읽는지 본다. */
function stub(write: (body: Record<string, unknown>) => Response) {
  let holders = ['h1'];
  const posts: { url: string; body: Record<string, unknown> }[] = [];
  const fetcher = vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url === '/api/me') return Promise.resolve(response(admin));
    if (url === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (init?.method === 'POST') {
      const body = JSON.parse(String(init.body)); posts.push({ url, body });
      const result = write(body);
      if (result.ok) holders = [...new Set([...holders, ...(body.principal_ids as string[])])];
      return Promise.resolve(result);
    }
    if (url === '/api/admin/provider-credentials') return Promise.resolve(response({ providers: [{ provider: 'anthropic', status: 'active', credentials: [meta('a', 'team-a', '0001', holders)], history: [] }] }));
    if (url === '/api/admin/principals?limit=100') return Promise.resolve(response({ items: people.slice(0, 2), next_cursor: 'next', as_of: 'now' }));
    if (url === '/api/admin/principals?limit=100&cursor=next') return Promise.resolve(response({ items: people.slice(2), next_cursor: null, as_of: 'now' }));
    return Promise.resolve(response({ error: { code: 'unexpected' } }, 500));
  });
  vi.stubGlobal('fetch', fetcher);
  return posts;
}
async function open() {
  history.replaceState(null, '', '/dashboard/admin/credentials'); render(<App />);
  const item = await screen.findByRole('listitem', { name: 'Provider 키 team-a' });
  expect(within(item).getByText(/인가된 사용자 1명/)).toBeTruthy();
  fireEvent.click(within(item).getByRole('button', { name: '사용자 인가' }));
  const dialog = await screen.findByRole('dialog', { name: 'team-a 사용자 인가' });
  await within(dialog).findByRole('list', { name: '인가할 사용자' });
  return dialog;
}
const box = (dialog: HTMLElement, name: RegExp) => within(dialog).getByRole('checkbox', { name }) as HTMLInputElement;

test('one_provider_key_is_granted_to_many_checked_users_in_one_command', async () => {
  const posts = stub(() => response({ command_id: 'cmd', operation: 'authorize_principal_credentials', target: { kind: 'credential', id: 'a' }, result_version: '1', effect: 'changed' }));
  const dialog = await open();
  // 두 페이지의 사용자가 모두 보이고, 이미 인가된 사용자는 체크된 채 잠겨 있다.
  expect(within(dialog).getAllByRole('checkbox')).toHaveLength(4);
  expect(box(dialog, /가람/).checked).toBe(true); expect(box(dialog, /가람/).disabled).toBe(true);
  const confirm = within(dialog).getByRole('button', { name: /명에게 인가$/ }) as HTMLButtonElement;
  expect(confirm.disabled).toBe(true);
  // 전체 선택은 지금 걸러 보고 있는 목록에만 적용한다. 사람만 걸러 보면 서비스 계정은 고르지 않는다.
  pickOption(within(dialog).getByLabelText('종류'), '사람');
  expect(within(dialog).queryByRole('checkbox', { name: /배치 서비스/ })).toBeNull();
  fireEvent.click(within(dialog).getByRole('button', { name: '보이는 사용자 모두 선택' }));
  pickOption(within(dialog).getByLabelText('종류'), '전체');
  expect(box(dialog, /나래/).checked).toBe(true); expect(box(dialog, /다온/).checked).toBe(true); expect(box(dialog, /배치 서비스/).checked).toBe(false);
  // 검색으로 좁힌 뒤 하나를 뺀다.
  fireEvent.change(within(dialog).getByLabelText('사용자 검색 (이름·이메일·ID)'), { target: { value: 'daon@' } });
  expect(within(dialog).getAllByRole('checkbox')).toHaveLength(1);
  fireEvent.click(box(dialog, /다온/));
  fireEvent.change(within(dialog).getByLabelText('사용자 검색 (이름·이메일·ID)'), { target: { value: '' } });
  fireEvent.click(box(dialog, /배치 서비스/));
  expect(confirm.textContent).toBe('2명에게 인가');
  fireEvent.click(confirm);
  await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
  expect(posts).toHaveLength(1);
  expect(posts[0].url).toBe('/api/admin/provider-credentials/anthropic/a');
  expect([...(posts[0].body.principal_ids as string[])].sort()).toEqual(['h2', 's1']);
  expect((await screen.findByText('team-a (…0001)를 2명에게 인가했습니다.')).getAttribute('role')).toBe('status');
  expect(await screen.findByText(/인가된 사용자 3명/)).toBeTruthy();
});

test('failed_grant_keeps_the_window_open_and_is_not_sent_again', async () => {
  const posts = stub(() => response({ error: { code: 'resource_not_found' } }, 404));
  const dialog = await open();
  fireEvent.click(box(dialog, /나래/));
  fireEvent.click(within(dialog).getByRole('button', { name: '1명에게 인가' }));
  const alert = await within(dialog).findByRole('alert');
  expect(alert.textContent).toContain('아무도 인가하지 않았습니다.');
  expect(alert.textContent).toMatch(/명령 ID: [0-9a-f-]{36}/);
  expect(posts).toHaveLength(1);
  expect(box(dialog, /나래/).checked).toBe(true);
});
