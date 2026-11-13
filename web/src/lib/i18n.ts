import i18n from 'i18next';
import { initReactI18next } from 'react-i18next';
import { en } from '../locales/en';

/**
 * i18next 설정. 한국어 원문 자체를 번역 키로 쓴다(keySeparator·nsSeparator 끔).
 * 한국어는 번역 파일 없이 키가 곧 문구이고, 영어는 locales/en.ts가 같은 키에 영어 문구를 둔다.
 * 영어에 없는 키는 한국어 원문이 그대로 보인다.
 */
export type Language = 'ko' | 'en';
export const languages: { code: Language; label: string }[] = [{ code: 'ko', label: '한국어' }, { code: 'en', label: 'English' }];
const storageKey = 'm-gated.lang';

/** 기본은 한국어. 사용자가 영어를 고른 경우에만 이 브라우저에 그 선택 하나를 남긴다(인증·키 정보는 저장하지 않는다). */
function storedLanguage(): Language {
  try { return window.localStorage.getItem(storageKey) === 'en' ? 'en' : 'ko'; } catch { return 'ko'; }
}

void i18n.use(initReactI18next).init({
  resources: { ko: { translation: {} }, en: { translation: en } },
  lng: storedLanguage(),
  fallbackLng: false,
  keySeparator: false,
  nsSeparator: false,
  interpolation: { escapeValue: false },
  initAsync: false,
  react: { useSuspense: false },
});
document.documentElement.lang = i18n.language;
i18n.on('languageChanged', language => { document.documentElement.lang = language; });

/** 상수에 담아 두었다가 나중에 t()로 옮기는 문구 표시. 값은 그대로 돌려주며, 번역 누락 검사 테스트가 이 표시를 찾는다. */
export const msgid = <T extends string>(key: T) => key;

export function changeLanguage(language: Language) {
  try {
    if (language === 'en') window.localStorage.setItem(storageKey, language);
    else window.localStorage.removeItem(storageKey);
  } catch { /* 저장할 수 없는 환경이면 지금 화면에만 적용한다. */ }
  void i18n.changeLanguage(language);
}
export default i18n;
