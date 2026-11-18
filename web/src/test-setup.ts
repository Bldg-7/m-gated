/**
 * Vitest 공용 준비. jsdom에는 dialog의 showModal()·close()가 없어 열림 상태(open 속성)만 흉내 낸다.
 * 뒤 화면을 막는(inert) 동작과 브라우저의 Esc 처리는 실제 브라우저(Playwright)에서 확인한다.
 */
if (typeof HTMLDialogElement !== 'undefined' && typeof HTMLDialogElement.prototype.showModal !== 'function') {
  HTMLDialogElement.prototype.showModal = function showModal(this: HTMLDialogElement) { this.setAttribute('open', ''); };
  HTMLDialogElement.prototype.show = function show(this: HTMLDialogElement) { this.setAttribute('open', ''); };
  HTMLDialogElement.prototype.close = function close(this: HTMLDialogElement) {
    if (!this.hasAttribute('open')) return;
    this.removeAttribute('open');
    this.dispatchEvent(new Event('close'));
  };
}
