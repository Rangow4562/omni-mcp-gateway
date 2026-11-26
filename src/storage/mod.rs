pub mod audit_log;
pub mod file_store;
#[cfg(feature = "storage-postgres")]
pub mod postgres_store;
pub mod wal_store;

pub use audit_log::{
    sha256, sha256_hex, AuditSink, MerkleAuditRecord, MerkleAuditStore, GENESIS_HASH,
};
pub use file_store::{
    AzureBlobConfig, AzureFileBlobBackend, FileBlobBackend, FileBlobStore, GcsBlobConfig,
    GcsFileBlobBackend, HttpBlobConfig, HttpFileBlobBackend, LocalFileBlobBackend,
    PostgresFileBlobBackend, S3BlobConfig, S3FileBlobBackend,
};
#[cfg(feature = "storage-postgres")]
pub use postgres_store::{PgDsn, PgWireConnection, PostgresRegistryBackend};
pub use wal_store::{RegistryStorageBackend, WalRegistryStore};
#[cfg(feature = "storage-sqlite")]
pub use wal_store::{SqliteAuditSink, SqliteRegistryBackend, SqliteRegistryStore};
