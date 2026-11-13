import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import i18n, { msgid } from '../../lib/i18n';
import { ApiError, session, StaleResponse } from '../../lib/api';
import { failure, fields, ReadState, useResource } from './shared';
import type { CredentialList, CredentialMeta } from './types';
import { StatusBadge } from '../../ui/ui';

const names: Record<string, string> = { anthropic: 'Anthropic', openai: 'OpenAI', deepseek: 'DeepSeek' };
/** 상태 라벨은 StatusBadge(정상·미등록·복호화 불가)가 그리고, 여기에는 그 결과만 적는다. */
const statusNote: Record<string, string> = { missing: msgid('이 Provider 요청은 503으로 거절됩니다.'), undecryptable: msgid('CREDENTIAL_ENCRYPTION_KEY가 바뀌었습니다. 키를 다시 등록하세요.') };
type Pending = { kind: 'set'; provider: string; expected: string | null; hint: string } | { kind: 'retire'; provider: string; expected: string; hint: string };

function message(error: unknown) {
  if (error instanceof ApiError) {
    const known: Record<string, string> = {
      provider_credential_rejected: msgid('Provider가 이 키를 거절했습니다. 저장하지 않았습니다.'),
      provider_credential_unverified: msgid('Provider 확인 요청이 실패해 키가 유효한지 알 수 없습니다. 저장하지 않았습니다. 잠시 후 다시 시도하세요.'),
      credential_conflict: msgid('다른 관리자가 먼저 키를 바꿨습니다. 서버를 다시 조회했습니다. 현재 상태를 확인하고 다시 시도하세요.'),
      invalid_request: msgid('키 형식이 맞지 않습니다. Anthropic 키는 sk-ant-, OpenAI·DeepSeek 키는 sk-로 시작합니다.'),
    };
    if (known[error.code]) return i18n.t(known[error.code]);
  }
  return failure(error);
}

/** Provider 회사 키 등록·교체·해제. 키 원문은 입력칸에서 전송 직전까지만 들고 있고 화면에 다시 보여 주지 않는다. */
export function CredentialsScreen() {
  const { t } = useTranslation();
  const list = useResource<CredentialList>('/gateway/admin/provider-credentials');
  const secret = useRef<string | null>(null), mounted = useRef(true);
  const [pending, setPending] = useState<Pending | null>(null), [busy, setBusy] = useState(false), [status, setStatus] = useState<string | null>(null), [formKey, setFormKey] = useState(0);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; secret.current = null; }; }, []);
  const cancel = () => { secret.current = null; setPending(null); setFormKey(v => v + 1); };
  const execute = async () => {
    if (!pending || busy) return;
    const action = pending, value = secret.current, epoch = session.snapshot().epoch, commandId = crypto.randomUUID();
    secret.current = null; setPending(null); setBusy(true); setStatus(null); setFormKey(v => v + 1);
    const current = () => mounted.current && session.snapshot().epoch === epoch;
    let outcome: string;
    try {
      if (action.kind === 'set') await session.write(`/gateway/admin/provider-credentials/${action.provider}`, 'PUT', commandId, { secret: value, expected_credential_id: action.expected });
      else await session.write(`/gateway/admin/provider-credentials/${action.provider}?expected_credential_id=${encodeURIComponent(action.expected)}`, 'DELETE', commandId);
      outcome = action.kind === 'set' ? t('{{provider}} 키(…{{hint}})를 확인하고 저장했습니다.', { provider: names[action.provider], hint: action.hint }) : t('{{provider}} 키(…{{hint}})를 해제했습니다.', { provider: names[action.provider], hint: action.hint });
    } catch (e) {
      if (!current() || e instanceof StaleResponse) return;
      outcome = t('{{reason}} 명령 ID: {{id}}.', { reason: message(e), id: commandId });
    }
    try { await list.reload(); } catch { /* ReadState가 조회 실패를 표시한다 */ }
    if (current()) { setStatus(outcome); setBusy(false); }
  };
  return <div className="stack-lg">
    <ReadState {...list} />{status && <p role="status">{status}</p>}{busy && <p role="status">{t('Provider 확인·저장 중…')}</p>}
    {pending && <section aria-label={t('회사 키 명령 확인')} className={pending.kind === 'retire' ? 'confirm confirm-danger' : 'confirm'}><h3>{pending.kind === 'set' ? t('{{provider}} 키 등록 확인', { provider: names[pending.provider] }) : t('{{provider}} 키 해제 확인', { provider: names[pending.provider] })}</h3>
      <p>{pending.kind === 'set' ? <>{t('끝자리 …{{hint}} 키를 Provider에 확인한 뒤 저장합니다.', { hint: pending.hint })}{pending.expected && <> {t('지금 쓰는 키는 해제되고 암호문이 지워집니다.')}</>}</> : t('끝자리 …{{hint}} 키를 해제합니다. 해제 후 이 Provider 요청은 새 키를 등록할 때까지 거절됩니다.', { hint: pending.hint })}</p>
      <div className="actions"><button autoFocus className={pending.kind === 'retire' ? 'btn-danger' : 'btn-primary'} disabled={busy} onClick={() => void execute()}>{t('명령 실행')}</button><button className="btn-quiet" disabled={busy} onClick={cancel}>{t('취소')}</button></div></section>}
    <div className="grid-cards">{list.data?.providers.map(p => <section key={p.provider} aria-label={t('{{provider}} 회사 키', { provider: names[p.provider] })} className={p.status === 'active' ? 'card' : p.status === 'missing' ? 'card card-warning' : 'card card-critical'}>
      <div className="row row-between"><h3>{names[p.provider]}</h3><StatusBadge kind="credential" value={p.status} /></div>
      {statusNote[p.status] && <p className="meta">{t(statusNote[p.status])}</p>}
      {p.active && <Meta meta={p.active} />}
      <form key={`${p.provider}:${p.active?.id ?? 'none'}:${formKey}`} aria-label={t('{{provider}} 키 등록', { provider: names[p.provider] })} onSubmit={e => { const f = fields(e), value = String(f.get('secret') ?? ''); setStatus(null); if (value.length < 20) { setStatus(message(new ApiError(400, 'invalid_request'))); return; } secret.current = value; setPending({ kind: 'set', provider: p.provider, expected: p.active?.id ?? null, hint: value.slice(-4) }); }}>
        <fieldset disabled={busy || pending !== null} className="stack"><legend className="sr-only">{p.active ? t('키 교체') : t('키 등록')}</legend><label>{t('{{provider}} API 키', { provider: names[p.provider] })}<input name="secret" type="password" autoComplete="off" spellCheck={false} required maxLength={512} /></label><div className="actions"><button className={p.active ? undefined : 'btn-primary'}>{p.active ? t('교체 확인') : t('등록 확인')}</button>{p.active && <button type="button" className="btn-danger-outline" disabled={busy || pending !== null} onClick={() => { secret.current = null; setPending({ kind: 'retire', provider: p.provider, expected: p.active!.id, hint: p.active!.hint }); }}>{t('키 해제')}</button>}</div></fieldset>
      </form>
      {p.history.length > 0 && <details><summary>{t('최근 이력')}</summary><ul className="stack-sm history">{p.history.map(h => <li key={h.id}><Meta meta={h} /></li>)}</ul></details>}
    </section>)}</div>
  </div>;
}
function Meta({ meta }: { meta: CredentialMeta }) {
  const { t } = useTranslation();
  return <div className="stack-sm"><p className="mono"><span className="muted" aria-hidden="true">••••</span>{meta.hint} <span className="meta">{meta.state === 'active' ? t('사용 중') : t('해제됨')} · {t('버전 {{version}}', { version: meta.version })}</span></p><p className="meta">{t('등록 {{at}} ({{by}}) · Provider 확인 {{verified}}', { at: meta.created_at, by: meta.created_by_email ?? meta.created_by, verified: meta.verified_at })}{meta.retired_at && <> · {t('해제 {{at}} ({{by}})', { at: meta.retired_at, by: meta.retired_by_email ?? t('알 수 없음') })}</>}</p></div>;
}
