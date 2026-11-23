import { useId, useLayoutEffect, useRef, type KeyboardEvent, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';

export type ModalProps = {
  /** 제목. 대화 상자의 접근 가능한 이름이 된다. */
  title: ReactNode;
  children: ReactNode;
  /** Esc·바깥(어두운 배경) 클릭·취소 버튼으로 닫을 때 부른다. busy 동안에는 부르지 않는다. */
  onClose: () => void;
  /** 명령 처리 중. 닫히지 않고 버튼을 막는다. */
  busy?: boolean;
  /** danger: 되돌릴 수 없거나 막는 명령. 실행 버튼을 위험 색으로 그리고, 처음 초점을 '취소'(data-modal-cancel)에 둔다. */
  variant?: 'normal' | 'danger';
  size?: 'small' | 'normal';
  /** 기본 실행 버튼. 주면 [실행][취소] 두 버튼을 그린다. */
  confirm?: { label: string; onConfirm: () => void; disabled?: boolean };
  cancelLabel?: string;
  /** 버튼 줄을 직접 그릴 때. 주면 confirm·cancelLabel은 쓰지 않는다. danger 창이면 취소 버튼에 data-modal-cancel을 단다. */
  actions?: ReactNode;
  /** false면 바깥 클릭으로 닫지 않는다(한 번만 보여 주는 키처럼 실수로 닫으면 안 되는 내용). */
  closeOnBackdrop?: boolean;
};

const focusable = 'button:not(:disabled), [href], input:not(:disabled):not([type="hidden"]):not([tabindex="-1"]), textarea:not(:disabled), select:not(:disabled), [tabindex]:not([tabindex="-1"])';

/**
 * 어두운 배경 위 가운데 카드로 뜨는 확인 창. 기본 dialog의 showModal()로 뒤 화면을 막는다(inert).
 * 열리면 첫 버튼으로(위험 창은 '취소'로) 초점을 옮기고, Tab은 창 안에서만 돌며, 닫히면 연 요소로 초점을 돌려준다.
 * 위험 창에서 Enter를 바로 눌러도 실행되지 않게 하기 위해서다.
 */
export function Modal({ title, children, onClose, busy = false, variant = 'normal', size = 'normal', confirm, cancelLabel, actions, closeOnBackdrop = true }: ModalProps) {
  const { t } = useTranslation();
  const dialog = useRef<HTMLDialogElement>(null), titleId = useId();
  const pressedBackdrop = useRef(false);
  const latest = useRef({ onClose, busy });
  latest.current = { onClose, busy };
  const requestClose = () => { if (!latest.current.busy) latest.current.onClose(); };
  const danger = variant === 'danger';
  useLayoutEffect(() => {
    const node = dialog.current!;
    const opener = document.activeElement instanceof HTMLElement && document.activeElement !== document.body ? document.activeElement : null;
    if (!node.open) {
      try { if (typeof node.showModal === 'function') node.showModal(); else node.setAttribute('open', ''); } catch { node.setAttribute('open', ''); }
    }
    ((danger ? node.querySelector<HTMLElement>('[data-modal-cancel]:not(:disabled)') : null) ?? node.querySelector<HTMLElement>('button:not(:disabled)') ?? node).focus();
    return () => {
      try { if (node.open && typeof node.close === 'function') node.close(); } catch { /* 이미 닫혔다 */ }
      if (opener?.isConnected) opener.focus();
    };
  }, []);
  const onKeyDown = (event: KeyboardEvent<HTMLDialogElement>) => {
    // 열린 목록(Select)이 Esc를 먼저 처리하면 전파를 멈추므로 여기까지 오지 않는다.
    if (event.key === 'Escape') { event.preventDefault(); requestClose(); return; }
    if (event.key !== 'Tab') return;
    const items = [...dialog.current!.querySelectorAll<HTMLElement>(focusable)];
    if (!items.length) { event.preventDefault(); return; }
    const first = items[0], last = items[items.length - 1], current = document.activeElement;
    if (event.shiftKey && (current === first || current === dialog.current)) { event.preventDefault(); last.focus(); }
    else if (!event.shiftKey && current === last) { event.preventDefault(); first.focus(); }
  };
  return <dialog ref={dialog} role="dialog" aria-modal="true" aria-labelledby={titleId} aria-busy={busy || undefined} tabIndex={-1}
    className={['modal', size === 'small' ? 'modal-small' : '', variant === 'danger' ? 'modal-danger' : ''].filter(Boolean).join(' ')}
    onKeyDown={onKeyDown}
    onCancel={event => { event.preventDefault(); requestClose(); }}
    onMouseDown={event => { pressedBackdrop.current = event.target === event.currentTarget; }}
    onClick={event => { if (closeOnBackdrop && event.target === event.currentTarget && pressedBackdrop.current) requestClose(); pressedBackdrop.current = false; }}>
    <div className="modal-card">
      <h2 id={titleId} className="modal-title">{title}</h2>
      <div className="modal-body">{children}</div>
      <div className="modal-actions">{actions ?? <>
        {confirm && <button type="button" className={variant === 'danger' ? 'btn-danger' : 'btn-primary'} disabled={busy || confirm.disabled} aria-busy={busy || undefined} onClick={confirm.onConfirm}>{confirm.label}</button>}
        <button type="button" className="btn-quiet" data-modal-cancel="" disabled={busy} onClick={requestClose}>{cancelLabel ?? t('취소')}</button>
      </>}</div>
    </div>
  </dialog>;
}
