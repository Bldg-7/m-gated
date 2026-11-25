import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { msgid } from '../../lib/i18n';
import { Money, Select, Time } from '../../ui/ui';
import { ReadState, useResource } from './shared';
import type { TokenCheckReason, TokenCheckView } from './types';

/** Provider가 더 많이 센 토큰의 까닭. 순서와 색 자리(style.css의 .reason-N)는 고정이다. 서버도 이 순서로 하나를 고른다. */
const reasons: { id: TokenCheckReason; label: string; note: string }[] = [
  { id: 'cutoff', label: msgid('끊긴 요청'), note: msgid('그 시간에 끊긴 요청이 있었습니다. 받은 만큼 규칙은 thinking 전체와 아직 받지 못한 출력을 세지 않습니다. 사후 보정을 켜면 줄어듭니다.') },
  { id: 'unknown', label: msgid('사용량을 모르는 요청'), note: msgid('그 시간에 사용량을 모르는 요청(미정산)이 있었습니다.') },
  { id: 'outside', label: msgid('게이트웨이 밖 사용'), note: msgid('그 시간에 게이트웨이가 그 API 키로 보낸 요청이 없습니다. 같은 키를 게이트웨이 밖에서 쓴 것으로 보입니다.') },
  { id: 'unexplained', label: msgid('원인 모름'), note: msgid('위 까닭에 해당하지 않습니다. 시간 경계에 걸친 요청을 Provider가 다른 시간에 넣었을 수 있습니다.') },
];
const errors: Record<string, string> = {
  admin_key_rejected: msgid('Anthropic이 Admin API 키를 거절했습니다. 키를 확인하세요.'),
  admin_api_unavailable: msgid('Anthropic Admin API를 호출하지 못했습니다. 다음 조회 때 다시 시도합니다.'),
  no_matched_keys: msgid('Anthropic API 키 목록에서 게이트웨이의 Provider 키를 하나도 찾지 못했습니다.'),
  database_unavailable: msgid('조회 결과를 DB에 쓰지 못했습니다. 다음 조회 때 다시 시도합니다.'),
  poll_timeout: msgid('조회가 4분 안에 끝나지 않았습니다. 다음 조회 때 다시 시도합니다.'),
};
const ranges = [['24h', msgid('최근 24시간')], ['7d', msgid('최근 7일')], ['30d', msgid('최근 30일')]];

/** 정수 문자열에 천 단위 쉼표를 넣는다. */
function tokens(value: string) { try { return BigInt(value).toLocaleString('en-US'); } catch { return value; } }
function ratio(part: string, whole: string) { const w = Number(whole); return w > 0 ? Number(part) / w : 0; }
function percent(value: number) { return value > 0 && value < 0.01 ? '<1%' : `${Math.round(value * 100)}%`; }

export function TokenCheckScreen() {
  const { t } = useTranslation();
  const [range, setRange] = useState('24h');
  const view = useResource<TokenCheckView>(`/api/admin/token-check?range=${range}`);
  // 기간을 바꿔 다시 읽는 동안에는 이전 화면을 흐리게 남겨 둔다(화면이 사라졌다 나타나지 않게).
  const [last, setLast] = useState<TokenCheckView | null>(null);
  useEffect(() => { if (view.data) setLast(view.data); }, [view.data]);
  const v = view.data ?? (view.loading ? last : null), stale = !view.data && !!v;
  return <div className="stack-lg">
    <div className="filters" role="group" aria-label={t('대조 기간')}>
      <div className="field"><label htmlFor="token-check-range">{t('기간')}</label><Select id="token-check-range" value={range} onChange={setRange} options={ranges.map(([value, label]) => ({ value, label: t(label) }))} /></div>
    </div>
    <ReadState {...view} />
    {v && !v.enabled && <div className="callout callout-neutral"><div className="callout-body"><strong>{t('토큰 대조가 꺼져 있습니다.')}</strong><span>{t('서버 환경변수 ANTHROPIC_ADMIN_API_KEY에 Anthropic Admin API 키를 넣으면 5분마다 사용량 보고서를 받아 비교합니다.')}</span></div></div>}
    {v?.enabled && <div className={stale ? 'stack-lg refetching' : 'stack-lg'} aria-busy={stale || undefined}><TokenCheckBody v={v} /></div>}
  </div>;
}

function TokenCheckBody({ v }: { v: TokenCheckView }) {
  const { t } = useTranslation();
  const more = v.totals.provider_more_tokens;
  const label = (id: TokenCheckReason) => t(reasons.find(r => r.id === id)?.label ?? id);
  return <>
    <section className="stack" aria-labelledby="token-check-state">
      <h2 id="token-check-state">{t('조회 상태')}</h2>
      <dl className="kv">
        <dt>{t('마지막 조회')}</dt><dd>{v.state.last_attempt_at ? <Time value={v.state.last_attempt_at} /> : t('아직 없음')}</dd>
        <dt>{t('마지막 성공')}</dt><dd>{v.state.last_success_at ? <Time value={v.state.last_success_at} /> : t('아직 없음')}</dd>
        {v.state.last_error && <><dt>{t('마지막 오류')}</dt><dd className="text-critical">{errors[v.state.last_error] ? t(errors[v.state.last_error]) : t('알 수 없는 오류입니다.')} <span className="mono">({v.state.last_error})</span></dd></>}
        <dt>{t('비교한 시간')}</dt><dd>{v.compared?.from && v.compared.to ? <><Time value={v.compared.from} /> ~ <Time value={v.compared.to} /></> : t('아직 받아 둔 사용량 보고서가 없습니다.')}</dd>
        <dt>{t('1분 칸 기준')}</dt><dd>{v.state.minute_convention === 'end' ? t('요청이 끝난 분') : v.state.minute_convention === 'start' ? t('요청을 보낸 분') : t('아직 모릅니다')} <span className="meta">{t('(맞은 분: 보낸 분 {{start}} · 끝난 분 {{end}})', { start: v.state.start_minutes, end: v.state.end_minutes })}</span></dd>
      </dl>
      <p className="meta">{t('5분마다 Anthropic 사용량 보고서를 1시간 단위로 받아, 같은 API 키·모델·시간에 게이트웨이가 정산한 토큰과 비교합니다. 끝난 지 10분이 안 된 시간은 비교하지 않습니다. 금액은 지금 카탈로그 가격으로 환산합니다.')}</p>
      <p className="meta">{t('1분 칸 기준은 사후 보정에 씁니다. 사용량 보고서가 요청을 보낸 분과 끝난 분 가운데 어디에 넣는지는 문서에 없어서, 사후 보정을 켜면 사용량을 모두 아는 분으로 먼저 확인합니다. 기준을 확인하기 전에는 보정하지 않습니다.')}</p>
    </section>
    {v.unmatched_credentials.length > 0 && <div className="callout callout-warning"><div className="callout-body">
      <strong>{t('대조하지 못하는 Provider 키')}</strong>
      <span>{t('아래 키는 Anthropic API 키 목록에서 찾지 못했습니다. 이 키로 보낸 요청은 비교에서 빠집니다.')}</span>
      <ul>{v.unmatched_credentials.map(c => <li key={c.id}>{c.label} <span className="mono">(…{c.hint})</span></li>)}</ul>
    </div></div>}
    <dl className="stats">
      <div className="stat"><dt>{t('Provider가 더 센 토큰')}</dt><dd>{tokens(more)}</dd><dd className="stat-sub"><Money value={v.totals.provider_more_usd} /></dd></div>
      <div className="stat"><dt>{t('게이트웨이가 더 센 토큰')}</dt><dd>{tokens(v.totals.gateway_more_tokens)}</dd><dd className="stat-sub"><Money value={v.totals.gateway_more_usd} /></dd></div>
      <div className="stat"><dt>{t('사후 보정한 요청')}</dt><dd>{tokens(v.totals.corrected_requests)}</dd><dd className="stat-sub">{t('이 기간에 끝난 요청')}</dd></div>
    </dl>
    <p className="meta">{t('Provider가 센 토큰 {{provider}} · 게이트웨이가 정산한 토큰 {{gateway}}. 게이트웨이가 더 센 토큰은 대개 최대 출력 규칙으로 정산한 끊긴 요청입니다.', { provider: tokens(v.totals.provider_tokens), gateway: tokens(v.totals.gateway_tokens) })}</p>
    {v.totals.unpriced_models.length > 0 && <p className="meta">{t('가격을 모르는 모델은 금액에서 뺐습니다: {{models}}', { models: v.totals.unpriced_models.join(', ') })}</p>}
    <section className="card stack" aria-labelledby="token-check-reasons">
      <h3 id="token-check-reasons">{t('Provider가 더 센 토큰의 까닭')}</h3>
      {more === '0' ? <p className="empty">{t('이 기간에는 Provider가 더 센 토큰이 없습니다.')}</p> : <>
        <div className="segments" aria-hidden="true">{reasons.map((r, i) => {
          const part = v.totals.reasons.find(x => x.reason === r.id), share = ratio(part?.tokens ?? '0', more);
          return share > 0 && <span key={r.id} className={`reason-${i + 1}`} style={{ flexGrow: share }} title={`${t(r.label)} ${percent(share)}`} />;
        })}</div>
        <ul className="reason-list">{reasons.map((r, i) => {
          const part = v.totals.reasons.find(x => x.reason === r.id);
          return <li key={r.id}>
            <i className={`reason-${i + 1}`} aria-hidden="true" />
            <div className="stack-sm">
              <div className="reason-figures"><strong>{t(r.label)}</strong><span className="mono">{tokens(part?.tokens ?? '0')}</span><span className="meta">{percent(ratio(part?.tokens ?? '0', more))}</span>{part && part.usd === null ? <span className="meta">{t('가격 모름')}</span> : <Money value={part?.usd ?? '0'} />}</div>
              <p className="meta">{t(r.note)}</p>
            </div>
          </li>;
        })}</ul>
      </>}
    </section>
    <section className="stack" aria-labelledby="token-check-rows">
      <h2 id="token-check-rows">{t('Provider 키·모델별')}</h2>
      {v.rows.length === 0 ? <p className="empty">{t('비교할 사용량이 없습니다.')}</p> : <div className="table-wrap"><table>
        <thead><tr><th>{t('Provider 키')}</th><th>{t('모델')}</th><th className="num">{t('Provider 토큰')}</th><th className="num">{t('게이트웨이 토큰')}</th><th className="num">{t('Provider가 더 센 토큰')}</th><th className="num">{t('게이트웨이가 더 센 토큰')}</th><th>{t('까닭')}</th></tr></thead>
        <tbody>{v.rows.map(r => <tr key={`${r.api_key_id}/${r.model}`}>
          <th scope="row">{r.credential ? <>{r.credential.label} <span className="mono meta">(…{r.credential.hint})</span></> : <span className="mono">{r.api_key_id}</span>}</th>
          <td className="mono nowrap">{r.model}</td>
          <td className="num mono">{tokens(r.provider_tokens)}</td>
          <td className="num mono">{tokens(r.gateway_tokens)}</td>
          <td className="num"><div className="mono">{tokens(r.provider_more_tokens)}</div>{r.provider_more_tokens !== '0' && <div className="meta">{r.provider_more_usd === null ? t('가격 모름') : <Money value={r.provider_more_usd} />}</div>}</td>
          <td className="num"><div className="mono">{tokens(r.gateway_more_tokens)}</div>{r.gateway_more_tokens !== '0' && <div className="meta">{r.gateway_more_usd === null ? t('가격 모름') : <Money value={r.gateway_more_usd} />}</div>}</td>
          <td>{r.reasons.length === 0 ? <span className="meta">{t('없음')}</span> : r.reasons.map(x => `${label(x.reason)} ${tokens(x.tokens)}`).join(', ')}</td>
        </tr>)}</tbody>
      </table></div>}
    </section>
  </>;
}
