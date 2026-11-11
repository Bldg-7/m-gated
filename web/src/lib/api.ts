import type { Me } from './types';

export class ApiError extends Error {
  constructor(public status: number, public code: string, public details: Record<string, unknown> = {}) {
    super(code);
  }
}
export class StaleResponse extends Error {}
type State = {
  epoch: number;
  me: Me | null;
  status: 'checking' | 'ready' | 'error';
  error: string | null;
  refreshing: boolean;
};
type Fetch = typeof fetch;

export class Session {
  private state: State = { epoch: 0, me: null, status: 'checking', error: null, refreshing: false };
  private listeners = new Set<() => void>();
  private controllers = new Set<AbortController>();
  private refresh: Promise<number> | null = null;
  private refreshEpoch = -1;
  private refreshActiveEpoch = -1;
  private revalidation: Promise<void> | null = null;
  private revalidationEpoch = -1;
  private channel: BroadcastChannel | null = null;
  private logoutLatched = false;
  private verifiedPrincipal: string | null = null;

  constructor(
    private request: Fetch = (...args) => fetch(...args),
    private navigate: (url: string) => void = url => location.assign(url),
  ) {}

  snapshot = () => this.state;
  subscribe = (listener: () => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };
  private emit() {
    for (const listener of this.listeners) listener();
  }
  clear(broadcast = false, refreshing = false) {
    this.state = { epoch: this.state.epoch + 1, me: null, status: 'checking', error: null, refreshing };
    for (const controller of this.controllers) controller.abort();
    this.controllers.clear();
    this.emit();
    if (broadcast) this.channel?.postMessage({ type: 'auth-change' });
  }

  private async json<T>(url: string, epoch: number): Promise<T> {
    const controller = new AbortController();
    this.controllers.add(controller);
    const timer = setTimeout(() => controller.abort(), 10000);
    try {
      const response = await this.request(url, {
        credentials: 'same-origin', cache: 'no-store', signal: controller.signal,
        headers: { Accept: 'application/json' },
      });
      if (epoch !== this.state.epoch) throw new StaleResponse();
      const body = await response.json();
      if (epoch !== this.state.epoch) throw new StaleResponse();
      if (!response.ok) throw new ApiError(response.status, body?.error?.code ?? 'request_failed', body?.error?.details ?? {});
      return body as T;
    } finally {
      clearTimeout(timer);
      this.controllers.delete(controller);
    }
  }
  private accept(me: Me, epoch: number, notifyIdentityChange = true) {
    // json의 await가 끝난 뒤에도 수용 직전에 이전 인증 작업을 거부한다.
    if (epoch !== this.state.epoch) throw new StaleResponse();
    const changed = this.verifiedPrincipal !== null && this.verifiedPrincipal !== me.principal.id;
    this.verifiedPrincipal = me.principal.id;
    this.state = { ...this.state, me, status: 'ready', error: null, refreshing: false };
    this.emit();
    if (changed && notifyIdentityChange) this.channel?.postMessage({ type: 'auth-change' });
  }
  async validate(intent: 'explicit' | 'automatic' | 'remote' = 'explicit'): Promise<void> {
    if (intent === 'explicit') this.logoutLatched = false;
    else if (this.logoutLatched) return;
    if (this.revalidation && this.revalidationEpoch === this.state.epoch) return this.revalidation;
    this.clear();
    const epoch = this.state.epoch;
    this.revalidationEpoch = epoch;
    const job = (async () => {
      try {
        this.accept(await this.json<Me>('/gateway/me', epoch), epoch, intent !== 'remote');
      } catch (error) {
        if (epoch !== this.state.epoch || error instanceof StaleResponse) return;
        this.state = {
          ...this.state, status: 'error',
          error: error instanceof ApiError && error.status === 403 ? '접근 권한이 없습니다.' : '인증 상태를 확인할 수 없습니다.',
        };
        this.emit();
        if (error instanceof ApiError && error.status === 401) this.navigate('/auth/login');
      }
    })();
    this.revalidation = job;
    try { await job; } finally { if (this.revalidation === job) this.revalidation = null; }
  }
  private ownsRefresh(epoch: number) {
    return this.refresh !== null && this.refreshEpoch === epoch
      && this.refreshActiveEpoch === this.state.epoch && !this.logoutLatched;
  }
  private async refreshOnce(previous: Me, sourceEpoch: number): Promise<number> {
    if (this.ownsRefresh(sourceEpoch)) return this.refresh!;
    if (sourceEpoch !== this.state.epoch || this.logoutLatched) throw new StaleResponse();
    this.refreshEpoch = sourceEpoch;
    this.clear(false, true);
    const epoch = this.state.epoch;
    this.refreshActiveEpoch = epoch;
    const job = (async () => {
      let refreshed = false;
      try {
        const url = new URL(previous.sso.refresh_url);
        if (url.protocol !== 'https:') throw new Error('고정 SSO HTTPS만 허용');
        const controller = new AbortController();
        this.controllers.add(controller);
        const timer = setTimeout(() => controller.abort(), 10000);
        let response: Response;
        try {
          response = await this.request(url.toString(), {
            method: 'POST', credentials: 'include', cache: 'no-store', signal: controller.signal,
          });
        } finally {
          clearTimeout(timer);
          this.controllers.delete(controller);
        }
        if (epoch !== this.state.epoch) throw new StaleResponse();
        if (!response.ok) throw new Error('refresh_failed');
        refreshed = true;
        const me = await this.json<Me>('/gateway/me', epoch);
        if (epoch !== this.state.epoch || this.logoutLatched) throw new StaleResponse();
        // refresh된 JWT와 결합된 CSRF를 확인한 뒤에만 읽기 재시도를 허용한다.
        // 원문 token은 보관하지 않으며 쓰기는 실행 시 다시 발급받는다.
        await this.json<{ token: string; expires_at: string }>('/gateway/csrf', epoch);
        this.accept(me, epoch);
        return epoch;
      } catch (error) {
        if (epoch === this.state.epoch) {
          this.clear();
          if (refreshed && !(error instanceof ApiError && error.status === 401)) {
            this.state = {
              ...this.state, status: 'error',
              error: error instanceof ApiError && error.status === 403 ? '접근 권한이 없습니다.' : '인증 상태를 확인할 수 없습니다.',
            };
            this.emit();
          } else this.navigate('/auth/login');
        }
        throw error;
      }
    })();
    this.refresh = job;
    try { return await job; } finally { if (this.refresh === job) this.refresh = null; }
  }
  private invalidateRejectedRead(error: unknown, epoch: number) {
    if (epoch !== this.state.epoch || this.logoutLatched) throw new StaleResponse();
    if (!(error instanceof ApiError)) return;
    if (error.status === 401) {
      this.clear(true);
      this.navigate('/auth/login');
    } else if (error.status === 403) {
      this.clear();
      void this.validate('automatic');
    }
  }
  async read<T>(url: string, isCurrent: () => boolean = () => true): Promise<T> {
    if (!url.startsWith('/gateway/') || url.startsWith('//')) throw new Error('same-origin 관리 읽기만 허용');
    const previous = this.state.me;
    const epoch = this.state.epoch;
    if (!previous) throw new StaleResponse();
    try {
      const data = await this.json<T>(url, epoch);
      if (epoch !== this.state.epoch || this.logoutLatched
        || this.state.me?.principal.id !== previous.principal.id) throw new StaleResponse();
      return data;
    } catch (error) {
      const joining = this.ownsRefresh(epoch);
      // refresh에 의한 epoch 전환만 합류하며 logout/새 검증이 대체한 오류는 버린다.
      if (!joining && (epoch !== this.state.epoch || this.logoutLatched)) throw new StaleResponse();
      if (joining || (error instanceof ApiError && error.status === 401)) {
        if (!isCurrent()) throw new StaleResponse();
        const retryEpoch = await this.refreshOnce(previous, epoch);
        if (!isCurrent() || retryEpoch !== this.state.epoch || this.logoutLatched
          || this.state.me?.principal.id !== previous.principal.id) throw new StaleResponse();
        try {
          const data = await this.json<T>(url, retryEpoch);
          if (retryEpoch !== this.state.epoch || this.logoutLatched
            || this.state.me?.principal.id !== previous.principal.id) throw new StaleResponse();
          return data;
        } catch (retryError) {
          this.invalidateRejectedRead(retryError, retryEpoch);
          throw retryError;
        }
      }
      if (error instanceof StaleResponse) throw error;
      this.invalidateRejectedRead(error, epoch);
      throw error;
    }
  }
  async write<T>(url: string, method: 'POST' | 'PUT' | 'PATCH' | 'DELETE', commandId: string, body?: unknown): Promise<T | undefined> {
    if (!url.startsWith('/gateway/') || url.startsWith('//')) throw new Error('same-origin 관리 쓰기만 허용');
    const epoch = this.state.epoch, principal = this.state.me?.principal.id;
    const current = () => {
      if (!principal || epoch !== this.state.epoch || this.logoutLatched || this.state.me?.principal.id !== principal) throw new StaleResponse();
    };
    current();
    const controller = new AbortController();
    this.controllers.add(controller);
    const timer = setTimeout(() => controller.abort(), 10000);
    try {
      // CSRF는 이 쓰기 작업에만 보관한다. 조회의 refresh/retry 경로를 쓰기에 사용하지 않는다.
      const csrf = await this.json<{ token: string; expires_at: string }>('/gateway/csrf', epoch);
      current();
      if (controller.signal.aborted) throw new DOMException('쓰기 기한 초과', 'AbortError');
      const headers: Record<string, string> = { Accept: 'application/json', 'X-CSRF-Token': csrf.token, 'X-Gateway-Command-ID': commandId };
      if (body !== undefined) headers['Content-Type'] = 'application/json';
      const response = await this.request(url, { method, credentials: 'same-origin', cache: 'no-store', signal: controller.signal, headers, ...(body === undefined ? {} : { body: JSON.stringify(body) }) });
      current();
      const data = response.status === 204 ? undefined : await response.json();
      current();
      if (!response.ok) throw new ApiError(response.status, data?.error?.code ?? 'request_failed', data?.error?.details ?? {});
      return data as T | undefined;
    } catch (error) {
      current();
      if (error instanceof ApiError && error.status === 401) {
        this.clear(true);
        this.state = { ...this.state, status: 'error', error: '인증이 만료되었습니다. 다시 확인한 뒤 명령 결과를 조회하세요. 쓰기는 재전송하지 않았습니다.' };
        this.emit();
      } else if (error instanceof ApiError && error.status === 403 && error.code === 'access_denied') {
        this.clear();
        void this.validate('automatic');
      }
      throw error;
    } finally {
      clearTimeout(timer);
      this.controllers.delete(controller);
    }
  }
  private latchLogout() {
    this.logoutLatched = true;
    this.clear();
    this.state = { ...this.state, status: 'error', error: '로그아웃되었습니다.' };
    this.emit();
  }
  logout() {
    const url = this.state.me?.sso.logout_url;
    this.latchLogout();
    this.channel?.postMessage({ type: 'logout' });
    if (url && new URL(url).protocol === 'https:') this.navigate(url);
    else this.navigate('/auth/login');
  }
  attach() {
    if (typeof BroadcastChannel !== 'undefined') {
      this.channel = new BroadcastChannel('m-gated-auth');
      this.channel.onmessage = event => {
        if (event.data?.type === 'logout') {
          // SSO의 cookie 삭제 전에는 명시적인 다시 확인만 재인증을 허용한다.
          this.latchLogout();
        } else if (event.data?.type === 'auth-change') {
          if (!this.logoutLatched) {
            this.clear();
            void this.validate('remote');
          }
        }
      };
    }
    const pageshow = (event: PageTransitionEvent) => { if (event.persisted) void this.validate('automatic'); };
    const visibility = () => {
      if (document.visibilityState === 'visible') void this.validate('automatic');
      else if (!this.logoutLatched) this.clear();
    };
    window.addEventListener('pageshow', pageshow);
    document.addEventListener('visibilitychange', visibility);
    return () => {
      this.channel?.close();
      this.channel = null;
      window.removeEventListener('pageshow', pageshow);
      document.removeEventListener('visibilitychange', visibility);
      this.clear();
    };
  }
}
export const session = new Session();
