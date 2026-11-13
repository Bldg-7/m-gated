import { cleanup, fireEvent, render, screen, within } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../../app';
import { session } from '../../lib/api';
const me = { principal: { id: 'admin1', display_name: '관리자', email: 'admin@example.test' }, role: 'admin', grants: [], sso: { refresh_url: 'https://auth.example.test/sso/refresh' } };
const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
const rates = (input: string) => ({ input, output: '20.000000000000', cache_read: '0.200000000000', cache_write_5m: '5.000000000000' });
const opusOld = { provider: 'anthropic', model: 'claude-opus-5-5', rates: rates('5.000000000000'), max_input_tokens: 1000000, max_output_tokens: 128000, source: 'models_dev', note: null };
const opusNew = { ...opusOld, rates: rates('4.000000000000') };
const haiku = { provider: 'anthropic', model: 'claude-haiku-5-5', rates: rates('0.100000000000'), max_input_tokens: 100000, max_output_tokens: 128000, source: 'litellm', note: 'tier_input_limit:100000' };
const proposal = { base_version: 1, sources: [{ source: 'models_dev', ok: true, models: 3 }, { source: 'litellm', ok: true, models: 3 }, { source: 'openrouter', ok: false, models: 0 }], items: [
  { provider: 'anthropic', model: 'claude-opus-5-5', status: 'changed', current: opusOld, proposed: opusNew, agree: false, sources: { models_dev: { rates: rates('4.000000000000'), max_input_tokens: 1000000, max_output_tokens: 128000, tier_threshold: null }, litellm: { rates: rates('4.500000000000'), max_input_tokens: 1000000, max_output_tokens: 128000, tier_threshold: null } } },
  { provider: 'anthropic', model: 'claude-haiku-5-5', status: 'new', current: null, proposed: haiku, agree: true, sources: {} },
  { provider: 'anthropic', model: 'claude-sonnet-5-5', status: 'unchanged', current: opusOld, proposed: opusOld, agree: true, sources: {} },
] };
afterEach(() => { cleanup(); session.clear(); history.replaceState(null, '', '/ui/'); vi.unstubAllGlobals(); });

test('sync_shows_old_and_new_values_and_approves_only_checked_items', async () => {
  history.replaceState(null, '', '/ui/admin/models');
  const calls: { url: string; method: string; body?: Record<string, unknown> }[] = [];
  let version = '1';
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input); calls.push({ url, method: init?.method ?? 'GET', body: init?.body ? JSON.parse(String(init.body)) : undefined });
    if (url === '/gateway/me') return Promise.resolve(response(me));
    if (url === '/gateway/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (url === '/gateway/admin/model-catalog/sync') return Promise.resolve(response({ command_id: 'c1', operation: 'sync_model_catalog', proposal_id: 'p1', result_version: '1', effect: 'changed', replayed: false, proposal }, 201));
    if (url === '/gateway/admin/model-catalog/approve') { version = '2'; return Promise.resolve(response({ command_id: 'c2', operation: 'approve_model_catalog', proposal_id: 'p1', result_version: '2', effect: 'changed', replayed: false })); }
    if (url === '/gateway/admin/model-catalog') return Promise.resolve(response({ version, approved_at: '2026-11-13T00:00:00Z', approved_by_email: 'admin@example.test', entries: version === '1' ? [opusOld] : [opusNew, haiku], providers: [{ provider: 'anthropic', api: 'messages', model_prefixes: ['claude-'], required_rates: ['input'] }] }));
    return Promise.resolve(response({ error: { code: 'resource_not_found' } }, 404));
  }));
  render(<App />);
  await screen.findByText('현재 카탈로그 · 버전 1');
  fireEvent.click(screen.getByRole('button', { name: '공개 가격 목록과 동기화' }));
  const region = await screen.findByRole('region', { name: '동기화 변경안' });
  expect(region.textContent).toContain('OpenRouter: 받지 못함');
  // 바뀐 값은 처음부터 골라 두고, 새 모델은 관리자가 직접 고른다. 변경 없는 모델은 목록에 없다.
  const opus = within(region).getByLabelText('claude-opus-5-5 적용') as HTMLInputElement;
  const newModel = within(region).getByLabelText('claude-haiku-5-5 적용') as HTMLInputElement;
  expect(opus.checked).toBe(true);
  expect(newModel.checked).toBe(false);
  expect(within(region).queryByLabelText('claude-sonnet-5-5 적용')).toBeNull();
  expect(region.textContent).toContain('변경 없는 모델 1개');
  expect(region.textContent).toContain('출처 값 불일치');
  const inputRow = within(region).getAllByRole('row').find(r => r.textContent?.includes('claude-opus-5-5'))!;
  expect(inputRow.textContent).toContain('$5');
  expect(inputRow.textContent).toContain('$4');
  expect(inputRow.textContent).toContain('LiteLLM $4.5');
  expect(calls.some(c => c.url.endsWith('/approve'))).toBe(false);
  fireEvent.click(newModel);
  fireEvent.click(within(region).getByRole('button', { name: '고른 2개 승인' }));
  await screen.findByText('카탈로그 버전 2을 적용했습니다.');
  const approve = calls.find(c => c.url.endsWith('/approve'))!;
  expect(approve.body).toEqual({ proposal_id: 'p1', expected_version: '1', items: [{ provider: 'anthropic', model: 'claude-opus-5-5' }, { provider: 'anthropic', model: 'claude-haiku-5-5' }] });
  await screen.findByText('현재 카탈로그 · 버전 2');
  expect(screen.queryByRole('region', { name: '동기화 변경안' })).toBeNull();
});

test('stale_proposal_is_reported_and_not_retried', async () => {
  history.replaceState(null, '', '/ui/admin/models');
  let approves = 0;
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL) => {
    const url = String(input);
    if (url === '/gateway/me') return Promise.resolve(response(me));
    if (url === '/gateway/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (url.endsWith('/sync')) return Promise.resolve(response({ proposal_id: 'p1', proposal }, 201));
    if (url.endsWith('/approve')) { approves++; return Promise.resolve(response({ error: { code: 'catalog_proposal_stale' } }, 409)); }
    return Promise.resolve(response({ version: '1', approved_at: null, approved_by_email: null, entries: [], providers: [] }));
  }));
  render(<App />);
  fireEvent.click(await screen.findByRole('button', { name: '공개 가격 목록과 동기화' }));
  fireEvent.click(await screen.findByRole('button', { name: '고른 1개 승인' }));
  await screen.findByText(/이 변경안을 만든 뒤 카탈로그가 바뀌었습니다/);
  expect(approves).toBe(1);
});
