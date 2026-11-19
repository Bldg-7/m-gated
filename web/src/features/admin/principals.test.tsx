import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../../app';
import { session } from '../../lib/api';
import { optionNames, pickOption } from '../../test-select';

const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
const admin = { principal: { id: 'admin1', display_name: '관리자', email: 'admin@example.test', block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' } }, role: 'admin', grants: [], provider_credentials: [], sso: { refresh_url: 'https://auth.example.test/sso/refresh' } };
afterEach(() => { cleanup(); session.clear(); history.replaceState(null, '', '/dashboard/'); vi.unstubAllGlobals(); });
function stub() {
  const reads: string[] = [], posts: Record<string, unknown>[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url === '/api/me') return Promise.resolve(response(admin));
    if (url === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (init?.method === 'POST') { posts.push(JSON.parse(String(init.body))); return Promise.resolve(response({ command_id: 'cmd', operation: 'create_service', target: { kind: 'principal', id: 'service9' }, result_version: '1', effect: 'changed' }, 201)); }
    if (url.startsWith('/api/admin/principals')) reads.push(url);
    // owner 후보는 사람 사용자만 읽는다.
    if (url === '/api/admin/principals?kind=human&limit=100') return Promise.resolve(response({ items: [{ id: 'admin1', kind: 'human', display_name: '관리자', email: 'admin@example.test' }], next_cursor: null, as_of: '2026-11-01T00:00:00Z' }));
    return Promise.resolve(response({ items: [], next_cursor: null, as_of: '2026-11-01T00:00:00Z' }));
  }));
  return { reads, posts };
}

test('principal_list_is_searched_by_email_and_has_no_lookup_by_id', async () => {
  const { reads } = stub();
  history.replaceState(null, '', '/dashboard/admin/principals'); render(<App />);
  const filters = await screen.findByRole('form', { name: '사용자 조회' });
  // 사용자 ID를 입력해 상세를 여는 칸은 없다.
  expect(screen.queryByRole('form', { name: '사용자 ID 조회' })).toBeNull();
  expect(screen.queryByLabelText('사용자 ID')).toBeNull();
  fireEvent.change(within(filters).getByLabelText('이메일 (일부만 입력해도 됨)'), { target: { value: '  bob@Example ' } });
  fireEvent.submit(filters);
  await waitFor(() => expect(reads).toContain('/api/admin/principals?email=bob%40Example'));
});

test('service_is_created_from_a_window_opened_by_a_button', async () => {
  const { posts } = stub();
  history.replaceState(null, '', '/dashboard/admin/principals'); render(<App />);
  await screen.findByRole('form', { name: '사용자 조회' });
  // 페이지에는 창을 여는 버튼만 있고 생성 칸은 없다.
  expect(screen.queryByLabelText('새 서비스 이름')).toBeNull();
  fireEvent.click(screen.getByRole('button', { name: '서비스 생성' }));
  const dialog = await screen.findByRole('dialog', { name: '서비스 생성 (초기 deny-all)' });
  fireEvent.change(within(dialog).getByLabelText('새 서비스 이름'), { target: { value: '배치' } });
  // owner는 UUID를 입력하지 않고 이름·이메일로 찾아 고른다.
  const owner = within(dialog).getByLabelText('새 서비스 owner');
  await waitFor(() => expect(optionNames(owner)).toEqual(['관리자 (admin@example.test)']));
  pickOption(owner, '관리자 (admin@example.test)');
  fireEvent.submit(within(dialog).getByRole('form', { name: '서비스 생성' }));
  // 입력 창이 닫히고 확인 창이 뜬다. 확인해야 보낸다.
  const confirm = await screen.findByRole('dialog', { name: 'deny-all 서비스 생성 확인' });
  expect(posts).toHaveLength(0);
  fireEvent.click(within(confirm).getByRole('button', { name: '명령 실행' }));
  await waitFor(() => expect(posts).toEqual([{ display_name: '배치', management_owner_id: 'admin1' }]));
  await waitFor(() => expect(location.pathname + location.search).toBe('/dashboard/admin/principals/detail?principal_id=service9'));
});

test('principal_list_is_a_table_and_all_keys_live_on_their_own_page', async () => {
  const person = { id: 'human1', kind: 'human', display_name: '가람', email: 'garam@example.test', management_owner_id: null, version: '2', policy_version: '4', disabled: false, inference_safety: { paused: false, version: '1', reason: null, paused_at: null, trigger_request_id: null }, block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' } };
  const key = { provider_credentials: [], id: 'k1', principal_id: 'human1', name: '가람 키', key_hint: 'mgw_v1_k1', created_at: '2026-11-01T00:00:00Z', created_by: 'human1', created_command_id: 'c1', validity: '1_year', expires_at: '2027-11-01T00:00:00Z', revoked_at: null, version: '1', grants: [], limits: { budget: null, rpm: null, tpm: null, count_rpm: null } };
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL) => {
    const url = String(input);
    if (url === '/api/me') return Promise.resolve(response(admin));
    if (url.startsWith('/api/admin/principals')) return Promise.resolve(response({ items: [person], next_cursor: null, as_of: '2026-11-02T00:00:00Z' }));
    if (url.startsWith('/api/admin/keys')) return Promise.resolve(response({ items: [key], next_cursor: null, as_of: '2026-11-02T00:00:00Z' }));
    return Promise.resolve(response({ providers: [] }));
  }));
  history.replaceState(null, '', '/dashboard/admin/principals'); render(<App />);
  const row = (await screen.findByRole('link', { name: '가람 상세' })).closest('tr')!;
  expect(within(row).getByText('garam@example.test')).toBeTruthy();
  expect(within(row).getByText('2 / 4 / 1')).toBeTruthy();
  expect(screen.getAllByRole('columnheader').map(h => h.textContent)).toEqual(['이름', '종류', '이메일', '상태', '버전 (사용자 / 정책 / 안전)', 'owner', '동작']);
  // 사용자 목록 페이지에는 가상 키 목록이 없다. 메뉴의 가상 키 관리 페이지에 있다.
  expect(screen.queryByRole('button', { name: '키 조회' })).toBeNull();
  fireEvent.click(screen.getByRole('link', { name: '가상 키 관리' }));
  const keyRow = (await screen.findByRole('button', { name: '가람 키 회수' })).closest('tr')!;
  expect(location.pathname).toBe('/dashboard/admin/keys');
  expect(within(keyRow).getByRole('link', { name: 'human1' }).getAttribute('href')).toBe('/dashboard/admin/principals/detail?principal_id=human1');
  expect(within(keyRow).getByText('활성')).toBeTruthy();
  expect(screen.getAllByRole('columnheader').map(h => h.textContent)).toEqual(['이름', '키 주인', '상태', '유효기간', '생성', '만료', 'Provider 키', '동작']);
});
