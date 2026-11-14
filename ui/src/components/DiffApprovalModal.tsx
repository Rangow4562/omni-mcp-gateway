import React from 'react';
import { ConfigDiffItem } from '../types';

interface DiffApprovalModalProps {
  diffs: ConfigDiffItem[];
  isOpen: boolean;
  onClose: () => void;
  onApprove: (diffId: string) => void;
}

interface SchemaFieldRow {
  field: string;
  beforeVal: string;
  afterVal: string;
  status: 'added' | 'removed' | 'changed' | 'unchanged';
}

function extractSchemaFields(state: unknown): Record<string, string> {
  if (!state || typeof state !== 'object') return {};
  const obj = state as Record<string, unknown>;
  const inputSchema =
    obj.inputSchema && typeof obj.inputSchema === 'object'
      ? (obj.inputSchema as Record<string, unknown>)
      : obj.input_schema && typeof obj.input_schema === 'object'
        ? (obj.input_schema as Record<string, unknown>)
        : null;
  const props =
    inputSchema && inputSchema.properties && typeof inputSchema.properties === 'object'
      ? (inputSchema.properties as Record<string, unknown>)
      : obj;

  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(props)) {
    if (v && typeof v === 'object') {
      const vo = v as Record<string, unknown>;
      out[k] = typeof vo.type === 'string' ? vo.type : JSON.stringify(v);
    } else {
      out[k] = String(v);
    }
  }
  return out;
}

function computeSideBySideRows(beforeState: unknown, afterState: unknown): SchemaFieldRow[] {
  const beforeFields = extractSchemaFields(beforeState);
  const afterFields = extractSchemaFields(afterState);
  const keys = Array.from(new Set([...Object.keys(beforeFields), ...Object.keys(afterFields)])).sort();
  return keys.map((field) => {
    const b = beforeFields[field];
    const a = afterFields[field];
    if (b === undefined) {
      return { field, beforeVal: '—', afterVal: a, status: 'added' };
    }
    if (a === undefined) {
      return { field, beforeVal: b, afterVal: '—', status: 'removed' };
    }
    if (b !== a) {
      return { field, beforeVal: b, afterVal: a, status: 'changed' };
    }
    return { field, beforeVal: b, afterVal: a, status: 'unchanged' };
  });
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
                    <div style={{ fontSize: '12px', color: '#888', marginTop: '3px', display: 'flex', alignItems: 'center', gap: '8px', flexWrap: 'wrap' }}>
                      <span>Version: v{d.prevVersion} → <strong>v{d.proposedVersion}</strong> · Author: <span style={{ color: '#276ef1' }}>{d.proposedBy}</span></span>
                      {d.origin && (
                        <span style={{
                          fontSize: '10px',
                          fontWeight: 700,
                          padding: '1px 6px',
                          borderRadius: '4px',
                          background: d.origin === 'external_sync' ? 'rgba(255, 153, 0, 0.15)' : 'rgba(39, 110, 241, 0.15)',
                          color: d.origin === 'external_sync' ? '#ff9900' : '#8ab4f8',
                        }}>
                          {d.origin === 'external_sync' ? 'ExternalSync' : d.origin === 'human' ? 'Human' : d.origin}
                        </span>
                      )}
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

                <div style={{ marginTop: '12px', background: '#0e0f14', padding: '10px 12px', borderRadius: '6px' }}>
                  <div style={{ fontSize: '11px', fontWeight: 600, color: '#8ab4f8', textTransform: 'uppercase', marginBottom: '8px' }}>
                    Side-by-Side Schema Comparison (Current vs Proposed)
                  </div>
                  <div style={{ display: 'grid', gridTemplateColumns: '140px 1fr 1fr 80px', gap: '6px', fontSize: '11px', color: '#888', borderBottom: '1px solid #222536', paddingBottom: '4px', marginBottom: '4px' }}>
                    <span>Field</span>
                    <span>Current (v{d.prevVersion})</span>
                    <span>Proposed (v{d.proposedVersion})</span>
                    <span>Status</span>
                  </div>
                  {computeSideBySideRows(d.beforeState, d.afterState).map((row) => (
                    <div
                      key={row.field}
                      style={{
                        display: 'grid',
                        gridTemplateColumns: '140px 1fr 1fr 80px',
                        gap: '6px',
                        fontSize: '12px',
                        padding: '4px 0',
                        borderBottom: '1px solid #171924',
                        color: row.status === 'unchanged' ? '#999' : '#fff',
                      }}
                    >
                      <code style={{ color: '#e2e4f0' }}>{row.field}</code>
                      <span style={{ color: row.status === 'removed' || row.status === 'changed' ? '#ff7875' : '#aaa', wordBreak: 'break-word' }}>{row.beforeVal}</span>
                      <span style={{ color: row.status === 'added' || row.status === 'changed' ? '#73e6a0' : '#aaa', wordBreak: 'break-word' }}>{row.afterVal}</span>
                      <span style={{
                        fontSize: '10px',
                        fontWeight: 700,
                        textTransform: 'uppercase',
                        color: row.status === 'added' ? '#00e575' : row.status === 'removed' ? '#ff4d4f' : row.status === 'changed' ? '#ff9900' : '#686b80',
                      }}>
                        {row.status}
                      </span>
                    </div>
                  ))}
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

