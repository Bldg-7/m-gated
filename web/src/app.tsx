import { useEffect, useState, useSyncExternalStore, useRef, type FormEvent } from 'react';
import { flushSync } from 'react-dom';
import { ApiError, session, StaleResponse } from './lib/api';
import type { Usage } from './lib/types';
import { UsageView } from './features/usage/usage';
import { KeysScreen } from './features/keys/keys';
import { PrincipalsScreen } from './features/admin/principals';
import { RequestsScreen } from './features/admin/requests';
export function UsageScreen({ admin = false }: { admin?: boolean }) {
  const auth = useSyncExternalStore(session.subscribe, session.snapshot);
  const [usage, setUsage] = useState<Usage | null>(null), [error, setError] = useState<string | null>(null), [loading, setLoading] = useState(true);
  const [query, setQuery] = useState(() => `group_by=provider${admin && new URLSearchParams(location.search).get('principal_id') ? `&principal_id=${encodeURIComponent(new URLSearchParams(location.search).get('principal_id')!)}` : ''}`);
  const [filters, setFilters] = useState(() => `group_by=provider${admin && new URLSearchParams(location.search).get('principal_id') ? `&principal_id=${encodeURIComponent(new URLSearchParams(location.search).get('principal_id')!)}` : ''}`);
  const [revision, setRevision] = useState(0);
  useEffect(() => {
    let current = true; setUsage(null); setError(null); setLoading(true);
    // 같은 주체의 refresh 동안 App이 이 화면을 유지하며 Session만 읽기를 재시도한다.
    // 그 밖의 인증 변경은 App의 unmount/key 변경으로 이전 결과를 폐기한다.
    void session.read<Usage>(`/gateway/${admin ? 'admin/' : ''}usage?${query}`).then(data => { if (current) { setUsage(data); setLoading(false); } }).catch(e => { if (!current || e instanceof StaleResponse) return; setUsage(null); setLoading(false); setError(e instanceof ApiError && e.status === 409 ? `상세 사용량을 제공할 수 없습니다. 상세 가용 시작: ${String(e.details.detail_available_from ?? '미확인')}` : e instanceof ApiError && e.status === 403 ? '현재 권한으로 조회할 수 없습니다.' : '사용량을 가져올 수 없습니다. 다시 조회하세요.'); });
    return () => { current = false; };
  }, [query, revision]);
  const submit = (event: FormEvent<HTMLFormElement>) => { event.preventDefault(); const data = new FormData(event.currentTarget); const q = new URLSearchParams(); const from = String(data.get('from') ?? ''); const to = String(data.get('to') ?? ''); if (Boolean(from) !== Boolean(to)) { setUsage(null); setError('시작과 종료를 모두 지정하세요.'); return; } if (from && to) { q.set('from', new Date(from).toISOString()); q.set('to', new Date(to).toISOString()); } for (const name of [...(admin ? ['principal_id'] : []), 'provider', 'model', 'key_id', 'group_by']) { const value = String(data.get(name) ?? ''); if (value) q.set(name, value); } setFilters(q.toString()); setQuery(q.toString()); setRevision(v => v + 1); };
  if (auth.status !== 'ready') return null;
  const selected = new URLSearchParams(filters);
  const localTime = (value: string | null) => value ? new Date(new Date(value).getTime() - new Date(value).getTimezoneOffset() * 60000).toISOString().slice(0, 16) : '';
  return <><form onSubmit={submit} key={auth.epoch} aria-label="사용량 조회"><label>시작 (현지 시각)<input type="datetime-local" name="from" defaultValue={localTime(selected.get('from'))} /></label><label>종료 (현지 시각)<input type="datetime-local" name="to" defaultValue={localTime(selected.get('to'))} /></label><label htmlFor="usage-provider">Provider</label><select id="usage-provider" name="provider" defaultValue={selected.get('provider') ?? ''}><option value="">전체</option><option value="anthropic">Anthropic</option><option value="openai">OpenAI</option><option value="deepseek">DeepSeek</option></select><label>모델<input name="model" maxLength={200} defaultValue={selected.get('model') ?? ''} /></label>{admin && <label>주체 ID<input name="principal_id" defaultValue={selected.get('principal_id') ?? ''} /></label>}<label>키 ID<input name="key_id" defaultValue={selected.get('key_id') ?? ''} /></label><label htmlFor="usage-group">그룹</label><select id="usage-group" name="group_by" defaultValue={selected.get('group_by') ?? ''}><option value="">없음</option><option value="provider">Provider</option><option value="model">모델</option><option value="key">키</option></select><button disabled={loading} type="submit">조회</button></form>
    {loading && <p role="status">사용량 조회 중…</p>}{error && <p role="alert">{error}</p>}{usage && <><UsageView usage={usage} />{usage.buckets.next_cursor && <button onClick={() => setQuery(`cursor=${encodeURIComponent(usage.buckets.next_cursor!)}`)}>다음 그룹 페이지</button>}</>}
  </>;
}
export function App() {
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
  return <><header><h1>m-gated</h1><nav aria-label="기본"><a href="/ui/">내 사용량</a> <a href="/ui/keys">내 키</a>{auth.me?.role === 'admin' && <> <a href="/ui/admin/principals">주체·정책·키 관리</a> <a href="/ui/admin/usage">전체 사용량</a> <a href="/ui/admin/requests">미정산·처리 내역</a></>}</nav></header><main>
    {auth.status === 'checking' ? <p role="status">인증 확인 중…</p> : auth.status === 'error' ? <><p role="alert">{auth.error}</p><a href="/auth/login">SSO 로그인</a><button onClick={() => void session.validate()}>다시 확인</button></> : auth.me && <><p>{auth.me.principal.display_name} ({auth.me.principal.email}) · {auth.me.role === 'admin' ? '관리자' : '사용자'}</p><button onClick={() => session.logout()}>로그아웃</button></>}{(auth.status === 'ready' || auth.refreshing) && (location.pathname.startsWith('/ui/admin/') ? (auth.me?.role !== 'admin' ? <p role="alert">현재 관리자 권한이 없습니다.</p> : location.pathname === '/ui/admin/principals' ? <PrincipalsScreen key={`${principal.current}:${auth.epoch}`} /> : location.pathname === '/ui/admin/requests' ? <RequestsScreen key={`${principal.current}:${auth.epoch}`} /> : <UsageScreen admin key={principal.current} />) : location.pathname === '/ui/keys' ? <KeysScreen key={principal.current} /> : <UsageScreen key={principal.current} />)}
  </main></>;
}
