import React, { useState, useEffect, useCallback } from 'react';
import { TraceSpanItem, AuditRecordItem, AuditVerifyResult, OtlpStatusResponse } from '../types';
import { fetchTraces, fetchAuditLog, verifyAuditChain, fetchOtlpStatus } from '../api';

interface ObservabilityStudioProps {
  isOpen: boolean;
  onClose: () => void;
}

export const ObservabilityStudio: React.FC<ObservabilityStudioProps> = ({ isOpen, onClose }) => {
  const [activeTab, setActiveTab] = useState<'traces' | 'audit'>('traces');
  const [spans, setSpans] = useState<TraceSpanItem[]>([]);
  const [auditRecords, setAuditRecords] = useState<AuditRecordItem[]>([]);
  const [headHash, setHeadHash] = useState<string>('');
  const [verifyResult, setVerifyResult] = useState<AuditVerifyResult | null>(null);
  const [otlpStatus, setOtlpStatus] = useState<OtlpStatusResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [verifying, setVerifying] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const loadData = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [traceList, auditData, otlp] = await Promise.all([
        fetchTraces(),
        fetchAuditLog(),
        fetchOtlpStatus().catch(() => null),
      ]);
      setSpans(traceList);
      setAuditRecords(auditData.records || []);
      if (auditData.head_hash) setHeadHash(auditData.head_hash);
      setOtlpStatus(otlp);
    } catch (e: any) {
      setError(e?.message || 'Failed to load observability telemetry (Operator/Admin role required)');
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (isOpen) {
      loadData();
    }
  }, [isOpen, loadData]);

  if (!isOpen) return null;

  const handleVerifyChain = async () => {
    setVerifying(true);
    setError(null);
    try {
      const res = await verifyAuditChain();
      setVerifyResult(res);
    } catch (e: any) {
      setVerifyResult({
        verified: false,
        reason: e?.message || 'Verification failed',
      });
    } finally {
      setVerifying(false);
    }
  };

  const maxDurationUs = Math.max(1, ...spans.map((s) => s.durationUs || 1));

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
        data-testid="observability-studio-modal"
        style={{
          backgroundColor: '#12131b',
          border: '1px solid #2b2d3f',
          borderRadius: '10px',
          width: '960px',
          maxWidth: '95vw',
          maxHeight: '88vh',
          display: 'flex',
          flexDirection: 'column',
          overflow: 'hidden',
        }}
      >
        {/* Header */}
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
              📊 Observability &amp; Compliance Studio
            </h2>
            <div style={{ fontSize: '12px', color: '#8b8fa8', marginTop: '2px' }}>
              W3C Distributed Trace Waterfall, OTLP Batch Exporter &amp; Cryptographic SHA-256 Merkle Ledger
            </div>
          </div>
          <div style={{ display: 'flex', gap: '8px', alignItems: 'center' }}>
            <button
              onClick={loadData}
              style={{
                background: '#1e2030',
                border: '1px solid #30334a',
                color: '#c9d1d9',
                padding: '5px 10px',
                borderRadius: '6px',
                fontSize: '12px',
                cursor: 'pointer',
              }}
            >
              🔄 Refresh
            </button>
            <button
              onClick={onClose}
              aria-label="Close Observability Studio"
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
        </div>

        {/* Tabs & OTLP Status Bar */}
        <div
          style={{
            display: 'flex',
            justifyContent: 'space-between',
            alignItems: 'center',
            padding: '10px 20px',
            borderBottom: '1px solid #202232',
            backgroundColor: '#0d0e15',
          }}
        >
          <div style={{ display: 'flex', gap: '8px' }}>
            <button
              onClick={() => setActiveTab('traces')}
              style={{
                background: activeTab === 'traces' ? '#276ef1' : '#1b1d2b',
                color: '#fff',
                border: '1px solid #2e324a',
                padding: '6px 14px',
                borderRadius: '6px',
                fontSize: '12px',
                fontWeight: 600,
                cursor: 'pointer',
              }}
            >
              🌊 W3C Trace Waterfall ({spans.length})
            </button>
            <button
              onClick={() => setActiveTab('audit')}
              style={{
                background: activeTab === 'audit' ? '#276ef1' : '#1b1d2b',
                color: '#fff',
                border: '1px solid #2e324a',
                padding: '6px 14px',
                borderRadius: '6px',
                fontSize: '12px',
                fontWeight: 600,
                cursor: 'pointer',
              }}
            >
              🔐 Merkle Audit Explorer ({auditRecords.length})
            </button>
          </div>

          {otlpStatus && (
            <div style={{ display: 'flex', gap: '12px', fontSize: '11px', color: '#9ea3bd' }}>
              <span>
                OTLP Worker:{' '}
                <strong style={{ color: otlpStatus.running ? '#00e575' : '#8b8fa8' }}>
                  {otlpStatus.running ? 'ACTIVE' : 'STANDBY'}
                </strong>
              </span>
              {otlpStatus.latency_quantiles_us && (
                <span>
                  p50/p95/p99:{' '}
                  <strong style={{ color: '#79c0ff' }}>
                    {otlpStatus.latency_quantiles_us.p50}µs / {otlpStatus.latency_quantiles_us.p95}µs /{' '}
                    {otlpStatus.latency_quantiles_us.p99}µs
                  </strong>
                </span>
              )}
              {otlpStatus.storage_backend && (
                <span>
                  Store: <strong style={{ color: '#d2a8ff' }}>{otlpStatus.storage_backend}</strong>
                </span>
              )}
            </div>
          )}
        </div>

        {/* Content */}
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

          {loading ? (
            <div style={{ color: '#8b8fa8', padding: '24px', textAlign: 'center', fontSize: '13px' }}>
              Loading telemetry...
            </div>
          ) : activeTab === 'traces' ? (
            spans.length === 0 ? (
              <div
                style={{
                  color: '#8b8fa8',
                  padding: '32px',
                  textAlign: 'center',
                  border: '1px dashed #2a2d40',
                  borderRadius: '8px',
                  fontSize: '13px',
                }}
              >
                No W3C trace spans recorded yet. Invoke any tool or Code Mode script to populate the waterfall.
              </div>
            ) : (
              <div style={{ display: 'flex', flexDirection: 'column', gap: '8px' }}>
                {spans
                  .slice(-50)
                  .reverse()
                  .map((span) => {
                    const isOk = span.status?.message === 'OK' || span.status?.code === 1;
                    const widthPct = Math.max(4, Math.min(100, ((span.durationUs || 1) / maxDurationUs) * 100));
                    return (
                      <div
                        key={span.spanId}
                        style={{
                          backgroundColor: '#0c0d13',
                          border: '1px solid #222536',
                          borderRadius: '6px',
                          padding: '10px 12px',
                          display: 'grid',
                          gridTemplateColumns: '240px 1fr 110px',
                          gap: '12px',
                          alignItems: 'center',
                        }}
                      >
                        <div>
                          <div style={{ fontSize: '12px', fontWeight: 600, color: '#fff' }}>
                            {span.serviceName}.{span.toolName}
                          </div>
                          <div style={{ fontSize: '10px', color: '#7c819a', fontFamily: 'monospace' }}>
                            trace:{span.traceId.slice(0, 12)} span:{span.spanId.slice(0, 8)}
                          </div>
                        </div>

                        <div
                          style={{
                            backgroundColor: '#161824',
                            height: '16px',
                            borderRadius: '4px',
                            overflow: 'hidden',
                            position: 'relative',
                          }}
                        >
                          <div
                            style={{
                              width: `${widthPct}%`,
                              height: '100%',
                              backgroundColor: isOk ? '#276ef1' : '#e05252',
                              borderRadius: '4px',
                            }}
                          />
                        </div>

                        <div style={{ textAlign: 'right', fontSize: '11px' }}>
                          <span
                            style={{
                              backgroundColor: isOk ? '#123325' : '#3d1616',
                              color: isOk ? '#00e575' : '#ff8080',
                              padding: '2px 6px',
                              borderRadius: '4px',
                              fontWeight: 700,
                              marginRight: '6px',
                            }}
                          >
                            {span.status?.message || 'OK'}
                          </span>
                          <span style={{ color: '#9ea3bd', fontFamily: 'monospace' }}>
                            {span.durationUs}µs
                          </span>
                        </div>
                      </div>
                    );
                  })}
              </div>
            )
          ) : (
            <div>
              <div
                style={{
                  display: 'flex',
                  justifyContent: 'space-between',
                  alignItems: 'center',
                  marginBottom: '12px',
                  backgroundColor: '#0c0d13',
                  border: '1px solid #242739',
                  padding: '12px 14px',
                  borderRadius: '8px',
                }}
              >
                <div style={{ fontSize: '12px', color: '#9ea3bd' }}>
                  <div>
                    SHA-256 Head Hash:{' '}
                    <code style={{ color: '#79c0ff' }}>{headHash || 'GENESIS'}</code>
                  </div>
                  {verifyResult && (
                    <div
                      data-testid="merkle-verify-badge"
                      style={{
                        marginTop: '6px',
                        color: verifyResult.verified ? '#00e575' : '#ff8080',
                        fontWeight: 700,
                      }}
                    >
                      {verifyResult.verified
                        ? `✓ Merkle Chain Verified (${verifyResult.records_checked ?? 0} records intact, ${verifyResult.algorithm || 'SHA-256'})`
                        : `✕ Chain Integrity Failure: ${verifyResult.reason || 'Hash mismatch'}`}
                    </div>
                  )}
                </div>
                <button
                  onClick={handleVerifyChain}
                  disabled={verifying}
                  style={{
                    backgroundColor: '#00e575',
                    color: '#000',
                    border: 'none',
                    padding: '8px 14px',
                    borderRadius: '6px',
                    fontWeight: 700,
                    fontSize: '12px',
                    cursor: 'pointer',
                  }}
                >
                  {verifying ? 'Verifying...' : '🔐 Verify Cryptographic Chain'}
                </button>
              </div>

              {auditRecords.length === 0 ? (
                <div
                  style={{
                    color: '#8b8fa8',
                    padding: '28px',
                    textAlign: 'center',
                    border: '1px dashed #2a2d40',
                    borderRadius: '8px',
                    fontSize: '13px',
                  }}
                >
                  No audit records yet.
                </div>
              ) : (
                <table style={{ width: '100%', borderCollapse: 'collapse', fontSize: '11px' }}>
                  <thead>
                    <tr style={{ borderBottom: '1px solid #24273a', color: '#8b8fa8', textAlign: 'left' }}>
                      <th style={{ padding: '8px' }}>#</th>
                      <th style={{ padding: '8px' }}>Actor</th>
                      <th style={{ padding: '8px' }}>Target</th>
                      <th style={{ padding: '8px' }}>Status</th>
                      <th style={{ padding: '8px' }}>Chain Link (prev → current)</th>
                    </tr>
                  </thead>
                  <tbody>
                    {auditRecords
                      .slice(-50)
                      .reverse()
                      .map((rec) => (
                        <tr key={rec.index} style={{ borderBottom: '1px solid #1b1d2b' }}>
                          <td style={{ padding: '8px', color: '#9ea3bd' }}>{rec.index}</td>
                          <td style={{ padding: '8px', color: '#a5d6ff' }}>{rec.actor}</td>
                          <td style={{ padding: '8px', color: '#fff', fontWeight: 600 }}>
                            {rec.server_name}.{rec.tool_name}
                          </td>
                          <td style={{ padding: '8px', color: '#00e575' }}>{rec.status}</td>
                          <td style={{ padding: '8px', fontFamily: 'monospace', color: '#7c819a' }}>
                            {rec.prev_hash.slice(0, 10)}… →{' '}
                            <span style={{ color: '#79c0ff' }}>{rec.current_hash.slice(0, 12)}…</span>
                          </td>
                        </tr>
                      ))}
                  </tbody>
                </table>
              )}
            </div>
          )}
        </div>
      </div>
    </div>
  );
};
