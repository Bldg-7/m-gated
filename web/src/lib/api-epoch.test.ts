import { expect, test, vi } from 'vitest';
import { Session, StaleResponse } from './api';
import type { Me } from './types';

const me: Me = { principal: { id: 'human1', display_name: 'Synthetic', email: 'synthetic@example.invalid', block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' } }, role: 'user', grants: [], auth_expires_at: '2030-01-01T00:00:00Z', sso: { refresh_url: 'https://auth.example.test/sso/refresh', logout_url: 'https://auth.example.test/sso/logout' } };
const oldMe: Me = { ...me, principal: { ...me.principal, id: 'stale-human' } };
const newMe: Me = { ...me, principal: { ...me.principal, id: 'new-human' } };
const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status });
function deferred<T>() { let resolve!: (value: T) => void; const promise = new Promise<T>(r => { resolve = r; }); return { promise, resolve }; }

test.each((['validate', 'refresh'] as const).flatMap(path =>
  (['local-logout', 'remote-logout', 'auth-change'] as const).map(boundary => ({ path, boundary })),
))('final_me_accept_checks_epoch_after_json_[$path]_[$boundary]', async ({ path, boundary }) => {
  const sent: unknown[] = [];
  let channel!: Channel;
  class Channel {
    onmessage: ((event: { data: { type: string } }) => void) | null = null;
    constructor() { channel = this; }
    postMessage(data: unknown) { sent.push(data); }
    close() {}
  }
  vi.stubGlobal('BroadcastChannel', Channel);
  const body = deferred<Me>(), enteredJson = deferred<void>();
  const held = response(null);
  vi.spyOn(held, 'json').mockImplementation(() => { enteredJson.resolve(); return body.promise; });
  const replacement = deferred<Response>();
  const fetcher = vi.fn().mockResolvedValueOnce(response(me));
  if (path === 'refresh') fetcher.mockResolvedValueOnce(response({ error: { code: 'expired' } }, 401)).mockResolvedValueOnce(response({}));
  fetcher.mockResolvedValueOnce(held).mockReturnValueOnce(replacement.promise);
  const nav = vi.fn(), session = new Session(fetcher, nav);
  await session.validate();
  const detach = session.attach();
  const readyIdentities: string[] = [];
  const unsubscribe = session.subscribe(() => {
    const state = session.snapshot();
    if (state.status === 'ready' && state.me) readyIdentities.push(state.me.principal.id);
  });
  try {
    const operation = path === 'validate' ? session.validate() : session.read('/gateway/usage');
    // refresh의 stale 거부도 즉시 관찰해 unhandled rejection을 만들지 않는다.
    const settled = operation.then(() => null, error => error);
    await enteredJson.promise;
    body.resolve(oldMe);
    // json의 최종 검사와 외부 await 재개 사이에만 epoch를 바꾼다.
    queueMicrotask(() => {
      if (boundary === 'local-logout') session.logout();
      else channel.onmessage!({ data: { type: boundary === 'remote-logout' ? 'logout' : 'auth-change' } });
    });
    const error = await settled;
    const beforeReplacement = session.snapshot();
    expect.soft(beforeReplacement.me, `${path}/${boundary}: 이전 me 수용 금지`).toBeNull();
    expect.soft(readyIdentities, `${path}/${boundary}: 이전 ready 통지 금지`).toEqual([]);
    expect.soft(sent, `${path}/${boundary}: 이전 auth-change 발행 금지`).toEqual(boundary === 'local-logout' ? [{ type: 'logout' }] : []);
    expect.soft(fetcher.mock.calls.filter(c => c[0] === '/gateway/usage'), `${path}/${boundary}: 이전 읽기 retry 금지`).toHaveLength(path === 'refresh' ? 1 : 0);
    if (path === 'refresh') expect.soft(error).toBeInstanceOf(StaleResponse);
    if (boundary === 'auth-change') {
      expect.soft(beforeReplacement.status).toBe('checking');
      replacement.resolve(response(newMe));
      await session.validate('remote');
      expect.soft(session.snapshot().me?.principal.id).toBe('new-human');
      expect.soft(readyIdentities).toEqual(['new-human']);
      expect.soft(sent).toEqual([]);
    } else {
      expect.soft(beforeReplacement).toMatchObject({ status: 'error', error: '로그아웃되었습니다.' });
      const calls = fetcher.mock.calls.length;
      await session.validate('automatic');
      await session.validate('remote');
      expect.soft(fetcher).toHaveBeenCalledTimes(calls);
      expect(session.snapshot().me).toBeNull();
      await expect(session.read('/gateway/usage')).rejects.toBeInstanceOf(StaleResponse);
      replacement.resolve(response(me));
      await session.validate();
      expect.soft(session.snapshot().me?.principal.id).toBe('human1');
      // stale가 verifiedPrincipal까지 바꾸지 않아 명시 복구도 가짜 identity change를 발행하지 않는다.
      expect.soft(sent).toEqual(boundary === 'local-logout' ? [{ type: 'logout' }] : []);
    }
  } finally { unsubscribe(); detach(); vi.unstubAllGlobals(); }
});

test.each((['initial', 'retry'] as const).flatMap(path => [401, 403].flatMap(status =>
  (['local-logout', 'remote-logout', 'auth-change'] as const).map(boundary => ({ path, status, boundary })),
)))('stale_read_error_has_no_auth_effect_[$path]_[$status]_[$boundary]', async ({ path, status, boundary }) => {
  const sent: unknown[] = [];
  let channel!: Channel;
  class Channel {
    onmessage: ((event: { data: { type: string } }) => void) | null = null;
    constructor() { channel = this; }
    postMessage(data: unknown) { sent.push(data); }
    close() {}
  }
  vi.stubGlobal('BroadcastChannel', Channel);
  const body = deferred<unknown>(), enteredJson = deferred<void>(), replacement = deferred<Response>();
  const held = response(null, status);
  vi.spyOn(held, 'json').mockImplementation(() => { enteredJson.resolve(); return body.promise; });
  let meReads = 0, reads = 0;
  const fetcher = vi.fn((input: RequestInfo | URL) => {
    const url = String(input);
    if (url === '/gateway/csrf') return Promise.resolve(response({ token: 'synthetic-csrf', expires_at: '2030-01-01T00:00:00Z' }));
    if (url === '/gateway/me') {
      meReads++;
      return boundary === 'auth-change' && meReads === (path === 'retry' ? 3 : 2)
        ? replacement.promise : Promise.resolve(response(me));
    }
    if (url === me.sso.refresh_url) return Promise.resolve(response({}));
    reads++;
    if (path === 'retry' && reads === 1) return Promise.resolve(response({ error: { code: 'expired' } }, 401));
    return Promise.resolve(reads === (path === 'retry' ? 2 : 1) ? held : response({ private: 'stale' }));
  });
  const nav = vi.fn(), session = new Session(fetcher, nav);
  await session.validate();
  const detach = session.attach(), ready: string[] = [];
  const unsubscribe = session.subscribe(() => { if (session.snapshot().me) ready.push(session.snapshot().me!.principal.id); });
  try {
    const operation = session.read('/gateway/usage').then(() => null, error => error);
    await enteredJson.promise;
    ready.length = 0;
    body.resolve({ error: { code: status === 401 ? 'expired' : 'access_denied' } });
    queueMicrotask(() => {
      if (boundary === 'local-logout') session.logout();
      else channel.onmessage!({ data: { type: boundary === 'remote-logout' ? 'logout' : 'auth-change' } });
    });
    const error = await operation;
    expect.soft(error).toBeInstanceOf(StaleResponse);
    expect.soft(session.snapshot().me).toBeNull();
    expect.soft(ready).toEqual([]);
    expect.soft(sent).toEqual(boundary === 'local-logout' ? [{ type: 'logout' }] : []);
    expect.soft(nav.mock.calls).toEqual(boundary === 'local-logout' ? [[me.sso.logout_url]] : []);
    expect.soft(reads).toBe(path === 'retry' ? 2 : 1);
    expect.soft(fetcher.mock.calls.filter(c => c[0] === me.sso.refresh_url)).toHaveLength(path === 'retry' ? 1 : 0);
    if (boundary === 'auth-change') {
      expect.soft(session.snapshot().status).toBe('checking');
      replacement.resolve(response(newMe));
      await session.validate('remote');
      expect.soft(session.snapshot().me?.principal.id).toBe('new-human');
      expect.soft(ready).toEqual(['new-human']);
      expect.soft(sent).toEqual([]);
    } else {
      expect.soft(session.snapshot()).toMatchObject({ status: 'error', error: '로그아웃되었습니다.' });
      const calls = fetcher.mock.calls.length;
      await session.validate('automatic'); await session.validate('remote');
      expect.soft(fetcher).toHaveBeenCalledTimes(calls);
      replacement.resolve(response(me));
      await session.validate();
      expect.soft(session.snapshot().me?.principal.id).toBe('human1');
    }
  } finally { unsubscribe(); detach(); vi.unstubAllGlobals(); }
});

test.each(['remote-logout', 'auth-change'] as const)('superseded_refresh_cannot_own_late_read_%s', async boundary => {
  let channel!: Channel;
  class Channel {
    onmessage: ((event: { data: { type: string } }) => void) | null = null;
    constructor() { channel = this; }
    postMessage() {}
    close() {}
  }
  vi.stubGlobal('BroadcastChannel', Channel);
  const refresh = deferred<Response>(), enteredRefresh = deferred<void>(), late = deferred<Response>();
  let meReads = 0, reads = 0;
  const fetcher = vi.fn((input: RequestInfo | URL) => {
    const url = String(input);
    if (url === '/gateway/csrf') return Promise.resolve(response({ token: 'synthetic-csrf', expires_at: '2030-01-01T00:00:00Z' }));
    if (url === '/gateway/me') return Promise.resolve(response(++meReads === 1 ? me : newMe));
    if (url === me.sso.refresh_url) { enteredRefresh.resolve(); return refresh.promise; }
    return ++reads === 1 ? Promise.resolve(response({ error: { code: 'expired' } }, 401)) : late.promise;
  });
  const nav = vi.fn(), session = new Session(fetcher, nav);
  await session.validate(); const detach = session.attach();
  try {
    const first = session.read('/gateway/usage?a').then(() => null, error => error);
    const second = session.read('/gateway/usage?b').then(() => null, error => error);
    await enteredRefresh.promise;
    channel.onmessage!({ data: { type: boundary === 'remote-logout' ? 'logout' : 'auth-change' } });
    if (boundary === 'auth-change') await session.validate('remote');
    late.resolve(response({ error: { code: 'expired' } }, 401));
    refresh.resolve(response({}));
    expect(await first).toBeInstanceOf(StaleResponse);
    expect(await second).toBeInstanceOf(StaleResponse);
    expect(nav).not.toHaveBeenCalled();
    expect(reads).toBe(2);
    expect(fetcher.mock.calls.filter(c => c[0] === me.sso.refresh_url)).toHaveLength(1);
    if (boundary === 'remote-logout') expect(session.snapshot()).toMatchObject({ me: null, status: 'error', error: '로그아웃되었습니다.' });
    else expect(session.snapshot()).toMatchObject({ me: { principal: { id: 'new-human' } }, status: 'ready' });
  } finally { detach(); vi.unstubAllGlobals(); }
});

test('current_refresh_owns_concurrent_reads_and_each_retries_only_once', async () => {
  const a = deferred<Response>(), b = deferred<Response>(), refresh = deferred<Response>(), enteredRefresh = deferred<void>();
  const counts = new Map<string, number>();
  const fetcher = vi.fn((input: RequestInfo | URL) => {
    const url = String(input);
    if (url === '/gateway/csrf') return Promise.resolve(response({ token: 'synthetic-csrf', expires_at: '2030-01-01T00:00:00Z' }));
    if (url === '/gateway/me') return Promise.resolve(response(me));
    if (url === me.sso.refresh_url) { enteredRefresh.resolve(); return refresh.promise; }
    const count = (counts.get(url) ?? 0) + 1; counts.set(url, count);
    return count === 1 ? (url.endsWith('?a') ? a.promise : b.promise) : Promise.resolve(response({ value: url }));
  });
  const session = new Session(fetcher, vi.fn()); await session.validate();
  const first = session.read('/gateway/usage?a'), second = session.read('/gateway/usage?b');
  a.resolve(response({ error: { code: 'expired' } }, 401));
  await enteredRefresh.promise;
  // 이전 epoch 오류는 진행 중인 refresh가 현재도 소유할 때만 합류한다.
  b.resolve(response({ error: { code: 'expired' } }, 401));
  refresh.resolve(response({}));
  expect(await Promise.all([first, second])).toEqual([{ value: '/gateway/usage?a' }, { value: '/gateway/usage?b' }]);
  expect([...counts.values()]).toEqual([2, 2]);
  expect(fetcher.mock.calls.filter(c => c[0] === me.sso.refresh_url)).toHaveLength(1);
});

test('new_epoch_refresh_does_not_join_or_get_cleared_by_superseded_flight', async () => {
  const oldRefresh = deferred<Response>(), newRefresh = deferred<Response>();
  const oldEntered = deferred<void>(), newEntered = deferred<void>();
  let refreshes = 0;
  const counts = new Map<string, number>();
  const fetcher = vi.fn((input: RequestInfo | URL) => {
    const url = String(input);
    if (url === '/gateway/csrf') return Promise.resolve(response({ token: 'synthetic-csrf', expires_at: '2030-01-01T00:00:00Z' }));
    if (url === '/gateway/me') return Promise.resolve(response(me));
    if (url === me.sso.refresh_url) {
      if (++refreshes === 1) { oldEntered.resolve(); return oldRefresh.promise; }
      newEntered.resolve(); return newRefresh.promise;
    }
    const count = (counts.get(url) ?? 0) + 1; counts.set(url, count);
    return Promise.resolve(response(count === 1 ? { error: { code: 'expired' } } : { value: url }, count === 1 ? 401 : 200));
  });
  const session = new Session(fetcher, vi.fn()); await session.validate();
  const old = session.read('/gateway/usage?old').then(() => null, error => error);
  await oldEntered.promise;
  await session.validate();
  const current = session.read('/gateway/usage?current');
  await newEntered.promise;
  oldRefresh.resolve(response({}));
  expect(await old).toBeInstanceOf(StaleResponse);
  expect(session.snapshot()).toMatchObject({ me: null, status: 'checking', refreshing: true });
  newRefresh.resolve(response({}));
  expect(await current).toEqual({ value: '/gateway/usage?current' });
  expect(refreshes).toBe(2);
  expect([...counts.values()]).toEqual([1, 2]);
  expect(session.snapshot()).toMatchObject({ me: { principal: { id: 'human1' } }, status: 'ready' });
});

test.each((['initial', 'retry'] as const).flatMap(path =>
  (['local-logout', 'remote-logout', 'auth-change'] as const).map(boundary => ({ path, boundary })),
))('successful_read_checks_owner_before_final_return_[$path]_[$boundary]', async ({ path, boundary }) => {
  const sent: unknown[] = [];
  let channel!: Channel;
  class Channel {
    onmessage: ((event: { data: { type: string } }) => void) | null = null;
    constructor() { channel = this; }
    postMessage(data: unknown) { sent.push(data); }
    close() {}
  }
  vi.stubGlobal('BroadcastChannel', Channel);
  const body = deferred<unknown>(), enteredJson = deferred<void>(), replacement = deferred<Response>();
  const held = response(null);
  vi.spyOn(held, 'json').mockImplementation(() => { enteredJson.resolve(); return body.promise; });
  let meReads = 0, reads = 0, fulfilled = 0;
  const fetcher = vi.fn((input: RequestInfo | URL) => {
    const url = String(input);
    if (url === '/gateway/csrf') return Promise.resolve(response({ token: 'synthetic-csrf', expires_at: '2030-01-01T00:00:00Z' }));
    if (url === '/gateway/me') {
      meReads++;
      return boundary === 'auth-change' && meReads === (path === 'retry' ? 3 : 2)
        ? replacement.promise : Promise.resolve(response(me));
    }
    if (url === me.sso.refresh_url) return Promise.resolve(response({}));
    reads++;
    if (path === 'retry' && reads === 1) return Promise.resolve(response({ error: { code: 'expired' } }, 401));
    return Promise.resolve(held);
  });
  const nav = vi.fn(), session = new Session(fetcher, nav);
  await session.validate(); const detach = session.attach(), ready: string[] = [];
  const unsubscribe = session.subscribe(() => { if (session.snapshot().me) ready.push(session.snapshot().me!.principal.id); });
  try {
    const operation = session.read('/gateway/usage').then(() => { fulfilled++; return null; }, error => error);
    await enteredJson.promise;
    ready.length = 0;
    body.resolve({ private: 'old-usage' });
    // json 내부 검사가 통과한 뒤 read가 재개되기 전 인증 소유권을 바꾼다.
    queueMicrotask(() => {
      if (boundary === 'local-logout') session.logout();
      else channel.onmessage!({ data: { type: boundary === 'remote-logout' ? 'logout' : 'auth-change' } });
    });
    expect.soft(await operation).toBeInstanceOf(StaleResponse);
    expect.soft(fulfilled).toBe(0);
    expect.soft(session.snapshot().me).toBeNull();
    expect.soft(ready).toEqual([]);
    expect.soft(sent).toEqual(boundary === 'local-logout' ? [{ type: 'logout' }] : []);
    expect.soft(nav.mock.calls).toEqual(boundary === 'local-logout' ? [[me.sso.logout_url]] : []);
    expect.soft(reads).toBe(path === 'retry' ? 2 : 1);
    expect.soft(fetcher.mock.calls.filter(c => c[0] === me.sso.refresh_url)).toHaveLength(path === 'retry' ? 1 : 0);
    if (boundary === 'auth-change') {
      expect.soft(session.snapshot().status).toBe('checking');
      replacement.resolve(response(newMe));
      await session.validate('remote');
      expect.soft(session.snapshot().me?.principal.id).toBe('new-human');
      expect.soft(ready).toEqual(['new-human']);
      expect.soft(sent).toEqual([]);
    } else {
      expect.soft(session.snapshot()).toMatchObject({ status: 'error', error: '로그아웃되었습니다.' });
      const calls = fetcher.mock.calls.length;
      await session.validate('automatic'); await session.validate('remote');
      expect.soft(fetcher).toHaveBeenCalledTimes(calls);
      await session.validate();
      expect.soft(session.snapshot().me?.principal.id).toBe('human1');
      expect.soft(sent).toEqual(boundary === 'local-logout' ? [{ type: 'logout' }] : []);
    }
  } finally { unsubscribe(); detach(); vi.unstubAllGlobals(); }
});
