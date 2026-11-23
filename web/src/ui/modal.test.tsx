import { cleanup, fireEvent, render, screen, within } from '@testing-library/react';
import { useState } from 'react';
import { afterEach, expect, test, vi } from 'vitest';
import { Modal, Select } from './ui';
afterEach(cleanup);

function Harness({ busy = false, onConfirm = () => {}, variant }: { busy?: boolean; onConfirm?: () => void; variant?: 'normal' | 'danger' }) {
  const [open, setOpen] = useState(false);
  return <main>
    <button onClick={() => setOpen(true)}>열기</button>
    {open && <Modal title="삭제 확인" variant={variant} busy={busy} onClose={() => setOpen(false)} confirm={{ label: '삭제', onConfirm }}>
      <p>되돌릴 수 없습니다.</p>
      <Select aria-label="방식" defaultValue="a" options={[{ value: 'a', label: 'A' }, { value: 'b', label: 'B' }]} />
    </Modal>}
  </main>;
}

test('danger_modal_focuses_cancel_first_keeps_tab_inside_and_returns_focus_to_the_opener', () => {
  render(<Harness variant="danger" />);
  const opener = screen.getByRole('button', { name: '열기' });
  opener.focus();
  fireEvent.click(opener);
  const dialog = screen.getByRole('dialog', { name: '삭제 확인' });
  expect(dialog.tagName).toBe('DIALOG');
  expect(dialog.hasAttribute('open')).toBe(true);
  expect(dialog.getAttribute('aria-modal')).toBe('true');
  expect(dialog.className).toContain('modal-danger');
  // 위험 창은 '취소'에 처음 초점을 둔다. 바로 Enter를 눌러도 실행되지 않는다.
  const first = within(dialog).getByRole('combobox', { name: '방식' });
  const confirm = within(dialog).getByRole('button', { name: '삭제' }), cancel = within(dialog).getByRole('button', { name: '취소' });
  expect(document.activeElement).toBe(cancel);
  expect(confirm.className).toBe('btn-danger');
  fireEvent.keyDown(cancel, { key: 'Tab' });
  expect(document.activeElement).toBe(first);
  fireEvent.keyDown(first, { key: 'Tab', shiftKey: true });
  expect(document.activeElement).toBe(cancel);
  fireEvent.click(cancel);
  expect(screen.queryByRole('dialog')).toBeNull();
  expect(document.activeElement).toBe(opener);
});

test('modal_closes_on_escape_and_backdrop_click_but_escape_in_an_open_select_closes_only_the_list', () => {
  render(<Harness />);
  fireEvent.click(screen.getByRole('button', { name: '열기' }));
  let dialog = screen.getByRole('dialog', { name: '삭제 확인' });
  const select = within(dialog).getByRole('combobox', { name: '방식' });
  // 보통 창은 첫 버튼(본문의 목록 선택)에 처음 초점을 둔다.
  expect(document.activeElement).toBe(select);
  fireEvent.click(select);
  fireEvent.keyDown(select, { key: 'Escape' });
  expect(select.getAttribute('aria-expanded')).toBe('false');
  expect(screen.getByRole('dialog', { name: '삭제 확인' })).toBe(dialog);
  fireEvent.keyDown(select, { key: 'Escape' });
  expect(screen.queryByRole('dialog')).toBeNull();
  fireEvent.click(screen.getByRole('button', { name: '열기' }));
  dialog = screen.getByRole('dialog', { name: '삭제 확인' });
  // 카드 안을 누르면 닫지 않고, 어두운 배경(dialog 자신)을 누르면 닫는다.
  fireEvent.mouseDown(within(dialog).getByText('되돌릴 수 없습니다.'));
  fireEvent.click(within(dialog).getByText('되돌릴 수 없습니다.'));
  expect(screen.getByRole('dialog')).toBe(dialog);
  fireEvent.mouseDown(dialog);
  fireEvent.click(dialog);
  expect(screen.queryByRole('dialog')).toBeNull();
});

test('busy_modal_cannot_be_closed_by_escape_backdrop_or_cancel', () => {
  const onConfirm = vi.fn();
  render(<Harness busy onConfirm={onConfirm} />);
  fireEvent.click(screen.getByRole('button', { name: '열기' }));
  const dialog = screen.getByRole('dialog', { name: '삭제 확인' });
  expect(dialog.getAttribute('aria-busy')).toBe('true');
  fireEvent.keyDown(dialog, { key: 'Escape' });
  fireEvent(dialog, new Event('cancel', { cancelable: true }));
  fireEvent.mouseDown(dialog);
  fireEvent.click(dialog);
  const cancel = within(dialog).getByRole('button', { name: '취소' }) as HTMLButtonElement, confirm = within(dialog).getByRole('button', { name: '삭제' }) as HTMLButtonElement;
  expect(cancel.disabled).toBe(true);
  expect(confirm.disabled).toBe(true);
  fireEvent.click(cancel);
  fireEvent.click(confirm);
  expect(screen.getByRole('dialog', { name: '삭제 확인' })).toBe(dialog);
  expect(onConfirm).not.toHaveBeenCalled();
});

test('modal_without_backdrop_close_keeps_one_time_content_until_an_explicit_close', () => {
  const onClose = vi.fn();
  render(<Modal title="발급된 키" closeOnBackdrop={false} onClose={onClose} actions={<button onClick={onClose}>닫기</button>}><code>secret</code></Modal>);
  const dialog = screen.getByRole('dialog', { name: '발급된 키' });
  fireEvent.mouseDown(dialog);
  fireEvent.click(dialog);
  expect(onClose).not.toHaveBeenCalled();
  expect(document.activeElement).toBe(within(dialog).getByRole('button', { name: '닫기' }));
  fireEvent.click(within(dialog).getByRole('button', { name: '닫기' }));
  expect(onClose).toHaveBeenCalledOnce();
});
