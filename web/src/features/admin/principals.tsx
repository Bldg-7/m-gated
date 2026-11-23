import { useEffect, useState, useSyncExternalStore } from 'react';
import { useTranslation } from 'react-i18next';
import { session } from '../../lib/api';
import { msgid } from '../../lib/i18n';
import { Link, navigate, useRoute } from '../../lib/router';
import type { Page, Grant, Limits } from '../../lib/types';
import { useTimeFormat } from '../../lib/time';
import { Badge, Modal, Pill, Select, StatusBadge } from '../../ui/ui';
import { GrantTable } from '../../ui/grant-table';
import { BudgetTimezoneNote, KeysScreen } from '../keys/keys';
import { CredentialAccess } from './access';
import { providerNames } from './credentials';
import type { Policy, PrincipalDetail, PrincipalSummary } from './types';
import { directId, fields, PrincipalSelect, ReadState, useCommand, useResource } from './shared';
/** 사용자 상세 하위 페이지 주소. */
export const principalHref = (id: string) => `/dashboard/admin/principals/detail?principal_id=${encodeURIComponent(id)}`;

/** 사용자 목록. 상세는 하위 페이지(/dashboard/admin/principals/detail)에서 연다. */
export function PrincipalsScreen() {
  const { t } = useTranslation();
  const [query, setQuery] = useState('');
  // 예전 주소(/dashboard/admin/principals?principal_id=…)로 들어오면 상세 페이지로 옮긴다.
  useEffect(() => { const id = directId('principal_id'); if (id) navigate(principalHref(id), { replace: true }); }, []);
  const page = useResource<Page<PrincipalSummary>>(`/api/admin/principals${query ? `?${query}` : ''}`);
  const create = useCommand(async ack => { if (ack) { navigate(principalHref(ack.target.id)); return; } await page.reload(); });
  const [creating, setCreating] = useState(false);
  return <div className="stack-lg">
    <p className="callout callout-neutral">{t('owner는 책임자 메타데이터이며 서비스 접근·관리 권한을 자동 부여하지 않습니다.')}</p>
    <section aria-labelledby="principal-list-heading" className="stack">
      <div className="row row-between"><h2 id="principal-list-heading">{t('사용자 목록')}</h2><button className="btn-primary" disabled={create.busy} onClick={() => setCreating(true)}>{t('서비스 생성')}</button></div>
      {create.view}
      {creating && <Modal title={t('서비스 생성 (초기 deny-all)')} onClose={() => setCreating(false)} actions={<><button type="submit" form="service-create-form" className="btn-primary">{t('서비스 생성 확인')}</button><button type="button" className="btn-quiet" onClick={() => setCreating(false)}>{t('취소')}</button></>}>
        <form id="service-create-form" aria-label={t('서비스 생성')} className="stack-sm" onSubmit={e => { const f = fields(e); setCreating(false); create.prepare({ label: msgid('deny-all 서비스 생성 확인'), url: '/api/admin/principals', method: 'POST', body: { display_name: String(f.get('display_name')), management_owner_id: String(f.get('owner')) } }); }}>
          <label>{t('새 서비스 이름')}<input name="display_name" required maxLength={100} /></label><div className="field"><label htmlFor="service-create-owner">{t('새 서비스 owner')}</label><PrincipalSelect id="service-create-owner" name="owner" kind="human" required /></div>
        </form>
      </Modal>}
      <form aria-label={t('사용자 조회')} className="filters" onSubmit={e => { const f = fields(e), q = new URLSearchParams(); for (const key of ['kind', 'disabled', 'email']) { const value = String(f.get(key) ?? '').trim(); if (value) q.set(key, value); } setQuery(q.toString()); if (q.toString() === query) void page.reload().catch(() => {}); }}>
        <div className="field"><label htmlFor="principal-kind">{t('사용자 종류')}</label><Select id="principal-kind" name="kind" defaultValue="" options={[{ value: '', label: t('전체') }, { value: 'human', label: t('사람') }, { value: 'service', label: t('서비스') }]} /></div>
        <div className="field"><label htmlFor="principal-disabled">{t('계정 정지')}</label><Select id="principal-disabled" name="disabled" defaultValue="" options={[{ value: '', label: t('전체') }, { value: 'true', label: t('정지') }, { value: 'false', label: t('활성') }]} /></div>
        <label>{t('이메일 (일부만 입력해도 됨)')}<input name="email" type="search" maxLength={200} autoComplete="off" spellCheck={false} /></label>
        <div className="actions"><button className="btn-primary">{t('사용자 조회')}</button></div>
      </form>
      <ReadState {...page} asOf={page.data?.as_of} />
      {page.data && <>
        {!page.data.items.length && <p className="empty">{t('사용자가 없습니다.')}</p>}
        {page.data.items.length > 0 && <div className="table-wrap"><table>
          <thead><tr><th>{t('이름')}</th><th>{t('종류')}</th><th>{t('이메일')}</th><th>{t('상태')}</th><th>{t('버전 (사용자 / 정책 / 안전)')}</th><th>owner</th><th>{t('동작')}</th></tr></thead>
          <tbody>{page.data.items.map(p => <tr key={p.id}>
            <td><div className="stack-sm"><strong>{p.display_name}</strong><span className="meta mono">{p.id}</span></div></td>
            <td><Pill>{p.kind === 'service' ? t('서비스') : t('사람')}</Pill></td>
            <td>{p.email ?? <span className="meta">{t('이메일 없음')}</span>}</td>
            <td><div className="row">{p.disabled ? <Badge tone="critical">{t('계정 정지')}</Badge> : <Badge tone="success">{t('계정 활성')}</Badge>}{p.inference_safety.paused && <Badge tone="critical">{t('자동 안전차단')}</Badge>}{p.block_state.reasons.filter(reason => !(reason === 'disabled' && p.disabled) && !(reason === 'inference_paused' && p.inference_safety.paused)).map(reason => <StatusBadge key={reason} kind="block" value={reason} />)}</div></td>
            <td className="mono">{p.version} / {p.policy_version} / {p.inference_safety.version}</td>
            <td className="mono">{p.management_owner_id ?? <span className="meta">{t('없음')}</span>}</td>
            <td><Link className="btn btn-sm" aria-label={t('{{name}} 상세', { name: p.display_name })} href={principalHref(p.id)}>{t('상세')}</Link></td>
          </tr>)}</tbody>
        </table></div>}
        {page.data.next_cursor && <div className="actions"><button onClick={() => setQuery(`cursor=${encodeURIComponent(page.data!.next_cursor!)}`)}>{t('다음 사용자 페이지')}</button></div>}
      </>}
    </section>
  </div>;
}
/** 사용자 상세 하위 페이지. 주소의 principal_id 로 사용자 하나를 연다. */
export function PrincipalDetailScreen() {
  const { t } = useTranslation();
  const id = new URLSearchParams(useRoute().search).get('principal_id') ?? '';
  return <div className="stack-lg">
    {id ? <PrincipalEditor key={id} id={id} /> : <p role="alert">{t('사용자 ID가 없습니다. 사용자 목록에서 상세를 여세요.')}</p>}
  </div>;
}
/** 서비스 계정의 가상 키 발급 하위 페이지. 주소의 principal_id 로 서비스 계정 하나를 연다. */
export function PrincipalKeyIssueScreen() {
  const { t } = useTranslation();
  const id = new URLSearchParams(useRoute().search).get('principal_id') ?? '';
  return id ? <PrincipalKeyIssue key={id} id={id} /> : <p role="alert">{t('사용자 ID가 없습니다. 사용자 목록에서 상세를 여세요.')}</p>;
}
function PrincipalKeyIssue({ id }: { id: string }) {
  const { t } = useTranslation();
  const detail = useResource<PrincipalDetail>(`/api/admin/principals/${encodeURIComponent(id)}`), p = detail.data?.principal;
  return <div className="stack">
    <ReadState {...detail} />
    {p && (p.kind === 'service'
      ? <KeysScreen key={`${p.id}:${session.snapshot().epoch}`} view="issue" adminPrincipal={{ id: p.id, service: true, grants: p.policy.grants, authorized: p.provider_credentials ?? [] }} />
      : <p role="alert">{t('사람 사용자의 가상 키는 본인이 내 가상 키에서 발급합니다.')}</p>)}
  </div>;
}
function PrincipalEditor({ id }: { id: string }) {
  const { t } = useTranslation();
  const detail = useResource<PrincipalDetail>(`/api/admin/principals/${encodeURIComponent(id)}`);
  const command = useCommand(async () => { await detail.reload(); });
  const [formError, setFormError] = useState<string | null>(null), p = detail.data?.principal, format = useTimeFormat();
  // Provider 키 인가를 저장한 결과. 상세를 다시 읽는 동안에도 보이게 상세 내용 밖에 둔다.
  const [accessNote, setAccessNote] = useState<string | null>(null);
  // 계정 정지·정지 해제 확인 창. 사유를 여기서 받는다.
  const [suspending, setSuspending] = useState(false);
  const me = useSyncExternalStore(session.subscribe, session.snapshot).me?.principal.id;
  const pauseReasons: Record<string, string> = { cost_bound_violation: t('실제 사용량이 요청을 받을 때 정한 상한을 넘었습니다.'), amount_overflow: t('금액이 계산할 수 있는 범위를 넘었습니다.') };
  return <section aria-label={t('사용자 상세')} className="stack">
    <ReadState {...detail} />{formError && <p role="alert">{formError}</p>}{command.view}{accessNote && <p role="status">{accessNote}</p>}{p && <>
    <div className={p.block_state.reasons.length ? 'card card-critical' : 'card'}>
      <div className="stack-sm">
        <div className="row"><h3>{p.display_name}</h3><Pill>{p.kind === 'service' ? t('서비스') : t('사람')}</Pill>{p.block_state.reasons.map(reason => <StatusBadge key={reason} kind="block" value={reason} />)}</div>
        <p className="meta"><span className="mono">{p.id}</span> · {p.kind} · {t('사용자 버전 {{principal}} · 정책 버전 {{policy}} · 안전 버전 {{safety}}', { principal: p.version, policy: p.policy.version, safety: p.inference_safety.version })}</p>
        <p className="meta">owner: <span className="mono">{p.management_owner_id ?? t('없음')}</span> · {t('과금 차단')}: {p.block_state.reasons.join(', ') || t('없음')} · {t('전체 미정산 {{count}}', { count: p.block_state.global_unknown_count })}</p>
      </div>
      <details><summary>{t('실행 설정 상한 (편집 불가)')}</summary><pre>{JSON.stringify(detail.data!.execution_ceiling, null, 2)}</pre></details>
      <div className="actions"><Link className="btn btn-sm" href={`/dashboard/admin/usage?principal_id=${encodeURIComponent(p.id)}`}>{t('이 사용자 사용량·현재 잔액')}</Link><Link className="btn btn-sm" href={`/dashboard/admin/requests?principal_id=${encodeURIComponent(p.id)}`}>{t('이 사용자 미정산')}</Link>
        {p.policy.disabled
          ? <button className="btn-sm" disabled={command.busy} onClick={() => setSuspending(true)}>{t('정지 해제')}</button>
          : <button className="btn-sm btn-danger-outline" disabled={command.busy || p.id === me} title={p.id === me ? t('자기 계정은 정지할 수 없습니다.') : undefined} onClick={() => setSuspending(true)}>{t('이 계정 정지')}</button>}
      </div>
    </div>
    {suspending && <Modal variant={p.policy.disabled ? 'normal' : 'danger'} title={p.policy.disabled ? t('{{name}} 정지 해제', { name: p.display_name }) : t('{{name}} 계정 정지', { name: p.display_name })} onClose={() => setSuspending(false)}
      actions={<><button type="submit" form="principal-suspend-form" className={p.policy.disabled ? 'btn-primary' : 'btn-danger'}>{p.policy.disabled ? t('정지 해제') : t('이 계정 정지')}</button><button type="button" className="btn-quiet" data-modal-cancel="" onClick={() => setSuspending(false)}>{t('취소')}</button></>}>
      <form id="principal-suspend-form" className="stack-sm" onSubmit={e => { const f = fields(e); setSuspending(false); command.run({ label: p.policy.disabled ? msgid('정지 해제') : msgid('이 계정 정지'), url: `/api/admin/principals/${p.id}/policy`, method: 'PUT', body: { expected_version: p.policy.version, disabled: !p.policy.disabled, grants: p.policy.grants, limits: p.policy.limits, reason: String(f.get('reason')) } }); }}>
        <p>{p.policy.disabled
          ? t('다음 요청부터 이 계정의 추론과 대시보드 접근을 다시 허용합니다.')
          : p.kind === 'service' ? t('다음 요청부터 이 서비스 계정의 추론을 막습니다. 정책과 가상 키는 지우지 않습니다.') : t('다음 요청부터 이 사람의 추론과 대시보드 접근을 막습니다. 이 사람이 owner인 서비스 계정도 막힙니다. 정책과 가상 키는 지우지 않습니다.')}</p>
        <label>{p.policy.disabled ? t('정지 해제 사유') : t('정지 사유')}<input name="reason" required maxLength={1000} /></label>
      </form>
    </Modal>}
    {/* 자동과금 차단은 차단 중일 때만 보여 준다. 해제 상태에는 할 일이 없다. */}
    {(p.kind === 'service' || p.inference_safety.paused) && <div className="grid-2">
      {p.kind === 'service' && <form key={`principal-${p.version}`} aria-label={t('서비스 변경')} onSubmit={e => { const f = fields(e); command.prepare({ label: msgid('서비스 변경 확인'), url: `/api/admin/principals/${p.id}`, method: 'PATCH', body: { expected_version: p.version, display_name: String(f.get('display_name')), management_owner_id: String(f.get('owner')) } }); }}><fieldset disabled={command.busy} className="group"><legend>{t('서비스 변경')}</legend><label>{t('서비스 이름')}<input name="display_name" defaultValue={p.display_name} required maxLength={100} /></label><div className="field"><label htmlFor="service-owner">{t('서비스 owner')}</label><PrincipalSelect id="service-owner" name="owner" kind="human" defaultValue={p.management_owner_id ?? ''} required /></div><div className="actions"><button className="btn-primary">{t('서비스 변경 확인')}</button></div></fieldset></form>}
      {p.inference_safety.paused && <section aria-labelledby="principal-safety-heading" className="card card-alert"><h3 id="principal-safety-heading">{t('자동과금 차단됨')}</h3><p>{p.inference_safety.paused_at ? t('{{at}}부터 이 사용자의 유료 요청을 막고 있습니다.', { at: format(p.inference_safety.paused_at) }) : t('이 사용자의 유료 요청을 막고 있습니다.')} {p.inference_safety.reason ? pauseReasons[p.inference_safety.reason] ?? p.inference_safety.reason : ''}</p>{p.inference_safety.trigger_request_id && <p><Link href={`/dashboard/admin/requests?principal_id=${encodeURIComponent(p.id)}&request_id=${encodeURIComponent(p.inference_safety.trigger_request_id)}`}>{t('원인이 된 요청 보기')}</Link></p>}<p className="meta">{t('여기서 해제해도 미정산 차단, 계정 정지, 예산 소진, 회수된 키는 그대로입니다.')}</p><form key={`safety-${p.inference_safety.version}`} aria-label={t('명시 안전차단 해제')} onSubmit={e => { const f = fields(e); if (!f.has('confirmed')) return; command.prepare({ label: msgid('별도 안전차단 해제 확인'), url: `/api/admin/principals/${p.id}/inference-pause/release`, method: 'POST', body: { expected_version: p.inference_safety.version, reason: String(f.get('reason')) } }); }}><fieldset disabled={command.busy} className="stack"><label>{t('안전차단 해제 사유')}<input name="reason" required maxLength={1000} /></label><label className="attest"><input name="confirmed" type="checkbox" required />{t('과금 원인과 남은 차단을 확인했습니다')}</label><div className="actions"><button className="btn-primary">{t('명시 해제 확인')}</button></div></fieldset></form></section>}
    </div>}
    <PolicyForm key={`policy-${p.policy.version}`} principalId={p.id} policy={p.policy} ceiling={detail.data!.execution_ceiling.grants} busy={command.busy} onSubmit={body => command.prepare({ label: msgid('정책 전체 교체 확인'), url: `/api/admin/principals/${p.id}/policy`, method: 'PUT', body })} onError={setFormError} />
    <CredentialAccess principalId={p.id} authorized={p.provider_credentials ?? []} onChanged={async () => { await detail.reload(); }} onNotice={setAccessNote} />
    <div className="card"><KeysScreen key={`${p.id}:${session.snapshot().epoch}:${JSON.stringify(p.provider_credentials ?? [])}`} adminPrincipal={{ id: p.id, service: p.kind === 'service', grants: p.policy.grants, authorized: p.provider_credentials ?? [] }} /></div>
  </>}</section>;
}

/** 모델·API 하나. 정책의 grant 는 (provider, model) 마다 capabilities 묶음이라 화면에서는 한 줄씩 푼다. */
type Choice = { provider: string; model: string; capability: string; known: boolean };
const choiceKey = (c: { provider: string; model: string; capability: string }) => `${c.provider}\u0000${c.model}\u0000${c.capability}`;
/** 서버의 USD 문자열(소수점 12자리)을 입력칸용으로 줄인다. 값 자체는 바꾸지 않는다. */
function shortUsd(value: string) { const [whole, fraction = ''] = value.split('.'); const trimmed = fraction.replace(/0+$/, ''); return trimmed ? `${whole}.${trimmed}` : whole; }
/** 입력한 금액을 서버 형식(소수점 12자리)으로 맞춘다. 형식이 틀리면 null. */
export function toUsd(raw: string): string | null {
  const m = /^(0|[1-9][0-9]{0,15})(?:\.([0-9]{1,12}))?$/.exec(raw.trim());
  return m ? `${m[1]}.${(m[2] ?? '').padEnd(12, '0')}` : null;
}
/** 정책 편집. 쓸 수 있는 모델·API는 표(GrantTable)에서, 한도는 입력칸으로 고르고 서버 형식으로 바꿔 보낸다. */
function PolicyForm({ policy, ceiling, busy, onSubmit, onError }: { principalId: string; policy: Policy; ceiling: Grant[]; busy: boolean; onSubmit: (body: Record<string, unknown>) => void; onError: (message: string | null) => void }) {
  const { t } = useTranslation();
  const flatten = (grants: Grant[]) => grants.flatMap(g => g.capabilities.map(capability => ({ provider: g.provider, model: g.model, capability })));
  // 고를 수 있는 목록은 서버 실행 상한(설정 파일·승인된 카탈로그)이다. 지금 정책에만 남은 항목도 보여 주어 뺄 수 있게 한다.
  const known = new Set(flatten(ceiling).map(choiceKey));
  const choices: Choice[] = [];
  for (const c of [...flatten(ceiling), ...flatten(policy.grants)]) if (!choices.some(x => choiceKey(x) === choiceKey(c))) choices.push({ ...c, known: known.has(choiceKey(c)) });
  const [selected, setSelected] = useState(() => new Set(flatten(policy.grants).map(choiceKey)));
  const [budgetMode, setBudgetMode] = useState<string>(policy.limits.budget?.mode ?? 'none');
  const toggle = (key: string, on: boolean) => setSelected(prev => { const next = new Set(prev); if (on) next.add(key); else next.delete(key); return next; });
  const submit = (event: React.FormEvent<HTMLFormElement>) => {
    const f = fields(event); onError(null);
    // 고른 항목을 (provider, model) 마다 묶어 서버 grant 형식으로 만든다. 순서는 목록 순서를 따른다.
    const grants: Grant[] = [];
    for (const c of choices) {
      if (!selected.has(choiceKey(c))) continue;
      const g = grants.find(x => x.provider === c.provider && x.model === c.model);
      if (g) g.capabilities.push(c.capability); else grants.push({ provider: c.provider, model: c.model, capabilities: [c.capability] });
    }
    const limits: Limits = { budget: null, rpm: null, tpm: null, count_rpm: null };
    for (const field of ['rpm', 'tpm', 'count_rpm'] as const) {
      const raw = String(f.get(field) ?? '').trim();
      if (raw && (!/^[1-9][0-9]*$/.test(raw) || Number(raw) > 1000000000)) { onError(t('요청·토큰 한도는 1~1000000000의 정수입니다.')); return; }
      limits[field] = raw ? Number(raw) : null;
    }
    if (budgetMode !== 'none') {
      const amount = toUsd(String(f.get('budget') ?? ''));
      if (!amount) { onError(t('월 예산은 USD 금액으로 입력하세요(예: 100 또는 12.5, 소수점 12자리까지).')); return; }
      limits.budget = { mode: budgetMode === 'reserved' ? 'reserved' : 'actual', limit_usd: amount, period: { kind: 'calendar_month' } };
    }
    // 계정 정지는 요약 카드의 버튼으로 따로 바꾼다. 여기서는 지금 상태를 그대로 보낸다.
    onSubmit({ expected_version: policy.version, disabled: policy.disabled, grants, limits, reason: String(f.get('reason')) });
  };
  const budget = policy.limits.budget;
  return <form aria-label={t('정책 교체')} onSubmit={submit}><fieldset disabled={busy} className="group"><legend>{t('정책 전체 교체')}</legend>
    <p className="meta">{t('저장하면 정책 전체가 이 내용으로 바뀝니다. 빈 한도는 정책에서 제한하지 않는다는 뜻이고, 서버 실행 상한(설정 파일)은 그래도 적용됩니다. 계정 정지와 자동 안전차단은 이 폼으로 바뀌지 않습니다.')}</p>
    <fieldset className="stack-sm"><legend>{t('사용할 수 있는 모델·API')}</legend>
      {choices.length === 0 ? <p className="meta">{t('고를 수 있는 모델이 없습니다. 모델을 먼저 승인하세요.')} <Link href="/dashboard/admin/models">{t('모델·가격 화면 열기')}</Link></p> : <>
        <GrantTable rows={choices.map(c => ({ key: choiceKey(c), provider: c.provider, providerLabel: providerNames[c.provider] ?? c.provider, model: c.model, capability: c.capability, note: c.known ? undefined : <Badge tone="warning">{t('지금은 쓸 수 없는 모델')}</Badge> }))}
          selected={selected} onToggle={toggle} onAll={on => setSelected(on ? new Set(choices.map(choiceKey)) : new Set())} />
      </>}
    </fieldset>
    <fieldset className="stack-sm"><legend>{t('한도')}</legend>
      <div className="form-grid">
        <div className="field"><label htmlFor="policy-budget-mode">{t('월 예산 방식')}</label><Select id="policy-budget-mode" value={budgetMode} onChange={setBudgetMode} options={[{ value: 'none', label: t('예산 없음') }, { value: 'actual', label: t('실사용량 (하드캡 아님)') }, { value: 'reserved', label: t('예약 기반') }]} /></div>
        {budgetMode !== 'none' && <label>{t('월 예산 (USD)')}<input name="budget" className="amount" inputMode="decimal" spellCheck={false} defaultValue={budget ? shortUsd(budget.limit_usd) : ''} required /></label>}
        <label>{t('분당 요청 수 한도 (RPM)')}<input name="rpm" className="mono" inputMode="numeric" defaultValue={policy.limits.rpm ?? ''} placeholder={t('제한 없음')} /></label>
        <label>{t('분당 토큰 수 한도 (TPM)')}<input name="tpm" className="mono" inputMode="numeric" defaultValue={policy.limits.tpm ?? ''} placeholder={t('제한 없음')} /></label>
        <label>{t('분당 토큰 계산 요청 수 한도 (Count RPM)')}<input name="count_rpm" className="mono" inputMode="numeric" defaultValue={policy.limits.count_rpm ?? ''} placeholder={t('제한 없음')} /></label>
      </div>
      {budgetMode !== 'none' && <BudgetTimezoneNote />}
    </fieldset>
    <label>{t('정책 변경 사유')}<input name="reason" required maxLength={1000} /></label>
    <div className="actions"><button className="btn-primary">{t('정책 교체 확인')}</button></div>
  </fieldset></form>;
}
