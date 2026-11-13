import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ApiError, session, StaleResponse } from '../../lib/api';
import i18n, { msgid } from '../../lib/i18n';
import { Badge, type Tone } from '../../ui/ui';
import { failure, ReadState, useResource } from './shared';
import type { CatalogEntry, CatalogProposal, CatalogProposalItem, CatalogView } from './types';

const sourceNames: Record<string, string> = { models_dev: 'models.dev', litellm: 'LiteLLM', openrouter: 'OpenRouter' };
const rateNames: [string, string][] = [['input', msgid('입력')], ['output', msgid('출력')], ['cache_read', msgid('캐시 읽기')], ['cache_write_5m', msgid('캐시 쓰기 (5분)')]];
const statusLabel: Record<string, [string, Tone]> = { changed: [msgid('변경'), 'warning'], new: [msgid('새 모델'), 'info'], missing: [msgid('출처에 없음'), 'critical'], incomplete: [msgid('값 부족'), 'neutral'], unchanged: [msgid('변경 없음'), 'success'] };
/** 승인 대상으로 고를 수 있는 상태. 처음에는 바뀐 값·사라진 모델만 골라 두고, 새 모델은 관리자가 직접 고른다. */
const selectable = new Set(['changed', 'new', 'missing']);

function message(error: unknown) {
  if (error instanceof ApiError) {
    const known: Record<string, string> = {
      catalog_sources_unavailable: msgid('공개 가격 목록을 하나도 받지 못했습니다. 잠시 후 다시 동기화하세요.'),
      catalog_proposal_stale: msgid('이 변경안을 만든 뒤 카탈로그가 바뀌었습니다. 다시 동기화하세요.'),
      version_conflict: msgid('다른 관리자가 먼저 카탈로그를 바꿨습니다. 다시 동기화하세요.'),
    };
    if (known[error.code]) return i18n.t(known[error.code]);
  }
  return failure(error);
}
/** USD/1M 12자리 문자열을 읽기 쉽게 줄인다(끝의 0만 뺀다). 값 자체는 바꾸지 않는다. */
function price(value: string | undefined) {
  if (value === undefined) return '—';
  const [whole, fraction = ''] = value.split('.');
  const trimmed = fraction.replace(/0+$/, '');
  return `$${trimmed ? `${whole}.${trimmed}` : whole}`;
}
function tokens(value: number | null | undefined) {
  return value === null || value === undefined ? '—' : value.toLocaleString('en-US');
}

/** 모델·가격 카탈로그. 동기화는 변경안만 만들고, 관리자가 비교해 보고 승인해야 적용된다. */
export function ModelsScreen() {
  const { t } = useTranslation();
  const view = useResource<CatalogView>('/gateway/admin/model-catalog');
  const [proposal, setProposal] = useState<{ id: string; body: CatalogProposal } | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState<'sync' | 'approve' | null>(null), [status, setStatus] = useState<string | null>(null);
  const mounted = useRef(true);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  const key = (i: { provider: string; model: string }) => `${i.provider}/${i.model}`;
  const run = async (kind: 'sync' | 'approve') => {
    if (busy || !view.data) return;
    const epoch = session.snapshot().epoch, commandId = crypto.randomUUID();
    const current = () => mounted.current && session.snapshot().epoch === epoch;
    setBusy(kind); setStatus(null);
    try {
      if (kind === 'sync') {
        const ack = await session.write<{ proposal_id: string; proposal: CatalogProposal }>('/gateway/admin/model-catalog/sync', 'POST', commandId, {});
        if (!current() || !ack) return;
        setProposal({ id: ack.proposal_id, body: ack.proposal });
        setSelected(new Set(ack.proposal.items.filter(i => i.status === 'changed' || i.status === 'missing').map(key)));
        setStatus(t('변경안을 만들었습니다. 아직 적용되지 않았습니다. 확인 후 승인하세요.'));
      } else if (proposal) {
        const items = proposal.body.items.filter(i => selected.has(key(i))).map(i => ({ provider: i.provider, model: i.model }));
        const ack = await session.write<{ result_version: string }>('/gateway/admin/model-catalog/approve', 'POST', commandId, { proposal_id: proposal.id, expected_version: view.data.version, items });
        if (!current() || !ack) return;
        setProposal(null); setSelected(new Set());
        setStatus(t('카탈로그 버전 {{version}}을 적용했습니다.', { version: ack.result_version }));
        try { await view.reload(); } catch { /* ReadState가 조회 실패를 표시한다 */ }
      }
    } catch (e) {
      if (!current() || e instanceof StaleResponse) return;
      setStatus(t('{{reason}} 명령 ID: {{id}}.', { reason: message(e), id: commandId }));
    } finally {
      if (current()) setBusy(null);
    }
  };
  const items = proposal?.body.items ?? [];
  const actionable = items.filter(i => selectable.has(i.status));
  const unchanged = items.filter(i => i.status === 'unchanged').length;
  const incomplete = items.filter(i => i.status === 'incomplete');
  return <div className="stack-lg">
    <ReadState {...view} />{status && <p role="status">{status}</p>}
    {view.data && <section aria-label={t('현재 카탈로그')} className="card stack">
      <div className="row row-between"><h3>{t('현재 카탈로그 · 버전 {{version}}', { version: view.data.version })}</h3>
        <div className="actions"><button className="btn-primary" disabled={busy !== null} aria-busy={busy === 'sync' || undefined} onClick={() => void run('sync')}>{busy === 'sync' ? t('동기화 중…') : t('공개 가격 목록과 동기화')}</button></div></div>
      {view.data.approved_at ? <p className="meta">{t('승인 {{at}} ({{by}})', { at: view.data.approved_at, by: view.data.approved_by_email ?? t('알 수 없음') })}</p> : <p className="meta">{t('아직 승인된 카탈로그가 없습니다. 동기화한 뒤 쓸 모델을 골라 승인하세요.')}</p>}
      <p className="meta">{t('카탈로그로 관리하는 공급사')}: {view.data.providers.map(p => `${p.provider} (${p.model_prefixes.join(', ')})`).join(' · ') || t('없음')}</p>
      {view.data.entries.length > 0 && <EntriesTable entries={view.data.entries} />}
    </section>}
    {proposal && <section aria-label={t('동기화 변경안')} className="card stack">
      <h3>{t('동기화 변경안')}</h3>
      <p className="meta">{proposal.body.sources.map(s => `${sourceNames[s.source] ?? s.source}: ${s.ok ? t('{{count}}개 모델', { count: s.models }) : t('받지 못함')}`).join(' · ')}</p>
      <p className="meta">{t('값이 출처마다 다르면 models.dev → LiteLLM → OpenRouter 순서로 가격이 모두 있는 첫 출처의 값을 제안합니다. 입력 토큰 수에 따라 가격이 올라가는 모델은 그 구간 아래로 입력을 제한합니다.')}</p>
      {actionable.length === 0 ? <p className="empty">{t('적용할 변경이 없습니다.')}</p> : <div className="table-wrap"><table>
        <thead><tr><th>{t('적용')}</th><th>{t('모델')}</th><th>{t('상태')}</th><th>{t('항목')}</th><th className="num">{t('현재')}</th><th className="num">{t('변경 후')}</th><th>{t('출처별 값')}</th></tr></thead>
        <tbody>{actionable.map(item => <ProposalRows key={key(item)} item={item} checked={selected.has(key(item))} onToggle={checked => setSelected(prev => { const next = new Set(prev); if (checked) next.add(key(item)); else next.delete(key(item)); return next; })} disabled={busy !== null} />)}</tbody>
      </table></div>}
      <p className="meta">{t('변경 없는 모델 {{count}}개', { count: unchanged })}{incomplete.length > 0 && <> · {t('가격 항목이 모자라 제안하지 못한 모델: {{models}}', { models: incomplete.map(i => i.model).join(', ') })}</>}</p>
      <div className="actions"><button className="btn-primary" disabled={busy !== null || selected.size === 0} aria-busy={busy === 'approve' || undefined} onClick={() => void run('approve')}>{t('고른 {{count}}개 승인', { count: selected.size })}</button><button className="btn-quiet" disabled={busy !== null} onClick={() => { setProposal(null); setSelected(new Set()); setStatus(null); }}>{t('변경안 닫기')}</button></div>
    </section>}
  </div>;
}

function EntriesTable({ entries }: { entries: CatalogEntry[] }) {
  const { t } = useTranslation();
  return <div className="table-wrap"><table>
    <thead><tr><th>{t('모델')}</th>{rateNames.map(([k, label]) => <th key={k} className="num">{t(label)}</th>)}<th className="num">{t('최대 입력 / 출력 token')}</th><th>{t('출처')}</th></tr></thead>
    <tbody>{entries.map(e => <tr key={`${e.provider}/${e.model}`}><th scope="row">{e.provider} / <span className="mono">{e.model}</span></th>{rateNames.map(([k]) => <td key={k} className="num mono">{price(e.rates[k])}</td>)}<td className="num mono">{tokens(e.max_input_tokens)} / {tokens(e.max_output_tokens)}</td><td>{sourceNames[e.source] ?? e.source}</td></tr>)}</tbody>
  </table><p className="meta">{t('가격 단위: 100만 token당 USD')}</p></div>;
}

/** 변경안 항목 하나. 값이 바뀌는 칸만 강조하고, 출처별 원래 값을 같이 보여 준다. */
function ProposalRows({ item, checked, onToggle, disabled }: { item: CatalogProposalItem; checked: boolean; onToggle: (checked: boolean) => void; disabled: boolean }) {
  const { t } = useTranslation();
  const [label, tone] = statusLabel[item.status];
  const fields: [string, string, (e: CatalogEntry | null) => string][] = [
    ...rateNames.map(([k, name]) => [k, t(name), (e: CatalogEntry | null) => e ? price(e.rates[k]) : '—'] as [string, string, (e: CatalogEntry | null) => string]),
    ['max_input_tokens', t('최대 입력 token'), e => tokens(e?.max_input_tokens)],
    ['max_output_tokens', t('최대 출력 token'), e => tokens(e?.max_output_tokens)],
  ];
  const after = item.status === 'missing' ? null : item.proposed;
  const sources = Object.entries(item.sources);
  return <>{fields.map(([field, name, show], index) => {
    const before = show(item.current), next = show(after);
    return <tr key={field} className={index === 0 ? 'row-start' : undefined}>
      {index === 0 && <>
        <td rowSpan={fields.length}><input type="checkbox" aria-label={t('{{model}} 적용', { model: item.model })} checked={checked} disabled={disabled} onChange={e => onToggle(e.currentTarget.checked)} /></td>
        <th scope="rowgroup" rowSpan={fields.length}>{item.provider} / <span className="mono">{item.model}</span></th>
        <td rowSpan={fields.length}><Badge tone={tone}>{t(label)}</Badge>{!item.agree && <><br /><Badge tone="warning">{t('출처 값 불일치')}</Badge></>}{item.proposed?.note && <p className="meta">{t('구간 가격 때문에 입력 제한')}</p>}</td>
      </>}
      <td>{name}</td>
      <td className="num mono">{before}</td>
      <td className={before !== next ? 'num mono text-warning' : 'num mono'}>{next}</td>
      <td className="meta mono">{sources.map(([source, c]) => `${sourceNames[source] ?? source} ${field.startsWith('max_') ? tokens(field === 'max_input_tokens' ? c.max_input_tokens : c.max_output_tokens) : price(c.rates[field])}`).join(' · ') || t('출처 없음')}</td>
    </tr>;
  })}</>;
}
