import { useId, useState, type ReactNode } from 'react';

const paths: Record<string, ReactNode> = {
  detail: <><circle cx="8" cy="8" r="6" /><path d="M8 7.5v3.5" /><path d="M8 5h.01" /></>,
  edit: <><path d="M10.5 2.5l3 3L6 13l-3.5.5L3 10z" /><path d="M9 4l3 3" /></>,
  revoke: <><circle cx="8" cy="8" r="6" /><path d="M3.8 3.8l8.4 8.4" /></>,
  grant: <><circle cx="6" cy="5.5" r="2.5" /><path d="M1.5 13.5c0-2.5 2-4 4.5-4s4.5 1.5 4.5 4" /><path d="M12.5 5v4" /><path d="M10.5 7h4" /></>,
  replace: <><path d="M13.5 8a5.5 5.5 0 1 1-1.6-3.9" /><path d="M13.5 2.5v3h-3" /></>,
};
export type IconName = 'detail' | 'edit' | 'revoke' | 'grant' | 'replace';

/**
 * 아이콘만 보이는 버튼. 글자 이름은 화면에 두지 않고 말풍선으로 보여 준다.
 * - `label`: 읽기 도구가 읽는 전체 이름(예: "team-a 키 회수").
 * - `tip`: 마우스를 올리거나 키보드로 옮겨 왔을 때 보이는 짧은 이름(예: "회수").
 * 말풍선은 화면 기준 위치에 그린다. 표의 가로 스크롤 영역에 잘리지 않게 하기 위해서다.
 */
export function IconButton({ icon, label, tip, danger = false, disabled, onClick }: { icon: IconName; label: string; tip: string; danger?: boolean; disabled?: boolean; onClick: () => void }) {
  const tipId = useId();
  const [at, setAt] = useState<{ left: number; top: number } | null>(null);
  const show = (el: HTMLElement) => { const r = el.getBoundingClientRect(); setAt({ left: r.left + r.width / 2, top: r.top }); };
  return <>
    <button type="button" className={danger ? 'icon-btn icon-btn-danger' : 'icon-btn'} aria-label={label} aria-describedby={at ? tipId : undefined} disabled={disabled} onClick={onClick}
      onMouseEnter={e => show(e.currentTarget)} onMouseLeave={() => setAt(null)} onFocus={e => show(e.currentTarget)} onBlur={() => setAt(null)}>
      <svg className="icon" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">{paths[icon]}</svg>
    </button>
    {at && <span id={tipId} role="tooltip" className="tooltip" style={{ left: at.left, top: at.top }}>{tip}</span>}
  </>;
}
