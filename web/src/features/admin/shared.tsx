import { useCallback, useEffect, useRef, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { ApiError, session, StaleResponse } from '../../lib/api';
import i18n, { msgid } from '../../lib/i18n';
import type { Page } from '../../lib/types';
import type { CommandAck, PrincipalSummary } from './types';
import { formatTime } from '../../lib/time';
import { Modal, RefreshIcon, SearchSelect, Time } from '../../ui/ui';
const messages: Record<string, string> = { version_conflict: msgid('버전이 변경되었습니다. 서버를 다시 조회했습니다. 새 상태를 확인하고 다시 선택하세요.'), execution_not_final: msgid('실행 종료를 확인할 수 없거나 알려진 sender가 실행 중입니다. 정산하지 않았습니다.'), settlement_conflict: msgid('다른 수동 처리와 상충합니다. 서버 처리 내역을 확인하세요.'), access_exists: msgid('이 이메일은 이미 접근 목록에 있습니다.'), access_protected: msgid('설정 파일의 고정 관리자와 자기 자신의 항목은 바꾸거나 지울 수 없습니다.'), security_fence_conflict: msgid('자기 자신과 마지막 활성 관리자는 정지할 수 없습니다. 이관 중 잠긴 계정은 이 화면에서 바꿀 수 없습니다.'), policy_period_conflict: msgid('기존 장부·예약이 있어 예산 기간 또는 모드를 변경할 수 없습니다.'), resource_not_found: msgid('대상이 없거나 현재 권한으로 조회할 수 없습니다.'), invalid_request: msgid('입력이 올바르지 않습니다.'), access_denied: msgid('현재 관리자 권한이 없습니다.'), invalid_filter: msgid('조회 필터가 올바르지 않습니다.'), invalid_cursor: msgid('페이지가 만료되었거나 조회 조건이 다릅니다. 처음부터 조회하세요.'), mail_failed: msgid('메일을 보내지 못했습니다. 잠시 뒤 다시 보내세요.') };
export function failure(error: unknown) {
  const t = (key: string, values?: Record<string, string>) => i18n.t(key, values);
  if (!(error instanceof ApiError)) return t('서버에 연결할 수 없습니다. 자동 재전송하지 않았습니다.');
  if (error.code === 'usage_detail_unavailable') return t('상세 사용량을 제공할 수 없습니다. 상세 가용 시작: {{from}}', { from: typeof error.details.detail_available_from === 'string' ? formatTime(error.details.detail_available_from) : t('미확인') });
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
/** 명령을 보내고 서버 결과를 기다리는 동안의 알림. 자동으로 다시 보내지 않는다는 것과, 화면을 닫아도 서버 처리는 이어질 수 있다는 것을 함께 알린다. */
export function CommandBusy() {
  const { t } = useTranslation();
  return <div role="status" className="callout callout-info"><div className="callout-body"><strong>{t('명령 처리·서버 확인 중…')}</strong><span>{t('자동으로 다시 보내지 않습니다. 이 창을 닫아도 서버 처리는 계속될 수 있습니다.')}</span></div></div>;
}
export function ReadState({ loading, error, reload, asOf }: { loading: boolean; error: string | null; reload: () => void; asOf?: string }) {
  const { t } = useTranslation();
  return <div className="stack-sm">
    {loading && <p role="status">{t('서버 조회 중…')}</p>}{error && <p role="alert">{error}</p>}
    <div className="row row-between">{asOf ? <span className="meta">{t('조회 시각')}: <Time value={asOf} /></span> : <span />}<button className="btn-sm" disabled={loading} aria-busy={loading || undefined} onClick={() => void Promise.resolve(reload()).catch(() => {})}><RefreshIcon />{t('서버 다시 조회')}</button></div>
  </div>;
}
/**
 * label은 msgid로 표시한 한국어 원문(화면에서 번역). danger: 되돌릴 수 없는 명령. 확인 창의 실행 버튼을 위험 색으로 그린다.
 * noBody: 본문 없이 보낸다(DELETE와 같음). body는 확인 창에 보여 줄 내용으로만 쓴다.
 */
export type Action = { label: string; url: string; method: 'POST' | 'PUT' | 'PATCH' | 'DELETE'; body: Record<string, unknown>; danger?: boolean; noBody?: true };
export function useCommand(onRead: (ack?: CommandAck) => Promise<void>) {
  const { t } = useTranslation();
  const [pending, setPending] = useState<Action | null>(null), [busy, setBusy] = useState(false), [message, setMessage] = useState<string | null>(null);
  const lock = useRef(false), mounted = useRef(true);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  const prepare = (action: Action) => { setMessage(null); setPending(action); };
  const execute = async (action: Action | null = pending) => {
    const pending = action;
    if (!pending || lock.current) return;
    lock.current = true; setBusy(true); setPending(null); setMessage(null);
    const epoch = session.snapshot().epoch, commandId = crypto.randomUUID();
    const current = () => mounted.current && session.snapshot().epoch === epoch;
    let ack: CommandAck | undefined;
    let outcome = '';
    try {
      // DELETE와 noBody 명령은 본문 없이 보낸다. 확인 창에는 body를 그대로 보여 준다.
      ack = await session.write<CommandAck>(pending.url, pending.method, commandId, pending.method === 'DELETE' || pending.noBody ? undefined : pending.body);
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
  // run: 호출한 쪽이 이미 확인 창에서 확인을 받은 명령을 바로 보낸다.
  return { prepare, run: (action: Action) => void execute(action), busy, view: <>{message && <p role="status">{message}</p>}{busy && <CommandBusy />}{pending && <Modal title={t(pending.label)} variant={pending.danger ? 'danger' : 'normal'} busy={busy} onClose={() => setPending(null)} confirm={{ label: t('명령 실행'), onConfirm: () => void execute() }}><pre>{JSON.stringify(pending.body, null, 2)}</pre><p className="meta">{t('서버 상태와 CAS 버전을 확인했습니다. 성공 후 서버를 다시 조회하며 자동 재전송하지 않습니다.')}</p></Modal>}</> };
}
export function fields(event: FormEvent<HTMLFormElement>) { event.preventDefault(); return new FormData(event.currentTarget); }
export function directId(name: string) { return new URLSearchParams(location.search).get(name) ?? ''; }

/** 페이지 단위 목록을 끝까지 읽는다. 고르는 칸의 후보처럼 전체가 필요한 곳에 쓴다. url이 바뀌면 다시 읽는다. */
export function useAll<T>(url: string) {
  const [items, setItems] = useState<T[] | null>(null), [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let current = true; setItems(null); setError(null);
    void (async () => {
      const all: T[] = [], base = `${url}${url.includes('?') ? '&' : '?'}limit=100`;
      let cursor: string | null = null;
      do {
        const page: Page<T> = await session.read<Page<T>>(`${base}${cursor ? `&cursor=${encodeURIComponent(cursor)}` : ''}`, () => current);
        all.push(...page.items); cursor = page.next_cursor;
      } while (cursor);
      if (current) setItems(all);
    })().catch(e => { if (current && !(e instanceof StaleResponse)) setError(failure(e)); });
    return () => { current = false; };
  }, [url]);
  return { items, error };
}
/** 사용자(사람·서비스 계정)를 이름·이메일로 찾아 고르는 칸. 폼에는 고른 사용자의 ID가 전송된다. */
/**
 * 요청 기록에 나온 모델을 고르는 SearchSelect. 값은 모델 이름이고 첫 항목은 '전체'(빈 값)다.
 * mine이면 내 요청 기록(/api/usage/models), 아니면 전체 요청 기록(관리자 전용)에서 읽는다.
 * 승인된 모델 목록에서 빠진 모델도 기록이 있으면 나온다. 같은 이름이 여러 Provider에 있으면 후보 하나로 묶는다.
 */
export function RequestModelSelect({ mine = false, ...props }: { id?: string; name?: string; defaultValue?: string; 'aria-label'?: string; mine?: boolean }) {
  const { t } = useTranslation();
  const models = useResource<{ items: { provider: string; model: string }[] }>(mine ? '/api/usage/models' : '/api/admin/requests/models');
  const byModel = new Map<string, string[]>();
  for (const m of models.data?.items ?? []) byModel.set(m.model, [...(byModel.get(m.model) ?? []), m.provider]);
  const options = [{ value: '', label: t('전체') }, ...[...byModel].map(([model, list]) => ({ value: model, label: `${model} (${list.join(', ')})` }))];
  return <SearchSelect {...props} placeholder={models.data ? t('선택하세요') : t('모델 목록 조회 중…')} options={options} />;
}
export function PrincipalSelect({ kind, optional = false, ...props }: { id?: string; name?: string; value?: string; defaultValue?: string; onChange?: (value: string) => void; required?: boolean; kind?: 'human' | 'service'; optional?: boolean; 'aria-label'?: string }) {
  const { t } = useTranslation();
  const { items, error } = useAll<PrincipalSummary>(`/api/admin/principals${kind ? `?kind=${kind}` : ''}`);
  const options = (items ?? []).map(p => ({ value: p.id, label: p.email ? `${p.display_name} (${p.email})` : `${p.display_name} (${t('서비스')})` }));
  return <><SearchSelect {...props} placeholder={items ? t('선택하세요') : error ? t('사용자 목록을 가져올 수 없습니다') : t('사용자 목록 조회 중…')} options={optional ? [{ value: '', label: t('전체') }, ...options] : options} />{error && <p role="alert">{error}</p>}</>;
}
