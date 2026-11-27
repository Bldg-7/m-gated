import { useState, useSyncExternalStore, type FormEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { session } from '../../lib/api';
import { msgid } from '../../lib/i18n';
import { formatTime } from '../../lib/time';
import { Link } from '../../lib/router';
import { SecretValue } from '../keys/keys';
import { fields, ReadState, useCommand, useResource } from './shared';
import type { CommandAck } from './types';
import { Badge, IconButton, Modal, Time, TipBadge } from '../../ui/ui';

/** 관리 토큰 하나(결정 0014). 비밀값은 목록에 없다. */
export type ManagementToken = {
  id: string; name: string; scope: 'read' | 'write'; validity: '30_days' | '90_days' | '1_year';
  owner: { id: string; email: string | null; display_name: string };
  created_at: string; expires_at: string; revoked_at: string | null; revoked_by_email: string | null; last_used_at: string | null; active: boolean;
};
/** 관리자 한 명이 동시에 가질 수 있는 활성 토큰 수. 서버의 상한과 같다. */
export const ACTIVE_LIMIT = 30;
const scopes: ['read' | 'write', string][] = [['read', msgid('읽기')], ['write', msgid('쓰기')]];
const validities: [ManagementToken['validity'], string][] = [['30_days', msgid('30일')], ['90_days', msgid('90일')], ['1_year', msgid('1년')]];
const scopeName = (scope: string) => scopes.find(([key]) => key === scope)?.[1] ?? scope;

/**
 * 관리 토큰. 스크립트와 CI 작업이 관리 API를 부를 때 쓴다. 토큰은 발급한 관리자로 동작하고,
 * 비밀값은 발급 응답에서 한 번만 보인다. 어느 관리자든 모든 토큰을 보고 폐기할 수 있다.
 */
export function TokensScreen() {
  const { t } = useTranslation();
  const auth = useSyncExternalStore(session.subscribe, session.snapshot);
  const view = useResource<{ items: ManagementToken[] }>('/api/admin/management-tokens');
  // 발급한 토큰의 비밀값. 로그인 상태가 바뀌면(epoch) 보이지 않는다.
  const [secret, setSecret] = useState<{ value: string; name: string; epoch: number } | null>(null);
  // 발급에 성공하면 이름 칸을 비운다. 같은 이름으로 한 번 더 발급하는 실수를 줄인다.
  const [formKey, setFormKey] = useState(0);
  const command = useCommand(async ack => {
    const issued = ack as (CommandAck & { secret?: string; token?: ManagementToken }) | undefined;
    if (issued?.operation === 'create_management_token' && issued.secret) { setSecret({ value: issued.secret, name: issued.token?.name ?? '', epoch: session.snapshot().epoch }); setFormKey(key => key + 1); }
    await view.reload();
  });
  const [scope, setScope] = useState<'read' | 'write'>('read');
  const [validity, setValidity] = useState<ManagementToken['validity']>('90_days');
  const me = auth.me?.principal.id;
  const mine = (view.data?.items ?? []).filter(token => token.owner.id === me && token.active).length;
  const full = view.data !== null && mine >= ACTIVE_LIMIT;
  const issue = (event: FormEvent<HTMLFormElement>) => {
    const form = fields(event), name = String(form.get('name') ?? '').trim();
    if (!name) return;
    command.prepare({ label: msgid('관리 토큰 발급'), url: '/api/admin/management-tokens', method: 'POST', body: { name, scope, validity } });
  };
  return <div className="stack-lg">
    <ReadState {...view} />
    {command.view}
    <form aria-label={t('관리 토큰 발급')} className="card stack" onSubmit={issue}>
      <h3>{t('관리 토큰 발급')}</h3>
      <p className="meta">{t('토큰은 발급한 관리자로 동작합니다. 읽기 토큰은 조회만 하고, 쓰기 토큰은 명령도 보냅니다. 어느 토큰도 토큰을 만들거나 폐기할 수 없습니다.')} <Link href="/dashboard/api">{t('API 문서')}</Link></p>
      <fieldset disabled={command.busy} className="stack">
        <div className="field"><label htmlFor="token-name">{t('이름')}</label><input key={formKey} id="token-name" name="name" required maxLength={64} autoComplete="off" placeholder={t('예: 배포 스크립트')} /></div>
        <div className="form-grid">
          <fieldset className="field"><legend className="field-label">{t('범위')}</legend><div className="segment">{scopes.map(([value, label]) => <label key={value}><input type="radio" name="scope" value={value} checked={scope === value} onChange={() => setScope(value)} />{t(label)}</label>)}</div></fieldset>
          <fieldset className="field"><legend className="field-label">{t('유효 기간')}</legend><div className="segment">{validities.map(([value, label]) => <label key={value}><input type="radio" name="validity" value={value} checked={validity === value} onChange={() => setValidity(value)} />{t(label)}</label>)}</div></fieldset>
        </div>
        {full
          ? <p role="status" className="text-warning">{t('활성 토큰이 {{count}}개라 더 발급할 수 없습니다. 쓰지 않는 토큰을 아래 목록에서 폐기하세요.', { count: ACTIVE_LIMIT })}</p>
          : <div className="actions"><button className="btn-primary">{t('발급')}</button><span className="meta">{t('내 활성 토큰 {{count}}/{{limit}}개', { count: mine, limit: ACTIVE_LIMIT })}</span></div>}
      </fieldset>
    </form>
    {view.data && (view.data.items.length === 0
      ? <p className="meta">{t('발급한 관리 토큰이 없습니다.')}</p>
      : <div className="table-wrap"><table>
        <caption className="sr-only">{t('관리 토큰')}</caption>
        <thead><tr><th scope="col">{t('이름')}</th><th scope="col">{t('소유자')}</th><th scope="col">{t('범위')}</th><th scope="col">{t('상태')}</th><th scope="col">{t('만료 시각')}</th><th scope="col">{t('마지막 사용')}</th><th scope="col">{t('작업')}</th></tr></thead>
        <tbody>{view.data.items.map(token => <tr key={token.id}>
          <td>{token.name}</td>
          <td>{token.owner.display_name}{token.owner.email && <div className="meta mono">{token.owner.email}</div>}</td>
          <td>{t(scopeName(token.scope))}</td>
          <td>{token.revoked_at
            ? <TipBadge tone="neutral" tip={t('{{time}}에 {{email}}이(가) 폐기했습니다.', { time: formatTime(token.revoked_at), email: token.revoked_by_email ?? t('알 수 없음') })}>{t('폐기됨')}</TipBadge>
            : token.active ? <Badge tone="success">{t('활성')}</Badge> : <Badge tone="neutral">{t('만료됨')}</Badge>}</td>
          <td><Time value={token.expires_at} /></td>
          <td>{token.last_used_at ? <Time value={token.last_used_at} /> : <span className="meta">{t('쓴 적 없음')}</span>}</td>
          <td>{token.active && <IconButton icon="revoke" danger label={t('{{name}} 토큰 폐기', { name: token.name })} tip={t('폐기')} disabled={command.busy}
            onClick={() => command.prepare({ label: msgid('관리 토큰 폐기'), url: `/api/admin/management-tokens/${token.id}`, method: 'DELETE', body: { name: token.name, owner: token.owner.email ?? token.owner.display_name, scope: token.scope }, danger: true })} />}</td>
        </tr>)}</tbody>
      </table></div>)}
    {secret?.epoch === auth.epoch && <Modal title={t('발급된 관리 토큰')} closeOnBackdrop={false} onClose={() => setSecret(null)} actions={<button type="button" className="btn-primary" onClick={() => setSecret(null)}>{t('닫기')}</button>}>
      <p>{t('{{name}} 토큰의 비밀값입니다. 이 창을 닫으면 다시 볼 수 없습니다.', { name: secret.name })}</p>
      <SecretValue value={secret.value} copyLabel={t('토큰 복사')} />
      <p className="meta">{t('스크립트는 이 값을 Authorization: Bearer 헤더로 보냅니다.')}</p>
    </Modal>}
  </div>;
}
