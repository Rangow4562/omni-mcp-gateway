import React, { useState, useEffect, useCallback } from 'react';
import { SkillRecordItem, ToolGroupItem } from '../types';
import {
  fetchSkills,
  saveSkill,
  deleteSkill,
  listToolGroups,
  fetchPluginManifest,
  PluginManifestResponse,
} from '../api';

interface SkillsStudioModalProps {
  isOpen: boolean;
  onClose: () => void;
  sseRevision?: number;
}

const AGENT_OPTIONS: Array<{ id: string; label: string }> = [
  { id: 'claude', label: 'Claude Code' },
  { id: 'antigravity', label: 'Antigravity' },
  { id: 'gemini', label: 'Gemini CLI' },
  { id: 'codex', label: 'Codex' },
  { id: 'cursor', label: 'Cursor' },
  { id: 'vscode', label: 'VS Code / Copilot' },
  { id: 'windsurf', label: 'Windsurf' },
  { id: 'opencode', label: 'OpenCode' },
];

export const SkillsStudioModal: React.FC<SkillsStudioModalProps> = ({
  isOpen,
  onClose,
  sseRevision = 0,
}) => {
  const [activeTab, setActiveTab] = useState<'registry' | 'plugin'>('registry');

  const [skills, setSkills] = useState<SkillRecordItem[]>([]);
  const [selectedSlug, setSelectedSlug] = useState<string | null>(null);
  const [maxFiles, setMaxFiles] = useState<number>(512);
  const [maxTotalBytes, setMaxTotalBytes] = useState<number>(16 * 1024 * 1024);
  const [loading, setLoading] = useState(false);
  const [errorMsg, setErrorMsg] = useState<string | null>(null);
  const [statusMsg, setStatusMsg] = useState<string | null>(null);

  // Author form state
  const [slug, setSlug] = useState('enterprise-triage-skill');
  const [groupSlug, setGroupSlug] = useState('');
  const [skillMarkdown, setSkillMarkdown] = useState(
    '---\nname: Enterprise Triage Playbook\ndescription: Governed incident triage workflow over OmniMCP Code Mode\n---\n\n# Enterprise Triage Playbook\n\nUse `codemode.<service>.<tool>` inside Code Mode before mutating production state.\n'
  );
  const [saving, setSaving] = useState(false);

  // Enterprise Plugin Center state
  const [toolGroups, setToolGroups] = useState<ToolGroupItem[]>([]);
  const [pluginAgent, setPluginAgent] = useState<string>('claude');
  const [pluginMode, setPluginMode] = useState<'standard' | 'enterprise'>('standard');
  const [pluginGroup, setPluginGroup] = useState<string>('');
  const [pluginAuthEnv, setPluginAuthEnv] = useState<string>('OMNI_AUTH_TOKEN');
  const [pluginGatewayUrl, setPluginGatewayUrl] = useState<string>('https://mcp.corp.example.com');
  const [pluginManifest, setPluginManifest] = useState<PluginManifestResponse | null>(null);
  const [pluginLoading, setPluginLoading] = useState<boolean>(false);
  const [copiedKey, setCopiedKey] = useState<string | null>(null);

  const loadSkills = useCallback(async () => {
    setLoading(true);
    setErrorMsg(null);
    try {
      const res = await fetchSkills();
      setSkills(res.skills || []);
      if (res.max_files) setMaxFiles(res.max_files);
      if (res.max_total_bytes) setMaxTotalBytes(res.max_total_bytes);
      if (!selectedSlug && res.skills && res.skills.length > 0) {
        setSelectedSlug(res.skills[0].slug);
      }
    } catch (e: any) {
      setErrorMsg(e?.message || 'Failed to load MCP skills');
    } finally {
      setLoading(false);
    }
  }, [selectedSlug]);

  const loadPluginCenter = useCallback(async () => {
    setPluginLoading(true);
    setErrorMsg(null);
    try {
      const manifest = await fetchPluginManifest({
        agent: pluginAgent,
        mode: pluginMode,
        group: pluginGroup || undefined,
        gateway_url: pluginMode === 'enterprise' ? pluginGatewayUrl.trim() || undefined : undefined,
        auth_env: pluginAuthEnv.trim() || 'OMNI_AUTH_TOKEN',
      });
      setPluginManifest(manifest);
    } catch (e: any) {
      setErrorMsg(e?.message || 'Failed to generate plugin manifest');
    } finally {
      setPluginLoading(false);
    }
  }, [pluginAgent, pluginMode, pluginGroup, pluginAuthEnv, pluginGatewayUrl]);

  useEffect(() => {
    if (isOpen) {
      void loadSkills();
      void listToolGroups()
        .then((groups) => setToolGroups(groups || []))
        .catch(() => setToolGroups([]));
    }
  }, [isOpen, sseRevision, loadSkills]);

  useEffect(() => {
    if (isOpen && activeTab === 'plugin') {
      void loadPluginCenter();
    }
  }, [isOpen, activeTab, loadPluginCenter]);

  if (!isOpen) return null;

  const selected = skills.find((s) => s.slug === selectedSlug) || null;
  const totalBytesPct = selected
    ? Math.min(100, Math.round((selected.total_bytes / maxTotalBytes) * 100))
    : 0;
  const fileCountPct = selected
    ? Math.min(100, Math.round((selected.file_count / maxFiles) * 100))
    : 0;

  const handlePublish = async () => {
    setSaving(true);
    setErrorMsg(null);
    setStatusMsg(null);
    try {
      const saved = await saveSkill({
        slug: slug.trim(),
        group_slug: groupSlug.trim() || null,
        files: [
          {
            path: 'SKILL.md',
            mime_type: 'text/markdown',
            content: skillMarkdown,
          },
        ],
      });
      setStatusMsg(
        `Published '${saved.slug}' (manifest SHA-256: ${String(saved.manifest_sha256 || '').slice(0, 16)}...)`
      );
      setSelectedSlug(saved.slug);
      await loadSkills();
    } catch (e: any) {
      setErrorMsg(e?.message || 'Failed to publish skill');
    } finally {
      setSaving(false);
    }
  };

  const handleDelete = async (targetSlug: string) => {
    setErrorMsg(null);
    setStatusMsg(null);
    try {
      await deleteSkill(targetSlug);
      setStatusMsg(`Deleted skill '${targetSlug}'`);
      if (selectedSlug === targetSlug) setSelectedSlug(null);
      await loadSkills();
    } catch (e: any) {
      setErrorMsg(e?.message || 'Failed to delete skill');
    }
  };

  const handleCopy = (key: string, text: string) => {
    if (navigator?.clipboard?.writeText) {
      void navigator.clipboard.writeText(text);
    }
    setCopiedKey(key);
    setTimeout(() => setCopiedKey(null), 1500);
  };

  return (
    <div
      data-testid="skills-studio-modal"
      style={{
        position: 'fixed',
        inset: 0,
        backgroundColor: 'rgba(0, 0, 0, 0.72)',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        zIndex: 1000,
      }}
    >
      <div
        style={{
          backgroundColor: '#13141c',
          border: '1px solid #2b2d3e',
          borderRadius: '10px',
          width: '980px',
          maxWidth: '95vw',
          maxHeight: '90vh',
          display: 'flex',
          flexDirection: 'column',
          overflow: 'hidden',
          color: '#e5e7eb',
        }}
      >
        <div
          style={{
            padding: '14px 20px',
            borderBottom: '1px solid #252736',
            display: 'flex',
            justifyContent: 'space-between',
            alignItems: 'center',
          }}
        >
          <div>
            <h2 style={{ margin: 0, fontSize: '16px', fontWeight: 700 }}>
              📚 Skills Studio (io.modelcontextprotocol/skills · SEP-2640)
            </h2>
            <div style={{ fontSize: '12px', color: '#9ca3af', marginTop: '2px' }}>
              Deterministic SHA-256 skill bundles, auto-synthesized playbooks & Enterprise Plugin distribution
            </div>
          </div>
          <button
            onClick={onClose}
            style={{
              background: '#222434',
              color: '#fff',
              border: '1px solid #35384d',
              borderRadius: '6px',
              padding: '5px 10px',
              cursor: 'pointer',
            }}
          >
            ✕ Close
          </button>
        </div>

        {/* Studio Navigation Tabs */}
        <div
          style={{
            display: 'flex',
            gap: '8px',
            padding: '10px 20px',
            borderBottom: '1px solid #252736',
            backgroundColor: '#101118',
          }}
        >
          <button
            data-testid="tab-skills-registry"
            onClick={() => setActiveTab('registry')}
            style={{
              background: activeTab === 'registry' ? '#276ef1' : '#1b1d29',
              color: '#fff',
              border: '1px solid #2e3144',
              borderRadius: '6px',
              padding: '6px 12px',
              fontSize: '12px',
              fontWeight: 600,
              cursor: 'pointer',
            }}
          >
            Skills Registry ({skills.length})
          </button>
          <button
            data-testid="tab-plugin-center"
            onClick={() => setActiveTab('plugin')}
            style={{
              background: activeTab === 'plugin' ? '#276ef1' : '#1b1d29',
              color: '#fff',
              border: '1px solid #2e3144',
              borderRadius: '6px',
              padding: '6px 12px',
              fontSize: '12px',
              fontWeight: 600,
              cursor: 'pointer',
            }}
          >
            Install & Enterprise Plugin
          </button>
        </div>

        {(errorMsg || statusMsg) && (
          <div style={{ padding: '8px 20px 0 20px' }}>
            {errorMsg && (
              <div style={{ background: '#3a1414', color: '#fca5a5', padding: '8px 12px', borderRadius: '6px', fontSize: '12px' }}>
                ⚠️ {errorMsg}
              </div>
            )}
            {statusMsg && (
              <div style={{ background: '#112e22', color: '#6ee7b7', padding: '8px 12px', borderRadius: '6px', fontSize: '12px' }}>
                ✓ {statusMsg}
              </div>
            )}
          </div>
        )}

        {activeTab === 'registry' ? (
          <div style={{ display: 'flex', flex: 1, overflow: 'hidden', minHeight: '420px' }}>
            {/* Left: Skill List */}
            <div style={{ width: '300px', borderRight: '1px solid #252736', padding: '14px', overflowY: 'auto' }}>
              <div style={{ fontSize: '11px', color: '#9ca3af', marginBottom: '8px', textTransform: 'uppercase' }}>
                Published & Synthesized Skills ({skills.length})
              </div>
              {loading && <div style={{ fontSize: '12px', color: '#9ca3af' }}>Loading skills...</div>}
              {skills.map((sk) => (
                <div
                  key={sk.slug}
                  onClick={() => setSelectedSlug(sk.slug)}
                  style={{
                    padding: '9px 10px',
                    marginBottom: '6px',
                    borderRadius: '6px',
                    cursor: 'pointer',
                    backgroundColor: selectedSlug === sk.slug ? '#1f2542' : '#191a24',
                    border: selectedSlug === sk.slug ? '1px solid #4c6ef5' : '1px solid #262838',
                  }}
                >
                  <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
                    <strong style={{ fontSize: '13px' }}>{sk.slug}</strong>
                    <span style={{ fontSize: '10px', color: sk.auto_generated ? '#79c0ff' : '#00e575' }}>
                      {sk.auto_generated ? 'AUTO' : 'CURATED'}
                    </span>
                  </div>
                  <div style={{ fontSize: '11px', color: '#9ca3af', marginTop: '2px' }}>{sk.name}</div>
                  <div style={{ fontSize: '10px', color: '#6b7280', marginTop: '4px', fontFamily: 'monospace' }}>
                    sha256:{(sk.manifest_sha256 || '').slice(0, 12)} · {sk.file_count} files
                  </div>
                </div>
              ))}
            </div>

            {/* Right: Detail & Author */}
            <div style={{ flex: 1, padding: '16px', overflowY: 'auto', display: 'flex', flexDirection: 'column', gap: '14px' }}>
              {selected && (
                <div style={{ background: '#181a24', border: '1px solid #282a3b', borderRadius: '8px', padding: '12px' }}>
                  <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
                    <div>
                      <strong style={{ fontSize: '14px' }}>{selected.name}</strong>
                      <span style={{ marginLeft: '8px', fontSize: '11px', color: '#9ca3af' }}>
                        {selected.version}
                      </span>
                    </div>
                    {!selected.auto_generated && (
                      <button
                        onClick={() => handleDelete(selected.slug)}
                        style={{
                          background: '#3d1919',
                          color: '#fca5a5',
                          border: '1px solid #6b2424',
                          borderRadius: '5px',
                          padding: '4px 8px',
                          fontSize: '11px',
                          cursor: 'pointer',
                        }}
                      >
                        Delete
                      </button>
                    )}
                  </div>
                  <div style={{ fontSize: '11px', color: '#9ca3af', marginTop: '4px', fontFamily: 'monospace' }}>
                    Manifest SHA-256: <span data-testid="skill-manifest-sha">{selected.manifest_sha256}</span>
                  </div>
                  <div style={{ display: 'flex', gap: '16px', marginTop: '8px', fontSize: '11px' }}>
                    <div style={{ flex: 1 }}>
                      Files Quota: {selected.file_count} / {maxFiles} ({fileCountPct}%)
                      <div style={{ height: '5px', background: '#262838', borderRadius: '3px', marginTop: '3px' }}>
                        <div style={{ height: '100%', width: `${fileCountPct}%`, background: '#3b82f6', borderRadius: '3px' }} />
                      </div>
                    </div>
                    <div style={{ flex: 1 }}>
                      Size Quota: {(selected.total_bytes / 1024).toFixed(1)} KiB / 16 MiB ({totalBytesPct}%)
                      <div style={{ height: '5px', background: '#262838', borderRadius: '3px', marginTop: '3px' }}>
                        <div style={{ height: '100%', width: `${totalBytesPct}%`, background: '#10b981', borderRadius: '3px' }} />
                      </div>
                    </div>
                  </div>
                </div>
              )}

              <div style={{ background: '#181a24', border: '1px solid #282a3b', borderRadius: '8px', padding: '12px' }}>
                <div style={{ fontSize: '13px', fontWeight: 600, marginBottom: '8px' }}>
                  Publish Curated Skill Bundle
                </div>
                <div style={{ display: 'flex', gap: '10px', marginBottom: '8px' }}>
                  <input
                    data-testid="skill-slug-input"
                    value={slug}
                    onChange={(e) => setSlug(e.target.value)}
                    placeholder="skill-slug"
                    style={{
                      flex: 1,
                      background: '#0e0f14',
                      color: '#fff',
                      border: '1px solid #2e3042',
                      borderRadius: '6px',
                      padding: '6px 10px',
                      fontSize: '12px',
                    }}
                  />
                  <input
                    data-testid="skill-group-input"
                    value={groupSlug}
                    onChange={(e) => setGroupSlug(e.target.value)}
                    placeholder="Optional Tool Group slug"
                    style={{
                      flex: 1,
                      background: '#0e0f14',
                      color: '#fff',
                      border: '1px solid #2e3042',
                      borderRadius: '6px',
                      padding: '6px 10px',
                      fontSize: '12px',
                    }}
                  />
                </div>
                <textarea
                  data-testid="skill-md-input"
                  value={skillMarkdown}
                  onChange={(e) => setSkillMarkdown(e.target.value)}
                  rows={7}
                  style={{
                    width: '100%',
                    background: '#0e0f14',
                    color: '#d1d5db',
                    border: '1px solid #2e3042',
                    borderRadius: '6px',
                    padding: '8px',
                    fontFamily: 'monospace',
                    fontSize: '12px',
                    boxSizing: 'border-box',
                  }}
                />
                <div style={{ marginTop: '8px', display: 'flex', justifyContent: 'flex-end' }}>
                  <button
                    data-testid="publish-skill-btn"
                    disabled={saving}
                    onClick={handlePublish}
                    style={{
                      background: '#276ef1',
                      color: '#fff',
                      border: 'none',
                      borderRadius: '6px',
                      padding: '7px 14px',
                      fontSize: '12px',
                      fontWeight: 600,
                      cursor: 'pointer',
                    }}
                  >
                    {saving ? 'Publishing...' : 'Publish Skill'}
                  </button>
                </div>
              </div>
            </div>
          </div>
        ) : (
          <div
            data-testid="plugin-center-panel"
            style={{
              flex: 1,
              padding: '16px 20px',
              overflowY: 'auto',
              display: 'flex',
              flexDirection: 'column',
              gap: '14px',
              minHeight: '420px',
            }}
          >
            {/* Controls Row */}
            <div
              style={{
                display: 'grid',
                gridTemplateColumns: 'repeat(2, minmax(0, 1fr))',
                gap: '12px',
                background: '#181a24',
                border: '1px solid #282a3b',
                borderRadius: '8px',
                padding: '14px',
              }}
            >
              <div>
                <label style={{ display: 'block', fontSize: '11px', color: '#9ca3af', marginBottom: '4px' }}>
                  Coding Agent Target
                </label>
                <select
                  data-testid="plugin-agent-select"
                  value={pluginAgent}
                  onChange={(e) => setPluginAgent(e.target.value)}
                  style={{
                    width: '100%',
                    background: '#0e0f14',
                    color: '#fff',
                    border: '1px solid #2e3042',
                    borderRadius: '6px',
                    padding: '7px 10px',
                    fontSize: '12px',
                  }}
                >
                  {AGENT_OPTIONS.map((a) => (
                    <option key={a.id} value={a.id}>
                      {a.label}
                    </option>
                  ))}
                </select>
              </div>

              <div>
                <label style={{ display: 'block', fontSize: '11px', color: '#9ca3af', marginBottom: '4px' }}>
                  Distribution Mode
                </label>
                <div style={{ display: 'flex', gap: '8px' }}>
                  <button
                    data-testid="plugin-mode-standard"
                    onClick={() => setPluginMode('standard')}
                    style={{
                      flex: 1,
                      background: pluginMode === 'standard' ? '#1b382b' : '#0e0f14',
                      color: pluginMode === 'standard' ? '#00e575' : '#9ca3af',
                      border: pluginMode === 'standard' ? '1px solid #00e575' : '1px solid #2e3042',
                      borderRadius: '6px',
                      padding: '7px 10px',
                      fontSize: '12px',
                      fontWeight: 600,
                      cursor: 'pointer',
                    }}
                  >
                    Standard Local (stdio)
                  </button>
                  <button
                    data-testid="plugin-mode-enterprise"
                    onClick={() => setPluginMode('enterprise')}
                    style={{
                      flex: 1,
                      background: pluginMode === 'enterprise' ? '#1e2a4a' : '#0e0f14',
                      color: pluginMode === 'enterprise' ? '#60a5fa' : '#9ca3af',
                      border: pluginMode === 'enterprise' ? '1px solid #3b82f6' : '1px solid #2e3042',
                      borderRadius: '6px',
                      padding: '7px 10px',
                      fontSize: '12px',
                      fontWeight: 600,
                      cursor: 'pointer',
                    }}
                  >
                    Enterprise Governed Remote
                  </button>
                </div>
              </div>

              <div>
                <label style={{ display: 'block', fontSize: '11px', color: '#9ca3af', marginBottom: '4px' }}>
                  Tool Group Scope (Optional Isolation)
                </label>
                <select
                  data-testid="plugin-group-select"
                  value={pluginGroup}
                  onChange={(e) => setPluginGroup(e.target.value)}
                  style={{
                    width: '100%',
                    background: '#0e0f14',
                    color: '#fff',
                    border: '1px solid #2e3042',
                    borderRadius: '6px',
                    padding: '7px 10px',
                    fontSize: '12px',
                  }}
                >
                  <option value="">All Gateway Tools (/omni/mcp)</option>
                  {toolGroups.map((g) => (
                    <option key={g.slug} value={g.slug}>
                      {g.display_name || g.slug} (/groups/{g.slug}/mcp)
                    </option>
                  ))}
                </select>
              </div>

              <div>
                <label style={{ display: 'block', fontSize: '11px', color: '#9ca3af', marginBottom: '4px' }}>
                  Auth Token Environment Variable (Never Literal Secret)
                </label>
                <input
                  data-testid="plugin-auth-env-input"
                  value={pluginAuthEnv}
                  onChange={(e) => setPluginAuthEnv(e.target.value)}
                  placeholder="OMNI_AUTH_TOKEN"
                  style={{
                    width: '100%',
                    background: '#0e0f14',
                    color: '#fff',
                    border: '1px solid #2e3042',
                    borderRadius: '6px',
                    padding: '7px 10px',
                    fontSize: '12px',
                    boxSizing: 'border-box',
                  }}
                />
              </div>

              {pluginMode === 'enterprise' && (
                <div style={{ gridColumn: '1 / -1' }}>
                  <label style={{ display: 'block', fontSize: '11px', color: '#9ca3af', marginBottom: '4px' }}>
                    Enterprise Gateway HTTPS URL
                  </label>
                  <input
                    data-testid="plugin-gateway-url-input"
                    value={pluginGatewayUrl}
                    onChange={(e) => setPluginGatewayUrl(e.target.value)}
                    placeholder="https://mcp.corp.example.com"
                    style={{
                      width: '100%',
                      background: '#0e0f14',
                      color: '#fff',
                      border: '1px solid #2e3042',
                      borderRadius: '6px',
                      padding: '7px 10px',
                      fontSize: '12px',
                      boxSizing: 'border-box',
                    }}
                  />
                </div>
              )}
            </div>

            {pluginLoading && (
              <div style={{ fontSize: '12px', color: '#9ca3af' }}>Generating plugin manifest...</div>
            )}

            {pluginManifest && (
              <>
                <div style={{ background: '#181a24', border: '1px solid #282a3b', borderRadius: '8px', padding: '12px' }}>
                  <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: '6px' }}>
                    <div style={{ fontSize: '12px', fontWeight: 600 }}>
                      Client Configuration ({pluginManifest.config_filename} · {pluginManifest.config_format.toUpperCase()})
                    </div>
                    <button
                      onClick={() => handleCopy('config', pluginManifest.config_snippet)}
                      style={{
                        background: '#222434',
                        color: '#e5e7eb',
                        border: '1px solid #35384d',
                        borderRadius: '5px',
                        padding: '3px 8px',
                        fontSize: '11px',
                        cursor: 'pointer',
                      }}
                    >
                      {copiedKey === 'config' ? '✓ Copied' : 'Copy Config'}
                    </button>
                  </div>
                  <pre
                    data-testid="plugin-config-snippet"
                    style={{
                      margin: 0,
                      background: '#0e0f14',
                      border: '1px solid #262838',
                      borderRadius: '6px',
                      padding: '10px',
                      fontSize: '12px',
                      fontFamily: 'monospace',
                      color: '#93c5fd',
                      overflowX: 'auto',
                    }}
                  >
                    {pluginManifest.config_snippet}
                  </pre>
                </div>

                <div style={{ background: '#181a24', border: '1px solid #282a3b', borderRadius: '8px', padding: '12px', display: 'flex', flexDirection: 'column', gap: '8px' }}>
                  <div style={{ fontSize: '12px', fontWeight: 600 }}>1-Line CLI Commands</div>
                  <div style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
                    <code
                      data-testid="plugin-install-cmd"
                      style={{
                        flex: 1,
                        background: '#0e0f14',
                        border: '1px solid #262838',
                        borderRadius: '6px',
                        padding: '7px 10px',
                        fontSize: '12px',
                        color: '#6ee7b7',
                      }}
                    >
                      {pluginManifest.install_skills_cmd}
                    </code>
                    <button
                      onClick={() => handleCopy('skills', pluginManifest.install_skills_cmd)}
                      style={{
                        background: '#222434',
                        color: '#e5e7eb',
                        border: '1px solid #35384d',
                        borderRadius: '5px',
                        padding: '5px 8px',
                        fontSize: '11px',
                        cursor: 'pointer',
                      }}
                    >
                      {copiedKey === 'skills' ? '✓' : 'Copy'}
                    </button>
                  </div>
                  <div style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
                    <code
                      data-testid="plugin-init-cmd"
                      style={{
                        flex: 1,
                        background: '#0e0f14',
                        border: '1px solid #262838',
                        borderRadius: '6px',
                        padding: '7px 10px',
                        fontSize: '12px',
                        color: '#fcd34d',
                      }}
                    >
                      {pluginManifest.init_plugin_cmd}
                    </code>
                    <button
                      onClick={() => handleCopy('init', pluginManifest.init_plugin_cmd)}
                      style={{
                        background: '#222434',
                        color: '#e5e7eb',
                        border: '1px solid #35384d',
                        borderRadius: '5px',
                        padding: '5px 8px',
                        fontSize: '11px',
                        cursor: 'pointer',
                      }}
                    >
                      {copiedKey === 'init' ? '✓' : 'Copy'}
                    </button>
                  </div>
                </div>
              </>
            )}
          </div>
        )}
      </div>
    </div>
  );
};
