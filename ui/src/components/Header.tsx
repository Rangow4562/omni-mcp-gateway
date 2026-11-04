import React, { useState, useEffect } from 'react';
import { PlatformStats } from '../types';
import { getToken, setToken, DEV_ROLE_TOKENS } from '../api';

interface HeaderProps {
  stats: PlatformStats | null;
  selectedActor: string;
  onActorChange: (actor: string) => void;
  onTriggerCrawler: () => void;
  onOpenOmni: () => void;
  onOpenDiffs: () => void;
  onOpenRegisterExternal?: () => void;
  pendingDiffCount: number;
  onTokenChanged?: () => void;
}

export const Header: React.FC<HeaderProps> = ({
  stats,
  selectedActor,
  onActorChange,
  onTriggerCrawler,
  onOpenOmni,
  onOpenDiffs,
  onOpenRegisterExternal,
  pendingDiffCount,
  onTokenChanged,
}) => {
  const [currentToken, setCurrentToken] = useState<string>(getToken());
  const [isEditingToken, setIsEditingToken] = useState(false);
  const [customTokenInput, setCustomTokenInput] = useState('');

  useEffect(() => {
    setCurrentToken(getToken());
  }, []);

  const handleRoleSelect = (roleToken: string) => {
    setToken(roleToken);
    setCurrentToken(roleToken);
    setIsEditingToken(false);
    onTokenChanged?.();
  };

  const handleSaveCustomToken = () => {
    if (customTokenInput.trim()) {
      setToken(customTokenInput.trim());
      setCurrentToken(customTokenInput.trim());
      setIsEditingToken(false);
      onTokenChanged?.();
    }
  };

  const getRoleLabel = (token: string) => {
    if (token.includes('admin')) return '👑 Admin';
    if (token.includes('approver')) return '🛡️ Approver';
    if (token.includes('operator')) return '🔧 Operator';
    if (token.includes('agent')) return '🤖 Agent';
    return '🔑 Custom Token';
  };

  return (
    <header style={{
      borderBottom: '1px solid #1f2029',
      backgroundColor: '#0e0f14',
      padding: '12px 24px',
      display: 'flex',
      alignItems: 'center',
      justifyContent: 'space-between',
      position: 'sticky',
      top: 0,
      zIndex: 100,
    }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: '16px' }}>
        <div style={{
          backgroundColor: '#276ef1',
          color: '#ffffff',
          fontWeight: 900,
          fontSize: '15px',
          padding: '4px 10px',
          borderRadius: '4px',
          letterSpacing: '-0.3px'
        }}>
          OMNI
        </div>
        <div>
          <div style={{ display: 'flex', alignItems: 'center', gap: '10px' }}>
            <span style={{ fontSize: '16px', fontWeight: 700, letterSpacing: '-0.3px' }}>Omni MCP Gateway</span>
            <span style={{
              fontSize: '11px',
              backgroundColor: '#1b382b',
              color: '#00e575',
              padding: '2px 7px',
              borderRadius: '12px',
              fontWeight: 600,
            }}>
              v5.5.0 FLEET READY
            </span>
          </div>
          <div style={{ fontSize: '11px', color: '#7c7f8e' }}>
            Control Plane & External MCP Fleet Gateway
          </div>
        </div>
      </div>

      <div style={{ display: 'flex', alignItems: 'center', gap: '12px' }}>
        {stats && (
          <div style={{ display: 'flex', gap: '14px', marginRight: '8px', fontSize: '12px' }}>
            <div>
              <span style={{ color: '#888' }}>Servers: </span>
              <strong style={{ color: '#fff' }}>{stats.total_servers}</strong>
            </div>
            <div>
              <span style={{ color: '#888' }}>Active Tools: </span>
              <strong style={{ color: '#00e575' }}>{stats.enabled_tools}</strong> / {stats.total_tools}
            </div>
          </div>
        )}

        {/* Dev Token / RBAC Role Switcher */}
        <div style={{ display: 'flex', alignItems: 'center', gap: '6px', background: '#161720', padding: '3px 8px', borderRadius: '6px', border: '1px solid #282a3b' }}>
          <span style={{ fontSize: '11px', color: '#888' }}>Auth Role:</span>
          {!isEditingToken ? (
            <div style={{ display: 'flex', alignItems: 'center', gap: '6px' }}>
              <select
                value={currentToken}
                onChange={(e) => {
                  if (e.target.value === 'custom') {
                    setIsEditingToken(true);
                    setCustomTokenInput(currentToken);
                  } else {
                    handleRoleSelect(e.target.value);
                  }
                }}
                style={{
                  background: '#222330',
                  border: '1px solid #333446',
                  color: '#00e575',
                  fontSize: '11px',
                  fontWeight: 600,
                  padding: '3px 6px',
                  borderRadius: '4px',
                  cursor: 'pointer',
                }}
              >
                <option value={DEV_ROLE_TOKENS.admin}>👑 Admin (admin_dev_token)</option>
                <option value={DEV_ROLE_TOKENS.approver}>🛡️ Approver (approver_dev_token)</option>
                <option value={DEV_ROLE_TOKENS.operator}>🔧 Operator (operator_dev_token)</option>
                <option value={DEV_ROLE_TOKENS.agent}>🤖 Agent (agent_dev_token)</option>
                <option value="custom">✏️ Custom Bearer...</option>
              </select>
            </div>
          ) : (
            <div style={{ display: 'flex', alignItems: 'center', gap: '4px' }}>
              <input
                type="text"
                placeholder="Bearer token"
                value={customTokenInput}
                onChange={(e) => setCustomTokenInput(e.target.value)}
                style={{
                  background: '#222330',
                  border: '1px solid #333446',
                  color: '#fff',
                  fontSize: '11px',
                  padding: '3px 6px',
                  borderRadius: '4px',
                  width: '120px',
                }}
              />
              <button
                onClick={handleSaveCustomToken}
                style={{
                  background: '#276ef1',
                  color: '#fff',
                  border: 'none',
                  fontSize: '11px',
                  padding: '3px 6px',
                  borderRadius: '4px',
                  cursor: 'pointer',
                }}
              >
                Set
              </button>
              <button
                onClick={() => setIsEditingToken(false)}
                style={{
                  background: '#333446',
                  color: '#fff',
                  border: 'none',
                  fontSize: '11px',
                  padding: '3px 6px',
                  borderRadius: '4px',
                  cursor: 'pointer',
                }}
              >
                ✕
              </button>
            </div>
          )}
        </div>

        <div style={{ display: 'flex', alignItems: 'center', gap: '6px', background: '#161720', padding: '3px 8px', borderRadius: '6px' }}>
          <span style={{ fontSize: '11px', color: '#888' }}>Caller:</span>
          <select
            value={selectedActor}
            onChange={(e) => onActorChange(e.target.value)}
            style={{
              background: '#222330',
              border: '1px solid #333446',
              color: '#fff',
              fontSize: '11px',
              padding: '3px 6px',
              borderRadius: '4px',
              cursor: 'pointer',
            }}
          >
            <option value="agent">🤖 AI Agent</option>
            <option value="human">👤 Human Engineer</option>
            <option value="service">⚙️ Service (Muttley)</option>
          </select>
        </div>

        {onOpenRegisterExternal && (
          <button
            onClick={onOpenRegisterExternal}
            style={{
              backgroundColor: '#00e575',
              color: '#000000',
              border: 'none',
              padding: '6px 12px',
              borderRadius: '6px',
              fontWeight: 700,
              fontSize: '12px',
              cursor: 'pointer',
              display: 'flex',
              alignItems: 'center',
              gap: '4px',
            }}
          >
            ➕ Register External MCP
          </button>
        )}

        <button
          onClick={onOpenOmni}
          style={{
            backgroundColor: '#276ef1',
            color: '#fff',
            border: 'none',
            padding: '6px 12px',
            borderRadius: '6px',
            fontWeight: 600,
            fontSize: '12px',
            cursor: 'pointer',
          }}
        >
          🔍 Omni Search
        </button>

        <button
          onClick={onOpenDiffs}
          style={{
            backgroundColor: pendingDiffCount > 0 ? '#ff9900' : '#222330',
            color: pendingDiffCount > 0 ? '#000' : '#fff',
            border: '1px solid #333446',
            padding: '6px 10px',
            borderRadius: '6px',
            fontWeight: 600,
            fontSize: '12px',
            cursor: 'pointer',
          }}
        >
          Diffs ({pendingDiffCount})
        </button>

        <button
          onClick={onTriggerCrawler}
          style={{
            backgroundColor: '#1b1c26',
            color: '#c5c7d5',
            border: '1px solid #2e3042',
            padding: '6px 10px',
            borderRadius: '6px',
            fontSize: '12px',
            cursor: 'pointer',
          }}
        >
          ⚡ AutoCrawler
        </button>
      </div>
    </header>
  );
};
