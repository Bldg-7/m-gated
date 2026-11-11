import { afterEach, beforeEach, describe, expect, it, test, vi } from 'vitest';
import type { Suite, TestCase, TestResult } from '@playwright/test/reporter';
import SafeReporter from '../../e2e/safe-reporter';

const csp = { title: 's9_csp_blocks_inline_external_scripts_and_forms_have_keyboard_names', annotations: [] } as unknown as TestCase;
const origin = { title: 's9_real_origin_denial_never_refreshes_or_replays_cookie_write', annotations: [] } as unknown as TestCase;
const passed = { status: 'passed', errors: [] } as unknown as TestResult;
const suite = (...cases: TestCase[]) => ({ allTests: () => cases }) as unknown as Suite;
beforeEach(() => { vi.spyOn(console, 'log').mockImplementation(() => {}); });
afterEach(() => vi.restoreAllMocks());

describe('SafeReporter의 유한 사례 수용', () => {
  it('등록된 두 사례를 모두 한 번 통과해야 성공한다', async () => {
    const reporter = new SafeReporter(); reporter.onBegin({}, suite(csp, origin));
    reporter.onTestEnd(csp, passed); reporter.onTestEnd(origin, passed);
    expect(await reporter.onEnd()).toBeUndefined();
  });
  it('미등록·누락·중복·빈 실행은 성공으로 숨기지 않는다', async () => {
    const unknown = { title: 'unregistered synthetic', annotations: [] } as unknown as TestCase;
    for (const [planned, observed] of [[[unknown], [unknown]], [[csp, origin], [csp]], [[csp], [csp, csp]], [[], []]] as const) {
      const reporter = new SafeReporter(); reporter.onBegin({}, suite(...planned));
      for (const test of observed) reporter.onTestEnd(test, passed);
      expect(await reporter.onEnd()).toEqual({ status: 'failed' });
    }
  });
  it.each(['skipped', 'failed', 'timedOut', 'interrupted'])('%s 상태는 통과가 아니다', async status => {
    const reporter = new SafeReporter(); reporter.onBegin({}, suite(csp));
    reporter.onTestEnd(csp, { status, errors: [] } as unknown as TestResult);
    expect(await reporter.onEnd()).toEqual({ status: 'failed' });
  });
  it('전역 runner 오류도 실패한다', async () => {
    const reporter = new SafeReporter(); reporter.onBegin({}, suite(csp));
    reporter.onTestEnd(csp, passed); reporter.onError();
    expect(await reporter.onEnd()).toEqual({ status: 'failed' });
  });
});

test('safe_reporter_assigns_unique_fixed_ids_to_all_review_regressions', () => {
  const log = vi.spyOn(console, 'log').mockImplementation(() => {});
  const titles = [
    'cross_tab_logout_latch_blocks_visibility_pageshow_and_old_cookie_until_explicit_login',
    'refresh_success_then_current_me403_stays_permission_error_without_login',
    'verified_signed_identity_change_broadcasts_and_discards_other_window_old_payload',
  ];
  const reporter = new SafeReporter();
  for (const title of titles) reporter.onTestEnd({ title } as TestCase, { status: 'passed', errors: [] } as unknown as TestResult);
  expect(log.mock.calls.map(call => call[0])).toEqual([4, 6, 7].map(id => `U0_E2E case=${id} status=passed assertion_lines=none`));
});

test('safe_reporter_unknown_title_is_explicit_and_never_leaks_title_or_errors', () => {
  const log = vi.spyOn(console, 'log').mockImplementation(() => {});
  const reporter = new SafeReporter();
  reporter.onTestEnd({ title: 'synthetic-secret-title' } as TestCase, { status: 'failed', errors: [{ stack: 'synthetic-secret-error' }] } as TestResult);
  expect(log.mock.calls).toEqual([['U0_E2E case=unregistered status=failed assertion_lines=unknown']]);
});

test('safe_reporter_registers_all_key_cases_and_drops_unknown_diagnostic_content', () => {
  const log = vi.spyOn(console, 'log').mockImplementation(() => {});
  const titles = [
    'personal_keys_five_explicit_durations_one_time_secret_and_bodyless_revoke',
    'lost_issue_response_links_command_without_replay_then_user_revokes_and_reissues',
    'key_read_refresh_once_then_cookie_write401_never_refreshes_or_replays',
    'key_secret_and_forms_disappear_on_bfcache_broadcast_identity_and_cross_tab_logout',
    'delayed_committed_issue_response_never_restores_secret_after_epoch_change',
    'real_navigation_hides_key_before_freeze_and_back_never_restores_secret',
    'key_revoke409_reloads_server_metadata_before_new_user_confirmation',
    'lost_delete_response_get_confirms_revoke_before_explicit_replacement',
  ];
  const reporter = new SafeReporter();
  for (const title of titles) reporter.onTestEnd({ title, annotations: [{ type: 'key-phase', description: 'synthetic-secret' }, { type: 'key-http', description: 'synthetic-secret' }, { type: 'key-grants', description: 'synthetic-secret' }, { type: 'key-navigation', description: 'synthetic-secret' }] } as TestCase, { status: 'passed', errors: [] } as unknown as TestResult);
  expect(log.mock.calls.map(call => call[0])).toEqual(titles.map((_, index) => `U0_E2E case=${index + 8} status=passed assertion_lines=none`));
});

test('safe_reporter_only_emits_bounded_recovery_count_and_boolean_tuple', () => {
  const log = vi.spyOn(console, 'log').mockImplementation(() => {});
  new SafeReporter().onTestEnd({ title: 'lost_delete_response_get_confirms_revoke_before_explicit_replacement', annotations: [
    { type: 'key-recovery', description: 'synthetic-secret' },
    { type: 'key-recovery', description: '1:1:2:1:false:false:false' },
  ] } as TestCase, { status: 'passed', errors: [] } as unknown as TestResult);
  expect(log.mock.calls.map(call => call[0])).toEqual([
    'U0_E2E case=15 status=passed assertion_lines=none',
    'U0_E2E key_recovery=1:1:2:1:false:false:false',
  ]);
});

test('safe_reporter_registers_admin_cases_and_only_exposes_bounded_http_enums', () => {
  const log = vi.spyOn(console, 'log').mockImplementation(() => {});
  const titles = [
    'admin_principal_separate_three_cas_ack_get_policy_conflict_and_safety',
    'admin_service_create_deny_all_owner_authority_service_key_and_global_keys',
    'admin_manual_runtime_and_confirmation_records_are_not_provider_evidence',
    'admin_two_admin_settlement_race_applies_once_and_active_sender_stays_unknown',
    'admin_usage_current_balances_period_totals_and_retention409_remove_old_amounts',
    'admin_direct_ids_role_demotion403_purges_forms_cache_and_late_payload',
    'admin_failed_empty_loading_and_lost_ack_never_replays_writes',
    'admin_cursor_pages_and_selected_key_balance_remain_server_scoped',
    'admin_self_fence_and_stale_safety_require_separate_explicit_confirmation',
    'admin_mutual_last_admin_policy_race_preserves_one_current_admin',
  ];
  const reporter = new SafeReporter();
  for (const title of titles) reporter.onTestEnd({ title, annotations: [{ type: 'admin-http', description: 'synthetic-secret' }, { type: 'admin-http', description: 'policy:409' }] } as TestCase, { status: 'passed', errors: [] } as unknown as TestResult);
  expect(log.mock.calls.map(call => call[0])).toEqual(titles.flatMap((_, index) => [`U0_E2E case=${index + 16} status=passed assertion_lines=none`, 'U0_E2E admin_http=policy:409']));
});
