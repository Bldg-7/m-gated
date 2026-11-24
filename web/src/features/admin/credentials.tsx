import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import i18n, { msgid } from '../../lib/i18n';
import { ApiError, session, StaleResponse } from '../../lib/api';
import { CommandBusy, failure, fields, ReadState, useResource } from './shared';
import type { CredentialList, CredentialMeta } from './types';
import { useTimeFormat } from '../../lib/time';
import { Badge, IconButton, Modal, SearchSelect, Select, StatusBadge } from '../../ui/ui';
import { GrantModal } from './grant';

export const providerNames: Record<string, string> = { anthropic: 'Anthropic', openai: 'OpenAI', deepseek: 'DeepSeek' };
/** 상태 라벨은 StatusBadge(정상·미등록·복호화 불가)가 그리고, 여기에는 그 결과만 적는다. */
const statusNote: Record<string, string> = { missing: msgid('활성 Provider 키가 없는 요청은 503으로 거절됩니다.'), undecryptable: msgid('CREDENTIAL_ENCRYPTION_KEY가 바뀌었습니다. 복호화할 수 없는 키마다 「키 교체」로 같은 키를 다시 넣으세요. 이름·연결·인가는 그대로 유지됩니다.') };
/** 키 교체와 해제는 확인 창을 거친다. 키 추가는 입력 창에서 바로 보낸다. */
type Pending =
  | { kind: 'replace'; provider: string; target: CredentialMeta; hint: string }
  | { kind: 'retire'; provider: string; target: CredentialMeta; moveTo: CredentialMeta | null };
/** 키 하나에 대해 열어 둔 편집 칸. 한 번에 하나만 연다. */
type Editing = { id: string; kind: 'rename' | 'replace' | 'retire' };
/** 표의 한 줄. 같은 Provider의 다른 키는 해제할 때 연결을 옮길 후보다. */
type Row = { provider: string; c: CredentialMeta; others: CredentialMeta[] };

export function credentialLabel(c: { label: string; hint: string }) {
  return `${c.label} (…${c.hint})`;
}

function message(error: unknown) {
  if (error instanceof ApiError) {
    const known: Record<string, string> = {
      provider_credential_rejected: msgid('Provider가 이 키를 거절했습니다. 저장하지 않았습니다.'),
      provider_credential_unverified: msgid('Provider 확인 요청이 실패해 키가 유효한지 알 수 없습니다. 저장하지 않았습니다. 잠시 후 다시 시도하세요.'),
      credential_duplicate: msgid('같은 키가 이미 다른 이름으로 등록돼 있습니다.'),
      credential_label_taken: msgid('같은 Provider에 이미 이 이름을 쓰는 키가 있습니다. 다른 이름을 쓰세요.'),
      credential_in_use: msgid('가상 키가 연결된 키는 연결을 옮길 키를 고른 뒤에 해제할 수 있습니다.'),
      version_conflict: msgid('다른 관리자가 먼저 이 키를 바꿨습니다. 서버를 다시 조회했습니다. 현재 상태를 확인하고 다시 시도하세요.'),
      invalid_request: msgid('입력 형식이 맞지 않습니다. 이름은 앞뒤 공백 없이 1~60자이고, Anthropic 키는 sk-ant-, OpenAI·DeepSeek 키는 sk-로 시작합니다.'),
    };
    if (known[error.code]) return i18n.t(known[error.code]);
  }
  return failure(error);
}

/** Provider 키 관리. 키 값은 입력칸에서 전송 직전까지만 들고 있고 화면에 다시 보여 주지 않는다. */
export function CredentialsScreen() {
  const { t } = useTranslation();
  const format = useTimeFormat();
  const list = useResource<CredentialList>('/api/admin/provider-credentials');
  const secret = useRef<string | null>(null), mounted = useRef(true);
  const [pending, setPending] = useState<Pending | null>(null), [editing, setEditing] = useState<Editing | null>(null);
  const [busy, setBusy] = useState(false), [status, setStatus] = useState<string | null>(null), [formKey, setFormKey] = useState(0);
  // 열려 있는 창 안에 보여 줄 입력 오류.
  const [formError, setFormError] = useState<string | null>(null);
  // 사용자 인가 창을 연 Provider 키.
  const [granting, setGranting] = useState<{ provider: string; target: CredentialMeta } | null>(null);
  // 표에 보일 Provider(빈 값은 전체)와, 키 추가 창에서 고른 Provider.
  const [filter, setFilter] = useState(''), [adding, setAdding] = useState<string | null>(null);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; secret.current = null; }; }, []);
  const cancel = () => { secret.current = null; setPending(null); setFormKey(v => v + 1); };
  /** 명령 하나를 보내고 결과 문구를 남긴 뒤 목록을 다시 읽는다. 실패해도 자동으로 다시 보내지 않는다. */
  const send = async (url: string, method: 'POST' | 'PUT' | 'PATCH' | 'DELETE', body: unknown, done: string, inDialog = false) => {
    const epoch = session.snapshot().epoch, commandId = crypto.randomUUID();
    const current = () => mounted.current && session.snapshot().epoch === epoch;
    setBusy(true); setStatus(null); setFormError(null);
    let outcome: string, failed = false;
    try {
      await session.write(url, method, commandId, body);
      outcome = done; setEditing(null);
    } catch (e) {
      if (!current() || e instanceof StaleResponse) return;
      outcome = t('{{reason}} 명령 ID: {{id}}.', { reason: message(e), id: commandId }); failed = true;
    }
    try { await list.reload(); } catch { /* ReadState가 조회 실패를 표시한다 */ }
    if (!current()) return;
    setBusy(false);
    // 창이 열려 있으면 실패 사유를 창 안에 보여 준다. 뒤 화면의 문구는 창에 가려 보이지 않는다.
    if (failed && inDialog) setFormError(outcome); else { setStatus(outcome); setAdding(null); }
  };
  const execute = async () => {
    if (!pending || busy) return;
    const action = pending, value = secret.current, base = `/api/admin/provider-credentials/${action.provider}`;
    secret.current = null; setPending(null); setFormKey(v => v + 1);
    if (action.kind === 'replace') await send(`${base}/${action.target.id}`, 'PUT', { secret: value, expected_version: action.target.version }, t('{{label}}의 키를 …{{hint}}로 바꿨습니다. 연결된 가상 키는 새 키를 씁니다.', { label: action.target.label, hint: action.hint }));
    else {
      const query = new URLSearchParams({ expected_version: action.target.version });
      if (action.moveTo) query.set('move_to', action.moveTo.id);
      await send(`${base}/${action.target.id}?${query}`, 'DELETE', undefined, action.moveTo ? t('{{label}}를 해제하고 연결을 {{to}}로 옮겼습니다.', { label: credentialLabel(action.target), to: credentialLabel(action.moveTo) }) : t('{{label}}를 해제했습니다.', { label: credentialLabel(action.target) }));
    }
  };
  const locked = busy || pending !== null;
  const providers = list.data?.providers ?? [];
  const rows: Row[] = providers.filter(p => !filter || p.provider === filter).flatMap(p => p.credentials.map(c => ({ provider: p.provider, c, others: p.credentials.filter(o => o.id !== c.id) })));
  const history = providers.filter(p => !filter || p.provider === filter).flatMap(p => p.history);
  const target = editing ? providers.flatMap(p => p.credentials.map(c => ({ provider: p.provider, c, others: p.credentials.filter(o => o.id !== c.id) }))).find(r => r.c.id === editing.id) ?? null : null;
  const bound = target ? target.c.bound_keys !== '0' : false;
  return <div className="stack-lg">
    <ReadState {...list} />{status && <p role="status">{status}</p>}{busy && <CommandBusy />}
    {pending && <Modal variant={pending.kind === 'retire' ? 'danger' : 'normal'} busy={busy} onClose={cancel} confirm={{ label: t('명령 실행'), onConfirm: () => void execute() }}
      title={pending.kind === 'replace' ? t('{{label}} 키 교체 확인', { label: pending.target.label }) : t('{{label}} 해제 확인', { label: pending.target.label })}>
      <p>{pending.kind === 'replace' ? t('끝자리 …{{hint}} 키를 Provider에 확인한 뒤 {{label}}의 키를 바꿉니다. 이전 키는 해제되고 암호문이 지워집니다. 이름·인가·가상 키 연결은 그대로 이어집니다.', { hint: pending.hint, label: credentialLabel(pending.target) })
        : pending.moveTo ? t('{{label}}를 해제합니다. 이 키에 연결된 가상 키는 {{to}}로 옮겨집니다.', { label: credentialLabel(pending.target), to: credentialLabel(pending.moveTo) })
        : t('{{label}}를 해제합니다. 되돌릴 수 없습니다. 이 Provider의 마지막 키라면 새 키를 등록할 때까지 요청이 거절됩니다.', { label: credentialLabel(pending.target) })}</p>
    </Modal>}
    {granting && <GrantModal provider={granting.provider} providerName={providerNames[granting.provider]} credential={granting.target} onClose={() => setGranting(null)}
      onDone={count => { const label = credentialLabel(granting.target); setGranting(null); setStatus(t('{{label}}를 {{count}}명에게 인가했습니다.', { label, count })); void list.reload().catch(() => { /* ReadState가 조회 실패를 표시한다 */ }); }} />}
    {adding !== null && <Modal title={t('Provider 키 추가')} busy={busy} onClose={() => setAdding(null)} actions={<><button type="submit" form="credential-add" className="btn-primary" disabled={locked}>{t('추가')}</button><button type="button" className="btn-quiet" disabled={busy} onClick={() => setAdding(null)}>{t('취소')}</button></>}>
      <form id="credential-add" aria-label={t('Provider 키 추가')} className="stack" onSubmit={e => { const f = fields(e), label = String(f.get('label') ?? '').trim(), value = String(f.get('secret') ?? ''); setStatus(null); if (!adding) { setFormError(t('Provider를 고르세요.')); return; } if (!label || value.length < 20) { setFormError(message(new ApiError(400, 'invalid_request'))); return; } void send(`/api/admin/provider-credentials/${adding}`, 'POST', { label, secret: value }, t('{{provider}} 키 {{label}}(…{{hint}})를 확인하고 저장했습니다.', { provider: providerNames[adding] ?? adding, label, hint: value.slice(-4) }), true); }}>
        {formError && <p role="alert">{formError}</p>}
        <div className="field"><label htmlFor="credential-add-provider">Provider</label><Select id="credential-add-provider" required value={adding} onChange={setAdding} placeholder={t('선택하세요')} options={providers.map(p => ({ value: p.provider, label: providerNames[p.provider] ?? p.provider }))} /></div>
        <label>{t('이름 (예: 팀·용도)')}<input name="label" required maxLength={60} autoComplete="off" /></label>
        <label>{t('API 키')}<input name="secret" type="password" autoComplete="off" spellCheck={false} required maxLength={512} /></label>
        <p className="meta">{t('사용자는 관리자가 인가한 Provider 키만 쓸 수 있습니다. 저장한 뒤 이 키의 「사용자 인가」로 여러 사용자에게 한 번에 인가하거나, 사용자 상세의 Provider 키 인가에서 인가하세요.')}</p>
      </form>
    </Modal>}
    {target && editing && !pending && <Modal variant={editing.kind === 'retire' ? 'danger' : 'normal'} busy={busy} onClose={() => setEditing(null)}
      title={editing.kind === 'rename' ? t('{{label}} 이름 변경', { label: target.c.label }) : editing.kind === 'replace' ? t('{{label}} 키 교체', { label: target.c.label }) : t('{{label}} 해제', { label: target.c.label })}
      actions={<><button type="submit" form="credential-edit" className={editing.kind === 'retire' ? 'btn-danger-outline' : 'btn-primary'} disabled={locked || (editing.kind === 'retire' && bound && target.others.length === 0)}>{editing.kind === 'rename' ? t('저장') : editing.kind === 'replace' ? t('교체 확인') : t('해제 확인')}</button><button type="button" className="btn-quiet" data-modal-cancel="" disabled={busy} onClick={() => setEditing(null)}>{t('취소')}</button></>}>
      {formError && <p role="alert">{formError}</p>}
      {editing.kind === 'rename' && <form id="credential-edit" aria-label={t('{{label}} 이름 변경', { label: target.c.label })} onSubmit={e => { const label = String(fields(e).get('label') ?? ''); if (label === target.c.label) { setEditing(null); return; } void send(`/api/admin/provider-credentials/${target.provider}/${target.c.id}`, 'PATCH', { expected_version: target.c.version, label }, t('이름을 {{label}}로 바꿨습니다.', { label }), true); }}>
        <label>{t('새 이름')}<input name="label" defaultValue={target.c.label} required maxLength={60} autoFocus /></label>
      </form>}
      {editing.kind === 'replace' && <form id="credential-edit" key={formKey} aria-label={t('{{label}} 키 교체', { label: target.c.label })} className="stack-sm" onSubmit={e => { const value = String(fields(e).get('secret') ?? ''); setStatus(null); if (value.length < 20) { setFormError(message(new ApiError(400, 'invalid_request'))); return; } setFormError(null); secret.current = value; setPending({ kind: 'replace', provider: target.provider, target: target.c, hint: value.slice(-4) }); setEditing(null); }}>
        <p className="meta">{t('이름과 연결은 그대로 두고 키만 바꿉니다')}</p>
        <label>{t('새 {{provider}} API 키', { provider: providerNames[target.provider] })}<input name="secret" type="password" autoComplete="off" spellCheck={false} required maxLength={512} autoFocus /></label>
      </form>}
      {editing.kind === 'retire' && <form id="credential-edit" aria-label={t('{{label}} 해제', { label: target.c.label })} className="stack-sm" onSubmit={e => { const to = String(fields(e).get('move_to') ?? ''); if (bound && !target.others.some(o => o.id === to)) return; setPending({ kind: 'retire', provider: target.provider, target: target.c, moveTo: bound ? target.others.find(o => o.id === to) ?? null : null }); setEditing(null); }}>
        {bound && target.others.length > 0 && <div className="field"><label htmlFor={`move-${target.c.id}`}>{t('연결을 옮길 키 (필수)')}</label><SearchSelect id={`move-${target.c.id}`} name="move_to" required placeholder={t('선택하세요')} defaultValue={target.others.length === 1 ? target.others[0].id : ''} options={target.others.map(o => ({ value: o.id, label: credentialLabel(o) }))} /></div>}
        <p className="meta">{bound ? target.others.length > 0 ? t('이 키에 연결된 가상 키를 고른 키로 옮깁니다. 옮길 키를 그 가상 키 주인에게 인가하지 않았다면, 옮긴 가상 키의 요청은 거절됩니다.') : t('이 키에 연결된 가상 키를 옮길 다른 키가 없어 해제할 수 없습니다. 먼저 키를 추가하세요.')
          : target.others.length > 0 ? t('연결된 가상 키가 없어 바로 해제할 수 있습니다.') : t('이 Provider의 마지막 키입니다. 해제하면 새 키를 등록할 때까지 요청이 거절됩니다.')}</p>
      </form>}
    </Modal>}
    {list.data && <>
      {providers.filter(p => p.status !== 'active').map(p => <section key={p.provider} aria-label={t('{{provider}} Provider 키 상태', { provider: providerNames[p.provider] ?? p.provider })} className={p.status === 'missing' ? 'card card-warning stack-sm' : 'card card-critical stack-sm'}><div className="row"><strong>{providerNames[p.provider] ?? p.provider}</strong><StatusBadge kind="credential" value={p.status} /></div>{statusNote[p.status] && <p className="meta">{t(statusNote[p.status])}</p>}</section>)}
      <div className="row row-between">
        <div className="field"><label htmlFor="credential-filter">Provider</label><Select id="credential-filter" value={filter} onChange={setFilter} options={[{ value: '', label: t('전체') }, ...providers.map(p => ({ value: p.provider, label: providerNames[p.provider] ?? p.provider }))]} /></div>
        <div className="actions"><button className="btn-primary" disabled={locked} onClick={() => { setStatus(null); setFormError(null); setAdding(filter); }}>{t('Provider 키 추가')}</button></div>
      </div>
      {rows.length === 0 ? <p className="empty">{t('등록된 Provider 키가 없습니다.')}</p> : <div className="table-wrap"><table aria-label={t('Provider 키')}>
        <thead><tr><th>Provider</th><th>{t('이름')}</th><th>{t('키 끝자리')}</th><th>{t('인가된 사용자')}</th><th>{t('연결된 가상 키')}</th><th>{t('등록')}</th><th>{t('Provider 확인')}</th><th>{t('동작')}</th></tr></thead>
        <tbody>{rows.map(({ provider, c }) => <tr key={c.id} aria-label={t('Provider 키 {{label}}', { label: c.label })}>
          <td>{providerNames[provider] ?? provider}</td>
          <td><strong>{c.label}</strong>{!c.decryptable && <> <Badge tone="critical">{t('복호화 불가')}</Badge></>}</td>
          <td className="mono"><span aria-hidden="true">••••</span>{c.hint}</td>
          <td>{t('{{count}}명', { count: c.authorized_principal_ids?.length ?? 0 })}</td>
          <td>{t('{{count}}개', { count: Number(c.bound_keys) })}</td>
          <td>{format(c.created_at)}<p className="meta">{c.created_by_email ?? c.created_by}</p></td>
          <td>{format(c.verified_at)}</td>
          <td><div className="actions">
            <IconButton icon="grant" label={t('{{label}} 사용자 인가', { label: c.label })} tip={t('사용자 인가')} disabled={locked} onClick={() => { setStatus(null); setGranting({ provider, target: c }); }} />
            <IconButton icon="edit" label={t('{{label}} 이름 변경', { label: c.label })} tip={t('이름 변경')} disabled={locked} onClick={() => { setFormError(null); setEditing({ id: c.id, kind: 'rename' }); }} />
            <IconButton icon="replace" label={t('{{label}} 키 교체', { label: c.label })} tip={t('키 교체')} disabled={locked} onClick={() => { setFormError(null); setEditing({ id: c.id, kind: 'replace' }); }} />
            <IconButton icon="revoke" danger label={t('{{label}} 해제', { label: c.label })} tip={t('해제')} disabled={locked} onClick={() => { setFormError(null); setEditing({ id: c.id, kind: 'retire' }); }} />
          </div></td>
        </tr>)}</tbody>
      </table></div>}
      {history.length > 0 && <details><summary>{t('해제 이력')}</summary><ul className="stack-sm history">{history.map(h => <li key={h.id}><p className="mono">{providerNames[h.provider] ?? h.provider} · {h.label} <span className="muted" aria-hidden="true">••••</span>{h.hint}</p><p className="meta">{t('등록 {{at}} ({{by}})', { at: format(h.created_at), by: h.created_by_email ?? h.created_by })} · {t('해제 {{at}} ({{by}})', { at: h.retired_at ? format(h.retired_at) : '', by: h.retired_by_email ?? t('알 수 없음') })}</p></li>)}</ul></details>}
    </>}
  </div>;
}
