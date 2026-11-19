import { useEffect, useId, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ApiError, session, StaleResponse } from '../../lib/api';
import { failure, useAll } from './shared';
import type { CredentialMeta, PrincipalSummary } from './types';
import { Modal, Pill, Select } from '../../ui/ui';

/** 서버가 한 번에 받는 사용자 수(src/ledger/credentials.rs MAX_GRANT_PRINCIPALS)와 같다. */
const MAX_GRANT = 500;
type Kind = '' | 'human' | 'service';

/**
 * Provider 키 하나를 여러 사용자(사람·서비스 계정)에게 한 번에 인가하는 창.
 * 인가를 더하기만 한다. 이미 인가된 사용자는 표시만 하고, 인가를 푸는 일은 사용자 상세에서 한다.
 */
export function GrantModal({ provider, providerName, credential, onClose, onDone }: { provider: string; providerName: string; credential: CredentialMeta; onClose: () => void; onDone: (granted: number) => void }) {
  const { t } = useTranslation();
  const prefix = useId(), mounted = useRef(true);
  // 사용자 목록은 페이지 단위다. 전체 선택이 빠뜨리는 사용자가 없도록 끝까지 읽는다.
  const { items: principals, error: loadError } = useAll<PrincipalSummary>('/api/admin/principals');
  const [search, setSearch] = useState(''), [kind, setKind] = useState<Kind>(''), [picked, setPicked] = useState<Set<string>>(() => new Set());
  const [busy, setBusy] = useState(false), [error, setError] = useState<string | null>(null);
  const holders = new Set(credential.authorized_principal_ids ?? []);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  const needle = search.trim().toLowerCase();
  const shown = (principals ?? []).filter(p => (!kind || p.kind === kind) && (!needle || p.display_name.toLowerCase().includes(needle) || (p.email ?? '').toLowerCase().includes(needle) || p.id.includes(needle)));
  const selectable = shown.filter(p => !holders.has(p.id));
  const toggle = (id: string, on: boolean) => setPicked(prev => { const next = new Set(prev); if (on) next.add(id); else next.delete(id); return next; });
  const submit = async () => {
    if (busy || picked.size === 0 || picked.size > MAX_GRANT) return;
    const epoch = session.snapshot().epoch, commandId = crypto.randomUUID();
    setBusy(true); setError(null);
    try {
      await session.write(`/api/admin/provider-credentials/${provider}/${credential.id}`, 'POST', commandId, { principal_ids: [...picked] });
      if (mounted.current && session.snapshot().epoch === epoch) onDone(picked.size);
    } catch (e) {
      if (!mounted.current || e instanceof StaleResponse) return;
      const reason = e instanceof ApiError && e.code === 'resource_not_found' ? t('고른 사용자나 이 Provider 키가 더 이상 없습니다. 아무도 인가하지 않았습니다. 창을 닫고 다시 여세요.') : failure(e);
      setError(t('{{reason}} 명령 ID: {{id}}.', { reason, id: commandId })); setBusy(false);
    }
  };
  return <Modal title={t('{{label}} 사용자 인가', { label: credential.label })} busy={busy} onClose={onClose}
    confirm={{ label: t('{{count}}명에게 인가', { count: picked.size }), onConfirm: () => void submit(), disabled: picked.size === 0 || picked.size > MAX_GRANT }}>
    <p className="meta">{t('고른 사용자가 {{provider}} Provider 키 {{label}}(…{{hint}})를 쓸 수 있게 합니다. 이미 준 인가와 가상 키의 연결은 바꾸지 않습니다. 인가를 푸는 일은 사용자 상세에서 합니다.', { provider: providerName, label: credential.label, hint: credential.hint })}</p>
    {error && <p role="alert">{error}</p>}{loadError && <p role="alert">{loadError}</p>}
    {!principals && !loadError && <p role="status">{t('사용자 목록 조회 중…')}</p>}
    {principals && <>
      <div className="form-grid">
        <label>{t('사용자 검색 (이름·이메일·ID)')}<input type="search" value={search} onChange={e => setSearch(e.currentTarget.value)} autoComplete="off" spellCheck={false} /></label>
        <div className="field"><label htmlFor={`${prefix}-kind`}>{t('종류')}</label><Select id={`${prefix}-kind`} value={kind} onChange={value => setKind(value as Kind)} options={[{ value: '', label: t('전체') }, { value: 'human', label: t('사람') }, { value: 'service', label: t('서비스') }]} /></div>
      </div>
      <div className="row row-between">
        <p className="meta" role="status">{t('{{picked}}명 선택 · 이미 인가됨 {{held}}명 · 보이는 사용자 {{shown}}명', { picked: picked.size, held: holders.size, shown: shown.length })}</p>
        <div className="actions">
          <button type="button" className="btn-sm" disabled={busy || selectable.every(p => picked.has(p.id))} onClick={() => setPicked(prev => new Set([...prev, ...selectable.map(p => p.id)]))}>{t('보이는 사용자 모두 선택')}</button>
          <button type="button" className="btn-sm btn-quiet" disabled={busy || picked.size === 0} onClick={() => setPicked(new Set())}>{t('선택 해제')}</button>
        </div>
      </div>
      {picked.size > MAX_GRANT && <p role="alert">{t('한 번에 {{max}}명까지 인가할 수 있습니다. 선택을 줄이세요.', { max: MAX_GRANT })}</p>}
      {shown.length === 0 ? <p className="empty">{t('조건에 맞는 사용자가 없습니다.')}</p>
        : <ul className="list grant-list" aria-label={t('인가할 사용자')}>{shown.map(p => {
          const held = holders.has(p.id);
          return <li key={p.id}><label className="row"><input type="checkbox" checked={held || picked.has(p.id)} disabled={held || busy} onChange={e => toggle(p.id, e.currentTarget.checked)} />
            <span>{p.display_name}</span><Pill>{p.kind === 'service' ? t('서비스') : t('사람')}</Pill>{held && <Pill>{t('이미 인가됨')}</Pill>}{p.disabled && <Pill>{t('정지')}</Pill>}
            <span className="meta mono">{p.email ?? p.id}</span></label></li>;
        })}</ul>}
    </>}
  </Modal>;
}
