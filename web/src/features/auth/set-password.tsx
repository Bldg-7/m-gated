import { useCallback, useEffect, useState, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { msgid } from '../../lib/i18n';
import { navigate } from '../../lib/router';
import { Time } from '../../ui/ui';
import { passwordLengthOk, postAuth } from './post';

/** POST /auth/password/link 의 답. kind: 초대, 비밀번호 재설정, 첫 관리자 설정 링크. */
export type LinkInfo = { email: string; kind: 'invitation' | 'reset' | 'setup'; password_set: boolean; expires_at: string | null };
type View = { state: 'loading' } | { state: 'invalid' } | { state: 'error' } | { state: 'ready'; info: LinkInfo };

const titles: Record<LinkInfo['kind'], string> = { invitation: msgid('초대 수락'), reset: msgid('비밀번호 재설정'), setup: msgid('첫 관리자 설정') };

/** 링크 주소의 fragment(#token=...)에서 토큰을 읽는다. 토큰은 서버 로그와 Referer에 남지 않도록 fragment에만 있다. */
function linkToken() {
  return new URLSearchParams(location.hash.slice(1)).get('token') ?? '';
}

/**
 * 메일·로그로 받은 링크로 비밀번호를 정하는 화면(/dashboard/password). 로그인 없이 보인다.
 * 처음 정하는 사람(password_set가 false)은 표시 이름도 정한다. 성공하면 바로 로그인된다.
 */
export function SetPasswordScreen() {
  const { t } = useTranslation();
  const [token] = useState(linkToken);
  const [view, setView] = useState<View>({ state: 'loading' });
  const [busy, setBusy] = useState(false), [error, setError] = useState<string | null>(null);
  const load = useCallback(async (current: () => boolean = () => true) => {
    if (!token) { setView({ state: 'invalid' }); return; }
    setView({ state: 'loading' });
    try {
      const reply = await postAuth<LinkInfo>('/auth/password/link', { token });
      if (!current()) return;
      if (reply.ok && reply.data) setView({ state: 'ready', info: reply.data });
      else setView({ state: reply.code === 'link_invalid' ? 'invalid' : 'error' });
    } catch {
      if (current()) setView({ state: 'error' });
    }
  }, [token]);
  useEffect(() => { let alive = true; void load(() => alive); return () => { alive = false; }; }, [load]);

  const submit = async (event: FormEvent<HTMLFormElement>, info: LinkInfo) => {
    event.preventDefault();
    if (busy) return;
    const form = new FormData(event.currentTarget);
    const name = String(form.get('display_name') ?? '').trim(), password = String(form.get('password') ?? ''), confirm = String(form.get('confirm') ?? '');
    setError(null);
    if (!info.password_set && ([...name].length < 1 || [...name].length > 100)) { setError(t('표시 이름은 1자 이상 100자 이하로 적으세요.')); return; }
    if (!passwordLengthOk(password)) { setError(t('비밀번호는 12자 이상 128자 이하여야 합니다.')); return; }
    if (password !== confirm) { setError(t('두 비밀번호가 다릅니다.')); return; }
    setBusy(true);
    try {
      const reply = await postAuth('/auth/password/set', { token, password, ...(info.password_set ? {} : { display_name: name }) });
      if (reply.status === 204) {
        // 다 쓴 토큰을 주소와 방문 기록에서 지운 뒤 대시보드로 간다.
        history.replaceState(null, '', location.pathname + location.search);
        navigate('/dashboard/', { replace: true });
        return;
      }
      if (reply.code === 'link_invalid') setView({ state: 'invalid' });
      else if (reply.code === 'password_invalid') setError(t('비밀번호는 12자 이상 128자 이하여야 합니다.'));
      else if (reply.code === 'invalid_request') setError(t('입력이 올바르지 않습니다.'));
      else setError(t('비밀번호를 정하지 못했습니다. 잠시 뒤 다시 시도하세요.'));
    } catch {
      setError(t('서버에 연결할 수 없습니다. 잠시 뒤 다시 시도하세요.'));
    }
    setBusy(false);
  };

  if (view.state === 'loading') return <p role="status">{t('링크 확인 중…')}</p>;
  if (view.state === 'invalid') return <div className="stack">
    <h1 className="auth-title">{t('비밀번호 정하기')}</h1>
    <p role="alert">{t('이 링크는 기한이 지났거나 이미 쓰였습니다. 관리자에게 새 링크를 요청하세요.')}</p>
    <a className="btn" href="/dashboard/signin">{t('로그인 화면으로')}</a>
  </div>;
  if (view.state === 'error') return <div className="stack">
    <h1 className="auth-title">{t('비밀번호 정하기')}</h1>
    <p role="alert">{t('링크를 확인하지 못했습니다. 잠시 뒤 다시 시도하세요.')}</p>
    <button type="button" onClick={() => void load()}>{t('다시 시도')}</button>
  </div>;
  const { info } = view;
  return <div className="stack">
    <h1 className="auth-title">{t(titles[info.kind])}</h1>
    <p className="meta">{t('비밀번호를 정하면 바로 로그인합니다.')}{info.expires_at && <> {t('링크 기한')}: <Time value={info.expires_at} /></>}</p>
    {error && <p role="alert">{error}</p>}
    <form aria-label={t('비밀번호 정하기')} className="stack" onSubmit={event => void submit(event, info)}>
      <fieldset disabled={busy} className="stack">
        {/* 비밀번호 관리 도구가 어느 계정의 비밀번호인지 알도록 이메일을 읽기 전용 칸으로 둔다. */}
        <div className="field"><label htmlFor="set-email">{t('이메일')}</label><input id="set-email" type="email" value={info.email} readOnly autoComplete="username" /></div>
        {!info.password_set && <div className="field"><label htmlFor="set-name">{t('표시 이름')}</label><input id="set-name" name="display_name" required maxLength={100} autoComplete="name" /></div>}
        <div className="field"><label htmlFor="set-password">{t('새 비밀번호')}</label><input id="set-password" name="password" type="password" required autoComplete="new-password" aria-describedby="set-password-help" /><span id="set-password-help" className="help">{t('12자 이상 128자 이하로 정하세요.')}</span></div>
        <div className="field"><label htmlFor="set-confirm">{t('새 비밀번호 확인')}</label><input id="set-confirm" name="confirm" type="password" required autoComplete="new-password" /></div>
        <button className="btn-primary" type="submit" aria-busy={busy || undefined}>{t('비밀번호 정하기')}</button>
      </fieldset>
    </form>
  </div>;
}
