import React, { useState, useEffect, useCallback } from 'react';
import { Header } from './components/Header';
import { RegistryCatalog } from './components/RegistryCatalog';
import { ToolDetailView } from './components/ToolDetailView';
import { DiffApprovalModal } from './components/DiffApprovalModal';
import { TestConsole } from './components/TestConsole';
import { OmniSearchModal } from './components/OmniSearchModal';
import { RegisterExternalServerModal } from './components/RegisterExternalServerModal';
import { ExternalServerInspector } from './components/ExternalServerInspector';
import { ToolGroupsStudio } from './components/ToolGroupsStudio';
import { CodeModeStudio } from './components/CodeModeStudio';
import { ApprovalInboxModal } from './components/ApprovalInboxModal';
import { ObservabilityStudio } from './components/ObservabilityStudio';
import { ImportConfigModal } from './components/ImportConfigModal';
import {
  ServerSummary,
  ServerDetail,
  ConfigDiffItem,
  PlatformStats,
  ToolItem,
} from './types';
import { apiFetch, ApiError, getToken, listSuspendedFrames, listPendingCallbacks } from './api';

export const App: React.FC = () => {
  const [servers, setServers] = useState<ServerSummary[]>([]);
  const [selectedServerName, setSelectedServerName] = useState<string | null>(null);
  const [serverDetail, setServerDetail] = useState<ServerDetail | null>(null);
  const [stats, setStats] = useState<PlatformStats | null>(null);
  const [diffs, setDiffs] = useState<ConfigDiffItem[]>([]);
  const [selectedActor, setSelectedActor] = useState<string>('agent');
  const [errorBanner, setErrorBanner] = useState<string | null>(null);

  // Modals
  const [isDiffModalOpen, setIsDiffModalOpen] = useState(false);
  const [isOmniModalOpen, setIsOmniModalOpen] = useState(false);
  const [isRegisterExternalOpen, setIsRegisterExternalOpen] = useState(false);
  const [isToolGroupsOpen, setIsToolGroupsOpen] = useState(false);
  const [isCodeModeOpen, setIsCodeModeOpen] = useState(false);
  const [isApprovalInboxOpen, setIsApprovalInboxOpen] = useState(false);
  const [isObservabilityOpen, setIsObservabilityOpen] = useState(false);
  const [isImportConfigOpen, setIsImportConfigOpen] = useState(false);
  const [pendingApprovalCount, setPendingApprovalCount] = useState(0);
  const [sseRevision, setSseRevision] = useState(0);
  const [sseConnected, setSseConnected] = useState(false);
  const [inspectServerTarget, setInspectServerTarget] = useState<ServerSummary | null>(null);
  const [testConsoleTarget, setTestConsoleTarget] = useState<{
    serverName: string;
    toolName: string;
    tool: ToolItem;
  } | null>(null);

  const handleError = (err: any) => {
    console.error(err);
    if (err && typeof err === 'object' && 'status' in err) {
      const apiErr = err as ApiError;
      if (apiErr.status === 401) {
        setErrorBanner('401 Unauthorized: Valid Bearer token required. Switch token in the header.');
      } else if (apiErr.status === 403) {
        setErrorBanner(`403 Forbidden: Your role lacks permission for this action. (${apiErr.message})`);
      } else if (apiErr.status === 409) {
        setErrorBanner(`409 Conflict: ${apiErr.message}`);
      } else {
        setErrorBanner(`Error (${apiErr.status}): ${apiErr.message}`);
      }
    } else {
      setErrorBanner(err?.message || 'Network or unexpected error occurred.');
    }
  };

  const fetchStats = useCallback(async () => {
    try {
      const data = await apiFetch<PlatformStats>('/api/stats');
      setStats(data);
    } catch (e) {
      handleError(e);
    }
  }, []);

  const fetchServers = useCallback(async () => {
    try {
      const data = await apiFetch<{ servers: ServerSummary[] }>('/api/servers');
      const list = data.servers || [];
      setServers(list);
      if (!selectedServerName && list.length > 0) {
        setSelectedServerName(list[0].name);
      }
    } catch (e) {
      handleError(e);
    }
  }, [selectedServerName]);

  const fetchServerDetail = useCallback(async (name: string) => {
    try {
      const data = await apiFetch<ServerDetail>(`/api/servers/${encodeURIComponent(name)}`);
      setServerDetail(data);
    } catch (e) {
      handleError(e);
    }
  }, []);

  const fetchDiffs = useCallback(async () => {
    try {
      const data = await apiFetch<{ diffs: ConfigDiffItem[] }>('/api/diffs');
      setDiffs(data.diffs || []);
    } catch (e) {
      // 403 on diffs is expected for non-operator tokens; silently ignore or log
      console.warn('Could not fetch diffs:', e);
    }
  }, []);

  const fetchPendingApprovals = useCallback(async () => {
    try {
      const [frames, cbs] = await Promise.all([
        listSuspendedFrames().catch(() => []),
        listPendingCallbacks().catch(() => []),
      ]);
      setPendingApprovalCount(frames.length + cbs.length);
    } catch {
      // Non-approver tokens ignore silently
    }
  }, []);

  const refreshAll = useCallback(() => {
    setErrorBanner(null);
    fetchStats();
    fetchServers();
    fetchDiffs();
    fetchPendingApprovals();
    if (selectedServerName) {
      fetchServerDetail(selectedServerName);
    }
  }, [fetchStats, fetchServers, fetchDiffs, fetchPendingApprovals, fetchServerDetail, selectedServerName]);

  useEffect(() => {
    refreshAll();
  }, []);

  // Live SSE telemetry subscription to /api/v1/events
  useEffect(() => {
    const controller = new AbortController();
    let active = true;

    const connectSse = async () => {
      try {
        const resp = await fetch('/api/v1/events', {
          headers: { Authorization: `Bearer ${getToken()}` },
          signal: controller.signal,
        });
        if (!resp.ok || !resp.body) {
          setSseConnected(false);
          return;
        }
        setSseConnected(true);
        const reader = resp.body.getReader();
        const decoder = new TextDecoder();
        let buffer = '';
        while (active) {
          const { done, value } = await reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream: true });
          const frames = buffer.split('\n\n');
          buffer = frames.pop() || '';
          for (const frame of frames) {
            if (
              frame.includes('event: server.health') ||
              frame.includes('event: approval.') ||
              frame.includes('event: codemode.') ||
              frame.includes('event: mcp.approval_required') ||
              frame.includes('event: mcp.callback_resolved') ||
              frame.includes('event: registry.')
            ) {
              setSseRevision((r) => r + 1);
              fetchServers();
              fetchStats();
              fetchDiffs();
              fetchPendingApprovals();
            }
          }
        }
      } catch {
        // Abort or reconnect
      } finally {
        if (active) setSseConnected(false);
      }
    };

    connectSse();
    return () => {
      active = false;
      controller.abort();
    };
  }, [fetchServers, fetchStats, fetchDiffs, fetchPendingApprovals]);

  useEffect(() => {
    if (selectedServerName) {
      fetchServerDetail(selectedServerName);
    }
  }, [selectedServerName, fetchServerDetail]);

  const handleToggleServerStatus = async (serverName: string, currentStatus: string) => {
    setErrorBanner(null);
    const newStatus = currentStatus === 'ENABLED' ? 'DISABLED' : 'ENABLED';
    try {
      await apiFetch('/api/servers/toggle_status', {
        method: 'POST',
        body: JSON.stringify({ serverName, status: newStatus }),
      });
      await fetchServers();
      if (selectedServerName === serverName) await fetchServerDetail(serverName);
      await fetchStats();
    } catch (e) {
      handleError(e);
    }
  };

  const handleToggleToolStatus = async (serverName: string, toolName: string, currentStatus: string) => {
    setErrorBanner(null);
    const newStatus = currentStatus === 'ENABLED' ? 'DISABLED' : 'ENABLED';
    try {
      await apiFetch('/api/tools/toggle_status', {
        method: 'POST',
        body: JSON.stringify({ serverName, toolName, status: newStatus }),
      });
      await fetchServerDetail(serverName);
      await fetchServers();
      await fetchStats();
    } catch (e) {
      handleError(e);
    }
  };

  const handleProposeUpdate = async (serverName: string, toolName: string, newDesc: string) => {
    setErrorBanner(null);
    try {
      await apiFetch('/api/tools/propose_update', {
        method: 'POST',
        body: JSON.stringify({ serverName, toolName, description: newDesc, author: 'engineer' }),
      });
      await fetchDiffs();
      setIsDiffModalOpen(true);
    } catch (e) {
      handleError(e);
    }
  };

  const handleApproveDiff = async (diffId: string) => {
    setErrorBanner(null);
    try {
      await apiFetch('/api/diffs/approve', {
        method: 'POST',
        body: JSON.stringify({ diffId, approver: 'service_owner' }),
      });
      await fetchDiffs();
      if (selectedServerName) await fetchServerDetail(selectedServerName);
      await fetchServers();
    } catch (e) {
      handleError(e);
    }
  };

  const handleRollback = async (serverName: string, toolName: string, targetVersion: number) => {
    setErrorBanner(null);
    try {
      await apiFetch('/api/tools/rollback', {
        method: 'POST',
        body: JSON.stringify({ serverName, toolName, targetVersion }),
      });
      await fetchServerDetail(serverName);
      await fetchServers();
    } catch (e) {
      handleError(e);
    }
  };

  const handleTriggerCrawler = async () => {
    setErrorBanner(null);
    try {
      await apiFetch('/api/crawler/trigger', { method: 'POST' });
      await fetchServers();
      await fetchStats();
      if (selectedServerName) await fetchServerDetail(selectedServerName);
    } catch (e) {
      handleError(e);
    }
  };

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100vh', width: '100vw', overflow: 'hidden' }}>
      <Header
        stats={stats}
        selectedActor={selectedActor}
        onActorChange={setSelectedActor}
        onTriggerCrawler={handleTriggerCrawler}
        onOpenOmni={() => setIsOmniModalOpen(true)}
        onOpenDiffs={() => setIsDiffModalOpen(true)}
        onOpenRegisterExternal={() => setIsRegisterExternalOpen(true)}
        onOpenToolGroups={() => setIsToolGroupsOpen(true)}
        onOpenCodeMode={() => setIsCodeModeOpen(true)}
        onOpenApprovalInbox={() => setIsApprovalInboxOpen(true)}
        onOpenObservability={() => setIsObservabilityOpen(true)}
        onOpenImportConfig={() => setIsImportConfigOpen(true)}
        sseConnected={sseConnected}
        pendingDiffCount={diffs.length}
        pendingApprovalCount={pendingApprovalCount}
        onTokenChanged={refreshAll}
      />

      {errorBanner && (
        <div style={{
          backgroundColor: '#421616',
          borderBottom: '1px solid #752525',
          color: '#ff9999',
          padding: '8px 24px',
          fontSize: '12px',
          display: 'flex',
          justifyContent: 'space-between',
          alignItems: 'center',
          zIndex: 90,
        }}>
          <span>⚠️ {errorBanner}</span>
          <button
            onClick={() => setErrorBanner(null)}
            style={{
              background: 'none',
              border: 'none',
              color: '#ff9999',
              cursor: 'pointer',
              fontWeight: 700,
            }}
          >
            ✕
          </button>
        </div>
      )}

      <div style={{ display: 'flex', flex: 1, overflow: 'hidden' }}>
        <RegistryCatalog
          servers={servers}
          selectedServer={selectedServerName}
          onSelectServer={setSelectedServerName}
          onToggleServerStatus={handleToggleServerStatus}
          onInspectServer={(s) => setInspectServerTarget(s)}
        />

        <ToolDetailView
          server={serverDetail}
          onToggleToolStatus={handleToggleToolStatus}
          onProposeUpdate={handleProposeUpdate}
          onRollback={handleRollback}
          onOpenTestConsole={(s, t, item) => setTestConsoleTarget({ serverName: s, toolName: t, tool: item })}
        />
      </div>

      <DiffApprovalModal
        isOpen={isDiffModalOpen}
        onClose={() => setIsDiffModalOpen(false)}
        diffs={diffs}
        onApprove={handleApproveDiff}
      />

      <OmniSearchModal
        isOpen={isOmniModalOpen}
        onClose={() => setIsOmniModalOpen(false)}
        onSelectServer={setSelectedServerName}
      />

      <RegisterExternalServerModal
        isOpen={isRegisterExternalOpen}
        onClose={() => setIsRegisterExternalOpen(false)}
        onRegistered={() => {
          fetchServers();
          fetchStats();
        }}
      />

      <ToolGroupsStudio
        isOpen={isToolGroupsOpen}
        onClose={() => setIsToolGroupsOpen(false)}
        servers={servers}
      />

      <CodeModeStudio
        isOpen={isCodeModeOpen}
        onClose={() => setIsCodeModeOpen(false)}
      />

      <ApprovalInboxModal
        isOpen={isApprovalInboxOpen}
        onClose={() => setIsApprovalInboxOpen(false)}
        onUpdated={refreshAll}
        sseRevision={sseRevision}
      />

      <ObservabilityStudio
        isOpen={isObservabilityOpen}
        onClose={() => setIsObservabilityOpen(false)}
      />

      <ImportConfigModal
        isOpen={isImportConfigOpen}
        onClose={() => setIsImportConfigOpen(false)}
        onImported={refreshAll}
      />

      {inspectServerTarget && (
        <ExternalServerInspector
          server={inspectServerTarget}
          isOpen={true}
          onClose={() => setInspectServerTarget(null)}
          onRediscovered={() => {
            fetchServers();
            if (selectedServerName) fetchServerDetail(selectedServerName);
            fetchStats();
          }}
          onDeregistered={() => {
            setSelectedServerName(null);
            fetchServers();
            fetchStats();
          }}
        />
      )}

      {testConsoleTarget && (
        <TestConsole
          serverName={testConsoleTarget.serverName}
          toolName={testConsoleTarget.toolName}
          tool={testConsoleTarget.tool}
          callerActor={selectedActor}
          isOpen={true}
          onClose={() => setTestConsoleTarget(null)}
        />
      )}
    </div>
  );
};

export default App;
