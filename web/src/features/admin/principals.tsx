import { useState } from 'react';
import { session } from '../../lib/api';
import type { Page, Grant, Limits } from '../../lib/types';
import { KeysScreen } from '../keys/keys';
import type { PrincipalDetail, PrincipalSummary } from './types';
import { directId, fields, ReadState, useCommand, useResource } from './shared';
export function PrincipalsScreen() {
  const [query, setQuery] = useState(''), [id, setId] = useState(directId('principal_id'));
  const page = useResource<Page<PrincipalSummary>>(`/gateway/admin/principals${query ? `?${query}` : ''}`);
  const create = useCommand(async ack => { if (ack) { await session.read<PrincipalDetail>(`/gateway/admin/principals/${encodeURIComponent(ack.target.id)}`); setId(ack.target.id); } await page.reload(); });
  return <section><h2>관리자 주체</h2><p>owner는 책임자 메타데이터이며 서비스 접근·관리 권한을 자동 부여하지 않습니다.</p>
    <form aria-label="주체 조회" onSubmit={e => { const f = fields(e), q = new URLSearchParams(); for (const key of ['kind', 'disabled', 'limit']) if (f.get(key)) q.set(key, String(f.get(key))); setQuery(q.toString()); if (q.toString() === query) void page.reload().catch(() => {}); }}><label htmlFor="principal-kind">주체 종류</label><select id="principal-kind" name="kind"><option value="">전체</option><option value="human">사람</option><option value="service">서비스</option></select><label htmlFor="principal-disabled">계정 정지</label><select id="principal-disabled" name="disabled"><option value="">전체</option><option value="true">정지</option><option value="false">활성</option></select><label>주체 페이지 크기<input name="limit" type="number" min="1" max="100" /></label><button>주체 조회</button></form>
    <ReadState {...page} />{page.data && <><p>관측 시각: {page.data.as_of}</p>{!page.data.items.length && <p>주체가 없습니다.</p>}<ul>{page.data.items.map(p => <li key={p.id}><button onClick={() => setId(p.id)}>{p.display_name} 상세</button><p>{p.id} · {p.kind} · {p.email ?? '이메일 없음'} · owner {p.management_owner_id ?? '없음'}</p><p>주체 버전 {p.version} / 정책 버전 {p.policy_version} / 안전 버전 {p.inference_safety.version} · {p.disabled ? '계정 정지' : '계정 활성'} · {p.inference_safety.paused ? '자동 안전차단' : '안전차단 없음'} · 차단 {p.block_state.reasons.join(', ')}</p></li>)}</ul>{page.data.next_cursor && <button onClick={() => setQuery(`cursor=${encodeURIComponent(page.data!.next_cursor!)}`)}>다음 주체 페이지</button>}</>}
    <form aria-label="주체 ID 조회" onSubmit={e => setId(String(fields(e).get('id')))}><label>주체 ID<input name="id" required /></label><button>상세 조회</button></form>
    <form aria-label="서비스 생성" onSubmit={e => { const f = fields(e); create.prepare({ label: 'deny-all 서비스 생성 확인', url: '/gateway/admin/principals', method: 'POST', body: { display_name: String(f.get('display_name')), management_owner_id: String(f.get('owner')) } }); }}><fieldset disabled={create.busy}><legend>서비스 생성 (초기 deny-all)</legend><label>새 서비스 이름<input name="display_name" required maxLength={100} /></label><label>새 서비스 owner ID<input name="owner" required /></label><button>서비스 생성 확인</button></fieldset></form>{create.view}
    <details><summary>전체 키 조회·회수</summary><KeysScreen adminAll /></details>
    {id && <PrincipalEditor key={id} id={id} />}
  </section>;
}
function PrincipalEditor({ id }: { id: string }) {
  const detail = useResource<PrincipalDetail>(`/gateway/admin/principals/${encodeURIComponent(id)}`);
  const command = useCommand(async () => { await detail.reload(); });
  const [formError, setFormError] = useState<string | null>(null), p = detail.data?.principal;
  return <section aria-label="주체 상세"><h2>주체 상세</h2><ReadState {...detail} />{formError && <p role="alert">{formError}</p>}{command.view}{p && <>
    <p>{p.display_name} · {p.id} · {p.kind} · 주체 버전 {p.version} · 정책 버전 {p.policy.version} · 안전 버전 {p.inference_safety.version}</p><p>owner: {p.management_owner_id ?? '없음'} · 과금 차단: {p.block_state.reasons.join(', ') || '없음'} · 전체 미정산 {p.block_state.global_unknown_count}</p>
    <details><summary>실행 설정 상한 (편집 불가)</summary><pre>{JSON.stringify(detail.data!.execution_ceiling, null, 2)}</pre></details>
    {p.kind === 'service' && <form key={`principal-${p.version}`} aria-label="서비스 변경" onSubmit={e => { const f = fields(e); command.prepare({ label: '서비스 변경 확인', url: `/gateway/admin/principals/${p.id}`, method: 'PATCH', body: { expected_version: p.version, display_name: String(f.get('display_name')), management_owner_id: String(f.get('owner')) } }); }}><fieldset disabled={command.busy}><legend>서비스 변경</legend><label>서비스 이름<input name="display_name" defaultValue={p.display_name} required maxLength={100} /></label><label>서비스 owner ID<input name="owner" defaultValue={p.management_owner_id ?? ''} required /></label><button>서비스 변경 확인</button></fieldset></form>}
    <form key={`policy-${p.policy.version}`} aria-label="정책 교체" onSubmit={e => {
      const f = fields(e); setFormError(null);
      try {
        const grants = JSON.parse(String(f.get('grants'))) as Grant[], limits = JSON.parse(String(f.get('limits'))) as Limits;
        if (!Array.isArray(grants) || typeof limits !== 'object' || !limits) throw new Error('shape');
        const body: Record<string, unknown> = { expected_version: p.policy.version, disabled: f.has('disabled'), grants, limits, reason: String(f.get('reason')) };
        if (f.has('fence_confirmed')) body.fence_release = { fence_ref: String(f.get('fence_ref')), old_processes_stopped_ref: String(f.get('stopped_ref')), confirmed: true };
        command.prepare({ label: '정책 전체 교체 확인', url: `/gateway/admin/principals/${p.id}/policy`, method: 'PUT', body });
      } catch { setFormError('권한 배열과 Limits 객체를 유효한 JSON으로 입력하세요.'); }
    }}><fieldset disabled={command.busy}><legend>정책 전체 교체</legend><p>서버 DTO를 그대로 편집합니다. Amount는 소수점 12자리 문자열, budget=null은 실행 상한 우회가 아닙니다. 자동 안전차단은 이 폼으로 해제되지 않습니다.</p><label><input type="checkbox" name="disabled" defaultChecked={p.policy.disabled} />관리용 계정 정지</label><label htmlFor="policy-grants">Grant JSON</label><textarea id="policy-grants" name="grants" defaultValue={JSON.stringify(p.policy.grants, null, 2)} required /><label htmlFor="policy-limits">Limits JSON</label><textarea id="policy-limits" name="limits" defaultValue={JSON.stringify(p.policy.limits, null, 2)} required /><label>정책 변경 사유<input name="reason" required maxLength={1000} /></label><details><summary>보안 설정 fence 해제 진술</summary><p>fence 설정 사유는 security_config_fence:승인참조입니다. 구 프로세스 종료 확인 전 해제하지 마세요.</p><label>Fence 참조<input name="fence_ref" /></label><label>구 프로세스 종료 확인 참조<input name="stopped_ref" /></label><label><input name="fence_confirmed" type="checkbox" />구 설정 프로세스 종료를 확인했습니다</label></details><button>정책 교체 확인</button></fieldset></form>
    <section aria-label="자동 안전차단"><h3>자동 과금 안전차단</h3><p>{p.inference_safety.paused ? '차단 중' : '해제 상태'} · 안전 버전 {p.inference_safety.version} · 사유 {p.inference_safety.reason ?? '없음'} · 시각 {p.inference_safety.paused_at ?? '없음'} · 원인 요청 {p.inference_safety.trigger_request_id ?? '없음'}</p><p>별도 해제는 unknown·계정 정지·fence·예산·키 회수를 해제하지 않습니다.</p><form key={`safety-${p.inference_safety.version}`} aria-label="명시 안전차단 해제" onSubmit={e => { const f = fields(e); if (!f.has('confirmed')) return; command.prepare({ label: '별도 안전차단 해제 확인', url: `/gateway/admin/principals/${p.id}/inference-pause/release`, method: 'POST', body: { expected_version: p.inference_safety.version, reason: String(f.get('reason')) } }); }}><fieldset disabled={command.busy}><label>안전차단 해제 사유<input name="reason" required maxLength={1000} /></label><label><input name="confirmed" type="checkbox" required />과금 원인과 남은 차단을 확인했습니다</label><button>명시 해제 확인</button></fieldset></form></section>
    <a href={`/ui/admin/usage?principal_id=${encodeURIComponent(p.id)}`}>이 주체 사용량·현재 잔액</a> <a href={`/ui/admin/requests?principal_id=${encodeURIComponent(p.id)}`}>이 주체 미정산</a>
    <KeysScreen key={`${p.id}:${session.snapshot().epoch}`} adminPrincipal={{ id: p.id, service: p.kind === 'service', grants: p.policy.grants }} />
  </>}</section>;
}
