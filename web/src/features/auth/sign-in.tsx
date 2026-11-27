import { useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { navigate } from '../../lib/router';
import { postAuth, type AuthReply } from './post';

/** 잠긴 시간(Retry-After, 초)을 분으로 올림한다. 값이 없거나 이상하면 잠금 시간(15분)을 쓴다. */
function lockedMinutes(retryAfter: string | null) {
  const seconds = Number(retryAfter);
  return Number.isFinite(seconds) && seconds > 0 ? Math.ceil(seconds / 60) : 15;
}

/**
 * 비밀번호 방식의 로그인 화면(/dashboard/signin). 로그인 없이 보인다.
 * 성공하면 서버가 세션 쿠키를 주고, 화면은 대시보드 첫 화면으로 간다.
 * 비밀번호를 잊은 사람은 관리자에게 재설정 링크를 받는다(스스로 재설정하는 메일은 없다).
 */
export function SignInScreen() {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false), [error, setError] = useState<string | null>(null);
  const changed = new URLSearchParams(location.search).get('reason') === 'password_changed';
  const message = (reply: AuthReply) => {
    if (reply.status === 401) return t('이메일 또는 비밀번호가 틀렸습니다.');
    if (reply.status === 429) return t('로그인에 여러 번 실패해 잠시 막혔습니다. {{minutes}}분 뒤 다시 시도하세요.', { minutes: String(lockedMinutes(reply.retryAfter)) });
    if (reply.status === 403) return t('이 계정은 로그인할 수 없습니다. 관리자에게 문의하세요.');
    return t('로그인하지 못했습니다. 잠시 뒤 다시 시도하세요.');
  };
  const submit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (busy) return;
    const form = new FormData(event.currentTarget);
    const email = String(form.get('email') ?? '').trim(), password = String(form.get('password') ?? '');
    if (!email || !password) return;
    setBusy(true); setError(null);
    try {
      const reply = await postAuth('/auth/password', { email, password });
      if (reply.status === 204) { navigate('/dashboard/', { replace: true }); return; }
      setError(message(reply));
    } catch {
      setError(t('서버에 연결할 수 없습니다. 잠시 뒤 다시 시도하세요.'));
    }
    setBusy(false);
  };
  return <div className="stack">
    <h1 className="auth-title">{t('로그인')}</h1>
    {changed && <p role="status">{t('비밀번호를 바꿨습니다. 모든 세션이 끝났습니다. 새 비밀번호로 다시 로그인하세요.')}</p>}
    {error && <p role="alert">{error}</p>}
    <form aria-label={t('로그인')} className="stack" onSubmit={event => void submit(event)}>
      <fieldset disabled={busy} className="stack">
        <div className="field"><label htmlFor="signin-email">{t('이메일')}</label><input id="signin-email" name="email" type="email" required maxLength={320} autoComplete="username" /></div>
        <div className="field"><label htmlFor="signin-password">{t('비밀번호')}</label><input id="signin-password" name="password" type="password" required maxLength={512} autoComplete="current-password" /></div>
        <button className="btn-primary" type="submit" aria-busy={busy || undefined}>{t('로그인')}</button>
      </fieldset>
    </form>
    <p className="meta">{t('비밀번호를 잊었으면 관리자에게 재설정 링크를 요청하세요.')}</p>
  </div>;
}
