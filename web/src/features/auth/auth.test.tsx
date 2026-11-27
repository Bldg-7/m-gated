import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../../app';
import { session } from '../../lib/api';

// 비밀번호 방식(결정 0011)의 로그인 화면, 링크로 비밀번호를 정하는 화면, 자기 비밀번호 바꾸기.
const response = (body: unknown, status = 200, headers: Record<string, string> = {}) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json', ...headers } });
const empty = () => new Response(null, { status: 204 });
const me = { principal: { id: 'human1', display_name: '가람', email: 'garam@example.test', block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' } }, role: 'user', grants: [], auth: { mode: 'password', refresh_url: null, logout_url: '/dashboard/signin' } };
const usage = { currency: 'USD', from: '2026-01-01T00:00:00Z', to: '2026-02-01T00:00:00Z', as_of: '2026-02-01T00:00:00Z', detail_available_from: null, aggregation: 'detail', settled_usd: '0.000000000000', period_unknown_count: '0', global_unknown_count: '0', global_expired_dispatch_count: '0', block_state: null, balance_context: 'current_principal', budget_balances: [], buckets: { items: [], next_cursor: null, as_of: '2026-02-01T00:00:00Z' } };
afterEach(() => { cleanup(); session.clear(); history.replaceState(null, '', '/dashboard/'); vi.unstubAllGlobals(); });

type Call = { url: string; method: string; body: unknown; init?: RequestInit };
/**
 * auth: /auth/* 와 POST /api/me/password 에 답하는 함수. 그 밖의 읽기는 user(기본은 비밀번호 방식의 me)와 빈 사용량으로 답한다.
 */
function stub(auth: (url: string, body: Record<string, unknown>) => Response, user: unknown = me) {
  const calls: Call[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input), method = init?.method ?? 'GET', body = typeof init?.body === 'string' ? JSON.parse(init.body) : undefined;
    calls.push({ url, method, body, init });
    if (method === 'POST' && (url.startsWith('/auth/') || url === '/api/me/password')) return Promise.resolve(auth(url, body ?? {}));
    if (url === '/api/me') return Promise.resolve(response(user));
    if (url === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (url.startsWith('/api/usage')) return Promise.resolve(response(url === '/api/usage/models' ? { items: [] } : usage));
    return Promise.resolve(response({ items: [], next_cursor: null, as_of: '2026-02-01T00:00:00Z' }));
  }));
  return calls;
}
function signIn(email: string, password: string) {
  const form = screen.getByRole('form', { name: '로그인' });
  fireEvent.change(within(form).getByLabelText('이메일'), { target: { value: email } });
  fireEvent.change(within(form).getByLabelText('비밀번호'), { target: { value: password } });
  fireEvent.submit(form);
}

test('sign_in_screen_shows_without_checking_the_session_and_goes_to_the_dashboard_on_success', async () => {
  const calls = stub(() => empty());
  history.replaceState(null, '', '/dashboard/signin'); render(<App />);
  expect(screen.getByRole('heading', { level: 1, name: '로그인' })).toBeTruthy();
  // 로그인 전 화면은 사이드바가 없고, 로그인 상태를 묻지 않는다.
  expect(screen.queryByRole('navigation')).toBeNull();
  expect(screen.getByText('비밀번호를 잊었으면 관리자에게 재설정 링크를 요청하세요.')).toBeTruthy();
  expect(calls).toEqual([]);
  signIn(' garam@example.test ', 'correct horse battery');
  await waitFor(() => expect(location.pathname).toBe('/dashboard/'));
  expect(calls[0]).toMatchObject({ url: '/auth/password', method: 'POST', body: { email: 'garam@example.test', password: 'correct horse battery' } });
  expect(calls[0].init).toMatchObject({ credentials: 'same-origin' });
  // 대시보드로 가면 그때 로그인 상태를 확인한다.
  expect(await screen.findByRole('region', { name: '로그인 사용자' })).toBeTruthy();
  expect(calls.some(c => c.url === '/api/me')).toBe(true);
});

test('sign_in_failures_are_told_in_plain_words_and_the_lock_says_when_to_try_again', async () => {
  let reply = () => response({ error: { code: 'invalid_credentials' } }, 401);
  stub(() => reply());
  history.replaceState(null, '', '/dashboard/signin'); render(<App />);
  signIn('garam@example.test', 'wrong password!');
  expect((await screen.findByRole('alert')).textContent).toBe('이메일 또는 비밀번호가 틀렸습니다.');
  expect(location.pathname).toBe('/dashboard/signin');

  // 429는 Retry-After(초)를 분으로 올려 알린다.
  reply = () => response({ error: { code: 'sign_in_locked' } }, 429, { 'Retry-After': '840' });
  signIn('garam@example.test', 'wrong password!');
  await waitFor(() => expect(screen.getByRole('alert').textContent).toBe('로그인에 여러 번 실패해 잠시 막혔습니다. 14분 뒤 다시 시도하세요.'));

  reply = () => response({ error: { code: 'access_denied' } }, 403);
  signIn('garam@example.test', 'correct horse battery');
  await waitFor(() => expect(screen.getByRole('alert').textContent).toBe('이 계정은 로그인할 수 없습니다. 관리자에게 문의하세요.'));
});

const invitation = { email: 'new@example.test', kind: 'invitation', password_set: false, expires_at: '2030-01-08T00:00:00Z' };

test('invitation_link_asks_for_a_display_name_and_both_passwords_must_match', async () => {
  const calls = stub(url => url === '/auth/password/link' ? response(invitation) : empty());
  history.replaceState(null, '', '/dashboard/password#token=tok-1'); render(<App />);
  expect(await screen.findByRole('heading', { level: 1, name: '초대 수락' })).toBeTruthy();
  expect(calls[0]).toMatchObject({ url: '/auth/password/link', method: 'POST', body: { token: 'tok-1' } });
  const form = screen.getByRole('form', { name: '비밀번호 정하기' });
  expect((within(form).getByLabelText('이메일') as HTMLInputElement).value).toBe('new@example.test');
  fireEvent.change(within(form).getByLabelText('표시 이름'), { target: { value: ' 새 사람 ' } });
  fireEvent.change(within(form).getByLabelText('새 비밀번호'), { target: { value: 'twelve chars!' } });
  fireEvent.change(within(form).getByLabelText('새 비밀번호 확인'), { target: { value: 'twelve chars?' } });
  fireEvent.submit(form);
  // 두 칸이 다르면 보내지 않는다.
  expect(screen.getByRole('alert').textContent).toBe('두 비밀번호가 다릅니다.');
  // 짧은 비밀번호도 보내지 않는다.
  fireEvent.change(within(form).getByLabelText('새 비밀번호'), { target: { value: 'short' } });
  fireEvent.change(within(form).getByLabelText('새 비밀번호 확인'), { target: { value: 'short' } });
  fireEvent.submit(form);
  expect(screen.getByRole('alert').textContent).toBe('비밀번호는 12자 이상 128자 이하여야 합니다.');
  expect(calls.filter(c => c.url === '/auth/password/set')).toEqual([]);

  fireEvent.change(within(form).getByLabelText('새 비밀번호'), { target: { value: 'twelve chars!' } });
  fireEvent.change(within(form).getByLabelText('새 비밀번호 확인'), { target: { value: 'twelve chars!' } });
  fireEvent.submit(form);
  // 성공하면 토큰을 주소에서 지우고 대시보드로 간다.
  await waitFor(() => expect(location.pathname).toBe('/dashboard/'));
  expect(location.hash).toBe('');
  expect(calls.find(c => c.url === '/auth/password/set')).toMatchObject({ method: 'POST', body: { token: 'tok-1', password: 'twelve chars!', display_name: '새 사람' } });
});

test('reset_link_does_not_ask_for_a_name_and_an_used_link_is_explained', async () => {
  let set = () => empty();
  const calls = stub(url => url === '/auth/password/link' ? response({ ...invitation, kind: 'reset', password_set: true }) : set());
  history.replaceState(null, '', '/dashboard/password#token=tok-2'); render(<App />);
  expect(await screen.findByRole('heading', { level: 1, name: '비밀번호 재설정' })).toBeTruthy();
  expect(screen.queryByLabelText('표시 이름')).toBeNull();
  // 그사이 링크가 끝나면 설정 요청이 link_invalid로 거절된다.
  set = () => response({ error: { code: 'link_invalid' } }, 404);
  fireEvent.change(screen.getByLabelText('새 비밀번호'), { target: { value: 'another password' } });
  fireEvent.change(screen.getByLabelText('새 비밀번호 확인'), { target: { value: 'another password' } });
  fireEvent.submit(screen.getByRole('form', { name: '비밀번호 정하기' }));
  expect((await screen.findByRole('alert')).textContent).toBe('이 링크는 기한이 지났거나 이미 쓰였습니다. 관리자에게 새 링크를 요청하세요.');
  expect(calls.find(c => c.url === '/auth/password/set')?.body).toEqual({ token: 'tok-2', password: 'another password' });
  expect(location.pathname).toBe('/dashboard/password');
});

test('expired_or_missing_link_token_shows_that_the_link_ended', async () => {
  stub(() => response({ error: { code: 'link_invalid' } }, 404));
  history.replaceState(null, '', '/dashboard/password#token=gone'); const view = render(<App />);
  expect((await screen.findByRole('alert')).textContent).toBe('이 링크는 기한이 지났거나 이미 쓰였습니다. 관리자에게 새 링크를 요청하세요.');
  expect(screen.getByRole('link', { name: '로그인 화면으로' }).getAttribute('href')).toBe('/dashboard/signin');
  view.unmount();
  // 토큰이 없으면 서버에 묻지 않는다.
  const calls = stub(() => response(invitation));
  history.replaceState(null, '', '/dashboard/password'); render(<App />);
  expect(await screen.findByRole('alert')).toBeTruthy();
  expect(calls).toEqual([]);
});

test('own_password_is_changed_in_a_window_and_then_the_sign_in_screen_says_to_sign_in_again', async () => {
  let change = () => response({ error: { code: 'current_password_invalid' } }, 400);
  const calls = stub(() => change());
  render(<App />);
  const identity = await screen.findByRole('region', { name: '로그인 사용자' });
  fireEvent.click(within(identity).getByRole('button', { name: '비밀번호 바꾸기' }));
  const dialog = screen.getByRole('dialog', { name: '비밀번호 바꾸기' });
  const fill = (current: string, next: string, confirm: string) => {
    fireEvent.change(within(dialog).getByLabelText('지금 비밀번호'), { target: { value: current } });
    fireEvent.change(within(dialog).getByLabelText('새 비밀번호'), { target: { value: next } });
    fireEvent.change(within(dialog).getByLabelText('새 비밀번호 확인'), { target: { value: confirm } });
    fireEvent.submit(within(dialog).getByRole('form', { name: '비밀번호 바꾸기' }));
  };
  fill('old password', 'new password 1', 'new password 2');
  expect(within(dialog).getByRole('alert').textContent).toBe('두 비밀번호가 다릅니다.');
  expect(calls.filter(c => c.url === '/api/me/password')).toEqual([]);

  fill('old password', 'new password 1', 'new password 1');
  await waitFor(() => expect(within(dialog).getByRole('alert').textContent).toBe('지금 비밀번호가 틀렸습니다.'));
  const sent = calls.find(c => c.url === '/api/me/password')!;
  // 쿠키 쓰기라 CSRF 토큰을 붙여 보낸다.
  expect(sent.body).toEqual({ current_password: 'old password', new_password: 'new password 1' });
  expect((sent.init?.headers as Record<string, string>)['X-CSRF-Token']).toBe('csrf');

  change = () => empty();
  fill('old password', 'new password 1', 'new password 1');
  await waitFor(() => expect(location.pathname).toBe('/dashboard/signin'));
  expect(await screen.findByText('비밀번호를 바꿨습니다. 모든 세션이 끝났습니다. 새 비밀번호로 다시 로그인하세요.')).toBeTruthy();
  expect(screen.getByRole('form', { name: '로그인' })).toBeTruthy();
});

test('sso_mode_has_no_password_change_button', async () => {
  stub(() => empty(), { ...me, auth: { mode: 'sso', refresh_url: 'https://auth.example.test/sso/refresh', logout_url: 'https://auth.example.test/sso/logout' } });
  render(<App />);
  await screen.findByRole('button', { name: '로그아웃' });
  expect(screen.queryByRole('button', { name: '비밀번호 바꾸기' })).toBeNull();
});
