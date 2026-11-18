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

/** 게이트웨이 전체 설정. 지금은 월 예산의 기준 시간대 하나다. */
export function SettingsScreen() {
  const { t } = useTranslation();
  const view = useResource<{ budget_timezone: BudgetTimezone }>('/api/admin/settings');
  const tz = view.data?.budget_timezone;
  return <div className="stack-lg">
    <ReadState {...view} />
    {tz && <BudgetTimezoneForm key={tz.version} current={tz} onSaved={async () => { await view.reload(); await session.validate('automatic'); }} />}
    {!tz && !view.error && <p role="status">{t('설정을 불러오는 중…')}</p>}
  </div>;
}

function BudgetTimezoneForm({ current, onSaved }: { current: BudgetTimezone; onSaved: () => Promise<void> }) {
  const { t } = useTranslation();
  const target = current.pending?.timezone ?? current.timezone;
  const [value, setValue] = useState(target), [busy, setBusy] = useState(false), [status, setStatus] = useState<string | null>(null);
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
    {status && <p role="status">{status}</p>}
  </form>;
}
