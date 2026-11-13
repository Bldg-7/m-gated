import { useEffect, useState, useSyncExternalStore, useRef, type FormEvent } from 'react';
import { flushSync } from 'react-dom';
import { useTranslation } from 'react-i18next';
import { ApiError, session, StaleResponse } from './lib/api';
import type { Usage } from './lib/types';
import { UsageView } from './features/usage/usage';
import { KeysScreen } from './features/keys/keys';
import { PrincipalsScreen } from './features/admin/principals';
import { RequestsScreen } from './features/admin/requests';
import { CredentialsScreen } from './features/admin/credentials';
import { ModelsScreen } from './features/admin/models';
import { isScreen, Link, useRoute, type Screen } from './lib/router';
import { changeLanguage, languages, msgid } from './lib/i18n';
import { Pill, Select } from './ui/ui';
export function UsageScreen({ admin = false }: { admin?: boolean }) {
  const { t } = useTranslation();
  const auth = useSyncExternalStore(session.subscribe, session.snapshot);
  const [usage, setUsage] = useState<Usage | null>(null), [error, setError] = useState<string | null>(null), [loading, setLoading] = useState(true);
  const [query, setQuery] = useState(() => `group_by=provider${admin && new URLSearchParams(location.search).get('principal_id') ? `&principal_id=${encodeURIComponent(new URLSearchParams(location.search).get('principal_id')!)}` : ''}`);
  const [filters, setFilters] = useState(() => `group_by=provider${admin && new URLSearchParams(location.search).get('principal_id') ? `&principal_id=${encodeURIComponent(new URLSearchParams(location.search).get('principal_id')!)}` : ''}`);
  const [revision, setRevision] = useState(0);
  useEffect(() => {
    let current = true; setUsage(null); setError(null); setLoading(true);
    // 같은 주체의 refresh 동안 App이 이 화면을 유지하며 Session만 읽기를 재시도한다.
    // 그 밖의 인증 변경은 App의 unmount/key 변경으로 이전 결과를 폐기한다.
    void session.read<Usage>(`/gateway/${admin ? 'admin/' : ''}usage?${query}`).then(data => { if (current) { setUsage(data); setLoading(false); } }).catch(e => { if (!current || e instanceof StaleResponse) return; setUsage(null); setLoading(false); setError(e instanceof ApiError && e.status === 409 ? t('상세 사용량을 제공할 수 없습니다. 상세 가용 시작: {{from}}', { from: String(e.details.detail_available_from ?? t('미확인')) }) : e instanceof ApiError && e.status === 403 ? t('현재 권한으로 조회할 수 없습니다.') : t('사용량을 가져올 수 없습니다. 다시 조회하세요.')); });
    return () => { current = false; };
  }, [query, revision]);
  const submit = (event: FormEvent<HTMLFormElement>) => { event.preventDefault(); const data = new FormData(event.currentTarget); const q = new URLSearchParams(); const from = String(data.get('from') ?? ''); const to = String(data.get('to') ?? ''); if (Boolean(from) !== Boolean(to)) { setUsage(null); setError(t('시작과 종료를 모두 지정하세요.')); return; } if (from && to) { q.set('from', new Date(from).toISOString()); q.set('to', new Date(to).toISOString()); } for (const name of [...(admin ? ['principal_id'] : []), 'provider', 'model', 'key_id', 'group_by']) { const value = String(data.get(name) ?? ''); if (value) q.set(name, value); } setFilters(q.toString()); setQuery(q.toString()); setRevision(v => v + 1); };
  if (auth.status !== 'ready') return null;
  const selected = new URLSearchParams(filters);
  const localTime = (value: string | null) => value ? new Date(new Date(value).getTime() - new Date(value).getTimezoneOffset() * 60000).toISOString().slice(0, 16) : '';
  return <>
    <form onSubmit={submit} key={auth.epoch} aria-label={t('사용량 조회')} className="filters">
      <label>{t('시작 (현지 시각)')}<input type="datetime-local" name="from" defaultValue={localTime(selected.get('from'))} /></label>
      <label>{t('종료 (현지 시각)')}<input type="datetime-local" name="to" defaultValue={localTime(selected.get('to'))} /></label>
      <div className="field"><label htmlFor="usage-provider">Provider</label><Select id="usage-provider" name="provider" defaultValue={selected.get('provider') ?? ''}><option value="">{t('전체')}</option><option value="anthropic">Anthropic</option><option value="openai">OpenAI</option><option value="deepseek">DeepSeek</option></Select></div>
      <label>{t('모델')}<input name="model" maxLength={200} defaultValue={selected.get('model') ?? ''} /></label>
      {admin && <label>{t('주체 ID')}<input name="principal_id" className="mono" spellCheck={false} defaultValue={selected.get('principal_id') ?? ''} /></label>}
      <label>{t('키 ID')}<input name="key_id" className="mono" spellCheck={false} defaultValue={selected.get('key_id') ?? ''} /></label>
      <div className="field"><label htmlFor="usage-group">{t('그룹')}</label><Select id="usage-group" name="group_by" defaultValue={selected.get('group_by') ?? ''}><option value="">{t('없음')}</option><option value="provider">Provider</option><option value="model">{t('모델')}</option><option value="key">{t('키')}</option></Select></div>
      <div className="actions"><button className="btn-primary" disabled={loading} aria-busy={loading || undefined} type="submit">{t('조회')}</button></div>
    </form>
    {loading && <p role="status">{t('사용량 조회 중…')}</p>}{error && <p role="alert">{error}</p>}{usage && <><UsageView usage={usage} />{usage.buckets.next_cursor && <div className="actions"><button onClick={() => setQuery(`cursor=${encodeURIComponent(usage.buckets.next_cursor!)}`)}>{t('다음 그룹 페이지')}</button></div>}</>}
  </>;
}
const pages: Record<Screen, { nav: string; title: string; lead?: string }> = {
  '/ui/': { nav: msgid('내 사용량'), title: msgid('내 사용량'), lead: msgid('조회 기간의 확정 비용과 현재 예산 잔액입니다. 모든 값은 서버가 관측한 시각 기준이며 화면에서 다시 계산하지 않습니다.') },
  '/ui/keys': { nav: msgid('내 키'), title: msgid('내 API 키') },
  '/ui/admin/principals': { nav: msgid('주체·정책·키 관리'), title: msgid('주체 · 정책 · 키'), lead: msgid('사람·서비스 주체의 정책, 자동 안전차단, 가상 키를 관리합니다.') },
  '/ui/admin/usage': { nav: msgid('전체 사용량'), title: msgid('전체 사용량'), lead: msgid('주체·키별 사용량을 조회합니다. 주체를 지정하지 않은 전체 합계에는 예산 잔액이 없습니다.') },
  '/ui/admin/requests': { nav: msgid('미정산·처리 내역'), title: msgid('미정산 · 처리 내역'), lead: msgid('기본은 전체 기간 unknown입니다. pending 단독도 무기간 전체 조회이며 settled 기본은 당월입니다. 종료와 비용 확인은 별개입니다.') },
  '/ui/admin/models': { nav: msgid('모델·가격'), title: msgid('모델 · 가격 카탈로그'), lead: msgid('공급사 모델과 가격을 공개 가격 목록(models.dev, LiteLLM, OpenRouter)에서 불러옵니다. 동기화는 변경안만 만들고, 관리자가 이전 값과 비교해 승인해야 적용됩니다. 이미 기록된 요청의 금액은 바뀌지 않습니다.') },
  '/ui/admin/credentials': { nav: msgid('Provider 키'), title: msgid('Provider 회사 키'), lead: msgid('회사 키는 DB에 암호화해 저장합니다. 저장 전에 Provider의 모델 목록 API로 실제 확인을 1회 거치며, 거절되거나 확인이 안 되면 저장하지 않습니다. 저장한 키는 다시 볼 수 없고 끝 4자리만 표시합니다. 다른 서버 인스턴스에는 상태 점검 주기(수 초) 안에 반영되고, 진행 중인 요청은 시작할 때의 키를 끝까지 씁니다.') },
};
/** 서버가 내준 경로를 화면으로. 등록되지 않은 /ui/admin/* 는 전체 사용량, 그 밖은 내 사용량(기존 동작과 같음). */
function screenOf(path: string): Screen {
  return isScreen(path) ? path : path.startsWith('/ui/admin/') ? '/ui/admin/usage' : '/ui/';
}
/** 언어 선택. 각 언어 이름은 그 언어로 적는다. */
function LanguageSwitch() {
  const { t, i18n } = useTranslation();
  return <fieldset className="lang-switch"><legend className="sr-only">{t('언어')}</legend>
    {languages.map(language => <label key={language.code} lang={language.code}><input type="radio" name="language" value={language.code} checked={i18n.language === language.code} onChange={() => changeLanguage(language.code)} />{language.label}</label>)}
  </fieldset>;
}
export function App() {
  const { t } = useTranslation();
  const auth = useSyncExternalStore(session.subscribe, session.snapshot);
  const principal = useRef<string | null>(null);
  if (auth.me) principal.current = auth.me.principal.id; else if (!auth.refreshing) principal.current = null;
  useEffect(() => {
    const detach = session.attach();
    // bfcache가 DOM을 동결하기 전에 원문/폼을 동기적으로 제거한다.
    const pagehide = () => flushSync(() => session.clear());
    window.addEventListener('pagehide', pagehide);
    void session.validate();
    return () => { window.removeEventListener('pagehide', pagehide); detach(); };
  }, []);
  const route = useRoute(), screen = screenOf(route.path), page = pages[screen];
  const title = useRef<HTMLHeadingElement>(null), firstRoute = useRef(true);
  const pageTitle = t(page.title);
  useEffect(() => { document.title = `${pageTitle} · m-gated`; }, [pageTitle]);
  useEffect(() => {
    // 첫 진입은 브라우저 기본 초점을 두고, 화면 전환 뒤에만 새 화면 제목으로 초점을 옮긴다.
    if (firstRoute.current) { firstRoute.current = false; return; }
    title.current?.focus();
  }, [route.href]);
  const link = (path: Screen) => <Link href={path} aria-current={screen === path ? 'page' : undefined}>{t(pages[path].nav)}</Link>;
  const content = screen.startsWith('/ui/admin/') ? (auth.me?.role !== 'admin' ? <p role="alert">{t('현재 관리자 권한이 없습니다.')}</p> : screen === '/ui/admin/principals' ? <PrincipalsScreen key={`${principal.current}:${auth.epoch}`} /> : screen === '/ui/admin/requests' ? <RequestsScreen key={`${principal.current}:${auth.epoch}`} /> : screen === '/ui/admin/credentials' ? <CredentialsScreen key={`${principal.current}:${auth.epoch}`} /> : screen === '/ui/admin/models' ? <ModelsScreen key={`${principal.current}:${auth.epoch}`} /> : <UsageScreen admin key={principal.current} />) : screen === '/ui/keys' ? <KeysScreen key={principal.current} /> : <UsageScreen key={principal.current} />;
  return <div className="app">
    <aside className="sidebar">
      <div className="brand"><span className="brand-mark" aria-hidden="true">mg</span>m-gated</div>
      {auth.status === 'ready' && auth.me && <section className="identity" aria-label={t('로그인 사용자')}>
        <div className="identity-top"><span className="identity-name">{auth.me.principal.display_name}</span><Pill accent={auth.me.role === 'admin'}>{auth.me.role === 'admin' ? t('관리자') : t('사용자')}</Pill></div>
        <span className="identity-email">{auth.me.principal.email}</span>
        <div className="actions"><button className="btn-sm" onClick={() => session.logout()}>{t('로그아웃')}</button></div>
      </section>}
      <nav aria-label={t('기본')} className="nav">
        <div className="nav-group"><span className="nav-label">{t('내 계정')}</span>{link('/ui/')}{link('/ui/keys')}</div>
        {auth.me?.role === 'admin' && <div className="nav-group"><span className="nav-label">{t('관리')}</span>{link('/ui/admin/principals')}{link('/ui/admin/usage')}{link('/ui/admin/requests')}{link('/ui/admin/credentials')}{link('/ui/admin/models')}</div>}
      </nav>
      <LanguageSwitch />
    </aside>
    <main className="content"><div className="page">
      <header className="page-header"><h1 className="page-title" tabIndex={-1} ref={title}>{pageTitle}</h1>{page.lead && <p className="lead">{t(page.lead)}</p>}</header>
      {auth.status === 'checking' && <p role="status">{t('인증 확인 중…')}</p>}
      {auth.status === 'error' && <div className="auth-state"><p role="alert">{auth.error && t(auth.error)}</p><div className="actions"><a className="btn btn-primary" href="/auth/login">{t('SSO 로그인')}</a><button onClick={() => void session.validate()}>{t('다시 확인')}</button></div></div>}
      {(auth.status === 'ready' || auth.refreshing) && <div key={route.href} className="stack-lg">{content}</div>}
    </div></main>
  </div>;
}
