import React, { useState, useEffect } from 'react';
import { Header } from './components/Header';
import { RegistryCatalog } from './components/RegistryCatalog';
import { ToolDetailView } from './components/ToolDetailView';
import { DiffApprovalModal } from './components/DiffApprovalModal';
import { TestConsole } from './components/TestConsole';
import { OmniSearchModal } from './components/OmniSearchModal';
import { ServerSummary, ServerDetail, ConfigDiffItem, PlatformStats, ToolItem } from './types';

export const App: React.FC = () => {
  const [servers, setServers] = useState<ServerSummary[]>([]);
  const [selectedServerName, setSelectedServerName] = useState<string | null>(null);
  const [serverDetail, setServerDetail] = useState<ServerDetail | null>(null);
  const [stats, setStats] = useState<PlatformStats | null>(null);
  const [diffs, setDiffs] = useState<ConfigDiffItem[]>([]);
  const [selectedActor, setSelectedActor] = useState<string>('agent');

  // Modals
  const [isDiffModalOpen, setIsDiffModalOpen] = useState(false);
  const [isOmniModalOpen, setIsOmniModalOpen] = useState(false);
  const [testConsoleTarget, setTestConsoleTarget] = useState<{
    serverName: string;
    toolName: string;
    tool: ToolItem;
  } | null>(null);

  const fetchStats = async () => {
    try {
      const res = await fetch('http://localhost:8080/api/stats');
      if (res.ok) setStats(await res.json());
    } catch (e) {
      console.error(e);
    }
  };

  const fetchServers = async () => {
    try {
      const res = await fetch('http://localhost:8080/api/servers');
      if (res.ok) {
        const data = await res.json();
        setServers(data.servers || []);
        if (!selectedServerName && data.servers && data.servers.length > 0) {
          setSelectedServerName(data.servers[0].name);
        }
      }
    } catch (e) {
      console.error(e);
    }
  };

  const fetchServerDetail = async (name: string) => {
    try {
      const res = await fetch(`http://localhost:8080/api/servers/${name}`);
      if (res.ok) {
        setServerDetail(await res.json());
      }
    } catch (e) {
      console.error(e);
    }
  };

  const fetchDiffs = async () => {
    try {
      const res = await fetch('http://localhost:8080/api/diffs');
      if (res.ok) {
        const data = await res.json();
        setDiffs(data.diffs || []);
      }
    } catch (e) {
      console.error(e);
    }
  };

  useEffect(() => {
    fetchStats();
    fetchServers();
    fetchDiffs();
  }, []);

  useEffect(() => {
    if (selectedServerName) {
      fetchServerDetail(selectedServerName);
    }
  }, [selectedServerName]);

  const handleToggleServerStatus = async (serverName: string, currentStatus: string) => {
    const newStatus = currentStatus === 'ENABLED' ? 'DISABLED' : 'ENABLED';
    await fetch('http://localhost:8080/api/servers/toggle_status', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ serverName, status: newStatus }),
    });
    fetchServers();
    if (selectedServerName === serverName) fetchServerDetail(serverName);
    fetchStats();
  };

  const handleToggleToolStatus = async (serverName: string, toolName: string, currentStatus: string) => {
    const newStatus = currentStatus === 'ENABLED' ? 'DISABLED' : 'ENABLED';
    await fetch('http://localhost:8080/api/tools/toggle_status', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ serverName, toolName, status: newStatus }),
    });
    fetchServerDetail(serverName);
    fetchServers();
    fetchStats();
  };

  const handleProposeUpdate = async (serverName: string, toolName: string, newDesc: string) => {
    await fetch('http://localhost:8080/api/tools/propose_update', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ serverName, toolName, description: newDesc, author: 'engineer' }),
    });
    fetchDiffs();
    setIsDiffModalOpen(true);
  };

  const handleApproveDiff = async (diffId: string) => {
    await fetch('http://localhost:8080/api/diffs/approve', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ diffId, approver: 'service_owner' }),
    });
    fetchDiffs();
    if (selectedServerName) fetchServerDetail(selectedServerName);
    fetchServers();
  };

  const handleRollback = async (serverName: string, toolName: string, targetVersion: number) => {
    await fetch('http://localhost:8080/api/tools/rollback', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ serverName, toolName, targetVersion }),
    });
    fetchServerDetail(serverName);
    fetchServers();
  };

  const handleTriggerCrawler = async () => {
    await fetch('http://localhost:8080/api/crawler/trigger', { method: 'POST' });
    fetchServers();
    fetchStats();
    if (selectedServerName) fetchServerDetail(selectedServerName);
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
        pendingDiffCount={diffs.length}
      />

      <div style={{ display: 'flex', flex: 1, overflow: 'hidden' }}>
        <RegistryCatalog
          servers={servers}
          selectedServer={selectedServerName}
          onSelectServer={setSelectedServerName}
          onToggleServerStatus={handleToggleServerStatus}
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

