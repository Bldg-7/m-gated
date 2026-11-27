import { cleanup, fireEvent, render, screen, within } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../../app';
import { pickOption } from '../../test-select';
import { session } from '../../lib/api';
import type { TokenCheckView } from './types';

const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
const budgetTimezone = { timezone: 'UTC', period_start: '2026-11-01T00:00:00Z', period_end: '2026-12-01T00:00:00Z', pending: null, version: '3' };
const admin = { principal: { id: 'admin1', display_name: '관리자', email: 'admin@example.test', block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' } }, role: 'admin', grants: [], budget_timezone: budgetTimezone, provider_credentials: [], auth: { mode: 'sso', refresh_url: 'https://auth.example.test/sso/refresh', logout_url: 'https://auth.example.test/sso/logout' } };
const platform = { id: 'cred1', label: 'team-platform', hint: 'a1f3' };
const zero = '0.000000000000';
const view: TokenCheckView = {
  enabled: true,
  range: { from: '2026-11-23T10:00:00Z', to: '2026-11-24T10:00:00Z' },
  compared: { from: '2026-11-23T10:00:00Z', to: '2026-11-24T10:00:00Z' },
  state: { last_attempt_at: '2026-11-24T10:12:00Z', last_success_at: '2026-11-24T09:12:00Z', last_error: 'admin_api_unavailable', minute_convention: 'end', start_minutes: '1', end_minutes: '14' },
  totals: {
    provider_tokens: '1300000', gateway_tokens: '1200000', provider_more_tokens: '100000', gateway_more_tokens: '3000',
    provider_more_usd: '0.500000000000', gateway_more_usd: '0.075000000000', unpriced_models: ['claude-unknown-1'], corrected_requests: '2',
    reasons: [
      { reason: 'cutoff', tokens: '20000', usd: '0.500000000000' },
      { reason: 'unknown', tokens: '0', usd: zero },
      { reason: 'outside', tokens: '80000', usd: null },
      { reason: 'unexplained', tokens: '0', usd: zero },
    ],
  },
  rows: [
    { credential: platform, api_key_id: 'apikey_1', model: 'claude-opus-5-5', provider_tokens: '1220000', gateway_tokens: '1200000', provider_more_tokens: '20000', gateway_more_tokens: '3000', provider_more_usd: '0.500000000000', gateway_more_usd: '0.075000000000', reasons: [{ reason: 'cutoff', tokens: '20000' }] },
    { credential: platform, api_key_id: 'apikey_1', model: 'claude-unknown-1', provider_tokens: '80000', gateway_tokens: '0', provider_more_tokens: '80000', gateway_more_tokens: '0', provider_more_usd: null, gateway_more_usd: zero, reasons: [{ reason: 'outside', tokens: '80000' }] },
  ],
  unmatched_credentials: [{ id: 'cred2', label: 'team-research', hint: '9k2d' }],
};
function stub(routes: (url: string, init?: RequestInit) => Response | undefined) {
  const calls: { url: string; body?: unknown }[] = [];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    calls.push({ url, body: init?.body ? JSON.parse(String(init.body)) : undefined });
    if (url === '/api/me') return Promise.resolve(response(admin));
    if (url === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
    return Promise.resolve(routes(url, init) ?? response({ items: [], next_cursor: null, as_of: 'now' }));
  }));
  return calls;
}
afterEach(() => { cleanup(); session.clear(); history.replaceState(null, '', '/dashboard/'); vi.unstubAllGlobals(); });

test('token_check_screen_says_the_check_is_off_without_an_admin_key', async () => {
  history.replaceState(null, '', '/dashboard/admin/token-check');
  stub(url => url.startsWith('/api/admin/token-check') ? response({ ...view, enabled: false }) : undefined);
  render(<App />);
  await screen.findByText('토큰 대조가 꺼져 있습니다.');
  expect(screen.getByRole('link', { name: '토큰 대조' }).getAttribute('aria-current')).toBe('page');
  // 꺼져 있으면 지난 숫자를 보여 주지 않는다.
  expect(screen.queryByText('Provider가 더 센 토큰의 까닭')).toBeNull();
});

test('token_check_screen_splits_the_provider_surplus_by_reason_and_marks_unknown_prices', async () => {
  history.replaceState(null, '', '/dashboard/admin/token-check');
  const calls = stub(url => url.startsWith('/api/admin/token-check') ? response(view) : undefined);
  render(<App />);
  const card = (await screen.findByRole('heading', { name: 'Provider가 더 센 토큰의 까닭' })).closest('section')!;
  // 까닭은 고정 순서로 모두 보이고, 몫은 Provider가 더 센 토큰 전체에 대한 비율이다.
  const items = within(card).getAllByRole('listitem').map(li => li.querySelector('.reason-figures')!.textContent);
  expect(items).toEqual(['끊긴 요청20,00020%0.500000000000 USD', '사용량을 모르는 요청00%0.000000000000 USD', '게이트웨이 밖 사용80,00080%가격 모름', '원인 모름00%0.000000000000 USD']);
  // 0인 까닭은 막대에 그리지 않는다.
  expect(card.querySelectorAll('.segments > span')).toHaveLength(2);
  expect(screen.getByText('가격을 모르는 모델은 금액에서 뺐습니다: claude-unknown-1')).toBeTruthy();
  expect(screen.getByText(/Anthropic Admin API를 호출하지 못했습니다/)).toBeTruthy();
  expect(screen.getByText('team-research')).toBeTruthy();
  const rows = within(screen.getByRole('table')).getAllByRole('row').slice(1);
  expect(rows.map(r => within(r).getAllByRole('cell')[0].textContent)).toEqual(['claude-opus-5-5', 'claude-unknown-1']);
  expect(within(rows[1]).getByText('가격 모름')).toBeTruthy();
  // 기간을 바꾸면 그 기간으로 다시 읽는다.
  pickOption(screen.getByLabelText('기간'), '최근 7일');
  await vi.waitFor(() => expect(calls.some(c => c.url === '/api/admin/token-check?range=7d')).toBe(true));
});

const rule = (extra: Record<string, unknown>) => ({ budget_timezone: budgetTimezone, cutoff_rule: { provider: 'anthropic', cutoff_rule: 'received', correction: false, correction_available: true, version: '5', ...extra } });

test('cut_off_rule_form_saves_the_correction_switch', async () => {
  history.replaceState(null, '', '/dashboard/admin/settings');
  const calls = stub((url, init) => init?.method === 'PUT' ? response({ command_id: 'cmd', effect: 'changed', result_version: '6' }) : url === '/api/admin/settings' ? response(rule({})) : undefined);
  render(<App />);
  const form = await screen.findByRole('form', { name: '끊긴 요청 정산 (Anthropic)' });
  const box = within(form).getByRole('checkbox', { name: /사후 보정/ }) as HTMLInputElement;
  const save = within(form).getByRole('button', { name: '정산 규칙 저장' }) as HTMLButtonElement;
  expect(box.checked).toBe(false);
  expect(save.disabled).toBe(true);
  fireEvent.click(box);
  expect(save.disabled).toBe(false);
  fireEvent.click(save);
  await screen.findByText('정산 규칙을 저장했습니다. 지금부터 받는 요청에 적용합니다.');
  expect(calls.filter(c => c.body).map(c => c.body)).toEqual([{ provider: 'anthropic', cutoff_rule: 'received', correction: true, expected_version: '5' }]);
});

test('correction_switch_cannot_be_turned_on_without_an_admin_key', async () => {
  history.replaceState(null, '', '/dashboard/admin/settings');
  stub(url => url === '/api/admin/settings' ? response(rule({ correction_available: false })) : undefined);
  render(<App />);
  const form = await screen.findByRole('form', { name: '끊긴 요청 정산 (Anthropic)' });
  expect((within(form).getByRole('checkbox', { name: /사후 보정/ }) as HTMLInputElement).disabled).toBe(true);
  expect(within(form).getByText(/Admin API 키\(ANTHROPIC_ADMIN_API_KEY\)가 없어 켤 수 없습니다/)).toBeTruthy();
});

test('correction_switch_can_still_be_turned_off_after_the_admin_key_is_removed', async () => {
  history.replaceState(null, '', '/dashboard/admin/settings');
  stub(url => url === '/api/admin/settings' ? response(rule({ correction: true, correction_available: false })) : undefined);
  render(<App />);
  const form = await screen.findByRole('form', { name: '끊긴 요청 정산 (Anthropic)' });
  const box = within(form).getByRole('checkbox', { name: /사후 보정/ }) as HTMLInputElement;
  expect(box.checked).toBe(true);
  expect(box.disabled).toBe(false);
});

test('request_detail_shows_a_corrected_output', async () => {
  history.replaceState(null, '', '/dashboard/admin/requests?request_id=r21');
  const usage = (source: string, output: string) => ({ source, actual_model: 'claude-opus-5-5', input_tokens: '31000', output_tokens: output, cache_read_tokens: '0', cache_write_tokens: '0', reasoning_tokens: null, counted_input_tokens: null });
  const detail = {
    id: 'r21', parent_request_id: null, purpose: 'root', principal_id: 'human1', key_id: 'k1', provider: 'anthropic', model: 'claude-opus-5-5', api: 'messages', admitted_at: '2026-11-24T09:00:00Z', finished_at: '2026-11-24T09:01:00Z', phase: 'finished', outcome: 'cancelled', accounting_state: 'settled', cost_usd: '0.210250000000', version: '4', reason_code: 'client_cancelled',
    provider_credential: platform, provider_request_id: null, http_status: null, pricing_snapshot: {}, execution_snapshot: {}, budgets: [], executor_id: 'ex1', execution_stopped_at: '2026-11-24T09:01:00Z', settlement: null,
    usage: { partial: null, final: { ...usage('cutoff_received', '640'), cutoff_rule: 'received', cutoff_fallback_reason: null }, late_final: null, correction: { ...usage('provider_report', '2210'), previous_output_tokens: '640', previous_source: 'cutoff_received' } },
  };
  stub(url => url === '/api/admin/requests/r21' ? response(detail) : undefined);
  render(<App />);
  await screen.findByText('Anthropic 사용량 보고서로 출력 토큰을 640에서 2210(으)로 고쳤습니다. 비용은 고친 값입니다.');
  // 사용량 기록 목록에도 보정 기록이 출처와 함께 보인다.
  const usages = screen.getByRole('region', { name: '선택 usage' });
  expect(within(usages).getByText('correction')).toBeTruthy();
  expect(within(usages).getByText(/출처: provider_report · 입력 31000 \/ 출력 2210/)).toBeTruthy();
});
