import React, { useState } from 'react';

interface OmniSearchModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSelectServer: (serverName: string) => void;
}

export const OmniSearchModal: React.FC<OmniSearchModalProps> = ({
  isOpen,
  onClose,
  onSelectServer,
}) => {
  if (!isOpen) return null;

  const [intentQuery, setIntentQuery] = useState('');
  const [searching, setSearching] = useState(false);
  const [results, setResults] = useState<any[]>([]);

  const handleSearch = async () => {
    if (!intentQuery.trim()) return;
    setSearching(true);
    try {
      const res = await fetch('http://localhost:8080/omni/mcp', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          jsonrpc: '2.0',
          id: Date.now(),
          method: 'tools/call',
          params: {
            name: 'discover_server',
            arguments: { query: intentQuery, limit: 6 },
          },
        }),
      });
      const data = await res.json();
      if (data.result && data.result.content && data.result.content[0]) {
        const parsed = JSON.parse(data.result.content[0].text);
        setResults(parsed.servers || []);
      }
    } catch (err) {
      console.error(err);
    } finally {
      setSearching(false);
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
      zIndex: 250,
      padding: '20px',
    }}>
      <div style={{
        background: '#12131b',
        border: '1px solid #282a3b',
        borderRadius: '10px',
        width: '100%',
        maxWidth: '680px',
        maxHeight: '80vh',
        display: 'flex',
        flexDirection: 'column',
      }}>
        <div style={{ padding: '16px 20px', borderBottom: '1px solid #202230', display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
          <div>
            <h3 style={{ margin: 0, fontSize: '17px', color: '#fff' }}>Omni MCP Runtime Discovery</h3>
            <div style={{ fontSize: '12px', color: '#888', marginTop: '3px' }}>
              Token-optimized incremental intent search (<code>discover_server</code>)
            </div>
          </div>
          <button onClick={onClose} style={{ background: 'transparent', border: 'none', color: '#aaa', fontSize: '20px', cursor: 'pointer' }}>✕</button>
        </div>

        <div style={{ padding: '20px' }}>
          <div style={{ display: 'flex', gap: '8px', marginBottom: '16px' }}>
            <input
              type="text"
              placeholder="e.g. Find services for driver coordinates, billing, or issue tracking..."
              value={intentQuery}
              onChange={(e) => setIntentQuery(e.target.value)}
              onKeyDown={(e) => e.key === 'Enter' && handleSearch()}
              style={{
                flex: 1,
                background: '#0a0a0f',
                border: '1px solid #282a3b',
                color: '#fff',
                padding: '10px 14px',
                borderRadius: '6px',
                fontSize: '13px',
              }}
            />
            <button
              onClick={handleSearch}
              disabled={searching}
              style={{
                background: '#276ef1',
                color: '#fff',
                border: 'none',
                padding: '0 18px',
                borderRadius: '6px',
                fontWeight: 600,
                cursor: 'pointer',
              }}
            >
              {searching ? 'Discovering...' : 'Search'}
            </button>
          </div>

          <div style={{ maxHeight: '350px', overflowY: 'auto' }}>
            {results.map((r) => (
              <div
                key={r.server_name}
                onClick={() => {
                  onSelectServer(r.server_name);
                  onClose();
                }}
                style={{
                  background: '#161823',
                  border: '1px solid #242636',
                  borderRadius: '6px',
                  padding: '12px 16px',
                  marginBottom: '10px',
                  cursor: 'pointer',
                  display: 'flex',
                  justifyContent: 'space-between',
                  alignItems: 'center',
                }}
              >
                <div>
                  <div style={{ fontSize: '14px', fontWeight: 600, color: '#fff' }}>{r.display_name}</div>
                  <div style={{ fontSize: '12px', color: '#888', marginTop: '2px' }}>{r.description}</div>
                  <div style={{ fontSize: '11px', color: '#555', marginTop: '4px' }}>
                    Protocol: <code>{r.protocol}</code> · {r.enabled_tools_count} / {r.total_tools_count} tools enabled
                  </div>
                </div>
                <span style={{ fontSize: '12px', color: '#276ef1', fontWeight: 600 }}>Select →</span>
              </div>
            ))}
          </div>
        </div>
      </div>
    </div>
  );
};

