/** /auth/* 경로의 답. code는 오류 본문 {"error":{"code":...}}의 코드다. */
export type AuthReply<T = unknown> = { status: number; ok: boolean; code: string | null; data: T | null; retryAfter: string | null };

/**
 * 로그인 전 화면이 /auth/* 에 JSON을 보낸다. 아직 세션이 없으므로 Session(CSRF·명령 ID)을 거치지 않는다.
 * 쿠키는 같은 origin에만 보내고 받는다. 10초 안에 답이 없으면 멈춘다.
 */
export async function postAuth<T = unknown>(url: string, body: unknown): Promise<AuthReply<T>> {
  const controller = new AbortController(), timer = setTimeout(() => controller.abort(), 10000);
  try {
    const response = await fetch(url, {
      method: 'POST', credentials: 'same-origin', cache: 'no-store', signal: controller.signal,
      headers: { Accept: 'application/json', 'Content-Type': 'application/json' }, body: JSON.stringify(body),
    });
    const data = response.status === 204 ? null : await response.json().catch(() => null);
    return { status: response.status, ok: response.ok, code: data?.error?.code ?? null, data: response.ok ? data as T : null, retryAfter: response.headers.get('Retry-After') };
  } finally {
    clearTimeout(timer);
  }
}

/** 비밀번호 길이 규칙(결정 0011 규칙 4). 서버처럼 문자(코드 포인트) 수로 센다. */
export const passwordLengthOk = (value: string) => { const n = [...value].length; return n >= 12 && n <= 128; };
