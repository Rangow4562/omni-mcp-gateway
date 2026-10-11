import React, { useState } from 'react';
import { ServerSummary } from '../types';

interface RegistryCatalogProps {
  servers: ServerSummary[];
  selectedServer: string | null;
  onSelectServer: (serverName: string) => void;
  onToggleServerStatus: (serverName: string, currentStatus: string) => void;
}

export const RegistryCatalog: React.FC<RegistryCatalogProps> = ({
  servers,
  selectedServer,
  onSelectServer,
  onToggleServerStatus,
}) => {
  const [searchTerm, setSearchTerm] = useState('');
  const [protocolFilter, setProtocolFilter] = useState('ALL');

  const filtered = servers.filter((s) => {
    const matchesSearch =
      s.name.toLowerCase().includes(searchTerm.toLowerCase()) ||
      s.displayName.toLowerCase().includes(searchTerm.toLowerCase()) ||
      s.description.toLowerCase().includes(searchTerm.toLowerCase()) ||
      s.owningTeam.toLowerCase().includes(searchTerm.toLowerCase());

    const matchesProtocol = protocolFilter === 'ALL' || s.protocol === protocolFilter;
    return matchesSearch && matchesProtocol;
  });

  const getProtocolColor = (proto: string) => {
    switch (proto) {
      case 'gRPC':
        return '#00b4d8';
      case 'TChannel':
        return '#9d4edd';
      case 'Native_MCP':
        return '#06d6a0';
      case 'ThirdParty':
        return '#ffb703';
      default:
        return '#48cae4';
    }
  };

  return (
    <div style={{ flex: '1', display: 'flex', flexDirection: 'column', height: '100%', borderRight: '1px solid #1f2029' }}>
      {/* Search and filters bar */}
      <div style={{ padding: '16px 20px', borderBottom: '1px solid #1f2029', background: '#0e0f14' }}>
        <div style={{ display: 'flex', gap: '10px', marginBottom: '12px' }}>
          <input
            type="text"
            placeholder="Search servers, descriptions, teams..."
            value={searchTerm}
            onChange={(e) => setSearchTerm(e.target.value)}
            style={{
              flex: 1,
              background: '#161722',
              border: '1px solid #282a3b',
              color: '#fff',
              padding: '9px 14px',
              borderRadius: '6px',
              fontSize: '13px',
              outline: 'none',
            }}
          />
          <select
            value={protocolFilter}
            onChange={(e) => setProtocolFilter(e.target.value)}
            style={{
              background: '#161722',
              border: '1px solid #282a3b',
              color: '#fff',
              padding: '9px 12px',
              borderRadius: '6px',
              fontSize: '13px',
              cursor: 'pointer',
            }}
          >
            <option value="ALL">All Protocols</option>
            <option value="gRPC">gRPC</option>
            <option value="TChannel">TChannel</option>
            <option value="HTTP">HTTP</option>
            <option value="Native_MCP">Native MCP</option>
            <option value="ThirdParty">3P Services</option>
          </select>
        </div>
        <div style={{ fontSize: '12px', color: '#6d7082', display: 'flex', justifyContent: 'space-between' }}>
          <span>Figure 3: MCP Registry Catalog</span>
          <span>{filtered.length} servers matching</span>
        </div>
      </div>

      {/* Servers list */}
      <div style={{ flex: 1, overflowY: 'auto', padding: '12px' }}>
        {filtered.map((s) => {
          const isSelected = selectedServer === s.name;
          const isEnabled = s.status === 'ENABLED';

          return (
            <div
              key={s.name}
              onClick={() => onSelectServer(s.name)}
              style={{
                backgroundColor: isSelected ? '#1b1d28' : '#12131b',
                border: isSelected ? '1px solid #276ef1' : '1px solid #1c1e2b',
                borderRadius: '8px',
                padding: '14px 16px',
                marginBottom: '10px',
                cursor: 'pointer',
                transition: 'border 0.15s, background 0.15s',
              }}
            >
              <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'flex-start', marginBottom: '6px' }}>
                <div>
                  <h4 style={{ margin: 0, fontSize: '15px', fontWeight: 600, color: '#fff' }}>
                    {s.displayName}
                  </h4>
                  <div style={{ fontSize: '12px', color: '#7e8299', marginTop: '2px' }}>
                    <code>{s.name}</code> · <span style={{ color: '#a0a3bd' }}>{s.owningTeam}</span>
                  </div>
                </div>

                <div style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
                  <span
                    style={{
                      fontSize: '11px',
                      padding: '2px 8px',
                      borderRadius: '4px',
                      fontWeight: 600,
                      backgroundColor: `${getProtocolColor(s.protocol)}22`,
                      color: getProtocolColor(s.protocol),
                      border: `1px solid ${getProtocolColor(s.protocol)}44`,
                    }}
                  >
                    {s.protocol}
                  </span>

                  <button
                    onClick={(e) => {
                      e.stopPropagation();
                      onToggleServerStatus(s.name, s.status);
                    }}
                    style={{
                      fontSize: '11px',
                      padding: '3px 8px',
                      borderRadius: '4px',
                      border: 'none',
                      fontWeight: 700,
                      cursor: 'pointer',
                      backgroundColor: isEnabled ? '#00e575' : '#2b2d3d',
                      color: isEnabled ? '#000000' : '#8e92a8',
                    }}
                  >
                    {isEnabled ? 'ENABLED' : 'DISABLED'}
                  </button>
                </div>
              </div>

              <p style={{ margin: '8px 0', fontSize: '13px', color: '#b0b3c6', lineHeight: '1.4' }}>
                {s.description}
              </p>

              <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', fontSize: '12px', color: '#686b80', marginTop: '10px', paddingTop: '8px', borderTop: '1px solid #1a1c27' }}>
                <span>v{s.version}</span>
                <span>
                  Tools: <strong style={{ color: s.enabledToolsCount > 0 ? '#00e575' : '#686b80' }}>{s.enabledToolsCount}</strong> / {s.toolsCount} active
                </span>
                <span style={{ color: '#276ef1', fontWeight: 500 }}>Configure →</span>
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
};

