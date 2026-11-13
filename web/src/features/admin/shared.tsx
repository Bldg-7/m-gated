import { useCallback, useEffect, useRef, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { ApiError, session, StaleResponse } from '../../lib/api';
import i18n, { msgid } from '../../lib/i18n';
import type { CommandAck } from './types';
import { RefreshIcon } from '../../ui/ui';
const messages: Record<string, string> = { version_conflict: msgid('버전이 변경되었습니다. 서버를 다시 조회했습니다. 새 상태를 확인하고 다시 선택하세요.'), execution_not_final: msgid('실행 종료를 확인할 수 없거나 알려진 sender가 실행 중입니다. 정산하지 않았습니다.'), settlement_conflict: msgid('다른 수동 처리와 상충합니다. 서버 처리 내역을 확인하세요.'), security_fence_conflict: msgid('자기·마지막 관리자 정지 또는 보안 fence 해제 조건을 만족하지 못했습니다.'), policy_period_conflict: msgid('기존 장부·예약이 있어 예산 기간 또는 모드를 변경할 수 없습니다.'), resource_not_found: msgid('대상이 없거나 현재 권한으로 조회할 수 없습니다.'), invalid_request: msgid('입력이 올바르지 않습니다.'), access_denied: msgid('현재 관리자 권한이 없습니다.'), invalid_filter: msgid('조회 필터가 올바르지 않습니다.'), invalid_cursor: msgid('페이지가 만료되었거나 조회 조건이 다릅니다. 처음부터 조회하세요.') };
export function failure(error: unknown) {
  const t = (key: string, values?: Record<string, string>) => i18n.t(key, values);
  if (!(error instanceof ApiError)) return t('서버에 연결할 수 없습니다. 자동 재전송하지 않았습니다.');
  if (error.code === 'usage_detail_unavailable') return t('상세 사용량을 제공할 수 없습니다. 상세 가용 시작: {{from}}', { from: String(error.details.detail_available_from ?? t('미확인')) });
  return messages[error.code] ? t(messages[error.code]) : t('요청이 거부되었습니다 ({{code}}). 현재 상태를 다시 조회하세요.', { code: error.code });
}
export function useResource<T>(url: string) {
  const [data, setData] = useState<T | null>(null), [error, setError] = useState<string | null>(null), [loading, setLoading] = useState(true);
  const generation = useRef(0);
  const reload = useCallback(async () => {
    const token = ++generation.current; setData(null); setError(null); setLoading(true);
    try {
      const value = await session.read<T>(url, () => token === generation.current);
      if (token === generation.current) { setData(value); setLoading(false); }
    } catch (e) {
      if (token === generation.current && !(e instanceof StaleResponse)) { setData(null); setError(failure(e)); setLoading(false); }
      throw e;
    }
  }, [url]);
  useEffect(() => { void reload().catch(() => {}); return () => { generation.current++; }; }, [reload]);
  return { data, error, loading, reload };
}
export function ReadState({ loading, error, reload, asOf }: { loading: boolean; error: string | null; reload: () => void; asOf?: string }) {
  const { t } = useTranslation();
  return <div className="stack-sm">
    {loading && <p role="status">{t('서버 조회 중…')}</p>}{error && <p role="alert">{error}</p>}
    <div className="row row-between">{asOf ? <span className="meta">{t('관측 시각')}: <span className="mono">{asOf}</span></span> : <span />}<button className="btn-sm" disabled={loading} aria-busy={loading || undefined} onClick={() => void Promise.resolve(reload()).catch(() => {})}><RefreshIcon />{t('서버 다시 조회')}</button></div>
  </div>;
}
/** label은 msgid로 표시한 한국어 원문(화면에서 번역). danger: 되돌릴 수 없는 명령. 확인 창의 실행 버튼을 위험 색으로 그린다. */
export type Action = { label: string; url: string; method: 'POST' | 'PUT' | 'PATCH'; body: Record<string, unknown>; danger?: boolean };
export function useCommand(onRead: (ack?: CommandAck) => Promise<void>) {
  const { t } = useTranslation();
  const [pending, setPending] = useState<Action | null>(null), [busy, setBusy] = useState(false), [message, setMessage] = useState<string | null>(null);
  const lock = useRef(false), mounted = useRef(true);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  const prepare = (action: Action) => { setMessage(null); setPending(action); };
  const execute = async () => {
    if (!pending || lock.current) return;
    lock.current = true; setBusy(true); setPending(null); setMessage(null);
    const epoch = session.snapshot().epoch, commandId = crypto.randomUUID();
    const current = () => mounted.current && session.snapshot().epoch === epoch;
    let ack: CommandAck | undefined;
    let outcome = '';
    try {
      ack = await session.write<CommandAck>(pending.url, pending.method, commandId, pending.body);
      if (!current()) return;
      if (!ack) throw new Error('missing_ack');
      outcome = i18n.t('명령 {{id}} · {{effect}} · 결과 버전 {{version}}.', { id: commandId, effect: ack.effect, version: ack.result_version ?? i18n.t('없음') });
    } catch (e) {
      if (!current() || e instanceof StaleResponse) return;
      outcome = i18n.t('{{reason}} 명령 ID: {{id}}. 성공 여부는 서버 조회로 확인하세요. 쓰기는 재전송하지 않았습니다.', { reason: failure(e), id: commandId });
    }
    try {
      await onRead(ack);
      if (current()) setMessage(i18n.t('{{outcome}} 서버 재조회 완료.', { outcome }));
    } catch (e) { if (current() && !(e instanceof StaleResponse)) setMessage(i18n.t('{{outcome}} 재조회 실패. {{reason}}', { outcome, reason: failure(e) })); }
    finally { if (current()) { lock.current = false; setBusy(false); } }
  };
  return { prepare, busy, view: <>{message && <p role="status">{message}</p>}{busy && <p role="status">{t('명령 처리·서버 확인 중…')}</p>}{pending && <section aria-label={t('관리 명령 확인')} className={pending.danger ? 'confirm confirm-danger' : 'confirm'}><h3>{t(pending.label)}</h3><pre>{JSON.stringify(pending.body, null, 2)}</pre><p className="meta">{t('서버 상태와 CAS 버전을 확인했습니다. 성공 후 서버를 다시 조회하며 자동 재전송하지 않습니다.')}</p><div className="actions"><button autoFocus className={pending.danger ? 'btn-danger' : 'btn-primary'} disabled={busy} onClick={() => void execute()}>{t('명령 실행')}</button><button className="btn-quiet" disabled={busy} onClick={() => setPending(null)}>{t('취소')}</button></div></section>}</> };
}
export function fields(event: FormEvent<HTMLFormElement>) { event.preventDefault(); return new FormData(event.currentTarget); }
export function directId(name: string) { return new URLSearchParams(location.search).get(name) ?? ''; }
