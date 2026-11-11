import { render, screen, cleanup } from '@testing-library/react';
import { afterEach, expect, test } from 'vitest';
import { UsageView } from './usage';
import type { Usage } from '../../lib/types';
afterEach(cleanup);
export const fixture: Usage = { currency: 'USD', from: '2026-01-01T00:00:00Z', to: '2026-02-01T00:00:00Z', as_of: '2026-03-10T00:00:00Z', detail_available_from: null, aggregation: 'detail', settled_usd: '0.000000000001', held_reservation_usd: '1.200000000000', period_unknown_count: '0', global_unknown_count: '1', global_expired_dispatch_count: '0', block_state: { blocked: true, reasons: ['unsettled'], global_unknown_count: '1', expired_dispatch_count: '0' }, balance_context: 'current_principal', budget_balances: [{ scope_id: 'scope', scope_kind: 'principal', key_id: null, period_start: '2026-03-01T00:00:00Z', period_end: '2026-04-01T00:00:00Z', timezone: 'UTC', policy_version: '7', config_digest: 'abc', mode: 'actual', limit_usd: '100.000000000000', settled_usd: '3.000000000000', reserved_usd: '2.000000000000', scope_unknown_count: '1', principal_unknown_count: '1', balance_state: 'unresolved', remaining_usd: null, blocked: true, reasons: ['unsettled'], as_of: '2026-03-10T00:00:00Z' }], buckets: { items: [], next_cursor: null, as_of: '2026-03-10T00:00:00Z' } };
test('dashboard_separates_cost_reservation_unknown_and_global_block', () => {
  render(<UsageView usage={fixture} />);
  expect(screen.getByText('0.000000000001 USD')).toBeTruthy();
  expect(screen.getByText('1.200000000000 USD')).toBeTruthy();
  expect(screen.getByText('확인 불가 (미정산)')).toBeTruthy();
  expect(screen.getByText(/전체 기간 미정산: 1/)).toBeTruthy();
  expect(screen.getByText(/하드캡 보장이 아닙니다/)).toBeTruthy();
  expect(screen.getByText(/그룹별 사용량이 없습니다/)).toBeTruthy();
  expect(screen.queryByText('0.000000000000 USD')).toBeNull();
});
