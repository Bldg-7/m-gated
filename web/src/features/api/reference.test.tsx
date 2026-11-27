import { cleanup, fireEvent, render, screen, within } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../../app';
import { session } from '../../lib/api';
import { curl, example, operations, Prose, typeLabel, type ApiDocument } from './reference';

const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
const me = (role: 'user' | 'admin') => ({ principal: { id: 'p1', display_name: 'Synthetic', email: 'synthetic@example.test', block_state: { blocked: false, reasons: [], global_unknown_count: '0', expired_dispatch_count: '0' } }, role, grants: [], provider_credentials: [], auth: { mode: 'sso', refresh_url: 'https://auth.example.test/sso/refresh', logout_url: 'https://auth.example.test/sso/logout' } });
const error = (codes: string[]) => ({ description: 'Error', 'x-error-codes': codes, content: { 'application/json': { schema: { $ref: '#/components/schemas/Error' } } } });
/** 화면이 읽는 규칙만 담은 작은 문서. 실제 문서는 api/management.openapi.json이며 서버 시험이 검사한다. */
const doc: ApiDocument = {
  info: { title: 'm-gated management API', version: '1', description: 'Send a token in `Authorization`.\n\nWrites need a command ID.' },
  tags: [{ name: 'Me' }, { name: 'Access list' }],
  'x-common-errors': { '405': ['method_not_allowed'], '503': ['db_unavailable'] },
  components: { schemas: {
    Error: { type: 'object', required: ['error'], properties: { error: { type: 'object' } } },
    Entry: { type: 'object', required: ['id', 'role'], properties: { id: { type: 'string', format: 'uuid' }, role: { enum: ['user', 'admin'] }, principal: { type: ['object', 'null'], properties: { id: { type: 'string' } } } } },
  } },
  paths: {
    '/api/me': { get: { operationId: 'getMe', tags: ['Me'], summary: 'Read my account', responses: { '200': { description: 'OK', content: { 'application/json': { schema: { type: 'object', properties: { role: { type: 'string' } } } } } } }, 'x-role': 'user', 'x-token-scope': 'read', 'x-command-id': false } },
    '/api/admin/access/{id}': {
      parameters: [{ name: 'id', in: 'path', required: true, schema: { type: 'string', format: 'uuid' } }],
      patch: {
        operationId: 'updateAccess', tags: ['Access list'], summary: 'Change a role', description: 'Changes the `role` of one entry.',
        parameters: [{ name: 'X-Gateway-Command-ID', in: 'header', required: true, schema: { type: 'string', format: 'uuid' } }],
        requestBody: { required: true, content: { 'application/json': { schema: { type: 'object', required: ['role', 'expected_version'], additionalProperties: false, properties: { role: { enum: ['user', 'admin'] }, expected_version: { type: 'string', description: 'The version you read.' } } } } } },
        responses: { '200': { description: 'Changed', content: { 'application/json': { schema: { type: 'object', properties: { access: { $ref: '#/components/schemas/Entry' } } } } } }, '409': error(['version_conflict', 'access_protected']) },
        'x-role': 'admin', 'x-token-scope': 'write', 'x-command-id': true,
      },
    },
    '/api/csrf': { get: { operationId: 'getCsrf', tags: ['Me'], summary: 'Get a CSRF token', responses: {}, 'x-role': 'user', 'x-token-scope': null, 'x-command-id': false } },
  },
};
afterEach(() => { cleanup(); session.clear(); history.replaceState(null, '', '/dashboard/'); vi.unstubAllGlobals(); });
function stub(role: 'user' | 'admin') {
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL) => {
    const url = String(input);
    if (url === '/api/me') return Promise.resolve(response(me(role)));
    if (url === '/api/openapi.json') return Promise.resolve(response(doc));
    return Promise.resolve(response({ items: [], next_cursor: null, as_of: '2026-11-01T00:00:00Z' }));
  }));
}

test('operations_merge_path_parameters_and_labels_read_schema_types', () => {
  const list = operations(doc);
  expect(list.map(e => `${e.method} ${e.path}`)).toEqual(['get /api/me', 'patch /api/admin/access/{id}', 'get /api/csrf']);
  expect(list[1].parameters.map(p => p.name)).toEqual(['id', 'X-Gateway-Command-ID']);
  expect(typeLabel(doc, { type: ['string', 'null'] })).toBe('string | null');
  expect(typeLabel(doc, { type: 'array', items: { $ref: '#/components/schemas/Entry' } })).toBe('Entry[]');
  expect(typeLabel(doc, { enum: ['read', 'write'] })).toBe('"read" | "write"');
  expect(example(doc, { $ref: '#/components/schemas/Entry' })).toEqual({ id: '00000000-0000-4000-8000-000000000000', role: 'user' });
  // null일 수 있는 값은 null로 둔다.
  expect(example(doc, { type: 'object', required: ['budget', 'rpm'], properties: { budget: { type: ['object', 'null'], properties: { limit_usd: { type: 'string' } } }, rpm: { type: ['integer', 'null'], minimum: 1 } } })).toEqual({ budget: null, rpm: null });
});

test('curl_example_uses_the_token_header_command_id_and_an_example_body', () => {
  const entry = operations(doc)[1];
  expect(curl(doc, entry, 'https://gw.example.test')).toBe([
    "curl -sS -X PATCH 'https://gw.example.test/api/admin/access/{id}'",
    '-H "Authorization: Bearer $M_GATED_TOKEN"',
    '-H "X-Gateway-Command-ID: $(uuidgen)"',
    "-H 'Content-Type: application/json'",
    `-d '{"role":"user","expected_version":"string"}'`,
  ].join(' \\\n  '));
});

test('reference_lists_routes_by_group_and_shows_one_route_with_permissions_and_errors', async () => {
  stub('admin');
  history.replaceState(null, '', '/dashboard/api'); render(<App />);
  const overview = await screen.findByRole('article', { name: 'API 개요' });
  expect(within(overview).getByText('Authorization')).toBeTruthy();
  expect(within(overview).getByText('method_not_allowed')).toBeTruthy();
  const nav = screen.getByRole('navigation', { name: 'API 경로 목록' });
  fireEvent.click(within(nav).getByRole('link', { name: 'PATCH /api/admin/access/{id}' }));
  const detail = await screen.findByRole('article', { name: 'PATCH /api/admin/access/{id}' });
  expect(location.hash).toBe('#updateAccess');
  for (const text of ['관리자만', '쓰기 토큰이 필요함', '명령 ID 필요', 'version_conflict', 'access_protected', 'expected_version', 'The version you read.']) expect(within(detail).getAllByText(text).length).toBeGreaterThan(0);
  expect(within(detail).getByLabelText('curl 예시').textContent).toContain('X-Gateway-Command-ID');
  // 관리 토큰으로 부를 수 없는 경로는 curl 대신 안내를 보인다.
  fireEvent.click(within(nav).getByRole('link', { name: 'GET /api/csrf' }));
  await screen.findByText('이 경로는 관리 토큰으로 부를 수 없습니다. 대시보드에서 쓰세요.');
  expect(screen.getByText('관리 토큰으로 부를 수 없음')).toBeTruthy();
});

test('a_user_sees_only_user_routes', async () => {
  stub('user');
  history.replaceState(null, '', '/dashboard/api'); render(<App />);
  const nav = await screen.findByRole('navigation', { name: 'API 경로 목록' });
  expect(within(nav).getByRole('link', { name: 'GET /api/me' })).toBeTruthy();
  expect(within(nav).queryByRole('link', { name: 'PATCH /api/admin/access/{id}' })).toBeNull();
  expect(within(nav).queryByText('Access list')).toBeNull();
});

test('prose_renders_headings_lists_code_blocks_and_inline_code', () => {
  const { container } = render(<Prose text={'First `code` line\nsame paragraph.\n\n## Errors\n\n- `a` (400): one\n- two\n\n```json\n{"error": {}}\n```\nLast.'} />);
  expect([...container.children].map(e => e.tagName)).toEqual(['P', 'H3', 'UL', 'PRE', 'P']);
  expect(container.querySelector('p')?.textContent).toBe('First code line same paragraph.');
  expect(container.querySelectorAll('li')).toHaveLength(2);
  expect(container.querySelector('li code')?.textContent).toBe('a');
  expect(container.querySelector('pre')?.textContent).toBe('{"error": {}}');
});
