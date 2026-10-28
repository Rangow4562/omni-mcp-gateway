//! Omni MCP (`omni_mcp`) - High-Performance Rust Enterprise MCP Management Platform & Autonomous Runtime Gateway.
//!
//! Architecture Layers:
//! - `models`: Domain entities (`ServerRecord`, `ToolRecord`, `CharterPolicy`, `ConfigDiff`), `-320xx` error contracts (`FeatureStatus`), and JSON-RPC 2.0 / MCP primitives.
//! - `storage`: Durable Write-Ahead Log (WAL) + Atomic Snapshot Storage Engine (`WalRegistryStore`) with FNV-1a checksum verification.
//! - `control_plane`: `MCPRegistry`, `AutoCrawler` (in-process IDL & heartbeat discovery engine), `IDLParser` (Protobuf, Thrift, OpenAPI), `ToolDescriptionGenerator`, `MCPFxHeartbeatMonitor`, `ThirdPartyAuthExchange`.
//! - `data_plane`: `InMemoryConfigCache` (CoW `Arc` table), `MCPProxyGateway`, `ProtocolTranslator` (in-memory HTTP/gRPC/TChannel envelope framing), `MuttleyMeshClient` (in-memory service mesh sidecar adapter & mock responder), `CircuitBreakerRegistry`, `SemanticResponseCache`, `RateLimiter`, `SecurityCharterEngine`, `PIIRedactor`, `ResponseProjectionEngine`.
//! - `extension`: `OmniMCPServer` (Okapi BM25 incremental discovery proxy) and `AifxCLI` (Code Mode CLI + benchmark).
//! - `server`: Multi-threaded keep-alive HTTP/1.1 & JSON-RPC 2.0 gateway server.

#[macro_use]
pub mod json;
pub mod config;
pub mod control_plane;
pub mod data_plane;
pub mod extension;
pub mod logging;
pub mod models;
pub mod server;
pub mod storage;
