import { useState, type ReactNode, type SelectHTMLAttributes, type TextareaHTMLAttributes } from 'react';
import { useTranslation } from 'react-i18next';
import { msgid } from '../lib/i18n';

/** 정밀 장부 디자인 시스템의 공용 표시 요소. 값은 서버 문자열을 그대로 쓰고 화면에서 새 숫자를 만들지 않는다. */
export type Tone = 'success' | 'warning' | 'critical' | 'info' | 'neutral' | 'blocked';

const paths: Record<Tone, string[]> = {
  success: ['M3.8 6.2l1.5 1.5 2.9-3.2'],
  warning: ['M6 3.4V6l1.8 1.2'],
  critical: ['M6 4v2.3', 'M6 8.1v.1'],
  info: ['M6 5.4v3', 'M6 3.6v.1'],
  neutral: ['M3.8 6h4.4'],
  blocked: ['M2.6 9.4l6.8-6.8'],
};
export function ToneIcon({ tone, size = 12 }: { tone: Tone; size?: number }) {
  return <svg className="icon" width={size} height={size} viewBox="0 0 12 12" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
    {tone === 'critical' ? <path d="M6 1.2L10.8 6 6 10.8 1.2 6z" /> : <circle cx="6" cy="6" r="5" />}
    {paths[tone].map(d => <path key={d} d={d} />)}
  </svg>;
}
export function RefreshIcon() {
  return <svg className="icon" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><path d="M13.5 8a5.5 5.5 0 1 1-1.6-3.9" /><path d="M13.5 2.5v3h-3" /></svg>;
}

export function Badge({ tone, children, title }: { tone: Tone; children: ReactNode; title?: string }) {
  return <span className={`badge tone-${tone}`} title={title}><ToneIcon tone={tone} />{children}</span>;
}
export function Pill({ children, accent = false }: { children: ReactNode; accent?: boolean }) {
  return <span className={accent ? 'pill pill-accent' : 'pill'}>{children}</span>;
}

type Kind = 'accounting' | 'phase' | 'outcome' | 'credential' | 'block';
const status: Record<Kind, Record<string, [string, Tone]>> = {
  accounting: { settled: [msgid('확정'), 'success'], pending: [msgid('정산 대기'), 'warning'], unknown: [msgid('미정산'), 'critical'] },
  phase: { admitted: [msgid('입장'), 'neutral'], dispatching: [msgid('전송 중'), 'info'], finished: [msgid('종료'), 'neutral'] },
  outcome: { completed: [msgid('완료'), 'success'], rejected: [msgid('거절'), 'warning'], failed: [msgid('실패'), 'critical'], cancelled: [msgid('취소됨'), 'neutral'] },
  credential: { active: [msgid('정상'), 'success'], missing: [msgid('미등록'), 'warning'], undecryptable: [msgid('복호화 불가'), 'critical'] },
  block: { unsettled: [msgid('미정산 차단'), 'blocked'], inference_paused: [msgid('자동 안전차단'), 'blocked'], security_fence: [msgid('보안 fence'), 'blocked'], disabled: [msgid('계정 정지'), 'neutral'] },
};
/** 서버 상태 코드를 톤 + 라벨로. 원문 코드는 title로 남긴다. 모르는 코드는 원문 그대로 중립 톤. */
export function StatusBadge({ kind, value }: { kind: Kind; value: string }) {
  const { t } = useTranslation();
  const known = status[kind][value];
  return <Badge tone={known ? known[1] : 'neutral'} title={value}>{known ? t(known[0]) : value}</Badge>;
}

/** 소수점 12자리 USD 문자열을 그대로 보여 주고, 마지막 유효 숫자 뒤의 0만 흐리게 한다. null은 0이 아니라 미확인. */
export function Money({ value }: { value: string | null }) {
  const { t } = useTranslation();
  if (value === null) return <span className="money money-unknown">{t('미확인')}</span>;
  const match = /^(-?\d+)\.(\d+)$/.exec(value);
  if (!match) return <span className="money">{value} USD</span>;
  const significant = match[2].replace(/0+$/, '');
  const head = significant ? `${match[1]}.${significant}` : match[1];
  const zeros = significant ? match[2].slice(significant.length) : `.${match[2]}`;
  return <span className="money"><span>{head}</span>{zeros && <span className="money-zeros">{zeros}</span>}{' '}<span className="money-unit">USD</span></span>;
}

/** 기본 화살표를 끈 select에 CSS 화살표를 붙이는 감싸개. CSP(img-src 'self') 때문에 배경 이미지를 쓰지 않는다. */
export function Select(props: SelectHTMLAttributes<HTMLSelectElement>) {
  return <span className="select"><select {...props} /></span>;
}

/**
 * 크기 조절 손잡이 없이 rows로 처음 높이를 정한다.
 * 내용이 길어질 수 있는 입력은 expandLabel(입력칸 이름)을 주면 우측 하단에 크게 보기 토글이 붙는다.
 */
export function TextArea({ expandLabel, ...props }: TextareaHTMLAttributes<HTMLTextAreaElement> & { id: string; rows: number; expandLabel?: string }) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  if (!expandLabel) return <textarea {...props} />;
  return <div className={expanded ? 'textarea-box expanded' : 'textarea-box'}>
    <textarea {...props} />
    <button type="button" className="textarea-toggle" aria-controls={props.id} aria-expanded={expanded} aria-label={t('{{name}} 입력칸 크게 보기', { name: expandLabel })} title={expanded ? t('원래 크기로') : t('크게 보기')} onClick={() => setExpanded(value => !value)}>
      <svg className="icon" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
        {expanded ? <><path d="M9.5 2.5v4h4" /><path d="M9.5 6.5l4-4" /><path d="M6.5 13.5v-4h-4" /><path d="M6.5 9.5l-4 4" /></> : <><path d="M9.5 2.5h4v4" /><path d="M13.5 2.5l-4 4" /><path d="M6.5 13.5h-4v-4" /><path d="M2.5 13.5l4-4" /></>}
      </svg>
    </button>
  </div>;
}
