import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../../app';
import { pickOption, selectedValue } from '../../test-select';
import { session } from '../../lib/api';
const me = { principal: { id: 'human1', display_name: 'Synthetic', email: 'a@example.test' }, role: 'user', grants: [{ provider: 'anthropic', model: 'synthetic-text', capabilities: ['messages'] }], provider_credentials: [{ provider: 'anthropic', credentials: [{ id: 'cred1', label: 'team-a', hint: '0001' }] }], auth: { mode: 'sso', refresh_url: 'https://auth.example.test/sso/refresh', logout_url: 'https://auth.example.test/sso/logout' } };
const key = { id: '00000000-0000-4000-8000-000000000001', name: '현재 키', version: '7', created_command_id: 'original', validity: 'unlimited', expires_at: null, revoked_at: null, grants: [], limits: {} };
const response = (body: unknown, status = 200) => new Response(status === 204 ? null : JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
function deferred<T>() { let resolve!: (value: T) => void; const promise = new Promise<T>(r => { resolve = r; }); return { promise, resolve }; }
afterEach(() => { cleanup(); session.clear(); history.replaceState(null, '', '/dashboard/'); vi.unstubAllGlobals(); });
/** view='list'는 목록 페이지(/dashboard/keys), view='issue'는 발급 하위 페이지(/dashboard/keys/new)에서 시작한다. */
async function mount(write: (input: RequestInfo | URL, init: RequestInit) => Promise<Response>, items: () => unknown[] = () => [key], read?: () => Promise<Response>, view: 'list' | 'issue' = 'list') {
  history.replaceState(null, '', view === 'issue' ? '/dashboard/keys/new' : '/dashboard/keys');
  const fetcher = vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    if (String(input) === '/api/me') return Promise.resolve(response(me));
    if (String(input) === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (init?.method) return write(input, init);
    if (read) return read();
    return Promise.resolve(response({ items: items(), next_cursor: null, as_of: '2026-01-01T00:00:00Z' }));
  });
  vi.stubGlobal('fetch', fetcher); render(<App />);
  if (view === 'issue') { await screen.findByRole('form', { name: '가상 키 발급' }); await waitFor(() => expect(lists(fetcher)).toBeGreaterThan(0)); return fetcher; }
  await screen.findByRole('region', { name: '내 가상 키' });
  await waitFor(() => expect((screen.getByRole('button', { name: '조회' }) as HTMLButtonElement).disabled).toBe(false));
  return fetcher;
}
const mountIssue = (write: Parameters<typeof mount>[0], items: () => unknown[] = () => [key]) => mount(write, items, undefined, 'issue');
/** 키 목록 GET 횟수. 발급 페이지도 명령 결과를 확인하려고 목록을 읽는다. */
const lists = (fetcher: { mock: { calls: [RequestInfo | URL, RequestInit?][] } }) => fetcher.mock.calls.filter(c => String(c[0]).startsWith('/api/keys') && !c[1]?.method).length;
/** 목록 페이지의 발급 링크. 확인되지 않은 명령 결과가 있는 동안에는 보이지 않는다. */
const issueLink = () => screen.queryByRole('link', { name: '가상 키 발급' });
const issueFieldset = () => screen.getByRole('button', { name: '발급 내용 확인' }).closest('fieldset') as HTMLFieldSetElement;
function prepare(name = '새 키') {
  fireEvent.change(screen.getByLabelText('키 이름'), { target: { value: name } });
  pickOption(screen.getByLabelText('유효기간'), '무제한');
  pickOption(screen.getByLabelText('연동할 Provider 키 (1개 이상 필수)'), 'Anthropic · team-a (…0001)');
  fireEvent.click(screen.getByLabelText('anthropic / synthetic-text / messages'));
  fireEvent.submit(screen.getByRole('form', { name: '가상 키 발급' }));
  fireEvent.click(screen.getByRole('button', { name: '발급 확인' }));
}
test('revoke409_keeps_explanation_after_get_and_requires_new_confirmation_version', async () => {
  let version = '7', writes = 0;
  const fetcher = await mount(async (input) => {
    writes++;
    expect(String(input)).toContain(`expected_version=${version}`);
    if (writes === 1) { version = '11'; return response({ error: { code: 'version_conflict', details: { current_version: '11' } } }, 409); }
    return response(null, 204);
  }, () => [{ ...key, version }]);
  fireEvent.click(screen.getByRole('button', { name: '현재 키 회수' }));
  fireEvent.click(screen.getByRole('button', { name: '회수 확인' }));
  // 거절된 뒤 목록을 다시 읽어 새 버전을 받는다. 버전은 상세 창에 보인다.
  await screen.findByRole('alert');
  fireEvent.click(screen.getByRole('button', { name: '현재 키 상세' }));
  await waitFor(() => expect(within(screen.getByRole('dialog')).getByText('11')).toBeTruthy());
  fireEvent.click(within(screen.getByRole('dialog')).getByRole('button', { name: '닫기' }));
  expect(screen.getByRole('alert').textContent).toContain('버전이 변경되었습니다.');
  expect(writes).toBe(1);
  fireEvent.click(screen.getByRole('button', { name: '현재 키 회수' }));
  fireEvent.click(screen.getByRole('button', { name: '회수 확인' }));
  await waitFor(() => expect(writes).toBe(2));
  const commands = fetcher.mock.calls.filter(c => c[1]?.method === 'DELETE').map(c => new Headers(c[1]?.headers).get('x-gateway-command-id'));
  expect(new Set(commands).size).toBe(2);
});
test('lost_issue_uses_created_command_get_never_post_replay', async () => {
  let command = '', writes = 0;
  const fetcher = await mountIssue(async (_, init) => { writes++; command = new Headers(init.headers).get('x-gateway-command-id')!; throw new TypeError('synthetic network loss'); }, () => command ? [{ ...key, created_command_id: command }] : []);
  prepare(); await screen.findByText(/연결된 키:/);
  // 결과를 단정할 수 없으면 발급 페이지에 머물며 명령 ID로 목록에서 확인한다. 폼은 잠근다.
  expect(location.pathname).toBe('/dashboard/keys/new');
  expect(screen.getByRole('region', { name: '명령 결과 확인' }).textContent).toContain(command);
  expect(screen.getByRole('region', { name: '명령 결과 확인' }).textContent).toContain(`연결된 키: ${key.id}`);
  expect(issueFieldset().disabled).toBe(true);
  expect(writes).toBe(1); expect(screen.queryByRole('dialog', { name: '발급된 키' })).toBeNull();
  const reads = lists(fetcher);
  fireEvent.click(screen.getByRole('button', { name: '명령 결과 다시 조회' }));
  await waitFor(() => expect(lists(fetcher)).toBe(reads + 1));
  await screen.findByText(/연결된 키:/); expect(writes).toBe(1);
  expect(issueFieldset().disabled).toBe(true);
});
test('pagehide_synchronously_removes_secret_and_form_before_page_can_freeze', async () => {
  await mountIssue(async () => response({ key: 'temporary-secret', metadata: key }, 201));
  prepare(); await screen.findByText('temporary-secret');
  await act(async () => { window.dispatchEvent(new PageTransitionEvent('pagehide', { persisted: true })); });
  expect(screen.queryByText('temporary-secret')).toBeNull(); expect(screen.queryByLabelText('키 이름')).toBeNull();
  await act(async () => { window.dispatchEvent(new PageTransitionEvent('pageshow', { persisted: true })); });
  await screen.findByRole('region', { name: '가상 키 발급' });
  expect(location.pathname).toBe('/dashboard/keys/new');
  expect(screen.queryByText('temporary-secret')).toBeNull(); expect((screen.getByLabelText('키 이름') as HTMLInputElement).value).toBe('');
  // 고른 Provider 키와 허용한 API도 남지 않는다.
  expect(selectedValue(screen.getByLabelText('연동할 Provider 키 (1개 이상 필수)'))).toBe('');
  expect(screen.queryByLabelText('anthropic / synthetic-text / messages')).toBeNull();
  expect((screen.getByRole('button', { name: '발급 내용 확인' }) as HTMLButtonElement).disabled).toBe(true);
});
test('late_committed_issue_is_not_consumed_after_explicit_revalidation', async () => {
  const late = deferred<Response>(), entered = deferred<void>();
  await mountIssue(() => { entered.resolve(); return late.promise; });
  prepare(); await entered.promise;
  await act(async () => { await session.validate(); late.resolve(response({ key: 'stale-secret', metadata: key }, 201)); });
  expect(screen.queryByText('stale-secret')).toBeNull();
  expect(screen.queryByRole('dialog', { name: '발급된 키' })).toBeNull();
  expect(screen.queryByRole('region', { name: '명령 결과 확인' })).toBeNull();
  expect(location.pathname).toBe('/dashboard/keys/new');
});

test('same_human_key_refresh_preserves_filter_and_has_one_read_retry', async () => {
  history.replaceState(null, '', '/dashboard/keys'); let reads = 0;
  const refresh = deferred<Response>();
  const fetcher = vi.fn((input: RequestInfo | URL) => {
    const path = String(input);
    if (path === '/api/me') return Promise.resolve(response(me));
    if (path === me.auth.refresh_url) return refresh.promise;
    if (path === '/api/keys?status=revoked' && ++reads === 1) return Promise.resolve(response({ error: { code: 'invalid_credentials' } }, 401));
    return Promise.resolve(response({ items: [], next_cursor: null, as_of: '2026-01-01T00:00:00Z' }));
  });
  vi.stubGlobal('fetch', fetcher); render(<App />); await screen.findByText('키가 없습니다.');
  pickOption(screen.getByLabelText('키 상태'), '회수');
  fireEvent.submit(screen.getByRole('form', { name: '키 조회' }));
  await screen.findByText('인증 확인 중…');
  await act(async () => { refresh.resolve(response({})); });
  await screen.findByText('키가 없습니다.');
  expect(selectedValue(screen.getByLabelText('키 상태'))).toBe('revoked');
  expect(reads).toBe(2);
  expect(fetcher.mock.calls.filter(c => c[0] === '/api/keys')).toHaveLength(1);
  expect(fetcher.mock.calls.filter(c => c[0] === me.auth.refresh_url)).toHaveLength(1);
});

test('definite_cookie_rejection_keeps_error_but_allows_a_new_user_confirmation', async () => {
  let writes = 0;
  await mountIssue(async () => { writes++; return response({ error: { code: 'csrf_invalid' } }, 403); });
  prepare(); await screen.findByRole('alert');
  await waitFor(() => expect((screen.getByRole('button', { name: '발급 내용 확인' }) as HTMLButtonElement).disabled).toBe(false));
  expect(screen.queryByRole('region', { name: '명령 결과 확인' })).toBeNull(); expect(writes).toBe(1);
  expect(issueFieldset().disabled).toBe(false);
  expect(location.pathname).toBe('/dashboard/keys/new');
});

function confirmRevoke() {
  // 회수 전에는 확인되지 않은 명령이 없으므로 발급 링크가 보인다.
  expect(issueLink()?.getAttribute('href')).toBe('/dashboard/keys/new');
  fireEvent.click(screen.getByRole('button', { name: '현재 키 회수' }));
  fireEvent.click(screen.getByRole('button', { name: '회수 확인' }));
}
const revoked = { ...key, version: '8', revoked_at: '2026-01-02T00:00:00Z' };
const pageResponse = (items: unknown[]) => response({ items, next_cursor: null, as_of: '2026-01-03T00:00:00Z' });

test('lost_delete_get_confirms_exact_revocation_and_unlocks_only_user_confirmed_new_issue', async () => {
  let deleted = false, deletes = 0, posts = 0;
  await mount(async (_, init) => {
    if (init.method === 'DELETE') { deletes++; deleted = true; throw new TypeError('synthetic response loss'); }
    posts++; return response({ key: 'explicit-new-secret', metadata: key }, 201);
  }, () => [deleted ? revoked : key]);
  confirmRevoke();
  await screen.findByText('2026-01-02 00:00');
  // 회수가 서버 조회로 확인되면 발급 링크가 다시 나타난다.
  await waitFor(() => expect(issueLink()).toBeTruthy());
  expect(screen.queryByRole('region', { name: '명령 결과 확인' })).toBeNull();
  expect(screen.queryByRole('alert')).toBeNull();
  expect(screen.queryByRole('dialog', { name: '발급된 키' })).toBeNull();
  expect(deletes).toBe(1); expect(posts).toBe(0);
  // 새 키는 사용자가 발급 페이지로 가서 내용을 직접 확인해야만 발급된다.
  fireEvent.click(issueLink()!);
  await screen.findByRole('form', { name: '가상 키 발급' });
  expect(location.pathname).toBe('/dashboard/keys/new');
  expect(issueFieldset().disabled).toBe(false);
  expect(posts).toBe(0);
  fireEvent.change(screen.getByLabelText('키 이름'), { target: { value: '명시한 새 키' } });
  pickOption(screen.getByLabelText('유효기간'), '무제한');
  pickOption(screen.getByLabelText('연동할 Provider 키 (1개 이상 필수)'), 'Anthropic · team-a (…0001)');
  fireEvent.click(screen.getByLabelText('anthropic / synthetic-text / messages'));
  fireEvent.submit(screen.getByRole('form', { name: '가상 키 발급' }));
  expect(posts).toBe(0);
  fireEvent.click(screen.getByRole('button', { name: '발급 확인' }));
  await screen.findByText('explicit-new-secret');
  expect(posts).toBe(1); expect(deletes).toBe(1);
});

for (const candidate of ['different_id', 'active', 'expired', 'absent', 'failed'] as const) {
  test(`lost_delete_${candidate}_get_keeps_recovery_without_mutation_retry`, async () => {
    let reads = 0, deletes = 0;
    const items = candidate === 'different_id' ? [{ ...revoked, id: 'another-key' }] : candidate === 'absent' ? [] : [{ ...key, ...(candidate === 'expired' ? { validity: '3_months', expires_at: '2025-01-01T00:00:00Z' } : {}) }];
    await mount(async () => { deletes++; throw new TypeError('synthetic response loss'); }, undefined, async () => {
      if (++reads === 1) return pageResponse([key]);
      if (candidate === 'failed') throw new TypeError('synthetic read failure');
      return pageResponse(items);
    });
    confirmRevoke();
    await waitFor(() => expect(reads).toBe(2));
    await waitFor(() => expect((screen.getByRole('button', { name: '명령 결과 다시 조회' }) as HTMLButtonElement).disabled).toBe(false));
    expect(issueLink()).toBeTruthy();
    expect(screen.getByRole('region', { name: '명령 결과 확인' })).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: '명령 결과 다시 조회' }));
    await waitFor(() => expect(reads).toBe(3));
    expect(deletes).toBe(1);
    expect(issueLink()).toBeTruthy();
  });
}

test('lost_delete_superseded_get_cannot_unlock_current_uncertain_recovery', async () => {
  const old = deferred<Response>(); let reads = 0, deletes = 0;
  await mount(async () => { deletes++; throw new TypeError('synthetic response loss'); }, undefined, async () => {
    reads++;
    if (reads === 2) return old.promise;
    return pageResponse([key]);
  });
  confirmRevoke(); await waitFor(() => expect(reads).toBe(2));
  pickOption(screen.getByLabelText('키 상태'), '활성');
  fireEvent.submit(screen.getByRole('form', { name: '키 조회' }));
  await waitFor(() => expect(reads).toBe(3));
  await act(async () => { old.resolve(pageResponse([revoked])); });
  expect(screen.getByRole('region', { name: '명령 결과 확인' })).toBeTruthy();
  expect(issueLink()).toBeTruthy();
  expect(deletes).toBe(1);
});

test('lost_delete_old_epoch_get_cannot_clear_new_revoke_recovery_for_same_key', async () => {
  const old = deferred<Response>(); let reads = 0, deletes = 0;
  await mount(async () => { deletes++; throw new TypeError('synthetic response loss'); }, undefined, async () => {
    if (++reads === 2) return old.promise;
    return pageResponse([key]);
  });
  confirmRevoke(); await waitFor(() => expect(reads).toBe(2));
  const firstCommand = screen.getByRole('region', { name: '명령 결과 확인' }).textContent;
  await act(async () => { await session.validate(); });
  expect(screen.queryByRole('region', { name: '명령 결과 확인' })).toBeNull();
  fireEvent.submit(screen.getByRole('form', { name: '키 조회' }));
  await screen.findByRole('button', { name: '현재 키 회수' });
  confirmRevoke();
  await waitFor(() => expect(reads).toBe(4));
  const newCommand = screen.getByRole('region', { name: '명령 결과 확인' }).textContent;
  expect(newCommand).not.toBe(firstCommand);
  await act(async () => { old.resolve(pageResponse([revoked])); });
  expect(screen.getByRole('region', { name: '명령 결과 확인' }).textContent).toBe(newCommand);
  expect(issueLink()).toBeTruthy();
  expect(deletes).toBe(2);
});
