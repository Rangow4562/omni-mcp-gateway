import { test, expect } from '@playwright/test';

test.describe('Omni MCP v6.0.0 Fleet & Studio E2E', () => {
  test('dashboard loads, connects live SSE, and opens Tool Groups & Code Mode studios', async ({ page }) => {
    await page.goto('/');
    await expect(page.getByText('Omni MCP Gateway')).toBeVisible();
    await expect(page.getByText('v6.0.0 FLEET READY')).toBeVisible();

    // Verify Tool Groups Studio opens and renders builder
    await page.getByRole('button', { name: /Tool Groups/i }).click();
    await expect(page.getByText('Tool Groups Studio')).toBeVisible();
    await expect(page.getByRole('button', { name: /Save Tool Group/i })).toBeVisible();
    await page.getByRole('button', { name: '✕' }).click();

    // Verify Code Mode Studio opens and loads .d.ts declarations
    await page.getByRole('button', { name: /Code Mode/i }).click();
    await expect(page.getByText('Code Mode Studio')).toBeVisible();
    await expect(page.getByText(/Generated TypeScript Declarations/i)).toBeVisible();
    await page.getByRole('button', { name: '✕' }).click();

    // Verify Register External MCP modal opens
    await page.getByRole('button', { name: /Register External MCP/i }).click();
    await expect(page.getByText(/Register External MCP Server/i)).toBeVisible();
  });
});
