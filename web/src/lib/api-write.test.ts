import { expect, test, vi } from 'vitest';
import { ApiError, Session, StaleResponse } from './api';
import type { Me } from './types';
const me: Me = { principal: { id: 'human1', display_name: 'Synthetic', email: 'a@example.test', block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' } }, role: 'user', grants: [], auth_expires_at: '2030-01-01T00:00:00Z', auth: { mode: 'sso', refresh_url: 'https://auth.example.test/sso/refresh', logout_url: 'https://auth.example.test/sso/logout' } };
const response = (body: unknown, status = 200) => new Response(status === 204 ? null : JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
const csrf = () => response({ token: 'synthetic-csrf', expires_at: '2030-01-01T00:00:00Z' });
function deferred<T>() { let resolve!: (value: T) => void; const promise = new Promise<T>(r => { resolve = r; }); return { promise, resolve }; }

test.each(['POST', 'PUT', 'PATCH', 'DELETE'] as const)('cookie_write_%s_sends_once_and_handles_empty204', async method => {
  const fetcher = vi.fn().mockResolvedValueOnce(response(me)).mockResolvedValueOnce(csrf()).mockResolvedValueOnce(response(null, 204));
  const session = new Session(fetcher, vi.fn()); await session.validate();
  expect(await session.write('/api/keys', method, 'synthetic-command', method === 'DELETE' ? undefined : { name: 'synthetic' })).toBeUndefined();
  expect(fetcher).toHaveBeenCalledTimes(3);
  const init = fetcher.mock.calls[2][1]; const headers = new Headers(init.headers);
  expect(headers.get('x-csrf-token')).toBe('synthetic-csrf'); expect(headers.get('x-gateway-command-id')).toBe('synthetic-command');
  expect(headers.get('content-type')).toBe(method === 'DELETE' ? null : 'application/json');
  expect(init).toMatchObject({ method, credentials: 'same-origin', cache: 'no-store' });
  expect(init.body).toBe(method === 'DELETE' ? undefined : '{"name":"synthetic"}');
});

test.each([401, 403, 409, 503, 'network', 'timeout'] as const)('write_failure_%s_never_refreshes_or_replays', async failure => {
  const fetcher = vi.fn().mockResolvedValueOnce(response(me)).mockResolvedValueOnce(csrf());
  if (typeof failure === 'number') fetcher.mockResolvedValueOnce(response({ error: { code: 'synthetic' } }, failure));
  else fetcher.mockRejectedValueOnce(new DOMException('synthetic', failure === 'timeout' ? 'AbortError' : 'NetworkError'));
  const navigate = vi.fn(), session = new Session(fetcher, navigate); await session.validate();
  await expect(session.write('/api/keys', 'POST', 'command', {})).rejects.toBeDefined();
  expect(fetcher.mock.calls.map(c => c[0])).toEqual(['/api/me', '/api/csrf', '/api/keys']);
  expect(navigate).not.toHaveBeenCalled();
  if (failure === 401) expect(session.snapshot().me).toBeNull();
});

test('csrf_read401_clears_identity_without_refresh_or_write', async () => {
  const fetcher = vi.fn().mockResolvedValueOnce(response(me)).mockResolvedValueOnce(response({ error: { code: 'invalid_credentials' } }, 401));
  const session = new Session(fetcher, vi.fn()); await session.validate();
  await expect(session.write('/api/keys', 'POST', 'command', {})).rejects.toBeInstanceOf(ApiError);
  expect(fetcher).toHaveBeenCalledTimes(2); expect(session.snapshot().me).toBeNull();
});

for (const step of ['csrf', 'write'] as const) for (const change of ['logout', 'revalidate'] as const) {
  test(`stale_${step}_${change}_cannot_send_or_accept_secret`, async () => {
    const late = deferred<Response>(), entered = deferred<void>();
    const fetcher = vi.fn((url: RequestInfo | URL) => {
      if (url === '/api/me') return Promise.resolve(response(me));
      if ((step === 'csrf' && url === '/api/csrf') || url === '/api/keys') { entered.resolve(); return late.promise; }
      return Promise.resolve(csrf());
    });
    const session = new Session(fetcher, vi.fn()); await session.validate();
    const pending = session.write('/api/keys', 'POST', 'command', {});
    const rejected = expect(pending).rejects.toBeInstanceOf(StaleResponse);
    await entered.promise; if (change === 'logout') session.logout(); else await session.validate();
    late.resolve(step === 'csrf' ? csrf() : response({ key: 'old-secret' }, 201)); await rejected;
    expect(fetcher.mock.calls.filter(c => c[0] === '/api/keys')).toHaveLength(step === 'csrf' ? 0 : 1);
    expect(fetcher.mock.calls.filter(c => c[0] === me.auth.refresh_url)).toHaveLength(0);
  });
}

test('final_write_json_commit_rechecks_epoch_before_returning', async () => {
  const body = deferred<unknown>(), entered = deferred<void>(), held = response(null, 201);
  vi.spyOn(held, 'json').mockImplementation(() => { entered.resolve(); return body.promise; });
  const fetcher = vi.fn().mockResolvedValueOnce(response(me)).mockResolvedValueOnce(csrf()).mockResolvedValueOnce(held);
  const session = new Session(fetcher, vi.fn()); await session.validate();
  const pending = session.write('/api/keys', 'POST', 'command', {}); const rejected = expect(pending).rejects.toBeInstanceOf(StaleResponse);
  await entered.promise; queueMicrotask(() => session.logout()); body.resolve({ key: 'old-secret' }); await rejected;
});

test('actual_write_deadline_aborts_once_without_refresh_or_replay', async () => {
  vi.useFakeTimers();
  try {
    const entered = deferred<void>();
    const fetcher = vi.fn((url: RequestInfo | URL, init?: RequestInit) => {
      if (url === '/api/me') return Promise.resolve(response(me));
      if (url === '/api/csrf') return Promise.resolve(csrf());
      entered.resolve();
      return new Promise<Response>((_, reject) => init!.signal!.addEventListener('abort', () => reject(new DOMException('synthetic deadline', 'AbortError')), { once: true }));
    });
    const session = new Session(fetcher, vi.fn()); await session.validate();
    const pending = session.write('/api/keys', 'POST', 'command', {});
    const rejected = expect(pending).rejects.toMatchObject({ name: 'AbortError' });
    await entered.promise; await vi.advanceTimersByTimeAsync(10000); await rejected;
    expect(fetcher.mock.calls.map(c => c[0])).toEqual(['/api/me', '/api/csrf', '/api/keys']);
  } finally { vi.useRealTimers(); }
});

test('read_refresh_validates_new_me_and_csrf_before_its_only_retry', async () => {
  let reads = 0;
  const paths: string[] = [];
  const fetcher = vi.fn((input: RequestInfo | URL) => {
    const path = String(input); paths.push(path);
    if (path === '/api/me') return Promise.resolve(response(me));
    if (path === '/api/csrf') return Promise.resolve(csrf());
    if (path === me.auth.refresh_url) return Promise.resolve(response({}));
    return Promise.resolve(++reads === 1 ? response({ error: { code: 'invalid_credentials' } }, 401) : response({ value: 1 }));
  });
  const session = new Session(fetcher, vi.fn()); await session.validate();
  expect(await session.read('/api/keys')).toEqual({ value: 1 });
  expect(paths).toEqual(['/api/me', '/api/keys', me.auth.refresh_url, '/api/me', '/api/csrf', '/api/keys']);
});

for (const boundary of ['logout', 'validate'] as const) test(`refresh_csrf_completion_is_epoch_bound_${boundary}`, async () => {
  const late = deferred<Response>(), entered = deferred<void>();
  const paths: string[] = [];
  const fetcher = vi.fn((input: RequestInfo | URL) => {
    const path = String(input); paths.push(path);
    if (path === '/api/me') return Promise.resolve(response(me));
    if (path === '/api/csrf') { entered.resolve(); return late.promise; }
    if (path === me.auth.refresh_url) return Promise.resolve(response({}));
    return Promise.resolve(response({ error: { code: 'invalid_credentials' } }, 401));
  });
  const session = new Session(fetcher, vi.fn()); await session.validate();
  const pending = session.read('/api/keys'); const rejected = expect(pending).rejects.toBeInstanceOf(StaleResponse);
  await entered.promise;
  if (boundary === 'logout') session.logout(); else await session.validate();
  late.resolve(csrf()); await rejected;
  expect(paths.filter(path => path === '/api/keys')).toHaveLength(1);
});

test.each([401, 403, 503])('refresh_csrf_failure_%s_never_retries_read_or_any_write', async status => {
  const paths: string[] = [];
  const fetcher = vi.fn((input: RequestInfo | URL) => {
    const path = String(input); paths.push(path);
    if (path === '/api/me') return Promise.resolve(response(me));
    if (path === '/api/csrf') return Promise.resolve(response({ error: { code: 'synthetic' } }, status));
    if (path === me.auth.refresh_url) return Promise.resolve(response({}));
    return Promise.resolve(response({ error: { code: 'invalid_credentials' } }, 401));
  });
  const nav = vi.fn(), session = new Session(fetcher, nav); await session.validate();
  await expect(session.read('/api/keys')).rejects.toMatchObject({ status });
  expect(paths).toEqual(['/api/me', '/api/keys', me.auth.refresh_url, '/api/me', '/api/csrf']);
  expect(session.snapshot().me).toBeNull(); expect(nav).toHaveBeenCalledTimes(status === 401 ? 1 : 0);
});
