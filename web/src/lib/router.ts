import { createElement, useSyncExternalStore, type AnchorHTMLAttributes, type MouseEvent } from 'react';

/** 서버(src/dashboard.rs SCREENS)가 HTML을 내주는 경로와 같아야 한다. 여기 없는 경로로는 이동하지 않는다. */
export const SCREENS = ['/ui/', '/ui/keys', '/ui/admin/principals', '/ui/admin/usage', '/ui/admin/requests', '/ui/admin/credentials', '/ui/admin/models'] as const;
export type Screen = (typeof SCREENS)[number];
export type Route = { path: string; search: string; href: string };

const listeners = new Set<() => void>();
let current: Route = { path: '', search: '', href: '' };

/** 브라우저 주소가 기준이다. 테스트의 history.replaceState처럼 이벤트 없이 바뀐 주소도 다음 렌더에서 읽는다. */
function snapshot(): Route {
  const { pathname, search } = window.location;
  if (pathname + search !== current.href) current = { path: pathname, search, href: pathname + search };
  return current;
}
function subscribe(listener: () => void) {
  listeners.add(listener);
  window.addEventListener('popstate', listener);
  return () => { listeners.delete(listener); window.removeEventListener('popstate', listener); };
}
export function useRoute(): Route {
  return useSyncExternalStore(subscribe, snapshot);
}
export function isScreen(path: string): path is Screen {
  return (SCREENS as readonly string[]).includes(path);
}
/** 같은 문서 안에서 화면만 바꾼다. 서버가 모르는 경로나 다른 origin은 일반 이동으로 넘긴다. */
export function navigate(href: string, { replace = false }: { replace?: boolean } = {}) {
  const url = new URL(href, window.location.href);
  if (url.origin !== window.location.origin || !isScreen(url.pathname)) { window.location.assign(url.href); return; }
  const next = url.pathname + url.search;
  if (next === window.location.pathname + window.location.search) return;
  if (replace) window.history.replaceState(null, '', next); else window.history.pushState(null, '', next);
  for (const listener of [...listeners]) listener();
}

type LinkProps = AnchorHTMLAttributes<HTMLAnchorElement> & { href: string };
/** 화면 사이 이동용 링크. 새 탭·수정키 클릭과 화면 밖 경로는 브라우저 기본 동작을 그대로 둔다. */
export function Link({ href, onClick, ...rest }: LinkProps) {
  const handle = (event: MouseEvent<HTMLAnchorElement>) => {
    onClick?.(event);
    if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    if (rest.target && rest.target !== '_self') return;
    const url = new URL(href, window.location.href);
    if (url.origin !== window.location.origin || !isScreen(url.pathname)) return;
    event.preventDefault();
    navigate(url.pathname + url.search);
  };
  return createElement('a', { ...rest, href, onClick: handle });
}
