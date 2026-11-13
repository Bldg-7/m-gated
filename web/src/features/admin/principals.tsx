import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { session } from '../../lib/api';
import { msgid } from '../../lib/i18n';
import { Link } from '../../lib/router';
import type { Page, Grant, Limits } from '../../lib/types';
import { Pill, Select, StatusBadge, TextArea } from '../../ui/ui';
import { KeysScreen } from '../keys/keys';
import type { PrincipalDetail, PrincipalSummary } from './types';
import { directId, fields, ReadState, useCommand, useResource } from './shared';
export function PrincipalsScreen() {
  const { t } = useTranslation();
  const [query, setQuery] = useState(''), [id, setId] = useState(directId('principal_id'));
  const page = useResource<Page<PrincipalSummary>>(`/gateway/admin/principals${query ? `?${query}` : ''}`);
  const create = useCommand(async ack => { if (ack) { await session.read<PrincipalDetail>(`/gateway/admin/principals/${encodeURIComponent(ack.target.id)}`); setId(ack.target.id); } await page.reload(); });
  return <div className="stack-lg">
    <p className="callout callout-neutral">{t('owner는 책임자 메타데이터이며 서비스 접근·관리 권한을 자동 부여하지 않습니다.')}</p>
    <section aria-labelledby="principal-list-heading" className="stack">
      <h2 id="principal-list-heading">{t('주체 목록')}</h2>
      <form aria-label={t('주체 조회')} className="filters" onSubmit={e => { const f = fields(e), q = new URLSearchParams(); for (const key of ['kind', 'disabled', 'limit']) if (f.get(key)) q.set(key, String(f.get(key))); setQuery(q.toString()); if (q.toString() === query) void page.reload().catch(() => {}); }}>
        <div className="field"><label htmlFor="principal-kind">{t('주체 종류')}</label><Select id="principal-kind" name="kind"><option value="">{t('전체')}</option><option value="human">{t('사람')}</option><option value="service">{t('서비스')}</option></Select></div>
        <div className="field"><label htmlFor="principal-disabled">{t('계정 정지')}</label><Select id="principal-disabled" name="disabled"><option value="">{t('전체')}</option><option value="true">{t('정지')}</option><option value="false">{t('활성')}</option></Select></div>
        <label>{t('주체 페이지 크기')}<input name="limit" type="number" min="1" max="100" /></label>
        <div className="actions"><button className="btn-primary">{t('주체 조회')}</button></div>
      </form>
      <ReadState {...page} asOf={page.data?.as_of} />
      {page.data && <>
        {!page.data.items.length && <p className="empty">{t('주체가 없습니다.')}</p>}
        {page.data.items.length > 0 && <ul className="list">{page.data.items.map(p => <li key={p.id}>
          <div className="list-main">
            <div className="row"><span className="list-title">{p.display_name}</span><Pill>{p.kind === 'service' ? t('서비스') : t('사람')}</Pill>{p.block_state.reasons.map(reason => <StatusBadge key={reason} kind="block" value={reason} />)}</div>
            <p className="meta"><span className="mono">{p.id}</span> · {p.kind} · {p.email ?? t('이메일 없음')} · owner <span className="mono">{p.management_owner_id ?? t('없음')}</span></p>
            <p className="meta">{t('주체 버전 {{principal}} / 정책 버전 {{policy}} / 안전 버전 {{safety}}', { principal: p.version, policy: p.policy_version, safety: p.inference_safety.version })} · {p.disabled ? t('계정 정지') : t('계정 활성')} · {p.inference_safety.paused ? t('자동 안전차단') : t('안전차단 없음')} · {t('차단')} {p.block_state.reasons.join(', ')}</p>
          </div>
          <div className="list-side"><button className="btn-sm" aria-label={t('{{name}} 상세', { name: p.display_name })} onClick={() => setId(p.id)}>{t('상세')}</button></div>
        </li>)}</ul>}
        {page.data.next_cursor && <div className="actions"><button onClick={() => setQuery(`cursor=${encodeURIComponent(page.data!.next_cursor!)}`)}>{t('다음 주체 페이지')}</button></div>}
      </>}
    </section>
    <div className="grid-2">
      <form aria-label={t('주체 ID 조회')} className="card" onSubmit={e => setId(String(fields(e).get('id')))}><label>{t('주체 ID')}<input name="id" className="mono" spellCheck={false} required /></label><div className="actions"><button>{t('상세 조회')}</button></div></form>
      <div className="stack">
        <form aria-label={t('서비스 생성')} onSubmit={e => { const f = fields(e); create.prepare({ label: msgid('deny-all 서비스 생성 확인'), url: '/gateway/admin/principals', method: 'POST', body: { display_name: String(f.get('display_name')), management_owner_id: String(f.get('owner')) } }); }}><fieldset disabled={create.busy} className="group"><legend>{t('서비스 생성 (초기 deny-all)')}</legend><label>{t('새 서비스 이름')}<input name="display_name" required maxLength={100} /></label><label>{t('새 서비스 owner ID')}<input name="owner" className="mono" spellCheck={false} required /></label><div className="actions"><button className="btn-primary">{t('서비스 생성 확인')}</button></div></fieldset></form>{create.view}
      </div>
    </div>
    <details><summary>{t('전체 키 조회·회수')}</summary><KeysScreen adminAll /></details>
    {id && <PrincipalEditor key={id} id={id} />}
  </div>;
}
function PrincipalEditor({ id }: { id: string }) {
  const { t } = useTranslation();
  const detail = useResource<PrincipalDetail>(`/gateway/admin/principals/${encodeURIComponent(id)}`);
  const command = useCommand(async () => { await detail.reload(); });
  const [formError, setFormError] = useState<string | null>(null), p = detail.data?.principal;
  return <section aria-label={t('주체 상세')} className="stack">
    <h2>{t('주체 상세')}</h2>
    <ReadState {...detail} />{formError && <p role="alert">{formError}</p>}{command.view}{p && <>
    <div className={p.block_state.reasons.length ? 'card card-critical' : 'card'}>
      <div className="stack-sm">
        <div className="row"><h3>{p.display_name}</h3><Pill>{p.kind === 'service' ? t('서비스') : t('사람')}</Pill>{p.block_state.reasons.map(reason => <StatusBadge key={reason} kind="block" value={reason} />)}</div>
        <p className="meta"><span className="mono">{p.id}</span> · {p.kind} · {t('주체 버전 {{principal}} · 정책 버전 {{policy}} · 안전 버전 {{safety}}', { principal: p.version, policy: p.policy.version, safety: p.inference_safety.version })}</p>
        <p className="meta">owner: <span className="mono">{p.management_owner_id ?? t('없음')}</span> · {t('과금 차단')}: {p.block_state.reasons.join(', ') || t('없음')} · {t('전체 미정산 {{count}}', { count: p.block_state.global_unknown_count })}</p>
      </div>
      <details><summary>{t('실행 설정 상한 (편집 불가)')}</summary><pre>{JSON.stringify(detail.data!.execution_ceiling, null, 2)}</pre></details>
      <div className="actions"><Link className="btn btn-sm" href={`/ui/admin/usage?principal_id=${encodeURIComponent(p.id)}`}>{t('이 주체 사용량·현재 잔액')}</Link><Link className="btn btn-sm" href={`/ui/admin/requests?principal_id=${encodeURIComponent(p.id)}`}>{t('이 주체 미정산')}</Link></div>
    </div>
    <div className="grid-2">
      {p.kind === 'service' && <form key={`principal-${p.version}`} aria-label={t('서비스 변경')} onSubmit={e => { const f = fields(e); command.prepare({ label: msgid('서비스 변경 확인'), url: `/gateway/admin/principals/${p.id}`, method: 'PATCH', body: { expected_version: p.version, display_name: String(f.get('display_name')), management_owner_id: String(f.get('owner')) } }); }}><fieldset disabled={command.busy} className="group"><legend>{t('서비스 변경')}</legend><label>{t('서비스 이름')}<input name="display_name" defaultValue={p.display_name} required maxLength={100} /></label><label>{t('서비스 owner ID')}<input name="owner" className="mono" spellCheck={false} defaultValue={p.management_owner_id ?? ''} required /></label><div className="actions"><button className="btn-primary">{t('서비스 변경 확인')}</button></div></fieldset></form>}
      <section aria-label={t('자동 안전차단')} className={p.inference_safety.paused ? 'card card-critical' : 'card'}><h3>{t('자동 과금 안전차단')}</h3><p className="meta">{t('{{state}} · 안전 버전 {{version}} · 사유 {{reason}} · 시각 {{at}} · 원인 요청 {{request}}', { state: p.inference_safety.paused ? t('차단 중') : t('해제 상태'), version: p.inference_safety.version, reason: p.inference_safety.reason ?? t('없음'), at: p.inference_safety.paused_at ?? t('없음'), request: p.inference_safety.trigger_request_id ?? t('없음') })}</p><p className="meta">{t('별도 해제는 unknown·계정 정지·fence·예산·키 회수를 해제하지 않습니다.')}</p><form key={`safety-${p.inference_safety.version}`} aria-label={t('명시 안전차단 해제')} onSubmit={e => { const f = fields(e); if (!f.has('confirmed')) return; command.prepare({ label: msgid('별도 안전차단 해제 확인'), url: `/gateway/admin/principals/${p.id}/inference-pause/release`, method: 'POST', body: { expected_version: p.inference_safety.version, reason: String(f.get('reason')) } }); }}><fieldset disabled={command.busy} className="stack"><label>{t('안전차단 해제 사유')}<input name="reason" required maxLength={1000} /></label><label className="attest"><input name="confirmed" type="checkbox" required />{t('과금 원인과 남은 차단을 확인했습니다')}</label><div className="actions"><button className="btn-primary">{t('명시 해제 확인')}</button></div></fieldset></form></section>
    </div>
    <form key={`policy-${p.policy.version}`} aria-label={t('정책 교체')} onSubmit={e => {
      const f = fields(e); setFormError(null);
      try {
        const grants = JSON.parse(String(f.get('grants'))) as Grant[], limits = JSON.parse(String(f.get('limits'))) as Limits;
        if (!Array.isArray(grants) || typeof limits !== 'object' || !limits) throw new Error('shape');
        const body: Record<string, unknown> = { expected_version: p.policy.version, disabled: f.has('disabled'), grants, limits, reason: String(f.get('reason')) };
        if (f.has('fence_confirmed')) body.fence_release = { fence_ref: String(f.get('fence_ref')), old_processes_stopped_ref: String(f.get('stopped_ref')), confirmed: true };
        command.prepare({ label: msgid('정책 전체 교체 확인'), url: `/gateway/admin/principals/${p.id}/policy`, method: 'PUT', body });
      } catch { setFormError(t('권한 배열과 Limits 객체를 유효한 JSON으로 입력하세요.')); }
    }}><fieldset disabled={command.busy} className="group"><legend>{t('정책 전체 교체')}</legend><p className="meta">{t('서버 DTO를 그대로 편집합니다. Amount는 소수점 12자리 문자열, budget=null은 실행 상한 우회가 아닙니다. 자동 안전차단은 이 폼으로 해제되지 않습니다.')}</p><label><input type="checkbox" name="disabled" defaultChecked={p.policy.disabled} />{t('관리용 계정 정지')}</label><div className="grid-2"><div className="field"><label htmlFor="policy-grants">Grant JSON</label><TextArea id="policy-grants" className="mono" spellCheck={false} name="grants" rows={10} expandLabel="Grant JSON" defaultValue={JSON.stringify(p.policy.grants, null, 2)} required /></div><div className="field"><label htmlFor="policy-limits">Limits JSON</label><TextArea id="policy-limits" className="mono" spellCheck={false} name="limits" rows={10} expandLabel="Limits JSON" defaultValue={JSON.stringify(p.policy.limits, null, 2)} required /></div></div><label>{t('정책 변경 사유')}<input name="reason" required maxLength={1000} /></label><details><summary>{t('보안 설정 fence 해제 진술')}</summary><div className="stack"><p className="meta">{t('fence 설정 사유는 security_config_fence:승인참조입니다. 구 프로세스 종료 확인 전 해제하지 마세요.')}</p><label>{t('Fence 참조')}<input name="fence_ref" className="mono" spellCheck={false} /></label><label>{t('구 프로세스 종료 확인 참조')}<input name="stopped_ref" className="mono" spellCheck={false} /></label><label className="attest"><input name="fence_confirmed" type="checkbox" />{t('구 설정 프로세스 종료를 확인했습니다')}</label></div></details><div className="actions"><button className="btn-primary">{t('정책 교체 확인')}</button></div></fieldset></form>
    <div className="card"><KeysScreen key={`${p.id}:${session.snapshot().epoch}`} adminPrincipal={{ id: p.id, service: p.kind === 'service', grants: p.policy.grants }} /></div>
  </>}</section>;
}
