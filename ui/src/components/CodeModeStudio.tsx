import React, { useState, useEffect } from 'react';
import { apiFetch } from '../api';

interface CodeModeStudioProps {
  isOpen: boolean;
  onClose: () => void;
}

export const CodeModeStudio: React.FC<CodeModeStudioProps> = ({ isOpen, onClose }) => {
  const [dtsText, setDtsText] = useState<string>('// Loading TypeScript declarations...');
  const [script, setScript] = useState<string>(
    'async () => {\n  const rides = await codemode.rides_service.list_recent({ user_id: "usr_456" });\n  console.log("Loaded rides", rides);\n  return rides;\n}'
  );
  const [execResult, setExecResult] = useState<any>(null);
  const [running, setRunning] = useState(false);

  useEffect(() => {
    if (!isOpen) return;
    (async () => {
      try {
        const res = await apiFetch<any>('/omni/mcp', {
          method: 'POST',
          body: JSON.stringify({
            jsonrpc: '2.0',
            id: 1,
            method: 'resources/read',
            params: { uri: 'omni://codemode/types.d.ts' },
          }),
        });
        const text = res?.result?.contents?.[0]?.text;
        if (text) setDtsText(text);
      } catch (e: any) {
        setDtsText(`// Failed to load types.d.ts: ${e?.message || 'unknown error'}`);
      }
    })();
  }, [isOpen]);

  if (!isOpen) return null;

  const handleExecute = async () => {
    setRunning(true);
    try {
      const res = await apiFetch<any>('/omni/mcp', {
        method: 'POST',
        body: JSON.stringify({
          jsonrpc: '2.0',
          id: 2,
          method: 'tools/call',
          params: {
            name: 'execute_codemode',
            arguments: { script, require_mutation_approval: true },
          },
        }),
      });
      setExecResult(res?.result || res);
    } catch (e: any) {
      setExecResult({ error: e?.message || 'Execution failed' });
    } finally {
      setRunning(false);
    }
  };

  return (
    <div style={{
      position: 'fixed', inset: 0, backgroundColor: 'rgba(0,0,0,0.75)',
      display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 220, padding: '20px',
    }}>
      <div style={{
        background: '#12131b', border: '1px solid #282a3b', borderRadius: '10px',
        width: '100%', maxWidth: '1040px', maxHeight: '88vh', display: 'flex', flexDirection: 'column',
      }}>
        <div style={{
          padding: '16px 22px', borderBottom: '1px solid #202230',
          display: 'flex', justifyContent: 'space-between', alignItems: 'center',
        }}>
          <div>
            <h3 style={{ margin: 0, fontSize: '18px', color: '#fff' }}>Code Mode Studio</h3>
            <div style={{ fontSize: '12px', color: '#888', marginTop: '3px' }}>
              Deep <code>.d.ts</code> IntelliSense Inspector, SSA Parallel-Wave Sandbox &amp; Execution Telemetry
            </div>
          </div>
          <button onClick={onClose} style={{ background: 'transparent', border: 'none', color: '#aaa', fontSize: '20px', cursor: 'pointer' }}>✕</button>
        </div>

        <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: '16px', padding: '18px', overflowY: 'auto' }}>
          <div>
            <div style={{ fontSize: '12px', fontWeight: 700, color: '#9ca3af', marginBottom: '6px' }}>
              Generated TypeScript Declarations (<code>omni://codemode/types.d.ts</code>)
            </div>
            <pre style={{
              background: '#0b0c12', border: '1px solid #242638', borderRadius: '6px',
              padding: '12px', color: '#a5d6ff', fontSize: '11px', height: '380px', overflow: 'auto', margin: 0,
            }}>
              {dtsText}
            </pre>
          </div>

          <div style={{ display: 'flex', flexDirection: 'column', gap: '10px' }}>
            <div style={{ fontSize: '12px', fontWeight: 700, color: '#9ca3af' }}>
              Sandbox Script Runner (AST + SSA Auto-Parallelizer)
            </div>
            <textarea
              value={script}
              onChange={(e) => setScript(e.target.value)}
              rows={9}
              style={{
                background: '#0b0c12', border: '1px solid #242638', borderRadius: '6px',
                padding: '10px', color: '#e5e7eb', fontFamily: 'monospace', fontSize: '12px',
              }}
            />
            <button
              disabled={running}
              onClick={handleExecute}
              style={{
                background: '#00e575', color: '#000', border: 'none', borderRadius: '6px',
                padding: '8px 14px', fontSize: '13px', fontWeight: 700, cursor: 'pointer',
              }}
            >
              {running ? 'Running in Rust AST VM...' : 'Run Code Mode Script'}
            </button>
            {execResult && (
              <pre style={{
                background: '#0b0c12', border: '1px solid #242638', borderRadius: '6px',
                padding: '10px', color: '#7ee787', fontSize: '11px', maxHeight: '180px', overflow: 'auto', margin: 0,
              }}>
                {JSON.stringify(execResult, null, 2)}
              </pre>
            )}
          </div>
        </div>
      </div>
    </div>
  );
};
