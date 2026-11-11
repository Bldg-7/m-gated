import type { Usage } from '../../lib/types';
export function money(value: string | null): string { return value === null ? '미확인' : `${value} USD`; }
export function UsageView({ usage }: { usage: Usage }) {
  return <>
    <section aria-labelledby="period-heading"><h2 id="period-heading">조회 기간 사용량</h2>
      <p>{usage.from} ~ {usage.to} (끝 시각 제외)</p><p>관측: {usage.as_of}</p>
      <p>집계 모드: {usage.aggregation === 'detail' ? '상세' : '보관된 기간 합계'}</p>
      {usage.detail_available_from && <p>상세 가용 시작: {usage.detail_available_from}</p>}
      <dl><dt>확정 비용</dt><dd>{money(usage.settled_usd)}</dd><dt>유지 예약</dt><dd>{money(usage.held_reservation_usd)}</dd><dt>기간 내 미정산</dt><dd>{usage.period_unknown_count}</dd></dl>
      <p role={usage.block_state?.blocked ? 'status' : undefined}>전체 기간 미정산: {usage.global_unknown_count} / 만료된 과금 실행: {usage.global_expired_dispatch_count}</p>
      {usage.block_state?.blocked && <p>과금 호출 차단: {usage.block_state.reasons.join(', ')}. 조회 기간이나 모델을 바꿔도 해제되지 않습니다.</p>}
    </section>
    <section aria-labelledby="balance-heading"><h2 id="balance-heading">현재 scope별 예산 잔액</h2><p>아래 잔액은 조회 필터와 무관한 현재 예산 기간의 서버 관측입니다. 다음 호출 허가를 보장하지 않습니다.</p>
      {usage.balance_context === 'aggregate_no_balance' && <p>전체 합계: 주체별 예산을 합산한 잔액은 제공하지 않습니다.</p>}{usage.budget_balances.map(b => <article key={b.scope_id}><h3>{b.scope_kind === 'principal' ? '주체' : `선택 키 ${b.key_id}`}</h3><p>{b.period_start} ~ {b.period_end} / {b.timezone}</p><p>관측: {b.as_of}, 정책 버전: {b.policy_version}</p><p>scope ID: {b.scope_id} · 설정 지문: {b.config_digest}</p><p>이 scope 미정산 {b.scope_unknown_count} · 주체 전체 미정산 {b.principal_unknown_count}</p>
        <dl><dt>실효 한도</dt><dd>{b.limit_usd === null ? '별도 한도 없음' : money(b.limit_usd)}</dd><dt>현재 확정액</dt><dd>{money(b.settled_usd)}</dd><dt>현재 예약액</dt><dd>{money(b.reserved_usd)}</dd><dt>잔액</dt><dd>{b.balance_state === 'unresolved' ? '확인 불가 (미정산)' : b.balance_state === 'unlimited' ? '한도 없음' : money(b.remaining_usd)}</dd></dl>
        <p>{b.mode === 'actual' ? '실사용량 모드: 진행 호출 때문에 초과할 수 있으며 하드캡 보장이 아닙니다.' : '예약 모드: 확정액과 유지 예약을 뺀 잔액입니다.'}</p>
        {b.blocked && <p>차단 사유: {b.reasons.join(', ')}</p>}
      </article>)}
    </section>
    <section><h2>그룹별 사용량</h2>{usage.buckets.items.length === 0 ? <p>그룹별 사용량이 없습니다. 그룹을 선택하지 않았거나 일치한 데이터가 없습니다.</p> : <table><thead><tr><th>그룹</th><th>확정 비용</th><th>유지 예약</th><th>미정산</th><th>입력 / 출력 token</th></tr></thead><tbody>{usage.buckets.items.map(b => <tr key={JSON.stringify(b.group)}><th>{b.group.kind === 'key' ? b.group.key_id ?? 'SSO (키 없음)' : `${b.group.provider}${b.group.model ? ` / ${b.group.model}` : ''}`}</th><td>{money(b.settled_usd)}</td><td>{money(b.held_reservation_usd)}</td><td>{b.unknown_count}</td><td>{b.tokens_complete ? `${b.input_tokens} / ${b.output_tokens}` : '미확인 (일부 token만으로 전체를 표시하지 않음)'}</td></tr>)}</tbody></table>}<p>페이지의 행을 합산해 전체 비용으로 사용하지 않습니다.</p></section>
  </>;
}
