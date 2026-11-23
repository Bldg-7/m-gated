import { useId, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';

/** 표의 한 줄. key는 고른 상태를 기억하는 값이고, provider·model·capability는 서버 값 그대로다. */
export type GrantRow = { key: string; provider: string; providerLabel: string; model: string; capability: string; note?: ReactNode };

/**
 * 모델·API를 고르는 표. 한 줄이 (Provider, 모델, API) 하나다.
 * - 첫 칸 체크박스로 고르고, 머리 칸 체크박스는 표에 보이는 줄 전체를 켜고 끈다(일부만 고르면 중간 상태).
 * - 체크박스의 접근 가능한 이름은 "provider / model / API"(서버 값)다. 줄의 글자를 눌러도 고른다.
 * - 줄이 많으면 표 안에서 세로로 넘기고 머리 줄은 위에 붙어 있다.
 */
export function GrantTable({ rows, selected, onToggle, onAll, disabled = false }: { rows: GrantRow[]; selected: ReadonlySet<string>; onToggle: (key: string, on: boolean) => void; onAll: (on: boolean) => void; disabled?: boolean }) {
  const { t } = useTranslation();
  const id = useId();
  const all = rows.length > 0 && rows.every(r => selected.has(r.key)), some = rows.some(r => selected.has(r.key));
  return <div className="table-wrap table-scroll"><table className="grant-table">
    <thead><tr>
      <th className="cell-check"><input type="checkbox" aria-label={t('전체 선택')} disabled={disabled} checked={all} ref={el => { if (el) el.indeterminate = !all && some; }} onChange={e => onAll(e.currentTarget.checked)} /></th>
      <th>Provider</th><th>{t('모델')}</th><th>API</th>
    </tr></thead>
    <tbody>{rows.map((r, i) => {
      const box = `${id}-${i}`;
      return <tr key={r.key}>
        <td className="cell-check"><input id={box} type="checkbox" aria-label={`${r.provider} / ${r.model} / ${r.capability}`} disabled={disabled} checked={selected.has(r.key)} onChange={e => onToggle(r.key, e.currentTarget.checked)} /></td>
        <td><label htmlFor={box}>{r.providerLabel}</label></td>
        <td><label htmlFor={box}><span className="mono">{r.model}</span>{r.note}</label></td>
        <td><label htmlFor={box}><span className="mono">{r.capability}</span></label></td>
      </tr>;
    })}</tbody>
  </table></div>;
}
