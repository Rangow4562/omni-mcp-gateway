import React, { useState, useEffect, useCallback, useRef } from 'react';
import { SuspendedFrameItem, PendingCallbackItem } from '../types';
import {
  listSuspendedFrames,
  resumeSuspendedFrame,
  cancelSuspendedFrame,
  listPendingCallbacks,
  respondPendingCallback,
  getToken,
  setToken,
  DEV_ROLE_TOKENS,
} from '../api';

interface ApprovalInboxModalProps {
  isOpen: boolean;
  onClose: () => void;
  onUpdated?: () => void;
  sseRevision?: number;
}

export const ApprovalInboxModal: React.FC<ApprovalInboxModalProps> = ({
  isOpen,
  onClose,
  onUpdated,
  sseRevision = 0,
}) => {
  const [frames, setFrames] = useState<SuspendedFrameItem[]>([]);
  const [callbacks, setCallbacks] = useState<PendingCallbackItem[]>([]);
  const [loading, setLoading] = useState(false);
  const [statusMsg, setStatusMsg] = useState<string | null>(null);
  const [errorMsg, setErrorMsg] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const prevOpenRef = useRef(false);
  const requestSeqRef = useRef(0);

  const loadInbox = useCallback(async (options?: { background?: boolean }) => {
    const isBackground = Boolean(options?.background);
    const reqId = ++requestSeqRef.current;
    if (!isBackground) {
      setLoading(true);
      setErrorMsg(null);
    }
    try {
      const [fList, cbList] = await Promise.all([
        listSuspendedFrames(),
        listPendingCallbacks().catch(() => []),
      ]);
      if (reqId !== requestSeqRef.current) return;
      setFrames(fList);
      setCallbacks(cbList);
    } catch (e: any) {
      if (reqId !== requestSeqRef.current) return;
      const msg = e?.message || 'Failed to load HITL Approval Inbox (Approver/Admin role required)';
      if (isBackground) {
        setErrorMsg((prev) => prev ?? msg);
      } else {
        setErrorMsg(msg);
      }
    } finally {
      if (!isBackground && reqId === requestSeqRef.current) {
        setLoading(false);
      }
    }
  }, []);

  useEffect(() => {
    if (!isOpen) {
      prevOpenRef.current = false;
      return;
    }
    const isInitialOpen = !prevOpenRef.current;
    prevOpenRef.current = true;
    if (isInitialOpen) {
      setStatusMsg(null);
      setErrorMsg(null);
      void loadInbox({ background: false });
    } else {
      void loadInbox({ background: true });
    }
  }, [isOpen, sseRevision, loadInbox]);

  if (!isOpen) return null;

  const currentToken = getToken();
  const currentPrincipalGuess = currentToken.includes('admin')
    ? 'admin_dev'
    : currentToken.includes('approver')
    ? 'approver_dev'
    : currentToken.includes('operator')
    ? 'operator_dev'
    : currentToken.includes('agent')
    ? 'agent_dev'
    : currentToken;

  const handleApproveResume = async (frame: SuspendedFrameItem) => {
    setBusyId(frame.execution_id);
    setErrorMsg(null);
    setStatusMsg(null);
    try {
      const res = await resumeSuspendedFrame(frame.execution_id, frame.pending_arguments_hash);
      if (res?.error) {
        setErrorMsg(`Resume rejected: ${res.error}`);
        await loadInbox({ background: true });
        onUpdated?.();
      } else {
        setStatusMsg(
          `Approved & resumed ${frame.execution_id} (${frame.pending_server}.${frame.pending_tool}) — Status: ${res?.status || 'OK'}`
        );
        await loadInbox({ background: true });
        onUpdated?.();
      }
    } catch (e: any) {
      setErrorMsg(e?.message || 'Failed to resume suspended execution frame');
    } finally {
      setBusyId(null);
    }
  };

  const handleCancelFrame = async (executionId: string) => {
    setBusyId(executionId);
    setErrorMsg(null);
    setStatusMsg(null);
    try {
      await cancelSuspendedFrame(executionId);
      setStatusMsg(`Cancelled suspended frame ${executionId}`);
      await loadInbox({ background: true });
      onUpdated?.();
    } catch (e: any) {
      setErrorMsg(e?.message || 'Failed to cancel suspended frame');
    } finally {
      setBusyId(null);
    }
  };

  const handleRespondCallback = async (cb: PendingCallbackItem, approved: boolean) => {
    setBusyId(cb.callback_id);
    setErrorMsg(null);
    setStatusMsg(null);
    try {
      await respondPendingCallback(
        cb.callback_id,
        approved,
        approved ? { approved: true, model: 'operator-approved', content: { type: 'text', text: 'Approved by operator' } } : undefined,
        approved ? undefined : 'Rejected in Governance Approval Inbox'
      );
      setStatusMsg(`${approved ? 'Approved' : 'Rejected'} upstream callback ${cb.callback_id}`);
      await loadInbox({ background: true });
      onUpdated?.();
    } catch (e: any) {
      setErrorMsg(e?.message || 'Failed to resolve upstream callback');
    } finally {
      setBusyId(null);
    }
  };

  const renderHighlightedScript = (script: string, targetTool: string) => {
    return script.split('\n').map((line, idx) => {
      const isMutationLine = line.includes(targetTool);
      return (
        <div
          key={idx}
          style={{
            backgroundColor: isMutationLine ? '#3b240c' : 'transparent',
            borderLeft: isMutationLine ? '3px solid #ff9900' : '3px solid transparent',
            padding: '1px 8px',
            color: isMutationLine ? '#ffd27d' : '#c9d1d9',
          }}
        >
          <span style={{ color: '#555', marginRight: '10px', userSelect: 'none' }}>
            {String(idx + 1).padStart(2, ' ')}
          </span>
          {line}
        </div>
      );
    });
  };

  return (
    <div
      style={{
        position: 'fixed',
        inset: 0,
        backgroundColor: 'rgba(0,0,0,0.75)',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        zIndex: 1000,
      }}
    >
      <div
        data-testid="approval-inbox-modal"
        style={{
          backgroundColor: '#12131b',
          border: '1px solid #2b2d3f',
          borderRadius: '10px',
          width: '880px',
          maxWidth: '94vw',
          maxHeight: '88vh',
          display: 'flex',
          flexDirection: 'column',
          overflow: 'hidden',
        }}
      >
        {/* Header */}
        <div
          style={{
            padding: '16px 20px',
            borderBottom: '1px solid #232535',
            display: 'flex',
            justifyContent: 'space-between',
            alignItems: 'center',
          }}
        >
          <div>
            <h2 style={{ margin: 0, fontSize: '16px', fontWeight: 700, color: '#fff' }}>
              🛡️ HITL Approval Inbox
            </h2>
            <div style={{ fontSize: '12px', color: '#8b8fa8', marginTop: '2px' }}>
              Review paused mutating Code Mode continuations &amp; upstream sampling/elicitation gates
            </div>
          </div>
          <div style={{ display: 'flex', gap: '8px', alignItems: 'center' }}>
            <button
              onClick={() => void loadInbox({ background: false })}
              style={{
                background: '#1e2030',
                border: '1px solid #30334a',
                color: '#c9d1d9',
                padding: '5px 10px',
                borderRadius: '6px',
                fontSize: '12px',
                cursor: 'pointer',
              }}
            >
              🔄 Refresh
            </button>
            <button
              onClick={onClose}
              aria-label="Close Approval Inbox"
              style={{
                background: '#252738',
                border: 'none',
                color: '#fff',
                width: '28px',
                height: '28px',
                borderRadius: '6px',
                cursor: 'pointer',
                fontWeight: 700,
              }}
            >
              ✕
            </button>
          </div>
        </div>

        {/* Body */}
        <div style={{ padding: '16px 20px', overflowY: 'auto', flex: 1 }}>
          {statusMsg && (
            <div
              style={{
                backgroundColor: '#143325',
                border: '1px solid #00e57555',
                color: '#00e575',
                padding: '10px 14px',
                borderRadius: '6px',
                fontSize: '12px',
                marginBottom: '14px',
              }}
            >
              ✓ {statusMsg}
            </div>
          )}

          {errorMsg && (
            <div
              style={{
                backgroundColor: '#3d1616',
                border: '1px solid #7a2929',
                color: '#ff9999',
                padding: '10px 14px',
                borderRadius: '6px',
                fontSize: '12px',
                marginBottom: '14px',
              }}
            >
              ⚠️ {errorMsg}
            </div>
          )}

          {loading ? (
            <div style={{ color: '#8b8fa8', padding: '24px', textAlign: 'center', fontSize: '13px' }}>
              Loading suspended execution frames...
            </div>
          ) : frames.length === 0 && callbacks.length === 0 ? (
            <div
              style={{
                color: '#8b8fa8',
                padding: '36px 20px',
                textAlign: 'center',
                border: '1px dashed #2a2d40',
                borderRadius: '8px',
                fontSize: '13px',
              }}
            >
              No pending HITL approvals or upstream sampling callbacks.
            </div>
          ) : (
            <div style={{ display: 'flex', flexDirection: 'column', gap: '16px' }}>
              {frames.map((frame) => {
                const sameAsRequester =
                  Boolean(frame.caller_actor) &&
                  (frame.caller_actor === currentPrincipalGuess ||
                    currentToken.startsWith(frame.caller_actor));
                return (
                  <div
                    key={frame.execution_id}
                    data-testid={`suspended-frame-${frame.execution_id}`}
                    style={{
                      backgroundColor: '#0c0d13',
                      border: '1px solid #2b2e42',
                      borderRadius: '8px',
                      padding: '14px 16px',
                    }}
                  >
                    <div
                      style={{
                        display: 'flex',
                        justifyContent: 'space-between',
                        alignItems: 'center',
                        marginBottom: '10px',
                      }}
                    >
                      <div>
                        <span
                          style={{
                            backgroundColor: '#3b240c',
                            color: '#ffaa33',
                            fontSize: '11px',
                            fontWeight: 700,
                            padding: '2px 8px',
                            borderRadius: '10px',
                            marginRight: '8px',
                          }}
                        >
                          MUTATION GATE
                        </span>
                        <strong style={{ color: '#fff', fontSize: '14px' }}>
                          {frame.pending_server}.{frame.pending_tool}
                        </strong>
                        <span style={{ color: '#7c819a', fontSize: '11px', marginLeft: '10px' }}>
                          ID: {frame.execution_id}
                        </span>
                      </div>
                      <div style={{ fontSize: '11px', color: '#9ea3bd' }}>
                        Requester: <strong style={{ color: '#a5d6ff' }}>{frame.caller_actor}</strong>
                      </div>
                    </div>

                    {sameAsRequester && (
                      <div
                        style={{
                          backgroundColor: '#36230d',
                          border: '1px solid #754a18',
                          color: '#ffd27d',
                          fontSize: '11px',
                          padding: '8px 10px',
                          borderRadius: '6px',
                          marginBottom: '10px',
                          display: 'flex',
                          justifyContent: 'space-between',
                          alignItems: 'center',
                        }}
                      >
                        <span>
                          ⚠️ Separation-of-Duties Warning: Your active token matches requester &quot;
                          {frame.caller_actor}&quot;. Switch to a distinct Approver token to resume.
                        </span>
                        <button
                          onClick={() => {
                            setToken(DEV_ROLE_TOKENS.approver);
                            onUpdated?.();
                            void loadInbox({ background: false });
                          }}
                          style={{
                            background: '#ff9900',
                            color: '#000',
                            border: 'none',
                            borderRadius: '4px',
                            padding: '3px 8px',
                            fontWeight: 700,
                            fontSize: '11px',
                            cursor: 'pointer',
                            marginLeft: '10px',
                          }}
                        >
                          Switch to Approver
                        </button>
                      </div>
                    )}

                    <div style={{ fontSize: '11px', color: '#8b8fa8', marginBottom: '8px' }}>
                      Arguments SHA-256 Digest:{' '}
                      <code style={{ color: '#79c0ff' }}>{frame.pending_arguments_hash}</code>
                    </div>

                    {frame.script_preview && (
                      <div
                        style={{
                          backgroundColor: '#07080c',
                          border: '1px solid #222536',
                          borderRadius: '6px',
                          padding: '8px 0',
                          fontFamily: 'monospace',
                          fontSize: '11px',
                          marginBottom: '10px',
                          overflowX: 'auto',
                        }}
                      >
                        {renderHighlightedScript(frame.script_preview, frame.pending_tool)}
                      </div>
                    )}

                    <div
                      style={{
                        display: 'grid',
                        gridTemplateColumns: '1fr 1fr',
                        gap: '10px',
                        marginBottom: '12px',
                      }}
                    >
                      <div
                        style={{
                          backgroundColor: '#090a0f',
                          border: '1px solid #202334',
                          borderRadius: '6px',
                          padding: '8px 10px',
                        }}
                      >
                        <div style={{ fontSize: '10px', color: '#7c819a', marginBottom: '4px' }}>
                          PENDING ARGUMENTS (PII-REDACTED)
                        </div>
                        <pre style={{ margin: 0, fontSize: '11px', color: '#00e575', overflowX: 'auto' }}>
                          {JSON.stringify(frame.pending_arguments_preview ?? {}, null, 2)}
                        </pre>
                      </div>
                      <div
                        style={{
                          backgroundColor: '#090a0f',
                          border: '1px solid #202334',
                          borderRadius: '6px',
                          padding: '8px 10px',
                        }}
                      >
                        <div style={{ fontSize: '10px', color: '#7c819a', marginBottom: '4px' }}>
                          SERIALIZED SCOPE PREVIEW
                        </div>
                        <pre style={{ margin: 0, fontSize: '11px', color: '#d2a8ff', overflowX: 'auto' }}>
                          {JSON.stringify(frame.scope_preview ?? {}, null, 2)}
                        </pre>
                      </div>
                    </div>

                    <div style={{ display: 'flex', justifyContent: 'flex-end', gap: '10px' }}>
                      <button
                        onClick={() => handleCancelFrame(frame.execution_id)}
                        disabled={busyId === frame.execution_id}
                        style={{
                          backgroundColor: '#3b1c1c',
                          border: '1px solid #772b2b',
                          color: '#ff8080',
                          padding: '7px 14px',
                          borderRadius: '6px',
                          fontWeight: 600,
                          fontSize: '12px',
                          cursor: 'pointer',
                        }}
                      >
                        Reject &amp; Discard
                      </button>
                      <button
                        onClick={() => handleApproveResume(frame)}
                        disabled={busyId === frame.execution_id}
                        style={{
                          backgroundColor: '#00e575',
                          border: 'none',
                          color: '#000',
                          padding: '7px 16px',
                          borderRadius: '6px',
                          fontWeight: 700,
                          fontSize: '12px',
                          cursor: 'pointer',
                        }}
                      >
                        {busyId === frame.execution_id ? 'Resuming...' : '✓ Approve & Resume'}
                      </button>
                    </div>
                  </div>
                );
              })}

              {callbacks.map((cb) => (
                <div
                  key={cb.callback_id}
                  style={{
                    backgroundColor: '#0c0d13',
                    border: '1px solid #334066',
                    borderRadius: '8px',
                    padding: '14px 16px',
                  }}
                >
                  <div style={{ display: 'flex', justifyContent: 'space-between', marginBottom: '8px' }}>
                    <div>
                      <span
                        style={{
                          backgroundColor: '#1b2b4a',
                          color: '#79c0ff',
                          fontSize: '11px',
                          fontWeight: 700,
                          padding: '2px 8px',
                          borderRadius: '10px',
                          marginRight: '8px',
                        }}
                      >
                        {cb.mode}
                      </span>
                      <strong style={{ color: '#fff', fontSize: '13px' }}>
                        {cb.server_name} → {cb.method}
                      </strong>
                    </div>
                    <span style={{ fontSize: '11px', color: '#8b8fa8' }}>{cb.callback_id}</span>
                  </div>
                  <pre
                    style={{
                      backgroundColor: '#07080c',
                      padding: '8px 10px',
                      borderRadius: '6px',
                      fontSize: '11px',
                      color: '#c9d1d9',
                      overflowX: 'auto',
                    }}
                  >
                    {JSON.stringify(cb.params, null, 2)}
                  </pre>
                  <div style={{ display: 'flex', justifyContent: 'flex-end', gap: '8px', marginTop: '10px' }}>
                    <button
                      onClick={() => handleRespondCallback(cb, false)}
                      disabled={busyId === cb.callback_id}
                      style={{
                        backgroundColor: '#3b1c1c',
                        border: '1px solid #772b2b',
                        color: '#ff8080',
                        padding: '6px 12px',
                        borderRadius: '6px',
                        fontSize: '12px',
                        cursor: 'pointer',
                      }}
                    >
                      Deny
                    </button>
                    <button
                      onClick={() => handleRespondCallback(cb, true)}
                      disabled={busyId === cb.callback_id}
                      style={{
                        backgroundColor: '#276ef1',
                        border: 'none',
                        color: '#fff',
                        padding: '6px 14px',
                        borderRadius: '6px',
                        fontWeight: 600,
                        fontSize: '12px',
                        cursor: 'pointer',
                      }}
                    >
                      Approve Callback
                    </button>
                  </div>
                </div>
              ))}
            </div>
          )}
        </div>
      </div>
    </div>
  );
};
