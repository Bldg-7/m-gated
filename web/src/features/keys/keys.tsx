import { useId, useEffect, useLayoutEffect, useRef, useState, useSyncExternalStore, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { ApiError, session, StaleResponse } from '../../lib/api';
import i18n, { msgid } from '../../lib/i18n';
import type { AuthorizedCredentials, Grant, IssuedKey, KeyMetadata, Limits, Page, Validity } from '../../lib/types';
import { credentialLabel, providerNames } from '../admin/credentials';
import { useTimeFormat } from '../../lib/time';
import { Link, navigate } from '../../lib/router';
import { Badge, IconButton, Modal, Pill, SearchSelect, Select, Time } from '../../ui/ui';
import { EditKeyModal } from './keys-edit';

/** 발급된 키와 복사 버튼. 사용자가 누를 때만 클립보드에 쓰고, 키 창이 닫히면 상태도 함께 사라진다. */
function SecretValue({ value }: { value: string }) {
  const { t } = useTranslation();
  const [copy, setCopy] = useState<'idle' | 'done' | 'failed'>('idle');
  const write = async () => {
    try { await navigator.clipboard.writeText(value); setCopy('done'); } catch { setCopy('failed'); }
  };
  return <>
    <div className="secret-value"><code>{value}</code><button type="button" className="secret-copy" aria-label={t('키 복사')} title={t('키 복사')} onClick={() => void write()}>
      <svg className="icon" width="18" height="18" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">{copy === 'done' ? <path d="M3.5 8.5l3 3 6-6.5" /> : <><rect x="5.5" y="5.5" width="8" height="8" rx="1.5" /><path d="M10.5 5.5V4A1.5 1.5 0 0 0 9 2.5H4A1.5 1.5 0 0 0 2.5 4v5A1.5 1.5 0 0 0 4 10.5h1.5" /></>}</svg>
    </button></div>
    {copy !== 'idle' && <p role="status" className="plain meta">{copy === 'done' ? t('클립보드에 복사했습니다.') : t('복사하지 못했습니다. 키를 직접 선택해 복사하세요.')}</p>}
  </>;
}
export const durations: [Validity, string][] = [['3_months', msgid('3개월')], ['6_months', msgid('6개월')], ['1_year', msgid('1년')], ['3_years', msgid('3년')], ['unlimited', msgid('무제한 (만료 없음, 회수·정지는 적용)')]];
type IssueInput = { name: string; validity: Validity; grants: Grant[]; limits: Limits; provider_credentials: Record<string, string> };
type Confirmation = { kind: 'issue'; input: IssueInput } | { kind: 'revoke'; key: KeyMetadata };
type Recovery = { commandId: string; kind: 'issue' | 'revoke'; keyId?: string };
const durationName = (value: Validity) => durations.find(([key]) => key === value)?.[1] ?? value;
/** 고를 수 있는 Provider 키 하나. */
type ProviderKey = { id: string; label: string; hint: string };
/** 예산 기간의 기준 시간대 안내. 시간대는 관리자 설정 하나로 정하고 키·정책마다 따로 두지 않는다. */
export function BudgetTimezoneNote() {
  const { t } = useTranslation();
  const tz = useSyncExternalStore(session.subscribe, session.snapshot).me?.budget_timezone;
  const format = useTimeFormat();
  if (!tz) return null;
  return <p className="meta">{t('월 예산 기준 시간대: {{timezone}} (관리자 설정). 이번 기간: {{start}} ~ {{end}}', { timezone: tz.timezone, start: format(tz.period_start), end: format(tz.period_end) })}{tz.pending && <> · {t('{{at}}부터 {{timezone}} 기준으로 바뀝니다.', { at: format(tz.pending.effective_from), timezone: tz.pending.timezone })}</>}</p>;
}
/** 가상 키 발급 하위 페이지와 그 목록 페이지의 주소. 서비스 계정의 가상 키는 관리자 화면 아래에 있다. */
export const keyIssueHref = (principalId?: string) => principalId ? `/dashboard/admin/principals/key?principal_id=${encodeURIComponent(principalId)}` : '/dashboard/keys/new';
const keyListHref = (principalId?: string) => principalId ? `/dashboard/admin/principals/detail?principal_id=${encodeURIComponent(principalId)}` : '/dashboard/keys';
/**
 * view='list'는 가상 키 목록(조회·회수·연결 변경), view='issue'는 발급 하위 페이지다.
 * 발급 페이지도 목록을 읽어 둔다. 발급 결과를 단정할 수 없을 때 명령 ID로 목록에서 확인하기 위해서다.
 */
export function KeysScreen({ adminPrincipal, adminAll = false, view = 'list' }: { adminPrincipal?: { id: string; service: boolean; grants: Grant[]; authorized?: AuthorizedCredentials }; adminAll?: boolean; view?: 'list' | 'issue' } = {}) {
  const { t } = useTranslation();
  const issuing = view === 'issue', canIssue = !adminAll && (!adminPrincipal || adminPrincipal.service);
  const issueHref = keyIssueHref(adminPrincipal?.id), listHref = keyListHref(adminPrincipal?.id);
  const prefix = useId(), isAdmin = !!adminPrincipal || adminAll;
  const listUrl = adminPrincipal ? `/api/admin/keys?principal_id=${encodeURIComponent(adminPrincipal.id)}` : adminAll ? '/api/admin/keys' : '/api/keys';
  const availableGrants = adminPrincipal?.grants;
  const auth = useSyncExternalStore(session.subscribe, session.snapshot);
  const [page, setPage] = useState<Page<KeyMetadata> | null>(null);
  const [query, setQuery] = useState(''), [status, setStatus] = useState(''), [revision, setRevision] = useState(0);
  const [loading, setLoading] = useState(true), [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null), [listError, setListError] = useState<string | null>(null), [notice, setNotice] = useState<string | null>(null);
  const [secret, setSecret] = useState<{ value: string; epoch: number } | null>(null);
  const [confirmation, setConfirmation] = useState<Confirmation | null>(null);
  const [recovery, setRecovery] = useState<Recovery | null>(null);
  // 발급 폼에서 Provider마다 고른 Provider 키와, 그 Provider의 API 가운데 허용한 것.
  const [chosen, setChosen] = useState<Record<string, string>>({});
  const [granted, setGranted] = useState<Set<string>>(() => new Set());
  // 수정 창을 연 가상 키.
  const [editing, setEditing] = useState<KeyMetadata | null>(null), [detail, setDetail] = useState<KeyMetadata | null>(null);
  const mounted = useRef(true), lock = useRef(false);
  const format = useTimeFormat();
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  // 인증 epoch마다 폼/키/명령 상태를 폐기한다. 같은 사용자 refresh도 예외가 아니다.
  useLayoutEffect(() => { setSecret(null); setConfirmation(null); setRecovery(null); setPage(null); setError(null); setListError(null); setNotice(null); setBusy(false); setChosen({}); setGranted(new Set()); setEditing(null); lock.current = false; }, [auth.epoch]);
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
    // 같은 사용자 refresh의 재시도는 Session 하나만 소유한다. 다른 인증 변경은 App이 unmount한다.
  }, [query, revision]);
  // 키 주인에게 인가된 Provider 키. 서비스 계정의 가상 키는 그 서비스 계정의 인가, 내 가상 키는 내 인가다.
  // 발급할 때는 관리자도 전체 Provider 키가 아니라 키 주인에게 인가된 키 안에서만 고른다.
  const authorized: AuthorizedCredentials = adminPrincipal ? adminPrincipal.authorized ?? [] : auth.me?.provider_credentials ?? [];
  const authorizedFor = (provider: string): ProviderKey[] => authorized.find(a => a.provider === provider)?.credentials ?? [];
  const grantList = availableGrants ?? auth.me?.grants ?? [];
  // 정책이 허용한 API의 Provider. 이 가운데 인가된 Provider 키가 있는 Provider만 가상 키에 연동할 수 있다.
  const policyProviders = [...new Set(grantList.map(grant => grant.provider))];
  const linkable = policyProviders.filter(provider => authorizedFor(provider).length > 0);
  const chosenProviders = linkable.filter(provider => authorizedFor(provider).some(c => c.id === chosen[provider]));
  const grantedProviders = new Set(grantList.flatMap((grant, gi) => grant.capabilities.some((_, ci) => granted.has(`${gi}:${ci}`)) ? [grant.provider] : []));
  // Provider 키를 골랐는데 그 Provider의 API를 하나도 허용하지 않은 Provider.
  const withoutApi = chosenProviders.filter(provider => !grantedProviders.has(provider));
  const choose = (provider: string, id: string) => {
    setChosen(prev => ({ ...prev, [provider]: id }));
    // 연동을 푼 Provider의 API는 허용 목록에서도 뺀다.
    if (!id) setGranted(prev => new Set([...prev].filter(value => grantList[Number(value.split(':')[0])]?.provider !== provider)));
  };
  const reload = () => { setQuery(''); setStatus(''); setRevision(v => v + 1); };
  const prepare = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault(); setError(null); setSecret(null); setNotice(null);
    const data = new FormData(event.currentTarget), name = String(data.get('name') ?? '').trim(), validity = String(data.get('validity') ?? '');
    // Provider 키를 고른 Provider의 API만 보낸다.
    const grants = grantList.flatMap((grant, gi) => {
      const capabilities = chosenProviders.includes(grant.provider) ? grant.capabilities.filter((_, ci) => granted.has(`${gi}:${ci}`)) : [];
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
      limits.budget = { mode: data.get('mode') === 'reserved' ? 'reserved' : 'actual', limit_usd: amount, period: { kind: 'calendar_month' } };
    }
    // 가상 키는 키 주인에게 인가된 Provider 키 하나 이상에 연동해야 하고, 연동한 Provider마다 허용할 API가 있어야 한다.
    if (!chosenProviders.length) { setError(i18n.t('연동할 Provider 키를 하나 이상 고르세요.')); return; }
    if (withoutApi.length) { setError(i18n.t('Provider 키를 고른 Provider마다 허용할 API를 하나 이상 고르세요.')); return; }
    setConfirmation({ kind: 'issue', input: { name, validity: validity as Validity, grants, limits, provider_credentials: Object.fromEntries(chosenProviders.map(provider => [provider, chosen[provider]])) } });
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
        const issued = await session.write<IssuedKey>('/api/keys', 'POST', commandId, { ...action.input, ...(adminPrincipal ? { service_principal_id: adminPrincipal.id } : {}) });
        if (!current()) return;
        if (!issued) throw new Error('invalid_issue_response');
        setSecret({ value: issued.key, epoch });
        setNotice(i18n.t('발급되었습니다. 키는 지금 한 번만 표시합니다. 닫은 뒤 다시 볼 수 없습니다.'));
      } else {
        await session.write(`/api/keys/${action.key.id}?expected_version=${encodeURIComponent(action.key.version)}`, 'DELETE', commandId);
        if (!current()) return;
        setNotice(i18n.t('회수되었습니다. 서버 상태를 다시 조회합니다.'));
      }
      setRecovery(null); reload();
    } catch (e) {
      if (!current() || e instanceof StaleResponse) return;
      if (e instanceof ApiError && e.code === 'version_conflict') {
        setRecovery(null); setError(i18n.t('버전이 변경되었습니다. 다시 조회한 상태를 확인하고 회수를 새로 선택하세요.')); reload();
      } else if (e instanceof ApiError && e.code === 'provider_credential_not_authorized') {
        setRecovery(null); setError(i18n.t('고른 Provider 키를 쓸 수 있는 인가가 없습니다. 쓰기는 재전송하지 않았습니다.')); reload();
      } else if (e instanceof ApiError && e.code === 'provider_credential_required') {
        setRecovery(null); setError(i18n.t('가상 키는 Provider 키를 하나 이상 연동해야 하고, 허용한 API의 Provider마다 Provider 키를 골라야 합니다. 쓰기는 재전송하지 않았습니다.')); reload();
      } else if (e instanceof ApiError && ['invalid_request', 'csrf_invalid', 'origin_denied', 'unsupported_media_type', 'access_denied', 'key_quota_exceeded', 'resource_not_found'].includes(e.code)) {
        setRecovery(null); setError(i18n.t('요청이 거부되었습니다. 현재 권한·입력·키 개수를 확인하고 다시 내용을 확인하세요. 쓰기는 재전송하지 않았습니다.')); reload();
      } else {
        setError(e instanceof ApiError && e.code === 'key_secret_unavailable' ? i18n.t('발급된 키를 다시 받을 수 없습니다. 아래 명령과 목록을 연결해 회수 후 새로 발급하세요.') : i18n.t('성공 여부를 단정할 수 없습니다. 쓰기를 재전송하지 않았습니다. 명령 결과를 목록에서 확인하세요.'));
        reload();
      }
    } finally { if (current()) { lock.current = false; setBusy(false); } }
  };
  if (auth.status !== 'ready') return null;
  const resolved = recovery && page?.items.find(key => recovery.kind === 'issue' ? key.created_command_id === recovery.commandId : key.id === recovery.keyId);
  const issueTitle = adminPrincipal ? t('서비스 계정의 가상 키 발급') : t('가상 키 발급');
  // 발급 페이지에서 키 창을 닫으면 목록 페이지로 돌아간다.
  const closeSecret = () => { setSecret(null); if (issuing) navigate(listHref); };
  const labelOf = (provider: string, id: string) => { const c = authorizedFor(provider).find(x => x.id === id); return c ? credentialLabel(c) : id; };
  // 상세 창은 목록을 다시 읽으면 새 값을 보여 준다.
  const shown = detail && (page?.items.find(k => k.id === detail.id) ?? detail);
  return <section aria-labelledby={`${prefix}-keys-heading`} className="stack-lg">
    {issuing && <p><Link href={listHref}>{adminPrincipal ? t('← 사용자 상세') : t('← 내 가상 키')}</Link></p>}
    <div className="stack-sm"><div className="row row-between"><h2 id={`${prefix}-keys-heading`}>{issuing ? issueTitle : isAdmin ? t('사용자의 가상 키') : t('내 가상 키')}</h2>{!issuing && canIssue && <Link className="btn btn-primary" href={issueHref}>{issueTitle}</Link>}</div>
    <p className="lead">{t('가상 키는 추론 전용이며, 추론 요청은 가상 키로만 보냅니다. 권한·한도는 현재 서버 정책과 발급 상한의 교집합이며 키는 다시 볼 수 없습니다.')}</p></div>
    {error && <p role="alert">{error}</p>}{listError && (!issuing || recovery) && <p role="alert">{listError}</p>}{notice && <p role="status">{notice}</p>}
    {secret?.epoch === auth.epoch && <Modal title={t('발급된 키')} closeOnBackdrop={false} onClose={closeSecret} actions={<button type="button" className="btn-primary" onClick={closeSecret}>{t('닫기')}</button>}>
      <p className="text-warning">{t('키는 다시 볼 수 없습니다. 안전한 곳에 옮긴 뒤 닫으세요. 자동으로 복사·저장하지 않으며, 복사 버튼을 누를 때만 클립보드에 넣습니다.')}</p>
      <SecretValue value={secret.value} />
    </Modal>}
    {recovery && <section aria-label={t('명령 결과 확인')} className="panel-warning"><h3>{t('명령 결과 확인')}</h3><p>{t('명령 ID')}: <code>{recovery.commandId}</code></p><p className="meta">{t('자동 재발급·재전송하지 않습니다. 목록은 페이지 단위이므로 다음 페이지도 확인하세요.')}</p>{resolved ? <p>{t('연결된 키: {{id}} · {{state}}', { id: resolved.id, state: resolved.revoked_at ? t('회수됨') : t('키 복구 불가, 회수 후 명시적으로 새 발급') })}</p> : <p>{t('현재 페이지에서 결과를 찾지 못했습니다. 실패로 단정하거나 다시 발급하지 마세요.')}</p>}<div className="actions"><button className="btn-sm" disabled={loading || busy} onClick={reload}>{t('명령 결과 다시 조회')}</button></div></section>}
    {issuing && canIssue && (linkable.length === 0
      ? <p className="callout callout-critical">{!grantList.length ? t('정책에 허용된 API가 없어 가상 키를 발급할 수 없습니다.') : adminPrincipal ? t('이 서비스 계정에 인가된 Provider 키가 없어 가상 키를 발급할 수 없습니다. 사용자 상세의 Provider 키 인가에서 먼저 인가하세요.') : t('나에게 인가된 Provider 키가 없어 가상 키를 발급할 수 없습니다. 관리자에게 Provider 키 인가를 요청하세요.')}</p>
      : <form key={auth.epoch} aria-label={issueTitle} onSubmit={prepare}>
      <fieldset disabled={busy || !!recovery} className="group"><legend>{issueTitle}</legend>
        <div className="form-grid">
          <label>{t('키 이름')}<input name="name" maxLength={100} required /></label>
          <div className="field"><label htmlFor={`${prefix}-key-validity`}>{t('유효기간')}</label><Select id={`${prefix}-key-validity`} name="validity" defaultValue="" required placeholder={t('선택하세요')} options={durations.map(([value, label]) => ({ value, label: t(label) }))} /></div>
        </div>
        <fieldset className="stack-sm"><legend>{t('연동할 Provider 키 (1개 이상 필수)')}</legend>
          <p className="meta">{t('이 가상 키의 요청은 여기서 고른 Provider 키로 보냅니다. Provider마다 하나를 고를 수 있고, 고르지 않은 Provider의 API는 이 키로 쓸 수 없습니다.')}</p>
          <div className="form-grid">{policyProviders.map(provider => {
            const options = authorizedFor(provider), name = providerNames[provider] ?? provider;
            return <div key={provider} className="field">{options.length > 0
              ? <><label htmlFor={`${prefix}-credential-${provider}`}>{t('{{provider}} Provider 키', { provider: name })}</label><SearchSelect id={`${prefix}-credential-${provider}`} value={chosen[provider] ?? ''} onChange={id => choose(provider, id)} options={[{ value: '', label: t('연동 안 함') }, ...options.map(c => ({ value: c.id, label: credentialLabel(c) }))]} /></>
              : <><span className="field-label">{t('{{provider}} Provider 키', { provider: name })}</span><p className="meta">{adminPrincipal ? t('이 서비스 계정에 인가된 {{provider}} Provider 키가 없습니다.', { provider: name }) : t('나에게 인가된 {{provider}} Provider 키가 없습니다.', { provider: name })}</p></>}</div>;
          })}</div>
        </fieldset>
        <fieldset className="stack-sm"><legend>{t('허용할 API (연동한 Provider마다 1개 이상 필수)')}</legend>
          {chosenProviders.length === 0 ? <p className="meta">{t('Provider 키를 먼저 고르면 그 Provider의 API를 고를 수 있습니다.')}</p>
            : <div className="checks">{grantList.flatMap((g, gi) => chosenProviders.includes(g.provider) ? g.capabilities.map((cap, ci) => { const value = `${gi}:${ci}`; return <label key={value}><input type="checkbox" name="grant" value={value} checked={granted.has(value)} onChange={e => { const on = e.currentTarget.checked; setGranted(prev => { const next = new Set(prev); if (on) next.add(value); else next.delete(value); return next; }); }} />{g.provider} / {g.model} / {cap}</label>; }) : [])}</div>}
        </fieldset>
        <p className="meta">{t('빈 한도는 서버의 실행 상한으로 정규화됩니다. 현재 정책을 우회하지 않습니다.')}</p>
        <div className="form-grid">
          <label>{t('예산 USD (소수점 12자리)')}<input name="budget" className="amount" inputMode="decimal" spellCheck={false} placeholder="0.000000000000" /></label>
          <div className="field"><label htmlFor={`${prefix}-key-budget-mode`}>{t('예산 모드')}</label><Select id={`${prefix}-key-budget-mode`} name="mode" defaultValue="actual" options={[{ value: 'actual', label: t('실사용량 (하드캡 아님)') }, { value: 'reserved', label: t('예약 기반') }]} /></div>
          <label>{t('생성 RPM')}<input name="rpm" className="mono" inputMode="numeric" /></label><label>{t('생성 TPM')}<input name="tpm" className="mono" inputMode="numeric" /></label><label>Count RPM<input name="count_rpm" className="mono" inputMode="numeric" /></label>
        </div>
        <BudgetTimezoneNote />
        <div className="actions"><button type="submit" className="btn-primary" disabled={chosenProviders.length === 0 || withoutApi.length > 0}>{t('발급 내용 확인')}</button><Link className="btn btn-quiet" href={listHref}>{t('취소')}</Link></div>
      </fieldset>
    </form>)}
    {editing && <EditKeyModal target={editing} validity={t(durationName(editing.validity))} owner={adminAll ? null : { grants: grantList, authorized }} onClose={() => setEditing(null)}
      onDone={message => { setEditing(null); setNotice(message); reload(); }} />}
    {confirmation && <Modal variant={confirmation.kind === 'revoke' ? 'danger' : 'normal'} busy={busy} onClose={() => setConfirmation(null)}
      title={confirmation.kind === 'issue' ? t('발급 확인') : t('회수 확인')}
      confirm={{ label: confirmation.kind === 'issue' ? t('발급 확인') : t('회수 확인'), onConfirm: () => void execute() }}>
      {confirmation.kind === 'issue' ? <>
        <p>{confirmation.input.name} · {t(durationName(confirmation.input.validity))}</p>
        <pre>{JSON.stringify({ grants: confirmation.input.grants, limits: confirmation.input.limits }, null, 2)}</pre>
        <p>{t('Provider 키')}: {Object.entries(confirmation.input.provider_credentials).map(([provider, id]) => `${providerNames[provider] ?? provider} · ${labelOf(provider, id)}`).join(', ')}</p>
        <p className="meta">{t('만료일과 실효 상한은 서버가 계산합니다.')}</p>
      </> : <p>{t('{{name}} · {{id}} · version {{version}} 키를 회수합니다. 되돌릴 수 없습니다.', { name: confirmation.key.name, id: confirmation.key.id, version: confirmation.key.version })}</p>}
    </Modal>}
    {shown && <Modal title={t('{{name}} 상세', { name: shown.name })} onClose={() => setDetail(null)} actions={<button type="button" onClick={() => setDetail(null)}>{t('닫기')}</button>}>
      <dl className="kv">
        <dt>ID</dt><dd className="mono">{shown.id}</dd>
        <dt>{t('키 앞부분')}</dt><dd className="mono">{shown.key_hint}</dd>
        <dt>{t('버전')}</dt><dd className="mono">{shown.version}</dd>
        <dt>{t('발급 명령')}</dt><dd className="mono">{shown.created_command_id}</dd>
      </dl>
      <p className="meta">{t('발급 권한·고정 상한')}</p><pre>{JSON.stringify({ grants: shown.grants, limits: shown.limits }, null, 2)}</pre>
    </Modal>}
    {!issuing && <div className="stack">
      <form aria-label={t('키 조회')} className="filters" onSubmit={e => { e.preventDefault(); setQuery(status ? `status=${status}` : ''); setRevision(v => v + 1); }}><div className="field"><label htmlFor={`${prefix}-key-status`}>{t('키 상태')}</label><Select id={`${prefix}-key-status`} value={status} onChange={setStatus} options={[{ value: '', label: t('전체') }, { value: 'active', label: t('활성') }, { value: 'revoked', label: t('회수') }, { value: 'expired', label: t('만료') }]} /></div><div className="actions"><button disabled={loading || busy}>{t('키 조회')}</button></div></form>
      {loading && <p role="status">{t('키 목록 조회 중…')}</p>}{page && <><p className="meta">{t('목록 관측 시각')}: <Time value={page.as_of} /></p>{!page.items.length && <p className="empty">{t('키가 없습니다.')}</p>}{page.items.length > 0 && <div className="table-wrap"><table>
        <thead><tr><th>{t('이름')}</th>{adminAll && <th>{t('키 주인')}</th>}<th>{t('상태')}</th><th>{t('유효기간')}</th><th>{t('생성')}</th><th>{t('만료')}</th><th>{t('Provider 키')}</th><th>{t('동작')}</th></tr></thead>
        <tbody>{page.items.map(key => { const expired = !key.revoked_at && !!key.expires_at && new Date(key.expires_at).getTime() <= new Date(page.as_of).getTime(); return <tr key={key.id}>
          <td><div className="stack-sm"><strong>{key.name}</strong><span className="meta mono">{key.key_hint}</span></div></td>
          {adminAll && <td><Link href={keyListHref(key.principal_id)}>{key.owner.email ?? key.owner.display_name}</Link>{key.owner.kind === 'service' && <> <Pill>{t('서비스')}</Pill></>}</td>}
          <td>{key.revoked_at ? <Badge tone="neutral">{t('회수됨')}</Badge> : expired ? <Badge tone="neutral">{t('만료')}</Badge> : <Badge tone="success">{t('활성')}</Badge>}{key.revoked_at && <p className="meta">{format(key.revoked_at)}</p>}</td>
          <td>{t(durationName(key.validity))}</td>
          <td>{format(key.created_at)}</td>
          <td>{key.expires_at ? format(key.expires_at) : t('없음')}</td>
          <td><div className="stack-sm" aria-label={t('{{name}} Provider 키 연결', { name: key.name })}>{(key.provider_credentials ?? []).length === 0 ? <p className="meta">{t('Provider 키: 연결 없음')}</p> : (key.provider_credentials ?? []).map(b => { return <div key={b.provider} className="row">
          <span className="meta">{t('Provider 키')}: {providerNames[b.provider] ?? b.provider} · {credentialLabel(b)}{b.state === 'retired' && <> <Badge tone="neutral">{t('해제됨')}</Badge></>}</span>
          {b.authorized === false && !key.revoked_at && <><Badge tone="critical">{t('인가 해제됨')}</Badge><span className="text-critical">{t('이 Provider 키의 사용 인가가 해제돼 이 키로 보내는 {{provider}} 요청은 거절됩니다.', { provider: providerNames[b.provider] ?? b.provider })}{` ${t('수정에서 인가된 Provider 키로 다시 연결하세요.')}`}</span></>}
        </div>; })}</div></td>
          <td><div className="actions"><IconButton icon="detail" label={t('{{name}} 상세', { name: key.name })} tip={t('상세')} onClick={() => setDetail(key)} />{!key.revoked_at && <>{!expired && <IconButton icon="edit" label={t('{{name}} 수정', { name: key.name })} tip={t('수정')} disabled={busy || loading} onClick={() => { setError(null); setNotice(null); setEditing(key); }} />}<IconButton icon="revoke" danger label={t('{{name}} 회수', { name: key.name })} tip={t('키 회수')} disabled={busy || loading} onClick={() => { setSecret(null); setConfirmation({ kind: 'revoke', key }); }} /></>}</div></td>
        </tr>; })}</tbody>
      </table></div>}{page.next_cursor && <div className="actions"><button disabled={loading || busy} onClick={() => setQuery(`cursor=${encodeURIComponent(page.next_cursor!)}`)}>{t('다음 키 페이지')}</button></div>}</>}
    </div>}
  </section>;
}
