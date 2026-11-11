import { useId, useEffect, useLayoutEffect, useRef, useState, useSyncExternalStore, type FormEvent } from 'react';
import { ApiError, session, StaleResponse } from '../../lib/api';
import type { Grant, IssuedKey, KeyMetadata, Limits, Page, Validity } from '../../lib/types';
export const durations: [Validity, string][] = [['3_months', '3개월'], ['6_months', '6개월'], ['1_year', '1년'], ['3_years', '3년'], ['unlimited', '무제한 (만료 없음, 회수·정지는 적용)']];
type IssueInput = { name: string; validity: Validity; grants: Grant[]; limits: Limits };
type Confirmation = { kind: 'issue'; input: IssueInput } | { kind: 'revoke'; key: KeyMetadata };
type Recovery = { commandId: string; kind: 'issue' | 'revoke'; keyId?: string };
const durationName = (value: Validity) => durations.find(([key]) => key === value)?.[1] ?? value;
export function KeysScreen({ adminPrincipal, adminAll = false }: { adminPrincipal?: { id: string; service: boolean; grants: Grant[] }; adminAll?: boolean } = {}) {
  const prefix = useId(), isAdmin = !!adminPrincipal || adminAll;
  const listUrl = adminPrincipal ? `/gateway/admin/keys?principal_id=${encodeURIComponent(adminPrincipal.id)}` : adminAll ? '/gateway/admin/keys' : '/gateway/keys';
  const availableGrants = adminPrincipal?.grants;
  const auth = useSyncExternalStore(session.subscribe, session.snapshot);
  const [page, setPage] = useState<Page<KeyMetadata> | null>(null);
  const [query, setQuery] = useState(''), [status, setStatus] = useState(''), [revision, setRevision] = useState(0);
  const [loading, setLoading] = useState(true), [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null), [listError, setListError] = useState<string | null>(null), [notice, setNotice] = useState<string | null>(null);
  const [secret, setSecret] = useState<{ value: string; epoch: number } | null>(null);
  const [confirmation, setConfirmation] = useState<Confirmation | null>(null);
  const [recovery, setRecovery] = useState<Recovery | null>(null);
  const mounted = useRef(true), lock = useRef(false);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  // 인증 epoch마다 폼/원문/명령 상태를 폐기한다. 같은 주체 refresh도 예외가 아니다.
  useLayoutEffect(() => { setSecret(null); setConfirmation(null); setRecovery(null); setPage(null); setError(null); setListError(null); setNotice(null); setBusy(false); lock.current = false; }, [auth.epoch]);
  useEffect(() => {
    if (auth.status !== 'ready') return;
    let current = true;
    setPage(null); setListError(null); setLoading(true);
    void session.read<Page<KeyMetadata>>(`${listUrl}${query ? `${adminPrincipal ? '&' : '?'}${query}` : ''}`, () => current).then(data => {
      if (current) {
        setPage(data); setLoading(false);
        // 이 GET을 시작한 epoch·회수 명령의 정확한 서버 종결 상태만 인정한다.
        if (session.snapshot().epoch === auth.epoch && recovery?.kind === 'revoke' && data.items.some(key => key.id === recovery.keyId && key.revoked_at !== null)) {
          setRecovery(pending => pending === recovery ? null : pending);
          setError(null); setNotice('회수가 서버 조회로 확인되었습니다. 새 키는 발급 내용을 다시 확인한 뒤 발급하세요.');
        }
      }
    }).catch(e => { if (current && !(e instanceof StaleResponse)) { setLoading(false); setListError('키 목록을 가져올 수 없습니다. 다시 조회하세요.'); } });
    return () => { current = false; };
    // 같은 주체 refresh의 재시도는 Session 하나만 소유한다. 다른 인증 변경은 App이 unmount한다.
  }, [query, revision]);
  const reload = () => { setQuery(''); setStatus(''); setRevision(v => v + 1); };
  const prepare = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault(); setError(null); setSecret(null); setNotice(null);
    const data = new FormData(event.currentTarget), name = String(data.get('name') ?? '').trim(), validity = String(data.get('validity') ?? '');
    const selected = data.getAll('grant').map(String);
    const grants = (availableGrants ?? auth.me?.grants ?? []).flatMap((grant, gi) => {
      const capabilities = grant.capabilities.filter((_, ci) => selected.includes(`${gi}:${ci}`));
      return capabilities.length ? [{ ...grant, capabilities }] : [];
    });
    if (!name || !durations.some(([value]) => value === validity)) { setError('이름과 유효기간을 명시하세요.'); return; }
    const limits: Limits = { budget: null, rpm: null, tpm: null, count_rpm: null };
    for (const field of ['rpm', 'tpm', 'count_rpm'] as const) {
      const raw = String(data.get(field) ?? '');
      if (raw && (!/^[1-9][0-9]*$/.test(raw) || Number(raw) > 1000000000)) { setError('요청·토큰 한도는 1~1000000000의 정수입니다.'); return; }
      limits[field] = raw ? Number(raw) : null;
    }
    const amount = String(data.get('budget') ?? '');
    if (amount) {
      if (!/^(0|[1-9][0-9]{0,15})\.[0-9]{12}$/.test(amount)) { setError('예산은 소수점 12자리 USD 문자열로 입력하세요.'); return; }
      limits.budget = { mode: data.get('mode') === 'reserved' ? 'reserved' : 'actual', limit_usd: amount, period: { kind: 'calendar_month', timezone: String(data.get('timezone') ?? '') } };
    }
    setConfirmation({ kind: 'issue', input: { name, validity: validity as Validity, grants, limits } });
  };
  const execute = async () => {
    if (!confirmation || lock.current) return;
    lock.current = true; setBusy(true); setError(null); setSecret(null); setNotice(null);
    const epoch = auth.epoch, commandId = crypto.randomUUID(), action = confirmation;
    const current = () => mounted.current && session.snapshot().epoch === epoch && session.snapshot().status === 'ready';
    setRecovery({ commandId, kind: action.kind, ...(action.kind === 'revoke' ? { keyId: action.key.id } : {}) });
    setConfirmation(null);
    try {
      if (action.kind === 'issue') {
        const issued = await session.write<IssuedKey>('/gateway/keys', 'POST', commandId, { ...action.input, ...(adminPrincipal ? { service_principal_id: adminPrincipal.id } : {}) });
        if (!current()) return;
        if (!issued) throw new Error('invalid_issue_response');
        setSecret({ value: issued.key, epoch });
        setNotice('발급되었습니다. 원문은 지금 한 번만 표시합니다. 닫은 뒤 다시 볼 수 없습니다.');
      } else {
        await session.write(`/gateway/keys/${action.key.id}?expected_version=${encodeURIComponent(action.key.version)}`, 'DELETE', commandId);
        if (!current()) return;
        setNotice('회수되었습니다. 서버 상태를 다시 조회합니다.');
      }
      setRecovery(null); reload();
    } catch (e) {
      if (!current() || e instanceof StaleResponse) return;
      if (e instanceof ApiError && e.code === 'version_conflict') {
        setRecovery(null); setError('버전이 변경되었습니다. 다시 조회한 상태를 확인하고 회수를 새로 선택하세요.'); reload();
      } else if (e instanceof ApiError && ['invalid_request', 'csrf_invalid', 'origin_denied', 'unsupported_media_type', 'access_denied', 'key_quota_exceeded', 'resource_not_found'].includes(e.code)) {
        setRecovery(null); setError('요청이 거부되었습니다. 현재 권한·입력·키 개수를 확인하고 다시 내용을 확인하세요. 쓰기는 재전송하지 않았습니다.'); reload();
      } else {
        setError(e instanceof ApiError && e.code === 'key_secret_unavailable' ? '발급된 키 원문을 복구할 수 없습니다. 아래 명령과 목록을 연결해 회수 후 새로 발급하세요.' : '성공 여부를 단정할 수 없습니다. 쓰기를 재전송하지 않았습니다. 명령 결과를 목록에서 확인하세요.');
        reload();
      }
    } finally { if (current()) { lock.current = false; setBusy(false); } }
  };
  if (auth.status !== 'ready') return null;
  const resolved = recovery && page?.items.find(key => recovery.kind === 'issue' ? key.created_command_id === recovery.commandId : key.id === recovery.keyId);
  return <section aria-labelledby={`${prefix}-keys-heading`}><h2 id={`${prefix}-keys-heading`}>{isAdmin ? '주체 키' : '내 키'}</h2>
    <p>{isAdmin ? '가상 키는' : '개인 키는'} 추론 전용입니다. 권한·한도는 현재 서버 정책과 발급 상한의 교집합이며 원문은 다시 조회할 수 없습니다.</p>
    {error && <p role="alert">{error}</p>}{listError && <p role="alert">{listError}</p>}{notice && <p role="status">{notice}</p>}
    {secret?.epoch === auth.epoch && <section aria-label="발급 원문"><p>안전한 곳에 직접 옮긴 뒤 닫으세요. 자동 복사·저장은 하지 않습니다.</p><code>{secret.value}</code><button autoFocus onClick={() => setSecret(null)}>원문 닫기</button></section>}
    {recovery && <section aria-label="명령 결과 확인"><h3>명령 결과 확인</h3><p>명령 ID: <code>{recovery.commandId}</code></p><p>자동 재발급·재전송하지 않습니다. 목록은 페이지 단위이므로 다음 페이지도 확인하세요.</p>{resolved ? <p>연결된 키: {resolved.id} · {resolved.revoked_at ? '회수됨' : '원문 복구 불가, 회수 후 명시적으로 새 발급'}</p> : <p>현재 페이지에서 결과를 찾지 못했습니다. 실패로 단정하거나 다시 발급하지 마세요.</p>}<button disabled={loading || busy} onClick={reload}>명령 결과 다시 조회</button></section>}
    {(!adminAll && (!adminPrincipal || adminPrincipal.service)) && <form key={auth.epoch} aria-label={adminPrincipal ? '서비스 키 발급' : '개인 키 발급'} onSubmit={prepare}>
      <fieldset disabled={busy || !!recovery}><legend>{adminPrincipal ? '서비스 키 발급' : '개인 키 발급'}</legend>
        <label>키 이름<input name="name" maxLength={100} required /></label>
        <label htmlFor={`${prefix}-key-validity`}>유효기간</label><select id={`${prefix}-key-validity`} name="validity" defaultValue="" required><option value="" disabled>선택하세요</option>{durations.map(([value, label]) => <option key={value} value={value}>{label}</option>)}</select>
        <fieldset><legend>허용할 API (미선택 시 허용 API 없음)</legend>{(availableGrants ?? auth.me?.grants ?? []).flatMap((g, gi) => g.capabilities.map((cap, ci) => <label key={`${gi}:${ci}`}><input type="checkbox" name="grant" value={`${gi}:${ci}`} />{g.provider} / {g.model} / {cap}</label>))}</fieldset>
        <p>빈 한도는 서버의 실행 상한으로 정규화됩니다. 현재 정책을 우회하지 않습니다.</p>
        <label>예산 USD (소수점 12자리)<input name="budget" placeholder="0.000000000000" /></label><label htmlFor={`${prefix}-key-budget-mode`}>예산 모드</label><select id={`${prefix}-key-budget-mode`} name="mode" defaultValue="actual"><option value="actual">실사용량 (하드캡 아님)</option><option value="reserved">예약 기반</option></select><label>예산 timezone (IANA)<input name="timezone" defaultValue="UTC" /></label>
        <label>생성 RPM<input name="rpm" inputMode="numeric" /></label><label>생성 TPM<input name="tpm" inputMode="numeric" /></label><label>Count RPM<input name="count_rpm" inputMode="numeric" /></label>
        <button type="submit">발급 내용 확인</button>
      </fieldset>
    </form>}
    {confirmation && <section role="region" aria-label="쓰기 확인"><h3>{confirmation.kind === 'issue' ? '발급 확인' : '회수 확인'}</h3>{confirmation.kind === 'issue' ? <><p>{confirmation.input.name} · {durationName(confirmation.input.validity)}</p><pre>{JSON.stringify({ grants: confirmation.input.grants, limits: confirmation.input.limits }, null, 2)}</pre><p>만료일과 실효 상한은 서버가 계산합니다.</p></> : <p>{confirmation.key.name} · {confirmation.key.id} · version {confirmation.key.version} 키를 회수합니다. 되돌릴 수 없습니다.</p>}<button autoFocus disabled={busy} onClick={() => void execute()}>{confirmation.kind === 'issue' ? '발급 확인' : '회수 확인'}</button><button onClick={() => setConfirmation(null)}>취소</button></section>}
    <form aria-label="키 조회" onSubmit={e => { e.preventDefault(); setQuery(status ? `status=${status}` : ''); setRevision(v => v + 1); }}><label htmlFor={`${prefix}-key-status`}>키 상태</label><select id={`${prefix}-key-status`} value={status} onChange={e => setStatus(e.target.value)}><option value="">전체</option><option value="active">활성</option><option value="revoked">회수</option><option value="expired">만료</option></select><button disabled={loading || busy}>키 조회</button></form>
    {loading && <p role="status">키 목록 조회 중…</p>}{page && <><p>목록 관측 시각: {page.as_of}</p>{!page.items.length && <p>키가 없습니다.</p>}<ul>{page.items.map(key => <li key={key.id}><h3>{key.name}</h3><p>ID: {key.id} · {key.key_hint} · version {key.version}</p><p>{durationName(key.validity)} · 생성: {key.created_at} · 만료: {key.expires_at ?? '없음'} · 회수: {key.revoked_at ?? '아직 회수되지 않음'}</p><p>발급 명령: {key.created_command_id}</p><details><summary>발급 권한·고정 상한</summary><pre>{JSON.stringify({ grants: key.grants, limits: key.limits }, null, 2)}</pre></details>{!key.revoked_at && <button disabled={busy || loading} onClick={() => { setSecret(null); setConfirmation({ kind: 'revoke', key }); }}>{key.name} 회수</button>}</li>)}</ul>{page.next_cursor && <button disabled={loading || busy} onClick={() => setQuery(`cursor=${encodeURIComponent(page.next_cursor!)}`)}>다음 키 페이지</button>}</>}
  </section>;
}
