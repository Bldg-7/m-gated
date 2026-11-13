import type { Reporter, TestCase, TestResult, Suite } from '@playwright/test/reporter';
const titles = [
  'Rust bundle serves precise usage, global block, keyboard and text-only identity',
  '401 refresh is bounded and stale usage never re-enters after logout',
  'retention 409 removes previous figures without silently dropping filters',
  'cross_tab_logout_latch_blocks_visibility_pageshow_and_old_cookie_until_explicit_login',
  'broadcast and bfcache revalidation conceal cached data; server stop returns403',
  'refresh_success_then_current_me403_stays_permission_error_without_login',
  'verified_signed_identity_change_broadcasts_and_discards_other_window_old_payload',
  'personal_keys_five_explicit_durations_one_time_secret_and_bodyless_revoke',
  'lost_issue_response_links_command_without_replay_then_user_revokes_and_reissues',
  'key_read_refresh_once_then_cookie_write401_never_refreshes_or_replays',
  'key_secret_and_forms_disappear_on_bfcache_broadcast_identity_and_cross_tab_logout',
  'delayed_committed_issue_response_never_restores_secret_after_epoch_change',
  'real_navigation_hides_key_before_freeze_and_back_never_restores_secret',
  'key_revoke409_reloads_server_metadata_before_new_user_confirmation',
  'lost_delete_response_get_confirms_revoke_before_explicit_replacement',
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
  's9_csp_blocks_inline_external_scripts_and_forms_have_keyboard_names',
  's9_real_origin_denial_never_refreshes_or_replays_cookie_write',
  'spa_navigation_unmounts_key_and_back_never_restores_secret',
];
export default class SafeReporter implements Reporter {
  private expected = new Set<number>();
  private seen = new Set<number>();
  private invalid = false;
  onBegin(_config: unknown, suite: Suite) {
    for (const test of suite.allTests()) {
      const id = titles.indexOf(test.title);
      if (id < 0 || this.expected.has(id)) this.invalid = true;
      this.expected.add(id);
    }
    if (!this.expected.size) this.invalid = true;
  }
  async onEnd() {
    if (this.invalid || this.expected.size !== this.seen.size) return { status: 'failed' as const };
    return undefined;
  }
  onTestEnd(test: TestCase, result: TestResult) {
    const index = titles.indexOf(test.title);
    if (index < 0 || !this.expected.has(index) || this.seen.has(index) || result.status !== 'passed') this.invalid = true;
    this.seen.add(index);
    const lines = result.errors.map(error => error.stack?.match(/(?:dashboard|keys|admin)\.spec\.ts:(\d+):/)?.[1] ?? 'unknown');
    console.log(`U0_E2E case=${index < 0 ? 'unregistered' : index + 1} status=${result.status} assertion_lines=${lines.join(',') || 'none'}`);
    const adminHttp = (test.annotations ?? []).filter(a => a.type === 'admin-http' && /^(me|settle|policy|release|usage|requests|principals):[1-5][0-9]{2}$/.test(a.description ?? '')).map(a => a.description).slice(-24);
    if (adminHttp.length) console.log(`U0_E2E admin_http=${adminHttp.join(',')}`);
    const phases = (test.annotations ?? []).filter(a => a.type === 'key-phase' && /^(open|cookie|shell|heading|list|prepared|issued)$/.test(a.description ?? '')).map(a => a.description).slice(-20);
    const http = (test.annotations ?? []).filter(a => a.type === 'key-http' && /^(me|keys|csrf|shell):[1-5][0-9]{2}$/.test(a.description ?? '')).map(a => a.description).slice(-20);
    if (phases.length) console.log(`U0_E2E key_phases=${phases.join(',')} http=${http.join(',')}`);
    const grants = (test.annotations ?? []).filter(a => a.type === 'key-grants' && /^[0-9]{1,3}:(true|false):[0-9]{1,3}:[0-9]{1,3}$/.test(a.description ?? '')).map(a => a.description);
    if (grants.length) console.log(`U0_E2E key_grants=${grants.join(',')}`);
    const navigation = (test.annotations ?? []).filter(a => a.type === 'key-navigation' && /^(hide|show):(true|false):(true|false):(true|false)$/.test(a.description ?? '')).map(a => a.description).slice(0, 8);
    if (navigation.length) console.log(`U0_E2E key_navigation=${navigation.join(',')}`);
    const recovery = (test.annotations ?? []).filter(a => a.type === 'key-recovery' && /^[0-9]{1,3}:[0-9]{1,3}:[0-9]{1,3}:[0-9]{1,3}:(true|false):(true|false):(true|false)$/.test(a.description ?? '')).map(a => a.description).slice(0, 1);
    if (recovery.length) console.log(`U0_E2E key_recovery=${recovery.join(',')}`);
  }
  onError() { this.invalid = true; console.log('U0_E2E global_error=runner'); }
}
