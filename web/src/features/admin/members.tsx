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
  /** 비밀번호를 정했는지(결정 0011). 비밀번호 방식에서만 화면에 보인다. */
  password_set?: boolean;
};

/** 게이트웨이를 쓸 수 있는 사람과 관리자를 정하는 접근 목록. 아직 로그인하지 않은 사람도 미리 더할 수 있다. */
export function MembersScreen() {
  const { t } = useTranslation();
  const view = useResource<{ items: AccessEntry[] }>('/api/admin/access');
  // 비밀번호 방식에서 사람을 추가했는데 초대 메일을 보내지 못했는지. 다음 명령을 보내면 지운다.
  const [inviteFailed, setInviteFailed] = useState(false);
  const command = useCommand(async ack => { setInviteFailed(ack?.invitation === 'failed'); await view.reload(); });
  const me = session.snapshot().me?.principal.email;
  const password = session.snapshot().me?.auth.mode === 'password';
  const [role, setRole] = useState<'user' | 'admin'>('user');
  const add = (event: FormEvent<HTMLFormElement>) => {
    const form = fields(event), email = String(form.get('email') ?? '').trim();
    if (!email) return;
    command.prepare({ label: msgid('접근 목록에 추가'), url: '/api/admin/access', method: 'POST', body: { email, role } });
  };
  return <div className="stack-lg">
    <ReadState {...view} />
    {command.view}
    {inviteFailed && <p role="alert">{t('초대 메일을 보내지 못했습니다. 목록의 초대 다시 보내기 버튼으로 다시 보내세요.')}</p>}
    <form aria-label={t('사람 추가')} className="card stack" onSubmit={add}>
      <h3>{t('사람 추가')}</h3>
      <p className="meta">{t('목록에 있는 이메일만 로그인할 수 있습니다. 아직 로그인한 적 없는 사람도 미리 더할 수 있습니다.')}{password && <> {t('추가하면 초대 메일을 보냅니다.')}</>}</p>
      <fieldset disabled={command.busy} className="row">
        <div className="field"><label htmlFor="access-email">{t('이메일')}</label><input id="access-email" name="email" type="email" required maxLength={320} autoComplete="off" /></div>
        <div className="field"><label htmlFor="access-role">{t('역할')}</label><Select id="access-role" value={role} onChange={value => setRole(value as 'user' | 'admin')} options={[{ value: 'user', label: t('사용자') }, { value: 'admin', label: t('관리자') }]} /></div>
        <div className="actions"><button className="btn-primary">{t('추가')}</button></div>
      </fieldset>
    </form>
    {view.data && <div className="table-wrap"><table>
      <caption className="sr-only">{t('접근 목록')}</caption>
      <thead><tr><th scope="col">{t('이메일')}</th><th scope="col">{t('역할')}</th><th scope="col">{t('계정')}</th>{password && <th scope="col">{t('비밀번호')}</th>}<th scope="col">{t('추가한 사람')}</th><th scope="col">{t('추가 시각')}</th><th scope="col">{t('작업')}</th></tr></thead>
      <tbody>{view.data.items.map(entry => {
        const locked = entry.fixed || entry.email === me;
        const next = entry.role === 'admin' ? 'user' : 'admin';
        // 링크 보내기: 비밀번호가 없으면 초대, 있으면 재설정 링크를 메일로 보낸다. 자기 항목에는 없다.
        const linkLabel = entry.password_set ? msgid('재설정 링크 보내기') : msgid('초대 다시 보내기');
        const sendLink = password && entry.email !== me && <button className="btn-sm" disabled={command.busy} onClick={() => command.prepare({ label: linkLabel, url: `/api/admin/access/${entry.id}/link`, method: 'POST', body: { email: entry.email }, noBody: true })}>{t(linkLabel)}</button>;
        return <tr key={entry.id}>
          <td className="mono">{entry.email}</td>
          <td>{entry.role === 'admin' ? t('관리자') : t('사용자')}{entry.fixed && <> <span className="badge">{t('설정 파일 관리자')}</span></>}</td>
          <td>{entry.principal ? (entry.principal.disabled ? t('정지됨') : t('로그인함')) : t('아직 로그인하지 않음')}</td>
          {password && <td>{entry.password_set ? t('정함') : t('초대 대기')}</td>}
          <td>{entry.added_by_email ?? t('설정 파일')}</td>
          <td><Time value={entry.added_at} /></td>
          <td><div className="actions">{sendLink}{locked ? <span className="meta">{entry.fixed ? t('설정 파일에서만 바꿉니다') : t('자기 항목은 바꿀 수 없습니다')}</span> : <>
            <button className="btn-sm" disabled={command.busy} onClick={() => command.prepare({ label: next === 'admin' ? msgid('관리자로 바꾸기') : msgid('사용자로 바꾸기'), url: `/api/admin/access/${entry.id}`, method: 'PATCH', body: { role: next, expected_version: entry.version } })}>{next === 'admin' ? t('관리자로 바꾸기') : t('사용자로 바꾸기')}</button>
            <button className="btn-sm btn-danger" disabled={command.busy} onClick={() => command.prepare({ label: msgid('접근 목록에서 지우기'), url: `/api/admin/access/${entry.id}?expected_version=${encodeURIComponent(entry.version)}`, method: 'DELETE', body: { email: entry.email, expected_version: entry.version }, danger: true })}>{t('지우기')}</button>
          </>}</div></td>
        </tr>;
      })}</tbody>
    </table></div>}
  </div>;
}
