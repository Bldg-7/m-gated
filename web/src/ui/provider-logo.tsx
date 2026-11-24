import anthropic from './provider-logos/anthropic.svg?raw';
import deepseek from './provider-logos/deepseek.svg?raw';
import openai from './provider-logos/openai.svg?raw';

/** Provider 로고(SVG 원문). 출처와 사용 조건은 provider-logos/README.md 에 있다. */
const logos: Record<string, string> = { anthropic, deepseek, openai };

/**
 * Provider 로고. 로고가 있으면 그림으로, 없으면 이름 글자로 보인다.
 * 그림은 role="img"와 이름(aria-label)을 가져 읽기 도구가 Provider 이름을 읽고, 마우스를 올리면 이름이 뜬다.
 * SVG는 저장소에 들어 있는 고정 파일이라 외부에서 불러오지 않는다(CSP).
 */
export function ProviderLogo({ provider, name }: { provider: string; name: string }) {
  const svg = logos[provider];
  if (!svg) return <span className="provider-name">{name}</span>;
  return <span className={`provider-logo provider-logo-${provider}`} role="img" aria-label={name} title={name} dangerouslySetInnerHTML={{ __html: svg }} />;
}
