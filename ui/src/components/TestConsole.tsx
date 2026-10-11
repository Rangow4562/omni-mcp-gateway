import React, { useState } from 'react';
import { ToolItem } from '../types';

interface TestConsoleProps {
  serverName: string;
  toolName: string;
  tool: ToolItem;
  callerActor: string;
  isOpen: boolean;
  onClose: () => void;
}

export const TestConsole: React.FC<TestConsoleProps> = ({
  serverName,
  toolName,
  tool,
  callerActor,
  isOpen,
  onClose,
}) => {
  if (!isOpen) return null;

  const [argsJson, setArgsJson] = useState('{\n  "driver_id": "drv_10284",\n  "include_telemetry": true\n}');
  const [projection, setProjection] = useState('');
  const [executing, setExecuting] = useState(false);
  const [response, setResponse] = useState<any>(null);
  const [errorMsg, setErrorMsg] = useState<string | null>(null);

  const handleExecute = async () => {
    setExecuting(true);
    setErrorMsg(null);
    setResponse(null);

    try {
      let parsedArgs: any = {};
      try {
        parsedArgs = JSON.parse(argsJson);
      } catch (err: any) {
        setErrorMsg(`JSON parse error in arguments: ${err.message}`);
        setExecuting(false);
        return;
      }

      if (projection.trim()) {
        parsedArgs._projection = projection.split(',').map((p) => p.trim()).filter(Boolean);
      }

      const res = await fetch(`http://localhost:8080/${serverName}/mcp`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          'X-Caller-Actor': callerActor,
          'Authorization': 'Bearer uber_emp_demo_token',
        },
        body: JSON.stringify({
          jsonrpc: '2.0',
          id: Date.now(),
          method: 'tools/call',
          params: {
            name: toolName,
            arguments: parsedArgs,
          },
        }),
      });

      const data = await res.json();
      setResponse(data);
    } catch (err: any) {
      setErrorMsg(`Network or gateway error: ${err.message}`);
    } finally {
      setExecuting(false);
    }
  };

  return (
    <div style={{
      position: 'fixed',
      top: 0,
      left: 0,
      right: 0,
      bottom: 0,
      backgroundColor: 'rgba(0, 0, 0, 0.8)',
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
        maxWidth: '820px',
        maxHeight: '85vh',
        display: 'flex',
        flexDirection: 'column',
      }}>
        <div style={{ padding: '16px 20px', borderBottom: '1px solid #202230', display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
          <div>
            <h3 style={{ margin: 0, fontSize: '17px', color: '#fff' }}>
              Test Sandbox: <code>{serverName}</code> / <strong>{toolName}</strong>
            </h3>
            <div style={{ fontSize: '12px', color: '#888', marginTop: '3px' }}>
              Caller Actor: <span style={{ color: '#00e575' }}>{callerActor}</span> · Endpoint: <code>/{serverName}/mcp</code>
            </div>
          </div>
          <button onClick={onClose} style={{ background: 'transparent', border: 'none', color: '#aaa', fontSize: '20px', cursor: 'pointer' }}>
            ✕
          </button>
        </div>

        <div style={{ flex: 1, overflowY: 'auto', padding: '20px' }}>
          <div style={{ marginBottom: '14px' }}>
            <label style={{ display: 'block', fontSize: '12px', color: '#aaa', marginBottom: '6px', fontWeight: 600 }}>
              INPUT ARGUMENTS (JSON)
            </label>
            <textarea
              value={argsJson}
              onChange={(e) => setArgsJson(e.target.value)}
              style={{
                width: '100%',
                height: '110px',
                background: '#0a0a0f',
                border: '1px solid #262838',
                color: '#fff',
                borderRadius: '6px',
                padding: '10px',
                fontFamily: 'monospace',
                fontSize: '12px',
                boxSizing: 'border-box',
              }}
            />
          </div>

          <div style={{ marginBottom: '18px' }}>
            <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: '6px' }}>
              <label style={{ fontSize: '12px', color: '#00e575', fontWeight: 600 }}>
                RESPONSE PROJECTION (GraphQL-like field filter)
              </label>
              <span style={{ fontSize: '11px', color: '#777' }}>Comma-separated paths (e.g. data.driver.name, data.driver.rating)</span>
            </div>
            <input
              type="text"
              placeholder="e.g. data.driver.name, data.driver.vehicle.license_plate"
              value={projection}
              onChange={(e) => setProjection(e.target.value)}
              style={{
                width: '100%',
                background: '#0a0a0f',
                border: '1px solid #262838',
                color: '#fff',
                borderRadius: '6px',
                padding: '8px 12px',
                fontSize: '12px',
                boxSizing: 'border-box',
              }}
            />
          </div>

          <button
            onClick={handleExecute}
            disabled={executing}
            style={{
              width: '100%',
              backgroundColor: '#276ef1',
              color: '#fff',
              border: 'none',
              padding: '10px',
              borderRadius: '6px',
              fontWeight: 700,
              cursor: 'pointer',
              marginBottom: '16px',
            }}
          >
            {executing ? 'Executing via Muttley Sidecar...' : 'Execute Tool Call'}
          </button>

          {errorMsg && (
            <div style={{ background: '#3b1219', border: '1px solid #ff4d4f', padding: '12px', borderRadius: '6px', color: '#ff7875', fontSize: '13px', marginBottom: '16px' }}>
              {errorMsg}
            </div>
          )}

          {response && (
            <div>
              <div style={{ fontSize: '12px', color: '#aaa', fontWeight: 600, marginBottom: '6px' }}>
                GATEWAY MCP RESPONSE PAYLOAD:
              </div>
              <pre style={{
                margin: 0,
                background: '#07070a',
                border: '1px solid #1a1c27',
                padding: '14px',
                borderRadius: '6px',
                fontSize: '12px',
                color: response.error ? '#ff7875' : '#73e6a0',
                maxHeight: '220px',
                overflowY: 'auto',
              }}>
                {JSON.stringify(response, null, 2)}
              </pre>
            </div>
          )}
        </div>
      </div>
    </div>
  );
};

