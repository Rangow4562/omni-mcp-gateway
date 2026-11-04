export type ProtocolTypeStr =
  | 'HTTP'
  | 'gRPC'
  | 'TChannel'
  | 'Native_MCP'
  | 'ThirdParty'
  | 'External_MCP';

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
