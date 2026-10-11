import React from 'react';
import { ConfigDiffItem } from '../types';

interface DiffApprovalModalProps {
  diffs: ConfigDiffItem[];
  isOpen: boolean;
  onClose: () => void;
  onApprove: (diffId: string) => void;
}

export const DiffApprovalModal: React.FC<DiffApprovalModalProps> = ({
  diffs,
  isOpen,
  onClose,
  onApprove,
}) => {
  if (!isOpen) return null;

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
      zIndex: 200,
      padding: '20px',
    }}>
      <div style={{
        background: '#12131b',
        border: '1px solid #282a3b',
        borderRadius: '10px',
        width: '100%',
        maxWidth: '750px',
        maxHeight: '85vh',
        display: 'flex',
        flexDirection: 'column',
        boxShadow: '0 20px 40px rgba(0,0,0,0.8)',
      }}>
        {/* Header */}
        <div style={{
          padding: '18px 24px',
          borderBottom: '1px solid #202230',
          display: 'flex',
          justifyContent: 'space-between',
          alignItems: 'center',
        }}>
          <div>
            <h3 style={{ margin: 0, fontSize: '18px', color: '#fff' }}>Pending Config Diffs ({diffs.length})</h3>
            <div style={{ fontSize: '12px', color: '#888', marginTop: '4px' }}>
              Service owner review & approval workflow before deployment
            </div>
          </div>
          <button
            onClick={onClose}
            style={{ background: 'transparent', border: 'none', color: '#aaa', fontSize: '20px', cursor: 'pointer' }}
          >
            ✕
          </button>
        </div>

        {/* Content */}
        <div style={{ flex: 1, overflowY: 'auto', padding: '20px' }}>
          {diffs.length === 0 ? (
            <div style={{ textAlign: 'center', padding: '40px 20px', color: '#686b80' }}>
              <div style={{ fontSize: '32px', marginBottom: '8px' }}>✓</div>
              <div style={{ fontSize: '15px' }}>No pending configuration diffs</div>
              <div style={{ fontSize: '12px', marginTop: '4px' }}>All server and tool definitions are synchronized with production.</div>
            </div>
          ) : (
            diffs.map((d) => (
              <div
                key={d.diffId}
                style={{
                  background: '#181a24',
                  border: '1px solid #2b2e40',
                  borderRadius: '8px',
                  padding: '16px',
                  marginBottom: '16px',
                }}
              >
                <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'flex-start' }}>
                  <div>
                    <h4 style={{ margin: 0, fontSize: '15px', color: '#fff' }}>
                      {d.serverName} / <code>{d.toolName}</code>
                    </h4>
                    <div style={{ fontSize: '12px', color: '#888', marginTop: '3px' }}>
                      Version: v{d.prevVersion} → <strong>v{d.proposedVersion}</strong> · Author: <span style={{ color: '#276ef1' }}>{d.proposedBy}</span>
                    </div>
                  </div>
                  <button
                    onClick={() => onApprove(d.diffId)}
                    style={{
                      background: '#00e575',
                      color: '#000',
                      border: 'none',
                      padding: '6px 14px',
                      borderRadius: '6px',
                      fontWeight: 700,
                      fontSize: '12px',
                      cursor: 'pointer',
                    }}
                  >
                    Approve & Deploy
                  </button>
                </div>

                <div style={{ marginTop: '12px', background: '#0e0f14', padding: '10px 12px', borderRadius: '6px' }}>
                  <div style={{ fontSize: '11px', fontWeight: 600, color: '#ff9900', textTransform: 'uppercase' }}>
                    Changes Detected:
                  </div>
                  <ul style={{ margin: '6px 0 0 16px', padding: 0, fontSize: '13px', color: '#ccc' }}>
                    {d.changes.map((c, i) => (
                      <li key={i}>{c}</li>
                    ))}
                  </ul>
                </div>

                <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: '10px', marginTop: '12px' }}>
                  <div>
                    <div style={{ fontSize: '11px', color: '#ff4d4f', fontWeight: 600, marginBottom: '4px' }}>BEFORE (v{d.prevVersion})</div>
                    <pre style={{ margin: 0, background: '#0a0a0f', padding: '10px', borderRadius: '4px', fontSize: '11px', color: '#aaa', maxHeight: '120px', overflowY: 'auto' }}>
                      {JSON.stringify(d.beforeState, null, 2)}
                    </pre>
                  </div>
                  <div>
                    <div style={{ fontSize: '11px', color: '#00e575', fontWeight: 600, marginBottom: '4px' }}>AFTER (v{d.proposedVersion})</div>
                    <pre style={{ margin: 0, background: '#0a0a0f', padding: '10px', borderRadius: '4px', fontSize: '11px', color: '#73e6a0', maxHeight: '120px', overflowY: 'auto' }}>
                      {JSON.stringify(d.afterState, null, 2)}
                    </pre>
                  </div>
                </div>
              </div>
            ))
          )}
        </div>
      </div>
    </div>
  );
};

