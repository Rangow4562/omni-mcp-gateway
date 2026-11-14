pub mod audit_log;
pub mod wal_store;

pub use audit_log::{
    sha256, sha256_hex, AuditSink, MerkleAuditRecord, MerkleAuditStore, GENESIS_HASH,
};
#[cfg(feature = "storage-postgres")]
pub use wal_store::PostgresRegistryBackend;
pub use wal_store::{RegistryStorageBackend, WalRegistryStore};
#[cfg(feature = "storage-sqlite")]
pub use wal_store::{SqliteAuditSink, SqliteRegistryBackend, SqliteRegistryStore};
