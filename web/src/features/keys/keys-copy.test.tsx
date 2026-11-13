import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../../app';
import { session } from '../../lib/api';
const me = { principal: { id: 'human1', display_name: 'Synthetic', email: 'a@example.test' }, role: 'user', grants: [{ provider: 'anthropic', model: 'synthetic-text', capabilities: ['messages'] }], sso: { refresh_url: 'https://auth.example.test/sso/refresh' } };
const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
afterEach(() => { cleanup(); session.clear(); history.replaceState(null, '', '/ui/'); vi.unstubAllGlobals(); Reflect.deleteProperty(navigator, 'clipboard'); });

async function issue() {
  history.replaceState(null, '', '/ui/keys');
  vi.stubGlobal('fetch', vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    if (String(input) === '/gateway/me') return Promise.resolve(response(me));
    if (String(input) === '/gateway/csrf') return Promise.resolve(response({ token: 'synthetic-csrf', expires_at: '2099-01-01T00:00:00Z' }));
    if (init?.method === 'POST') return Promise.resolve(response({ key: 'one-time-synthetic-secret', metadata: { id: 'key1' } }, 201));
    return Promise.resolve(response({ items: [], next_cursor: null, as_of: '2026-01-01T00:00:00Z' }));
  }));
  render(<App />);
  await screen.findByRole('heading', { name: '내 키' });
  fireEvent.change(screen.getByLabelText('키 이름'), { target: { value: '복사 키' } });
  fireEvent.change(screen.getByLabelText('유효기간'), { target: { value: 'unlimited' } });
  fireEvent.submit(screen.getByRole('form', { name: '개인 키 발급' }));
  fireEvent.click(await screen.findByRole('button', { name: '발급 확인' }));
  await screen.findByText('one-time-synthetic-secret');
}

test('issued_key_is_copied_only_when_the_copy_button_is_pressed', async () => {
  const writeText = vi.fn(() => Promise.resolve());
  Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true });
  await issue();
  expect(writeText).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole('button', { name: '원문 복사' }));
  await screen.findByText('클립보드에 복사했습니다.');
  expect(writeText).toHaveBeenCalledExactlyOnceWith('one-time-synthetic-secret');
  fireEvent.click(screen.getByRole('button', { name: '원문 닫기' }));
  expect(screen.queryByText('one-time-synthetic-secret')).toBeNull();
  expect(screen.queryByRole('button', { name: '원문 복사' })).toBeNull();
  expect(localStorage.length + sessionStorage.length).toBe(0);
});

test('copy_failure_keeps_the_key_visible_and_asks_for_manual_copy', async () => {
  Object.defineProperty(navigator, 'clipboard', { value: { writeText: vi.fn(() => Promise.reject(new Error('denied'))) }, configurable: true });
  await issue();
  fireEvent.click(screen.getByRole('button', { name: '원문 복사' }));
  await screen.findByText('복사하지 못했습니다. 원문을 직접 선택해 복사하세요.');
  expect(screen.getByText('one-time-synthetic-secret')).toBeTruthy();
});
