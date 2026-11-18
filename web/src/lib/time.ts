import { useSyncExternalStore } from 'react';

/**
 * 화면에 보이는 시각의 기준. 기본은 브라우저 시간대이고, 사용자가 UTC를 고른 경우에만
 * 이 브라우저에 그 선택 하나를 남긴다(언어 선택과 같은 방식). 입력칸(현지 시각)에는 영향이 없다.
 */
export type TimeBasis = 'browser' | 'utc';
const storageKey = 'm-gated.time';
const listeners = new Set<() => void>();

function storedBasis(): TimeBasis {
  try { return window.localStorage.getItem(storageKey) === 'utc' ? 'utc' : 'browser'; } catch { return 'browser'; }
}
let basis: TimeBasis = storedBasis();

export const timeBasis = {
  subscribe(listener: () => void) { listeners.add(listener); return () => { listeners.delete(listener); }; },
  snapshot: () => basis,
  set(next: TimeBasis) {
    try {
      if (next === 'utc') window.localStorage.setItem(storageKey, next);
      else window.localStorage.removeItem(storageKey);
    } catch { /* 저장할 수 없는 환경이면 지금 화면에만 적용한다. */ }
    if (next === basis) return;
    basis = next;
    for (const listener of listeners) listener();
  },
};

/** 브라우저의 IANA 시간대. 알 수 없으면 UTC. */
export function browserTimeZone(): string {
  try { return Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC'; } catch { return 'UTC'; }
}

/** 지금 기준에서 시각을 그릴 시간대. */
export function displayTimeZone(current: TimeBasis = basis): string {
  return current === 'utc' ? 'UTC' : browserTimeZone();
}

const instant = /^(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2})(?::(\d{2})(?:\.(\d+))?)?(Z|[+-]\d{2}:?\d{2})$/i;
/** 서버 RFC3339 문자열(소수점 6자리 포함)을 밀리초로. 형식이 다르면 null. */
export function parseInstant(value: string): number | null {
  if (typeof value !== 'string') return null;
  const m = instant.exec(value.trim());
  if (!m) return null;
  const [, y, mo, d, h, mi, s = '0', fraction = '', zone] = m;
  let ms = Date.UTC(Number(y), Number(mo) - 1, Number(d), Number(h), Number(mi), Number(s), Number(fraction.slice(0, 3).padEnd(3, '0')));
  if (zone.toUpperCase() !== 'Z') {
    const sign = zone[0] === '-' ? -1 : 1, digits = zone.slice(1).replace(':', '');
    ms -= sign * (Number(digits.slice(0, 2)) * 60 + Number(digits.slice(2))) * 60000;
  }
  return Number.isFinite(ms) ? ms : null;
}

const formatters = new Map<string, Intl.DateTimeFormat>();
/**
 * 시각을 YYYY-MM-DD HH:mm(분까지, 초 이하는 버림)으로 그린다. 날짜만 보이거나 '마지막 날'로 바꾸는 예외는 두지 않는다.
 * 서버 시각 형식이 아니면 받은 문자열을 그대로 돌려준다.
 */
export function formatTime(value: string, timeZone: string = displayTimeZone()): string {
  const ms = parseInstant(value);
  if (ms === null) return typeof value === 'string' ? value : value == null ? '' : String(value);
  let format = formatters.get(timeZone);
  if (!format) {
    try {
      format = new Intl.DateTimeFormat('en-US', { timeZone, year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hourCycle: 'h23' });
    } catch { return formatTime(value, 'UTC'); }
    formatters.set(timeZone, format);
  }
  const parts: Record<string, string> = {};
  for (const part of format.formatToParts(new Date(ms))) parts[part.type] = part.value;
  const hour = parts.hour === '24' ? '00' : parts.hour;
  return `${parts.year.padStart(4, '0')}-${parts.month}-${parts.day} ${hour}:${parts.minute}`;
}

/** 지금 기준을 따라 다시 그려지는 시각 포매터. 기준을 바꾸면 화면의 모든 시각이 함께 바뀐다. */
export function useTimeFormat() {
  const current = useSyncExternalStore(timeBasis.subscribe, timeBasis.snapshot);
  const zone = displayTimeZone(current);
  return (value: string) => formatTime(value, zone);
}
export function useTimeBasis() {
  return useSyncExternalStore(timeBasis.subscribe, timeBasis.snapshot);
}
