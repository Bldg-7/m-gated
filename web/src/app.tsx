import { useEffect, useState, useSyncExternalStore, useRef, type FormEvent } from 'react';
import { flushSync } from 'react-dom';
import { useTranslation } from 'react-i18next';
import { ApiError, session, StaleResponse } from './lib/api';
import type { KeyMetadata, Usage } from './lib/types';
import { PrincipalSelect, RequestModelSelect, useAll } from './features/admin/shared';
import { UsageView } from './features/usage/usage';
import { KeysScreen } from './features/keys/keys';
import { principalHref, PrincipalDetailScreen, PrincipalKeyIssueScreen, PrincipalsScreen } from './features/admin/principals';
import { RequestsScreen } from './features/admin/requests';
import { CredentialsScreen } from './features/admin/credentials';
import { ModelsScreen } from './features/admin/models';
import { SettingsScreen } from './features/admin/settings';
import { TokenCheckScreen } from './features/admin/token-check';
import { isScreen, Link, useRoute, type Screen } from './lib/router';
import { changeLanguage, languages, msgid } from './lib/i18n';
import { browserTimeZone, formatTime, timeBasis, useTimeBasis } from './lib/time';
import { Pill, SearchSelect, Select } from './ui/ui';
export function UsageScreen({ admin = false }: { admin?: boolean }) {
  const { t } = useTranslation();
  const auth = useSyncExternalStore(session.subscribe, session.snapshot);
  const [usage, setUsage] = useState<Usage | null>(null), [error, setError] = useState<string | null>(null), [loading, setLoading] = useState(true);
  const [query, setQuery] = useState(() => `group_by=provider${admin && new URLSearchParams(location.search).get('principal_id') ? `&principal_id=${encodeURIComponent(new URLSearchParams(location.search).get('principal_id')!)}` : ''}`);
  const [filters, setFilters] = useState(() => `group_by=provider${admin && new URLSearchParams(location.search).get('principal_id') ? `&principal_id=${encodeURIComponent(new URLSearchParams(location.search).get('principal_id')!)}` : ''}`);
  const [revision, setRevision] = useState(0);
  // 조회 조건의 사용자(관리자 화면). 고르면 가상 키 후보를 그 사용자의 키로 좁힌다. 내 사용량에서는 내 가상 키가 후보다.
  const [principalPick, setPrincipalPick] = useState(() => new URLSearchParams(filters).get('principal_id') ?? '');
  const keys = useAll<KeyMetadata>(admin ? `/api/admin/keys${principalPick ? `?principal_id=${encodeURIComponent(principalPick)}` : ''}` : '/api/keys');
  useEffect(() => {
    let current = true; setUsage(null); setError(null); setLoading(true);
    // 같은 사용자의 refresh 동안 App이 이 화면을 유지하며 Session만 읽기를 재시도한다.
    // 그 밖의 인증 변경은 App의 unmount/key 변경으로 이전 결과를 폐기한다.
    void session.read<Usage>(`/api/${admin ? 'admin/' : ''}usage?${query}`).then(data => { if (current) { setUsage(data); setLoading(false); } }).catch(e => { if (!current || e instanceof StaleResponse) return; setUsage(null); setLoading(false); setError(e instanceof ApiError && e.status === 409 ? t('상세 사용량을 제공할 수 없습니다. 상세 가용 시작: {{from}}', { from: typeof e.details.detail_available_from === 'string' ? formatTime(e.details.detail_available_from) : t('미확인') }) : e instanceof ApiError && e.status === 403 ? t('현재 권한으로 조회할 수 없습니다.') : t('사용량을 가져올 수 없습니다. 다시 조회하세요.')); });
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
      <div className="field"><label htmlFor="usage-provider">Provider</label><Select id="usage-provider" name="provider" defaultValue={selected.get('provider') ?? ''} options={[{ value: '', label: t('전체') }, { value: 'anthropic', label: 'Anthropic' }, { value: 'openai', label: 'OpenAI' }, { value: 'deepseek', label: 'DeepSeek' }]} /></div>
      {/* 요청 기록의 모델에서 고른다. 전체 사용량은 전체 기록, 내 사용량은 내 기록이 후보다. */}
      <div className="field"><label htmlFor="usage-model">{t('모델')}</label><RequestModelSelect id="usage-model" name="model" mine={!admin} defaultValue={selected.get('model') ?? ''} /></div>
      {admin && <div className="field"><label htmlFor="usage-principal">{t('사용자')}</label><PrincipalSelect id="usage-principal" name="principal_id" optional value={principalPick} onChange={setPrincipalPick} /></div>}
      <div className="field"><label htmlFor="usage-key">{t('가상 키')}</label><SearchSelect key={principalPick} id="usage-key" name="key_id" defaultValue={selected.get('principal_id') === principalPick || !admin ? selected.get('key_id') ?? '' : ''} placeholder={keys.items ? t('선택하세요') : t('키 목록 조회 중…')} options={[{ value: '', label: t('전체') }, ...(keys.items ?? []).map(k => ({ value: k.id, label: `${k.name} (${k.key_hint})` }))]} /></div>
      <div className="field"><label htmlFor="usage-group">{t('그룹')}</label><Select id="usage-group" name="group_by" defaultValue={selected.get('group_by') ?? ''} options={[{ value: '', label: t('없음') }, { value: 'provider', label: 'Provider' }, { value: 'model', label: t('모델') }, { value: 'key', label: t('키') }]} /></div>
      <div className="actions"><button className="btn-primary" disabled={loading} aria-busy={loading || undefined} type="submit">{t('조회')}</button></div>
    </form>
    {loading && <p role="status">{t('사용량 조회 중…')}</p>}{error && <p role="alert">{error}</p>}{usage && <><UsageView usage={usage} />{usage.buckets.next_cursor && <div className="actions"><button onClick={() => setQuery(`cursor=${encodeURIComponent(usage.buckets.next_cursor!)}`)}>{t('다음 그룹 페이지')}</button></div>}</>}
  </>;
}
/** 하위 페이지의 상위로 돌아가는 링크. label은 화면에 보이는 글, name은 읽기 도구가 읽는 전체 이름(label을 포함한다). */
type Back = { href: string; label: string; name: string };
const toPrincipals: Back = { href: '/dashboard/admin/principals', label: msgid('사용자 목록'), name: msgid('사용자 목록으로 돌아가기') };
const pages: Record<Screen, { nav: string; title: string; lead?: string; back?: (search: URLSearchParams) => Back }> = {
  '/dashboard/': { nav: msgid('내 사용량'), title: msgid('내 사용량'), lead: msgid('조회 기간의 확정 비용과 현재 예산 잔액입니다.') },
  '/dashboard/keys': { nav: msgid('내 가상 키'), title: msgid('내 가상 키') },
  '/dashboard/keys/new': { nav: msgid('내 가상 키'), title: msgid('가상 키 발급'), lead: msgid('연동할 Provider 키와 허용할 API를 골라 가상 키를 발급합니다.'), back: () => ({ href: '/dashboard/keys', label: msgid('내 가상 키'), name: msgid('내 가상 키로 돌아가기') }) },
  '/dashboard/admin/principals': { nav: msgid('사용자·정책·키 관리'), title: msgid('사용자 · 정책 · 키'), lead: msgid('사용자와 서비스 계정의 정책, 자동 안전차단, 가상 키를 관리합니다.') },
  '/dashboard/admin/principals/detail': { nav: msgid('사용자·정책·키 관리'), title: msgid('사용자 상세'), lead: msgid('사용자 하나의 정책, 자동 안전차단, 가상 키를 관리합니다.'), back: () => toPrincipals },
  '/dashboard/admin/principals/key': { nav: msgid('사용자·정책·키 관리'), title: msgid('서비스 계정의 가상 키 발급'), lead: msgid('서비스 계정에 인가된 Provider 키와 허용할 API를 골라 가상 키를 발급합니다.'), back: search => { const id = search.get('principal_id'); return id ? { href: principalHref(id), label: msgid('사용자 상세'), name: msgid('사용자 상세로 돌아가기') } : toPrincipals; } },
  '/dashboard/admin/keys': { nav: msgid('가상 키 관리'), title: msgid('가상 키 관리'), lead: msgid('모든 사용자와 서비스 계정의 가상 키를 조회하고 회수합니다. 가상 키가 쓰는 Provider 키도 여기서 바꿉니다.') },
  '/dashboard/admin/usage': { nav: msgid('전체 사용량'), title: msgid('전체 사용량'), lead: msgid('사용자·키별 사용량을 조회합니다. 사용자를 지정하지 않은 전체 합계에는 예산 잔액이 없습니다.') },
  '/dashboard/admin/requests': { nav: msgid('미정산·처리 내역'), title: msgid('미정산 · 처리 내역'), lead: msgid('기본은 전체 기간 unknown입니다. pending 단독도 무기간 전체 조회이며 settled 기본은 당월입니다. 종료와 비용 확인은 별개입니다.') },
  '/dashboard/admin/models': { nav: msgid('모델·가격'), title: msgid('모델 · 가격 카탈로그'), lead: msgid('공급사 모델과 가격을 공개 가격 목록 models.dev 에서 불러옵니다. 동기화는 변경안만 만들고, 관리자가 이전 값과 비교해 승인해야 적용됩니다. 이미 기록된 요청의 금액은 바뀌지 않습니다.') },
  '/dashboard/admin/token-check': { nav: msgid('토큰 대조'), title: msgid('토큰 대조'), lead: msgid('Anthropic 사용량 보고서와 게이트웨이가 정산한 토큰을 비교합니다. 차이는 알림을 보내지 않고 이 화면에만 보입니다.') },
  '/dashboard/admin/settings': { nav: msgid('설정'), title: msgid('게이트웨이 설정'), lead: msgid('게이트웨이 전체에 한 번 정하는 설정입니다. 월 예산의 기준 시간대와 끊긴 요청의 정산 규칙을 정합니다.') },
  '/dashboard/admin/credentials': { nav: msgid('Provider 키'), title: msgid('Provider 키'), lead: msgid('Provider마다 키를 등록하고 관리합니다. 사용자는 관리자가 인가한 키만 쓸 수 있습니다.') },
};
/** 서버가 내준 경로를 화면으로. 등록되지 않은 /dashboard/admin/* 는 전체 사용량, 그 밖은 내 사용량(기존 동작과 같음). */
function screenOf(path: string): Screen {
  return isScreen(path) ? path : path.startsWith('/dashboard/admin/') ? '/dashboard/admin/usage' : '/dashboard/';
}
/** 언어 선택. 각 언어 이름은 그 언어로 적는다. */
function LanguageSwitch() {
  const { t, i18n } = useTranslation();
  return <fieldset className="segment segment-sm"><legend className="sr-only">{t('언어')}</legend>
    {languages.map(language => <label key={language.code} lang={language.code}><input type="radio" name="language" value={language.code} checked={i18n.language === language.code} onChange={() => changeLanguage(language.code)} />{language.label}</label>)}
  </fieldset>;
}
/** 화면의 모든 시각을 브라우저 시간과 UTC 사이에서 바꾼다. 입력칸(현지 시각)에는 영향이 없다. */
function TimeSwitch() {
  const { t } = useTranslation();
  const basis = useTimeBasis();
  const hint = basis === 'utc' ? t('브라우저 시간으로 보기') : t('UTC로 보기');
  return <div className="time-switch">
    <button type="button" className="btn-sm" aria-label={hint} title={hint} onClick={() => timeBasis.set(basis === 'utc' ? 'browser' : 'utc')}>
      <svg className="icon" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><circle cx="8" cy="8" r="6" /><path d="M8 4.8V8l2.2 1.4" /></svg>
    </button>
    <span>{basis === 'utc' ? 'UTC' : t('브라우저 시간({{zone}})', { zone: browserTimeZone() })}</span>
  </div>;
}
export function App() {
  const { t } = useTranslation();
  const auth = useSyncExternalStore(session.subscribe, session.snapshot);
  const principal = useRef<string | null>(null);
  // 이 페이지에서 로그인·관리자 권한이 있었는지. 잃으면 화면(입력 중이던 내용, 발급된 키 원문)을 지웠다고 알리고 다시 로그인하게 한다.
  const wasReady = useRef(false), wasAdmin = useRef(false);
  if (auth.status === 'ready') { wasReady.current = true; if (auth.me?.role === 'admin') wasAdmin.current = true; }
  // 스스로 로그아웃한 경우는 알릴 것이 없다.
  const cleared = wasReady.current && auth.error !== '로그아웃되었습니다.';
  if (auth.me) principal.current = auth.me.principal.id; else if (!auth.refreshing) principal.current = null;
  useEffect(() => {
    const detach = session.attach();
    // bfcache가 DOM을 동결하기 전에 발급된 키/폼을 동기적으로 제거한다.
    const pagehide = () => flushSync(() => session.clear());
    window.addEventListener('pagehide', pagehide);
    void session.validate();
    return () => { window.removeEventListener('pagehide', pagehide); detach(); };
  }, []);
  const route = useRoute(), screen = screenOf(route.path), page = pages[screen];
  const title = useRef<HTMLHeadingElement>(null), firstRoute = useRef(true);
  const pageTitle = t(page.title), back = page.back?.(new URLSearchParams(route.search));
  useEffect(() => { document.title = `${pageTitle} · m-gated`; }, [pageTitle]);
  useEffect(() => {
    // 첫 진입은 브라우저 기본 초점을 두고, 화면 전환 뒤에만 새 화면 제목으로 초점을 옮긴다.
    if (firstRoute.current) { firstRoute.current = false; return; }
    title.current?.focus();
  }, [route.href]);
  // 하위 페이지(사용자 상세)에 있을 때도 상위 메뉴를 현재 위치로 표시한다.
  const link = (path: Screen) => <Link href={path} aria-current={screen === path || screen.startsWith(`${path}/`) ? 'page' : undefined}>{t(pages[path].nav)}</Link>;
  const content = screen.startsWith('/dashboard/admin/') ? (auth.me?.role !== 'admin' ? (wasAdmin.current && auth.status === 'ready' ? <div className="stack-sm"><p role="alert">{t('현재 관리자 권한이 없습니다.')} {t('입력 중이던 내용과 원문은 지웠습니다.')}</p><p><a href="/auth/login">{t('SSO 다시 로그인')}</a></p></div> : <p role="alert">{t('현재 관리자 권한이 없습니다.')}</p>) : screen === '/dashboard/admin/principals' ? <PrincipalsScreen key={`${principal.current}:${auth.epoch}`} /> : screen === '/dashboard/admin/principals/detail' ? <PrincipalDetailScreen key={`${principal.current}:${auth.epoch}:${route.search}`} /> : screen === '/dashboard/admin/principals/key' ? <PrincipalKeyIssueScreen key={`${principal.current}:${auth.epoch}:${route.search}`} /> : screen === '/dashboard/admin/keys' ? <KeysScreen adminAll key={`${principal.current}:${auth.epoch}`} /> : screen === '/dashboard/admin/requests' ? <RequestsScreen key={`${principal.current}:${auth.epoch}`} /> : screen === '/dashboard/admin/credentials' ? <CredentialsScreen key={`${principal.current}:${auth.epoch}`} /> : screen === '/dashboard/admin/settings' ? <SettingsScreen key={`${principal.current}:${auth.epoch}`} /> : screen === '/dashboard/admin/token-check' ? <TokenCheckScreen key={`${principal.current}:${auth.epoch}`} /> : screen === '/dashboard/admin/models' ? <ModelsScreen key={`${principal.current}:${auth.epoch}`} /> : <UsageScreen admin key={principal.current} />) : screen === '/dashboard/keys' ? <KeysScreen key={principal.current} /> : screen === '/dashboard/keys/new' ? <KeysScreen key={`${principal.current}:new`} view="issue" /> : <UsageScreen key={principal.current} />;
  return <div className="app">
    <aside className="sidebar">
      <div className="brand"><span className="brand-mark" aria-hidden="true">mg</span>m-gated</div>
      {auth.status === 'ready' && auth.me && <section className="identity" aria-label={t('로그인 사용자')}>
        <div className="identity-top"><span className="identity-name">{auth.me.principal.display_name}</span><Pill accent={auth.me.role === 'admin'}>{auth.me.role === 'admin' ? t('관리자') : t('사용자')}</Pill></div>
        <span className="identity-email">{auth.me.principal.email}</span>
        <div className="actions"><button className="btn-sm" onClick={() => session.logout()}>{t('로그아웃')}</button></div>
      </section>}
      <nav aria-label={t('기본')} className="nav">
        <div className="nav-group"><span className="nav-label">{t('내 계정')}</span>{link('/dashboard/')}{link('/dashboard/keys')}</div>
        {auth.me?.role === 'admin' && <div className="nav-group"><span className="nav-label">{t('관리')}</span>{link('/dashboard/admin/principals')}{link('/dashboard/admin/keys')}{link('/dashboard/admin/usage')}{link('/dashboard/admin/requests')}{link('/dashboard/admin/credentials')}{link('/dashboard/admin/models')}{link('/dashboard/admin/token-check')}{link('/dashboard/admin/settings')}</div>}
      </nav>
      <div className="sidebar-foot"><TimeSwitch /><LanguageSwitch /></div>
    </aside>
    <main className="content"><div className="page">
      <header className="page-header">
        {back && <Link className="back-link" href={back.href} aria-label={t(back.name)}><svg className="icon" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><path d="M10 3.5L5.5 8l4.5 4.5" /></svg>{t(back.label)}</Link>}
        <h1 className="page-title" tabIndex={-1} ref={title}>{pageTitle}</h1>{page.lead && <p className="lead">{t(page.lead)}</p>}</header>
      {auth.status === 'checking' && <p role="status">{t('인증 확인 중…')}</p>}
      {auth.status === 'error' && <div className="auth-state"><p role="alert">{auth.error && t(auth.error)}{cleared && <> {t('입력 중이던 내용과 원문은 지웠습니다.')}</>}</p><div className="actions"><a className="btn btn-primary" href="/auth/login">{cleared ? t('SSO 다시 로그인') : t('SSO 로그인')}</a><button onClick={() => void session.validate()}>{t('다시 확인')}</button></div></div>}
      {(auth.status === 'ready' || auth.refreshing) && <div key={route.href} className="stack-lg">{content}</div>}
    </div></main>
  </div>;
}
