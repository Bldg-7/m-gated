import { cleanup, fireEvent, render, screen, within } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import { App } from '../../app';
import { optionNames, pickOption, selectedValue } from '../../test-select';
import { session } from '../../lib/api';
const me = { principal: { id: 'human1', display_name: 'Synthetic', email: 'a@example.test' }, role: 'user', grants: [{ provider: 'anthropic', model: 'synthetic-text', capabilities: ['messages'] }], provider_credentials: [{ provider: 'anthropic', credentials: [{ id: 'cred1', label: 'team-a', hint: '0001' }] }], sso: { refresh_url: 'https://auth.example.test/sso/refresh' } };
const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
const noKeys = { ...me, provider_credentials: [] };
const secondKey = { ...me, grants: [...me.grants, { provider: 'openai', model: 'synthetic-gpt', capabilities: ['responses'] }], provider_credentials: [...me.provider_credentials, { provider: 'openai', credentials: [{ id: 'cred2', label: 'team-o', hint: '0002' }] }] };
function stub(profile: unknown = me) {
  const fetcher = vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    if (String(input) === '/api/me') return Promise.resolve(response(profile));
    if (String(input).startsWith('/api/usage')) return Promise.resolve(response({ error: { code: 'synthetic' } }, 503));
    if (String(input) === '/api/csrf') return Promise.resolve(response({ token: 'synthetic-csrf', expires_at: '2099-01-01T00:00:00Z' }));
    if (init?.method === 'POST') return Promise.resolve(response({ key: 'one-time-synthetic-secret', metadata: { id: 'key1' } }, 201));
    return Promise.resolve(response({ items: [], next_cursor: null, as_of: '2026-01-01T00:00:00Z' }));
  });
  vi.stubGlobal('fetch', fetcher);
  return fetcher;
}
const posts = (fetcher: ReturnType<typeof stub>) => fetcher.mock.calls.filter(c => c[1]?.method === 'POST');
const submitButton = () => screen.getByRole('button', { name: '발급 내용 확인' }) as HTMLButtonElement;
afterEach(() => { cleanup(); session.clear(); history.replaceState(null, '', '/dashboard/'); vi.unstubAllGlobals(); });
test('personal_keys_issue_route_requires_explicit_duration_and_clears_one_time_secret', async () => {
  history.replaceState(null, '', '/dashboard/keys/new');
  const fetcher = stub(); render(<App />);
  await screen.findByRole('region', { name: '가상 키 발급' });
  const duration = screen.getByLabelText('유효기간') as HTMLButtonElement;
  expect(duration.labels?.[0].textContent).toBe('유효기간');
  expect(selectedValue(duration)).toBe('');
  expect(duration.textContent).toBe('선택하세요');
  expect(optionNames(duration)).toEqual(['3개월', '6개월', '1년', '3년', '무제한 (만료 없음, 회수·정지는 적용)']);
  // 인가된 Provider 키가 하나뿐이어도 미리 골라 두지 않는다.
  const providerKey = screen.getByLabelText('Anthropic Provider 키');
  expect(selectedValue(providerKey)).toBe('');
  expect(optionNames(providerKey)).toEqual(['연동 안 함', 'team-a (…0001)']);
  fireEvent.change(screen.getByLabelText('키 이름'), { target: { value: '명시 키' } });
  pickOption(providerKey, 'team-a (…0001)');
  expect(selectedValue(providerKey)).toBe('cred1');
  fireEvent.click(screen.getByLabelText('anthropic / synthetic-text / messages'));
  // 유효기간을 명시하지 않으면 확인 창도 POST도 없다.
  fireEvent.submit(screen.getByRole('form', { name: '가상 키 발급' }));
  expect(screen.getByRole('alert').textContent).toBe('이름과 유효기간을 명시하세요.');
  expect(screen.queryByRole('dialog')).toBeNull();
  expect(posts(fetcher)).toHaveLength(0);
  pickOption(duration, '무제한 (만료 없음, 회수·정지는 적용)');
  expect(selectedValue(duration)).toBe('unlimited');
  fireEvent.submit(screen.getByRole('form', { name: '가상 키 발급' }));
  expect(posts(fetcher)).toHaveLength(0);
  const confirm = await screen.findByRole('dialog', { name: '발급 확인' });
  expect(confirm.textContent).toContain('Provider 키: Anthropic · team-a (…0001)');
  fireEvent.click(within(confirm).getByRole('button', { name: '발급 확인' }));
  const secret = await screen.findByRole('dialog', { name: '발급된 키' });
  expect(within(secret).getByText('one-time-synthetic-secret')).toBeTruthy();
  const post = posts(fetcher)[0];
  expect(posts(fetcher)).toHaveLength(1);
  expect(new Headers(post[1]?.headers).get('x-csrf-token')).toBe('synthetic-csrf');
  expect(new Headers(post[1]?.headers).get('x-gateway-command-id')).toMatch(/^[0-9a-f-]{36}$/);
  const sent = JSON.parse(String(post[1]?.body));
  expect(sent.validity).toBe('unlimited');
  expect(sent.grants).toEqual([{ provider: 'anthropic', model: 'synthetic-text', capabilities: ['messages'] }]);
  expect(sent.provider_credentials).toEqual({ anthropic: 'cred1' });
  // 키 창을 닫으면 키를 지우고 목록 페이지로 돌아간다.
  fireEvent.click(within(secret).getByRole('button', { name: '닫기' }));
  expect(screen.queryByText('one-time-synthetic-secret')).toBeNull();
  await screen.findByRole('region', { name: '내 가상 키' });
  expect(location.pathname).toBe('/dashboard/keys');
  expect(screen.queryByRole('form', { name: '가상 키 발급' })).toBeNull();
  expect(localStorage.length + sessionStorage.length).toBe(0);
});

test('key_list_has_no_issue_form_and_links_to_the_issue_page', async () => {
  history.replaceState(null, '', '/dashboard/keys');
  const fetcher = stub(); render(<App />);
  await screen.findByRole('region', { name: '내 가상 키' });
  await screen.findByText('키가 없습니다.');
  expect(screen.queryByRole('form', { name: '가상 키 발급' })).toBeNull();
  expect(screen.queryByLabelText('키 이름')).toBeNull();
  expect(screen.queryByRole('button', { name: '발급 내용 확인' })).toBeNull();
  const link = screen.getByRole('link', { name: '가상 키 발급' });
  expect(link.getAttribute('href')).toBe('/dashboard/keys/new');
  fireEvent.click(link);
  // 발급 페이지에는 발급 폼만 있고 키 목록은 없다.
  await screen.findByRole('region', { name: '가상 키 발급' });
  expect(location.pathname).toBe('/dashboard/keys/new');
  expect(screen.getByRole('form', { name: '가상 키 발급' })).toBeTruthy();
  expect(screen.queryByRole('form', { name: '키 조회' })).toBeNull();
  expect(screen.queryByText('키가 없습니다.')).toBeNull();
  expect(screen.queryByRole('link', { name: '가상 키 발급' })).toBeNull();
  expect(screen.getByRole('link', { name: '내 가상 키로 돌아가기' }).getAttribute('href')).toBe('/dashboard/keys');
  // 상위 메뉴는 발급 페이지에서도 내 가상 키를 현재 위치로 표시한다.
  expect(screen.getByRole('link', { name: '내 가상 키' }).getAttribute('aria-current')).toBe('page');
  const cancel = screen.getByRole('link', { name: '취소' });
  expect(cancel.getAttribute('href')).toBe('/dashboard/keys');
  fireEvent.click(cancel);
  await screen.findByRole('region', { name: '내 가상 키' });
  expect(location.pathname).toBe('/dashboard/keys');
  expect(posts(fetcher)).toHaveLength(0);
});

test('issue_submit_stays_disabled_until_a_provider_key_and_an_api_are_chosen', async () => {
  history.replaceState(null, '', '/dashboard/keys/new');
  const fetcher = stub(secondKey); render(<App />);
  const form = await screen.findByRole('form', { name: '가상 키 발급' });
  const anthropic = screen.getByLabelText('Anthropic Provider 키'), openai = screen.getByLabelText('OpenAI Provider 키');
  // Provider 키를 고르기 전에는 고를 API가 없다.
  expect(submitButton().disabled).toBe(true);
  expect(within(form).queryAllByRole('checkbox')).toHaveLength(0);
  expect(within(form).getByText('Provider 키를 먼저 고르면 그 Provider의 API를 고를 수 있습니다.')).toBeTruthy();
  // 버튼이 꺼져 있어도 폼 제출로 확인 창을 열 수 없다.
  fireEvent.change(screen.getByLabelText('키 이름'), { target: { value: '새 키' } });
  pickOption(screen.getByLabelText('유효기간'), '1년');
  fireEvent.submit(form);
  expect(screen.getByRole('alert').textContent).toBe('연동할 Provider 키를 하나 이상 고르세요.');
  expect(screen.queryByRole('dialog')).toBeNull();
  // Provider 키를 고르면 그 Provider의 API만 나타나고, API를 고를 때까지 버튼은 꺼져 있다.
  pickOption(anthropic, 'team-a (…0001)');
  // 허용할 API는 표로 고른다. 머리 칸의 전체 선택과 고른 Provider의 API 줄만 있다.
  expect(within(form).getAllByRole('checkbox').map(box => box.getAttribute('aria-label'))).toEqual(['전체 선택', 'anthropic / synthetic-text / messages']);
  const all = within(form).getByLabelText('전체 선택') as HTMLInputElement;
  fireEvent.click(all);
  expect((within(form).getByLabelText('anthropic / synthetic-text / messages') as HTMLInputElement).checked).toBe(true);
  fireEvent.click(all);
  expect((within(form).getByLabelText('anthropic / synthetic-text / messages') as HTMLInputElement).checked).toBe(false);
  expect(within(form).queryByLabelText('openai / synthetic-gpt / responses')).toBeNull();
  expect(submitButton().disabled).toBe(true);
  fireEvent.submit(form);
  expect(screen.getByRole('alert').textContent).toBe('Provider 키를 고른 Provider마다 허용할 API를 하나 이상 고르세요.');
  expect(screen.queryByRole('dialog')).toBeNull();
  fireEvent.click(within(form).getByLabelText('anthropic / synthetic-text / messages'));
  expect(submitButton().disabled).toBe(false);
  // Provider 키를 하나 더 고르면 그 Provider의 API도 하나 이상 골라야 한다.
  pickOption(openai, 'team-o (…0002)');
  expect(submitButton().disabled).toBe(true);
  fireEvent.click(within(form).getByLabelText('openai / synthetic-gpt / responses'));
  expect(submitButton().disabled).toBe(false);
  // 연동을 풀면 그 Provider의 API는 사라지고, 다시 골라도 허용이 되살아나지 않는다.
  pickOption(openai, '연동 안 함');
  expect(within(form).queryByLabelText('openai / synthetic-gpt / responses')).toBeNull();
  expect(submitButton().disabled).toBe(false);
  pickOption(openai, 'team-o (…0002)');
  expect((within(form).getByLabelText('openai / synthetic-gpt / responses') as HTMLInputElement).checked).toBe(false);
  expect(submitButton().disabled).toBe(true);
  pickOption(openai, '연동 안 함');
  // 연동한 Provider의 API와 Provider 키만 보낸다.
  fireEvent.submit(form);
  fireEvent.click(within(await screen.findByRole('dialog', { name: '발급 확인' })).getByRole('button', { name: '발급 확인' }));
  await screen.findByRole('dialog', { name: '발급된 키' });
  const sent = JSON.parse(String(posts(fetcher)[0][1]?.body));
  expect(sent.grants).toEqual([{ provider: 'anthropic', model: 'synthetic-text', capabilities: ['messages'] }]);
  expect(sent.provider_credentials).toEqual({ anthropic: 'cred1' });
});

test('user_without_an_authorized_provider_key_sees_the_reason_and_no_issue_form', async () => {
  history.replaceState(null, '', '/dashboard/keys/new');
  const fetcher = stub(noKeys); render(<App />);
  await screen.findByRole('region', { name: '가상 키 발급' });
  expect(screen.getByText('나에게 인가된 Provider 키가 없어 가상 키를 발급할 수 없습니다. 관리자에게 Provider 키 인가를 요청하세요.')).toBeTruthy();
  expect(screen.queryByRole('form', { name: '가상 키 발급' })).toBeNull();
  expect(screen.queryByLabelText('키 이름')).toBeNull();
  expect(screen.queryByRole('button', { name: '발급 내용 확인' })).toBeNull();
  expect(screen.getByRole('link', { name: '내 가상 키로 돌아가기' }).getAttribute('href')).toBe('/dashboard/keys');
  expect(posts(fetcher)).toHaveLength(0);
});
