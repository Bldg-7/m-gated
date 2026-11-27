import { cleanup, fireEvent, render, screen, within } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../../app';
import { session } from '../../lib/api';
const me = { principal: { id: 'admin1', display_name: '관리자', email: 'admin@example.test' }, role: 'admin', grants: [], auth: { mode: 'sso', refresh_url: 'https://auth.example.test/sso/refresh', logout_url: 'https://auth.example.test/sso/logout' } };
const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
const rates = (input: string) => ({ input, output: '20.000000000000', cache_read: '0.200000000000', cache_write_5m: '5.000000000000' });
const opusOld = { provider: 'anthropic', model: 'claude-opus-5-5', rates: rates('5.000000000000'), max_input_tokens: 1000000, max_output_tokens: 128000, source: 'models_dev', note: null };
const opusNew = { ...opusOld, rates: rates('4.000000000000') };
const haiku = { provider: 'anthropic', model: 'claude-haiku-5-5', rates: rates('0.100000000000'), max_input_tokens: 100000, max_output_tokens: 128000, source: 'litellm', note: 'tier_input_limit:100000' };
const proposal = { base_version: 1, source: { source: 'models_dev', models: 3 }, items: [
  { provider: 'anthropic', model: 'claude-opus-5-5', status: 'changed', current: opusOld, proposed: opusNew },
  { provider: 'anthropic', model: 'claude-haiku-5-5', status: 'new', current: null, proposed: haiku },
  { provider: 'anthropic', model: 'claude-sonnet-5-5', status: 'unchanged', current: opusOld, proposed: opusOld },
] };
afterEach(() => { cleanup(); session.clear(); history.replaceState(null, '', '/dashboard/'); vi.unstubAllGlobals(); });

test('sync_shows_old_and_new_values_and_approves_only_checked_items', async () => {
  history.replaceState(null, '', '/dashboard/admin/models');
  const calls: { url: string; method: string; body?: Record<string, unknown> }[] = [];
  let version = '1';
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input); calls.push({ url, method: init?.method ?? 'GET', body: init?.body ? JSON.parse(String(init.body)) : undefined });
    if (url === '/api/me') return Promise.resolve(response(me));
    if (url === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (url === '/api/admin/model-catalog/sync') return Promise.resolve(response({ command_id: 'c1', operation: 'sync_model_catalog', proposal_id: 'p1', result_version: '1', effect: 'changed', replayed: false, proposal }, 201));
    if (url === '/api/admin/model-catalog/approve') { version = '2'; return Promise.resolve(response({ command_id: 'c2', operation: 'approve_model_catalog', proposal_id: 'p1', result_version: '2', effect: 'changed', replayed: false })); }
    if (url === '/api/admin/model-catalog') return Promise.resolve(response({ version, approved_at: '2026-11-13T00:00:00Z', approved_by_email: 'admin@example.test', entries: version === '1' ? [opusOld] : [opusNew, haiku], providers: [{ provider: 'anthropic', api: 'messages', model_prefixes: ['claude-'], required_rates: ['input'], usage_categories: ['input', 'output', 'cache_read', 'cache_write_5m', 'cache_write_1h'] }] }));
    return Promise.resolve(response({ error: { code: 'resource_not_found' } }, 404));
  }));
  render(<App />);
  await screen.findByText('현재 카탈로그 · 버전 1');
  fireEvent.click(screen.getByRole('button', { name: '공개 가격 목록과 동기화' }));
  const region = await screen.findByRole('region', { name: '동기화 변경안' });
  expect(region.textContent).toContain('가격 출처: models.dev (3개 모델)');
  expect(region.textContent).not.toContain('LiteLLM');
  // 바뀐 값은 처음부터 골라 두고, 새 모델은 관리자가 직접 고른다. 변경 없는 모델은 목록에 없다.
  const opus = within(region).getByLabelText('claude-opus-5-5 적용') as HTMLInputElement;
  const newModel = within(region).getByLabelText('claude-haiku-5-5 적용') as HTMLInputElement;
  expect(opus.checked).toBe(true);
  expect(newModel.checked).toBe(false);
  expect(within(region).queryByLabelText('claude-sonnet-5-5 적용')).toBeNull();
  expect(region.textContent).toContain('변경 없는 모델 1개');
  const inputRow = within(region).getAllByRole('row').find(r => r.textContent?.includes('claude-opus-5-5'))!;
  expect(inputRow.textContent).toContain('$5');
  expect(inputRow.textContent).toContain('$4');
  expect(calls.some(c => c.url.endsWith('/approve'))).toBe(false);
  // 전체 선택은 고를 수 있는 항목 전체를 켜고 끈다. 일부만 골랐을 때는 중간 상태로 보인다.
  const all = within(region).getByLabelText('전체 선택') as HTMLInputElement;
  expect(all.checked).toBe(false);
  expect(all.indeterminate).toBe(true);
  fireEvent.click(all);
  expect([opus.checked, newModel.checked, all.checked]).toEqual([true, true, true]);
  fireEvent.click(all);
  expect([opus.checked, newModel.checked]).toEqual([false, false]);
  expect((within(region).getByRole('button', { name: '고른 0개 승인' }) as HTMLButtonElement).disabled).toBe(true);
  fireEvent.click(all);
  fireEvent.click(within(region).getByRole('button', { name: '고른 2개 승인' }));
  await screen.findByText('카탈로그 버전 2을 적용했습니다.');
  const approve = calls.find(c => c.url.endsWith('/approve'))!;
  expect(approve.body).toEqual({ proposal_id: 'p1', expected_version: '1', items: [{ provider: 'anthropic', model: 'claude-opus-5-5' }, { provider: 'anthropic', model: 'claude-haiku-5-5' }] });
  await screen.findByText('현재 카탈로그 · 버전 2');
  expect(screen.queryByRole('region', { name: '동기화 변경안' })).toBeNull();
});

test('stale_proposal_is_reported_and_not_retried', async () => {
  history.replaceState(null, '', '/dashboard/admin/models');
  let approves = 0;
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL) => {
    const url = String(input);
    if (url === '/api/me') return Promise.resolve(response(me));
    if (url === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
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

test('rates_form_sends_each_declared_category_and_an_empty_field_clears_the_rate', async () => {
  history.replaceState(null, '', '/dashboard/admin/models');
  const calls: { url: string; method: string; body?: Record<string, unknown> }[] = [];
  let version = '1';
  const providers = [{ provider: 'anthropic', api: 'messages', model_prefixes: ['claude-'], required_rates: ['input', 'output', 'cache_read', 'cache_write_5m'], usage_categories: ['input', 'output', 'cache_read', 'cache_write_5m', 'cache_write_1h'] }];
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input); calls.push({ url, method: init?.method ?? 'GET', body: init?.body ? JSON.parse(String(init.body)) : undefined });
    if (url === '/api/me') return Promise.resolve(response(me));
    if (url === '/api/csrf') return Promise.resolve(response({ token: 'csrf' }));
    if (url === '/api/admin/model-catalog/rates') { version = '2'; return Promise.resolve(response({ command_id: 'c1', operation: 'edit_model_catalog_rates', target: { kind: 'catalog', id: 'v2' }, result_version: '2', effect: 'changed', replayed: false })); }
    if (url === '/api/admin/model-catalog') return Promise.resolve(response({ version, approved_at: '2026-11-13T00:00:00Z', approved_by_email: 'admin@example.test', entries: version === '1' ? [opusOld] : [{ ...opusOld, rates: { ...opusOld.rates, cache_write_1h: '8.000000000000' } }], providers }));
    return Promise.resolve(response({ error: { code: 'resource_not_found' } }, 404));
  }));
  render(<App />);
  const table = await screen.findByRole('region', { name: '현재 카탈로그' });
  // 그 API가 알린 칸만 열로 보인다. 사는 시간과 관계없는 캐시 쓰기 칸은 Anthropic에 없다.
  expect(within(table).getByRole('columnheader', { name: '캐시 쓰기 1시간' })).toBeTruthy();
  expect(within(table).queryByRole('columnheader', { name: '캐시 쓰기' })).toBeNull();
  fireEvent.click(within(table).getByRole('button', { name: 'claude-opus-5-5 단가 고치기' }));
  const dialog = await screen.findByRole('dialog', { name: 'claude-opus-5-5 단가 고치기' });
  const field = (name: string) => within(dialog).getByLabelText(name) as HTMLInputElement;
  expect(['입력', '출력', '캐시 읽기', '캐시 쓰기 5분', '캐시 쓰기 1시간'].map(name => field(name).value)).toEqual(['5', '20', '0.2', '5', '']);
  // 형식이 틀리면 보내지 않는다.
  fireEvent.change(field('캐시 쓰기 1시간'), { target: { value: '1.2.3' } });
  fireEvent.click(within(dialog).getByRole('button', { name: '단가 저장' }));
  expect(within(dialog).getByRole('alert').textContent).toContain('캐시 쓰기 1시간 단가는');
  expect(calls.some(c => c.url.endsWith('/rates'))).toBe(false);
  fireEvent.change(field('캐시 쓰기 1시간'), { target: { value: '8' } });
  fireEvent.change(field('캐시 읽기'), { target: { value: '' } });
  fireEvent.click(within(dialog).getByRole('button', { name: '단가 저장' }));
  await screen.findByText('현재 카탈로그 · 버전 2');
  const sent = calls.find(c => c.url.endsWith('/rates'))!;
  expect(sent.method).toBe('PUT');
  expect(sent.body).toEqual({ provider: 'anthropic', model: 'claude-opus-5-5', rates: { input: '5', output: '20', cache_read: null, cache_write_5m: '5', cache_write_1h: '8' }, expected_version: '1' });
});
