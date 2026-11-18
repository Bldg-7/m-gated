import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ApiError, session, StaleResponse } from '../../lib/api';
import type { AuthorizedCredentials } from '../../lib/types';
import { credentialLabel, providerNames } from './credentials';
import { failure, useResource } from './shared';
import type { CredentialList, CredentialMeta } from './types';
import { Modal, Select } from '../../ui/ui';

type Ack = { authorization?: { moved_keys: string; blocked_keys: string } };
type Body = { provider: string; credential_ids: string[]; expected: { credential_ids: string[] }; bound_keys?: 'block' | { move_to: string } };

/**
 * 사용자 하나가 쓸 수 있는 Provider 키(인가). 사용자는 관리자가 인가한 Provider 키로만 그 Provider를 쓸 수 있고,
 * 가상 키를 발급할 때 Provider마다 인가된 키 하나를 고른다. 아무것도 인가하지 않은 Provider는 쓸 수 없다.
 */
export function CredentialAccess({ principalId, authorized, onChanged }: { principalId: string; authorized: AuthorizedCredentials; onChanged: () => Promise<void> }) {
  const { t } = useTranslation();
  const list = useResource<CredentialList>('/api/admin/provider-credentials');
  const providers = (list.data?.providers ?? []).filter(p => p.credentials.length > 0 || authorized.some(a => a.provider === p.provider));
  return <section aria-label={t('Provider 키 인가')} className="card stack">
    <h3>{t('Provider 키 인가')}</h3>
    <p className="meta">{t('이 사용자가 쓸 수 있는 Provider 키를 고릅니다. 이 사용자는 고른 키로만 그 Provider를 쓸 수 있고, 가상 키를 발급할 때 이 중에서 하나를 고릅니다. 아무 키도 고르지 않은 Provider는 이 사용자가 쓸 수 없습니다.')}</p>
    {/* 상세 화면의 조회 상태·다시 조회 버튼과 겹치지 않게, 여기서는 읽기 실패만 짧게 적는다. 상세를 다시 조회하면 함께 다시 읽는다. */}
    {list.error && <p className="meta">{t('Provider 키 목록을 읽지 못했습니다. {{reason}}', { reason: list.error })}</p>}
    {providers.map(p => <ProviderAccess key={`${p.provider}:${JSON.stringify(authorized)}`} principalId={principalId} provider={p.provider} credentials={p.credentials} current={authorized.find(a => a.provider === p.provider)?.credentials ?? []} onChanged={onChanged} />)}
  </section>;
}

function ProviderAccess({ principalId, provider, credentials, current, onChanged }: { principalId: string; provider: string; credentials: CredentialMeta[]; current: { id: string; label: string; hint: string }[]; onChanged: () => Promise<void> }) {
  const { t } = useTranslation();
  const name = providerNames[provider] ?? provider;
  const currentIds = current.map(c => c.id);
  const [checked, setChecked] = useState(() => new Set(currentIds));
  const [busy, setBusy] = useState(false), [status, setStatus] = useState<string | null>(null);
  // 인가에서 빠지는 키에 연결된 가상 키가 있을 때 고르는 처리. 서버가 알려 준 개수와 보낼 본문을 들고 있는다.
  const [inUse, setInUse] = useState<{ count: string; body: Body } | null>(null), [moveTo, setMoveTo] = useState('');
  const mounted = useRef(true);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  const selected = credentials.filter(c => checked.has(c.id));
  const changed = selected.length !== currentIds.length || selected.some(c => !currentIds.includes(c.id));
  const toggle = (id: string, on: boolean) => setChecked(prev => { const next = new Set(prev); if (on) next.add(id); else next.delete(id); return next; });
  const send = async (body: Body) => {
    const epoch = session.snapshot().epoch, commandId = crypto.randomUUID();
    const live = () => mounted.current && session.snapshot().epoch === epoch;
    setBusy(true); setStatus(null);
    try {
      const ack = await session.write<Ack>(`/api/admin/principals/${principalId}/provider-credentials`, 'PUT', commandId, body);
      if (!live()) return;
      setInUse(null);
      const moved = Number(ack?.authorization?.moved_keys ?? 0), blocked = Number(ack?.authorization?.blocked_keys ?? 0);
      setStatus([body.credential_ids.length ? t('{{provider}} Provider 키 인가를 저장했습니다.', { provider: name }) : t('{{provider}} Provider 키 인가를 모두 해제했습니다. 이 사용자는 {{provider}}를 쓸 수 없습니다.', { provider: name }), moved ? t('가상 키 {{count}}개를 옮겼습니다.', { count: moved }) : '', blocked ? t('가상 키 {{count}}개는 막혔습니다. 그 키의 {{provider}} 요청은 거절됩니다.', { count: blocked, provider: name }) : ''].filter(Boolean).join(' '));
      await onChanged();
    } catch (e) {
      if (!live() || e instanceof StaleResponse) return;
      if (e instanceof ApiError && e.code === 'credential_authorization_in_use') {
        setInUse({ count: String(e.details.bound_keys ?? '?'), body }); setMoveTo(body.credential_ids.length === 1 ? body.credential_ids[0] : '');
      } else if (e instanceof ApiError && e.code === 'credential_authorization_conflict') {
        setInUse(null); setStatus(t('다른 관리자가 먼저 이 사용자의 인가를 바꿨습니다. 다시 조회했습니다. 확인하고 다시 저장하세요.'));
        await onChanged();
      } else {
        setStatus(t('{{reason}} 명령 ID: {{id}}.', { reason: failure(e), id: commandId }));
      }
    } finally { if (live()) setBusy(false); }
  };
  const save = (ids: string[]) => {
    setInUse(null);
    void send({ provider, credential_ids: ids, expected: { credential_ids: currentIds } });
  };
  const targets = inUse ? credentials.filter(c => inUse.body.credential_ids.includes(c.id)) : [];
  return <form aria-label={t('{{provider}} Provider 키 인가', { provider: name })} className="stack-sm" onSubmit={e => { e.preventDefault(); if (changed) save(selected.map(c => c.id)); }}>
    <fieldset disabled={busy} className="stack-sm"><legend>{name}</legend>
      <p className="meta">{currentIds.length
        ? t('지금: 인가된 키 {{count}}개를 쓸 수 있습니다.', { count: currentIds.length })
        : t('지금: 인가된 키가 없어 이 사용자는 {{provider}}를 쓸 수 없습니다.', { provider: name })}</p>
      <div className="checks">{credentials.map(c => <label key={c.id}><input type="checkbox" checked={checked.has(c.id)} onChange={e => toggle(c.id, e.currentTarget.checked)} />{credentialLabel(c)}</label>)}</div>
      {changed && selected.length === 0 && <p className="text-warning">{t('모두 해제하면 이 사용자는 {{provider}}를 쓸 수 없습니다.', { provider: name })}</p>}
      <div className="actions"><button className="btn-primary" disabled={!changed}>{selected.length || !currentIds.length ? t('인가 저장') : t('인가 모두 해제')}</button></div>
    </fieldset>
    {inUse && <Modal variant="danger" busy={busy} onClose={() => setInUse(null)} title={t('연결된 가상 키 처리')}
      actions={<>
        {targets.length > 0 && <button type="button" className="btn-primary" disabled={busy || !moveTo} onClick={() => void send({ ...inUse.body, bound_keys: { move_to: moveTo } })}>{t('이 키로 옮기기')}</button>}
        <button type="button" className="btn-danger" disabled={busy} onClick={() => void send({ ...inUse.body, bound_keys: 'block' })}>{t('옮기지 않고 막기')}</button>
        <button type="button" className="btn-quiet" disabled={busy} onClick={() => setInUse(null)}>{t('취소')}</button>
      </>}>
      <p>{t('인가에서 빠지는 Provider 키에 이 사용자의 가상 키 {{count}}개가 연결돼 있습니다. 어떻게 할지 고르세요.', { count: inUse.count })}</p>
      {targets.length > 0
        ? <div className="field"><label htmlFor={`access-move-${provider}`}>{t('옮길 Provider 키')}</label><Select id={`access-move-${provider}`} value={moveTo} onChange={setMoveTo} placeholder={t('선택하세요')} options={targets.map(c => ({ value: c.id, label: credentialLabel(c) }))} /></div>
        : <p className="meta">{t('남는 인가 키가 없어 옮길 수 없습니다.')}</p>}
      <p className="meta">{t('막으면 그 가상 키로 보내는 {{provider}} 요청이 거절되고, 키 목록에 「인가 해제됨」으로 표시됩니다. 나중에 Provider 키를 바꿔 연결하거나 다시 인가하면 쓸 수 있습니다.', { provider: name })}</p>
    </Modal>}
    {status && <p role="status">{status}</p>}
  </form>;
}
