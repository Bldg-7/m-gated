import { useEffect, useRef, useState } from 'react';
import { ApiError, session, StaleResponse } from '../../lib/api';
import { failure, fields, ReadState, useResource } from './shared';
import type { CredentialList, CredentialMeta } from './types';

const names: Record<string, string> = { anthropic: 'Anthropic', openai: 'OpenAI', deepseek: 'DeepSeek' };
const statusText: Record<string, string> = { active: '정상', missing: '미등록 (이 Provider 요청은 503으로 거절됩니다)', undecryptable: '복호화 불가 (CREDENTIAL_ENCRYPTION_KEY가 바뀌었습니다. 키를 다시 등록하세요)' };
type Pending = { kind: 'set'; provider: string; expected: string | null; hint: string } | { kind: 'retire'; provider: string; expected: string; hint: string };

function message(error: unknown) {
  if (error instanceof ApiError) {
    const known: Record<string, string> = {
      provider_credential_rejected: 'Provider가 이 키를 거절했습니다. 저장하지 않았습니다.',
      provider_credential_unverified: 'Provider 확인 요청이 실패해 키가 유효한지 알 수 없습니다. 저장하지 않았습니다. 잠시 후 다시 시도하세요.',
      credential_conflict: '다른 관리자가 먼저 키를 바꿨습니다. 서버를 다시 조회했습니다. 현재 상태를 확인하고 다시 시도하세요.',
      invalid_request: '키 형식이 맞지 않습니다. Anthropic 키는 sk-ant-, OpenAI·DeepSeek 키는 sk-로 시작합니다.',
    };
    if (known[error.code]) return known[error.code];
  }
  return failure(error);
}

/** Provider 회사 키 등록·교체·해제. 키 원문은 입력칸에서 전송 직전까지만 들고 있고 화면에 다시 보여 주지 않는다. */
export function CredentialsScreen() {
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
      outcome = action.kind === 'set' ? `${names[action.provider]} 키(…${action.hint})를 확인하고 저장했습니다.` : `${names[action.provider]} 키(…${action.hint})를 해제했습니다.`;
    } catch (e) {
      if (!current() || e instanceof StaleResponse) return;
      outcome = `${message(e)} 명령 ID: ${commandId}.`;
    }
    try { await list.reload(); } catch { /* ReadState가 조회 실패를 표시한다 */ }
    if (current()) { setStatus(outcome); setBusy(false); }
  };
  return <section><h2>Provider 회사 키</h2>
    <p>회사 키는 DB에 암호화해 저장합니다. 저장 전에 Provider의 모델 목록 API로 실제 확인을 1회 거치며, 거절되거나 확인이 안 되면 저장하지 않습니다. 저장한 키는 다시 볼 수 없고 끝 4자리만 표시합니다. 다른 서버 인스턴스에는 상태 점검 주기(수 초) 안에 반영되고, 진행 중인 요청은 시작할 때의 키를 끝까지 씁니다.</p>
    <ReadState {...list} />{status && <p role="status">{status}</p>}{busy && <p role="status">Provider 확인·저장 중…</p>}
    {pending && <section aria-label="회사 키 명령 확인"><h3>{pending.kind === 'set' ? `${names[pending.provider]} 키 등록 확인` : `${names[pending.provider]} 키 해제 확인`}</h3>
      <p>{pending.kind === 'set' ? `끝자리 …${pending.hint} 키를 Provider에 확인한 뒤 저장합니다.${pending.expected ? ' 지금 쓰는 키는 해제되고 암호문이 지워집니다.' : ''}` : `끝자리 …${pending.hint} 키를 해제합니다. 해제 후 이 Provider 요청은 새 키를 등록할 때까지 거절됩니다.`}</p>
      <button autoFocus disabled={busy} onClick={() => void execute()}>명령 실행</button><button disabled={busy} onClick={cancel}>취소</button></section>}
    {list.data?.providers.map(p => <section key={p.provider} aria-label={`${names[p.provider]} 회사 키`}><h3>{names[p.provider]}</h3>
      <p>상태: {statusText[p.status]}</p>
      {p.active && <Meta meta={p.active} />}
      <form key={`${p.provider}:${p.active?.id ?? 'none'}:${formKey}`} aria-label={`${names[p.provider]} 키 등록`} onSubmit={e => { const f = fields(e), value = String(f.get('secret') ?? ''); setStatus(null); if (value.length < 20) { setStatus(message(new ApiError(400, 'invalid_request'))); return; } secret.current = value; setPending({ kind: 'set', provider: p.provider, expected: p.active?.id ?? null, hint: value.slice(-4) }); }}>
        <fieldset disabled={busy || pending !== null}><legend>{p.active ? '키 교체' : '키 등록'}</legend><label>{names[p.provider]} API 키<input name="secret" type="password" autoComplete="off" spellCheck={false} required maxLength={512} /></label><button>{p.active ? '교체 확인' : '등록 확인'}</button></fieldset>
      </form>
      {p.active && <button disabled={busy || pending !== null} onClick={() => { secret.current = null; setPending({ kind: 'retire', provider: p.provider, expected: p.active!.id, hint: p.active!.hint }); }}>키 해제</button>}
      {p.history.length > 0 && <details><summary>최근 이력</summary><ul>{p.history.map(h => <li key={h.id}><Meta meta={h} /></li>)}</ul></details>}
    </section>)}
  </section>;
}
function Meta({ meta }: { meta: CredentialMeta }) {
  return <p>…{meta.hint} · {meta.state === 'active' ? '사용 중' : '해제됨'} · 등록 {meta.created_at} ({meta.created_by_email ?? meta.created_by}) · Provider 확인 {meta.verified_at}{meta.retired_at && ` · 해제 ${meta.retired_at} (${meta.retired_by_email ?? '알 수 없음'})`} · 버전 {meta.version}</p>;
}
