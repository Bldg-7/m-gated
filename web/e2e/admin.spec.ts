import { test, expect, type BrowserContext, type Page, type Route } from '@playwright/test';
const origin = 'https://m-gated.example.test', transport = process.env.E2E_BASE_URL!;
const service = process.env.E2E_SERVICE_ID!, alice = process.env.E2E_ADMIN_ID!, bob = process.env.E2E_BOB_ID!;
test.use({ timezoneId: 'UTC' });
function barrier() { let release!: () => void; const promise = new Promise<void>(resolve => { release = resolve; }); return { promise, release }; }
async function auth(context: BrowserContext, token = process.env.E2E_TOKEN!) { await context.addCookies([{ name: 'mgated_access', value: token, url: origin, secure: true, httpOnly: true, sameSite: 'Lax' }]); }
async function forward(route: Route, base = transport) { const r = route.request(), u = new URL(r.url()); return route.fetch({ url: `${base}${u.pathname}${u.search}`, headers: await r.allHeaders(), maxRetries: 0, maxRedirects: 0 }); }
async function open(page: Page, context: BrowserContext, path: string) { page.on('response', r => { const p = new URL(r.url()).pathname; if (p.startsWith('/gateway/')) test.info().annotations.push({ type: 'admin-http', description: `${p === '/gateway/me' ? 'me' : p.includes('/settlement') ? 'settle' : p.includes('/policy') ? 'policy' : p.includes('/release') ? 'release' : p.includes('/usage') ? 'usage' : p.includes('/requests') ? 'requests' : 'principals'}:${r.status()}` }); }); await auth(context); await page.goto(`${origin}${path}`); }
async function execute(page: Page) { await page.getByRole('button', { name: '명령 실행', exact: true }).focus(); await page.keyboard.press('Enter'); await expect(page.getByRole('status').filter({ hasText: '서버 재조회 완료' })).toBeVisible(); }
async function json(page: Page, path: string) { return page.evaluate(async path => { const r = await fetch(path); return { status: r.status, body: await r.json() }; }, path); }
async function cookieCommand(page: Page, path: string, body: unknown) { return page.evaluate(async ({ path, body }) => { const csrf = await (await fetch('/gateway/csrf')).json(); const r = await fetch(path, { method: 'PUT', headers: { 'Content-Type': 'application/json', 'X-CSRF-Token': csrf.token, 'X-Gateway-Command-ID': crypto.randomUUID() }, body: JSON.stringify(body) }); return { status: r.status, body: await r.json() }; }, { path, body }); }
async function prepareSettlement(page: Page, cost: string, reason: string) { await page.getByLabel('처리 결정', { exact: true }).selectOption(cost === '0.000000000000' ? 'no_charge' : 'charged'); await page.getByLabel('처리 금액 USD', { exact: true }).fill(cost); await page.getByLabel('수동 처리 사유', { exact: true }).fill(reason); const confirm = page.getByLabel('대상 executor 실행 종료를 확인했습니다 (관리자 진술)', { exact: true }); if (await confirm.count()) await confirm.check(); await page.getByRole('button', { name: '수동 정산 확인', exact: true }).click(); }
test.beforeEach(async ({ context }) => { await context.route('**/*', async route => { if (new URL(route.request().url()).origin !== origin) return route.abort('blockedbyclient'); const response = await forward(route); await route.fulfill({ response }); }); });

test('admin_principal_separate_three_cas_ack_get_policy_conflict_and_safety', async ({ page, context }) => {
  const writes: { method: string; version: string; path: string; command: string }[] = []; const order: string[] = [];
  page.on('request', r => { const u = new URL(r.url()); if (u.pathname.startsWith('/gateway/admin/principals/')) { order.push(`${r.method()}:${u.pathname}`); if (r.method() !== 'GET') writes.push({ method: r.method(), version: r.postDataJSON().expected_version, path: u.pathname, command: r.headers()['x-gateway-command-id'] }); } });
  await open(page, context, `/ui/admin/principals?principal_id=${service}`);
  const detail = page.getByRole('region', { name: '주체 상세', exact: true });
  await expect(detail).toContainText('주체 버전 7 · 정책 버전 13 · 안전 버전 29');
  await detail.getByLabel('서비스 이름', { exact: true }).fill('키보드 서비스');
  await detail.getByLabel('서비스 owner ID', { exact: true }).fill(bob);
  await detail.getByRole('button', { name: '서비스 변경 확인', exact: true }).focus(); await page.keyboard.press('Enter');
  await execute(page); expect(writes[0].version).toBe('7'); await expect(detail).toContainText(`owner: ${bob}`);
  const limits = JSON.parse(await detail.getByLabel('Limits JSON', { exact: true }).inputValue()); limits.rpm = 11; await detail.getByLabel('Limits JSON', { exact: true }).fill(JSON.stringify(limits));
  await detail.getByLabel('정책 변경 사유', { exact: true }).fill('안전차단 비승계 정책 확인');
  await detail.getByRole('button', { name: '정책 교체 확인', exact: true }).click(); await execute(page);
  expect(writes[1].version).toBe('13'); await expect(detail.getByRole('region', { name: '자동 안전차단', exact: true })).toContainText('차단 중 · 안전 버전 29');
  await detail.getByLabel('정책 변경 사유', { exact: true }).fill('stale 편집');
  await detail.getByRole('button', { name: '정책 교체 확인', exact: true }).click();
  await page.request.post(`${process.env.E2E_CONTROL_URL}/stale-policy`);
  await execute(page); await expect(detail).toContainText('버전이 변경되었습니다.'); expect(writes).toHaveLength(3);
  const latest = await json(page, `/gateway/admin/principals/${service}`); expect(latest.body.principal.policy.version).toBe('15');
  await detail.getByLabel('안전차단 해제 사유', { exact: true }).fill('원인과 unknown 유지 확인');
  await detail.getByLabel('과금 원인과 남은 차단을 확인했습니다', { exact: true }).check();
  await detail.getByRole('button', { name: '명시 해제 확인', exact: true }).click(); await execute(page);
  expect(writes[3].version).toBe('29');
  const released = await json(page, `/gateway/admin/principals/${service}`); expect(released.body.principal.inference_safety.paused).toBe(false); expect(released.body.principal.block_state.global_unknown_count).toBe('5'); expect(released.body.principal.block_state.blocked).toBe(true);
  for (const write of writes) { const i = order.indexOf(`${write.method}:${write.path}`); expect(order.slice(i + 1)).toContain(`GET:/gateway/admin/principals/${service}`); expect(write.command).toMatch(/^[0-9a-f-]{36}$/); }
  expect(new Set(writes.map(w => w.command)).size).toBe(writes.length);
});

test('admin_service_create_deny_all_owner_authority_service_key_and_global_keys', async ({ page, context, browser }) => {
  await open(page, context, '/ui/admin/principals');
  await page.getByLabel('새 서비스 이름', { exact: true }).fill('새 서비스'); await page.getByLabel('새 서비스 owner ID', { exact: true }).fill(process.env.E2E_USER_ID!);
  await page.getByRole('button', { name: '서비스 생성 확인', exact: true }).click(); await execute(page);
  const detail = page.getByRole('region', { name: '주체 상세', exact: true }); await expect(detail.getByLabel('Grant JSON', { exact: true })).toHaveValue('[]');
  const id = (await json(page, '/gateway/admin/principals?kind=service')).body.items.find((p: { display_name: string }) => p.display_name === '새 서비스').id;
  const owner = await browser.newContext(); await auth(owner, process.env.E2E_USER_TOKEN!); const ownerPage = await owner.newPage();
  await owner.route('**/*', async route => { if (new URL(route.request().url()).origin !== origin) return route.abort(); return route.fulfill({ response: await forward(route) }); });
  await ownerPage.goto(`${origin}/ui/admin/principals?principal_id=${id}`); await expect(ownerPage.getByRole('alert')).toContainText('관리자 권한이 없습니다'); expect((await json(ownerPage, `/gateway/admin/principals/${id}`)).status).toBe(403); await owner.close();
  await page.goto(`${origin}/ui/admin/principals?principal_id=${service}`); const keys = page.getByRole('region', { name: '주체 상세', exact: true });
  await keys.getByLabel('키 이름', { exact: true }).fill('서비스 키'); expect(await keys.getByLabel('유효기간', { exact: true }).inputValue()).toBe(''); await keys.getByLabel('유효기간', { exact: true }).selectOption('unlimited'); await keys.getByLabel('anthropic / synthetic-text / messages', { exact: true }).check();
  await keys.getByRole('button', { name: '발급 내용 확인', exact: true }).click(); await keys.getByRole('button', { name: '발급 확인', exact: true }).click(); await expect(keys.getByRole('region', { name: '발급 원문', exact: true })).toBeVisible();
  const secret = await keys.getByRole('region', { name: '발급 원문', exact: true }).locator('code').textContent(); await keys.getByRole('button', { name: '원문 닫기', exact: true }).click();
  await keys.getByRole('button', { name: '서비스 키 회수', exact: true }).click(); await keys.getByRole('button', { name: '회수 확인', exact: true }).click(); await expect(keys.getByRole('button', { name: '서비스 키 회수', exact: true })).toHaveCount(0);
  await page.getByText('전체 키 조회·회수', { exact: true }).click(); await page.getByRole('button', { name: '키 조회', exact: true }).first().click(); await expect(page.locator('details').filter({ has: page.getByText('전체 키 조회·회수', { exact: true }) })).toContainText('서비스 키');
  expect(await page.locator('main').textContent()).not.toContain(secret); expect(await page.evaluate(() => [localStorage.length, sessionStorage.length])).toEqual([0, 0]);
});

test('admin_manual_runtime_and_confirmation_records_are_not_provider_evidence', async ({ page, context }) => {
  for (const [id, cost, reason, source] of [[process.env.E2E_RUNTIME_REQUEST!, '1.230000000000', 'runtime 확인 수동 입력', 'runtime 종료 기록'], [process.env.E2E_CONFIRM_REQUEST!, '0.000000000000', '관리자 확인 무과금', '관리자 종료 확인 진술']] as const) {
    await open(page, context, `/ui/admin/requests?request_id=${id}`); await expect(page.getByRole('heading', { name: '요청 상세', exact: true })).toBeVisible();
    if (source !== 'runtime 종료 기록') { const missing = await cookieCommand(page, `/gateway/admin/requests/${id}/settlement`, { expected_version: (await json(page, `/gateway/admin/requests/${id}`)).body.version, decision: 'no_charge', cost_usd: '0.000000000000', reason: '종료 확인 누락' }); expect(missing.status).toBe(409); expect(missing.body.error.code).toBe('execution_not_final'); }
    await prepareSettlement(page, cost, reason); const commands: string[] = []; page.on('request', r => { if (r.method() === 'PUT') commands.push(r.headers()['x-gateway-command-id']); });
    await execute(page); const record = page.getByRole('region', { name: '수동 처리 기록', exact: true }); await expect(record).toContainText('관리자 수동 입력 (Provider 검증값 아님)'); await expect(record).toContainText(cost); await expect(record).toContainText(reason); await expect(record).toContainText(alice); await expect(record).toContainText(source); await expect(record).toContainText('manual_entry'); await expect(page.getByRole('form', { name: '수동 정산', exact: true })).toHaveCount(0); expect(commands).toHaveLength(1);
  }
  const usage = await json(page, `/gateway/admin/usage?principal_id=${service}&group_by=provider`); expect(usage.body.global_unknown_count).toBe('3'); expect(usage.body.buckets.items[0].input_tokens).toBeNull(); expect(usage.body.buckets.items[0].tokens_complete).toBe(false); expect(usage.body.budget_balances[0].remaining_usd).toBeNull();
});

test('admin_two_admin_settlement_race_applies_once_and_active_sender_stays_unknown', async ({ page, context, browser }) => {
  const id = process.env.E2E_RACE_REQUEST!; await open(page, context, `/ui/admin/requests?request_id=${id}`); await prepareSettlement(page, '2.000000000000', '첫 관리자 결정');
  const other = await browser.newContext(); await auth(other, process.env.E2E_BOB_TOKEN!); await other.route('**/*', async route => { if (new URL(route.request().url()).origin !== origin) return route.abort(); return route.fulfill({ response: await forward(route, process.env.E2E_PEER_URL!) }); }); const second = await other.newPage(); await second.goto(`${origin}/ui/admin/requests?request_id=${id}`); await prepareSettlement(second, '3.000000000000', '둘째 관리자 상충');
  const firstEntered = barrier(), secondEntered = barrier(), release = barrier();
  for (const [p, entered] of [[page, firstEntered], [second, secondEntered]] as const) await p.route(`${origin}/gateway/admin/requests/${id}/settlement`, async route => { entered.release(); await release.promise; const response = await forward(route, p === second ? process.env.E2E_PEER_URL! : transport); test.info().annotations.push({ type: 'admin-http', description: `settle:${response.status()}` }); return route.fulfill({ response }); });
  try { await Promise.all([page.getByRole('button', { name: '명령 실행', exact: true }).click(), second.getByRole('button', { name: '명령 실행', exact: true }).click()]); await Promise.all([firstEntered.promise, secondEntered.promise]); release.release(); await expect(page.getByRole('region', { name: '수동 처리 기록', exact: true })).toBeVisible(); await expect(second.getByRole('region', { name: '수동 처리 기록', exact: true })).toBeVisible(); const r = await json(page, `/gateway/admin/requests/${id}`); expect(['2.000000000000', '3.000000000000']).toContain(r.body.cost_usd); const loser = r.body.settlement.actor_principal_id === alice ? second : page; await expect(loser.getByRole('status').filter({ hasText: '상충' })).toBeVisible(); } finally { release.release(); await other.close(); }
  const active = process.env.E2E_ACTIVE_REQUEST!; await page.goto(`${origin}/ui/admin/requests?request_id=${active}`); await prepareSettlement(page, '4.000000000000', '실행 중 거부'); await execute(page); await expect(page.getByRole('status').filter({ hasText: '알려진 sender가 실행 중' })).toBeVisible(); expect((await json(page, `/gateway/admin/requests/${active}`)).body.accounting_state).toBe('unknown');
});

test('admin_cursor_pages_and_selected_key_balance_remain_server_scoped', async ({ page, context }) => {
  await open(page, context, '/ui/admin/principals');
  await page.getByLabel('주체 페이지 크기', { exact: true }).fill('1');
  const firstResponse = page.waitForResponse(r => r.url().includes('/gateway/admin/principals?limit=1') && r.status() === 200);
  await page.getByRole('button', { name: '주체 조회', exact: true }).click(); const first = await (await firstResponse).json(); expect(first.items).toHaveLength(1);
  const nextResponse = page.waitForResponse(r => r.url().includes('/gateway/admin/principals?cursor=') && r.status() === 200);
  await page.getByRole('button', { name: '다음 주체 페이지', exact: true }).click(); const next = await (await nextResponse).json(); expect(next.as_of).toBe(first.as_of); expect(next.items[0].id).not.toBe(first.items[0].id);
  const cross = await json(page, `/gateway/admin/requests?cursor=${encodeURIComponent(first.next_cursor)}`); expect(cross.status).toBe(400); expect(cross.body.error.code).toBe('invalid_cursor');
  await page.goto(`${origin}/ui/admin/requests`); await page.getByLabel('요청 페이지 크기', { exact: true }).fill('1'); await page.getByRole('button', { name: '요청 조회', exact: true }).click(); await expect(page.getByRole('button', { name: '다음 요청 페이지', exact: true })).toBeVisible(); await page.getByRole('button', { name: '다음 요청 페이지', exact: true }).click(); await expect(page.getByRole('button', { name: '서버 다시 조회', exact: true })).toBeEnabled();
  const keys = await json(page, `/gateway/admin/keys?principal_id=${service}`); expect(keys.body.items.length).toBeGreaterThan(0); const key = keys.body.items[0];
  await page.goto(`${origin}/ui/admin/usage?principal_id=${service}`); await page.getByLabel('키 ID', { exact: true }).fill(key.id); await page.getByRole('button', { name: '조회', exact: true }).click(); await expect(page.getByRole('heading', { name: `선택 키 ${key.id}`, exact: true })).toBeVisible();
  const usage = await json(page, `/gateway/admin/usage?principal_id=${service}&key_id=${key.id}`); expect(usage.body.balance_context).toBe('current_principal_and_key'); expect(usage.body.budget_balances).toHaveLength(2); expect(usage.body.budget_balances.map((b: {scope_kind: string}) => b.scope_kind)).toEqual(['principal', 'key']); expect(usage.body.budget_balances.every((b: {remaining_usd: string | null}) => b.remaining_usd === null)).toBe(true);
});

test('admin_usage_current_balances_period_totals_and_retention409_remove_old_amounts', async ({ page, context }) => {
  await open(page, context, '/ui/admin/usage'); await expect(page.getByText('전체 합계: 주체별 예산을 합산한 잔액은 제공하지 않습니다.', { exact: true })).toBeVisible();
  await page.getByLabel('주체 ID', { exact: true }).fill(service); await page.getByRole('button', { name: '조회', exact: true }).click(); await expect(page.getByText('확인 불가 (미정산)', { exact: true })).toBeVisible();
  await expect(page.getByText('실사용량 모드: 진행 호출 때문에 초과할 수 있으며 하드캡 보장이 아닙니다.', { exact: true })).toBeVisible();
  const usage = (await json(page, `/gateway/admin/usage?principal_id=${service}`)).body;
  const from = new Date(usage.from), to = new Date(from); to.setUTCMonth(to.getUTCMonth() + 1);
  const oldFigures = await page.locator('table').textContent(); expect(oldFigures).toContain('USD');
  await page.request.post(`${process.env.E2E_CONTROL_URL}/retention`);
  await page.getByRole('button', { name: '조회', exact: true }).click(); await expect(page.getByRole('alert')).toContainText('상세 가용 시작:'); await expect(page.getByRole('heading', { name: '조회 기간 사용량', exact: true })).toHaveCount(0); await expect(page.locator('table')).toHaveCount(0); expect(await page.getByLabel('그룹', { exact: true }).inputValue()).toBe('provider');
  await page.getByLabel('시작 (현지 시각)', { exact: true }).fill(from.toISOString().slice(0, 16)); await page.getByLabel('종료 (현지 시각)', { exact: true }).fill(to.toISOString().slice(0, 16)); await page.getByLabel('그룹', { exact: true }).selectOption(''); await page.getByRole('button', { name: '조회', exact: true }).click(); await expect(page.locator('p', { hasText: '집계 모드: 보관된 기간 합계' })).toBeVisible(); await expect(page.getByText('상세 가용 시작:', { exact: false })).toBeVisible();
});

test('admin_direct_ids_role_demotion403_purges_forms_cache_and_late_payload', async ({ page, context }) => {
  await open(page, context, `/ui/admin/principals?principal_id=${service}`); const detail = page.getByRole('region', { name: '주체 상세', exact: true }); await expect(detail.getByLabel('서비스 이름', { exact: true })).toBeVisible(); await detail.getByLabel('정책 변경 사유', { exact: true }).fill('비공개 관리자 폼');
  const entered = barrier(), release = barrier();
  await page.route(`${origin}/gateway/admin/principals/${service}`, async route => { const r = await forward(route); entered.release(); await release.promise; try { await route.fulfill({ response: r }); } catch { /* 취소된 이전 epoch 응답 폐기 */ } });
  try {
    await detail.getByRole('button', { name: '서버 다시 조회', exact: true }).click(); await entered.promise;
    await page.route(`${origin}/gateway/admin/principals`, async route => route.fulfill({ response: await forward(route, process.env.E2E_DEMOTED_URL!) }));
    await page.route(`${origin}/gateway/me`, async route => route.fulfill({ response: await forward(route, process.env.E2E_DEMOTED_URL!) }));
    await page.getByRole('button', { name: '주체 조회', exact: true }).click(); await expect(page.getByRole('alert')).toContainText('관리자 권한이 없습니다'); release.release();
    await expect(page.getByRole('region', { name: '주체 상세', exact: true })).toHaveCount(0); expect(await page.locator('main').textContent()).not.toContain('비공개 관리자 폼'); await expect(page.getByRole('link', { name: '주체·정책·키 관리', exact: true })).toHaveCount(0);
  } finally { release.release(); }
  const regular = await context.newPage(); await auth(context, process.env.E2E_USER_TOKEN!); await regular.goto(`${origin}/ui/admin/requests?request_id=${process.env.E2E_RUNTIME_REQUEST}`); await expect(regular.getByRole('alert')).toContainText('관리자 권한이 없습니다'); expect((await json(regular, `/gateway/admin/requests/${process.env.E2E_RUNTIME_REQUEST}`)).status).toBe(403); expect((await json(regular, `/gateway/admin/principals/${service}`)).status).toBe(403); await regular.close();
});

test('admin_failed_empty_loading_and_lost_ack_never_replays_writes', async ({ page, context }) => {
  await open(page, context, '/ui/admin/requests'); await page.getByLabel('요청 주체 ID', { exact: true }).fill('00000000-0000-4000-8000-000000000001'); await page.getByRole('button', { name: '요청 조회', exact: true }).click(); await expect(page.getByText('일치하는 요청이 없습니다.', { exact: true })).toBeVisible();
  await page.getByLabel('요청 ID', { exact: true }).fill('00000000-0000-4000-8000-000000000001'); await page.getByRole('button', { name: '요청 상세 조회', exact: true }).click(); await expect(page.getByRole('region', { name: '요청 상세', exact: true }).getByRole('alert')).toContainText('대상이 없거나');
  await page.goto(`${origin}/ui/admin/principals?principal_id=${service}`); const detail = page.getByRole('region', { name: '주체 상세', exact: true }); await expect(detail.getByLabel('서비스 이름', { exact: true })).toBeVisible();
  let writes = 0; await page.route(`${origin}/gateway/admin/principals/${service}`, async route => { if (route.request().method() !== 'PATCH') return route.fallback(); writes++; const response = await forward(route); expect(response.status()).toBe(200); await response.dispose(); return route.abort('failed'); });
  await detail.getByLabel('서비스 이름', { exact: true }).fill('유실 Ack 후 GET 서비스'); await detail.getByRole('button', { name: '서비스 변경 확인', exact: true }).click(); await execute(page); await expect(detail.getByLabel('서비스 이름', { exact: true })).toHaveValue('유실 Ack 후 GET 서비스'); expect(writes).toBe(1); await expect(detail).toContainText('쓰기는 재전송하지 않았습니다');
  const entered = barrier(), release = barrier(); await page.route(`${origin}/gateway/admin/principals/${service}`, async route => { if (route.request().method() !== 'GET') return route.fallback(); entered.release(); await release.promise; await route.abort('failed'); });
  try { await detail.getByRole('button', { name: '서버 다시 조회', exact: true }).click(); await entered.promise; await expect(detail.getByRole('status').filter({ hasText: '서버 조회 중' })).toBeVisible(); await expect(detail.getByLabel('서비스 이름', { exact: true })).toHaveCount(0); release.release(); await expect(detail.getByRole('alert')).toContainText('서버에 연결할 수 없습니다'); } finally { release.release(); }
  expect(writes).toBe(1);
});


test('admin_self_fence_and_stale_safety_require_separate_explicit_confirmation', async ({ page, context }) => {
  await open(page, context, `/ui/admin/principals?principal_id=${alice}`);
  let detail = page.getByRole('region', { name: '주체 상세', exact: true });
  await detail.getByLabel('관리용 계정 정지', { exact: true }).check(); await detail.getByLabel('정책 변경 사유', { exact: true }).fill('자기 정지 거부'); await detail.getByRole('button', { name: '정책 교체 확인', exact: true }).click(); await execute(page); await expect(detail).toContainText('자기·마지막 관리자 정지'); expect((await json(page, `/gateway/admin/principals/${alice}`)).body.principal.policy.disabled).toBe(false);
  await page.goto(`${origin}/ui/admin/principals?principal_id=${service}`); detail = page.getByRole('region', { name: '주체 상세', exact: true });
  await detail.getByLabel('관리용 계정 정지', { exact: true }).check(); await detail.getByLabel('정책 변경 사유', { exact: true }).fill('security_config_fence:u2-ref'); await detail.getByRole('button', { name: '정책 교체 확인', exact: true }).click(); await execute(page);
  await detail.getByLabel('관리용 계정 정지', { exact: true }).uncheck(); await detail.getByLabel('정책 변경 사유', { exact: true }).fill('일반 해제로 fence 우회 금지'); await detail.getByRole('button', { name: '정책 교체 확인', exact: true }).click(); await execute(page); await expect(detail).toContainText('보안 fence 해제 조건'); expect((await json(page, `/gateway/admin/principals/${service}`)).body.principal.policy.disabled).toBe(true);
  await detail.getByLabel('관리용 계정 정지', { exact: true }).uncheck(); await detail.getByLabel('정책 변경 사유', { exact: true }).fill('구 프로세스 종료 확인'); await detail.getByText('보안 설정 fence 해제 진술', { exact: true }).click(); await detail.getByLabel('Fence 참조', { exact: true }).fill('u2-ref'); await detail.getByLabel('구 프로세스 종료 확인 참조', { exact: true }).fill('synthetic-processes-stopped'); await detail.getByLabel('구 설정 프로세스 종료를 확인했습니다', { exact: true }).check(); await detail.getByRole('button', { name: '정책 교체 확인', exact: true }).click(); await execute(page); expect((await json(page, `/gateway/admin/principals/${service}`)).body.principal.policy.disabled).toBe(false);
  await detail.getByLabel('안전차단 해제 사유', { exact: true }).fill('새 원인 경합'); await detail.getByLabel('과금 원인과 남은 차단을 확인했습니다', { exact: true }).check(); await detail.getByRole('button', { name: '명시 해제 확인', exact: true }).click(); const cause = await page.request.post(`${process.env.E2E_CONTROL_URL}/new-safety-cause`); expect(cause.status()).toBe(204); await execute(page); await expect(detail).toContainText('버전이 변경되었습니다.');
  const paused = (await json(page, `/gateway/admin/principals/${service}`)).body.principal; expect(paused.inference_safety.paused).toBe(true); expect(paused.block_state.global_unknown_count).toBe('2');
  await detail.getByLabel('안전차단 해제 사유', { exact: true }).fill('새 원인을 재확인한 명시 해제'); await detail.getByLabel('과금 원인과 남은 차단을 확인했습니다', { exact: true }).check(); await detail.getByRole('button', { name: '명시 해제 확인', exact: true }).click(); await execute(page); const released = (await json(page, `/gateway/admin/principals/${service}`)).body.principal; expect(released.inference_safety.paused).toBe(false); expect(released.block_state.global_unknown_count).toBe('2'); expect(released.block_state.blocked).toBe(true);
});

test('admin_mutual_last_admin_policy_race_preserves_one_current_admin', async ({ page, context, browser }) => {
  await open(page, context, `/ui/admin/principals?principal_id=${bob}`);
  const other = await browser.newContext(); await auth(other, process.env.E2E_BOB_TOKEN!); await other.route('**/*', async route => { if (new URL(route.request().url()).origin !== origin) return route.abort(); return route.fulfill({ response: await forward(route, process.env.E2E_PEER_URL!) }); }); const second = await other.newPage(); await second.goto(`${origin}/ui/admin/principals?principal_id=${alice}`);
  const entered1 = barrier(), entered2 = barrier(), release = barrier(), statuses: number[] = [];
  for (const [p, target, entered, base] of [[page, bob, entered1, transport], [second, alice, entered2, process.env.E2E_PEER_URL!]] as const) {
    const detail = p.getByRole('region', { name: '주체 상세', exact: true }); await detail.getByLabel('관리용 계정 정지', { exact: true }).check(); await detail.getByLabel('정책 변경 사유', { exact: true }).fill('동시 상호 마지막 admin 정지 보호'); await detail.getByRole('button', { name: '정책 교체 확인', exact: true }).click();
    await p.route(`${origin}/gateway/admin/principals/${target}/policy`, async route => { entered.release(); await release.promise; const response = await forward(route, base); statuses.push(response.status()); test.info().annotations.push({ type: 'admin-http', description: `policy:${response.status()}` }); return route.fulfill({ response }); });
  }
  try { await Promise.all([page.getByRole('button', { name: '명령 실행', exact: true }).click(), second.getByRole('button', { name: '명령 실행', exact: true }).click()]); await Promise.all([entered1.promise, entered2.promise]); release.release(); await expect(page.getByRole('status').filter({hasText:'서버 재조회 완료'}).or(page.getByRole('alert'))).toBeVisible(); await expect(second.getByRole('status').filter({hasText:'서버 재조회 완료'}).or(second.getByRole('alert'))).toBeVisible(); expect(statuses.sort()).toEqual([200, 403]); } finally { release.release(); await other.close(); }
});
