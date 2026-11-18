import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../app';
import { session } from './api';
import { formatTime, parseInstant, timeBasis } from './time';

const me = { principal: { id: 'human1', display_name: 'Synthetic', email: 'synthetic@example.invalid', block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' } }, role: 'user', grants: [], sso: { refresh_url: 'https://auth.example.test/sso/refresh' } };
const usage = { currency: 'USD', from: '2026-10-31T15:00:00.000000Z', to: '2026-11-30T15:00:00.000000Z', as_of: '2026-11-18T03:04:59.999999Z', detail_available_from: null, aggregation: 'detail', settled_usd: '0.000000000001', held_reservation_usd: '0.000000000000', period_unknown_count: '0', global_unknown_count: '0', global_expired_dispatch_count: '0', block_state: null, balance_context: 'current_principal', budget_balances: [], buckets: { items: [], next_cursor: null, as_of: '2026-11-18T03:04:59Z' } };
const response = (body: unknown) => new Response(JSON.stringify(body), { status: 200, headers: { 'content-type': 'application/json' } });
afterEach(() => { cleanup(); session.clear(); act(() => timeBasis.set('browser')); history.replaceState(null, '', '/dashboard/'); vi.unstubAllGlobals(); });

test('times_are_shown_to_the_minute_in_the_given_zone_without_special_cases', () => {
  expect(formatTime('2026-11-01T00:00:00.000000Z', 'UTC')).toBe('2026-11-01 00:00');
  expect(formatTime('2026-11-01T00:00:00.000000Z', 'Asia/Seoul')).toBe('2026-11-01 09:00');
  expect(formatTime('2026-11-30T15:00:00Z', 'Asia/Seoul')).toBe('2026-12-01 00:00');
  // 초 이하는 반올림하지 않고 버린다.
  expect(formatTime('2026-11-18T12:34:59.999999Z', 'UTC')).toBe('2026-11-18 12:34');
  expect(formatTime('2026-11-01T09:00:00+09:00', 'UTC')).toBe('2026-11-01 00:00');
  expect(formatTime('2026-03-08T10:30:00Z', 'America/New_York')).toBe('2026-03-08 06:30');
  // 달의 마지막 날이나 자정도 그대로 분까지 보인다.
  expect(formatTime('2026-12-31T23:59:00Z', 'UTC')).toBe('2026-12-31 23:59');
  expect(formatTime('2027-01-01T00:00:00Z', 'UTC')).toBe('2027-01-01 00:00');
  expect(parseInstant('2026-11-01T00:00:00.123456Z')).toBe(Date.UTC(2026, 10, 1, 0, 0, 0, 123));
  // 서버 시각 형식이 아니면 받은 값을 그대로 보인다.
  expect(formatTime('now', 'UTC')).toBe('now');
  expect(formatTime('2026-11-01', 'UTC')).toBe('2026-11-01');
});

test('clock_toggle_switches_every_shown_time_between_browser_time_and_utc_and_only_remembers_utc', async () => {
  const real = Intl.DateTimeFormat.prototype.resolvedOptions;
  vi.spyOn(Intl.DateTimeFormat.prototype, 'resolvedOptions').mockImplementation(function (this: Intl.DateTimeFormat) { return { ...real.call(this), timeZone: 'Asia/Seoul' }; });
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL) => Promise.resolve(response(String(input) === '/api/me' ? me : usage))));
  render(<App />);
  await screen.findByRole('heading', { name: '조회 기간 사용량' });
  // 기본은 브라우저 시간대다.
  expect(screen.getByText('브라우저 시간(Asia/Seoul)')).toBeTruthy();
  expect(screen.getByText('2026-11-01 00:00')).toBeTruthy();
  expect(screen.getByText('2026-11-18 12:04')).toBeTruthy();
  expect(screen.getByText('2026-11-01 00:00').getAttribute('datetime')).toBe('2026-10-31T15:00:00.000000Z');
  expect(localStorage.length).toBe(0);
  const toggle = screen.getByRole('button', { name: 'UTC로 보기' });
  expect(toggle.getAttribute('title')).toBe('UTC로 보기');
  fireEvent.click(toggle);
  expect(screen.getByText('UTC')).toBeTruthy();
  expect(screen.getByText('2026-10-31 15:00')).toBeTruthy();
  expect(screen.getByText('2026-11-18 03:04')).toBeTruthy();
  expect(screen.queryByText('2026-11-01 00:00')).toBeNull();
  expect(localStorage.getItem('m-gated.time')).toBe('utc');
  // 입력칸(현지 시각)은 기준과 상관없이 브라우저 시각이다.
  expect(screen.getByLabelText('시작 (현지 시각)').getAttribute('type')).toBe('datetime-local');
  const back = screen.getByRole('button', { name: '브라우저 시간으로 보기' });
  expect(back.getAttribute('title')).toBe('브라우저 시간으로 보기');
  fireEvent.click(back);
  expect(screen.getByText('브라우저 시간(Asia/Seoul)')).toBeTruthy();
  expect(screen.getByText('2026-11-01 00:00')).toBeTruthy();
  expect(localStorage.length).toBe(0);
});
