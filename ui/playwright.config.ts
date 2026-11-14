import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: './tests/e2e',
  timeout: 30_000,
  retries: 1,
  use: {
    baseURL: process.env.OMNI_UI_URL || 'http://127.0.0.1:8080',
    headless: true,
    trace: 'on-first-retry',
  },
});
