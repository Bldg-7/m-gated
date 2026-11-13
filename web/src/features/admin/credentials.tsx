import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import i18n, { msgid } from '../../lib/i18n';
import { ApiError, session, StaleResponse } from '../../lib/api';
import { failure, fields, ReadState, useResource } from './shared';
import type { CredentialList, CredentialMeta } from './types';
import { Badge, Select, StatusBadge } from '../../ui/ui';

export const providerNames: Record<string, string> = { anthropic: 'Anthropic', openai: 'OpenAI', deepseek: 'DeepSeek' };
/** 상태 라벨은 StatusBadge(정상·미등록·복호화 불가)가 그리고, 여기에는 그 결과만 적는다. */
const statusNote: Record<string, string> = { missing: msgid('이 Provider 요청은 503으로 거절됩니다.'), undecryptable: msgid('CREDENTIAL_ENCRYPTION_KEY가 바뀌었습니다. 키를 다시 등록하세요.') };
/** 키 원문을 다루는 명령(추가·원문 교체)과 해제는 확인 단계를 거친다. */
type Pending =
  | { kind: 'add'; provider: string; label: string; hint: string }
  | { kind: 'replace'; provider: string; target: CredentialMeta; hint: string }
  | { kind: 'retire'; provider: string; target: CredentialMeta; moveTo: CredentialMeta | null };
/** 키 하나에 대해 열어 둔 편집 칸. 한 번에 하나만 연다. */
type Editing = { id: string; kind: 'rename' | 'replace' | 'retire' };

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
      credential_in_use: msgid('기본 키이거나 가상 키가 연결된 키는 옮길 키를 고른 뒤에 해제할 수 있습니다.'),
      version_conflict: msgid('다른 관리자가 먼저 이 키를 바꿨습니다. 서버를 다시 조회했습니다. 현재 상태를 확인하고 다시 시도하세요.'),
      invalid_request: msgid('입력 형식이 맞지 않습니다. 이름은 앞뒤 공백 없이 1~60자이고, Anthropic 키는 sk-ant-, OpenAI·DeepSeek 키는 sk-로 시작합니다.'),
    };
    if (known[error.code]) return i18n.t(known[error.code]);
  }
  return failure(error);
}

/** Provider 회사 키 관리. 키 원문은 입력칸에서 전송 직전까지만 들고 있고 화면에 다시 보여 주지 않는다. */
export function CredentialsScreen() {
  const { t } = useTranslation();
  const list = useResource<CredentialList>('/gateway/admin/provider-credentials');
  const secret = useRef<string | null>(null), mounted = useRef(true);
  const [pending, setPending] = useState<Pending | null>(null), [editing, setEditing] = useState<Editing | null>(null);
  const [busy, setBusy] = useState(false), [status, setStatus] = useState<string | null>(null), [formKey, setFormKey] = useState(0);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; secret.current = null; }; }, []);
  const cancel = () => { secret.current = null; setPending(null); setFormKey(v => v + 1); };
  /** 명령 하나를 보내고 결과 문구를 남긴 뒤 목록을 다시 읽는다. 실패해도 자동으로 다시 보내지 않는다. */
  const send = async (url: string, method: 'POST' | 'PUT' | 'PATCH' | 'DELETE', body: unknown, done: string) => {
    const epoch = session.snapshot().epoch, commandId = crypto.randomUUID();
    const current = () => mounted.current && session.snapshot().epoch === epoch;
    setBusy(true); setStatus(null);
    let outcome: string;
    try {
      await session.write(url, method, commandId, body);
      outcome = done; setEditing(null);
    } catch (e) {
      if (!current() || e instanceof StaleResponse) return;
      outcome = t('{{reason}} 명령 ID: {{id}}.', { reason: message(e), id: commandId });
    }
    try { await list.reload(); } catch { /* ReadState가 조회 실패를 표시한다 */ }
    if (current()) { setStatus(outcome); setBusy(false); }
  };
  const execute = async () => {
    if (!pending || busy) return;
    const action = pending, value = secret.current, base = `/gateway/admin/provider-credentials/${action.provider}`, name = providerNames[action.provider];
    secret.current = null; setPending(null); setFormKey(v => v + 1);
    if (action.kind === 'add') await send(base, 'POST', { label: action.label, secret: value }, t('{{provider}} 키 {{label}}(…{{hint}})를 확인하고 저장했습니다.', { provider: name, label: action.label, hint: action.hint }));
    else if (action.kind === 'replace') await send(`${base}/${action.target.id}`, 'PUT', { secret: value, expected_version: action.target.version }, t('{{label}}의 원문을 …{{hint}}로 바꿨습니다. 연결된 가상 키는 새 원문을 씁니다.', { label: action.target.label, hint: action.hint }));
    else {
      const query = new URLSearchParams({ expected_version: action.target.version });
      if (action.moveTo) query.set('move_to', action.moveTo.id);
      await send(`${base}/${action.target.id}?${query}`, 'DELETE', undefined, action.moveTo ? t('{{label}}를 해제하고 연결을 {{to}}로 옮겼습니다.', { label: credentialLabel(action.target), to: credentialLabel(action.moveTo) }) : t('{{label}}를 해제했습니다.', { label: credentialLabel(action.target) }));
    }
  };
  const locked = busy || pending !== null;
  return <div className="stack-lg">
    <ReadState {...list} />{status && <p role="status">{status}</p>}{busy && <p role="status">{t('처리 중…')}</p>}
    {pending && <section aria-label={t('회사 키 명령 확인')} className={pending.kind === 'retire' ? 'confirm confirm-danger' : 'confirm'}>
      <h3>{pending.kind === 'add' ? t('{{provider}} 키 추가 확인', { provider: providerNames[pending.provider] }) : pending.kind === 'replace' ? t('{{label}} 원문 교체 확인', { label: pending.target.label }) : t('{{label}} 해제 확인', { label: pending.target.label })}</h3>
      <p>{pending.kind === 'add' ? t('끝자리 …{{hint}} 키를 Provider에 확인한 뒤 이름 {{label}}로 저장합니다.', { hint: pending.hint, label: pending.label })
        : pending.kind === 'replace' ? t('끝자리 …{{hint}} 키를 Provider에 확인한 뒤 {{label}}의 원문을 바꿉니다. 이전 원문은 해제되고 암호문이 지워집니다. 이름·기본 여부·가상 키 연결은 그대로 이어집니다.', { hint: pending.hint, label: credentialLabel(pending.target) })
        : pending.moveTo ? t('{{label}}를 해제합니다. 이 키에 연결된 가상 키는 {{to}}로 옮겨집니다.', { label: credentialLabel(pending.target), to: credentialLabel(pending.moveTo) })
        : t('{{label}}를 해제합니다. 이 Provider의 마지막 키라면 새 키를 등록할 때까지 요청이 거절됩니다.', { label: credentialLabel(pending.target) })}</p>
      <div className="actions"><button autoFocus className={pending.kind === 'retire' ? 'btn-danger' : 'btn-primary'} disabled={busy} onClick={() => void execute()}>{t('명령 실행')}</button><button className="btn-quiet" disabled={busy} onClick={cancel}>{t('취소')}</button></div>
    </section>}
    <div className="stack-lg">{list.data?.providers.map(p => {
      const name = providerNames[p.provider];
      return <section key={p.provider} aria-label={t('{{provider}} 회사 키', { provider: name })} className={p.status === 'active' ? 'card stack' : p.status === 'missing' ? 'card card-warning stack' : 'card card-critical stack'}>
        <div className="row row-between"><h3>{name}</h3><StatusBadge kind="credential" value={p.status} /></div>
        {statusNote[p.status] && <p className="meta">{t(statusNote[p.status])}</p>}
        {p.credentials.length > 0 && <ul className="list list-block">{p.credentials.map(c => {
          const others = p.credentials.filter(o => o.id !== c.id), mustMove = others.length > 0 && (c.default || c.bound_keys !== '0');
          const open = editing?.id === c.id ? editing.kind : null;
          return <li key={c.id} aria-label={t('회사 키 {{label}}', { label: c.label })} className="stack-sm">
            <div className="row row-between">
              <div className="row"><strong>{c.label}</strong>{c.default && <Badge tone="info">{t('기본 키')}</Badge>}{!c.decryptable && <Badge tone="critical">{t('복호화 불가')}</Badge>}<span className="mono meta"><span aria-hidden="true">••••</span>{c.hint}</span></div>
              <div className="actions">
                <button className="btn-sm" disabled={locked} onClick={() => setEditing(open === 'rename' ? null : { id: c.id, kind: 'rename' })}>{t('이름 변경')}</button>
                {!c.default && <button className="btn-sm" disabled={locked} onClick={() => void send(`/gateway/admin/provider-credentials/${p.provider}/${c.id}`, 'PATCH', { expected_version: c.version, default: true }, t('{{label}}를 기본 키로 지정했습니다.', { label: credentialLabel(c) }))}>{t('기본 키로 지정')}</button>}
                <button className="btn-sm" disabled={locked} onClick={() => setEditing(open === 'replace' ? null : { id: c.id, kind: 'replace' })}>{t('원문 교체')}</button>
                <button className="btn-sm btn-danger-outline" disabled={locked} onClick={() => setEditing(open === 'retire' ? null : { id: c.id, kind: 'retire' })}>{t('해제')}</button>
              </div>
            </div>
            <p className="meta">{t('연결된 가상 키 {{count}}개', { count: Number(c.bound_keys) })} · {t('버전 {{version}}', { version: c.version })} · {t('등록 {{at}} ({{by}}) · Provider 확인 {{verified}}', { at: c.created_at, by: c.created_by_email ?? c.created_by, verified: c.verified_at })}</p>
            {open === 'rename' && <form aria-label={t('{{label}} 이름 변경', { label: c.label })} className="row" onSubmit={e => { const label = String(fields(e).get('label') ?? ''); if (label === c.label) { setEditing(null); return; } void send(`/gateway/admin/provider-credentials/${p.provider}/${c.id}`, 'PATCH', { expected_version: c.version, label }, t('이름을 {{label}}로 바꿨습니다.', { label })); }}>
              <label>{t('새 이름')}<input name="label" defaultValue={c.label} required maxLength={60} autoFocus /></label><div className="actions"><button className="btn-primary" disabled={locked}>{t('저장')}</button><button type="button" className="btn-quiet" onClick={() => setEditing(null)}>{t('취소')}</button></div>
            </form>}
            {open === 'replace' && <form key={formKey} aria-label={t('{{label}} 원문 교체', { label: c.label })} className="row" onSubmit={e => { const value = String(fields(e).get('secret') ?? ''); setStatus(null); if (value.length < 20) { setStatus(message(new ApiError(400, 'invalid_request'))); return; } secret.current = value; setPending({ kind: 'replace', provider: p.provider, target: c, hint: value.slice(-4) }); }}>
              <label>{t('새 {{provider}} API 키', { provider: name })}<input name="secret" type="password" autoComplete="off" spellCheck={false} required maxLength={512} autoFocus /></label><div className="actions"><button disabled={locked}>{t('교체 확인')}</button><button type="button" className="btn-quiet" onClick={() => setEditing(null)}>{t('취소')}</button></div>
            </form>}
            {open === 'retire' && <form aria-label={t('{{label}} 해제', { label: c.label })} className="row" onSubmit={e => { const to = String(fields(e).get('move_to') ?? ''); setPending({ kind: 'retire', provider: p.provider, target: c, moveTo: others.find(o => o.id === to) ?? null }); }}>
              {others.length > 0 ? <div className="field"><label htmlFor={`move-${c.id}`}>{mustMove ? t('연결을 옮길 키 (필수)') : t('연결을 옮길 키')}</label><Select id={`move-${c.id}`} name="move_to" required={mustMove} defaultValue=""><option value="" disabled={mustMove}>{mustMove ? t('선택하세요') : t('옮기지 않음')}</option>{others.map(o => <option key={o.id} value={o.id}>{credentialLabel(o)}</option>)}</Select></div> : <p className="meta">{t('이 Provider의 마지막 키입니다. 해제하면 새 키를 등록할 때까지 요청이 거절됩니다.')}</p>}
              {mustMove && <p className="meta">{c.default ? t('기본 키를 해제하면 고른 키가 새 기본 키가 됩니다.') : t('이 키에 연결된 가상 키를 고른 키로 옮깁니다.')}</p>}
              <div className="actions"><button className="btn-danger-outline" disabled={locked}>{t('해제 확인')}</button><button type="button" className="btn-quiet" onClick={() => setEditing(null)}>{t('취소')}</button></div>
            </form>}
          </li>;
        })}</ul>}
        <form key={`${p.provider}:${formKey}`} aria-label={t('{{provider}} 키 추가', { provider: name })} onSubmit={e => { const f = fields(e), label = String(f.get('label') ?? '').trim(), value = String(f.get('secret') ?? ''); setStatus(null); if (!label || value.length < 20) { setStatus(message(new ApiError(400, 'invalid_request'))); return; } secret.current = value; setPending({ kind: 'add', provider: p.provider, label, hint: value.slice(-4) }); }}>
          <fieldset disabled={locked} className="stack-sm"><legend>{p.credentials.length ? t('키 추가') : t('키 등록')}</legend>
            <div className="form-grid"><label>{t('이름 (예: 팀·용도)')}<input name="label" required maxLength={60} autoComplete="off" /></label><label>{t('{{provider}} API 키', { provider: name })}<input name="secret" type="password" autoComplete="off" spellCheck={false} required maxLength={512} /></label></div>
            <p className="meta">{p.credentials.length ? t('추가한 키는 기본 키가 아닙니다. 가상 키를 발급할 때 고르거나, 기본 키로 지정해 쓰세요.') : t('처음 등록하는 키는 기본 키가 됩니다.')}</p>
            <div className="actions"><button className="btn-primary">{p.credentials.length ? t('추가 확인') : t('등록 확인')}</button></div>
          </fieldset>
        </form>
        {p.history.length > 0 && <details><summary>{t('해제 이력')}</summary><ul className="stack-sm history">{p.history.map(h => <li key={h.id}><p className="mono">{h.label} <span className="muted" aria-hidden="true">••••</span>{h.hint}</p><p className="meta">{t('등록 {{at}} ({{by}})', { at: h.created_at, by: h.created_by_email ?? h.created_by })} · {t('해제 {{at}} ({{by}})', { at: h.retired_at ?? '', by: h.retired_by_email ?? t('알 수 없음') })}</p></li>)}</ul></details>}
      </section>;
    })}</div>
  </div>;
}
