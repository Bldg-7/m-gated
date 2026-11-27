import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../../app';
import { session } from '../../lib/api';
import { optionNames, pickOption } from '../../test-select';

const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
const admin = { principal: { id: 'admin1', display_name: '관리자', email: 'admin@example.test', block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' } }, role: 'admin', grants: [], provider_credentials: [], auth: { mode: 'sso', refresh_url: 'https://auth.example.test/sso/refresh', logout_url: 'https://auth.example.test/sso/logout' } };
afterEach(() => { cleanup(); session.clear(); history.replaceState(null, '', '/dashboard/'); vi.unstubAllGlobals(); });

test('request_filter_picks_user_and_key_by_name_and_sends_local_time_as_utc', async () => {
  const reads: string[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL) => {
    const url = String(input); reads.push(url);
    if (url === '/api/me') return Promise.resolve(response(admin));
    if (url === '/api/admin/principals?limit=100') return Promise.resolve(response({ items: [{ id: 'human1', kind: 'human', display_name: '가람', email: 'garam@example.test' }, { id: 'service1', kind: 'service', display_name: '배치', email: null }], next_cursor: null, as_of: 'now' }));
    if (url === '/api/admin/keys?principal_id=human1&limit=100') return Promise.resolve(response({ items: [{ id: 'key1', name: '가람 키', key_hint: 'mgw_v1_k1' }], next_cursor: null, as_of: 'now' }));
    return Promise.resolve(response({ items: [], next_cursor: null, as_of: '2026-11-01T00:00:00Z' }));
  }));
  history.replaceState(null, '', '/dashboard/admin/requests'); render(<App />);
  const form = await screen.findByRole('form', { name: '요청 조회' });
  // UUID나 RFC3339 문자열을 직접 넣는 칸은 없다.
  expect(within(form).queryByLabelText(/ID$/)).toBeNull();
  expect(within(form).queryByLabelText(/RFC3339/)).toBeNull();
  const user = within(form).getByLabelText('사용자');
  await waitFor(() => expect(optionNames(user)).toEqual(['전체', '가람 (garam@example.test)', '배치 (서비스)']));
  pickOption(user, '가람 (garam@example.test)');
  // 사용자를 고르면 가상 키 후보가 그 사용자의 키로 좁혀진다.
  await waitFor(() => expect(optionNames(within(form).getByLabelText('가상 키'))).toEqual(['전체', '가람 키 (mgw_v1_k1)']));
  pickOption(within(form).getByLabelText('가상 키'), '가람 키 (mgw_v1_k1)');
  fireEvent.change(within(form).getByLabelText('시작 (현지 시각)'), { target: { value: '2026-11-01T09:00' } });
  fireEvent.submit(form);
  const from = encodeURIComponent(new Date('2026-11-01T09:00').toISOString());
  await waitFor(() => expect(reads.some(url => url.startsWith('/api/admin/requests?') && url.includes('principal_id=human1') && url.includes('key_id=key1') && url.includes(`from=${from}`) && !url.includes('to='))).toBe(true));
});
