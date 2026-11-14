import React, { useState, useEffect } from 'react';
import { ServerSummary, ServerDetail } from '../types';
import {
  ToolGroupItem,
  ToolGroupEntry,
  listToolGroups,
  saveToolGroup,
  deleteToolGroup,
  apiFetch,
} from '../api';

interface ToolGroupsStudioProps {
  isOpen: boolean;
  onClose: () => void;
  servers: ServerSummary[];
}

export const ToolGroupsStudio: React.FC<ToolGroupsStudioProps> = ({
  isOpen,
  onClose,
  servers,
}) => {
  const [groups, setGroups] = useState<ToolGroupItem[]>([]);
  const [slug, setSlug] = useState('incident-triage');
  const [displayName, setDisplayName] = useState('Incident Triage');
  const [description, setDescription] = useState('Scoped virtual MCP endpoint for incident response');
  const [selectedEntries, setSelectedEntries] = useState<ToolGroupEntry[]>([]);
  const [serverDetails, setServerDetails] = useState<Record<string, ServerDetail>>({});
  const [statusMsg, setStatusMsg] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const loadGroups = async () => {
    try {
      const list = await listToolGroups();
      setGroups(list);
    } catch (e: any) {
      setStatusMsg(e?.message || 'Failed to load tool groups');
    }
  };

  useEffect(() => {
    if (!isOpen) return;
    loadGroups();
    servers.forEach(async (s) => {
      try {
        const detail = await apiFetch<ServerDetail>(`/api/servers/${encodeURIComponent(s.name)}`);
        setServerDetails((prev) => ({ ...prev, [s.name]: detail }));
      } catch {
        // Ignore individual server detail errors
      }
    });
  }, [isOpen, servers]);

  if (!isOpen) return null;

  const isSelected = (serverName: string, toolName?: string | null) =>
    selectedEntries.some(
      (e) => e.server_name === serverName && (e.tool_name || null) === (toolName || null)
    );

  const toggleEntry = (serverName: string, toolName?: string | null) => {
    setSelectedEntries((prev) => {
      const exists = prev.some(
        (e) => e.server_name === serverName && (e.tool_name || null) === (toolName || null)
      );
      if (exists) {
        return prev.filter(
          (e) => !(e.server_name === serverName && (e.tool_name || null) === (toolName || null))
        );
      }
      return [...prev, { server_name: serverName, tool_name: toolName ?? null }];
    });
  };

  const totalCatalogTools = servers.reduce((acc, s) => acc + (s.toolsCount || 0), 0);
  const selectedToolCount = selectedEntries.reduce((acc, e) => {
    if (e.tool_name) return acc + 1;
    const srv = servers.find((s) => s.name === e.server_name);
    return acc + (srv?.toolsCount || 1);
  }, 0);
  const savingsPct =
    totalCatalogTools > 0
      ? Math.max(0, Math.round(((totalCatalogTools - selectedToolCount) / totalCatalogTools) * 100))
      : 0;

  const handleSave = async () => {
    setLoading(true);
    setStatusMsg(null);
    try {
      await saveToolGroup({
        slug: slug.trim(),
        display_name: displayName.trim() || slug.trim(),
        description: description.trim(),
        entries: selectedEntries,
      });
      await loadGroups();
      setStatusMsg(`Saved virtual endpoint /groups/${slug.trim()}/mcp`);
    } catch (e: any) {
      setStatusMsg(e?.message || 'Failed to save tool group');
    } finally {
      setLoading(false);
    }
  };

  const handleDelete = async (targetSlug: string) => {
    setLoading(true);
    try {
      await deleteToolGroup(targetSlug);
      await loadGroups();
      setStatusMsg(`Deleted group '${targetSlug}'`);
    } catch (e: any) {
      setStatusMsg(e?.message || 'Failed to delete tool group');
    } finally {
      setLoading(false);
    }
  };

  return (
    <div style={{
      position: 'fixed', inset: 0, backgroundColor: 'rgba(0,0,0,0.75)',
      display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 220, padding: '20px',
    }}>
      <div style={{
        background: '#12131b', border: '1px solid #282a3b', borderRadius: '10px',
        width: '100%', maxWidth: '920px', maxHeight: '88vh', display: 'flex', flexDirection: 'column',
      }}>
        <div style={{
          padding: '16px 22px', borderBottom: '1px solid #202230',
          display: 'flex', justifyContent: 'space-between', alignItems: 'center',
        }}>
          <div>
            <h3 style={{ margin: 0, fontSize: '18px', color: '#fff' }}>Tool Groups Studio</h3>
            <div style={{ fontSize: '12px', color: '#888', marginTop: '3px' }}>
              Curate scoped <code>/groups/&lt;slug&gt;/mcp</code> virtual endpoints &amp; reduce agent context footprint
            </div>
          </div>
          <button onClick={onClose} style={{ background: 'transparent', border: 'none', color: '#aaa', fontSize: '20px', cursor: 'pointer' }}>✕</button>
        </div>

        <div style={{ display: 'grid', gridTemplateColumns: '1fr 1.3fr', gap: '16px', padding: '18px', overflowY: 'auto' }}>
          {/* Existing Groups */}
          <div>
            <div style={{ fontSize: '13px', fontWeight: 700, color: '#ccc', marginBottom: '10px' }}>
              Active Virtual Groups ({groups.length})
            </div>
            {groups.length === 0 ? (
              <div style={{ color: '#686b80', fontSize: '12px', padding: '14px', border: '1px dashed #2b2e40', borderRadius: '6px' }}>
                No tool groups configured yet.
              </div>
            ) : (
              groups.map((g) => (
                <div key={g.slug} style={{
                  background: '#181a24', border: '1px solid #2b2e40', borderRadius: '6px',
                  padding: '10px 12px', marginBottom: '8px',
                }}>
                  <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
                    <strong style={{ color: '#fff', fontSize: '13px' }}>{g.display_name}</strong>
                    <button
                      onClick={() => handleDelete(g.slug)}
                      style={{ background: '#3a1d24', color: '#ff6b7a', border: '1px solid #6b2a36', borderRadius: '4px', padding: '2px 8px', fontSize: '11px', cursor: 'pointer' }}
                    >
                      Delete
                    </button>
                  </div>
                  <div style={{ fontSize: '11px', color: '#6ea8fe', marginTop: '3px' }}>
                    POST /groups/{g.slug}/mcp ({g.entries.length} bindings)
                  </div>
                  {g.description && (
                    <div style={{ fontSize: '11px', color: '#888', marginTop: '4px' }}>{g.description}</div>
                  )}
                </div>
              ))
            )}
          </div>

          {/* Group Builder */}
          <div style={{ background: '#161822', border: '1px solid #26293a', borderRadius: '8px', padding: '14px' }}>
            <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: '10px' }}>
              <strong style={{ color: '#fff', fontSize: '14px' }}>Create / Update Group</strong>
              <span style={{ fontSize: '11px', background: '#1b382b', color: '#00e575', padding: '2px 8px', borderRadius: '10px' }}>
                ~{savingsPct}% token reduction ({selectedToolCount}/{totalCatalogTools} tools)
              </span>
            </div>

            <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: '8px', marginBottom: '8px' }}>
              <input
                value={slug}
                onChange={(e) => setSlug(e.target.value)}
                placeholder="Group slug (e.g. incident-triage)"
                style={{ background: '#0e0f14', border: '1px solid #2c2f42', color: '#fff', padding: '7px 10px', borderRadius: '5px', fontSize: '12px' }}
              />
              <input
                value={displayName}
                onChange={(e) => setDisplayName(e.target.value)}
                placeholder="Display name"
                style={{ background: '#0e0f14', border: '1px solid #2c2f42', color: '#fff', padding: '7px 10px', borderRadius: '5px', fontSize: '12px' }}
              />
            </div>
            <input
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              placeholder="Description"
              style={{ width: '100%', boxSizing: 'border-box', background: '#0e0f14', border: '1px solid #2c2f42', color: '#fff', padding: '7px 10px', borderRadius: '5px', fontSize: '12px', marginBottom: '10px' }}
            />

            <div style={{ maxHeight: '220px', overflowY: 'auto', border: '1px solid #232638', borderRadius: '6px', padding: '8px', marginBottom: '10px' }}>
              {servers.map((srv) => {
                const detail = serverDetails[srv.name];
                const toolNames = detail ? Object.keys(detail.tools || {}) : [];
                return (
                  <div key={srv.name} style={{ marginBottom: '8px' }}>
                    <label style={{ display: 'flex', alignItems: 'center', gap: '6px', color: '#fff', fontSize: '12px', fontWeight: 600, cursor: 'pointer' }}>
                      <input
                        type="checkbox"
                        checked={isSelected(srv.name, null)}
                        onChange={() => toggleEntry(srv.name, null)}
                      />
                      {srv.name} (all {srv.toolsCount} tools)
                    </label>
                    {toolNames.length > 0 && (
                      <div style={{ paddingLeft: '20px', marginTop: '4px', display: 'flex', flexWrap: 'wrap', gap: '6px' }}>
                        {toolNames.map((t) => (
                          <label key={t} style={{ display: 'flex', alignItems: 'center', gap: '4px', fontSize: '11px', color: '#aaa', cursor: 'pointer' }}>
                            <input
                              type="checkbox"
                              checked={isSelected(srv.name, t)}
                              onChange={() => toggleEntry(srv.name, t)}
                            />
                            <code>{t}</code>
                          </label>
                        ))}
                      </div>
                    )}
                  </div>
                );
              })}
            </div>

            {statusMsg && (
              <div style={{ fontSize: '12px', color: '#00e575', marginBottom: '8px' }}>{statusMsg}</div>
            )}

            <button
              disabled={loading || !slug.trim()}
              onClick={handleSave}
              style={{
                background: '#276ef1', color: '#fff', border: 'none', borderRadius: '6px',
                padding: '8px 14px', fontSize: '13px', fontWeight: 600, cursor: 'pointer', width: '100%',
              }}
            >
              {loading ? 'Saving...' : 'Save Tool Group'}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
};
