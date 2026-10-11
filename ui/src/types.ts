export interface ServerSummary {
  name: string;
  displayName: string;
  description: string;
  owningTeam: string;
  protocol: 'HTTP' | 'gRPC' | 'TChannel' | 'Native_MCP' | 'ThirdParty';
  status: 'DISABLED' | 'ENABLED' | 'PENDING_REVIEW';
  version: number;
  toolsCount: number;
  enabledToolsCount: number;
}

export interface ToolItem {
  name: string;
  description: string;
  inputSchema: Record<string, any>;
  status: 'DISABLED' | 'ENABLED' | 'PENDING_REVIEW';
  version: number;
  protocol: string;
  downstreamEndpoint?: string;
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
}

export interface PlatformStats {
  total_servers: number;
  total_tools: number;
  enabled_tools: number;
  disabled_tools: number;
  version: string;
}

