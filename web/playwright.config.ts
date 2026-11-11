import { defineConfig } from '@playwright/test';
export default defineConfig({ testDir: './e2e', timeout: 20000, expect: { timeout: 5000 }, fullyParallel: false, workers: 1, retries: 0, reporter: './e2e/safe-reporter.ts', use: { baseURL: process.env.E2E_BASE_URL, headless: true, trace: 'off', screenshot: 'off', video: 'off' }, outputDir: 'test-results' });
