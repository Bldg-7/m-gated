import { test, expect, type BrowserContext } from '@playwright/test';
const base = process.env.E2E_BASE_URL!; const token = process.env.E2E_TOKEN!; const cookie = process.env.E2E_COOKIE!;
async function authenticate(context: BrowserContext) { await context.addCookies([{ name: cookie, value: token, url: base, httpOnly: true, sameSite: 'Lax' }]); }
test.beforeEach(async ({ context }) => {
  await context.route('**/*', async route => {
    const url = new URL(route.request().url());
    if (url.origin === base) return route.continue();
    if (url.origin === 'https://auth.example.test' && url.pathname === '/sso/logout') return route.fulfill({ status: 200, contentType: 'text/html; charset=utf-8', body: '<!doctype html><title>합성 SSO 로그아웃</title>' });
    await route.abort('blockedbyclient');
  });
});
test('Rust bundle serves precise usage, global block, keyboard and text-only identity', async ({ page, context }) => {
  await authenticate(context); const errors: string[] = []; page.on('pageerror', e => errors.push(e.name));
  const response = await page.goto('/ui/'); expect(response?.status()).toBe(200); expect(response?.headers()['content-security-policy']).toContain("default-src 'none'");
  await expect(page.getByText('0.000000000001 USD', { exact: true }).first()).toBeVisible(); await expect(page.getByText('확인 불가 (미정산)')).toBeVisible(); await expect(page.getByText(/전체 기간 미정산: 1/)).toBeVisible();
  await expect(page.getByText(/<img src=x onerror=window.__xss=1>/)).toBeVisible(); expect(await page.evaluate(() => Reflect.get(window, '__xss'))).toBeUndefined(); expect(await page.locator('main img').count()).toBe(0);
  await page.getByLabel('모델', { exact: true }).fill('historical-missing-model'); await page.getByRole('button', { name: '조회', exact: true }).focus(); await page.keyboard.press('Enter'); await expect(page.getByText(/그룹별 사용량이 없습니다/)).toBeVisible(); await expect(page.getByText(/전체 기간 미정산: 1/)).toBeVisible();
  expect(await page.evaluate(() => [localStorage.length, sessionStorage.length])).toEqual([0, 0]); expect(errors).toEqual([]);
  for (const path of ['/ui/keys', '/ui/admin/principals', '/ui/admin/usage', '/ui/admin/requests']) { const r = await page.request.get(path); expect(r.status()).toBe(200); expect(r.headers()['content-type']).toContain('text/html'); }
  for (const path of ['/ui/assets/missing.js', '/ui/assets/%2e%2e%2findex.html', '/gateway/missing']) { const r = await page.request.get(path); expect(r.status()).toBe(404); }
  for (const path of ['/anthropic/v1/messages', '/anthropic/v1/messages/count_tokens', '/v1/messages', '/v1/messages/count_tokens']) { const inference = await page.request.post(path, { data: { model: 'synthetic-text', messages: [{ role: 'user', content: 'synthetic' }], max_tokens: 1 } }); expect(inference.status()).toBe(401); }
});
test('401 refresh is bounded and stale usage never re-enters after logout', async ({ page, context }) => {
  await authenticate(context);
  await page.goto('/ui/');
  await expect(page.getByRole('heading', { name: '조회 기간 사용량' })).toBeVisible();
  let refresh = 0, usageReads = 0;
  const usageStatuses: number[] = [];
  const matchesUsage = (url: string) => url.startsWith(`${base}/gateway/usage?`) && new URL(url).searchParams.get('model') === 'missing-after-refresh';
  page.on('request', request => { if (matchesUsage(request.url())) usageReads++; });
  page.on('response', response => { if (matchesUsage(response.url())) usageStatuses.push(response.status()); });
  await page.route('https://auth.example.test/sso/refresh', async route => {
    if (route.request().method() === 'POST') refresh++;
    const control=process.env.E2E_CONTROL_URL!;
    expect(new URL(control).hostname).toBe('127.0.0.1');
    const response=await page.request.post(`${control}/refresh`);
    expect(response.status()).toBe(204);
    await route.fulfill({ status: 200, headers: { 'Access-Control-Allow-Origin': base, 'Access-Control-Allow-Credentials': 'true' }, contentType: 'application/json', body: '{}' });
  });
  await context.addCookies([{name:cookie,value:process.env.E2E_EXPIRED_TOKEN!,url:base,httpOnly:true,sameSite:'Lax'}]);
  const recovered = page.waitForResponse(r => r.url().startsWith(`${base}/gateway/usage?`) && new URL(r.url()).searchParams.get('model') === 'missing-after-refresh' && r.status() === 200);
  await page.getByLabel('모델', { exact: true }).fill('missing-after-refresh');
  await page.getByRole('button', { name: '조회', exact: true }).click();
  await recovered;
  await expect(page.getByRole('heading', { name: '조회 기간 사용량' })).toBeVisible();
  expect(refresh).toBe(1);
  await expect(page.getByLabel('모델', { exact: true })).toHaveValue('missing-after-refresh');
  expect(usageReads).toBe(2);
  expect(usageStatuses).toEqual([401, 200]);
  // 자동 재시도 이후 같은 필터의 명시적인 조회는 별도의 새 읽기다.
  const explicitRead = page.waitForResponse(r => matchesUsage(r.url()) && r.status() === 200);
  await page.getByRole('button', { name: '조회', exact: true }).click();
  await explicitRead;
  await expect(page.getByRole('heading', { name: '조회 기간 사용량' })).toBeVisible();
  expect(usageReads).toBe(3);
  expect(usageStatuses).toEqual([401, 200, 200]);
  expect(refresh).toBe(1);
  let arrived!: () => void, released!: () => void;
  const held = new Promise<void>(r => { arrived = r; }); const release = new Promise<void>(r => { released = r; });
  await page.unroute(`${base}/gateway/usage?**`);
  await page.route(`${base}/gateway/usage?**`, async route => { arrived(); await release; try { await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ settled_usd: '999.999999999999' }) }); } catch {/* 이탈로 취소한 요청 */ } });
  await page.getByLabel('모델', { exact: true }).fill('held-before-logout');
  await page.getByRole('button', { name: '조회', exact: true }).click();
  await held;
  const logoutRequest = page.waitForRequest(r => r.url() === 'https://auth.example.test/sso/logout');
  try {
    await page.getByRole('button', { name: '로그아웃' }).click({ noWaitAfter: true });
    await logoutRequest;
    released();
    await expect(page).toHaveTitle('합성 SSO 로그아웃');
  } finally { released(); }
  expect(await page.locator('main').count()).toBe(0);
  expect(await page.getByText('999.999999999999 USD', { exact: true }).count()).toBe(0);
});
test('retention 409 removes previous figures without silently dropping filters', async ({ page, context }) => {
  await authenticate(context); await page.goto('/ui/'); await expect(page.getByRole('heading', { name: '조회 기간 사용량' })).toBeVisible();
  await page.route(`${base}/gateway/usage?**`, route => route.fulfill({ status: 409, contentType: 'application/json', body: JSON.stringify({ error: { code: 'usage_detail_unavailable', details: { detail_available_from: '2026-01-01T00:00:00Z' } } }) }));
  await page.getByLabel('모델', { exact: true }).fill('old-model'); await page.getByRole('button', { name: '조회', exact: true }).click(); await expect(page.getByRole('alert')).toContainText('상세 가용 시작: 2026-01-01'); expect(await page.getByRole('heading', { name: '조회 기간 사용량' }).count()).toBe(0); expect(await page.getByText('0.000000000001 USD', { exact: true }).count()).toBe(0); await expect(page.getByLabel('모델', { exact: true })).toHaveValue('old-model');
});
test('cross_tab_logout_latch_blocks_visibility_pageshow_and_old_cookie_until_explicit_login', async ({ page, context }) => {
  await authenticate(context); await page.goto('/ui/');
  await expect(page.getByRole('heading', { name: '조회 기간 사용량' })).toBeVisible();
  const other = await context.newPage(); await other.goto(`${base}/ui/`);
  await expect(other.getByRole('heading', { name: '조회 기간 사용량' })).toBeVisible();
  let arrived!: () => void, released!: () => void;
  const held = new Promise<void>(r => { arrived = r; }), release = new Promise<void>(r => { released = r; });
  await page.route('https://auth.example.test/sso/logout', async route => {
    arrived(); await release;
    await route.fulfill({ status: 200, contentType: 'text/html; charset=utf-8', body: '<title>합성 SSO 로그아웃</title>' });
  });
  try {
    await page.getByRole('button', { name: '로그아웃' }).click({ noWaitAfter: true }); await held;
    await expect(other.getByRole('alert')).toHaveText('로그아웃되었습니다.');
    await other.evaluate(() => {
      Reflect.set(window, '__authReads', 0);
      const original = window.fetch;
      window.fetch = (...args) => {
        if (String(args[0]).startsWith('/gateway/')) Reflect.set(window, '__authReads', Reflect.get(window, '__authReads') + 1);
        return original(...args);
      };
    });
    await other.bringToFront();
    await other.evaluate(() => {
      document.dispatchEvent(new Event('visibilitychange'));
      window.dispatchEvent(new PageTransitionEvent('pageshow', { persisted: true }));
    });
    await expect(other.getByRole('alert')).toHaveText('로그아웃되었습니다.');
    expect(await other.evaluate(() => Reflect.get(window, '__authReads'))).toBe(0);
    expect(await other.getByRole('heading', { name: '조회 기간 사용량' }).count()).toBe(0);
    expect(await other.getByText(/<img src=x/).count()).toBe(0);
    expect(await other.getByText('0.000000000001 USD', { exact: true }).count()).toBe(0);
    // 구 cookie가 여전히 정상 서명이므로 명시적인 사용자 재확인만 다시 로그인한다.
    await other.getByRole('button', { name: '다시 확인' }).click();
    await expect(other.getByRole('heading', { name: '조회 기간 사용량' })).toBeVisible();
    expect(await other.evaluate(() => Reflect.get(window, '__authReads'))).toBe(2);
  } finally { released(); }
  await expect(page).toHaveTitle('합성 SSO 로그아웃'); await other.close();
});

test('refresh_success_then_current_me403_stays_permission_error_without_login', async ({ page, context }) => {
  await authenticate(context); await page.goto('/ui/');
  await expect(page.getByRole('heading', { name: '조회 기간 사용량' })).toBeVisible();
  const control = process.env.E2E_CONTROL_URL!; expect(new URL(control).hostname).toBe('127.0.0.1');
  let refresh = 0, meReads = 0, login = 0, usageReads = 0;
  page.on('request', request => {
    if (request.url() === `${base}/auth/login`) login++;
    if (request.url() === `${base}/gateway/me`) meReads++;
    if (request.url().startsWith(`${base}/gateway/usage?`)) usageReads++;
  });
  await page.route('https://auth.example.test/sso/refresh', async route => {
    if (route.request().method() === 'POST') refresh++;
    expect((await page.request.post(`${control}/refresh`)).status()).toBe(204);
    expect((await page.request.post(`${control}/disable`)).status()).toBe(204);
    await route.fulfill({ status: 200, headers: { 'Access-Control-Allow-Origin': base, 'Access-Control-Allow-Credentials': 'true' }, contentType: 'application/json', body: '{}' });
  });
  try {
    await context.addCookies([{ name: cookie, value: process.env.E2E_EXPIRED_TOKEN!, url: base, httpOnly: true, sameSite: 'Lax' }]);
    await page.getByLabel('모델', { exact: true }).fill('refresh-denied'); await page.getByRole('button', { name: '조회', exact: true }).click();
    await expect(page.getByRole('alert')).toHaveText('접근 권한이 없습니다.');
    expect(page.url()).toBe(`${base}/ui/`); expect(login).toBe(0); expect(refresh).toBe(1); expect(meReads).toBe(1); expect(usageReads).toBe(1);
    expect(await page.getByRole('heading', { name: '조회 기간 사용량' }).count()).toBe(0);
    expect(await page.getByText(/<img src=x/).count()).toBe(0);
    expect(await page.getByText('0.000000000001 USD', { exact: true }).count()).toBe(0);
  } finally { expect((await page.request.post(`${control}/enable`)).status()).toBe(204); }
});

test('verified_signed_identity_change_broadcasts_and_discards_other_window_old_payload', async ({ page, context }) => {
  await authenticate(context); await page.goto('/ui/');
  await expect(page.getByRole('heading', { name: '조회 기간 사용량' })).toBeVisible();
  const other = await context.newPage(); await other.goto(`${base}/ui/`);
  await expect(other.getByRole('heading', { name: '조회 기간 사용량' })).toBeVisible();
  await other.evaluate(() => {
    Reflect.set(window, '__authMessages', []);
    const observer = new BroadcastChannel('m-gated-auth');
    observer.onmessage = event => Reflect.get(window, '__authMessages').push(event.data);
    Reflect.set(window, '__authObserver', observer);
  });
  let oldArrived!: () => void, releaseOld!: () => void, meArrived!: () => void, releaseMe!: () => void;
  const oldHeld = new Promise<void>(r => { oldArrived = r; }), oldGate = new Promise<void>(r => { releaseOld = r; });
  const meHeld = new Promise<void>(r => { meArrived = r; }), meGate = new Promise<void>(r => { releaseMe = r; });
  await other.route(`${base}/gateway/usage?**`, async route => {
    if (new URL(route.request().url()).searchParams.get('model') !== 'old-private-filter') return route.continue();
    const oldResponse = await route.fetch(); oldArrived(); await oldGate;
    try { await route.fulfill({ response: oldResponse }); } catch { /* epoch 변경으로 취소한 이전 응답 */ }
  });
  await other.route(`${base}/gateway/me`, async route => { meArrived(); await meGate; await route.continue(); });
  try {
    await other.getByLabel('모델', { exact: true }).fill('old-private-filter'); await other.getByRole('button', { name: '조회', exact: true }).click(); await oldHeld;
    await context.addCookies([{ name: cookie, value: process.env.E2E_SECOND_TOKEN!, url: base, httpOnly: true, sameSite: 'Lax' }]);
    await page.evaluate(() => window.dispatchEvent(new PageTransitionEvent('pageshow', { persisted: true })));
    await expect(page.getByText(/Synthetic Second/)).toBeVisible(); await meHeld;
    expect(await other.getByRole('heading', { name: '조회 기간 사용량' }).count()).toBe(0);
    expect(await other.getByText(/<img src=x/).count()).toBe(0);
    releaseOld(); releaseMe();
    await expect(other.getByText(/Synthetic Second/)).toBeVisible();
    await expect(other.getByRole('heading', { name: '조회 기간 사용량' })).toBeVisible();
    await expect(other.getByLabel('모델', { exact: true })).toHaveValue('');
    expect(await other.getByText('0.000000000001 USD', { exact: true }).count()).toBe(0);
    expect(await other.evaluate(() => Reflect.get(window, '__authMessages'))).toEqual([{ type: 'auth-change' }]);
    await page.evaluate(() => window.dispatchEvent(new PageTransitionEvent('pageshow', { persisted: true })));
    await expect(page.getByRole('heading', { name: '조회 기간 사용량' })).toBeVisible();
    expect(await other.evaluate(() => Reflect.get(window, '__authMessages'))).toEqual([{ type: 'auth-change' }]);
  } finally { releaseOld(); releaseMe(); await other.close(); }
});
test('broadcast and bfcache revalidation conceal cached data; server stop returns403', async ({ page, context }) => {
  await authenticate(context); await page.goto('/ui/'); await expect(page.getByRole('heading', { name: '조회 기간 사용량' })).toBeVisible();
  let arrived!: () => void, released!: () => void;
  let held = new Promise<void>(r => { arrived = r; }); let release = new Promise<void>(r => { released = r; });
  await page.route(`${base}/gateway/me`, async route => { arrived(); await release; await route.continue(); });
  await page.evaluate(() => { const c = new BroadcastChannel('m-gated-auth'); c.postMessage({ type: 'auth-change' }); c.close(); });
  await held;
  expect(await page.getByRole('heading', { name: '조회 기간 사용량' }).count()).toBe(0);
  released();
  await expect(page.getByRole('heading', { name: '조회 기간 사용량' })).toBeVisible();
  held = new Promise<void>(r => { arrived = r; }); release = new Promise<void>(r => { released = r; });
  await page.evaluate(() => window.dispatchEvent(new PageTransitionEvent('pageshow', { persisted: true })));
  await held;
  expect(await page.getByRole('heading', { name: '조회 기간 사용량' }).count()).toBe(0);
  released();
  await expect(page.getByRole('heading', { name: '조회 기간 사용량' })).toBeVisible();
  await page.unroute(`${base}/gateway/me`);
  const control = process.env.E2E_CONTROL_URL!; expect(new URL(control).hostname).toBe('127.0.0.1'); await page.request.post(`${control}/disable`);
  try {
    await page.getByLabel('모델', { exact: true }).fill('server-stopped'); await page.getByRole('button', { name: '조회', exact: true }).click(); await expect(page.getByRole('alert')).toContainText('접근 권한이 없습니다.'); expect(await page.getByRole('heading', { name: '조회 기간 사용량' }).count()).toBe(0);
  } finally { expect((await page.request.post(`${control}/enable`)).status()).toBe(204); }
});

test('s9_csp_blocks_inline_external_scripts_and_forms_have_keyboard_names', async ({ page, context }) => {
  await authenticate(context);
  let foreign = 0;
  await page.route('https://forbidden.invalid/**', route => { foreign++; return route.abort('blockedbyclient'); });
  const response = await page.goto('/ui/');
  const csp = response!.headers()['content-security-policy'];
  for (const directive of ["default-src 'none'", "script-src 'self'", "style-src 'self'", "object-src 'none'", "frame-ancestors 'none'", "base-uri 'none'", "form-action 'none'"]) expect(csp).toContain(directive);
  expect(csp).not.toContain('unsafe-inline'); expect(csp).not.toContain('unsafe-eval');
  expect(response!.headers()['x-content-type-options']).toBe('nosniff');
  expect(response!.headers()['referrer-policy']).toBe('no-referrer');
  await expect(page.getByRole('heading', { name: '조회 기간 사용량' })).toBeVisible();
  const violations = await page.evaluate(() => new Promise<string[]>(resolve => {
    const seen = new Set<string>();
    document.addEventListener('securitypolicyviolation', event => {
      if (event.blockedURI === 'inline') seen.add('inline');
      if (event.blockedURI.startsWith('https://forbidden.invalid')) seen.add('external');
      if (seen.size === 2) resolve([...seen].sort());
    });
    const inline = document.createElement('script'); inline.textContent = 'window.__s9InlineExecuted = true'; document.head.append(inline);
    const external = document.createElement('script'); external.src = 'https://forbidden.invalid/s9.js'; document.head.append(external);
  }));
  expect(violations).toEqual(['external', 'inline']); expect(foreign).toBe(0);
  expect(await page.evaluate(() => Reflect.get(window, '__s9InlineExecuted'))).toBeUndefined();
  expect(await page.evaluate(() => document.documentElement.lang)).toBe('ko');
  expect(await page.locator('main').count()).toBe(1);
  const unnamed = await page.locator('main input, main select, main textarea').evaluateAll(elements => elements.filter(element => {
    const input = element as HTMLInputElement;
    return !(input.labels?.length || input.getAttribute('aria-label') || input.getAttribute('aria-labelledby'));
  }).length);
  expect(unnamed).toBe(0);
  await page.getByLabel('모델', { exact: true }).focus(); await page.keyboard.type('s9-keyboard-empty');
  await page.getByRole('button', { name: '조회', exact: true }).focus(); await page.keyboard.press('Enter');
  await expect(page.getByText(/그룹별 사용량이 없습니다/)).toBeVisible();
  expect(await page.evaluate(() => [localStorage.length, sessionStorage.length])).toEqual([0, 0]);
});
