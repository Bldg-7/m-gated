import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, expect, test } from 'vitest';
import { TextArea } from './ui';
afterEach(cleanup);

test('textarea_with_max_length_shows_the_character_count_under_it', () => {
  render(<div><label htmlFor="reason">사유</label><TextArea id="reason" rows={4} maxLength={1000} defaultValue="12345" /></div>);
  const box = screen.getByLabelText('사유');
  // 글자 수는 입력칸 바로 뒤에 있고, 입력칸의 설명으로 이어진다.
  expect(screen.getByText('5 / 1000')).toBeTruthy();
  expect(box.getAttribute('aria-describedby')).toBe('reason-count');
  fireEvent.input(box, { target: { value: 'Provider 콘솔 청구 내역과 대조함' } });
  expect(screen.getByText('22 / 1000')).toBeTruthy();
});

test('textarea_without_max_length_has_no_count', () => {
  render(<div><label htmlFor="memo">메모</label><TextArea id="memo" rows={2} /></div>);
  expect(screen.getByLabelText('메모').getAttribute('aria-describedby')).toBeNull();
  expect(document.querySelector('.char-count')).toBeNull();
});
