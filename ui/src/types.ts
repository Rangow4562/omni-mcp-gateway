export type ProtocolTypeStr =
  | 'HTTP'
  | 'gRPC'
  | 'TChannel'
  | 'Native_MCP'
  | 'ThirdParty'
  | 'External_MCP'
  | 'WasmPlugin';

export type ToolStatusStr = 'DISABLED' | 'ENABLED' | 'PENDING_REVIEW' | 'UNAVAILABLE';

export interface ServerSummary {
  name: string;
  displayName: string;
  description: string;
  owningTeam: string;
  protocol: ProtocolTypeStr;
  status: ToolStatusStr;
  version: number;
  toolsCount: number;
  enabledToolsCount: number;
  upstream_config?: UpstreamTransportConfig;
}

export interface ToolItem {
  name: string;
  description: string;
  inputSchema: Record<string, any>;
  status: ToolStatusStr;
  version: number;
  protocol: string;
  downstreamEndpoint?: string;
  schema_source?: 'upstream' | 'human';
  retired_at?: number | null;
  last_seen_in_sync?: number | null;
}

export interface ServerDetail extends ServerSummary {
  tools: Record<string, ToolItem>;
  history: Array<{ action: string; version: number; time: number }>;
}

export interface ConfigDiffItem {
  diffId: string;
  serverName: string;
  toolName: string;
  prevVersion: number;
  proposedVersion: number;
  changes: string[];
  proposedBy: string;
  createdAt: number;
  beforeState: Record<string, any>;
  afterState: Record<string, any>;
  origin?: 'human' | 'external_sync' | 'auto_crawler';
  approved?: boolean;
}

export interface PlatformStats {
  total_servers: number;
  total_tools: number;
  enabled_tools: number;
  disabled_tools: number;
  version: string;
}

export interface StdioTransportConfig {
  transport: 'stdio';
  command: string;
  canonical_command?: string;
  args: string[];
  env_refs: Record<string, string>;
  cwd?: string | null;
}

export interface HttpTransportConfig {
  transport: 'streamable_http';
  url: string;
  bearer_ref?: string | null;
  header_refs: Record<string, string>;
  session_mode: 'stateless' | 'stateful';
}

export type UpstreamTransportConfig = StdioTransportConfig | HttpTransportConfig;

export interface ExternalSyncResult {
  added: number;
  updated: number;
  retired: number;
  unchanged: number;
  pending_human_conflicts: number;
}

export interface ServerInspection {
  server_name: string;
  health: 'Connected' | 'Idle' | 'Unreachable';
  last_initialize: number | null;
  session_mode: string | null;
  total_tools: number;
  enabled_tools: number;
  disabled_tools: number;
  unavailable_tools: number;
  transport_summary: Record<string, any>;
  stderr_tail: string[];
}

export interface SuspendedFrameItem {
  execution_id: string;
  script_hash: string;
  instruction_ptr: number;
  target_var?: string | null;
  pending_server: string;
  pending_tool: string;
  pending_reason: string;
  pending_arguments_hash: string;
  caller_actor: string;
  logs: string[];
  created_at: number;
  expires_at: number;
  is_consumed: boolean;
  redacted?: boolean;
  script_preview?: string;
  scope_preview?: Record<string, any>;
  pending_arguments_preview?: Record<string, any>;
}

export interface PendingCallbackItem {
  callback_id: string;
  server_name: string;
  upstream_request_id: any;
  method: string;
  params: Record<string, any>;
  mode: string;
  status: string;
  created_at: number;
}

export interface TraceSpanItem {
  traceId: string;
  spanId: string;
  parentSpanId?: string;
  name: string;
  serviceName: string;
  toolName: string;
  startTimeUnixNano: number;
  endTimeUnixNano: number;
  durationUs: number;
  status: { code: number; message: string };
  attributes?: Array<{ key: string; value: any }>;
}

export interface AuditRecordItem {
  index: number;
  timestamp: number;
  prev_hash: string;
  current_hash: string;
  actor: string;
  server_name: string;
  tool_name: string;
  args_hash: string;
  status: string;
  execution_us: number;
}

export interface AuditVerifyResult {
  verified: boolean;
  records_checked?: number;
  corrupted_at_index?: number;
  algorithm?: string;
  head_hash?: string;
  storage_backend?: string;
  external_sink_verified?: boolean | null;
  external_sink_records?: number | null;
  reason?: string;
}

export interface OtlpStatusResponse {
  endpoint: string | null;
  running: boolean;
  flush_interval_ms: number;
  queued_spans: number;
  total_exported: number;
  total_failed: number;
  last_flush_at?: number | null;
  last_error?: string | null;
  latency_quantiles_us?: { p50: number; p95: number; p99: number };
  storage_backend?: string;
}

export interface ImportConfigPreviewItem {
  server_name: string;
  slug: string;
  transport: Record<string, any>;
  converted_secrets: Array<{ key: string; converted_to: string }>;
}

export interface ImportConfigResponse {
  dry_run: boolean;
  imported_count: number;
  servers: ImportConfigPreviewItem[];
}
