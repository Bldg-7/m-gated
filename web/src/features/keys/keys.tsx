import { useId, useEffect, useLayoutEffect, useRef, useState, useSyncExternalStore, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { ApiError, session, StaleResponse } from '../../lib/api';
import i18n, { msgid } from '../../lib/i18n';
import type { Grant, IssuedKey, KeyCredential, KeyMetadata, Limits, Page, Validity } from '../../lib/types';
import type { CredentialList, CredentialMeta } from '../admin/types';
import { credentialLabel, providerNames } from '../admin/credentials';
import { Badge, Select } from '../../ui/ui';

/** 발급 원문과 복사 버튼. 사용자가 누를 때만 클립보드에 쓰고, 원문 영역이 닫히면 상태도 함께 사라진다. */
function SecretValue({ value }: { value: string }) {
  const { t } = useTranslation();
  const [copy, setCopy] = useState<'idle' | 'done' | 'failed'>('idle');
  const write = async () => {
    try { await navigator.clipboard.writeText(value); setCopy('done'); } catch { setCopy('failed'); }
  };
  return <>
    <div className="secret-value"><code>{value}</code><button type="button" className="secret-copy" aria-label={t('원문 복사')} title={t('원문 복사')} onClick={() => void write()}>
      <svg className="icon" width="18" height="18" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">{copy === 'done' ? <path d="M3.5 8.5l3 3 6-6.5" /> : <><rect x="5.5" y="5.5" width="8" height="8" rx="1.5" /><path d="M10.5 5.5V4A1.5 1.5 0 0 0 9 2.5H4A1.5 1.5 0 0 0 2.5 4v5A1.5 1.5 0 0 0 4 10.5h1.5" /></>}</svg>
    </button></div>
    {copy !== 'idle' && <p role="status" className="plain meta">{copy === 'done' ? t('클립보드에 복사했습니다.') : t('복사하지 못했습니다. 원문을 직접 선택해 복사하세요.')}</p>}
  </>;
}
export const durations: [Validity, string][] = [['3_months', msgid('3개월')], ['6_months', msgid('6개월')], ['1_year', msgid('1년')], ['3_years', msgid('3년')], ['unlimited', msgid('무제한 (만료 없음, 회수·정지는 적용)')]];
type IssueInput = { name: string; validity: Validity; grants: Grant[]; limits: Limits; provider_credentials?: Record<string, string> };
type Confirmation = { kind: 'issue'; input: IssueInput } | { kind: 'revoke'; key: KeyMetadata };
type Recovery = { commandId: string; kind: 'issue' | 'revoke'; keyId?: string };
const durationName = (value: Validity) => durations.find(([key]) => key === value)?.[1] ?? value;
export function KeysScreen({ adminPrincipal, adminAll = false }: { adminPrincipal?: { id: string; service: boolean; grants: Grant[] }; adminAll?: boolean } = {}) {
  const { t } = useTranslation();
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
  // 관리자만 회사 키 목록을 읽어 발급 때 고르거나 연결을 바꾼다. 일반 사용자 키는 기본 키에 연결된다.
  const [companyKeys, setCompanyKeys] = useState<CredentialList | null>(null), [rebinding, setRebinding] = useState<string | null>(null);
  const canChoose = auth.me?.role === 'admin';
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
          setError(null); setNotice(i18n.t('회수가 서버 조회로 확인되었습니다. 새 키는 발급 내용을 다시 확인한 뒤 발급하세요.'));
        }
      }
    }).catch(e => { if (current && !(e instanceof StaleResponse)) { setLoading(false); setListError(i18n.t('키 목록을 가져올 수 없습니다. 다시 조회하세요.')); } });
    return () => { current = false; };
    // 같은 주체 refresh의 재시도는 Session 하나만 소유한다. 다른 인증 변경은 App이 unmount한다.
  }, [query, revision]);
  useEffect(() => {
    if (auth.status !== 'ready' || !canChoose) return;
    let current = true;
    void session.read<CredentialList>('/gateway/admin/provider-credentials', () => current).then(data => { if (current) setCompanyKeys(data); }).catch(() => { /* 고를 수 없으면 기본 키로 발급된다 */ });
    return () => { current = false; };
  }, [canChoose, revision]);
  const choices = (companyKeys?.providers ?? []).filter(p => p.credentials.length > 1);
  const credentialsOf = (provider: string): CredentialMeta[] => companyKeys?.providers.find(p => p.provider === provider)?.credentials ?? [];
  const reload = () => { setQuery(''); setStatus(''); setRevision(v => v + 1); };
  const prepare = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault(); setError(null); setSecret(null); setNotice(null);
    const data = new FormData(event.currentTarget), name = String(data.get('name') ?? '').trim(), validity = String(data.get('validity') ?? '');
    const selected = data.getAll('grant').map(String);
    const grants = (availableGrants ?? auth.me?.grants ?? []).flatMap((grant, gi) => {
      const capabilities = grant.capabilities.filter((_, ci) => selected.includes(`${gi}:${ci}`));
      return capabilities.length ? [{ ...grant, capabilities }] : [];
    });
    if (!name || !durations.some(([value]) => value === validity)) { setError(i18n.t('이름과 유효기간을 명시하세요.')); return; }
    const limits: Limits = { budget: null, rpm: null, tpm: null, count_rpm: null };
    for (const field of ['rpm', 'tpm', 'count_rpm'] as const) {
      const raw = String(data.get(field) ?? '');
      if (raw && (!/^[1-9][0-9]*$/.test(raw) || Number(raw) > 1000000000)) { setError(i18n.t('요청·토큰 한도는 1~1000000000의 정수입니다.')); return; }
      limits[field] = raw ? Number(raw) : null;
    }
    const amount = String(data.get('budget') ?? '');
    if (amount) {
      if (!/^(0|[1-9][0-9]{0,15})\.[0-9]{12}$/.test(amount)) { setError(i18n.t('예산은 소수점 12자리 USD 문자열로 입력하세요.')); return; }
      limits.budget = { mode: data.get('mode') === 'reserved' ? 'reserved' : 'actual', limit_usd: amount, period: { kind: 'calendar_month', timezone: String(data.get('timezone') ?? '') } };
    }
    const chosen = Object.fromEntries(choices.map(p => [p.provider, String(data.get(`credential:${p.provider}`) ?? '')]).filter(([, id]) => id));
    setConfirmation({ kind: 'issue', input: { name, validity: validity as Validity, grants, limits, ...(Object.keys(chosen).length ? { provider_credentials: chosen } : {}) } });
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
        setNotice(i18n.t('발급되었습니다. 원문은 지금 한 번만 표시합니다. 닫은 뒤 다시 볼 수 없습니다.'));
      } else {
        await session.write(`/gateway/keys/${action.key.id}?expected_version=${encodeURIComponent(action.key.version)}`, 'DELETE', commandId);
        if (!current()) return;
        setNotice(i18n.t('회수되었습니다. 서버 상태를 다시 조회합니다.'));
      }
      setRecovery(null); reload();
    } catch (e) {
      if (!current() || e instanceof StaleResponse) return;
      if (e instanceof ApiError && e.code === 'version_conflict') {
        setRecovery(null); setError(i18n.t('버전이 변경되었습니다. 다시 조회한 상태를 확인하고 회수를 새로 선택하세요.')); reload();
      } else if (e instanceof ApiError && ['invalid_request', 'csrf_invalid', 'origin_denied', 'unsupported_media_type', 'access_denied', 'key_quota_exceeded', 'resource_not_found'].includes(e.code)) {
        setRecovery(null); setError(i18n.t('요청이 거부되었습니다. 현재 권한·입력·키 개수를 확인하고 다시 내용을 확인하세요. 쓰기는 재전송하지 않았습니다.')); reload();
      } else {
        setError(e instanceof ApiError && e.code === 'key_secret_unavailable' ? i18n.t('발급된 키 원문을 복구할 수 없습니다. 아래 명령과 목록을 연결해 회수 후 새로 발급하세요.') : i18n.t('성공 여부를 단정할 수 없습니다. 쓰기를 재전송하지 않았습니다. 명령 결과를 목록에서 확인하세요.'));
        reload();
      }
    } finally { if (current()) { lock.current = false; setBusy(false); } }
  };
  /** 관리자가 가상 키 하나의 회사 키 연결을 바꾼다. 지금 연결을 함께 보내 다른 관리자의 변경을 덮어쓰지 않는다. */
  const rebind = async (key: KeyMetadata, binding: KeyCredential, to: string) => {
    if (lock.current || to === binding.credential_id) { setRebinding(null); return; }
    lock.current = true; setBusy(true); setError(null); setNotice(null);
    const epoch = auth.epoch, commandId = crypto.randomUUID();
    try {
      await session.write(`/gateway/admin/keys/${key.id}/provider-credential`, 'PUT', commandId, { provider: binding.provider, credential_id: to, expected_credential_id: binding.credential_id });
      if (session.snapshot().epoch !== epoch) return;
      const target = credentialsOf(binding.provider).find(c => c.id === to);
      setNotice(i18n.t('{{name}} 키의 회사 키를 {{label}}로 바꿨습니다.', { name: key.name, label: target ? credentialLabel(target) : to }));
    } catch (e) {
      if (e instanceof StaleResponse) return;
      setError(i18n.t('회사 키 연결을 바꾸지 못했습니다. 목록을 다시 조회했습니다. 명령 ID: {{id}}', { id: commandId }));
    } finally { lock.current = false; setBusy(false); setRebinding(null); reload(); }
  };
  if (auth.status !== 'ready') return null;
  const resolved = recovery && page?.items.find(key => recovery.kind === 'issue' ? key.created_command_id === recovery.commandId : key.id === recovery.keyId);
  return <section aria-labelledby={`${prefix}-keys-heading`} className="stack-lg">
    <div className="stack-sm"><h2 id={`${prefix}-keys-heading`}>{isAdmin ? t('주체 키') : t('내 키')}</h2>
    <p className="lead">{isAdmin ? t('가상 키는 추론 전용입니다. 권한·한도는 현재 서버 정책과 발급 상한의 교집합이며 원문은 다시 조회할 수 없습니다.') : t('개인 키는 추론 전용입니다. 권한·한도는 현재 서버 정책과 발급 상한의 교집합이며 원문은 다시 조회할 수 없습니다.')}</p></div>
    {error && <p role="alert">{error}</p>}{listError && <p role="alert">{listError}</p>}{notice && <p role="status">{notice}</p>}
    {secret?.epoch === auth.epoch && <section aria-label={t('발급 원문')} className="secret"><p className="text-warning">{t('안전한 곳에 옮긴 뒤 닫으세요. 자동으로 복사·저장하지 않으며, 복사 버튼을 누를 때만 클립보드에 넣습니다.')}</p><SecretValue value={secret.value} /><div className="actions"><button autoFocus onClick={() => setSecret(null)}>{t('원문 닫기')}</button></div></section>}
    {recovery && <section aria-label={t('명령 결과 확인')} className="panel-warning"><h3>{t('명령 결과 확인')}</h3><p>{t('명령 ID')}: <code>{recovery.commandId}</code></p><p className="meta">{t('자동 재발급·재전송하지 않습니다. 목록은 페이지 단위이므로 다음 페이지도 확인하세요.')}</p>{resolved ? <p>{t('연결된 키: {{id}} · {{state}}', { id: resolved.id, state: resolved.revoked_at ? t('회수됨') : t('원문 복구 불가, 회수 후 명시적으로 새 발급') })}</p> : <p>{t('현재 페이지에서 결과를 찾지 못했습니다. 실패로 단정하거나 다시 발급하지 마세요.')}</p>}<div className="actions"><button className="btn-sm" disabled={loading || busy} onClick={reload}>{t('명령 결과 다시 조회')}</button></div></section>}
    {(!adminAll && (!adminPrincipal || adminPrincipal.service)) && <form key={auth.epoch} aria-label={adminPrincipal ? t('서비스 키 발급') : t('개인 키 발급')} onSubmit={prepare}>
      <fieldset disabled={busy || !!recovery} className="group"><legend>{adminPrincipal ? t('서비스 키 발급') : t('개인 키 발급')}</legend>
        <div className="form-grid">
          <label>{t('키 이름')}<input name="name" maxLength={100} required /></label>
          <div className="field"><label htmlFor={`${prefix}-key-validity`}>{t('유효기간')}</label><Select id={`${prefix}-key-validity`} name="validity" defaultValue="" required><option value="" disabled>{t('선택하세요')}</option>{durations.map(([value, label]) => <option key={value} value={value}>{t(label)}</option>)}</Select></div>
        </div>
        <fieldset className="stack-sm"><legend>{t('허용할 API (미선택 시 허용 API 없음)')}</legend><div className="checks">{(availableGrants ?? auth.me?.grants ?? []).flatMap((g, gi) => g.capabilities.map((cap, ci) => <label key={`${gi}:${ci}`}><input type="checkbox" name="grant" value={`${gi}:${ci}`} />{g.provider} / {g.model} / {cap}</label>))}</div></fieldset>
        {choices.length > 0 && <div className="form-grid">{choices.map(p => <div key={p.provider} className="field"><label htmlFor={`${prefix}-credential-${p.provider}`}>{t('{{provider}} 회사 키', { provider: providerNames[p.provider] ?? p.provider })}</label><Select id={`${prefix}-credential-${p.provider}`} name={`credential:${p.provider}`} defaultValue={p.credentials.find(c => c.default)?.id ?? ''}>{p.credentials.map(c => <option key={c.id} value={c.id}>{credentialLabel(c)}{c.default ? ` · ${t('기본 키')}` : ''}</option>)}</Select></div>)}</div>}
        <p className="meta">{t('빈 한도는 서버의 실행 상한으로 정규화됩니다. 현재 정책을 우회하지 않습니다.')}</p>
        <div className="form-grid">
          <label>{t('예산 USD (소수점 12자리)')}<input name="budget" className="amount" inputMode="decimal" spellCheck={false} placeholder="0.000000000000" /></label>
          <div className="field"><label htmlFor={`${prefix}-key-budget-mode`}>{t('예산 모드')}</label><Select id={`${prefix}-key-budget-mode`} name="mode" defaultValue="actual"><option value="actual">{t('실사용량 (하드캡 아님)')}</option><option value="reserved">{t('예약 기반')}</option></Select></div>
          <label>{t('예산 timezone (IANA)')}<input name="timezone" className="mono" spellCheck={false} defaultValue="UTC" /></label>
          <label>{t('생성 RPM')}<input name="rpm" className="mono" inputMode="numeric" /></label><label>{t('생성 TPM')}<input name="tpm" className="mono" inputMode="numeric" /></label><label>Count RPM<input name="count_rpm" className="mono" inputMode="numeric" /></label>
        </div>
        <div className="actions"><button type="submit" className="btn-primary">{t('발급 내용 확인')}</button></div>
      </fieldset>
    </form>}
    {confirmation && <section role="region" aria-label={t('쓰기 확인')} className={confirmation.kind === 'revoke' ? 'confirm confirm-danger' : 'confirm'}><h3>{confirmation.kind === 'issue' ? t('발급 확인') : t('회수 확인')}</h3>{confirmation.kind === 'issue' ? <><p>{confirmation.input.name} · {t(durationName(confirmation.input.validity))}</p><pre>{JSON.stringify({ grants: confirmation.input.grants, limits: confirmation.input.limits }, null, 2)}</pre>{confirmation.input.provider_credentials && <p>{t('회사 키')}: {Object.entries(confirmation.input.provider_credentials).map(([provider, id]) => { const c = credentialsOf(provider).find(x => x.id === id); return `${providerNames[provider] ?? provider} · ${c ? credentialLabel(c) : id}`; }).join(', ')}</p>}<p className="meta">{t('만료일과 실효 상한은 서버가 계산합니다.')}</p></> : <p>{t('{{name}} · {{id}} · version {{version}} 키를 회수합니다. 되돌릴 수 없습니다.', { name: confirmation.key.name, id: confirmation.key.id, version: confirmation.key.version })}</p>}<div className="actions"><button autoFocus className={confirmation.kind === 'revoke' ? 'btn-danger' : 'btn-primary'} disabled={busy} onClick={() => void execute()}>{confirmation.kind === 'issue' ? t('발급 확인') : t('회수 확인')}</button><button className="btn-quiet" onClick={() => setConfirmation(null)}>{t('취소')}</button></div></section>}
    <div className="stack">
      <form aria-label={t('키 조회')} className="filters" onSubmit={e => { e.preventDefault(); setQuery(status ? `status=${status}` : ''); setRevision(v => v + 1); }}><div className="field"><label htmlFor={`${prefix}-key-status`}>{t('키 상태')}</label><Select id={`${prefix}-key-status`} value={status} onChange={e => setStatus(e.target.value)}><option value="">{t('전체')}</option><option value="active">{t('활성')}</option><option value="revoked">{t('회수')}</option><option value="expired">{t('만료')}</option></Select></div><div className="actions"><button disabled={loading || busy}>{t('키 조회')}</button></div></form>
      {loading && <p role="status">{t('키 목록 조회 중…')}</p>}{page && <><p className="meta">{t('목록 관측 시각')}: <span className="mono">{page.as_of}</span></p>{!page.items.length && <p className="empty">{t('키가 없습니다.')}</p>}{page.items.length > 0 && <ul className="list list-block">{page.items.map(key => <li key={key.id}>
        <div className="row row-between"><div className="row"><h3>{key.name}</h3>{key.revoked_at && <Badge tone="neutral">{t('회수됨')}</Badge>}</div>{!key.revoked_at && <button className="btn-sm btn-danger-outline" disabled={busy || loading} onClick={() => { setSecret(null); setConfirmation({ kind: 'revoke', key }); }}>{t('{{name}} 회수', { name: key.name })}</button>}</div>
        <p className="meta mono">{t('ID: {{id}} · {{hint}} · version {{version}}', { id: key.id, hint: key.key_hint, version: key.version })}</p>
        <p className="meta">{t('{{duration}} · 생성: {{created}} · 만료: {{expires}} · 회수: {{revoked}}', { duration: t(durationName(key.validity)), created: key.created_at, expires: key.expires_at ?? t('없음'), revoked: key.revoked_at ?? t('아직 회수되지 않음') })}</p>
        <p className="meta">{t('발급 명령: {{id}}', { id: key.created_command_id })}</p>
        <div className="stack-sm" aria-label={t('{{name}} 회사 키 연결', { name: key.name })}>{(key.provider_credentials ?? []).length === 0 ? <p className="meta">{t('회사 키: 각 Provider의 기본 키')}</p> : (key.provider_credentials ?? []).map(b => <div key={b.provider} className="row">
          <span className="meta">{t('회사 키')}: {providerNames[b.provider] ?? b.provider} · {credentialLabel(b)}{b.state === 'retired' && <> <Badge tone="neutral">{t('해제됨')}</Badge></>}</span>
          {isAdmin && !key.revoked_at && b.state === 'active' && credentialsOf(b.provider).length > 1 && (rebinding === `${key.id}:${b.provider}`
            ? <form className="row" aria-label={t('{{name}} 회사 키 변경', { name: key.name })} onSubmit={e => { e.preventDefault(); void rebind(key, b, String(new FormData(e.currentTarget).get('to') ?? '')); }}><Select name="to" aria-label={t('바꿀 회사 키')} defaultValue={b.credential_id}>{credentialsOf(b.provider).map(c => <option key={c.id} value={c.id}>{credentialLabel(c)}</option>)}</Select><button className="btn-sm btn-primary" disabled={busy}>{t('변경')}</button><button type="button" className="btn-sm btn-quiet" onClick={() => setRebinding(null)}>{t('취소')}</button></form>
            : <button className="btn-sm" disabled={busy || loading} onClick={() => setRebinding(`${key.id}:${b.provider}`)}>{t('회사 키 변경')}</button>)}
        </div>)}</div>
        <details><summary>{t('발급 권한·고정 상한')}</summary><pre>{JSON.stringify({ grants: key.grants, limits: key.limits }, null, 2)}</pre></details>
      </li>)}</ul>}{page.next_cursor && <div className="actions"><button disabled={loading || busy} onClick={() => setQuery(`cursor=${encodeURIComponent(page.next_cursor!)}`)}>{t('다음 키 페이지')}</button></div>}</>}
    </div>
  </section>;
}
