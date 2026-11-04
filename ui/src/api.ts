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
  localStorage.setItem(TOKEN_STORAGE_KEY, token.trim());
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
