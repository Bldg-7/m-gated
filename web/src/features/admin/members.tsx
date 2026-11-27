import { useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { session } from '../../lib/api';
import { msgid } from '../../lib/i18n';
import { fields, ReadState, useCommand, useResource } from './shared';
import { Select, Time } from '../../ui/ui';

/** 접근 목록의 한 항목(결정 0001). fixed: 설정 파일의 고정 관리자. principal: 한 번이라도 로그인했으면 그 계정. */
export type AccessEntry = {
  id: string; email: string; role: 'user' | 'admin'; fixed: boolean; added_by_email: string | null; added_at: string; version: string;
  principal: { id: string; disabled: boolean } | null;
};

/** 게이트웨이를 쓸 수 있는 사람과 관리자를 정하는 접근 목록. 아직 로그인하지 않은 사람도 미리 더할 수 있다. */
export function MembersScreen() {
  const { t } = useTranslation();
  const view = useResource<{ items: AccessEntry[] }>('/api/admin/access');
  const command = useCommand(async () => { await view.reload(); });
  const me = session.snapshot().me?.principal.email;
  const [role, setRole] = useState<'user' | 'admin'>('user');
  const add = (event: FormEvent<HTMLFormElement>) => {
    const form = fields(event), email = String(form.get('email') ?? '').trim();
    if (!email) return;
    command.prepare({ label: msgid('접근 목록에 추가'), url: '/api/admin/access', method: 'POST', body: { email, role } });
  };
  return <div className="stack-lg">
    <ReadState {...view} />
    {command.view}
    <form aria-label={t('사람 추가')} className="card stack" onSubmit={add}>
      <h3>{t('사람 추가')}</h3>
      <p className="meta">{t('목록에 있는 이메일만 로그인할 수 있습니다. 아직 로그인한 적 없는 사람도 미리 더할 수 있습니다.')}</p>
      <fieldset disabled={command.busy} className="row">
        <div className="field"><label htmlFor="access-email">{t('이메일')}</label><input id="access-email" name="email" type="email" required maxLength={320} autoComplete="off" /></div>
        <div className="field"><label htmlFor="access-role">{t('역할')}</label><Select id="access-role" value={role} onChange={value => setRole(value as 'user' | 'admin')} options={[{ value: 'user', label: t('사용자') }, { value: 'admin', label: t('관리자') }]} /></div>
        <div className="actions"><button className="btn-primary">{t('추가')}</button></div>
      </fieldset>
    </form>
    {view.data && <div className="table-wrap"><table>
      <caption className="sr-only">{t('접근 목록')}</caption>
      <thead><tr><th scope="col">{t('이메일')}</th><th scope="col">{t('역할')}</th><th scope="col">{t('계정')}</th><th scope="col">{t('추가한 사람')}</th><th scope="col">{t('추가 시각')}</th><th scope="col">{t('작업')}</th></tr></thead>
      <tbody>{view.data.items.map(entry => {
        const locked = entry.fixed || entry.email === me;
        const next = entry.role === 'admin' ? 'user' : 'admin';
        return <tr key={entry.id}>
          <td className="mono">{entry.email}</td>
          <td>{entry.role === 'admin' ? t('관리자') : t('사용자')}{entry.fixed && <> <span className="badge">{t('설정 파일 관리자')}</span></>}</td>
          <td>{entry.principal ? (entry.principal.disabled ? t('정지됨') : t('로그인함')) : t('아직 로그인하지 않음')}</td>
          <td>{entry.added_by_email ?? t('설정 파일')}</td>
          <td><Time value={entry.added_at} /></td>
          <td><div className="actions">{locked ? <span className="meta">{entry.fixed ? t('설정 파일에서만 바꿉니다') : t('자기 항목은 바꿀 수 없습니다')}</span> : <>
            <button className="btn-sm" disabled={command.busy} onClick={() => command.prepare({ label: next === 'admin' ? msgid('관리자로 바꾸기') : msgid('사용자로 바꾸기'), url: `/api/admin/access/${entry.id}`, method: 'PATCH', body: { role: next, expected_version: entry.version } })}>{next === 'admin' ? t('관리자로 바꾸기') : t('사용자로 바꾸기')}</button>
            <button className="btn-sm btn-danger" disabled={command.busy} onClick={() => command.prepare({ label: msgid('접근 목록에서 지우기'), url: `/api/admin/access/${entry.id}?expected_version=${encodeURIComponent(entry.version)}`, method: 'DELETE', body: { email: entry.email, expected_version: entry.version }, danger: true })}>{t('지우기')}</button>
          </>}</div></td>
        </tr>;
      })}</tbody>
    </table></div>}
  </div>;
}
