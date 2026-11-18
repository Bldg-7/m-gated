import { act, cleanup, fireEvent, render, screen, within } from '@testing-library/react';
import { useState } from 'react';
import { afterEach, expect, test, vi } from 'vitest';
import { optionNames, pickOption, selectedValue } from '../test-select';
import { SearchSelect, Select, type SelectOption } from './ui';
afterEach(cleanup);

const fruits: SelectOption[] = [{ value: 'apple', label: 'Apple' }, { value: 'banana', label: 'Banana' }, { value: 'cherry', label: 'Cherry', disabled: true }, { value: 'date', label: 'Date' }, { value: 'durian', label: 'Durian' }];
const listbox = (combobox: HTMLElement) => document.getElementById(combobox.getAttribute('aria-controls')!)!;
const active = (combobox: HTMLElement) => document.getElementById(combobox.getAttribute('aria-activedescendant') ?? '')?.textContent ?? null;
const key = (element: HTMLElement, value: string, extra: Record<string, unknown> = {}) => fireEvent.keyDown(element, { key: value, ...extra });

test('select_is_a_labelled_combobox_and_moves_with_arrow_home_end_enter_escape_and_tab', () => {
  const onChange = vi.fn();
  render(<><label htmlFor="fruit">과일</label><Select id="fruit" name="fruit" defaultValue="banana" onChange={onChange} options={fruits} /></>);
  const combobox = screen.getByLabelText('과일');
  expect(screen.getByRole('combobox', { name: '과일' })).toBe(combobox);
  expect(combobox.getAttribute('aria-haspopup')).toBe('listbox');
  expect(combobox.getAttribute('aria-expanded')).toBe('false');
  expect(combobox.textContent).toBe('Banana');
  combobox.focus();
  key(combobox, 'ArrowDown');
  expect(combobox.getAttribute('aria-expanded')).toBe('true');
  expect(within(listbox(combobox)).getAllByRole('option')).toHaveLength(5);
  expect(within(listbox(combobox)).getByRole('option', { name: 'Banana' }).getAttribute('aria-selected')).toBe('true');
  expect(active(combobox)).toBe('Banana');
  key(combobox, 'ArrowDown');
  expect(active(combobox)).toBe('Date'); // 비활성 항목(Cherry)은 건너뛴다.
  key(combobox, 'Home');
  expect(active(combobox)).toBe('Apple');
  key(combobox, 'ArrowUp');
  expect(active(combobox)).toBe('Apple');
  key(combobox, 'End');
  expect(active(combobox)).toBe('Durian');
  key(combobox, 'Escape');
  expect(combobox.getAttribute('aria-expanded')).toBe('false');
  expect(selectedValue(combobox)).toBe('banana');
  expect(onChange).not.toHaveBeenCalled();
  key(combobox, 'End');
  key(combobox, 'Enter');
  expect(selectedValue(combobox)).toBe('durian');
  expect(combobox.textContent).toBe('Durian');
  expect(onChange).toHaveBeenCalledExactlyOnceWith('durian');
  key(combobox, 'Enter');
  expect(combobox.getAttribute('aria-expanded')).toBe('true');
  key(combobox, 'Tab');
  expect(combobox.getAttribute('aria-expanded')).toBe('false');
  expect(selectedValue(combobox)).toBe('durian');
});

test('select_type_ahead_jumps_to_the_option_starting_with_the_typed_character', () => {
  let now = 10_000;
  vi.spyOn(Date, 'now').mockImplementation(() => now);
  render(<Select aria-label="과일" defaultValue="apple" options={fruits} />);
  const combobox = screen.getByRole('combobox', { name: '과일' });
  key(combobox, 'd');
  expect(combobox.getAttribute('aria-expanded')).toBe('true');
  expect(active(combobox)).toBe('Date');
  key(combobox, 'd');
  expect(active(combobox)).toBe('Durian'); // 같은 글자를 다시 치면 다음 항목으로 간다.
  now += 1000; // 잠시 멈춘 뒤 친 글자는 새로 찾는다.
  key(combobox, 'D');
  key(combobox, 'u');
  expect(active(combobox)).toBe('Durian'); // 이어 친 글자는 합쳐서 찾는다.
  now += 1000;
  key(combobox, 'b');
  expect(active(combobox)).toBe('Banana');
  key(combobox, 'Enter');
  expect(selectedValue(combobox)).toBe('banana');
});

test('select_submits_through_a_hidden_input_and_supports_reset_required_and_disabled', () => {
  const submitted: (string | null)[] = [];
  render(<form aria-label="폼" onSubmit={event => { event.preventDefault(); submitted.push(new FormData(event.currentTarget).get('fruit') as string | null); }}>
    <label htmlFor="fruit">과일</label><Select id="fruit" name="fruit" required placeholder="선택하세요" options={fruits} />
    <label htmlFor="locked">잠김</label><Select id="locked" name="locked" disabled defaultValue="apple" options={fruits} />
  </form>);
  const form = screen.getByRole('form', { name: '폼' }) as HTMLFormElement, combobox = screen.getByLabelText('과일');
  expect(combobox.textContent).toBe('선택하세요');
  expect(combobox.getAttribute('aria-required')).toBe('true');
  // 빈 필수 선택은 브라우저 폼 검사를 통과하지 못하고, 오류로 표시된다.
  expect(form.checkValidity()).toBe(false);
  act(() => { form.reportValidity(); });
  expect(combobox.getAttribute('aria-invalid')).toBe('true');
  pickOption(combobox, 'Date');
  expect(combobox.getAttribute('aria-invalid')).toBeNull();
  expect(form.checkValidity()).toBe(true);
  fireEvent.submit(form);
  expect(submitted).toEqual(['date']);
  expect((form.querySelector('input[type="hidden"][name="fruit"]') as HTMLInputElement).value).toBe('date');
  // 이름 없는 검사용 입력은 전송되지 않는다.
  expect([...new FormData(form).keys()]).toEqual(['fruit']);
  // 비활성 선택은 열리지 않고 전송되지 않는다(기본 select와 같다).
  const locked = screen.getByLabelText('잠김') as HTMLButtonElement;
  expect(locked.disabled).toBe(true);
  fireEvent.click(locked);
  expect(locked.getAttribute('aria-expanded')).toBe('false');
  act(() => { form.reset(); });
  expect(selectedValue(combobox)).toBe('');
});

test('select_list_scrolls_inside_max_height_and_closes_on_outside_click', () => {
  render(<><button>바깥</button><Select aria-label="짧은 목록" maxHeight={120} options={fruits} /><Select aria-label="기본 목록" options={fruits} /></>);
  const short = screen.getByRole('combobox', { name: '짧은 목록' });
  fireEvent.click(short);
  expect(listbox(short).style.maxHeight).toBe('120px');
  fireEvent.mouseDown(screen.getByRole('button', { name: '바깥' }));
  expect(short.getAttribute('aria-expanded')).toBe('false');
  const normal = screen.getByRole('combobox', { name: '기본 목록' });
  fireEvent.click(normal);
  expect(listbox(normal).style.maxHeight).toBe('320px');
  expect(getComputedStyle(listbox(normal).parentElement!).position).toBe('fixed');
});

test('select_controlled_value_follows_the_parent', () => {
  function Controlled() {
    const [value, setValue] = useState('apple');
    return <><Select aria-label="과일" value={value} onChange={setValue} options={fruits} /><output>{value}</output></>;
  }
  render(<Controlled />);
  pickOption(screen.getByRole('combobox', { name: '과일' }), 'Banana');
  expect(screen.getByRole('status').textContent).toBe('banana');
  expect(screen.getByRole('combobox', { name: '과일' }).textContent).toBe('Banana');
});

test('search_select_filters_by_label_or_value_ignoring_case_and_shows_no_results', () => {
  const zones: SelectOption[] = [{ value: 'UTC', label: 'UTC' }, { value: 'Asia/Seoul', label: 'Asia/Seoul' }, { value: 'Asia/Tokyo', label: 'Asia/Tokyo' }, { value: 'cred-7', label: 'team-a (…0001)' }];
  render(<form aria-label="폼"><label htmlFor="zone">시간대</label><SearchSelect id="zone" name="zone" defaultValue="UTC" options={zones} /></form>);
  const input = screen.getByLabelText('시간대') as HTMLInputElement;
  expect(input.getAttribute('role')).toBe('combobox');
  expect(input.value).toBe('UTC');
  fireEvent.click(input);
  expect(optionNames(input)).toEqual(['UTC', 'Asia/Seoul', 'Asia/Tokyo', 'team-a (…0001)']);
  fireEvent.change(input, { target: { value: 'ASIA' } });
  expect(within(listbox(input)).getAllByRole('option').map(o => o.textContent)).toEqual(['Asia/Seoul', 'Asia/Tokyo']);
  expect(active(input)).toBe('Asia/Seoul');
  fireEvent.change(input, { target: { value: 'CRED-7' } }); // 값으로도 찾는다.
  expect(within(listbox(input)).getAllByRole('option').map(o => o.textContent)).toEqual(['team-a (…0001)']);
  fireEvent.change(input, { target: { value: 'mars' } });
  expect(within(listbox(input)).queryAllByRole('option')).toHaveLength(0);
  expect(screen.getByText('결과 없음')).toBeTruthy();
  fireEvent.change(input, { target: { value: 'tok' } });
  key(input, 'Enter');
  expect(selectedValue(input)).toBe('Asia/Tokyo');
  expect(input.value).toBe('Asia/Tokyo');
  expect(new FormData(screen.getByRole('form', { name: '폼' }) as HTMLFormElement).get('zone')).toBe('Asia/Tokyo');
});

test('search_select_restores_the_selected_label_on_blur_and_escape', () => {
  render(<><SearchSelect aria-label="시간대" defaultValue="Asia/Seoul" options={[{ value: 'UTC', label: 'UTC' }, { value: 'Asia/Seoul', label: 'Asia/Seoul' }]} /><button>다음</button></>);
  const input = screen.getByRole('combobox', { name: '시간대' }) as HTMLInputElement;
  fireEvent.change(input, { target: { value: 'ut' } });
  expect(input.value).toBe('ut');
  fireEvent.blur(input, { relatedTarget: screen.getByRole('button', { name: '다음' }) });
  expect(input.value).toBe('Asia/Seoul');
  expect(input.getAttribute('aria-expanded')).toBe('false');
  fireEvent.change(input, { target: { value: 'u' } });
  key(input, 'ArrowDown');
  key(input, 'Escape');
  expect(input.value).toBe('Asia/Seoul');
  expect(selectedValue(input)).toBe('Asia/Seoul');
});
