import React, { useState } from 'react';
import { ImportConfigResponse } from '../types';
import { importMcpConfig } from '../api';

interface ImportConfigModalProps {
  isOpen: boolean;
  onClose: () => void;
  onImported: () => void;
}

const SAMPLE_MCP_JSON = `{
  "mcpServers": {
    "github-mcp": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": {
        "GITHUB_TOKEN": "ghp_example_plaintext_secret"
      }
    },
    "linear-hosted": {
      "url": "https://mcp.linear.app/mcp",
      "headers": {
        "Authorization": "Bearer example_secret_token"
      }
    }
  }
}`;

export const ImportConfigModal: React.FC<ImportConfigModalProps> = ({
  isOpen,
  onClose,
  onImported,
}) => {
  const [configText, setConfigText] = useState(SAMPLE_MCP_JSON);
  const [preview, setPreview] = useState<ImportConfigResponse | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [successMsg, setSuccessMsg] = useState<string | null>(null);

  if (!isOpen) return null;

  const handleFileUpload = (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0];
    if (!file) return;
    const reader = new FileReader();
    reader.onload = () => {
      if (typeof reader.result === 'string') {
        setConfigText(reader.result);
        setPreview(null);
      }
    };
    reader.readAsText(file);
  };

  const handlePreview = async () => {
    setBusy(true);
    setError(null);
    setSuccessMsg(null);
    try {
      const res = await importMcpConfig(configText, true);
      setPreview(res);
    } catch (e: any) {
      setError(e?.message || 'Failed to parse mcp.json');
    } finally {
      setBusy(false);
    }
  };

  const handleImport = async () => {
    setBusy(true);
    setError(null);
    setSuccessMsg(null);
    try {
      const res = await importMcpConfig(configText, false);
      setPreview(res);
      setSuccessMsg(`Imported ${res.imported_count} external MCP server(s) with secret references converted.`);
      onImported();
    } catch (e: any) {
      setError(e?.message || 'Import failed');
    } finally {
      setBusy(false);
    }
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
        data-testid="import-config-modal"
        style={{
          backgroundColor: '#12131b',
          border: '1px solid #2b2d3f',
          borderRadius: '10px',
          width: '780px',
          maxWidth: '94vw',
          maxHeight: '86vh',
          display: 'flex',
          flexDirection: 'column',
          overflow: 'hidden',
        }}
      >
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
              📥 Import mcp.json Fleet Wizard
            </h2>
            <div style={{ fontSize: '12px', color: '#8b8fa8', marginTop: '2px' }}>
              Paste or upload claude_desktop_config.json / .cursor/mcp.json — plaintext secrets auto-convert to {'{env:...}'}
            </div>
          </div>
          <button
            onClick={onClose}
            aria-label="Close Import Config Modal"
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

        <div style={{ padding: '16px 20px', overflowY: 'auto', flex: 1 }}>
          {error && (
            <div
              style={{
                backgroundColor: '#3d1616',
                border: '1px solid #7a2929',
                color: '#ff9999',
                padding: '10px 14px',
                borderRadius: '6px',
                fontSize: '12px',
                marginBottom: '12px',
              }}
            >
              ⚠️ {error}
            </div>
          )}

          {successMsg && (
            <div
              style={{
                backgroundColor: '#143325',
                border: '1px solid #00e57555',
                color: '#00e575',
                padding: '10px 14px',
                borderRadius: '6px',
                fontSize: '12px',
                marginBottom: '12px',
              }}
            >
              ✓ {successMsg}
            </div>
          )}

          <div style={{ marginBottom: '10px', display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
            <label style={{ fontSize: '12px', fontWeight: 600, color: '#c9d1d9' }}>
              Configuration JSON (`mcpServers`)
            </label>
            <input
              type="file"
              accept=".json,application/json"
              onChange={handleFileUpload}
              style={{ fontSize: '11px', color: '#8b8fa8' }}
            />
          </div>

          <textarea
            value={configText}
            onChange={(e) => setConfigText(e.target.value)}
            rows={10}
            style={{
              width: '100%',
              backgroundColor: '#090a0f',
              border: '1px solid #2a2d40',
              borderRadius: '6px',
              color: '#00e575',
              fontFamily: 'monospace',
              fontSize: '12px',
              padding: '10px',
              boxSizing: 'border-box',
              marginBottom: '14px',
            }}
          />

          {preview && (
            <div
              data-testid="import-preview-panel"
              style={{
                backgroundColor: '#0c0d13',
                border: '1px solid #24283b',
                borderRadius: '8px',
                padding: '12px 14px',
                marginBottom: '14px',
              }}
            >
              <div style={{ fontSize: '12px', fontWeight: 700, color: '#a5d6ff', marginBottom: '8px' }}>
                Detected Servers ({preview.servers.length}) — Secret Ref Conversion Preview
              </div>
              {preview.servers.map((srv) => (
                <div
                  key={srv.server_name}
                  style={{
                    padding: '8px 10px',
                    borderBottom: '1px solid #1a1c28',
                    fontSize: '12px',
                  }}
                >
                  <div style={{ color: '#fff', fontWeight: 600 }}>{srv.server_name}</div>
                  {srv.converted_secrets.length > 0 && (
                    <div style={{ color: '#ffd27d', fontSize: '11px', marginTop: '4px' }}>
                      🔒 Converted secrets:{' '}
                      {srv.converted_secrets.map((s) => `${s.key} → ${s.converted_to}`).join(', ')}
                    </div>
                  )}
                </div>
              ))}
            </div>
          )}

          <div style={{ display: 'flex', justifyContent: 'flex-end', gap: '10px' }}>
            <button
              onClick={handlePreview}
              disabled={busy}
              style={{
                backgroundColor: '#1e2235',
                border: '1px solid #303652',
                color: '#a5d6ff',
                padding: '8px 14px',
                borderRadius: '6px',
                fontWeight: 600,
                fontSize: '12px',
                cursor: 'pointer',
              }}
            >
              👁️ Preview Secret Conversion
            </button>
            <button
              onClick={handleImport}
              disabled={busy}
              style={{
                backgroundColor: '#00e575',
                border: 'none',
                color: '#000',
                padding: '8px 16px',
                borderRadius: '6px',
                fontWeight: 700,
                fontSize: '12px',
                cursor: 'pointer',
              }}
            >
              {busy ? 'Importing...' : '📥 Import Fleet'}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
};
