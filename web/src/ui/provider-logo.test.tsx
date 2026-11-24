import { cleanup, render, screen } from '@testing-library/react';
import { afterEach, expect, test } from 'vitest';
import { ProviderLogo } from './provider-logo';
afterEach(cleanup);

test('provider_logo_is_an_image_named_after_the_provider_and_falls_back_to_the_name', () => {
  render(<><ProviderLogo provider="anthropic" name="Anthropic" /><ProviderLogo provider="openai" name="OpenAI" /><ProviderLogo provider="deepseek" name="DeepSeek" /><ProviderLogo provider="other" name="Other" /></>);
  // 로고가 있는 Provider는 이름을 가진 그림이다. 그림 안에 다른 글자는 없다.
  for (const name of ['Anthropic', 'OpenAI', 'DeepSeek']) {
    const logo = screen.getByRole('img', { name });
    expect(logo.querySelector('svg')).toBeTruthy();
    expect(logo.textContent?.trim()).toBe('');
  }
  // 로고가 없는 Provider는 이름 글자로 보인다.
  expect(screen.queryByRole('img', { name: 'Other' })).toBeNull();
  expect(screen.getByText('Other')).toBeTruthy();
});
