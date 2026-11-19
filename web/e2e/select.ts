import { expect, type Locator } from '@playwright/test';

/**
 * 대시보드의 목록 선택(Select·SearchSelect)을 다루는 도구. 기본 select가 아니라 selectOption을 쓸 수 없다.
 * 입력을 눌러 목록을 열고, 사용자가 보는 이름으로 항목을 누른다. 고른 값은 data-value에 있다.
 */
async function listbox(combobox: Locator) {
  if ((await combobox.getAttribute('aria-expanded')) !== 'true') await combobox.click();
  await expect(combobox).toHaveAttribute('aria-expanded', 'true');
  const id = await combobox.getAttribute('aria-controls');
  return combobox.page().locator(`[id="${id}"]`);
}
export async function pickOption(combobox: Locator, name: string) {
  await (await listbox(combobox)).getByRole('option', { name, exact: true }).click();
  await expect(combobox).toHaveAttribute('aria-expanded', 'false');
}
/** 열린 목록의 항목 이름. 읽은 뒤 Esc로 닫는다. */
export async function optionNames(combobox: Locator) {
  const names = await (await listbox(combobox)).getByRole('option').allTextContents();
  await combobox.press('Escape');
  return names;
}
/**
 * Provider 키를 아직 고르지 않았으면 키 주인에게 인가된 첫 키를 고른다.
 * 발급 폼은 인가된 키가 하나뿐이어도 미리 골라 두지 않는다. 목록의 첫 항목 「연동 안 함」(빈 값)은 건너뛴다.
 * 허용할 API 체크박스는 Provider 키를 고른 뒤에야 나타나므로, API를 고르기 전에 먼저 부른다.
 */
export async function ensureProviderKey(scope: Locator, provider = 'Anthropic') {
  const combobox = scope.getByLabel(`${provider} Provider 키`, { exact: true });
  await expect(combobox).toBeVisible();
  if (await combobox.getAttribute('data-value')) return;
  const list = await listbox(combobox);
  await list.getByRole('option').filter({ hasNotText: '연동 안 함' }).first().click();
  await expect(combobox).toHaveAttribute('aria-expanded', 'false');
  await expect(combobox).not.toHaveAttribute('data-value', '');
}

/** 값(예: 사용자 ID)으로 항목을 고른다. 화면에 보이는 이름을 시험이 모를 때 쓴다. 목록이 서버에서 올 때까지 기다린다. */
export async function pickValue(combobox: Locator, value: string) {
  await (await listbox(combobox)).locator(`[role="option"][data-value="${value}"]`).click();
  await expect(combobox).toHaveAttribute('data-value', value);
}
