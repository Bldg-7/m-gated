import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../app';
import { session } from './api';
import { Link } from './router';
const me = { principal: { id: 'admin1', display_name: '관리자', email: 'admin@example.test', block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' } }, role: 'admin', grants: [], sso: { refresh_url: 'https://auth.example.test/sso/refresh' } };
const usage = { currency: 'USD', from: '2026-01-01T00:00:00Z', to: '2026-02-01T00:00:00Z', as_of: '2026-02-01T00:00:00Z', detail_available_from: null, aggregation: 'detail', settled_usd: '0.000000000001', held_reservation_usd: '0.000000000000', period_unknown_count: '0', global_unknown_count: '0', global_expired_dispatch_count: '0', block_state: null, balance_context: 'current_principal', budget_balances: [], buckets: { items: [], next_cursor: null, as_of: '2026-02-01T00:00:00Z' } };
const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
afterEach(() => { cleanup(); session.clear(); history.replaceState(null, '', '/ui/'); vi.unstubAllGlobals(); });

/** React 처리 뒤 기본 동작이 막혔는지 기록하고, jsdom이 실제 문서 이동을 시도하지 않게 막는다. */
function clickRecord(element: Element, init: MouseEventInit = {}) {
  let prevented = false;
  const record = (event: Event) => { prevented = event.defaultPrevented; event.preventDefault(); };
  document.addEventListener('click', record);
  try { fireEvent.click(element, init); } finally { document.removeEventListener('click', record); }
  return prevented;
}

test('spa_link_moves_between_screens_without_document_navigation_and_remounts_each_screen', async () => {
  history.replaceState(null, '', '/ui/');
  const paths: string[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL) => {
    const url = String(input); paths.push(url);
    if (url === '/gateway/me') return Promise.resolve(response(me));
    if (url.startsWith('/gateway/usage') || url.startsWith('/gateway/admin/usage')) return Promise.resolve(response(usage));
    return Promise.resolve(response({ items: [], next_cursor: null, as_of: '2026-02-01T00:00:00Z' }));
  }));
  render(<App />);
  await screen.findByRole('heading', { name: '조회 기간 사용량' });
  expect(screen.getByRole('link', { name: '내 사용량' }).getAttribute('aria-current')).toBe('page');
  fireEvent.change(screen.getByLabelText('모델'), { target: { value: 'kept-only-on-this-screen' } });

  expect(clickRecord(screen.getByRole('link', { name: '전체 사용량' }))).toBe(true);
  expect(location.pathname).toBe('/ui/admin/usage');
  await screen.findByLabelText('사용자 ID');
  expect((screen.getByLabelText('모델') as HTMLInputElement).value).toBe('');
  expect(paths).toContain('/gateway/admin/usage?group_by=provider');
  expect(screen.getByRole('link', { name: '전체 사용량' }).getAttribute('aria-current')).toBe('page');
  expect(document.activeElement).toBe(screen.getByRole('heading', { level: 1, name: '전체 사용량' }));
  expect(document.title).toBe('전체 사용량 · m-gated');

  expect(clickRecord(screen.getByRole('link', { name: '내 키' }))).toBe(true);
  await screen.findByRole('heading', { name: '내 키' });
  expect(paths).toContain('/gateway/keys');

  await act(async () => { history.pushState(null, '', '/ui/'); window.dispatchEvent(new PopStateEvent('popstate')); });
  await screen.findByRole('heading', { name: '조회 기간 사용량' });
  expect(screen.queryByRole('heading', { name: '내 키' })).toBeNull();
  expect(paths.filter(path => path === '/gateway/me')).toHaveLength(1);
});

test('spa_link_leaves_modified_clicks_and_non_screen_paths_to_the_browser', () => {
  history.replaceState(null, '', '/ui/');
  render(<><Link href="/ui/keys">키</Link><Link href="/auth/login">로그인</Link><Link href="https://elsewhere.example.test/ui/keys">외부</Link></>);
  expect(clickRecord(screen.getByText('키'), { ctrlKey: true })).toBe(false);
  expect(clickRecord(screen.getByText('키'), { metaKey: true })).toBe(false);
  expect(clickRecord(screen.getByText('로그인'))).toBe(false);
  expect(clickRecord(screen.getByText('외부'))).toBe(false);
  expect(location.pathname).toBe('/ui/');
  expect(clickRecord(screen.getByText('키'))).toBe(true);
  expect(location.pathname).toBe('/ui/keys');
});
