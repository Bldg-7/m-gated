import { useEffect, useId, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ApiError, session, StaleResponse } from '../../lib/api';
import i18n from '../../lib/i18n';
import type { AuthorizedCredentials, Grant, KeyMetadata } from '../../lib/types';
import type { PrincipalDetail } from '../admin/types';
import { credentialLabel, providerNames } from '../admin/credentials';
import { failure, useResource } from '../admin/shared';
import { useTimeFormat } from '../../lib/time';
import { Modal, SearchSelect } from '../../ui/ui';
import { GrantTable } from '../../ui/grant-table';

/** 키 주인의 지금 정책(고를 수 있는 API)과 인가된 Provider 키. */
export type KeyOwner = { grants: Grant[]; authorized: AuthorizedCredentials };
type Props = { target: KeyMetadata; validity: string; onClose: () => void; onDone: (message: string) => void };

/**
 * 가상 키 수정 창. 이름·허용 API·Provider 키 연결을 바꾼다. 유효기간·만료일·한도는 보여 주기만 한다.
 * owner가 없으면(관리자의 전체 키 목록) 키 주인의 정책과 인가를 서버에서 읽는다.
 */
export function EditKeyModal({ owner, ...props }: Props & { owner: KeyOwner | null }) {
  return owner ? <EditKeyForm owner={owner} {...props} /> : <RemoteOwner {...props} />;
}
function RemoteOwner(props: Props) {
  const { t } = useTranslation();
  const detail = useResource<PrincipalDetail>(`/api/admin/principals/${encodeURIComponent(props.target.principal_id)}`), p = detail.data?.principal;
  if (p) return <EditKeyForm owner={{ grants: p.policy.grants, authorized: p.provider_credentials ?? [] }} {...props} />;
  return <Modal title={t('{{name}} 수정', { name: props.target.name })} onClose={props.onClose} actions={<button type="button" className="btn-quiet" onClick={props.onClose}>{t('닫기')}</button>}>
    {detail.error ? <p role="alert">{detail.error}</p> : <p role="status">{t('키 주인의 정책과 인가 조회 중…')}</p>}
  </Modal>;
}
function EditKeyForm({ target, owner, validity, onClose, onDone }: Props & { owner: KeyOwner }) {
  const { t } = useTranslation();
  const prefix = useId(), format = useTimeFormat(), mounted = useRef(true);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  const authorizedFor = (provider: string) => owner.authorized.find(a => a.provider === provider)?.credentials ?? [];
  const providers = [...new Set(owner.grants.map(grant => grant.provider))];
  const [name, setName] = useState(target.name);
  // 지금 연결 가운데 키 주인에게 아직 인가된 것만 처음 값으로 둔다. 인가가 풀린 연결은 다시 골라야 한다.
  const [chosen, setChosen] = useState<Record<string, string>>(() => Object.fromEntries((target.provider_credentials ?? []).filter(b => authorizedFor(b.provider).some(c => c.id === b.credential_id)).map(b => [b.provider, b.credential_id])));
  // 지금 허용 API 가운데 키 주인의 정책에 아직 있는 것만 처음 값으로 둔다.
  const [granted, setGranted] = useState<Set<string>>(() => new Set(owner.grants.flatMap((grant, gi) => grant.capabilities.flatMap((cap, ci) => target.grants.some(g => g.provider === grant.provider && g.model === grant.model && g.capabilities.includes(cap)) ? [`${gi}:${ci}`] : []))));
  const [busy, setBusy] = useState(false), [error, setError] = useState<string | null>(null);
  const chosenProviders = providers.filter(provider => authorizedFor(provider).some(c => c.id === chosen[provider]));
  const grantedProviders = new Set(owner.grants.flatMap((grant, gi) => grant.capabilities.some((_, ci) => granted.has(`${gi}:${ci}`)) ? [grant.provider] : []));
  const withoutApi = chosenProviders.filter(provider => !grantedProviders.has(provider));
  const dropped = target.grants.flatMap(g => g.capabilities.filter(cap => !owner.grants.some(o => o.provider === g.provider && o.model === g.model && o.capabilities.includes(cap))).map(cap => `${g.provider} / ${g.model} / ${cap}`));
  const choose = (provider: string, id: string) => {
    setChosen(prev => ({ ...prev, [provider]: id }));
    if (!id) setGranted(prev => new Set([...prev].filter(value => owner.grants[Number(value.split(':')[0])]?.provider !== provider)));
  };
  // 허용할 API 표의 줄. Provider 키를 고른 Provider의 API만 보인다. key는 `정책 grant 순번:기능 순번`이다.
  const grantRows = owner.grants.flatMap((g, gi) => chosenProviders.includes(g.provider) ? g.capabilities.map((capability, ci) => ({ key: `${gi}:${ci}`, provider: g.provider, providerLabel: providerNames[g.provider] ?? g.provider, model: g.model, capability })) : []);
  const ready = name.trim().length > 0 && chosenProviders.length > 0 && withoutApi.length === 0;
  const submit = async () => {
    if (busy || !ready) return;
    const grants = owner.grants.flatMap((grant, gi) => {
      const capabilities = chosenProviders.includes(grant.provider) ? grant.capabilities.filter((_, ci) => granted.has(`${gi}:${ci}`)) : [];
      return capabilities.length ? [{ ...grant, capabilities }] : [];
    });
    const epoch = session.snapshot().epoch, commandId = crypto.randomUUID();
    setBusy(true); setError(null);
    try {
      await session.write(`/api/keys/${target.id}`, 'PATCH', commandId, { expected_version: target.version, name: name.trim(), grants, provider_credentials: Object.fromEntries(chosenProviders.map(provider => [provider, chosen[provider]])) });
      if (mounted.current && session.snapshot().epoch === epoch) onDone(i18n.t('{{name}} 키를 수정했습니다.', { name: name.trim() }));
    } catch (e) {
      if (!mounted.current || e instanceof StaleResponse) return;
      // 403 access_denied 는 세션이 권한을 다시 확인하면서 화면을 새로 그리므로 여기서 다루지 않는다.
      const known: Record<string, string> = {
        version_conflict: t('다른 곳에서 이 키를 먼저 바꿨습니다. 창을 닫고 목록을 다시 조회한 뒤 수정하세요.'),
        provider_credential_not_authorized: t('고른 Provider 키를 쓸 수 있는 인가가 없습니다.'),
        provider_credential_required: t('가상 키는 Provider 키를 하나 이상 연동해야 하고, 허용한 API의 Provider마다 Provider 키를 골라야 합니다.'),
      };
      const reason = e instanceof ApiError && known[e.code] ? known[e.code] : failure(e);
      setError(t('{{reason}} 명령 ID: {{id}}.', { reason, id: commandId })); setBusy(false);
    }
  };
  return <Modal title={t('{{name}} 수정', { name: target.name })} busy={busy} onClose={onClose} confirm={{ label: t('수정 저장'), onConfirm: () => void submit(), disabled: !ready }}>
    {error && <p role="alert">{error}</p>}
    <label>{t('키 이름')}<input value={name} maxLength={100} required disabled={busy} onChange={e => setName(e.currentTarget.value)} /></label>
    <fieldset className="stack-sm"><legend>{t('연동할 Provider 키 (1개 이상 필수)')}</legend>
      <div className="form-grid">{providers.map(provider => {
        const options = authorizedFor(provider), label = providerNames[provider] ?? provider;
        return <div key={provider} className="field">{options.length > 0
          ? <><label htmlFor={`${prefix}-credential-${provider}`}>{t('{{provider}} Provider 키', { provider: label })}</label><SearchSelect id={`${prefix}-credential-${provider}`} value={chosen[provider] ?? ''} disabled={busy} onChange={id => choose(provider, id)} options={[{ value: '', label: t('연동 안 함') }, ...options.map(c => ({ value: c.id, label: credentialLabel(c) }))]} /></>
          : <><span className="field-label">{t('{{provider}} Provider 키', { provider: label })}</span><p className="meta">{t('키 주인에게 인가된 {{provider}} Provider 키가 없습니다.', { provider: label })}</p></>}</div>;
      })}</div>
      {providers.length === 0 && <p className="callout callout-critical">{t('키 주인의 정책에 허용된 API가 없어 이 키를 수정할 수 없습니다.')}</p>}
    </fieldset>
    <fieldset className="stack-sm"><legend>{t('허용할 API (연동한 Provider마다 1개 이상 필수)')}</legend>
      {chosenProviders.length === 0 ? <p className="meta">{t('Provider 키를 먼저 고르면 그 Provider의 API를 고를 수 있습니다.')}</p>
        : <GrantTable rows={grantRows} selected={granted} disabled={busy}
            onToggle={(value, on) => setGranted(prev => { const next = new Set(prev); if (on) next.add(value); else next.delete(value); return next; })}
            onAll={on => setGranted(prev => { const next = new Set(prev); for (const r of grantRows) { if (on) next.add(r.key); else next.delete(r.key); } return next; })} />}
      {dropped.length > 0 && <p className="meta">{t('키 주인의 지금 정책에 없어 저장하면 빠지는 API: {{list}}', { list: dropped.join(', ') })}</p>}
    </fieldset>
    <div className="stack-sm"><p className="meta">{t('유효기간 {{validity}} · 만료 {{expires}}', { validity, expires: target.expires_at ? format(target.expires_at) : t('없음') })}</p>
      <details><summary>{t('한도 (수정 불가)')}</summary><pre>{JSON.stringify(target.limits, null, 2)}</pre></details>
      <p className="meta">{t('유효기간·만료일·한도는 수정할 수 없습니다. 바꾸려면 이 키를 회수하고 새로 발급하세요.')}</p></div>
  </Modal>;
}
