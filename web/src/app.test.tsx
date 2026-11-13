import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from './app';
import { session } from './lib/api';
import type { Me, Usage } from './lib/types';
import { shownText } from './test-text';

const me: Me = { principal: { id: 'human1', display_name: 'Synthetic', email: 'synthetic@example.invalid', block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' } }, role: 'user', grants: [], auth_expires_at: '2030-01-01T00:00:00Z', sso: { refresh_url: 'https://auth.example.test/sso/refresh', logout_url: 'https://auth.example.test/sso/logout' } };
const usage: Usage = { currency: 'USD', from: '2026-01-01T00:00:00Z', to: '2026-02-01T00:00:00Z', as_of: '2026-02-01T00:00:00Z', detail_available_from: null, aggregation: 'detail', settled_usd: '0.000000000001', held_reservation_usd: '0.000000000000', period_unknown_count: '0', global_unknown_count: '0', global_expired_dispatch_count: '0', block_state: null, balance_context: 'current_principal', budget_balances: [], buckets: { items: [], next_cursor: null, as_of: '2026-02-01T00:00:00Z' } };
const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
function deferred<T>() { let resolve!: (value: T) => void; const promise = new Promise<T>(r => { resolve = r; }); return { promise, resolve }; }
afterEach(() => { cleanup(); session.clear(); vi.unstubAllGlobals(); });

test('app_refresh_retries_the_same_usage_url_once_and_preserves_explicit_queries', async () => {
  const refresh = deferred<Response>();
  const url = '/gateway/usage?model=same-human&group_by=provider';
  let reads = 0;
  const fetcher = vi.fn((input: RequestInfo | URL) => {
    const path = String(input);
    if (path === '/gateway/me') return Promise.resolve(response(me));
    if (path === me.sso.refresh_url) return refresh.promise;
    if (path === url && ++reads === 1) return Promise.resolve(response({ error: { code: 'expired' } }, 401));
    return Promise.resolve(response(usage));
  });
  vi.stubGlobal('fetch', fetcher);
  render(<App />);
  await screen.findByRole('heading', { name: '조회 기간 사용량' });
  fireEvent.change(screen.getByLabelText('모델'), { target: { value: 'same-human' } });
  fireEvent.submit(screen.getByRole('form', { name: '사용량 조회' }));
  await screen.findByText('인증 확인 중…');
  expect(screen.queryByText('Synthetic')).toBeNull();
  expect(screen.queryByRole('heading', { name: '조회 기간 사용량' })).toBeNull();
  await act(async () => { refresh.resolve(response({})); });
  await screen.findByRole('heading', { name: '조회 기간 사용량' });
  expect(reads).toBe(2);
  expect(fetcher.mock.calls.filter(c => c[0] === me.sso.refresh_url)).toHaveLength(1);
  expect((screen.getByLabelText('모델') as HTMLInputElement).value).toBe('same-human');
  fireEvent.submit(screen.getByRole('form', { name: '사용량 조회' }));
  await screen.findByRole('heading', { name: '조회 기간 사용량' });
  expect(reads).toBe(3);
});

test('app_refresh_to_another_principal_discards_the_old_query_and_payload', async () => {
  const refresh = deferred<Response>();
  const other: Me = { ...me, principal: { ...me.principal, id: 'human2', display_name: 'Second' } };
  let meReads = 0;
  const paths: string[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL) => {
    const path = String(input); paths.push(path);
    if (path === '/gateway/me') return Promise.resolve(response(++meReads === 1 ? me : other));
    if (path === me.sso.refresh_url) return refresh.promise;
    if (path.includes('old-private-filter')) return Promise.resolve(response({ error: { code: 'expired' } }, 401));
    return Promise.resolve(response(usage));
  }));
  render(<App />);
  await screen.findByRole('heading', { name: '조회 기간 사용량' });
  fireEvent.change(screen.getByLabelText('모델'), { target: { value: 'old-private-filter' } });
  fireEvent.submit(screen.getByRole('form', { name: '사용량 조회' }));
  await screen.findByText('인증 확인 중…');
  await act(async () => { refresh.resolve(response({})); });
  await screen.findByText('Second');
  await screen.findByRole('heading', { name: '조회 기간 사용량' });
  expect((screen.getByLabelText('모델') as HTMLInputElement).value).toBe('');
  expect(paths.filter(path => path.includes('old-private-filter'))).toHaveLength(1);
  expect(paths.filter(path => path === '/gateway/usage?group_by=provider')).toHaveLength(2);
});

test.each(['validate', 'refresh'] as const)('app_discards_final_me_after_microtask_remote_logout_%s', async path => {
  let channel!: Channel;
  class Channel {
    onmessage: ((event: { data: { type: string } }) => void) | null = null;
    constructor() { channel = this; }
    postMessage() {}
    close() {}
  }
  vi.stubGlobal('BroadcastChannel', Channel);
  const body = deferred<Me>(), enteredJson = deferred<void>();
  const held = response(null);
  vi.spyOn(held, 'json').mockImplementation(() => { enteredJson.resolve(); return body.promise; });
  let meReads = 0;
  const paths: string[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL) => {
    const url = String(input); paths.push(url);
    if (url === '/gateway/me') return Promise.resolve(path === 'validate' || ++meReads === 2 ? held : response(me));
    if (url === me.sso.refresh_url) return Promise.resolve(response({}));
    if (url.includes('private-query')) return Promise.resolve(response({ error: { code: 'expired' } }, 401));
    return Promise.resolve(response(usage));
  }));
  render(<App />);
  if (path === 'refresh') {
    await screen.findByRole('heading', { name: '조회 기간 사용량' });
    fireEvent.change(screen.getByLabelText('모델'), { target: { value: 'private-query' } });
    fireEvent.submit(screen.getByRole('form', { name: '사용량 조회' }));
  }
  await enteredJson.promise;
  const reads = paths.filter(url => url.startsWith('/gateway/usage')).length;
  await act(async () => {
    body.resolve(me);
    queueMicrotask(() => channel.onmessage!({ data: { type: 'logout' } }));
  });
  expect(session.snapshot()).toMatchObject({ me: null, status: 'error', error: '로그아웃되었습니다.' });
  expect(screen.queryByText('Synthetic')).toBeNull();
  expect(screen.queryByRole('heading', { name: '조회 기간 사용량' })).toBeNull();
  expect(screen.queryByText('0.000000000001')).toBeNull();
  expect(paths.filter(url => url.startsWith('/gateway/usage'))).toHaveLength(reads);
  expect(paths.filter(url => url === me.sso.refresh_url)).toHaveLength(path === 'refresh' ? 1 : 0);
});

test.each(['initial', 'retry'] as const)('app_stale_401_after_logout_never_restores_private_dom_%s', async path => {
  let channel!: Channel;
  class Channel {
    onmessage: ((event: { data: { type: string } }) => void) | null = null;
    constructor() { channel = this; }
    postMessage() {}
    close() {}
  }
  vi.stubGlobal('BroadcastChannel', Channel);
  const body = deferred<unknown>(), enteredJson = deferred<void>();
  const held = response(null, 401);
  vi.spyOn(held, 'json').mockImplementation(() => { enteredJson.resolve(); return body.promise; });
  let reads = 0;
  const paths: string[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL) => {
    const url = String(input); paths.push(url);
    if (url === '/gateway/me') return Promise.resolve(response(me));
    if (url === me.sso.refresh_url) return Promise.resolve(response({}));
    if (url.includes('private-query')) {
      reads++;
      if (path === 'retry' && reads === 1) return Promise.resolve(response({ error: { code: 'expired' } }, 401));
      if (reads === (path === 'retry' ? 2 : 1)) return Promise.resolve(held);
    }
    return Promise.resolve(response(usage));
  }));
  render(<App />);
  await screen.findByRole('heading', { name: '조회 기간 사용량' });
  fireEvent.change(screen.getByLabelText('모델'), { target: { value: 'private-query' } });
  fireEvent.submit(screen.getByRole('form', { name: '사용량 조회' }));
  await enteredJson.promise;
  const before = paths.length;
  await act(async () => {
    body.resolve({ error: { code: 'expired' } });
    queueMicrotask(() => channel.onmessage!({ data: { type: 'logout' } }));
  });
  expect(session.snapshot()).toMatchObject({ me: null, status: 'error', error: '로그아웃되었습니다.' });
  expect(screen.queryByText('Synthetic')).toBeNull();
  expect(screen.queryByRole('heading', { name: '조회 기간 사용량' })).toBeNull();
  expect(screen.queryByText('0.000000000001')).toBeNull();
  expect(paths).toHaveLength(before);
  expect(reads).toBe(path === 'retry' ? 2 : 1);
});

test.each(['initial', 'retry'] as const)('app_discards_delayed_success_after_microtask_logout_%s', async path => {
  let channel!: Channel;
  class Channel {
    onmessage: ((event: { data: { type: string } }) => void) | null = null;
    constructor() { channel = this; }
    postMessage() {}
    close() {}
  }
  vi.stubGlobal('BroadcastChannel', Channel);
  const body = deferred<Usage>(), enteredJson = deferred<void>();
  const held = response(null);
  vi.spyOn(held, 'json').mockImplementation(() => { enteredJson.resolve(); return body.promise; });
  let reads = 0;
  const paths: string[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL) => {
    const url = String(input); paths.push(url);
    if (url === '/gateway/me') return Promise.resolve(response(me));
    if (url === me.sso.refresh_url) return Promise.resolve(response({}));
    if (url.includes('private-query')) {
      reads++;
      if (path === 'retry' && reads === 1) return Promise.resolve(response({ error: { code: 'expired' } }, 401));
      return Promise.resolve(held);
    }
    return Promise.resolve(response(usage));
  }));
  render(<App />);
  await screen.findByRole('heading', { name: '조회 기간 사용량' });
  fireEvent.change(screen.getByLabelText('모델'), { target: { value: 'private-query' } });
  fireEvent.submit(screen.getByRole('form', { name: '사용량 조회' }));
  await enteredJson.promise;
  const before = paths.length;
  await act(async () => {
    body.resolve({ ...usage, settled_usd: '123.000000000001' });
    queueMicrotask(() => channel.onmessage!({ data: { type: 'logout' } }));
  });
  expect(session.snapshot()).toMatchObject({ me: null, status: 'error', error: '로그아웃되었습니다.' });
  expect(screen.queryByText('Synthetic')).toBeNull();
  expect(screen.queryByRole('heading', { name: '조회 기간 사용량' })).toBeNull();
  expect(screen.queryByText(shownText('123.000000000001 USD'))).toBeNull();
  expect(paths).toHaveLength(before);
  expect(reads).toBe(path === 'retry' ? 2 : 1);
});

test.each(['logout', 'auth-change'] as const)('app_consumer_discards_already_committed_read_after_%s', async boundary => {
  let channel!: Channel;
  class Channel {
    onmessage: ((event: { data: { type: string } }) => void) | null = null;
    constructor() { channel = this; }
    postMessage() {}
    close() {}
  }
  vi.stubGlobal('BroadcastChannel', Channel);
  const committed = deferred<void>(), consume = deferred<void>();
  const other: Me = { ...me, principal: { ...me.principal, id: 'human2', display_name: 'Second' } };
  let meReads = 0, usageReads = 0;
  const fetcher = vi.fn((input: RequestInfo | URL) => {
    if (String(input) === '/gateway/me') return Promise.resolve(response(++meReads === 1 ? me : other));
    return Promise.resolve(response({ ...usage, settled_usd: ++usageReads === 1 ? '123.000000000001' : '2.000000000002' }));
  });
  vi.stubGlobal('fetch', fetcher);
  const read = session.read.bind(session);
  let deliveries = 0;
  const delayedConsumer = vi.spyOn(session, 'read').mockImplementation(async <T,>(url: string): Promise<T> => {
    const data = await read<T>(url);
    if (++deliveries === 1) {
      // Session이 정상 반환을 완료한 뒤 화면 소비만 보류한다. API의 미래 예측 시험이 아니다.
      committed.resolve();
      await consume.promise;
    }
    return data;
  });
  try {
    render(<App />);
    await committed.promise;
    await act(async () => { channel.onmessage!({ data: { type: boundary } }); });
    if (boundary === 'logout') expect(screen.getByText('로그아웃되었습니다.')).toBeTruthy();
    else {
      await screen.findByText('Second');
      await screen.findByRole('heading', { name: '조회 기간 사용량' });
      expect((screen.getByLabelText('모델') as HTMLInputElement).value).toBe('');
    }
    const requests = fetcher.mock.calls.length;
    await act(async () => { consume.resolve(); });
    expect(screen.queryByText(shownText('123.000000000001 USD'))).toBeNull();
    expect(screen.queryByText('Synthetic')).toBeNull();
    expect(fetcher).toHaveBeenCalledTimes(requests);
    if (boundary === 'logout') expect(screen.queryByRole('heading', { name: '조회 기간 사용량' })).toBeNull();
    else expect(screen.getByText(shownText('2.000000000002 USD'))).toBeTruthy();
  } finally { consume.resolve(); delayedConsumer.mockRestore(); }
});
