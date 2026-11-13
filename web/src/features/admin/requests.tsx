import { principalHref } from './principals';
import { Fragment, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { msgid } from '../../lib/i18n';
import { Link } from '../../lib/router';
import type { Page } from '../../lib/types';
import { Money, Select, StatusBadge, TextArea } from '../../ui/ui';
import { directId, fields, ReadState, useCommand, useResource } from './shared';
import type { RequestDetail, RequestSummary } from './types';
const providers = [['anthropic', 'Anthropic'], ['openai', 'OpenAI'], ['deepseek', 'DeepSeek']];
const phases = [['admitted', msgid('입장')], ['dispatching', msgid('전송 중')], ['finished', msgid('종료')]];
const outcomes = [['completed', msgid('완료')], ['rejected', msgid('거절')], ['failed', msgid('실패')], ['cancelled', msgid('취소됨')]];
const accounting = [['unknown', msgid('미정산')], ['pending', msgid('정산 대기')], ['settled', msgid('확정')]];
export function RequestsScreen() {
  const { t } = useTranslation();
  const options = (list: string[][]) => list.map(([value, label]) => <option key={value} value={value}>{t(label)} ({value})</option>);
  const initial = directId('principal_id');
  const [query, setQuery] = useState(initial ? `principal_id=${encodeURIComponent(initial)}` : ''), [id, setId] = useState(directId('request_id'));
  const page = useResource<Page<RequestSummary>>(`/gateway/admin/requests${query ? `?${query}` : ''}`);
  return <div className="stack-lg">
    <form aria-label={t('요청 조회')} className="filters" onSubmit={e => { const f = fields(e), q = new URLSearchParams(); for (const key of ['principal_id', 'key_id', 'provider', 'model', 'phase', 'outcome', 'accounting_state', 'from', 'to', 'limit']) if (f.get(key)) q.set(key, String(f.get(key))); if (q.toString() === query) void page.reload().catch(() => {}); else setQuery(q.toString()); }}>
      <div className="field"><label htmlFor="request-accounting">{t('회계 상태')}</label><Select id="request-accounting" name="accounting_state" defaultValue="unknown">{options(accounting)}</Select></div>
      <div className="field"><label htmlFor="request-provider">{t('요청 Provider')}</label><Select id="request-provider" name="provider"><option value="">{t('전체')}</option>{providers.map(([value, label]) => <option key={value} value={value}>{label}</option>)}</Select></div>
      <div className="field"><label htmlFor="request-phase">{t('실행 단계')}</label><Select id="request-phase" name="phase"><option value="">{t('전체')}</option>{options(phases)}</Select></div>
      <div className="field"><label htmlFor="request-outcome">{t('결과')}</label><Select id="request-outcome" name="outcome"><option value="">{t('전체')}</option>{options(outcomes)}</Select></div>
      <label>{t('요청 사용자 ID')}<input name="principal_id" className="mono" spellCheck={false} defaultValue={initial} /></label>
      <label>{t('요청 키 ID')}<input name="key_id" className="mono" spellCheck={false} /></label>
      <label>{t('요청 모델')}<input name="model" maxLength={200} /></label>
      <label>{t('요청 시작 (RFC3339)')}<input name="from" className="mono" spellCheck={false} placeholder="2026-11-01T00:00:00Z" /></label>
      <label>{t('요청 종료 (RFC3339)')}<input name="to" className="mono" spellCheck={false} /></label>
      <label>{t('요청 페이지 크기')}<input name="limit" type="number" min="1" max="100" /></label>
      <div className="actions"><button className="btn-primary">{t('요청 조회')}</button></div>
    </form>
    <section aria-labelledby="request-list-heading" className="stack">
      <h2 id="request-list-heading">{t('요청 목록')}</h2>
      <ReadState {...page} asOf={page.data?.as_of} />
      {page.data && <>
        {!page.data.items.length && <p className="empty">{t('일치하는 요청이 없습니다.')}</p>}
        {page.data.items.length > 0 && <ul className="list">{page.data.items.map(r => <li key={r.id}>
          <div className="list-main">
            <span className="list-title mono">{r.id}</span>
            <p className="meta">{r.provider} / <span className="mono">{r.model}</span> / {r.api}{r.provider_credential?.label && <> · {t('회사 키')} {r.provider_credential.label}</>} · {r.purpose} · version {r.version} · <span className="mono">{r.admitted_at}</span></p>
          </div>
          <div className="list-side">{r.outcome ? <StatusBadge kind="outcome" value={r.outcome} /> : <StatusBadge kind="phase" value={r.phase} />}<StatusBadge kind="accounting" value={r.accounting_state} /><Money value={r.cost_usd} /><button className="btn-sm" aria-label={t('{{name}} 상세', { name: r.id })} onClick={() => setId(r.id)}>{t('상세')}</button></div>
        </li>)}</ul>}
        {page.data.next_cursor && <div className="actions"><button onClick={() => setQuery(`cursor=${encodeURIComponent(page.data!.next_cursor!)}`)}>{t('다음 요청 페이지')}</button></div>}
      </>}
    </section>
    <form aria-label={t('요청 ID 조회')} className="filters" onSubmit={e => setId(String(fields(e).get('id')))}><label>{t('요청 ID')}<input name="id" className="mono" spellCheck={false} required /></label><div className="actions"><button>{t('요청 상세 조회')}</button></div></form>
    {id && <RequestEditor id={id} key={id} />}
  </div>;
}
function RequestEditor({ id }: { id: string }) {
  const { t } = useTranslation();
  const detail = useResource<RequestDetail>(`/gateway/admin/requests/${encodeURIComponent(id)}`);
  const command = useCommand(async () => { await detail.reload(); });
  const [error, setError] = useState<string | null>(null), r = detail.data;
  return <section aria-label={t('요청 상세')} className="stack">
    <h2>{t('요청 상세')}</h2>
    <ReadState {...detail} />{command.view}{error && <p role="alert">{error}</p>}{r && <>
    <div className="grid-2">
      <div className={r.accounting_state === 'unknown' ? 'card card-critical' : 'card'}>
        <div className="stack-sm">
          <div className="row">{r.outcome ? <StatusBadge kind="outcome" value={r.outcome} /> : <StatusBadge kind="phase" value={r.phase} />}<StatusBadge kind="accounting" value={r.accounting_state} /><span className="pill">{t('버전 {{version}}', { version: r.version })}</span></div>
          <p className="mono">{r.id}</p>
        </div>
        <dl className="kv">
          <dt>{t('사용자')}</dt><dd><Link className="mono" href={principalHref(r.principal_id)}>{r.principal_id}</Link></dd>
          <dt>{t('키')}</dt><dd className="mono">{r.key_id ?? 'SSO'}</dd>
          <dt>{t('회사 키')}</dt><dd>{r.provider_credential ? <>{r.provider_credential.label ?? t('알 수 없음')} <span className="mono meta">(…{r.provider_credential.hint})</span></> : t('기록 없음')}</dd>
          <dt>{t('부모 요청')}</dt><dd className="mono">{r.parent_request_id ?? t('없음')}</dd>
          <dt>{t('상태')}</dt><dd>{r.phase} / {r.outcome ?? t('결과 미확인')} / {r.accounting_state}</dd>
          <dt>{t('비용')}</dt><dd><Money value={r.cost_usd} /></dd>
          <dt>{t('사유')}</dt><dd className="mono">{r.reason_code ?? t('없음')}</dd>
          <dt>{t('입장 · 마감')}</dt><dd className="mono">{r.admitted_at} · {r.finished_at ?? t('미종료')}</dd>
          <dt>executor</dt><dd className="mono">{r.executor_id}</dd>
          <dt>{t('runtime 실행 종료')}</dt><dd>{r.execution_stopped_at ? <span className="mono">{r.execution_stopped_at}</span> : <span className="text-warning">{t('기록 없음 (lease 만료는 종료 증명이 아님)')}</span>}</dd>
          <dt>{t('Provider 요청')}</dt><dd><span className="mono">{r.provider_request_id ?? t('없음')}</span> · HTTP {r.http_status ?? t('미확인')}</dd>
        </dl>
        <details><summary>{t('고정 가격·실행 snapshot·원래 예산 귀속')}</summary><pre>{JSON.stringify({ pricing_snapshot: r.pricing_snapshot, execution_snapshot: r.execution_snapshot, budgets: r.budgets }, null, 2)}</pre></details>
      </div>
      <div className="stack">
        {r.settlement && <section aria-label={t('수동 처리 기록')} className="card"><h3>{t('관리자 수동 입력 (Provider 검증값 아님)')}</h3><dl className="kv"><dt>{t('출처')}</dt><dd className="mono">{r.settlement.source}</dd><dt>{t('처리자')}</dt><dd className="mono">{r.settlement.actor_principal_id}</dd><dt>{t('처리 시각')}</dt><dd className="mono">{r.settlement.settled_at}</dd><dt>{t('결정')}</dt><dd>{r.settlement.decision}</dd><dt>{t('입력 금액')}</dt><dd><Money value={r.settlement.cost_usd} /></dd><dt>{t('사유')}</dt><dd>{r.settlement.reason}</dd><dt>{t('종료 확인 출처')}</dt><dd>{r.settlement.execution_stop_source === 'runtime' ? t('runtime 종료 기록') : t('관리자 종료 확인 진술 (원격 실행 검증 아님)')}</dd><dt>{t('대상 executor')}</dt><dd className="mono">{r.settlement.executor_id}</dd></dl></section>}
        {r.accounting_state === 'unknown' && <form key={r.version} aria-label={t('수동 정산')} onSubmit={e => { const f = fields(e), amount = String(f.get('cost_usd')), decision = String(f.get('decision')); setError(null); if (!/^(0|[1-9][0-9]{0,15})\.[0-9]{12}$/.test(amount) || (decision === 'no_charge' && amount !== '0.000000000000')) { setError(t('소수점 12자리 USD를 입력하세요. 무과금은 0.000000000000입니다.')); return; } const body: Record<string, unknown> = { expected_version: r.version, decision, cost_usd: amount, reason: String(f.get('reason')) }; if (f.has('confirmed')) body.execution_stopped_confirmed = true; command.prepare({ label: msgid('관리자 수동 금액 반영 확인'), url: `/gateway/admin/requests/${r.id}/settlement`, method: 'PUT', body, danger: true }); }}><fieldset disabled={command.busy} className="group"><legend>{t('관리자 수동 정산')}</legend><p className="meta">{t('금액·결정·사유만 기록합니다. Provider 청구 검증·증빙 수집이 아닙니다. 미확인 token은 null을 유지합니다. 다른 unknown·정지·자동 안전차단은 해제하지 않습니다.')}</p>
          <div className="form-grid"><div className="field"><label htmlFor="settlement-decision">{t('처리 결정')}</label><Select id="settlement-decision" name="decision" required defaultValue=""><option value="" disabled>{t('선택하세요')}</option><option value="charged">{t('과금')}</option><option value="no_charge">{t('무과금')}</option></Select></div><label>{t('처리 금액 USD')}<input name="cost_usd" className="amount" inputMode="decimal" spellCheck={false} required placeholder="0.000000000000" /></label></div>
          <div className="field"><label htmlFor="settlement-reason">{t('수동 처리 사유')}</label><TextArea id="settlement-reason" name="reason" rows={4} required maxLength={1000} /></div>
          {!r.execution_stopped_at && <label className="attest"><input type="checkbox" name="confirmed" required />{t('대상 executor 실행 종료를 확인했습니다 (관리자 진술)')}</label>}
          <div className="actions"><button className="btn-primary">{t('수동 정산 확인')}</button></div></fieldset></form>}
      </div>
    </div>
    <section aria-label={t('선택 usage')} className="card"><h3>{t('관측 출처와 token')}</h3><dl className="kv">{(['partial', 'final', 'late_final'] as const).map(kind => <Fragment key={kind}><dt className="mono">{kind}</dt><dd>{r.usage[kind] ? <div className="stack-sm"><span>{t('출처: {{source}} · 입력 {{input}} / 출력 {{output}}', { source: r.usage[kind]!.source, input: r.usage[kind]!.input_tokens ?? t('미확인'), output: r.usage[kind]!.output_tokens ?? t('미확인') })}</span><pre>{JSON.stringify(r.usage[kind], null, 2)}</pre></div> : <span className="meta">{t('관측 없음')}</span>}</dd></Fragment>)}</dl></section>
  </>}</section>;
}
