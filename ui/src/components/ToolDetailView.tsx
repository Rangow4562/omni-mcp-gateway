import React, { useState } from 'react';
import { ServerDetail, ToolItem } from '../types';

interface ToolDetailViewProps {
  server: ServerDetail | null;
  onToggleToolStatus: (serverName: string, toolName: string, currentStatus: string) => void;
  onProposeUpdate: (serverName: string, toolName: string, newDesc: string) => void;
  onRollback: (serverName: string, toolName: string, targetVersion: number) => void;
  onOpenTestConsole: (serverName: string, toolName: string, tool: ToolItem) => void;
}

export const ToolDetailView: React.FC<ToolDetailViewProps> = ({
  server,
  onToggleToolStatus,
  onProposeUpdate,
  onRollback,
  onOpenTestConsole,
}) => {
  const [selectedToolName, setSelectedToolName] = useState<string | null>(null);
  const [editingDesc, setEditingDesc] = useState<string>('');
  const [isEditing, setIsEditing] = useState<boolean>(false);
  const [targetRollbackVersion, setTargetRollbackVersion] = useState<number>(1);

  if (!server) {
    return (
      <div style={{ flex: '1.2', display: 'flex', alignItems: 'center', justifyContent: 'center', color: '#686b80', flexDirection: 'column' }}>
        <div style={{ fontSize: '40px', marginBottom: '12px' }}>⚡</div>
        <div style={{ fontSize: '16px', fontWeight: 500 }}>Select a Server to Inspect & Configure Tools</div>
        <div style={{ fontSize: '13px', marginTop: '6px' }}>Figure 4: MCP Tool Configuration & Schema UI</div>
      </div>
    );
  }

  const toolsArray = Object.values(server.tools);
  const activeTool = selectedToolName ? server.tools[selectedToolName] : toolsArray[0];

  const handleStartEdit = () => {
    if (activeTool) {
      setEditingDesc(activeTool.description);
      setIsEditing(true);
    }
  };

  const handleSaveDiff = () => {
    if (activeTool) {
      onProposeUpdate(server.name, activeTool.name, editingDesc);
      setIsEditing(false);
    }
  };

  return (
    <div style={{ flex: '1.4', display: 'flex', flexDirection: 'column', height: '100%', background: '#0b0b0f' }}>
      {/* Server Header Bar */}
      <div style={{ padding: '16px 24px', borderBottom: '1px solid #1f2029', background: '#0e0f14' }}>
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
          <div>
            <h3 style={{ margin: 0, fontSize: '18px', color: '#fff' }}>{server.displayName}</h3>
            <div style={{ fontSize: '13px', color: '#888', marginTop: '3px' }}>
              Virtual Endpoint: <code>/{server.name}/mcp</code> · Protocol: <strong>{server.protocol}</strong>
            </div>
          </div>
          <div style={{ fontSize: '12px', color: '#686b80' }}>
            Figure 4: MCP Tool Management
          </div>
        </div>

        {/* Tool selector pills */}
        <div style={{ display: 'flex', gap: '8px', marginTop: '14px', flexWrap: 'wrap' }}>
          {toolsArray.map((t) => {
            const isSelected = activeTool?.name === t.name;
            const isEnabled = t.status === 'ENABLED';

            return (
              <button
                key={t.name}
                onClick={() => {
                  setSelectedToolName(t.name);
                  setIsEditing(false);
                }}
                style={{
                  background: isSelected ? '#276ef1' : '#191b26',
                  color: isSelected ? '#fff' : '#b0b3c6',
                  border: isSelected ? '1px solid #276ef1' : '1px solid #262838',
                  padding: '6px 12px',
                  borderRadius: '6px',
                  fontSize: '12px',
                  fontWeight: 600,
                  cursor: 'pointer',
                  display: 'flex',
                  alignItems: 'center',
                  gap: '6px',
                }}
              >
                <span>{t.name}</span>
                <span
                  style={{
                    width: '7px',
                    height: '7px',
                    borderRadius: '50%',
                    backgroundColor: isEnabled ? '#00e575' : '#888',
                  }}
                />
              </button>
            );
          })}
        </div>
      </div>

      {/* Tool Detail Canvas */}
      {activeTool ? (
        <div style={{ flex: 1, overflowY: 'auto', padding: '24px' }}>
          {/* Top Banner */}
          <div style={{
            background: '#13141e',
            border: '1px solid #212333',
            borderRadius: '8px',
            padding: '16px 20px',
            marginBottom: '20px',
            display: 'flex',
            justifyContent: 'space-between',
            alignItems: 'center',
          }}>
            <div>
              <div style={{ display: 'flex', alignItems: 'center', gap: '10px' }}>
                <h4 style={{ margin: 0, fontSize: '18px', color: '#fff' }}>{activeTool.name}</h4>
                <span style={{ fontSize: '12px', background: '#202232', color: '#8d90a6', padding: '2px 8px', borderRadius: '4px' }}>
                  v{activeTool.version}
                </span>
                <span style={{
                  fontSize: '11px',
                  padding: '2px 8px',
                  borderRadius: '4px',
                  fontWeight: 700,
                  backgroundColor: activeTool.status === 'ENABLED' ? '#00e57522' : '#ff4d4f22',
                  color: activeTool.status === 'ENABLED' ? '#00e575' : '#ff4d4f',
                  border: `1px solid ${activeTool.status === 'ENABLED' ? '#00e57544' : '#ff4d4f44'}`
                }}>
                  {activeTool.status}
                </span>
              </div>
              <div style={{ fontSize: '12px', color: '#6d7085', marginTop: '4px' }}>
                Downstream Target: <code>{activeTool.downstreamEndpoint || 'muttley://internal'}</code>
              </div>
            </div>

            <div style={{ display: 'flex', gap: '10px' }}>
              <button
                onClick={() => onToggleToolStatus(server.name, activeTool.name, activeTool.status)}
                style={{
                  background: activeTool.status === 'ENABLED' ? '#ff4d4f' : '#00e575',
                  color: activeTool.status === 'ENABLED' ? '#fff' : '#000',
                  border: 'none',
                  padding: '8px 14px',
                  borderRadius: '6px',
                  fontWeight: 700,
                  fontSize: '13px',
                  cursor: 'pointer',
                }}
              >
                {activeTool.status === 'ENABLED' ? 'Disable Tool' : 'Enable Tool'}
              </button>

              <button
                onClick={() => onOpenTestConsole(server.name, activeTool.name, activeTool)}
                style={{
                  background: '#276ef1',
                  color: '#fff',
                  border: 'none',
                  padding: '8px 14px',
                  borderRadius: '6px',
                  fontWeight: 600,
                  fontSize: '13px',
                  cursor: 'pointer',
                }}
              >
                ▶ Test Invoke
              </button>
            </div>
          </div>

          {/* Description Section with Diff Propose */}
          <div style={{
            background: '#13141e',
            border: '1px solid #212333',
            borderRadius: '8px',
            padding: '18px 20px',
            marginBottom: '20px',
          }}>
            <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: '10px' }}>
              <h5 style={{ margin: 0, fontSize: '14px', color: '#fff', textTransform: 'uppercase', letterSpacing: '0.5px' }}>
                Agent Tool Description
              </h5>
              {!isEditing ? (
                <button
                  onClick={handleStartEdit}
                  style={{
                    background: '#1f2130',
                    color: '#276ef1',
                    border: '1px solid #30334a',
                    padding: '4px 10px',
                    borderRadius: '4px',
                    fontSize: '12px',
                    cursor: 'pointer',
                  }}
                >
                  Edit & Propose Diff
                </button>
              ) : (
                <div style={{ display: 'flex', gap: '6px' }}>
                  <button
                    onClick={handleSaveDiff}
                    style={{
                      background: '#ff9900',
                      color: '#000',
                      border: 'none',
                      padding: '4px 10px',
                      borderRadius: '4px',
                      fontSize: '12px',
                      fontWeight: 600,
                      cursor: 'pointer',
                    }}
                  >
                    Submit Diff
                  </button>
                  <button
                    onClick={() => setIsEditing(false)}
                    style={{
                      background: '#2b2d3d',
                      color: '#fff',
                      border: 'none',
                      padding: '4px 10px',
                      borderRadius: '4px',
                      fontSize: '12px',
                      cursor: 'pointer',
                    }}
                  >
                    Cancel
                  </button>
                </div>
              )}
            </div>

            {isEditing ? (
              <textarea
                value={editingDesc}
                onChange={(e) => setEditingDesc(e.target.value)}
                style={{
                  width: '100%',
                  height: '90px',
                  background: '#0a0a0f',
                  border: '1px solid #ff9900',
                  color: '#fff',
                  borderRadius: '6px',
                  padding: '10px',
                  fontSize: '13px',
                  boxSizing: 'border-box',
                }}
              />
            ) : (
              <p style={{ margin: 0, fontSize: '14px', color: '#d0d3e5', lineHeight: '1.5' }}>
                {activeTool.description}
              </p>
            )}
          </div>

          {/* JSON Schema Section */}
          <div style={{
            background: '#13141e',
            border: '1px solid #212333',
            borderRadius: '8px',
            padding: '18px 20px',
            marginBottom: '20px',
          }}>
            <h5 style={{ margin: '0 0 12px 0', fontSize: '14px', color: '#fff', textTransform: 'uppercase', letterSpacing: '0.5px' }}>
              Input Parameter Schema (JSON-RPC 2.0)
            </h5>
            <pre style={{
              margin: 0,
              background: '#090a0f',
              border: '1px solid #1a1c27',
              borderRadius: '6px',
              padding: '14px',
              fontSize: '13px',
              color: '#00e575',
              overflowX: 'auto',
            }}>
              {JSON.stringify(activeTool.inputSchema, null, 2)}
            </pre>
          </div>

          {/* Rollback Section */}
          <div style={{
            background: '#13141e',
            border: '1px solid #212333',
            borderRadius: '8px',
            padding: '16px 20px',
            display: 'flex',
            justifyContent: 'space-between',
            alignItems: 'center',
          }}>
            <div>
              <div style={{ fontSize: '14px', fontWeight: 600, color: '#fff' }}>Version Rollback</div>
              <div style={{ fontSize: '12px', color: '#777', marginTop: '2px' }}>
                Revert configuration changes to a previous approved state.
              </div>
            </div>
            <div style={{ display: 'flex', gap: '8px', alignItems: 'center' }}>
              <input
                type="number"
                min="1"
                max={Math.max(1, activeTool.version - 1)}
                value={targetRollbackVersion}
                onChange={(e) => setTargetRollbackVersion(parseInt(e.target.value) || 1)}
                style={{
                  width: '50px',
                  background: '#090a0f',
                  border: '1px solid #333',
                  color: '#fff',
                  padding: '6px',
                  borderRadius: '4px',
                  textAlign: 'center',
                }}
              />
              <button
                onClick={() => onRollback(server.name, activeTool.name, targetRollbackVersion)}
                style={{
                  background: '#2f3242',
                  color: '#fff',
                  border: '1px solid #41455a',
                  padding: '6px 12px',
                  borderRadius: '4px',
                  fontSize: '12px',
                  cursor: 'pointer',
                }}
              >
                Rollback to v{targetRollbackVersion}
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
};

