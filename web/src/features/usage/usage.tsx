import { useTranslation } from 'react-i18next';
import type { Balance, Usage } from '../../lib/types';
import { Money } from '../../ui/ui';
export function money(value: string | null): string { return value === null ? '미확인' : `${value} USD`; }
/** 한도 대비 비율 막대. 장식용(aria-hidden)이며 적힌 금액은 모두 서버 값이다. */
function Meter({ balance: b }: { balance: Balance }) {
  const { t } = useTranslation();
  if (b.balance_state === 'unresolved') return <div className="stack-sm" aria-hidden="true"><div className="meter"><span className="meter-unresolved" /></div><span className="meta">{t('미정산이 있어 잔액 비율을 표시하지 않습니다.')}</span></div>;
  const limit = b.limit_usd === null ? NaN : Number(b.limit_usd);
  if (b.balance_state !== 'limited' || !(limit > 0)) return null;
  const share = (value: string) => Math.max(0, Math.min(100, (Number(value) / limit) * 100)) || 0;
  const settled = share(b.settled_usd), reserved = Math.min(100 - settled, share(b.reserved_usd));
  return <div className="stack-sm" aria-hidden="true">
    <div className="meter"><span className="meter-settled" style={{ width: `${settled}%` }} /><span className="meter-reserved" style={{ width: `${reserved}%` }} /></div>
    <div className="legend"><span><i className="meter-settled" />{t('확정')}</span><span><i className="meter-reserved" />{t('예약')}</span><span><i className="meter-rest" />{t('잔액')}</span></div>
  </div>;
}
export function UsageView({ usage }: { usage: Usage }) {
  const { t } = useTranslation();
  const blocked = !!usage.block_state?.blocked;
  return <>
    <section aria-labelledby="period-heading" className="stack">
      <div className="stack-sm"><h2 id="period-heading">{t('조회 기간 사용량')}</h2>
        <p className="meta"><span className="mono">{usage.from} ~ {usage.to}</span> {t('(끝 시각 제외)')} · {t('관측')}: <span className="mono">{usage.as_of}</span> · {t('집계 모드')}: {usage.aggregation === 'detail' ? t('상세') : t('보관된 기간 합계')}</p>
        {usage.detail_available_from && <p className="meta">{t('상세 가용 시작')}: <span className="mono">{usage.detail_available_from}</span></p>}
      </div>
      <dl className="stats">
        <div className="stat"><dt>{t('확정 비용')}</dt><dd><Money value={usage.settled_usd} /></dd></div>
        <div className="stat"><dt>{t('유지 예약')}</dt><dd><Money value={usage.held_reservation_usd} /></dd></div>
        <div className={usage.period_unknown_count === '0' ? 'stat' : 'stat stat-critical'}><dt>{t('기간 내 미정산')}</dt><dd>{usage.period_unknown_count}</dd></div>
      </dl>
      <div className={blocked ? 'callout callout-critical' : undefined}><div className="stack-sm">
        <p role={blocked ? 'status' : undefined} className={blocked ? 'plain' : 'meta'}>{t('전체 기간 미정산: {{unknown}} / 만료된 과금 실행: {{expired}}', { unknown: usage.global_unknown_count, expired: usage.global_expired_dispatch_count })}</p>
        {usage.block_state?.blocked && <p className="plain">{t('과금 호출 차단: {{reasons}}. 조회 기간이나 모델을 바꿔도 해제되지 않습니다.', { reasons: usage.block_state.reasons.join(', ') })}</p>}
      </div></div>
    </section>
    <section aria-labelledby="balance-heading" className="stack">
      <div className="stack-sm"><h2 id="balance-heading">{t('현재 scope별 예산 잔액')}</h2><p className="lead">{t('아래 잔액은 조회 필터와 무관한 현재 예산 기간의 서버 관측입니다. 다음 호출 허가를 보장하지 않습니다.')}</p></div>
      {usage.balance_context === 'aggregate_no_balance' && <p className="callout callout-neutral">{t('전체 합계: 주체별 예산을 합산한 잔액은 제공하지 않습니다.')}</p>}
      {usage.budget_balances.length > 0 && <div className="grid-cards">{usage.budget_balances.map(b => <article key={b.scope_id} className={b.balance_state === 'unresolved' || b.blocked ? 'card card-critical' : 'card'}>
        <div className="stack-sm">
          <div className="row row-between"><h3>{b.scope_kind === 'principal' ? t('주체') : t('선택 키 {{key}}', { key: b.key_id })}</h3><span className={b.mode === 'actual' ? 'pill tone-warning' : 'pill'}>{b.mode === 'actual' ? t('실사용량') : t('예약 기반')}</span></div>
          <p className="meta"><span className="mono">{b.period_start} ~ {b.period_end}</span> / {b.timezone}</p>
        </div>
        <Meter balance={b} />
        <dl className="kv kv-figures">
          <dt>{t('실효 한도')}</dt><dd>{b.limit_usd === null ? t('별도 한도 없음') : <Money value={b.limit_usd} />}</dd>
          <dt>{t('현재 확정액')}</dt><dd><Money value={b.settled_usd} /></dd>
          <dt>{t('현재 예약액')}</dt><dd><Money value={b.reserved_usd} /></dd>
          <dt>{t('잔액')}</dt><dd>{b.balance_state === 'unresolved' ? <span className="money-unknown">{t('확인 불가 (미정산)')}</span> : b.balance_state === 'unlimited' ? t('한도 없음') : <Money value={b.remaining_usd} />}</dd>
        </dl>
        <p className={b.mode === 'actual' ? 'callout callout-warning' : 'meta'}>{b.mode === 'actual' ? t('실사용량 모드: 진행 호출 때문에 초과할 수 있으며 하드캡 보장이 아닙니다.') : t('예약 모드: 확정액과 유지 예약을 뺀 잔액입니다.')}</p>
        {b.blocked && <p className="callout callout-critical">{t('차단 사유: {{reasons}}', { reasons: b.reasons.join(', ') })}</p>}
        <div className="stack-sm">
          <p className="meta">{t('이 scope 미정산 {{scope}} · 주체 전체 미정산 {{principal}}', { scope: b.scope_unknown_count, principal: b.principal_unknown_count })}</p>
          <p className="meta">{t('관측')}: <span className="mono">{b.as_of}</span>, {t('정책 버전')}: {b.policy_version}</p>
          <p className="meta">scope ID: <span className="mono">{b.scope_id}</span> · {t('설정 지문')}: <span className="mono">{b.config_digest}</span></p>
        </div>
      </article>)}</div>}
    </section>
    <section aria-labelledby="group-heading" className="stack"><h2 id="group-heading">{t('그룹별 사용량')}</h2>
      {usage.buckets.items.length === 0 ? <p className="empty">{t('그룹별 사용량이 없습니다. 그룹을 선택하지 않았거나 일치한 데이터가 없습니다.')}</p> : <div className="table-wrap"><table><thead><tr><th>{t('그룹')}</th><th className="num">{t('확정 비용')}</th><th className="num">{t('유지 예약')}</th><th className="num">{t('미정산')}</th><th className="num">{t('입력 / 출력 token')}</th></tr></thead><tbody>{usage.buckets.items.map(b => <tr key={JSON.stringify(b.group)}><th scope="row">{b.group.kind === 'key' ? <span className="mono">{b.group.key_id ?? t('SSO (키 없음)')}</span> : <>{b.group.provider}{b.group.model && <> / <span className="mono">{b.group.model}</span></>}</>}</th><td className="num"><Money value={b.settled_usd} /></td><td className="num"><Money value={b.held_reservation_usd} /></td><td className={b.unknown_count === '0' ? 'num mono' : 'num mono text-critical'}>{b.unknown_count}</td><td className="num">{b.tokens_complete ? <span className="mono">{b.input_tokens} / {b.output_tokens}</span> : <span className="meta">{t('미확인 (일부 token만으로 전체를 표시하지 않음)')}</span>}</td></tr>)}</tbody></table></div>}
      <p className="meta">{t('페이지의 행을 합산해 전체 비용으로 사용하지 않습니다.')}</p>
    </section>
  </>;
}
