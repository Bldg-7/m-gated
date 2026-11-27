import { expect, test, vi } from 'vitest';
import { ApiError, Session } from './api';
import type { Me } from './types';

// 비밀번호 방식(결정 0011)의 Session 동작. 토큰 갱신 주소가 없고, 로그아웃은 POST /auth/logout 뒤 로그인 화면으로 간다.
const me: Me = { principal: { id: 'human1', display_name: 'Synthetic', email: 'a@example.test', block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' } }, role: 'user', grants: [], auth_expires_at: '2030-01-01T00:00:00Z', auth: { mode: 'password', refresh_url: null, logout_url: '/dashboard/signin' } };
const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });

test('password_mode_logout_posts_auth_logout_and_then_goes_to_the_sign_in_screen', async () => {
  const order: string[] = [];
  const fetcher = vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    order.push(`${init?.method ?? 'GET'} ${String(input)}`);
    return Promise.resolve(String(input) === '/api/me' ? response(me) : new Response(null, { status: 204 }));
  });
  const nav = vi.fn((url: string) => { order.push(`navigate ${url}`); });
  const session = new Session(fetcher as typeof fetch, nav);
  await session.validate();
  await session.logout();
  // 세션을 끝낸 뒤에 이동한다. 쿠키를 지우도록 같은 origin 자격 증명을 보낸다.
  expect(order).toEqual(['GET /api/me', 'POST /auth/logout', 'navigate /dashboard/signin']);
  expect(fetcher.mock.calls[1][1]).toMatchObject({ method: 'POST', credentials: 'same-origin' });
  expect(session.snapshot().me).toBeNull();
});

test('password_mode_logout_still_goes_to_the_sign_in_screen_when_the_server_is_unreachable', async () => {
  const fetcher = vi.fn().mockResolvedValueOnce(response(me)).mockRejectedValueOnce(new TypeError('offline'));
  const nav = vi.fn();
  const session = new Session(fetcher, nav);
  await session.validate();
  await session.logout();
  expect(nav).toHaveBeenCalledWith('/dashboard/signin');
});

test('password_mode_401_read_never_refreshes_and_goes_to_auth_login', async () => {
  const fetcher = vi.fn().mockResolvedValueOnce(response(me)).mockResolvedValueOnce(response({ error: { code: 'unauthenticated' } }, 401));
  const nav = vi.fn();
  const session = new Session(fetcher, nav);
  await session.validate();
  await expect(session.read('/api/usage')).rejects.toBeInstanceOf(ApiError);
  // 갱신 주소가 없으므로 다른 요청을 보내지 않는다. /auth/login이 로그인 화면으로 보낸다.
  expect(fetcher.mock.calls.map(c => String(c[0]))).toEqual(['/api/me', '/api/usage']);
  expect(nav).toHaveBeenCalledWith('/auth/login');
  expect(session.snapshot().me).toBeNull();
});
