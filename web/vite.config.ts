import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';
import { mockApi } from './mock/plugin';
// 개발용 가짜 API(mock/)는 개발 서버(`npm run dev`)에만 넣는다. 운영 빌드(command 'build')와 미리 보기 서버에는 넣지 않는다.
// vitest 도 command 가 'serve' 라서, mode 'test' 와 VITEST 환경 변수로 한 번 더 걸러 낸다.
const underVitest = () => Boolean((globalThis as { process?: { env?: Record<string, string | undefined> } }).process?.env?.VITEST);
export default defineConfig(({ command, mode, isPreview }) => ({ base: '/dashboard/', plugins: [react(), ...(command === 'serve' && !isPreview && mode !== 'test' && !underVitest() ? [mockApi()] : [])], build: { manifest: true, sourcemap: false, assetsInlineLimit: 0 }, test: { environment: 'jsdom', include: ['src/**/*.test.ts', 'src/**/*.test.tsx'], setupFiles: ['src/test-setup.ts'], env: { TZ: 'UTC' }, restoreMocks: true } }));
