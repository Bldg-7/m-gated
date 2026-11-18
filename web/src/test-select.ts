import { fireEvent, within } from '@testing-library/react';

/**
 * Select·SearchSelect(목록 선택)를 시험에서 다루는 도구.
 * 입력을 눌러 목록을 연 뒤, 사용자가 보는 이름으로 항목을 눌러 고른다.
 */
export function openOptions(combobox: HTMLElement): HTMLElement {
  if (combobox.getAttribute('aria-expanded') !== 'true') fireEvent.click(combobox);
  const id = combobox.getAttribute('aria-controls');
  const listbox = id ? document.getElementById(id) : null;
  if (!listbox) throw new Error('목록이 열리지 않았습니다.');
  return listbox;
}
export function pickOption(combobox: HTMLElement, name: string | RegExp) {
  fireEvent.click(within(openOptions(combobox)).getByRole('option', { name }));
}
/** 열린 목록의 항목 이름. 확인한 뒤 목록을 닫는다. */
export function optionNames(combobox: HTMLElement): string[] {
  const names = within(openOptions(combobox)).queryAllByRole('option').map(option => option.textContent ?? '');
  fireEvent.keyDown(combobox, { key: 'Escape' });
  return names;
}
/** 지금 고른 값(폼으로 전송되는 값). */
export function selectedValue(combobox: HTMLElement) {
  return combobox.getAttribute('data-value');
}
