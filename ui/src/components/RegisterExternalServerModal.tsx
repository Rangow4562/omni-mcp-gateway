import React, { useState } from 'react';
import { apiFetch } from '../api';

interface RegisterExternalServerModalProps {
  isOpen: boolean;
  onClose: () => void;
  onRegistered: () => void;
}

export const RegisterExternalServerModal: React.FC<RegisterExternalServerModalProps> = ({
  isOpen,
  onClose,
  onRegistered,
}) => {
  if (!isOpen) return null;

  const [slug, setSlug] = useState('');
  const [displayName, setDisplayName] = useState('');
  const [description, setDescription] = useState('');
  const [owningTeam, setOwningTeam] = useState('engineering');
  const [transport, setTransport] = useState<'stdio' | 'streamable_http'>('stdio');

  // Stdio fields
  const [command, setCommand] = useState('');
  const [argsStr, setArgsStr] = useState('');
  const [cwd, setCwd] = useState('');
  const [envRefsList, setEnvRefsList] = useState<Array<{ key: string; ref: string }>>([
    { key: '', ref: '' },
  ]);

  // Streamable HTTP fields
  const [url, setUrl] = useState('');
  const [sessionMode, setSessionMode] = useState<'stateless' | 'stateful'>('stateful');
  const [bearerRef, setBearerRef] = useState('');
  const [headerRefsList, setHeaderRefsList] = useState<Array<{ key: string; ref: string }>>([
    { key: '', ref: '' },
  ]);

  const [submitting, setSubmitting] = useState(false);
  const [errorMessage, setErrorMessage] = useState<string | null>(null);

  const isValidSecretRef = (val: string) => {
    if (!val.trim()) return true;
    return /^\{(env|file):[^}]+\}$/.test(val.trim());
  };

  const handleAddEnvRow = () => {
    setEnvRefsList([...envRefsList, { key: '', ref: '' }]);
  };

  const handleRemoveEnvRow = (idx: number) => {
    setEnvRefsList(envRefsList.filter((_, i) => i !== idx));
  };

  const handleAddHeaderRow = () => {
    setHeaderRefsList([...headerRefsList, { key: '', ref: '' }]);
  };

  const handleRemoveHeaderRow = (idx: number) => {
    setHeaderRefsList(headerRefsList.filter((_, i) => i !== idx));
  };

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setErrorMessage(null);

    const cleanSlug = slug.trim().toLowerCase().replace(/[^a-z0-9_-]/g, '-');
    if (!cleanSlug) {
      setErrorMessage('Server slug is required.');
      return;
    }

    if (transport === 'streamable_http') {
      if (!url.trim().startsWith('http://')) {
        if (url.trim().startsWith('https://')) {
          setErrorMessage(
            'HTTPS upstreams are not natively supported under zero-dependency policy. Use http:// or bridge via a local stdio gateway (e.g. npx -y supergateway --streamableHttp ...).'
          );
        } else {
          setErrorMessage("URL must start with 'http://'.");
        }
        return;
      }
      if (bearerRef.trim() && !isValidSecretRef(bearerRef)) {
        setErrorMessage("Bearer token must be a secret reference: '{env:NAME}' or '{file:PATH}'.");
        return;
      }
      for (const h of headerRefsList) {
        if (h.key.trim() && !isValidSecretRef(h.ref)) {
          setErrorMessage(`Header '${h.key}' ref must match '{env:NAME}' or '{file:PATH}'.`);
          return;
        }
      }
    } else {
      if (!command.trim()) {
        setErrorMessage('Command executable path is required.');
        return;
      }
      for (const envRow of envRefsList) {
        if (envRow.key.trim() && !isValidSecretRef(envRow.ref)) {
          setErrorMessage(`Env '${envRow.key}' ref must match '{env:NAME}' or '{file:PATH}'.`);
          return;
        }
      }
    }

    setSubmitting(true);
    try {
      let payload: any = {
        slug: cleanSlug,
        displayName: displayName.trim() || cleanSlug,
        description: description.trim(),
        owningTeam: owningTeam.trim() || 'engineering',
        transport,
      };

      if (transport === 'stdio') {
        const args = argsStr
          .split('\n')
          .map((s) => s.trim())
          .filter(Boolean);
        const env_refs: Record<string, string> = {};
        for (const r of envRefsList) {
          if (r.key.trim() && r.ref.trim()) {
            env_refs[r.key.trim()] = r.ref.trim();
          }
        }
        payload.stdio = {
          command: command.trim(),
          args,
          cwd: cwd.trim() || null,
          env_refs,
        };
      } else {
        const header_refs: Record<string, string> = {};
        for (const h of headerRefsList) {
          if (h.key.trim() && h.ref.trim()) {
            header_refs[h.key.trim()] = h.ref.trim();
          }
        }
        payload.streamable_http = {
          url: url.trim(),
          session_mode: sessionMode,
          bearer_ref: bearerRef.trim() || null,
          header_refs,
        };
      }

      await apiFetch('/api/v1/servers/external/register', {
        method: 'POST',
        body: JSON.stringify(payload),
      });

      onRegistered();
      onClose();
    } catch (err: any) {
      if (err.data && err.data.error === 'IDENTIFIER_COLLISION') {
        setErrorMessage(
          `Identifier collision: '${err.data.first_raw}' collides with '${err.data.second_raw}' (sanitized to '${err.data.sanitized}').`
        );
      } else {
        setErrorMessage(err.message || 'Failed to register external server.');
      }
    } finally {
      setSubmitting(false);
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
      zIndex: 200,
    }}>
      <div style={{
        backgroundColor: '#161720',
        border: '1px solid #282a3b',
        borderRadius: '8px',
        width: '640px',
        maxWidth: '92vw',
        maxHeight: '90vh',
        display: 'flex',
        flexDirection: 'column',
        boxShadow: '0 20px 50px rgba(0,0,0,0.6)',
      }}>
        {/* Header */}
        <div style={{
          padding: '16px 20px',
          borderBottom: '1px solid #222332',
          display: 'flex',
          justifyContent: 'space-between',
          alignItems: 'center',
        }}>
          <div>
            <h3 style={{ margin: 0, fontSize: '16px', fontWeight: 700, color: '#fff' }}>
              Register External MCP Server
            </h3>
            <div style={{ fontSize: '12px', color: '#888', marginTop: '2px' }}>
              Connect local stdio subprocesses or remote Streamable HTTP endpoints
            </div>
          </div>
          <button
            onClick={onClose}
            style={{
              background: 'none',
              border: 'none',
              color: '#888',
              fontSize: '18px',
              cursor: 'pointer',
            }}
          >
            ✕
          </button>
        </div>

        {/* Body */}
        <form onSubmit={handleSubmit} style={{ padding: '20px', overflowY: 'auto', flex: 1 }}>
          {errorMessage && (
            <div style={{
              backgroundColor: '#441818',
              border: '1px solid #772b2b',
              color: '#ff8888',
              padding: '10px 14px',
              borderRadius: '6px',
              fontSize: '12px',
              marginBottom: '16px',
              lineHeight: '1.4',
            }}>
              ⚠️ {errorMessage}
            </div>
          )}

          {/* Basic metadata */}
          <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: '12px', marginBottom: '12px' }}>
            <div>
              <label style={{ display: 'block', fontSize: '11px', fontWeight: 600, color: '#aaa', marginBottom: '4px' }}>
                SERVER SLUG * (auto-prefixed 'external-mcp:')
              </label>
              <input
                type="text"
                required
                placeholder="e.g. filesystem or sqlite"
                value={slug}
                onChange={(e) => setSlug(e.target.value)}
                style={{
                  width: '100%',
                  background: '#0e0f14',
                  border: '1px solid #282a3b',
                  color: '#fff',
                  padding: '7px 10px',
                  borderRadius: '4px',
                  fontSize: '13px',
                  boxSizing: 'border-box',
                }}
              />
            </div>
            <div>
              <label style={{ display: 'block', fontSize: '11px', fontWeight: 600, color: '#aaa', marginBottom: '4px' }}>
                DISPLAY NAME
              </label>
              <input
                type="text"
                placeholder="e.g. Local Filesystem"
                value={displayName}
                onChange={(e) => setDisplayName(e.target.value)}
                style={{
                  width: '100%',
                  background: '#0e0f14',
                  border: '1px solid #282a3b',
                  color: '#fff',
                  padding: '7px 10px',
                  borderRadius: '4px',
                  fontSize: '13px',
                  boxSizing: 'border-box',
                }}
              />
            </div>
          </div>

          <div style={{ display: 'grid', gridTemplateColumns: '2fr 1fr', gap: '12px', marginBottom: '16px' }}>
            <div>
              <label style={{ display: 'block', fontSize: '11px', fontWeight: 600, color: '#aaa', marginBottom: '4px' }}>
                DESCRIPTION
              </label>
              <input
                type="text"
                placeholder="Capabilities and purpose of this MCP server"
                value={description}
                onChange={(e) => setDescription(e.target.value)}
                style={{
                  width: '100%',
                  background: '#0e0f14',
                  border: '1px solid #282a3b',
                  color: '#fff',
                  padding: '7px 10px',
                  borderRadius: '4px',
                  fontSize: '13px',
                  boxSizing: 'border-box',
                }}
              />
            </div>
            <div>
              <label style={{ display: 'block', fontSize: '11px', fontWeight: 600, color: '#aaa', marginBottom: '4px' }}>
                OWNING TEAM
              </label>
              <input
                type="text"
                placeholder="engineering"
                value={owningTeam}
                onChange={(e) => setOwningTeam(e.target.value)}
                style={{
                  width: '100%',
                  background: '#0e0f14',
                  border: '1px solid #282a3b',
                  color: '#fff',
                  padding: '7px 10px',
                  borderRadius: '4px',
                  fontSize: '13px',
                  boxSizing: 'border-box',
                }}
              />
            </div>
          </div>

          {/* Transport Selector */}
          <div style={{ marginBottom: '16px', background: '#0e0f14', padding: '10px 14px', borderRadius: '6px', border: '1px solid #222332' }}>
            <label style={{ display: 'block', fontSize: '11px', fontWeight: 700, color: '#00e575', marginBottom: '6px' }}>
              UPSTREAM TRANSPORT
            </label>
            <div style={{ display: 'flex', gap: '20px' }}>
              <label style={{ display: 'flex', alignItems: 'center', gap: '6px', cursor: 'pointer', fontSize: '13px', color: '#fff' }}>
                <input
                  type="radio"
                  name="transport"
                  value="stdio"
                  checked={transport === 'stdio'}
                  onChange={() => setTransport('stdio')}
                />
                <strong>stdio</strong> (Local Subprocess / npx / python)
              </label>
              <label style={{ display: 'flex', alignItems: 'center', gap: '6px', cursor: 'pointer', fontSize: '13px', color: '#fff' }}>
                <input
                  type="radio"
                  name="transport"
                  value="streamable_http"
                  checked={transport === 'streamable_http'}
                  onChange={() => setTransport('streamable_http')}
                />
                <strong>Streamable HTTP</strong> (Remote / LAN Endpoint)
              </label>
            </div>
          </div>

          {/* Stdio Specific Fields */}
          {transport === 'stdio' ? (
            <div style={{ borderTop: '1px solid #282a3b', paddingTop: '12px' }}>
              <div style={{ marginBottom: '12px' }}>
                <label style={{ display: 'block', fontSize: '11px', fontWeight: 600, color: '#aaa', marginBottom: '4px' }}>
                  EXECUTABLE COMMAND * (Must match allowlist)
                </label>
                <input
                  type="text"
                  required
                  placeholder="e.g. C:\Program Files\nodejs\node.exe or /usr/bin/python3"
                  value={command}
                  onChange={(e) => setCommand(e.target.value)}
                  style={{
                    width: '100%',
                    background: '#0e0f14',
                    border: '1px solid #282a3b',
                    color: '#fff',
                    padding: '7px 10px',
                    borderRadius: '4px',
                    fontSize: '13px',
                    boxSizing: 'border-box',
                  }}
                />
              </div>

              <div style={{ display: 'grid', gridTemplateColumns: '2fr 1fr', gap: '12px', marginBottom: '12px' }}>
                <div>
                  <label style={{ display: 'block', fontSize: '11px', fontWeight: 600, color: '#aaa', marginBottom: '4px' }}>
                    ARGUMENTS (One per line)
                  </label>
                  <textarea
                    rows={3}
                    placeholder="-y&#10;@modelcontextprotocol/server-filesystem&#10;D:\Data"
                    value={argsStr}
                    onChange={(e) => setArgsStr(e.target.value)}
                    style={{
                      width: '100%',
                      background: '#0e0f14',
                      border: '1px solid #282a3b',
                      color: '#fff',
                      padding: '7px 10px',
                      borderRadius: '4px',
                      fontSize: '12px',
                      boxSizing: 'border-box',
                      fontFamily: 'monospace',
                    }}
                  />
                </div>
                <div>
                  <label style={{ display: 'block', fontSize: '11px', fontWeight: 600, color: '#aaa', marginBottom: '4px' }}>
                    WORKING DIRECTORY (Optional)
                  </label>
                  <input
                    type="text"
                    placeholder="e.g. D:\Projects"
                    value={cwd}
                    onChange={(e) => setCwd(e.target.value)}
                    style={{
                      width: '100%',
                      background: '#0e0f14',
                      border: '1px solid #282a3b',
                      color: '#fff',
                      padding: '7px 10px',
                      borderRadius: '4px',
                      fontSize: '12px',
                      boxSizing: 'border-box',
                    }}
                  />
                </div>
              </div>

              {/* Environment Variable Refs */}
              <div>
                <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: '6px' }}>
                  <label style={{ fontSize: '11px', fontWeight: 600, color: '#aaa' }}>
                    ENVIRONMENT VARIABLE REFS (Scrubbed child environment)
                  </label>
                  <button
                    type="button"
                    onClick={handleAddEnvRow}
                    style={{ background: '#222330', border: '1px solid #333446', color: '#00e575', fontSize: '11px', padding: '2px 8px', borderRadius: '4px', cursor: 'pointer' }}
                  >
                    + Add Var Ref
                  </button>
                </div>
                <div style={{ fontSize: '11px', color: '#777', marginBottom: '8px' }}>
                  Use <code>&#123;env:VAR_NAME&#125;</code> or <code>&#123;file:relative/path&#125;</code>. Raw secrets are strictly rejected.
                </div>

                {envRefsList.map((row, idx) => {
                  const isInvalid = row.ref.trim() && !isValidSecretRef(row.ref);
                  return (
                    <div key={idx} style={{ display: 'flex', gap: '8px', marginBottom: '6px' }}>
                      <input
                        type="text"
                        placeholder="VAR_NAME"
                        value={row.key}
                        onChange={(e) => {
                          const updated = [...envRefsList];
                          updated[idx].key = e.target.value;
                          setEnvRefsList(updated);
                        }}
                        style={{
                          flex: 1,
                          background: '#0e0f14',
                          border: '1px solid #282a3b',
                          color: '#fff',
                          padding: '5px 8px',
                          borderRadius: '4px',
                          fontSize: '12px',
                        }}
                      />
                      <input
                        type="text"
                        placeholder="{env:NAME} or {file:PATH}"
                        value={row.ref}
                        onChange={(e) => {
                          const updated = [...envRefsList];
                          updated[idx].ref = e.target.value;
                          setEnvRefsList(updated);
                        }}
                        style={{
                          flex: 2,
                          background: '#0e0f14',
                          border: isInvalid ? '1px solid #e53935' : '1px solid #282a3b',
                          color: isInvalid ? '#ff8888' : '#fff',
                          padding: '5px 8px',
                          borderRadius: '4px',
                          fontSize: '12px',
                        }}
                      />
                      <button
                        type="button"
                        onClick={() => handleRemoveEnvRow(idx)}
                        style={{ background: '#331a1a', border: '1px solid #552222', color: '#ff6666', padding: '4px 8px', borderRadius: '4px', cursor: 'pointer' }}
                      >
                        ✕
                      </button>
                    </div>
                  );
                })}
              </div>
            </div>
          ) : (
            /* Streamable HTTP Specific Fields */
            <div style={{ borderTop: '1px solid #282a3b', paddingTop: '12px' }}>
              <div style={{ display: 'grid', gridTemplateColumns: '3fr 1fr', gap: '12px', marginBottom: '12px' }}>
                <div>
                  <label style={{ display: 'block', fontSize: '11px', fontWeight: 600, color: '#aaa', marginBottom: '4px' }}>
                    ENDPOINT URL * (http:// only)
                  </label>
                  <input
                    type="text"
                    required
                    placeholder="http://localhost:3000/mcp"
                    value={url}
                    onChange={(e) => setUrl(e.target.value)}
                    style={{
                      width: '100%',
                      background: '#0e0f14',
                      border: '1px solid #282a3b',
                      color: '#fff',
                      padding: '7px 10px',
                      borderRadius: '4px',
                      fontSize: '13px',
                      boxSizing: 'border-box',
                    }}
                  />
                </div>
                <div>
                  <label style={{ display: 'block', fontSize: '11px', fontWeight: 600, color: '#aaa', marginBottom: '4px' }}>
                    SESSION MODE
                  </label>
                  <select
                    value={sessionMode}
                    onChange={(e) => setSessionMode(e.target.value as any)}
                    style={{
                      width: '100%',
                      background: '#0e0f14',
                      border: '1px solid #282a3b',
                      color: '#fff',
                      padding: '7px 10px',
                      borderRadius: '4px',
                      fontSize: '13px',
                      boxSizing: 'border-box',
                      cursor: 'pointer',
                    }}
                  >
                    <option value="stateful">Stateful (Mcp-Session-Id)</option>
                    <option value="stateless">Stateless</option>
                  </select>
                </div>
              </div>

              <div style={{ marginBottom: '12px' }}>
                <label style={{ display: 'block', fontSize: '11px', fontWeight: 600, color: '#aaa', marginBottom: '4px' }}>
                  BEARER TOKEN REF (Optional: &#123;env:...&#125; or &#123;file:...&#125;)
                </label>
                <input
                  type="text"
                  placeholder="{env:REMOTE_MCP_TOKEN}"
                  value={bearerRef}
                  onChange={(e) => setBearerRef(e.target.value)}
                  style={{
                    width: '100%',
                    background: '#0e0f14',
                    border: bearerRef.trim() && !isValidSecretRef(bearerRef) ? '1px solid #e53935' : '1px solid #282a3b',
                    color: bearerRef.trim() && !isValidSecretRef(bearerRef) ? '#ff8888' : '#fff',
                    padding: '7px 10px',
                    borderRadius: '4px',
                    fontSize: '13px',
                    boxSizing: 'border-box',
                  }}
                />
              </div>

              {/* Custom Header Refs */}
              <div>
                <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: '6px' }}>
                  <label style={{ fontSize: '11px', fontWeight: 600, color: '#aaa' }}>
                    CUSTOM HEADER REFS
                  </label>
                  <button
                    type="button"
                    onClick={handleAddHeaderRow}
                    style={{ background: '#222330', border: '1px solid #333446', color: '#00e575', fontSize: '11px', padding: '2px 8px', borderRadius: '4px', cursor: 'pointer' }}
                  >
                    + Add Header Ref
                  </button>
                </div>

                {headerRefsList.map((row, idx) => {
                  const isInvalid = row.ref.trim() && !isValidSecretRef(row.ref);
                  return (
                    <div key={idx} style={{ display: 'flex', gap: '8px', marginBottom: '6px' }}>
                      <input
                        type="text"
                        placeholder="Header-Name"
                        value={row.key}
                        onChange={(e) => {
                          const updated = [...headerRefsList];
                          updated[idx].key = e.target.value;
                          setHeaderRefsList(updated);
                        }}
                        style={{
                          flex: 1,
                          background: '#0e0f14',
                          border: '1px solid #282a3b',
                          color: '#fff',
                          padding: '5px 8px',
                          borderRadius: '4px',
                          fontSize: '12px',
                        }}
                      />
                      <input
                        type="text"
                        placeholder="{env:VAR} or {file:PATH}"
                        value={row.ref}
                        onChange={(e) => {
                          const updated = [...headerRefsList];
                          updated[idx].ref = e.target.value;
                          setHeaderRefsList(updated);
                        }}
                        style={{
                          flex: 2,
                          background: '#0e0f14',
                          border: isInvalid ? '1px solid #e53935' : '1px solid #282a3b',
                          color: isInvalid ? '#ff8888' : '#fff',
                          padding: '5px 8px',
                          borderRadius: '4px',
                          fontSize: '12px',
                        }}
                      />
                      <button
                        type="button"
                        onClick={() => handleRemoveHeaderRow(idx)}
                        style={{ background: '#331a1a', border: '1px solid #552222', color: '#ff6666', padding: '4px 8px', borderRadius: '4px', cursor: 'pointer' }}
                      >
                        ✕
                      </button>
                    </div>
                  );
                })}
              </div>
            </div>
          )}

          {/* Action buttons */}
          <div style={{ display: 'flex', justifyContent: 'flex-end', gap: '10px', marginTop: '24px', borderTop: '1px solid #222332', paddingTop: '16px' }}>
            <button
              type="button"
              onClick={onClose}
              disabled={submitting}
              style={{
                backgroundColor: '#222330',
                border: '1px solid #333446',
                color: '#fff',
                padding: '8px 16px',
                borderRadius: '6px',
                fontSize: '13px',
                cursor: 'pointer',
              }}
            >
              Cancel
            </button>
            <button
              type="submit"
              disabled={submitting}
              style={{
                backgroundColor: '#00e575',
                border: 'none',
                color: '#000',
                padding: '8px 20px',
                borderRadius: '6px',
                fontWeight: 700,
                fontSize: '13px',
                cursor: submitting ? 'wait' : 'pointer',
              }}
            >
              {submitting ? 'Connecting & Registering...' : 'Register Server'}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
};
