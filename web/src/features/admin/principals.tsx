import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { session } from '../../lib/api';
import { msgid } from '../../lib/i18n';
import { Link, navigate, useRoute } from '../../lib/router';
import type { Page, Grant, Limits } from '../../lib/types';
import { Pill, Select, StatusBadge } from '../../ui/ui';
import { KeysScreen } from '../keys/keys';
import type { Policy, PrincipalDetail, PrincipalSummary } from './types';
import { directId, fields, ReadState, useCommand, useResource } from './shared';
/** 사용자 상세 하위 페이지 주소. */
export const principalHref = (id: string) => `/ui/admin/principals/detail?principal_id=${encodeURIComponent(id)}`;

/** 사용자 목록. 상세는 하위 페이지(/ui/admin/principals/detail)에서 연다. */
export function PrincipalsScreen() {
  const { t } = useTranslation();
  const [query, setQuery] = useState('');
  // 예전 주소(/ui/admin/principals?principal_id=…)로 들어오면 상세 페이지로 옮긴다.
  useEffect(() => { const id = directId('principal_id'); if (id) navigate(principalHref(id), { replace: true }); }, []);
  const page = useResource<Page<PrincipalSummary>>(`/gateway/admin/principals${query ? `?${query}` : ''}`);
  const create = useCommand(async ack => { if (ack) { navigate(principalHref(ack.target.id)); return; } await page.reload(); });
  return <div className="stack-lg">
    <p className="callout callout-neutral">{t('owner는 책임자 메타데이터이며 서비스 접근·관리 권한을 자동 부여하지 않습니다.')}</p>
    <section aria-labelledby="principal-list-heading" className="stack">
      <h2 id="principal-list-heading">{t('사용자 목록')}</h2>
      <form aria-label={t('사용자 조회')} className="filters" onSubmit={e => { const f = fields(e), q = new URLSearchParams(); for (const key of ['kind', 'disabled', 'limit']) if (f.get(key)) q.set(key, String(f.get(key))); setQuery(q.toString()); if (q.toString() === query) void page.reload().catch(() => {}); }}>
        <div className="field"><label htmlFor="principal-kind">{t('사용자 종류')}</label><Select id="principal-kind" name="kind"><option value="">{t('전체')}</option><option value="human">{t('사람')}</option><option value="service">{t('서비스')}</option></Select></div>
        <div className="field"><label htmlFor="principal-disabled">{t('계정 정지')}</label><Select id="principal-disabled" name="disabled"><option value="">{t('전체')}</option><option value="true">{t('정지')}</option><option value="false">{t('활성')}</option></Select></div>
        <label>{t('사용자 페이지 크기')}<input name="limit" type="number" min="1" max="100" /></label>
        <div className="actions"><button className="btn-primary">{t('사용자 조회')}</button></div>
      </form>
      <ReadState {...page} asOf={page.data?.as_of} />
      {page.data && <>
        {!page.data.items.length && <p className="empty">{t('사용자가 없습니다.')}</p>}
        {page.data.items.length > 0 && <ul className="list">{page.data.items.map(p => <li key={p.id}>
          <div className="list-main">
            <div className="row"><span className="list-title">{p.display_name}</span><Pill>{p.kind === 'service' ? t('서비스') : t('사람')}</Pill>{p.block_state.reasons.map(reason => <StatusBadge key={reason} kind="block" value={reason} />)}</div>
            <p className="meta"><span className="mono">{p.id}</span> · {p.kind} · {p.email ?? t('이메일 없음')} · owner <span className="mono">{p.management_owner_id ?? t('없음')}</span></p>
            <p className="meta">{t('사용자 버전 {{principal}} / 정책 버전 {{policy}} / 안전 버전 {{safety}}', { principal: p.version, policy: p.policy_version, safety: p.inference_safety.version })} · {p.disabled ? t('계정 정지') : t('계정 활성')} · {p.inference_safety.paused ? t('자동 안전차단') : t('안전차단 없음')} · {t('차단')} {p.block_state.reasons.join(', ')}</p>
          </div>
          <div className="list-side"><Link className="btn btn-sm" aria-label={t('{{name}} 상세', { name: p.display_name })} href={principalHref(p.id)}>{t('상세')}</Link></div>
        </li>)}</ul>}
        {page.data.next_cursor && <div className="actions"><button onClick={() => setQuery(`cursor=${encodeURIComponent(page.data!.next_cursor!)}`)}>{t('다음 사용자 페이지')}</button></div>}
      </>}
    </section>
    <div className="grid-2">
      <form aria-label={t('사용자 ID 조회')} className="card" onSubmit={e => navigate(principalHref(String(fields(e).get('id')).trim()))}><label>{t('사용자 ID')}<input name="id" className="mono" spellCheck={false} required /></label><div className="actions"><button>{t('상세 조회')}</button></div></form>
      <div className="stack">
        <form aria-label={t('서비스 생성')} onSubmit={e => { const f = fields(e); create.prepare({ label: msgid('deny-all 서비스 생성 확인'), url: '/gateway/admin/principals', method: 'POST', body: { display_name: String(f.get('display_name')), management_owner_id: String(f.get('owner')) } }); }}><fieldset disabled={create.busy} className="group"><legend>{t('서비스 생성 (초기 deny-all)')}</legend><label>{t('새 서비스 이름')}<input name="display_name" required maxLength={100} /></label><label>{t('새 서비스 owner ID')}<input name="owner" className="mono" spellCheck={false} required /></label><div className="actions"><button className="btn-primary">{t('서비스 생성 확인')}</button></div></fieldset></form>{create.view}
      </div>
    </div>
    <details><summary>{t('전체 키 조회·회수')}</summary><KeysScreen adminAll /></details>
  </div>;
}
/** 사용자 상세 하위 페이지. 주소의 principal_id 로 사용자 하나를 연다. */
export function PrincipalDetailScreen() {
  const { t } = useTranslation();
  const id = new URLSearchParams(useRoute().search).get('principal_id') ?? '';
  return <div className="stack-lg">
    <p><Link href="/ui/admin/principals">{t('← 사용자 목록')}</Link></p>
    {id ? <PrincipalEditor key={id} id={id} /> : <p role="alert">{t('사용자 ID가 없습니다. 사용자 목록에서 상세를 여세요.')}</p>}
  </div>;
}
function PrincipalEditor({ id }: { id: string }) {
  const { t } = useTranslation();
  const detail = useResource<PrincipalDetail>(`/gateway/admin/principals/${encodeURIComponent(id)}`);
  const command = useCommand(async () => { await detail.reload(); });
  const [formError, setFormError] = useState<string | null>(null), p = detail.data?.principal;
  return <section aria-label={t('사용자 상세')} className="stack">
    <ReadState {...detail} />{formError && <p role="alert">{formError}</p>}{command.view}{p && <>
    <div className={p.block_state.reasons.length ? 'card card-critical' : 'card'}>
      <div className="stack-sm">
        <div className="row"><h3>{p.display_name}</h3><Pill>{p.kind === 'service' ? t('서비스') : t('사람')}</Pill>{p.block_state.reasons.map(reason => <StatusBadge key={reason} kind="block" value={reason} />)}</div>
        <p className="meta"><span className="mono">{p.id}</span> · {p.kind} · {t('사용자 버전 {{principal}} · 정책 버전 {{policy}} · 안전 버전 {{safety}}', { principal: p.version, policy: p.policy.version, safety: p.inference_safety.version })}</p>
        <p className="meta">owner: <span className="mono">{p.management_owner_id ?? t('없음')}</span> · {t('과금 차단')}: {p.block_state.reasons.join(', ') || t('없음')} · {t('전체 미정산 {{count}}', { count: p.block_state.global_unknown_count })}</p>
      </div>
      <details><summary>{t('실행 설정 상한 (편집 불가)')}</summary><pre>{JSON.stringify(detail.data!.execution_ceiling, null, 2)}</pre></details>
      <div className="actions"><Link className="btn btn-sm" href={`/ui/admin/usage?principal_id=${encodeURIComponent(p.id)}`}>{t('이 사용자 사용량·현재 잔액')}</Link><Link className="btn btn-sm" href={`/ui/admin/requests?principal_id=${encodeURIComponent(p.id)}`}>{t('이 사용자 미정산')}</Link></div>
    </div>
    <div className="grid-2">
      {p.kind === 'service' && <form key={`principal-${p.version}`} aria-label={t('서비스 변경')} onSubmit={e => { const f = fields(e); command.prepare({ label: msgid('서비스 변경 확인'), url: `/gateway/admin/principals/${p.id}`, method: 'PATCH', body: { expected_version: p.version, display_name: String(f.get('display_name')), management_owner_id: String(f.get('owner')) } }); }}><fieldset disabled={command.busy} className="group"><legend>{t('서비스 변경')}</legend><label>{t('서비스 이름')}<input name="display_name" defaultValue={p.display_name} required maxLength={100} /></label><label>{t('서비스 owner ID')}<input name="owner" className="mono" spellCheck={false} defaultValue={p.management_owner_id ?? ''} required /></label><div className="actions"><button className="btn-primary">{t('서비스 변경 확인')}</button></div></fieldset></form>}
      <section aria-label={t('자동 안전차단')} className={p.inference_safety.paused ? 'card card-critical' : 'card'}><h3>{t('자동 과금 안전차단')}</h3><p className="meta">{t('{{state}} · 안전 버전 {{version}} · 사유 {{reason}} · 시각 {{at}} · 원인 요청 {{request}}', { state: p.inference_safety.paused ? t('차단 중') : t('해제 상태'), version: p.inference_safety.version, reason: p.inference_safety.reason ?? t('없음'), at: p.inference_safety.paused_at ?? t('없음'), request: p.inference_safety.trigger_request_id ?? t('없음') })}</p><p className="meta">{t('별도 해제는 unknown·계정 정지·fence·예산·키 회수를 해제하지 않습니다.')}</p><form key={`safety-${p.inference_safety.version}`} aria-label={t('명시 안전차단 해제')} onSubmit={e => { const f = fields(e); if (!f.has('confirmed')) return; command.prepare({ label: msgid('별도 안전차단 해제 확인'), url: `/gateway/admin/principals/${p.id}/inference-pause/release`, method: 'POST', body: { expected_version: p.inference_safety.version, reason: String(f.get('reason')) } }); }}><fieldset disabled={command.busy} className="stack"><label>{t('안전차단 해제 사유')}<input name="reason" required maxLength={1000} /></label><label className="attest"><input name="confirmed" type="checkbox" required />{t('과금 원인과 남은 차단을 확인했습니다')}</label><div className="actions"><button className="btn-primary">{t('명시 해제 확인')}</button></div></fieldset></form></section>
    </div>
    <PolicyForm key={`policy-${p.policy.version}`} principalId={p.id} policy={p.policy} ceiling={detail.data!.execution_ceiling.grants} busy={command.busy} onSubmit={body => command.prepare({ label: msgid('정책 전체 교체 확인'), url: `/gateway/admin/principals/${p.id}/policy`, method: 'PUT', body })} onError={setFormError} />
    <div className="card"><KeysScreen key={`${p.id}:${session.snapshot().epoch}`} adminPrincipal={{ id: p.id, service: p.kind === 'service', grants: p.policy.grants }} /></div>
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
/** 정책 편집. 쓸 수 있는 모델·API는 체크박스로, 한도는 입력칸으로 고르고 서버 형식으로 바꿔 보낸다. */
function PolicyForm({ policy, ceiling, busy, onSubmit, onError }: { principalId: string; policy: Policy; ceiling: Grant[]; busy: boolean; onSubmit: (body: Record<string, unknown>) => void; onError: (message: string | null) => void }) {
  const { t } = useTranslation();
  const flatten = (grants: Grant[]) => grants.flatMap(g => g.capabilities.map(capability => ({ provider: g.provider, model: g.model, capability })));
  // 고를 수 있는 목록은 서버 실행 상한(설정 파일·승인된 카탈로그)이다. 지금 정책에만 남은 항목도 보여 주어 뺄 수 있게 한다.
  const known = new Set(flatten(ceiling).map(choiceKey));
  const choices: Choice[] = [];
  for (const c of [...flatten(ceiling), ...flatten(policy.grants)]) if (!choices.some(x => choiceKey(x) === choiceKey(c))) choices.push({ ...c, known: known.has(choiceKey(c)) });
  const [selected, setSelected] = useState(() => new Set(flatten(policy.grants).map(choiceKey)));
  const [budgetMode, setBudgetMode] = useState<string>(policy.limits.budget?.mode ?? 'none');
  const all = choices.length > 0 && choices.every(c => selected.has(choiceKey(c)));
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
      limits.budget = { mode: budgetMode === 'reserved' ? 'reserved' : 'actual', limit_usd: amount, period: { kind: 'calendar_month', timezone: String(f.get('timezone') ?? '').trim() } };
    }
    const body: Record<string, unknown> = { expected_version: policy.version, disabled: f.has('disabled'), grants, limits, reason: String(f.get('reason')) };
    if (f.has('fence_confirmed')) body.fence_release = { fence_ref: String(f.get('fence_ref')), old_processes_stopped_ref: String(f.get('stopped_ref')), confirmed: true };
    onSubmit(body);
  };
  const budget = policy.limits.budget;
  return <form aria-label={t('정책 교체')} onSubmit={submit}><fieldset disabled={busy} className="group"><legend>{t('정책 전체 교체')}</legend>
    <p className="meta">{t('저장하면 정책 전체가 이 내용으로 바뀝니다. 빈 한도는 정책에서 제한하지 않는다는 뜻이고, 서버 실행 상한(설정 파일)은 그래도 적용됩니다. 자동 안전차단은 이 폼으로 해제되지 않습니다.')}</p>
    <label><input type="checkbox" name="disabled" defaultChecked={policy.disabled} />{t('관리용 계정 정지')}</label>
    <fieldset className="stack-sm"><legend>{t('사용할 수 있는 모델·API')}</legend>
      {choices.length === 0 ? <p className="meta">{t('고를 수 있는 모델이 없습니다. 모델·가격 화면에서 모델을 먼저 승인하세요.')}</p> : <>
        <label><input type="checkbox" checked={all} onChange={e => setSelected(e.currentTarget.checked ? new Set(choices.map(choiceKey)) : new Set())} />{t('전체 선택')}</label>
        <div className="checks">{choices.map(c => { const key = choiceKey(c); return <label key={key}><input type="checkbox" checked={selected.has(key)} onChange={e => toggle(key, e.currentTarget.checked)} />{c.provider} / <span className="mono">{c.model}</span> / {c.capability}{!c.known && <span className="text-warning"> ({t('지금은 쓸 수 없는 모델')})</span>}</label>; })}</div>
      </>}
    </fieldset>
    <fieldset className="stack-sm"><legend>{t('한도')}</legend>
      <div className="form-grid">
        <div className="field"><label htmlFor="policy-budget-mode">{t('월 예산 방식')}</label><Select id="policy-budget-mode" value={budgetMode} onChange={e => setBudgetMode(e.target.value)}><option value="none">{t('예산 없음')}</option><option value="actual">{t('실사용량 (하드캡 아님)')}</option><option value="reserved">{t('예약 기반')}</option></Select></div>
        {budgetMode !== 'none' && <><label>{t('월 예산 (USD)')}<input name="budget" className="amount" inputMode="decimal" spellCheck={false} defaultValue={budget ? shortUsd(budget.limit_usd) : ''} required /></label>
        <label>{t('예산 기준 시간대 (IANA)')}<input name="timezone" className="mono" spellCheck={false} defaultValue={budget?.period.timezone ?? 'Asia/Seoul'} required /></label></>}
        <label>{t('분당 요청 수 한도 (RPM)')}<input name="rpm" className="mono" inputMode="numeric" defaultValue={policy.limits.rpm ?? ''} placeholder={t('제한 없음')} /></label>
        <label>{t('분당 토큰 수 한도 (TPM)')}<input name="tpm" className="mono" inputMode="numeric" defaultValue={policy.limits.tpm ?? ''} placeholder={t('제한 없음')} /></label>
        <label>{t('분당 토큰 계산 요청 수 한도 (Count RPM)')}<input name="count_rpm" className="mono" inputMode="numeric" defaultValue={policy.limits.count_rpm ?? ''} placeholder={t('제한 없음')} /></label>
      </div>
    </fieldset>
    <label>{t('정책 변경 사유')}<input name="reason" required maxLength={1000} /></label>
    <details><summary>{t('보안 설정 fence 해제 진술')}</summary><div className="stack"><p className="meta">{t('fence 설정 사유는 security_config_fence:승인참조입니다. 구 프로세스 종료 확인 전 해제하지 마세요.')}</p><label>{t('Fence 참조')}<input name="fence_ref" className="mono" spellCheck={false} /></label><label>{t('구 프로세스 종료 확인 참조')}<input name="stopped_ref" className="mono" spellCheck={false} /></label><label className="attest"><input name="fence_confirmed" type="checkbox" />{t('구 설정 프로세스 종료를 확인했습니다')}</label></div></details>
    <div className="actions"><button className="btn-primary">{t('정책 교체 확인')}</button></div>
  </fieldset></form>;
}
