import React, { useState, useEffect } from 'react';
import { ServerSummary, ServerDetail, ServerInspection, ExternalSyncResult } from '../types';
import { apiFetch } from '../api';

interface ExternalServerInspectorProps {
  server: ServerSummary | ServerDetail | null;
  isOpen: boolean;
  onClose: () => void;
  onRediscovered: () => void;
  onDeregistered: () => void;
}

export const ExternalServerInspector: React.FC<ExternalServerInspectorProps> = ({
  server,
  isOpen,
  onClose,
  onRediscovered,
  onDeregistered,
}) => {
  if (!isOpen || !server) return null;

  const [inspection, setInspection] = useState<ServerInspection | null>(null);
  const [loading, setLoading] = useState(false);
  const [rediscovering, setRediscovering] = useState(false);
  const [deregistering, setDeregistering] = useState(false);
  const [showStderr, setShowStderr] = useState(false);
  const [syncResult, setSyncResult] = useState<ExternalSyncResult | null>(null);
  const [errorMsg, setErrorMsg] = useState<string | null>(null);

  const fetchInspection = async () => {
    setLoading(true);
    setErrorMsg(null);
    try {
      const data = await apiFetch<ServerInspection>(
        `/api/v1/servers/external/${encodeURIComponent(server.name)}/inspect`
      );
      setInspection(data);
    } catch (err: any) {
      setErrorMsg(err.message || 'Failed to inspect server.');
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    fetchInspection();
  }, [server.name]);

  const handleRediscover = async () => {
    setRediscovering(true);
    setSyncResult(null);
    setErrorMsg(null);
    try {
      const res = await apiFetch<ExternalSyncResult>(
        `/api/v1/servers/external/${encodeURIComponent(server.name)}/rediscover`,
        { method: 'POST' }
      );
      setSyncResult(res);
      await fetchInspection();
      onRediscovered();
    } catch (err: any) {
      setErrorMsg(err.message || 'Failed to rediscover tools.');
    } finally {
      setRediscovering(false);
    }
  };

  const handleDeregister = async () => {
    if (!window.confirm(`Are you sure you want to deregister and remove server '${server.name}'?`)) {
      return;
    }
    setDeregistering(true);
    setErrorMsg(null);
    try {
      await apiFetch(
        `/api/v1/servers/external/${encodeURIComponent(server.name)}/deregister`,
        { method: 'POST' }
      );
      onDeregistered();
      onClose();
    } catch (err: any) {
      setErrorMsg(err.message || 'Failed to deregister server.');
    } finally {
      setDeregistering(false);
    }
  };

  const getHealthBadge = (health?: string) => {
    switch (health) {
      case 'Connected':
        return <span style={{ color: '#00e575', backgroundColor: '#133522', padding: '3px 8px', borderRadius: '4px', fontSize: '11px', fontWeight: 700 }}>● CONNECTED</span>;
      case 'Idle':
        return <span style={{ color: '#ffd166', backgroundColor: '#383219', padding: '3px 8px', borderRadius: '4px', fontSize: '11px', fontWeight: 700 }}>● IDLE</span>;
      case 'Unreachable':
        return <span style={{ color: '#ff6b6b', backgroundColor: '#3b1c1c', padding: '3px 8px', borderRadius: '4px', fontSize: '11px', fontWeight: 700 }}>● UNREACHABLE</span>;
      default:
        return <span style={{ color: '#888', backgroundColor: '#222', padding: '3px 8px', borderRadius: '4px', fontSize: '11px' }}>UNKNOWN</span>;
    }
  };

  return (
    <div style={{
      position: 'fixed',
      top: 0,
      left: 0,
      right: 0,
      bottom: 0,
      backgroundColor: 'rgba(0, 0, 0, 0.75)',
      display: 'flex',
      alignItems: 'center',
      justifyContent: 'center',
      zIndex: 220,
    }}>
      <div style={{
        backgroundColor: '#161720',
        border: '1px solid #282a3b',
        borderRadius: '8px',
        width: '680px',
        maxWidth: '92vw',
        maxHeight: '90vh',
        display: 'flex',
        flexDirection: 'column',
        boxShadow: '0 20px 50px rgba(0,0,0,0.7)',
      }}>
        {/* Header */}
        <div style={{
          padding: '16px 20px',
          borderBottom: '1px solid #222332',
          display: 'flex',
          justifyContent: 'space-between',
          alignItems: 'center',
        }}>
          <div>
            <div style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
              <h3 style={{ margin: 0, fontSize: '17px', fontWeight: 700, color: '#fff' }}>
                {server.displayName}
              </h3>
              <span style={{ fontSize: '11px', backgroundColor: '#06d6a022', color: '#06d6a0', border: '1px solid #06d6a044', padding: '1px 6px', borderRadius: '4px', fontWeight: 600 }}>
                EXTERNAL_MCP
              </span>
              {inspection && getHealthBadge(inspection.health)}
            </div>
            <div style={{ fontSize: '12px', color: '#888', marginTop: '3px' }}>
              <code>{server.name}</code> · Owning Team: <strong style={{ color: '#aaa' }}>{server.owningTeam}</strong>
            </div>
          </div>
          <button
            onClick={onClose}
            style={{
              background: 'none',
              border: 'none',
              color: '#888',
              fontSize: '18px',
              cursor: 'pointer',
            }}
          >
            ✕
          </button>
        </div>

        {/* Content */}
        <div style={{ padding: '20px', overflowY: 'auto', flex: 1 }}>
          {errorMsg && (
            <div style={{
              backgroundColor: '#441818',
              border: '1px solid #772b2b',
              color: '#ff8888',
              padding: '10px 14px',
              borderRadius: '6px',
              fontSize: '12px',
              marginBottom: '16px',
            }}>
              ⚠️ {errorMsg}
            </div>
          )}

          {/* Sync Result Banner */}
          {syncResult && (
            <div style={{
              backgroundColor: '#1b382b',
              border: '1px solid #00e57544',
              color: '#00e575',
              padding: '12px 16px',
              borderRadius: '6px',
              fontSize: '13px',
              marginBottom: '16px',
              display: 'flex',
              gap: '16px',
              alignItems: 'center',
            }}>
              <span style={{ fontWeight: 700 }}>✓ Reconciled:</span>
              <span>+{syncResult.added} Added</span>
              <span>~{syncResult.updated} Updated</span>
              <span>-{syncResult.retired} Retired</span>
              <span>={syncResult.unchanged} Unchanged</span>
              {syncResult.pending_human_conflicts > 0 && (
                <span style={{ color: '#ffbb33', fontWeight: 700 }}>
                  ! {syncResult.pending_human_conflicts} Pending Human Conflict(s)
                </span>
              )}
            </div>
          )}

          {/* Description */}
          <p style={{ margin: '0 0 16px 0', fontSize: '13px', color: '#b0b3c6', lineHeight: '1.5' }}>
            {server.description || 'No description provided.'}
          </p>

          {/* Metrics / Status Grid */}
          <div style={{
            display: 'grid',
            gridTemplateColumns: 'repeat(4, 1fr)',
            gap: '10px',
            marginBottom: '16px',
            background: '#0e0f14',
            padding: '12px',
            borderRadius: '6px',
            border: '1px solid #222332',
          }}>
            <div>
              <div style={{ fontSize: '11px', color: '#888' }}>Total Tools</div>
              <div style={{ fontSize: '18px', fontWeight: 700, color: '#fff' }}>
                {inspection?.total_tools ?? server.toolsCount}
              </div>
            </div>
            <div>
              <div style={{ fontSize: '11px', color: '#888' }}>Active Tools</div>
              <div style={{ fontSize: '18px', fontWeight: 700, color: '#00e575' }}>
                {inspection?.enabled_tools ?? server.enabledToolsCount}
              </div>
            </div>
            <div>
              <div style={{ fontSize: '11px', color: '#888' }}>Disabled Tools</div>
              <div style={{ fontSize: '18px', fontWeight: 700, color: '#aaa' }}>
                {inspection?.disabled_tools ?? (server.toolsCount - server.enabledToolsCount)}
              </div>
            </div>
            <div>
              <div style={{ fontSize: '11px', color: '#888' }}>Unavailable</div>
              <div style={{ fontSize: '18px', fontWeight: 700, color: '#ff6b6b' }}>
                {inspection?.unavailable_tools ?? 0}
              </div>
            </div>
          </div>

          {/* Transport Summary */}
          {inspection?.transport_summary && (
            <div style={{
              background: '#0e0f14',
              padding: '12px 16px',
              borderRadius: '6px',
              border: '1px solid #222332',
              marginBottom: '16px',
            }}>
              <div style={{ fontSize: '12px', fontWeight: 700, color: '#aaa', marginBottom: '8px' }}>
                UPSTREAM CONFIGURATION (Credentials shown as symbolic references)
              </div>
              <pre style={{ margin: 0, fontSize: '12px', color: '#00e575', fontFamily: 'monospace', whiteSpace: 'pre-wrap', wordBreak: 'break-all' }}>
                {JSON.stringify(inspection.transport_summary, null, 2)}
              </pre>
            </div>
          )}

          {/* Stderr Drawer Toggle */}
          {inspection && inspection.stderr_tail && inspection.stderr_tail.length > 0 && (
            <div style={{ marginBottom: '16px' }}>
              <button
                type="button"
                onClick={() => setShowStderr(!showStderr)}
                style={{
                  background: '#222330',
                  border: '1px solid #333446',
                  color: '#fff',
                  fontSize: '12px',
                  padding: '6px 12px',
                  borderRadius: '4px',
                  cursor: 'pointer',
                  width: '100%',
                  textAlign: 'left',
                  display: 'flex',
                  justifyContent: 'space-between',
                  alignItems: 'center',
                }}
              >
                <span>🔍 Stderr Diagnostics Ring Buffer ({inspection.stderr_tail.length} lines)</span>
                <span>{showStderr ? '▲ Collapse' : '▼ Expand'}</span>
              </button>

              {showStderr && (
                <div style={{
                  backgroundColor: '#0a0a0f',
                  border: '1px solid #222332',
                  borderTop: 'none',
                  padding: '10px 14px',
                  maxHeight: '180px',
                  overflowY: 'auto',
                  fontFamily: 'monospace',
                  fontSize: '11px',
                  color: '#ff9999',
                  whiteSpace: 'pre-wrap',
                }}>
                  {inspection.stderr_tail.map((line, idx) => (
                    <div key={idx}>{line}</div>
                  ))}
                </div>
              )}
            </div>
          )}

          {/* Actions */}
          <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginTop: '20px', borderTop: '1px solid #222332', paddingTop: '16px' }}>
            <button
              type="button"
              onClick={handleDeregister}
              disabled={deregistering}
              style={{
                backgroundColor: '#3b1c1c',
                border: '1px solid #772b2b',
                color: '#ff6b6b',
                padding: '8px 14px',
                borderRadius: '6px',
                fontWeight: 600,
                fontSize: '12px',
                cursor: deregistering ? 'wait' : 'pointer',
              }}
            >
              {deregistering ? 'Deregistering...' : '🗑️ Deregister Server'}
            </button>

            <div style={{ display: 'flex', gap: '10px' }}>
              <button
                type="button"
                onClick={handleRediscover}
                disabled={rediscovering}
                style={{
                  backgroundColor: '#276ef1',
                  border: 'none',
                  color: '#fff',
                  padding: '8px 16px',
                  borderRadius: '6px',
                  fontWeight: 600,
                  fontSize: '12px',
                  cursor: rediscovering ? 'wait' : 'pointer',
                  display: 'flex',
                  alignItems: 'center',
                  gap: '6px',
                }}
              >
                {rediscovering ? 'Syncing...' : '🔄 Rediscover Tools'}
              </button>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
};
