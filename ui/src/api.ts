// ui/src/api.ts
// Centralized API client with relative URLs, Bearer token management, and structured error handling.

export const TOKEN_STORAGE_KEY = 'omni_mcp_token';
export const DEFAULT_DEV_TOKEN = 'admin_dev_token';

export const DEV_ROLE_TOKENS = {
  admin: 'admin_dev_token',
  approver: 'approver_dev_token',
  operator: 'operator_dev_token',
  agent: 'agent_dev_token',
} as const;

export function getToken(): string {
  const token = localStorage.getItem(TOKEN_STORAGE_KEY);
  if (!token) {
    localStorage.setItem(TOKEN_STORAGE_KEY, DEFAULT_DEV_TOKEN);
    return DEFAULT_DEV_TOKEN;
  }
  return token;
}

export function setToken(token: string): void {
  const trimmed = token.trim();
  localStorage.setItem(TOKEN_STORAGE_KEY, trimmed);
  if (typeof window !== 'undefined') {
    window.dispatchEvent(new CustomEvent('omni-token-changed', { detail: trimmed }));
  }
}

export interface ApiError {
  status: number;
  message: string;
  data?: any;
}

export async function apiFetch<T = any>(
  path: string,
  options: RequestInit = {}
): Promise<T> {
  const token = getToken();
  const headers = new Headers(options.headers || {});

  if (!headers.has('Authorization') && token) {
    headers.set('Authorization', `Bearer ${token}`);
  }
  if (!headers.has('Content-Type') && options.body && typeof options.body === 'string') {
    headers.set('Content-Type', 'application/json');
  }

  // Normalize path to relative URL so same-origin works anywhere
  let url = path;
  if (url.startsWith('http://localhost:8080')) {
    url = url.replace('http://localhost:8080', '');
  }

  const response = await fetch(url, {
    ...options,
    headers,
  });

  if (!response.ok) {
    let errorData: any = null;
    try {
      errorData = await response.json();
    } catch {
      try {
        errorData = await response.text();
      } catch {
        errorData = null;
      }
    }

    const message =
      (errorData && errorData.error) ||
      (errorData && errorData.message) ||
      `Request failed with status ${response.status}`;

    const error: ApiError = {
      status: response.status,
      message: typeof message === 'string' ? message : JSON.stringify(message),
      data: errorData,
    };
    throw error;
  }

  const contentType = response.headers.get('content-type') || '';
  if (contentType.includes('application/json')) {
    return (await response.json()) as T;
  }
  return (await response.text()) as unknown as T;
}

export interface ToolGroupEntry {
  server_name: string;
  tool_name?: string | null;
}

export interface ToolGroupItem {
  slug: string;
  display_name: string;
  description: string;
  entries: ToolGroupEntry[];
}

export async function listToolGroups(): Promise<ToolGroupItem[]> {
  const res = await apiFetch<{ groups: ToolGroupItem[] }>('/api/v1/groups');
  return res.groups || [];
}

export async function saveToolGroup(group: ToolGroupItem): Promise<ToolGroupItem> {
  const res = await apiFetch<{ group: ToolGroupItem }>('/api/v1/groups', {
    method: 'POST',
    body: JSON.stringify(group),
  });
  return res.group;
}

export async function deleteToolGroup(slug: string): Promise<void> {
  await apiFetch(`/api/v1/groups/${encodeURIComponent(slug)}`, { method: 'DELETE' });
}

export async function listSuspendedFrames(): Promise<any[]> {
  const res = await apiFetch<{ suspended_frames: any[] }>(
    '/api/codemode/suspended?include_preview=true'
  );
  return res.suspended_frames || [];
}

export async function resumeSuspendedFrame(
  executionId: string,
  pendingArgumentsHash: string
): Promise<any> {
  return apiFetch('/api/codemode/resume', {
    method: 'POST',
    body: JSON.stringify({
      execution_id: executionId,
      arguments_digest: pendingArgumentsHash,
    }),
  });
}

export async function cancelSuspendedFrame(executionId: string): Promise<void> {
  await apiFetch(`/api/codemode/suspended/${encodeURIComponent(executionId)}`, {
    method: 'DELETE',
  });
}

export async function listPendingCallbacks(): Promise<any[]> {
  const res = await apiFetch<{ callbacks: any[] }>('/api/v1/callbacks');
  return res.callbacks || [];
}

export async function respondPendingCallback(
  callbackId: string,
  approved: boolean,
  result?: any,
  error?: string
): Promise<any> {
  return apiFetch(`/api/v1/callbacks/${encodeURIComponent(callbackId)}/respond`, {
    method: 'POST',
    body: JSON.stringify({ approved, result, error }),
  });
}

export async function fetchTraces(): Promise<any[]> {
  const res = await apiFetch<{ spans?: any[]; traces?: any[] }>('/api/traces');
  return res.spans || res.traces || [];
}

export async function fetchAuditLog(): Promise<{ records: any[]; head_hash?: string }> {
  const res = await apiFetch<{ records?: any[]; audit_log?: any[]; head_hash?: string }>('/api/audit');
  return {
    records: res.records || res.audit_log || [],
    head_hash: res.head_hash,
  };
}

export async function verifyAuditChain(): Promise<any> {
  return apiFetch('/api/audit/verify');
}

export async function fetchOtlpStatus(): Promise<any> {
  return apiFetch('/api/v1/telemetry/otlp/status');
}

export async function importMcpConfig(configJson: string, dryRun: boolean): Promise<any> {
  return apiFetch('/api/v1/import-config', {
    method: 'POST',
    body: JSON.stringify({ config_json: configJson, dry_run: dryRun }),
  });
}

export async function initOAuthAuthorize(serverName: string): Promise<any> {
  return apiFetch(`/api/v1/oauth/authorize/${encodeURIComponent(serverName)}`);
}
