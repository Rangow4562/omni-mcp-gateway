import { test, expect } from '@playwright/test';

test.describe('Omni MCP v6.8.5 Fleet, Governance & Observability Studio E2E', () => {
  test('dashboard loads, connects live SSE, and opens all studios', async ({ page }) => {
    await page.goto('/');
    await expect(page.getByText('Omni MCP Gateway')).toBeVisible();
    await expect(page.getByText('v6.8.5 FLEET READY')).toBeVisible();

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

    // Verify Register External MCP modal opens and closes cleanly
    await page.getByRole('button', { name: /Register External MCP/i }).click();
    await expect(page.getByText(/Register External MCP Server/i)).toBeVisible();
    await page.getByRole('button', { name: /Close Register External Server Modal/i }).click();
    await expect(page.getByText(/Register External MCP Server/i)).toBeHidden();
    await expect(page.getByText('Omni MCP Gateway')).toBeVisible();
  });

  test('opens HITL Approval Inbox, resumes paused mutation script, verifies Merkle chain, and runs mcp.json import wizard', async ({ page, request }) => {
    // 1. Trigger a paused mutating Code Mode script as agent_dev_token
    const evalResp = await request.post('/api/codemode/eval', {
      headers: {
        Authorization: 'Bearer agent_dev_token',
        'Content-Type': 'application/json',
      },
      data: {
        code: 'async () => { const res = await codemode.accounting.refund({ ride_id: "r_e2e_1", reason: "e2e_dispute" }); return res; }',
        require_mutation_approval: true,
      },
    });
    expect(evalResp.ok()).toBeTruthy();
    const evalJson = await evalResp.json();
    expect(evalJson.status).toBe('APPROVAL_REQUIRED');

    // 2. Open dashboard as default Admin (distinct principal from agent_dev) and open Approval Inbox
    await page.goto('/');
    await page.getByRole('button', { name: /Approvals/i }).click();
    await expect(page.getByTestId('approval-inbox-modal')).toBeVisible();
    await expect(page.getByText(/Unified Approval & Elicitation Inbox/i)).toBeVisible();

    // Approve & Resume the suspended frame unconditionally
    const approveBtn = page.getByRole('button', { name: /Approve & Resume/i }).first();
    await expect(approveBtn).toBeVisible({ timeout: 5000 });
    await approveBtn.click();
    await expect(page.getByText(/Approved & resumed/i)).toBeVisible({ timeout: 5000 });
    await page.getByRole('button', { name: /Close Approval Inbox/i }).click();

    // 3. Open Observability Studio -> verify Trace Waterfall & Merkle Audit Chain badge
    await page.getByRole('button', { name: /Observability/i }).click();
    await expect(page.getByTestId('observability-studio-modal')).toBeVisible();
    await expect(page.getByText(/W3C Trace Waterfall/i)).toBeVisible();
    await page.getByRole('button', { name: /Merkle Audit Explorer/i }).click();
    await page.getByRole('button', { name: /Verify Cryptographic Chain/i }).click();
    await expect(page.getByTestId('merkle-verify-badge')).toContainText(/Merkle Chain Verified/i);
    await page.getByRole('button', { name: /Close Observability Studio/i }).click();

    // 4. Open Import mcp.json Wizard -> Preview secret ref conversion
    await page.getByRole('button', { name: /Import mcp\.json/i }).click();
    await expect(page.getByTestId('import-config-modal')).toBeVisible();
    await page.getByRole('button', { name: /Preview Secret Conversion/i }).click();
    await expect(page.getByTestId('import-preview-panel')).toBeVisible();
    await expect(page.getByTestId('import-preview-panel')).toContainText(/\{env:/i);
    await page.getByRole('button', { name: /Close Import Config Modal/i }).click();
  });

  test('preserves Approval Inbox resume error banner across live SSE refreshes', async ({ page, request }) => {
    // Trigger a paused mutating Code Mode script as admin_dev_token so default Admin hits SoD rejection
    const evalResp = await request.post('/api/codemode/eval', {
      headers: {
        Authorization: 'Bearer admin_dev_token',
        'Content-Type': 'application/json',
      },
      data: {
        code: 'async () => { const res = await codemode.accounting.refund({ ride_id: "r_e2e_sod", reason: "sod_check" }); return res; }',
        require_mutation_approval: true,
      },
    });
    expect(evalResp.ok()).toBeTruthy();
    const evalJson = await evalResp.json();
    expect(evalJson.status).toBe('APPROVAL_REQUIRED');
    const execId: string = evalJson.execution_id;

    await page.goto('/');
    await expect(page.getByTestId('sse-status')).toContainText(/LIVE SSE/i);
    await page.getByRole('button', { name: /Approvals/i }).click();
    await expect(page.getByTestId('approval-inbox-modal')).toBeVisible();

    const frameCard = page.getByTestId(`suspended-frame-${execId}`);
    await expect(frameCard).toBeVisible({ timeout: 5000 });
    await frameCard.getByRole('button', { name: /Approve & Resume/i }).click();

    // Error banner must appear and remain visible even after SSE codemode.resumed triggers background refresh
    await expect(page.getByText(/Resume rejected:/i)).toBeVisible({ timeout: 5000 });
    await page.waitForTimeout(500);
    await expect(page.getByText(/Resume rejected:/i)).toBeVisible();

    // Clean up suspended frame so state stays tidy
    await frameCard.getByRole('button', { name: /Reject & Discard/i }).click();
    await page.getByRole('button', { name: /Close Approval Inbox/i }).click();
  });

  test('Skills Studio publishes and verifies deterministic SHA-256 skill manifest', async ({ page }) => {
    await page.goto('/');
    await page.getByTestId('open-skills-studio-btn').click();
    await expect(page.getByTestId('skills-studio-modal')).toBeVisible({ timeout: 5000 });

    const uniqueSlug = `e2e-skill-${Date.now()}`;
    await page.getByTestId('skill-slug-input').fill(uniqueSlug);
    await page.getByTestId('publish-skill-btn').click();

    await expect(page.getByText(new RegExp(`Published '${uniqueSlug}'`, 'i'))).toBeVisible({
      timeout: 5000,
    });
    await expect(page.getByTestId('skill-manifest-sha')).not.toBeEmpty();

    // Exercise the new v6.8.5 Install & Enterprise Plugin tab
    await page.getByTestId('tab-plugin-center').click();
    await expect(page.getByTestId('plugin-center-panel')).toBeVisible({ timeout: 5000 });
    await expect(page.getByTestId('plugin-config-snippet')).toContainText('@rangow2260/omni-mcp', {
      timeout: 5000,
    });
    await expect(page.getByTestId('plugin-install-cmd')).toContainText('omni skills install');

    // Switch to Enterprise Governed Remote + Codex (TOML)
    await page.getByTestId('plugin-mode-enterprise').click();
    await page.getByTestId('plugin-agent-select').selectOption('codex');
    await expect(page.getByTestId('plugin-config-snippet')).toContainText('[mcp_servers.omni-mcp-enterprise]', {
      timeout: 5000,
    });
    await expect(page.getByTestId('plugin-config-snippet')).toContainText('bearer_token_env_var = "OMNI_AUTH_TOKEN"');
    await expect(page.getByTestId('plugin-init-cmd')).toContainText('omni plugin init --mode enterprise');
  });

  test('Tasks Monitor opens, displays live MCP task stream, cancels task via UI + SSE, and verifies Unified Approval/Elicitation Inbox', async ({ page, request }) => {
    // 1. Start a long-running async Code Mode task via /omni/mcp
    const taskResp = await request.post('/omni/mcp', {
      headers: {
        Authorization: 'Bearer admin_dev_token',
        'Content-Type': 'application/json',
      },
      data: {
        jsonrpc: '2.0',
        id: 701,
        method: 'tools/call',
        params: {
          name: 'codemode',
          task: { ttl: 60000 },
          clientCapabilities: {
            extensions: { 'io.modelcontextprotocol/tasks': {} },
          },
          arguments: {
            code: 'async () => { let sum = 0; let i = 0; while (i < 40000) { sum += i; i += 1; } return sum; }',
            max_operations: 500000,
          },
        },
      },
    });
    expect(taskResp.ok()).toBeTruthy();
    const taskJson = await taskResp.json();
    const taskId: string = taskJson?.result?.taskId;
    expect(taskJson?.result?.resultType).toBe('task');
    expect(taskId).toBeTruthy();

    // 2. Open dashboard and Tasks Monitor, verify task appears live and cancel it via UI
    await page.goto('/');
    await expect(page.getByTestId('sse-status')).toContainText(/LIVE SSE/i);
    await page.getByTestId('open-tasks-monitor-btn').click();
    await expect(page.getByTestId('tasks-monitor-modal')).toBeVisible({ timeout: 5000 });
    await expect(page.getByText(/MCP Tasks Monitor/i)).toBeVisible();

    const taskRow = page.getByTestId(`task-row-${taskId}`);
    await expect(taskRow).toBeVisible({ timeout: 5000 });

    const cancelBtn = page.getByTestId(`cancel-task-${taskId}`);
    if (await cancelBtn.isVisible()) {
      await cancelBtn.click();
      await expect(taskRow).toContainText(/CANCELLED/i, { timeout: 5000 });
    }
    await page.getByRole('button', { name: /✕ Close/i }).click();

    // 3. Verify Unified Approval & Elicitation Inbox (form / url) renders cleanly
    await page.getByRole('button', { name: /Approvals/i }).click();
    await expect(page.getByTestId('approval-inbox-modal')).toBeVisible();
    await expect(page.getByText(/Unified Approval & Elicitation Inbox \(form \/ url\)/i)).toBeVisible();
    await page.getByRole('button', { name: /Close Approval Inbox/i }).click();
  });
});
