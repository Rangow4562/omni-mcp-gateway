import React from 'react';
import { PlatformStats } from '../types';

interface HeaderProps {
  stats: PlatformStats | null;
  selectedActor: string;
  onActorChange: (actor: string) => void;
  onTriggerCrawler: () => void;
  onOpenOmni: () => void;
  onOpenDiffs: () => void;
  pendingDiffCount: number;
}

export const Header: React.FC<HeaderProps> = ({
  stats,
  selectedActor,
  onActorChange,
  onTriggerCrawler,
  onOpenOmni,
  onOpenDiffs,
  pendingDiffCount,
}) => {
  return (
    <header style={{
      borderBottom: '1px solid #1f2029',
      backgroundColor: '#0e0f14',
      padding: '14px 28px',
      display: 'flex',
      alignItems: 'center',
      justifyContent: 'space-between',
      position: 'sticky',
      top: 0,
      zIndex: 100,
    }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: '18px' }}>
        <div style={{
          backgroundColor: '#276ef1',
          color: '#ffffff',
          fontWeight: 900,
          fontSize: '16px',
          padding: '4px 10px',
          borderRadius: '4px',
          letterSpacing: '-0.3px'
        }}>
          OMNI
        </div>
        <div>
          <div style={{ display: 'flex', alignItems: 'center', gap: '10px' }}>
            <span style={{ fontSize: '17px', fontWeight: 700, letterSpacing: '-0.3px' }}>Omni MCP Gateway</span>
            <span style={{
              fontSize: '11px',
              backgroundColor: '#1b382b',
              color: '#00e575',
              padding: '2px 7px',
              borderRadius: '12px',
              fontWeight: 600,
            }}>
              800+ SERVERS READY
            </span>
          </div>
          <div style={{ fontSize: '12px', color: '#7c7f8e' }}>
            Control Plane & Proxy Layer (Figure 3 & 4 Implementation)
          </div>
        </div>
      </div>

      <div style={{ display: 'flex', alignItems: 'center', gap: '14px' }}>
        {stats && (
          <div style={{ display: 'flex', gap: '16px', marginRight: '12px', fontSize: '13px' }}>
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

        <div style={{ display: 'flex', alignItems: 'center', gap: '6px', background: '#161720', padding: '4px 8px', borderRadius: '6px' }}>
          <span style={{ fontSize: '12px', color: '#888' }}>Caller:</span>
          <select
            value={selectedActor}
            onChange={(e) => onActorChange(e.target.value)}
            style={{
              background: '#222330',
              border: '1px solid #333446',
              color: '#fff',
              fontSize: '12px',
              padding: '4px 8px',
              borderRadius: '4px',
              cursor: 'pointer',
            }}
          >
            <option value="agent">🤖 AI Agent</option>
            <option value="human">👤 Human Engineer</option>
            <option value="service">⚙️ Service (Muttley)</option>
          </select>
        </div>

        <button
          onClick={onOpenOmni}
          style={{
            backgroundColor: '#276ef1',
            color: '#fff',
            border: 'none',
            padding: '7px 14px',
            borderRadius: '6px',
            fontWeight: 600,
            fontSize: '13px',
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
            padding: '7px 12px',
            borderRadius: '6px',
            fontWeight: 600,
            fontSize: '13px',
            cursor: 'pointer',
          }}
        >
          Diff Approvals ({pendingDiffCount})
        </button>

        <button
          onClick={onTriggerCrawler}
          style={{
            backgroundColor: '#1b1c26',
            color: '#c5c7d5',
            border: '1px solid #2e3042',
            padding: '7px 12px',
            borderRadius: '6px',
            fontSize: '13px',
            cursor: 'pointer',
          }}
        >
          ⚡ Run AutoCrawler
        </button>
      </div>
    </header>
  );
};

