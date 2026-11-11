import { test, expect, type BrowserContext, type Page } from '@playwright/test';
test.use({ launchOptions: { ignoreDefaultArgs: ['--disable-back-forward-cache'] } });
const transport = process.env.E2E_BASE_URL!;
// 가상 HTTPS origin은 브라우저 interception에서 전용 loopback Rust로만 전달한다.
// 제품 Origin 검사나 CSP를 변경하지 않고 canonical PUBLIC_ORIGIN을 그대로 검증한다.
const origin = 'https://m-gated.example.test';
const cookie = process.env.E2E_COOKIE!;
const labels = ['3개월', '6개월', '1년', '3년', '무제한 (만료 없음, 회수·정지는 적용)'];
const presets = ['3_months', '6_months', '1_year', '3_years', 'unlimited'];
async function authenticate(context: BrowserContext, value = process.env.E2E_TOKEN!) {
  await context.addCookies([{ name: cookie, value, url: origin, httpOnly: true, secure: true, sameSite: 'Lax' }]);
}
async function forward(route: import('@playwright/test').Route) {
  const request = route.request(), url = new URL(request.url());
  return route.fetch({ url: `${transport}${url.pathname}${url.search}`, headers: await request.allHeaders(), maxRedirects: 0, maxRetries: 0 });
}
function phase(value: string) { test.info().annotations.push({ type: 'key-phase', description: value }); }
async function open(page: Page, context: BrowserContext) {
  phase('open');
  page.on('response', response => {
    const url = new URL(response.url());
    const route = url.pathname === '/gateway/me' ? 'me' : url.pathname === '/gateway/keys' ? 'keys' : url.pathname === '/gateway/csrf' ? 'csrf' : url.pathname === '/ui/keys' ? 'shell' : 'other';
    if (route !== 'other') test.info().annotations.push({ type: 'key-http', description: `${route}:${response.status()}` });
  });
  await authenticate(context); phase('cookie'); await page.goto(`${origin}/ui/keys`); phase('shell');
  await expect(page.getByRole('heading', { name: '내 키', exact: true })).toBeVisible(); phase('heading');
  await expect(page.getByRole('button', { name: '키 조회', exact: true })).toBeEnabled(); phase('list');
  const grantState = await page.evaluate(async () => { const me = await (await fetch('/gateway/me')).json(); return { count: me.grants.length, allowed: me.grants.some((g: { provider: string; model: string; capabilities: string[] }) => g.provider === 'anthropic' && g.model === 'synthetic-text' && g.capabilities.includes('messages')) }; });
  const boxes = page.getByRole('checkbox'); const named = page.getByLabel('anthropic / synthetic-text / messages', { exact: true });
  test.info().annotations.push({ type: 'key-grants', description: `${grantState.count}:${grantState.allowed}:${await boxes.count()}:${await named.count()}` });
}
async function prepare(page: Page, name: string, preset = 'unlimited') {
  await page.getByLabel('키 이름', { exact: true }).fill(name);
  await page.getByLabel('유효기간', { exact: true }).selectOption(preset);
  await page.getByLabel('anthropic / synthetic-text / messages', { exact: true }).check();
  await page.getByRole('button', { name: '발급 내용 확인', exact: true }).click(); phase('prepared');
}
async function issue(page: Page, name: string, preset = 'unlimited') {
  await prepare(page, name, preset); await page.getByRole('button', { name: '발급 확인', exact: true }).click();
  await expect(page.getByRole('region', { name: '발급 원문', exact: true })).toBeVisible(); phase('issued');
  await expect(page.getByRole('button', { name: '키 조회', exact: true })).toBeEnabled();
  return (await page.getByRole('region', { name: '발급 원문', exact: true }).locator('code').textContent())!;
}
async function revoke(page: Page, name: string) {
  await page.getByRole('button', { name: `${name} 회수`, exact: true }).click();
  await page.getByRole('button', { name: '회수 확인', exact: true }).click();
  await expect(page.getByRole('button', { name: `${name} 회수`, exact: true })).toHaveCount(0);
  await expect(page.getByRole('button', { name: '키 조회', exact: true })).toBeEnabled();
}
function barrier() { let release!: () => void; const promise = new Promise<void>(r => { release = r; }); return { promise, release }; }
test.beforeEach(async ({ context }) => {
  await context.route('**/*', async route => {
    const url = new URL(route.request().url());
    if (url.origin === origin) { const response = await forward(route); return route.fulfill({ response }); }
    if (url.origin === 'https://auth.example.test' && url.pathname === '/sso/logout') return route.fulfill({ status: 200, contentType: 'text/html; charset=utf-8', body: '<title>합성 로그아웃</title>' });
    await route.abort('blockedbyclient');
  });
});
test('personal_keys_five_explicit_durations_one_time_secret_and_bodyless_revoke', async ({ page, context }) => {
  const consoleText: string[] = []; page.on('console', message => consoleText.push(message.text()));
  let writes = 0, deletes = 0;
  page.on('request', request => { if (request.url() === `${origin}/gateway/keys` && request.method() === 'POST') writes++; if (request.method() === 'DELETE') { deletes++; expect(request.postData()).toBeNull(); expect(request.headers()['x-csrf-token']).toBeTruthy(); } });
  await open(page, context);
  expect(await page.getByLabel('유효기간', { exact: true }).inputValue()).toBe('');
  expect(await page.getByLabel('유효기간', { exact: true }).locator('option').allTextContents()).toEqual(['선택하세요', ...labels]);
  await page.getByLabel('키 이름', { exact: true }).fill('duration-required');
  await page.getByRole('button', { name: '발급 내용 확인', exact: true }).focus(); await page.keyboard.press('Enter');
  expect(writes).toBe(0);
  for (const preset of presets) {
    const name = `duration-${preset}`;
    const secret = await issue(page, name, preset);
    expect(secret.startsWith('mgw_v1_')).toBe(true);
    const metadata = await page.evaluate(async name => { const page = await (await fetch('/gateway/keys')).json(); return page.items.find((key: { name: string }) => key.name === name); }, name);
    expect(metadata.validity).toBe(preset);
    if (preset === 'unlimited') expect(metadata.expires_at).toBeNull(); else {
      const created = new Date(metadata.created_at), expires = new Date(metadata.expires_at), months = [3, 6, 12, 36][presets.indexOf(preset)];
      const expected = new Date(created); const day = expected.getUTCDate(); expected.setUTCDate(1); expected.setUTCMonth(expected.getUTCMonth() + months); const last = new Date(Date.UTC(expected.getUTCFullYear(), expected.getUTCMonth() + 1, 0)).getUTCDate(); expected.setUTCDate(Math.min(day, last));
      expect(expires.getTime()).toBe(expected.getTime());
    }
    expect(JSON.stringify(metadata).includes(secret)).toBe(false);
    expect(consoleText.some(text => text.includes(secret))).toBe(false);
    expect(await page.evaluate(() => [localStorage.length, sessionStorage.length])).toEqual([0, 0]);
    await page.getByRole('button', { name: '원문 닫기', exact: true }).focus(); await page.keyboard.press('Enter');
    expect(await page.locator('main').textContent()).not.toContain(secret);
  }
  expect(writes).toBe(5);
  await revoke(page, 'duration-unlimited'); expect(deletes).toBe(1);
  await page.reload(); await expect(page.getByRole('heading', { name: '내 키', exact: true })).toBeVisible();
  expect(await page.getByRole('region', { name: '발급 원문', exact: true }).count()).toBe(0);
});

test('lost_issue_response_links_command_without_replay_then_user_revokes_and_reissues', async ({ page, context }) => {
  await open(page, context); let writes = 0;
  await page.route(`${origin}/gateway/keys`, async route => {
    if (route.request().method() !== 'POST') return route.fallback();
    writes++; const response = await forward(route); expect(response.status()).toBe(201);
    await response.dispose(); await route.abort('failed');
  });
  await prepare(page, 'lost-response'); await page.getByRole('button', { name: '발급 확인', exact: true }).click();
  await expect(page.getByRole('region', { name: '명령 결과 확인', exact: true })).toContainText('연결된 키:');
  expect(writes).toBe(1); expect(await page.getByRole('region', { name: '발급 원문', exact: true }).count()).toBe(0);
  await revoke(page, 'lost-response'); expect(writes).toBe(1);
  await page.unroute(`${origin}/gateway/keys`);
  await issue(page, 'explicit-replacement');
  await page.getByRole('link', { name: '내 사용량', exact: true }).click();
  await expect(page.getByRole('heading', { name: '조회 기간 사용량', exact: true })).toBeVisible();
  expect(await page.getByRole('region', { name: '발급 원문', exact: true }).count()).toBe(0);
});

test('key_read_refresh_once_then_cookie_write401_never_refreshes_or_replays', async ({ page, context }) => {
  await open(page, context); let refresh = 0, writes = 0;
  await page.route('https://auth.example.test/sso/refresh', async route => {
    refresh++; await authenticate(context, process.env.E2E_REFRESHED_TOKEN!);
    await route.fulfill({ status: 200, headers: { 'Access-Control-Allow-Origin': origin, 'Access-Control-Allow-Credentials': 'true' }, contentType: 'application/json', body: '{}' });
  });
  await authenticate(context, process.env.E2E_EXPIRED_TOKEN!);
  const reads: number[] = [];
  const filtered = `${origin}/gateway/keys?status=revoked`;
  page.on('response', response => { if (response.url() === filtered) reads.push(response.status()); });
  const recovered = page.waitForResponse(response => response.url() === filtered && response.status() === 200);
  await page.getByLabel('키 상태', { exact: true }).selectOption('revoked');
  await page.getByRole('button', { name: '키 조회', exact: true }).click();
  await recovered;
  await expect(page.getByRole('button', { name: '키 조회', exact: true })).toBeEnabled();
  await expect(page.getByLabel('키 상태', { exact: true })).toHaveValue('revoked');
  expect(reads).toEqual([401, 200]);
  await expect(page.getByRole('heading', { name: '내 키', exact: true })).toBeVisible(); expect(refresh).toBe(1);
  await page.route(`${origin}/gateway/keys`, async route => {
    if (route.request().method() !== 'POST') return route.fallback();
    writes++;
    const headers = await route.request().allHeaders(); headers.cookie = `${cookie}=${process.env.E2E_EXPIRED_TOKEN!}`;
    const response = await route.fetch({ url: `${transport}/gateway/keys`, headers, maxRedirects: 0, maxRetries: 0 });
    expect(response.status()).toBe(401); await route.fulfill({ response });
  });
  await prepare(page, 'write-expired'); await page.getByRole('button', { name: '발급 확인', exact: true }).click();
  await expect(page.getByRole('alert')).toContainText('쓰기는 재전송하지 않았습니다.');
  expect(writes).toBe(1); expect(refresh).toBe(1);
  expect(await page.getByLabel('키 이름', { exact: true }).count()).toBe(0);
  expect(await page.getByRole('region', { name: '발급 원문', exact: true }).count()).toBe(0);
});

test('key_secret_and_forms_disappear_on_bfcache_broadcast_identity_and_cross_tab_logout', async ({ page, context }) => {
  await open(page, context);
  for (const mode of ['bfcache', 'broadcast', 'identity'] as const) {
    const secret = await issue(page, `clear-${mode}`);
    await page.getByLabel('키 이름', { exact: true }).fill('private-form');
    const entered = barrier(), release = barrier();
    await page.route(`${origin}/gateway/me`, async route => { entered.release(); await release.promise; await route.fallback(); });
    try {
      if (mode === 'identity') await authenticate(context, process.env.E2E_SECOND_TOKEN!);
      await page.evaluate(mode => {
        if (mode === 'broadcast') { const channel = new BroadcastChannel('m-gated-auth'); channel.postMessage({ type: 'auth-change' }); channel.close(); }
        else window.dispatchEvent(new PageTransitionEvent('pageshow', { persisted: true }));
      }, mode);
      await entered.promise;
      expect(await page.locator('main').textContent()).not.toContain(secret);
      expect(await page.getByLabel('키 이름', { exact: true }).count()).toBe(0);
    } finally { release.release(); }
    await expect(page.getByRole('heading', { name: '내 키', exact: true })).toBeVisible();
    await expect(page.getByLabel('키 이름', { exact: true })).toHaveValue('');
    expect(await page.getByRole('region', { name: '발급 원문', exact: true }).count()).toBe(0);
    await page.unroute(`${origin}/gateway/me`);
    if (mode === 'identity') {
      await authenticate(context); await page.evaluate(() => window.dispatchEvent(new PageTransitionEvent('pageshow', { persisted: true })));
      await expect(page.getByLabel('anthropic / synthetic-text / messages', { exact: true })).toBeVisible();
    }
  }
  const secret = await issue(page, 'clear-logout');
  const other = await context.newPage(); await other.goto(`${origin}/ui/keys`);
  await expect(other.getByRole('heading', { name: '내 키', exact: true })).toBeVisible();
  await other.getByRole('button', { name: '로그아웃', exact: true }).click({ noWaitAfter: true });
  await expect(page.getByRole('alert')).toHaveText('로그아웃되었습니다.');
  expect(await page.locator('main').textContent()).not.toContain(secret);
  expect(await page.getByLabel('키 이름', { exact: true }).count()).toBe(0);
  await other.close();
});

test('delayed_committed_issue_response_never_restores_secret_after_epoch_change', async ({ page, context }) => {
  await open(page, context); const entered = barrier(), release = barrier(); let writes = 0;
  await page.route(`${origin}/gateway/keys`, async route => {
    if (route.request().method() !== 'POST') return route.fallback();
    writes++; const response = await forward(route); expect(response.status()).toBe(201); entered.release(); await release.promise;
    try { await route.fulfill({ response }); } catch { /* epoch 변경으로 취소된 응답은 폐기한다. */ }
  });
  try {
    await prepare(page, 'late-committed'); await page.getByRole('button', { name: '발급 확인', exact: true }).click(); await entered.promise;
    await page.evaluate(() => window.dispatchEvent(new PageTransitionEvent('pageshow', { persisted: true })));
    await expect(page.getByRole('heading', { name: '내 키', exact: true })).toBeVisible();
    release.release();
    await expect(page.getByRole('heading', { name: 'late-committed', exact: true })).toBeVisible();
    expect(await page.getByRole('region', { name: '발급 원문', exact: true }).count()).toBe(0);
    expect(writes).toBe(1); expect(await page.evaluate(() => [localStorage.length, sessionStorage.length])).toEqual([0, 0]);
  } finally { release.release(); }
});

test.describe('actual_navigation_lifecycle', () => {
  test('real_navigation_hides_key_before_freeze_and_back_never_restores_secret', async ({ page, context }) => {
    const events: { event: string; persisted: boolean; secret: boolean; form: boolean }[] = [];
    await page.exposeBinding('__keyLifecycle', (_, state) => { events.push(state); });
    await page.addInitScript(() => {
      window.addEventListener('pageshow', event => {
        Reflect.get(window, '__keyLifecycle')({ event: 'show', persisted: event.persisted, secret: !!document.querySelector('[aria-label="발급 원문"]'), form: !!document.querySelector('input[name="name"]') });
      });
    });
    await open(page, context); const secret = await issue(page, 'real-navigation');
    await page.getByLabel('키 이름', { exact: true }).fill('private-before-freeze');
    // App의 pagehide listener 뒤에 설치하여 동기 폐기 완료 시점만 관측한다.
    await page.evaluate(() => {
      window.addEventListener('pagehide', event => {
        Reflect.get(window, '__keyLifecycle')({ event: 'hide', persisted: event.persisted, secret: !!document.querySelector('[aria-label="발급 원문"]'), form: !!document.querySelector('input[name="name"]') });
      });
    });
    await page.getByRole('link', { name: '내 사용량', exact: true }).click();
    await expect(page.getByRole('heading', { name: '조회 기간 사용량', exact: true })).toBeVisible();
    await page.goBack();
    await expect(page.getByRole('heading', { name: '내 키', exact: true })).toBeVisible();
    await expect(page.getByLabel('키 이름', { exact: true })).toHaveValue('');
    expect(await page.locator('main').textContent()).not.toContain(secret);
    expect(events.filter(event => event.event === 'hide').length).toBeGreaterThan(0);
    expect(events.every(event => !event.secret)).toBe(true);
    expect(events.filter(event => event.event === 'hide').every(event => !event.form)).toBe(true);
    for (const event of events) test.info().annotations.push({ type: 'key-navigation', description: `${event.event}:${event.persisted}:${event.secret}:${event.form}` });
  });
});

test('key_revoke409_reloads_server_metadata_before_new_user_confirmation', async ({ page, context }) => {
  await open(page, context); await issue(page, 'cas-revoke');
  await page.getByRole('button', { name: '원문 닫기', exact: true }).click();
  let writes = 0;
  await page.route(`${origin}/gateway/keys/*`, async route => {
    if (route.request().method() !== 'DELETE') return route.fallback();
    writes++;
    if (writes !== 1) return route.fallback();
    const path = new URL(route.request().url()); path.searchParams.set('expected_version', '99999');
    const response = await route.fetch({ url: `${transport}${path.pathname}${path.search}`, headers: await route.request().allHeaders(), maxRedirects: 0, maxRetries: 0 });
    expect(response.status()).toBe(409); await route.fulfill({ response });
  });
  await page.getByRole('button', { name: 'cas-revoke 회수', exact: true }).click();
  await page.getByRole('button', { name: '회수 확인', exact: true }).click();
  await expect(page.getByRole('alert')).toContainText('버전이 변경되었습니다.');
  await expect(page.getByRole('button', { name: 'cas-revoke 회수', exact: true })).toBeEnabled();
  expect(writes).toBe(1);
  await revoke(page, 'cas-revoke'); expect(writes).toBe(2);
});

test('lost_delete_response_get_confirms_revoke_before_explicit_replacement', async ({ page, context }) => {
  await open(page, context);
  const postCommands: string[] = [], deleteCommands: string[] = [];
  page.on('request', request => {
    if (request.url() === `${origin}/gateway/keys` && request.method() === 'POST') postCommands.push(request.headers()['x-gateway-command-id']);
    if (request.method() === 'DELETE') deleteCommands.push(request.headers()['x-gateway-command-id']);
  });
  await issue(page, 'lost-delete-response');
  await page.getByRole('button', { name: '원문 닫기', exact: true }).click();
  let committed = 0, confirmedGet = 0, targetId = '';
  await page.route(url => url.origin === origin && (url.pathname === '/gateway/keys' || url.pathname.startsWith('/gateway/keys/')), async route => {
    const request = route.request(), path = new URL(request.url());
    if (request.method() === 'DELETE') {
      targetId = path.pathname.split('/').at(-1)!;
      const response = await forward(route); expect(response.status()).toBe(204); committed++;
      await response.dispose(); return route.abort('failed');
    }
    if (committed && request.method() === 'GET' && path.pathname === '/gateway/keys') {
      const response = await forward(route);
      expect(response.status()).toBe(200);
      const body = await response.json();
      if (body.items.some((key: { id: string; revoked_at: string | null }) => key.id === targetId && key.revoked_at !== null)) confirmedGet++;
      return route.fulfill({ response });
    }
    return route.fallback();
  });
  try {
    await page.getByRole('button', { name: 'lost-delete-response 회수', exact: true }).click();
    await page.getByRole('button', { name: '회수 확인', exact: true }).click();
    await expect(page.getByRole('button', { name: '발급 내용 확인', exact: true })).toBeEnabled();
    expect(committed).toBe(1); expect(confirmedGet).toBe(1);
    expect(deleteCommands).toHaveLength(1); expect(postCommands).toHaveLength(1);
    expect(await page.getByRole('region', { name: '명령 결과 확인', exact: true }).count()).toBe(0);
    expect(await page.getByRole('region', { name: '발급 원문', exact: true }).count()).toBe(0);
    await prepare(page, 'explicit-after-lost-delete');
    expect(postCommands).toHaveLength(1);
    await page.getByRole('button', { name: '발급 확인', exact: true }).click();
    await expect(page.getByRole('region', { name: '발급 원문', exact: true })).toBeVisible();
    expect(postCommands).toHaveLength(2); expect(new Set(postCommands).size).toBe(2);
    expect(deleteCommands).toHaveLength(1);
  } finally {
    const disabled = await page.getByRole('button', { name: '발급 내용 확인', exact: true }).isDisabled();
    const recovery = await page.getByRole('region', { name: '명령 결과 확인', exact: true }).count() > 0;
    const loading = await page.getByText('키 목록 조회 중…', { exact: true }).count() > 0;
    test.info().annotations.push({ type: 'key-recovery', description: `${committed}:${confirmedGet}:${postCommands.length}:${deleteCommands.length}:${disabled}:${recovery}:${loading}` });
  }
});

test('s9_real_origin_denial_never_refreshes_or_replays_cookie_write', async ({ page, context }) => {
  await open(page, context);
  let refresh = 0, writes = 0;
  await page.route('https://auth.example.test/sso/refresh', route => { refresh++; return route.abort('blockedbyclient'); });
  const before = await page.evaluate(async () => (await (await fetch('/gateway/keys')).json()).items.length as number);
  await page.route(`${origin}/gateway/keys`, async route => {
    if (route.request().method() !== 'POST') return route.fallback();
    writes++;
    const headers = await route.request().allHeaders(); headers.origin = 'https://untrusted.invalid';
    const response = await route.fetch({ url: `${transport}/gateway/keys`, headers, maxRedirects: 0, maxRetries: 0 });
    expect(response.status()).toBe(403); expect(response.headers()['x-gateway-error-code']).toBe('origin_denied');
    await route.fulfill({ response });
  });
  await prepare(page, 's9-origin-denied');
  await page.getByRole('button', { name: '발급 확인', exact: true }).focus(); await page.keyboard.press('Enter');
  await expect(page.getByRole('alert')).toBeVisible();
  await expect(page.getByRole('button', { name: '키 조회', exact: true })).toBeEnabled();
  await page.getByRole('button', { name: '키 조회', exact: true }).click();
  await expect(page.getByRole('button', { name: '키 조회', exact: true })).toBeEnabled();
  const after = await page.evaluate(async () => (await (await fetch('/gateway/keys')).json()).items.length as number);
  expect(after).toBe(before); expect(writes).toBe(1); expect(refresh).toBe(0);
  expect(await page.getByRole('region', { name: '발급 원문', exact: true }).count()).toBe(0);
  expect(await page.evaluate(() => [localStorage.length, sessionStorage.length])).toEqual([0, 0]);
});
