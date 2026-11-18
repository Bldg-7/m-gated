import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ApiError, session, StaleResponse } from '../../lib/api';
import type { AuthorizedCredentials } from '../../lib/types';
import { credentialLabel, providerNames } from './credentials';
import { failure, useResource } from './shared';
import type { CredentialList, CredentialMeta } from './types';
import { Badge, Select } from '../../ui/ui';

type Ack = { authorization?: { moved_keys: string; blocked_keys: string } };
type Body = { provider: string; credential_ids: string[]; default_credential_id: string | null; expected: { credential_ids: string[]; default_credential_id: string | null }; bound_keys?: 'block' | { move_to: string } };

/**
 * 사용자 하나가 쓸 수 있는 회사 키(인가). Provider마다 체크한 키만 쓸 수 있고, 그중 하나가 이 사용자의 기본 키다.
 * 아무것도 체크하지 않으면 제한이 없다(그 Provider의 기본 키를 쓰고, 다른 키는 관리자만 고른다).
 */
export function CredentialAccess({ principalId, authorized, onChanged }: { principalId: string; authorized: AuthorizedCredentials; onChanged: () => Promise<void> }) {
  const { t } = useTranslation();
  const list = useResource<CredentialList>('/api/admin/provider-credentials');
  const providers = (list.data?.providers ?? []).filter(p => p.credentials.length > 0);
  return <section aria-label={t('회사 키 인가')} className="card stack">
    <h3>{t('회사 키 인가')}</h3>
    <p className="meta">{t('이 사용자가 쓸 수 있는 회사 키를 고릅니다. 고른 키 중 하나가 이 사용자의 기본 키가 되어, 키를 고르지 않고 발급한 가상 키와 SSO로 직접 보낸 요청이 그 키로 나갑니다. 아무것도 고르지 않으면 제한이 없고, 그 Provider의 기본 키를 씁니다.')}</p>
    {/* 상세 화면의 조회 상태·다시 조회 버튼과 겹치지 않게, 여기서는 읽기 실패만 짧게 적는다. 상세를 다시 조회하면 함께 다시 읽는다. */}
    {list.error && <p className="meta">{t('회사 키 목록을 읽지 못했습니다. {{reason}}', { reason: list.error })}</p>}
    {providers.map(p => <ProviderAccess key={`${p.provider}:${JSON.stringify(authorized)}`} principalId={principalId} provider={p.provider} credentials={p.credentials} current={authorized.find(a => a.provider === p.provider)?.credentials ?? []} onChanged={onChanged} />)}
  </section>;
}

function ProviderAccess({ principalId, provider, credentials, current, onChanged }: { principalId: string; provider: string; credentials: CredentialMeta[]; current: { id: string; default: boolean }[]; onChanged: () => Promise<void> }) {
  const { t } = useTranslation();
  const name = providerNames[provider] ?? provider;
  const currentIds = current.map(c => c.id), currentDefault = current.find(c => c.default)?.id ?? null;
  const [checked, setChecked] = useState(() => new Set(currentIds));
  const [defaultId, setDefaultId] = useState<string | null>(currentDefault);
  const [busy, setBusy] = useState(false), [status, setStatus] = useState<string | null>(null);
  // 인가에서 빠지는 키에 연결된 가상 키가 있을 때 고르는 처리. 서버가 알려 준 개수와 보낼 본문을 들고 있는다.
  const [inUse, setInUse] = useState<{ count: string; body: Body } | null>(null), [moveTo, setMoveTo] = useState('');
  const mounted = useRef(true);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  const selected = credentials.filter(c => checked.has(c.id));
  const effectiveDefault = defaultId && checked.has(defaultId) ? defaultId : selected[0]?.id ?? null;
  const restricted = currentIds.length > 0;
  const providerDefault = credentials.find(c => c.default);
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
      setStatus([t('{{provider}} 회사 키 인가를 저장했습니다.', { provider: name }), moved ? t('가상 키 {{count}}개를 옮겼습니다.', { count: moved }) : '', blocked ? t('가상 키 {{count}}개는 막혔습니다. 그 키의 {{provider}} 요청은 거절됩니다.', { count: blocked, provider: name }) : ''].filter(Boolean).join(' '));
      await onChanged();
    } catch (e) {
      if (!live() || e instanceof StaleResponse) return;
      if (e instanceof ApiError && e.code === 'credential_authorization_in_use') {
        setInUse({ count: String(e.details.bound_keys ?? '?'), body }); setMoveTo(body.default_credential_id ?? '');
      } else if (e instanceof ApiError && e.code === 'credential_authorization_conflict') {
        setInUse(null); setStatus(t('다른 관리자가 먼저 이 사용자의 인가를 바꿨습니다. 다시 조회했습니다. 확인하고 다시 저장하세요.'));
        await onChanged();
      } else {
        setStatus(t('{{reason}} 명령 ID: {{id}}.', { reason: failure(e), id: commandId }));
      }
    } finally { if (live()) setBusy(false); }
  };
  const save = (ids: string[], def: string | null) => {
    setInUse(null);
    void send({ provider, credential_ids: ids, default_credential_id: def, expected: { credential_ids: currentIds, default_credential_id: currentDefault } });
  };
  return <form aria-label={t('{{provider}} 회사 키 인가', { provider: name })} className="stack-sm" onSubmit={e => { e.preventDefault(); save(selected.map(c => c.id), effectiveDefault); }}>
    <fieldset disabled={busy} className="stack-sm"><legend>{name}</legend>
      <p className="meta">{restricted
        ? t('지금: 고른 키 {{count}}개만 쓸 수 있습니다. 기본 키는 {{label}}입니다.', { count: currentIds.length, label: credentialLabel(credentials.find(c => c.id === currentDefault) ?? { label: '?', hint: '????' }) })
        : t('지금: 제한 없음. {{provider}}의 기본 키 {{label}}를 씁니다.', { provider: name, label: providerDefault ? credentialLabel(providerDefault) : t('없음') })}</p>
      <div className="checks">{credentials.map(c => <label key={c.id}><input type="checkbox" checked={checked.has(c.id)} onChange={e => toggle(c.id, e.currentTarget.checked)} />{credentialLabel(c)}{c.default && <> <Badge tone="neutral">{t('Provider 기본 키')}</Badge></>}</label>)}</div>
      {selected.length > 0 && <div className="field"><label htmlFor={`access-default-${provider}`}>{t('이 사용자의 기본 키')}</label><Select id={`access-default-${provider}`} value={effectiveDefault ?? ''} onChange={e => setDefaultId(e.target.value)}>{selected.map(c => <option key={c.id} value={c.id}>{credentialLabel(c)}</option>)}</Select></div>}
      <div className="actions"><button className="btn-primary">{selected.length ? t('인가 저장') : t('제한 없애기')}</button></div>
    </fieldset>
    {inUse && <section aria-label={t('연결된 가상 키 처리')} className="confirm confirm-danger stack-sm">
      <h4>{t('연결된 가상 키 처리')}</h4>
      <p>{t('인가에서 빠지는 회사 키에 이 사용자의 가상 키 {{count}}개가 연결돼 있습니다. 어떻게 할지 고르세요.', { count: inUse.count })}</p>
      <div className="field"><label htmlFor={`access-move-${provider}`}>{t('옮길 회사 키')}</label><Select id={`access-move-${provider}`} value={moveTo} onChange={e => setMoveTo(e.target.value)}>{credentials.filter(c => inUse.body.credential_ids.includes(c.id)).map(c => <option key={c.id} value={c.id}>{credentialLabel(c)}</option>)}</Select></div>
      <div className="actions">
        <button type="button" className="btn-primary" disabled={busy || !moveTo} onClick={() => void send({ ...inUse.body, bound_keys: { move_to: moveTo } })}>{t('이 키로 옮기기')}</button>
        <button type="button" className="btn-danger" disabled={busy} onClick={() => void send({ ...inUse.body, bound_keys: 'block' })}>{t('옮기지 않고 막기')}</button>
        <button type="button" className="btn-quiet" disabled={busy} onClick={() => setInUse(null)}>{t('취소')}</button>
      </div>
      <p className="meta">{t('막으면 그 가상 키로 보내는 {{provider}} 요청이 거절되고, 키 목록에 「인가 해제됨」으로 표시됩니다. 나중에 회사 키를 바꿔 연결하거나 다시 인가하면 쓸 수 있습니다.', { provider: name })}</p>
    </section>}
    {status && <p role="status">{status}</p>}
  </form>;
}
