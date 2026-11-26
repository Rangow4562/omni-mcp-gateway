import React, { useState, useEffect, useCallback } from 'react';
import { SkillRecordItem } from '../types';
import { fetchSkills, saveSkill, deleteSkill } from '../api';

interface SkillsStudioModalProps {
  isOpen: boolean;
  onClose: () => void;
  sseRevision?: number;
}

export const SkillsStudioModal: React.FC<SkillsStudioModalProps> = ({
  isOpen,
  onClose,
  sseRevision = 0,
}) => {
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
    '---\nname: Enterprise Triage Playbook\ndescription: Governed incident triage workflow over OmniMCP Code Mode\n---\n\n# Enterprise Triage Playbook\n\nUse `omni.discover_tools` before mutating production state.\n'
  );
  const [saving, setSaving] = useState(false);

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

  useEffect(() => {
    if (isOpen) {
      void loadSkills();
    }
  }, [isOpen, sseRevision, loadSkills]);

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
          width: '960px',
          maxWidth: '95vw',
          maxHeight: '88vh',
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
              Deterministic SHA-256 skill bundles & auto-synthesized Tool Group Code Mode playbooks
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
      </div>
    </div>
  );
};
