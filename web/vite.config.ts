import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';
export default defineConfig({ base: '/dashboard/', plugins: [react()], build: { manifest: true, sourcemap: false, assetsInlineLimit: 0 }, test: { environment: 'jsdom', include: ['src/**/*.test.ts', 'src/**/*.test.tsx'], setupFiles: ['src/test-setup.ts'], env: { TZ: 'UTC' }, restoreMocks: true } });
