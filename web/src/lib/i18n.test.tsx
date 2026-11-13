import { act, cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../app';
import { en } from '../locales/en';
import { session } from './api';
import i18n, { changeLanguage } from './i18n';

const me = { principal: { id: 'human1', display_name: 'Synthetic', email: 'synthetic@example.invalid', block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' } }, role: 'user', grants: [], sso: { refresh_url: 'https://auth.example.test/sso/refresh' } };
const usage = { currency: 'USD', from: '2026-01-01T00:00:00Z', to: '2026-02-01T00:00:00Z', as_of: '2026-02-01T00:00:00Z', detail_available_from: null, aggregation: 'detail', settled_usd: '0.000000000001', held_reservation_usd: '0.000000000000', period_unknown_count: '0', global_unknown_count: '1', global_expired_dispatch_count: '0', block_state: null, balance_context: 'current_principal', budget_balances: [], buckets: { items: [], next_cursor: null, as_of: '2026-02-01T00:00:00Z' } };
const response = (body: unknown) => new Response(JSON.stringify(body), { status: 200, headers: { 'content-type': 'application/json' } });
afterEach(() => { cleanup(); session.clear(); act(() => changeLanguage('ko')); history.replaceState(null, '', '/ui/'); vi.unstubAllGlobals(); });

/** Session이 state.error로 내보내는 문구. 화면에서 t(auth.error)로 번역한다. */
const sessionMessages = ['접근 권한이 없습니다.', '인증 상태를 확인할 수 없습니다.', '인증이 만료되었습니다. 다시 확인한 뒤 명령 결과를 조회하세요. 쓰기는 재전송하지 않았습니다.', '로그아웃되었습니다.'];

test('every_korean_source_message_has_an_english_translation', () => {
  const sources = import.meta.glob(['../**/*.ts', '../**/*.tsx', '!../**/*.test.ts', '!../**/*.test.tsx', '!../locales/**'], { query: '?raw', import: 'default', eager: true }) as Record<string, string>;
  const used = new Set(sessionMessages);
  for (const text of Object.values(sources)) for (const match of text.matchAll(/(?<![\w.])(?:t|i18n\.t|msgid)\(\s*'((?:[^'\\]|\\.)*)'/g)) used.add(match[1]);
  expect(used.size).toBeGreaterThan(300);
  expect([...used].filter(key => !(key in en))).toEqual([]);
  expect(Object.keys(en).filter(key => !used.has(key))).toEqual([]);
});

test('korean_is_the_default_and_the_source_text_is_returned_with_values', () => {
  expect(i18n.language).toBe('ko');
  expect(document.documentElement.lang).toBe('ko');
  expect(i18n.t('전체 기간 미정산: {{unknown}} / 만료된 과금 실행: {{expired}}', { unknown: '1', expired: '0' })).toBe('전체 기간 미정산: 1 / 만료된 과금 실행: 0');
  expect(i18n.t('fence 설정 사유는 security_config_fence:승인참조입니다. 구 프로세스 종료 확인 전 해제하지 마세요.')).toContain('security_config_fence:승인참조');
});

test('language_switch_renders_english_and_only_remembers_an_explicit_english_choice', async () => {
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL) => Promise.resolve(response(String(input) === '/gateway/me' ? me : usage))));
  render(<App />);
  await screen.findByRole('heading', { name: '조회 기간 사용량' });
  expect(localStorage.length).toBe(0);

  fireEvent.click(screen.getByLabelText('English'));
  await screen.findByRole('heading', { name: 'Usage for the period' });
  expect(screen.getByRole('heading', { level: 1, name: 'My usage' })).toBeTruthy();
  expect(screen.getByText('Unsettled (all time): 1 / Expired billable executions: 0')).toBeTruthy();
  expect(screen.getByRole('link', { name: 'My keys' })).toBeTruthy();
  expect(document.documentElement.lang).toBe('en');
  expect(document.title).toBe('My usage · m-gated');
  expect(localStorage.getItem('m-gated.lang')).toBe('en');

  fireEvent.click(screen.getByLabelText('한국어'));
  await screen.findByRole('heading', { name: '조회 기간 사용량' });
  expect(document.documentElement.lang).toBe('ko');
  expect(localStorage.length).toBe(0);
});
