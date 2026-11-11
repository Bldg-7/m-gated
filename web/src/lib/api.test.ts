import { expect, test, vi } from 'vitest';
import { Session, StaleResponse } from './api';
import type { Me } from './types';
const me: Me = { principal: { id: 'human1', display_name: 'Synthetic', email: 'synthetic@example.invalid', block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' } }, role: 'user', grants: [], auth_expires_at: '2030-01-01T00:00:00Z', sso: { refresh_url: 'https://auth.example.test/sso/refresh', logout_url: 'https://auth.example.test/sso/logout' } };
const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
function deferred<T>() { let resolve!: (value: T) => void; const promise = new Promise<T>(r => { resolve = r; }); return { promise, resolve }; }
test('auth_epoch_drops_stale_responses_and_logout_removes_identity', async () => {
  const late = deferred<Response>(); const fetcher = vi.fn().mockResolvedValueOnce(response(me)).mockReturnValueOnce(late.promise); const nav = vi.fn(); const session = new Session(fetcher, nav); await session.validate();
  const pending = session.read('/gateway/usage'); session.logout(); late.resolve(response({ settled_usd: '999.000000000000' })); await expect(pending).rejects.toBeInstanceOf(StaleResponse); expect(session.snapshot().me).toBeNull(); expect(nav).toHaveBeenCalledWith(me.sso.logout_url);
});
test('initial_401_redirects_without_refresh_and_403_never_loops', async () => {
  for (const status of [401, 403]) { const fetcher = vi.fn().mockResolvedValue(response({ error: { code: 'auth' } }, status)); const nav = vi.fn(); const session = new Session(fetcher, nav); await session.validate(); expect(fetcher).toHaveBeenCalledTimes(1); expect(nav).toHaveBeenCalledTimes(status === 401 ? 1 : 0); expect(session.snapshot().me).toBeNull(); }
});
test('refresh_reads_once_never_replays_writes', async () => {
  const fetcher = vi.fn().mockResolvedValueOnce(response(me)).mockResolvedValueOnce(response({ error: { code: 'invalid_credentials' } }, 401)).mockResolvedValueOnce(response({})).mockResolvedValueOnce(response(me)).mockResolvedValueOnce(response({ token: 'synthetic-csrf' })).mockResolvedValueOnce(response({ value: 1 })); const session = new Session(fetcher, vi.fn()); await session.validate(); expect(await session.read('/gateway/usage')).toEqual({ value: 1 }); expect(fetcher.mock.calls.map(c => String(c[0]))).toEqual(['/gateway/me', '/gateway/usage', me.sso.refresh_url, '/gateway/me', '/gateway/csrf', '/gateway/usage']); expect(fetcher.mock.calls[2][1].method).toBe('POST');
});
test('visible_revalidation_supersedes_an_old_pending_me', async () => {
  const old = deferred<Response>(); const fetcher = vi.fn().mockReturnValueOnce(old.promise).mockResolvedValueOnce(response(me)); const session = new Session(fetcher, vi.fn()); const first = session.validate(); session.clear(); const second = session.validate(); old.resolve(response({ ...me, principal: { ...me.principal, id: 'old-human' } })); await Promise.all([first, second]); expect(session.snapshot().me?.principal.id).toBe('human1'); expect(fetcher).toHaveBeenCalledTimes(2);
});
test('concurrent_401_reads_join_one_refresh_and_retry_once', async () => {
  const a = deferred<Response>(), b = deferred<Response>(), refresh = deferred<Response>();
  const fetcher = vi.fn().mockResolvedValueOnce(response(me)).mockReturnValueOnce(a.promise).mockReturnValueOnce(b.promise).mockReturnValueOnce(refresh.promise).mockResolvedValueOnce(response(me)).mockImplementation(() => Promise.resolve(response({ value: 1 })));
  const session = new Session(fetcher, vi.fn()); await session.validate(); const first = session.read('/gateway/usage'); const second = session.read('/gateway/me');
  a.resolve(response({ error: { code: 'expired' } }, 401)); await vi.waitFor(() => expect(fetcher).toHaveBeenCalledTimes(4)); b.resolve(response({ error: { code: 'expired' } }, 401)); refresh.resolve(response({}));
  const result = await Promise.allSettled([first, second]); expect(result.map(r => r.status)).toEqual(['fulfilled', 'fulfilled']); expect(fetcher.mock.calls.filter(c => c[0] === me.sso.refresh_url)).toHaveLength(1);
});
test('self_403_discards_identity_and_other_late_reads_without_login_loop', async () => {
  const late = deferred<Response>(); const fetcher = vi.fn().mockResolvedValueOnce(response(me)).mockReturnValueOnce(late.promise).mockResolvedValueOnce(response({ error: { code: 'access_denied' } }, 403)).mockResolvedValueOnce(response({ error: { code: 'access_denied' } }, 403));
  const nav = vi.fn(); const session = new Session(fetcher, nav); await session.validate(); const original = session.snapshot().epoch; const pending = session.read('/gateway/usage?model=old'); await expect(session.read('/gateway/usage')).rejects.toMatchObject({ status: 403 }); await vi.waitFor(() => expect(session.snapshot().status).toBe('error')); late.resolve(response({ value: 'late-private-data' })); await expect(pending).rejects.toBeInstanceOf(StaleResponse); expect(session.snapshot().epoch).toBeGreaterThan(original); expect(session.snapshot().me).toBeNull(); expect(nav).not.toHaveBeenCalled(); expect(fetcher.mock.calls.filter(c => c[0] === '/gateway/me')).toHaveLength(2);
});
test('a_403_on_the_single_retry_also_invalidates_all_cached_authority',async()=>{
 const fetcher=vi.fn().mockResolvedValueOnce(response(me)).mockResolvedValueOnce(response({error:{code:'expired'}},401)).mockResolvedValueOnce(response({})).mockResolvedValueOnce(response(me)).mockResolvedValueOnce(response({token:'synthetic-csrf'})).mockResolvedValueOnce(response({error:{code:'access_denied'}},403)).mockResolvedValueOnce(response({error:{code:'access_denied'}},403));
 const session=new Session(fetcher,vi.fn());await session.validate();await expect(session.read('/gateway/usage')).rejects.toMatchObject({status:403});await vi.waitFor(()=>expect(session.snapshot().status).toBe('error'));expect(session.snapshot().me).toBeNull();
});
test('broadcast_logout_does_not_rehydrate_still_valid_cookies',async()=>{
 let channel:{onmessage:((event:{data:{type:string}})=>void)|null};
 class Channel {onmessage:((event:{data:{type:string}})=>void)|null=null;constructor(){channel=this;}postMessage(){}close(){}}
 vi.stubGlobal('BroadcastChannel',Channel);
 const fetcher=vi.fn().mockImplementation(()=>Promise.resolve(response(me)));const session=new Session(fetcher,vi.fn());await session.validate();const detach=session.attach();
 try{channel!.onmessage!({data:{type:'logout'}});await vi.waitFor(()=>expect(session.snapshot().status).toBe('error'));expect(session.snapshot().me).toBeNull();expect(fetcher).toHaveBeenCalledTimes(1);}finally{detach();vi.unstubAllGlobals();}
});

test('logout_latch_blocks_automatic_revalidation_until_explicit_confirmation', async () => {
  let channel!: { onmessage: ((event: { data: { type: string } }) => void) | null };
  class Channel { onmessage = null; constructor() { channel = this; } postMessage() {} close() {} }
  vi.stubGlobal('BroadcastChannel', Channel);
  const fetcher = vi.fn().mockImplementation(() => Promise.resolve(response(me)));
  const session = new Session(fetcher, vi.fn()); await session.validate(); const detach = session.attach();
  try {
    channel.onmessage!({ data: { type: 'logout' } });
    session.clear();
    window.dispatchEvent(new PageTransitionEvent('pageshow', { persisted: true }));
    document.dispatchEvent(new Event('visibilitychange'));
    channel.onmessage!({ data: { type: 'auth-change' } });
    await Promise.resolve(); await Promise.resolve();
    expect(fetcher).toHaveBeenCalledTimes(1);
    expect(session.snapshot().me).toBeNull();
    await session.validate();
    expect(session.snapshot().me?.principal.id).toBe('human1');
    expect(fetcher).toHaveBeenCalledTimes(2);
    session.logout();
    window.dispatchEvent(new PageTransitionEvent('pageshow', { persisted: true }));
    await Promise.resolve();
    expect(fetcher).toHaveBeenCalledTimes(2);
  } finally { detach(); vi.unstubAllGlobals(); }
});

test('refresh_new_me_403_stays_permission_error_without_login', async () => {
  const late = deferred<Response>();
  const fetcher = vi.fn().mockResolvedValueOnce(response(me)).mockReturnValueOnce(late.promise)
    .mockResolvedValueOnce(response({ error: { code: 'expired' } }, 401))
    .mockResolvedValueOnce(response({})).mockResolvedValueOnce(response({ error: { code: 'access_denied' } }, 403));
  const nav = vi.fn(); const session = new Session(fetcher, nav); await session.validate();
  const pending = session.read('/gateway/usage?model=old');
  await expect(session.read('/gateway/usage')).rejects.toMatchObject({ status: 403 });
  late.resolve(response({ private: 'old' })); await expect(pending).rejects.toBeInstanceOf(StaleResponse);
  expect(nav).not.toHaveBeenCalled(); expect(session.snapshot()).toMatchObject({ me: null, status: 'error', error: '접근 권한이 없습니다.' });
  expect(fetcher.mock.calls.map(c => String(c[0]))).toEqual(['/gateway/me', '/gateway/usage?model=old', '/gateway/usage', me.sso.refresh_url, '/gateway/me']);
});

test('verified_identity_change_notifies_other_session_once_without_echo', async () => {
  const channels: Channel[] = []; const sent: unknown[] = [];
  class Channel {
    onmessage: ((event: { data: unknown }) => void) | null = null;
    constructor() { channels.push(this); }
    postMessage(data: unknown) { sent.push(data); for (const other of channels) if (other !== this) other.onmessage?.({ data }); }
    close() { channels.splice(channels.indexOf(this), 1); }
  }
  vi.stubGlobal('BroadcastChannel', Channel);
  const secondMe: Me = { ...me, principal: { ...me.principal, id: 'human2', display_name: 'Second' } };
  const held = deferred<Response>(); const late = deferred<Response>();
  const aFetch = vi.fn().mockResolvedValueOnce(response(me)).mockResolvedValueOnce(response(secondMe)).mockResolvedValue(response(secondMe));
  const bFetch = vi.fn().mockResolvedValueOnce(response(me)).mockReturnValueOnce(late.promise).mockReturnValueOnce(held.promise);
  const a = new Session(aFetch, vi.fn()), b = new Session(bFetch, vi.fn());
  await a.validate(); await b.validate(); const detachA = a.attach(), detachB = b.attach();
  try {
    const pending = b.read('/gateway/usage?model=old');
    await a.validate();
    expect(sent).toEqual([{ type: 'auth-change' }]);
    expect(b.snapshot().me).toBeNull(); expect(b.snapshot().status).toBe('checking');
    late.resolve(response({ private: 'old' })); await expect(pending).rejects.toBeInstanceOf(StaleResponse);
    held.resolve(response(secondMe)); await vi.waitFor(() => expect(b.snapshot().me?.principal.id).toBe('human2'));
    await a.validate(); expect(sent).toHaveLength(1); expect(bFetch).toHaveBeenCalledTimes(3);
  } finally { detachA(); detachB(); vi.unstubAllGlobals(); }
});

test('refresh_new_me_dependency_failure_is_not_a_login_failure', async () => {
  for (const status of [500, 503]) {
    const fetcher = vi.fn().mockResolvedValueOnce(response(me)).mockResolvedValueOnce(response({ error: { code: 'expired' } }, 401))
      .mockResolvedValueOnce(response({})).mockResolvedValueOnce(response({ error: { code: 'auth_dependency_unavailable' } }, status));
    const nav = vi.fn(), session = new Session(fetcher, nav); await session.validate();
    await expect(session.read('/gateway/usage')).rejects.toMatchObject({ status });
    expect(nav).not.toHaveBeenCalled();
    expect(session.snapshot()).toMatchObject({ me: null, status: 'error', error: '인증 상태를 확인할 수 없습니다.' });
    expect(fetcher).toHaveBeenCalledTimes(4);
  }
});

test('refresh_failure_or_new_me401_still_navigates_once_without_read_replay', async () => {
  for (const failure of ['refresh', 'me'] as const) {
    const fetcher = vi.fn().mockResolvedValueOnce(response(me)).mockResolvedValueOnce(response({ error: { code: 'expired' } }, 401))
      .mockResolvedValueOnce(response({}, failure === 'refresh' ? 403 : 200)).mockResolvedValueOnce(response({ error: { code: 'expired' } }, 401));
    const nav = vi.fn(), session = new Session(fetcher, nav); await session.validate();
    await expect(session.read('/gateway/usage')).rejects.toBeInstanceOf(Error);
    expect(nav.mock.calls).toEqual([['/auth/login']]); expect(session.snapshot().me).toBeNull();
    expect(fetcher).toHaveBeenCalledTimes(failure === 'refresh' ? 3 : 4);
  }
});

test('refresh_success_does_not_turn_new_me_network_failure_into_login', async () => {
  const fetcher = vi.fn().mockResolvedValueOnce(response(me)).mockResolvedValueOnce(response({ error: { code: 'expired' } }, 401))
    .mockResolvedValueOnce(response({})).mockRejectedValueOnce(new TypeError('synthetic network failure'));
  const nav = vi.fn(), session = new Session(fetcher, nav); await session.validate();
  await expect(session.read('/gateway/usage')).rejects.toBeInstanceOf(TypeError);
  expect(nav).not.toHaveBeenCalled(); expect(session.snapshot()).toMatchObject({ me: null, status: 'error', error: '인증 상태를 확인할 수 없습니다.' });
  expect(fetcher).toHaveBeenCalledTimes(4);
});
