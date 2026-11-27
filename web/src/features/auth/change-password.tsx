import { useId, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { ApiError, session, StaleResponse } from '../../lib/api';
import { navigate } from '../../lib/router';
import { Modal } from '../../ui/ui';
import { failure } from '../admin/shared';
import { passwordLengthOk } from './post';

/**
 * 비밀번호 방식에서 자기 비밀번호를 바꾸는 버튼과 창. 사이드바 계정 영역에 둔다.
 * 바꾸면 서버가 이 사람의 세션을 모두 끝내므로, 로그인 화면으로 가서 다시 로그인한다.
 */
export function ChangePasswordButton() {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false), [busy, setBusy] = useState(false), [error, setError] = useState<string | null>(null);
  const formId = useId();
  const close = () => { setOpen(false); setError(null); };
  const submit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (busy) return;
    const form = new FormData(event.currentTarget);
    const current = String(form.get('current_password') ?? ''), next = String(form.get('new_password') ?? ''), confirm = String(form.get('confirm') ?? '');
    setError(null);
    if (!current) { setError(t('지금 비밀번호를 적으세요.')); return; }
    if (!passwordLengthOk(next)) { setError(t('비밀번호는 12자 이상 128자 이하여야 합니다.')); return; }
    if (next !== confirm) { setError(t('두 비밀번호가 다릅니다.')); return; }
    setBusy(true);
    try {
      // 명령 ID 헤더는 서버가 무시한다. Session.write가 Origin·CSRF를 붙인다.
      await session.write('/api/me/password', 'POST', crypto.randomUUID(), { current_password: current, new_password: next });
      session.endedByPasswordChange();
      navigate('/dashboard/signin?reason=password_changed', { replace: true });
      return;
    } catch (e) {
      if (e instanceof StaleResponse) return;
      setError(e instanceof ApiError && e.code === 'current_password_invalid' ? t('지금 비밀번호가 틀렸습니다.')
        : e instanceof ApiError && e.code === 'password_invalid' ? t('비밀번호는 12자 이상 128자 이하여야 합니다.')
          : failure(e));
    }
    setBusy(false);
  };
  return <>
    <button className="btn-sm" onClick={() => setOpen(true)}>{t('비밀번호 바꾸기')}</button>
    {open && <Modal title={t('비밀번호 바꾸기')} size="small" busy={busy} onClose={close} closeOnBackdrop={false}
      actions={<>
        <button type="submit" form={formId} className="btn-primary" disabled={busy} aria-busy={busy || undefined}>{t('비밀번호 바꾸기')}</button>
        <button type="button" className="btn-quiet" data-modal-cancel="" disabled={busy} onClick={close}>{t('취소')}</button>
      </>}>
      <form id={formId} aria-label={t('비밀번호 바꾸기')} className="stack" onSubmit={event => void submit(event)}>
        {error && <p role="alert">{error}</p>}
        <fieldset disabled={busy} className="stack">
          <div className="field"><label htmlFor={`${formId}-current`}>{t('지금 비밀번호')}</label><input id={`${formId}-current`} name="current_password" type="password" required autoComplete="current-password" /></div>
          <div className="field"><label htmlFor={`${formId}-new`}>{t('새 비밀번호')}</label><input id={`${formId}-new`} name="new_password" type="password" required autoComplete="new-password" aria-describedby={`${formId}-help`} /><span id={`${formId}-help`} className="help">{t('12자 이상 128자 이하로 정하세요.')}</span></div>
          <div className="field"><label htmlFor={`${formId}-confirm`}>{t('새 비밀번호 확인')}</label><input id={`${formId}-confirm`} name="confirm" type="password" required autoComplete="new-password" /></div>
        </fieldset>
        <p className="meta">{t('바꾸면 모든 기기에서 로그아웃됩니다. 새 비밀번호로 다시 로그인하세요.')}</p>
      </form>
    </Modal>}
  </>;
}
