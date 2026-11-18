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
/** 고른 Provider 키가 없으면 키 주인에게 인가된 첫 키를 고른다. 인가된 키가 하나뿐이면 화면이 미리 골라 둔다. */
export async function ensureProviderKey(scope: Locator, provider = 'Anthropic') {
  const combobox = scope.getByLabel(`${provider} Provider 키`, { exact: true });
  await expect(combobox).toBeVisible();
  if (await combobox.getAttribute('data-value')) return;
  const list = await listbox(combobox);
  await list.getByRole('option').first().click();
  await expect(combobox).not.toHaveAttribute('data-value', '');
}
