import React, { useState, useEffect, useCallback } from 'react';
import { McpTaskItem } from '../types';
import { fetchTasks, cancelTask } from '../api';

interface TasksMonitorModalProps {
  isOpen: boolean;
  onClose: () => void;
  sseRevision?: number;
}

export const TasksMonitorModal: React.FC<TasksMonitorModalProps> = ({
  isOpen,
  onClose,
  sseRevision = 0,
}) => {
  const [tasks, setTasks] = useState<McpTaskItem[]>([]);
  const [loading, setLoading] = useState(false);
  const [errorMsg, setErrorMsg] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);

  const loadTasks = useCallback(async () => {
    setLoading(true);
    setErrorMsg(null);
    try {
      const list = await fetchTasks();
      setTasks(list);
    } catch (e: any) {
      setErrorMsg(e?.message || 'Failed to load MCP Tasks');
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (isOpen) {
      void loadTasks();
    }
  }, [isOpen, sseRevision, loadTasks]);

  if (!isOpen) return null;

  const handleCancel = async (taskId: string) => {
    setBusyId(taskId);
    setErrorMsg(null);
    try {
      await cancelTask(taskId);
      await loadTasks();
    } catch (e: any) {
      setErrorMsg(e?.message || 'Failed to cancel task');
    } finally {
      setBusyId(null);
    }
  };

  const badgeColor = (status: string) => {
    if (status === 'completed') return { bg: '#123225', fg: '#00e575' };
    if (status === 'working') return { bg: '#1b2b4a', fg: '#79c0ff' };
    if (status === 'input_required') return { bg: '#4d2d00', fg: '#ffd27d' };
    if (status === 'cancelled') return { bg: '#2e2f3a', fg: '#9ca3af' };
    return { bg: '#3f1515', fg: '#fca5a5' };
  };

  return (
    <div
      data-testid="tasks-monitor-modal"
      style={{
        position: 'fixed',
        inset: 0,
        backgroundColor: 'rgba(0, 0, 0, 0.72)',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        zIndex: 1000,
      }}
    >
      <div
        style={{
          backgroundColor: '#13141c',
          border: '1px solid #2b2d3e',
          borderRadius: '10px',
          width: '900px',
          maxWidth: '94vw',
          maxHeight: '85vh',
          display: 'flex',
          flexDirection: 'column',
          overflow: 'hidden',
          color: '#e5e7eb',
        }}
      >
        <div
          style={{
            padding: '14px 20px',
            borderBottom: '1px solid #252736',
            display: 'flex',
            justifyContent: 'space-between',
            alignItems: 'center',
          }}
        >
          <div>
            <h2 style={{ margin: 0, fontSize: '16px', fontWeight: 700 }}>
              ⏱️ MCP Tasks Monitor (io.modelcontextprotocol/tasks · GA)
            </h2>
            <div style={{ fontSize: '12px', color: '#9ca3af', marginTop: '2px' }}>
              Live SSE view of Postgres/SQLite/WAL-backed tasks, inputRequests, and cooperative cancellation
            </div>
          </div>
          <div style={{ display: 'flex', gap: '8px' }}>
            <button
              onClick={() => void loadTasks()}
              style={{
                background: '#1e2235',
                color: '#79c0ff',
                border: '1px solid #303652',
                borderRadius: '6px',
                padding: '5px 10px',
                fontSize: '12px',
                cursor: 'pointer',
              }}
            >
              ↻ Refresh
            </button>
            <button
              onClick={onClose}
              style={{
                background: '#222434',
                color: '#fff',
                border: '1px solid #35384d',
                borderRadius: '6px',
                padding: '5px 10px',
                cursor: 'pointer',
              }}
            >
              ✕ Close
            </button>
          </div>
        </div>

        <div style={{ padding: '16px 20px', overflowY: 'auto', flex: 1 }}>
          {errorMsg && (
            <div style={{ background: '#3a1414', color: '#fca5a5', padding: '8px 12px', borderRadius: '6px', fontSize: '12px', marginBottom: '12px' }}>
              ⚠️ {errorMsg}
            </div>
          )}
          {loading && tasks.length === 0 ? (
            <div style={{ color: '#9ca3af', fontSize: '13px' }}>Loading tasks...</div>
          ) : tasks.length === 0 ? (
            <div style={{ color: '#9ca3af', fontSize: '13px', padding: '24px', textAlign: 'center', border: '1px dashed #2c2f42', borderRadius: '8px' }}>
              No MCP Tasks recorded yet. Long-running Code Mode scripts or elicitation flows will appear here live via SSE.
            </div>
          ) : (
            <div style={{ display: 'flex', flexDirection: 'column', gap: '10px' }}>
              {tasks.map((t) => {
                const c = badgeColor(t.status);
                const canCancel = t.status === 'working' || t.status === 'input_required';
                return (
                  <div
                    key={t.task_id}
                    data-testid={`task-row-${t.task_id}`}
                    style={{
                      background: '#181a24',
                      border: '1px solid #282a3b',
                      borderRadius: '8px',
                      padding: '12px',
                    }}
                  >
                    <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
                      <div style={{ display: 'flex', alignItems: 'center', gap: '10px' }}>
                        <code style={{ fontSize: '12px', color: '#fff' }}>{t.task_id}</code>
                        <span
                          style={{
                            fontSize: '11px',
                            background: c.bg,
                            color: c.fg,
                            padding: '2px 8px',
                            borderRadius: '10px',
                            fontWeight: 600,
                          }}
                        >
                          {t.status.toUpperCase()}
                        </span>
                        <span style={{ fontSize: '11px', color: '#9ca3af' }}>
                          actor: {t.caller_actor} · pod: {t.owner_pod_id}
                        </span>
                      </div>
                      {canCancel && (
                        <button
                          data-testid={`cancel-task-${t.task_id}`}
                          disabled={busyId === t.task_id}
                          onClick={() => void handleCancel(t.task_id)}
                          style={{
                            background: '#4a1717',
                            color: '#fca5a5',
                            border: '1px solid #7f1d1d',
                            borderRadius: '6px',
                            padding: '4px 10px',
                            fontSize: '11px',
                            fontWeight: 600,
                            cursor: 'pointer',
                          }}
                        >
                          {busyId === t.task_id ? 'Cancelling...' : 'Cancel Task'}
                        </button>
                      )}
                    </div>
                    {t.status_message && (
                      <div style={{ fontSize: '12px', color: '#d1d5db', marginTop: '6px' }}>
                        {t.status_message}
                      </div>
                    )}
                    {t.input_requests && (
                      <pre
                        style={{
                          marginTop: '8px',
                          padding: '8px',
                          background: '#0e0f14',
                          borderRadius: '6px',
                          fontSize: '11px',
                          color: '#ffd27d',
                          overflowX: 'auto',
                        }}
                      >
                        {JSON.stringify(t.input_requests, null, 2)}
                      </pre>
                    )}
                  </div>
                );
              })}
            </div>
          )}
        </div>
      </div>
    </div>
  );
};
