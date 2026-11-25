import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ApiError, session, StaleResponse } from '../../lib/api';
import type { BudgetTimezone } from '../../lib/types';
import { failure, ReadState, useResource } from './shared';
import { useTimeFormat } from '../../lib/time';
import { SearchSelect, Time } from '../../ui/ui';

/** 브라우저가 아는 IANA 시간대 목록. UTC는 목록에 없을 수 있어 앞에 둔다. */
export function timezones(): string[] {
  const intl = Intl as unknown as { supportedValuesOf?: (key: string) => string[] };
  const listed = intl.supportedValuesOf?.('timeZone') ?? ['Asia/Seoul'];
  return ['UTC', ...listed.filter(z => z !== 'UTC')];
}

// 저장 결과 문구를 화면이 다시 만들어지는 사이에 잠깐 맡아 둔다.
// 시간대를 저장하면 로그인 정보(/api/me)를 다시 확인하고, 그동안 화면 전체가 사라졌다 다시 만들어진다.
// 그래서 문구를 컴포넌트 안에만 두면 사용자가 보기 전에 사라진다. 화면이 여러 번 다시 만들어질 수 있어 읽을 때 지우지 않고,
// 10초가 지난 문구만 버린다.
let carried: { text: string; at: number } | null = null;
function carriedText() { return carried && Date.now() - carried.at < 10000 ? carried.text : null; }

/** 끊긴 요청의 정산 규칙과 사후 보정 스위치. 이번 단계에서는 Anthropic 규칙만 쓴다.
 * correction_available: 이 서버에 Admin API 키가 있어 사후 보정을 켤 수 있는지. */
type CutoffRule = { provider: 'anthropic'; cutoff_rule: 'received' | 'max_tokens'; correction: boolean; correction_available: boolean; version: string };

/** 게이트웨이 전체 설정. 월 예산의 기준 시간대와 끊긴 요청의 정산 규칙이다. */
export function SettingsScreen() {
  const { t } = useTranslation();
  const view = useResource<{ budget_timezone: BudgetTimezone; cutoff_rule: CutoffRule }>('/api/admin/settings');
  const tz = view.data?.budget_timezone, rule = view.data?.cutoff_rule;
  // 저장 결과 문구. 저장한 뒤 설정을 다시 읽는 동안 폼이 사라졌다 다시 나오므로, 문구는 폼 밖에 둔다.
  const [status, setLocal] = useState<string | null>(carriedText);
  const setStatus = (text: string | null) => { carried = text ? { text, at: Date.now() } : null; setLocal(text); };
  return <div className="stack-lg">
    <ReadState {...view} />
    {status && <p role="status">{status}</p>}
    {tz && <BudgetTimezoneForm key={tz.version} current={tz} setStatus={setStatus} onSaved={async () => { await view.reload(); await session.validate('automatic'); }} />}
    {rule && <CutoffRuleForm key={rule.version} current={rule} setStatus={setStatus} onSaved={async () => { await view.reload(); }} />}
    {!tz && !view.error && <p role="status">{t('설정을 불러오는 중…')}</p>}
  </div>;
}

function BudgetTimezoneForm({ current, onSaved, setStatus }: { current: BudgetTimezone; onSaved: () => Promise<void>; setStatus: (text: string | null) => void }) {
  const { t } = useTranslation();
  const target = current.pending?.timezone ?? current.timezone;
  const [value, setValue] = useState(target), [busy, setBusy] = useState(false);
  const mounted = useRef(true), format = useTimeFormat();
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  const save = async () => {
    const epoch = session.snapshot().epoch, commandId = crypto.randomUUID();
    const live = () => mounted.current && session.snapshot().epoch === epoch;
    setBusy(true); setStatus(null);
    try {
      await session.write('/api/admin/settings/budget-timezone', 'PUT', commandId, { timezone: value, expected_version: current.version });
      if (!live()) return;
      setStatus(value === current.timezone ? t('예약된 변경을 취소했습니다. 계속 {{timezone}} 기준입니다.', { timezone: current.timezone }) : t('다음 예산 기간부터 {{timezone}} 기준으로 바뀝니다.', { timezone: value }));
      await onSaved();
    } catch (e) {
      if (!live() || e instanceof StaleResponse) return;
      setStatus(e instanceof ApiError && e.code === 'version_conflict' ? t('다른 관리자가 먼저 바꿨습니다. 다시 조회했습니다. 확인하고 다시 저장하세요.') : t('{{reason}} 명령 ID: {{id}}.', { reason: failure(e), id: commandId }));
      if (e instanceof ApiError && e.code === 'version_conflict') await onSaved();
    } finally { if (live()) setBusy(false); }
  };
  return <form aria-label={t('예산 기준 시간대')} className="card stack" onSubmit={e => { e.preventDefault(); void save(); }}>
    <h3>{t('예산 기준 시간대')}</h3>
    <p className="meta">{t('사용자 정책과 가상 키의 월 예산은 모두 이 시간대의 달력 한 달로 셉니다. 매달 1일 0시(이 시간대 기준)에 사용액이 0으로 돌아갑니다.')}</p>
    <dl className="stack-sm">
      <div><dt>{t('지금 시간대')}</dt><dd className="mono">{current.timezone}</dd></div>
      <div><dt>{t('이번 예산 기간')}</dt><dd><Time value={current.period_start} /> ~ <Time value={current.period_end} /></dd></div>
      {current.pending && <div><dt>{t('예약된 변경')}</dt><dd>{t('{{at}}부터 {{timezone}}. 바뀐 직후의 기간은 {{at}}부터 {{end}}까지입니다.', { at: format(current.pending.effective_from), timezone: current.pending.timezone, end: format(current.pending.first_period_end) })}</dd></div>}
    </dl>
    <fieldset disabled={busy} className="stack-sm">
      <div className="field"><label htmlFor="budget-timezone">{t('시간대')}</label><SearchSelect id="budget-timezone" value={value} onChange={setValue} options={timezones().map(z => ({ value: z, label: z }))} /></div>
      <p className="meta">{t('바꾸면 이번 기간은 지금 시간대로 끝까지 세고, 다음 기간부터 새 시간대를 씁니다. 바뀐 직후의 기간 하나만 이전 기간이 끝난 시각부터 새 시간대의 다음 달 1일 0시까지로 셉니다. 지금 시간대를 다시 고르면 예약된 변경이 취소됩니다.')}</p>
      <div className="actions"><button className="btn-primary" disabled={value === target}>{t('시간대 저장')}</button></div>
    </fieldset>
  </form>;
}

function CutoffRuleForm({ current, onSaved, setStatus }: { current: CutoffRule; onSaved: () => Promise<void>; setStatus: (text: string | null) => void }) {
  const { t } = useTranslation();
  const [value, setValue] = useState(current.cutoff_rule), [correction, setCorrection] = useState(current.correction), [busy, setBusy] = useState(false);
  const mounted = useRef(true);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  const save = async () => {
    const epoch = session.snapshot().epoch, commandId = crypto.randomUUID();
    const live = () => mounted.current && session.snapshot().epoch === epoch;
    setBusy(true); setStatus(null);
    try {
      await session.write('/api/admin/settings/cutoff-rule', 'PUT', commandId, { provider: current.provider, cutoff_rule: value, correction, expected_version: current.version });
      if (!live()) return;
      setStatus(t('정산 규칙을 저장했습니다. 지금부터 받는 요청에 적용합니다.'));
      await onSaved();
    } catch (e) {
      if (!live() || e instanceof StaleResponse) return;
      setStatus(e instanceof ApiError && e.code === 'version_conflict' ? t('다른 관리자가 먼저 바꿨습니다. 다시 조회했습니다. 확인하고 다시 저장하세요.') : t('{{reason}} 명령 ID: {{id}}.', { reason: failure(e), id: commandId }));
      if (e instanceof ApiError && e.code === 'version_conflict') await onSaved();
    } finally { if (live()) setBusy(false); }
  };
  const choices: { value: CutoffRule['cutoff_rule']; label: string; note: string }[] = [
    { value: 'received', label: t('받은 만큼'), note: t('끊기기 전에 받은 출력(텍스트, thinking, 도구 인자)의 토큰을 셉니다. thinking은 요약본만 받기 때문에 Provider가 청구하는 양보다 적게 잡힐 수 있습니다.') },
    { value: 'max_tokens', label: t('최대 출력(max_tokens)'), note: t('요청의 max_tokens만큼 출력을 셉니다. Provider가 청구하는 양보다 적게 잡히지는 않지만, 실제로 쓴 양보다 많이 잡힐 수 있습니다.') },
  ];
  return <form aria-label={t('끊긴 요청 정산 (Anthropic)')} className="card stack" onSubmit={e => { e.preventDefault(); void save(); }}>
    <h3>{t('끊긴 요청 정산 (Anthropic)')}</h3>
    <p className="meta">{t('사용자가 응답을 중간에 끊으면 Provider는 마지막 사용량을 보내지 않습니다. 이때 출력 토큰을 어떻게 셀지 정합니다. 입력 토큰은 Provider가 처음에 알려 준 값을 씁니다.')}</p>
    <fieldset disabled={busy} className="stack-sm">
      <legend>{t('출력 토큰을 세는 방법')}</legend>
      {choices.map(c => <label key={c.value}><input type="radio" name="cutoff_rule" value={c.value} checked={value === c.value} onChange={() => setValue(c.value)} /><span className="stack-sm"><strong>{c.label}</strong><span className="meta">{c.note}</span></span></label>)}
      <p className="meta">{t('받은 내용을 세지 못하면 최대 출력으로 정산합니다. 바꾸면 그 뒤에 받는 요청부터 적용합니다.')}</p>
      <label><input type="checkbox" name="correction" checked={correction} disabled={!current.correction_available && !correction} onChange={e => setCorrection(e.target.checked)} /><span className="stack-sm"><strong>{t('사후 보정')}</strong><span className="meta">{t('켜면 끊긴 요청의 출력 토큰을 요청이 끝나고 10~15분 뒤 Anthropic 사용량 보고서의 1분 단위 값으로 고칩니다. 보고서가 요청을 보낸 분과 끝난 분 가운데 어디에 넣는지 먼저 확인하고, 확인한 뒤에만 고칩니다. 그 분에 같은 API 키·모델로 들어간 다른 요청의 사용량을 모두 알고, 입력 토큰이 정확히 맞을 때만 고칩니다. 고친 금액은 원래 예산 기간에 반영하고 감사 기록에 남깁니다. 끝난 지 2시간이 넘은 요청은 고치지 않습니다.')}</span>{!current.correction_available && <span className="meta">{t('이 서버에는 Admin API 키(ANTHROPIC_ADMIN_API_KEY)가 없어 켤 수 없습니다.')}</span>}</span></label>
      <div className="actions"><button className="btn-primary" disabled={value === current.cutoff_rule && correction === current.correction}>{t('정산 규칙 저장')}</button></div>
    </fieldset>
  </form>;
}
