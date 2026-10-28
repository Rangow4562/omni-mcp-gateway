//! In-Memory Config Cache - Copy-on-Write (CoW) `Arc`-swapped routing table refreshed at fixed cadence.
//! Readers acquire an `Arc` clone in nanoseconds without deep copying or blocking concurrent calls.

use crate::control_plane::registry::MCPRegistry;
use crate::logging::now_epoch_secs;
use crate::models::ServerRecord;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

type ServerTable = Arc<BTreeMap<String, Arc<ServerRecord>>>;

#[derive(Clone)]
pub struct InMemoryConfigCache {
    pub registry: MCPRegistry,
    pub refresh_interval_seconds: f64,
    table: Arc<RwLock<ServerTable>>,
    last_refresh_timestamp: Arc<RwLock<f64>>,
    running: Arc<AtomicBool>,
    worker: Arc<RwLock<Option<JoinHandle<()>>>>,
}

impl InMemoryConfigCache {
    pub fn new(registry: MCPRegistry, refresh_interval_seconds: f64) -> Self {
        let cache = Self {
            registry,
            refresh_interval_seconds: refresh_interval_seconds.max(0.05),
            table: Arc::new(RwLock::new(Arc::new(BTreeMap::new()))),
            last_refresh_timestamp: Arc::new(RwLock::new(0.0)),
            running: Arc::new(AtomicBool::new(false)),
            worker: Arc::new(RwLock::new(None)),
        };
        cache.refresh_now();
        cache
    }

    /// Explicitly refreshes the Copy-on-Write routing table from the Control Plane registry.
    pub fn refresh_now(&self) {
        let all_servers = self.registry.list_servers(None);
        let mut new_map = BTreeMap::new();
        for s in all_servers {
            new_map.insert(s.name.clone(), Arc::new(s));
        }
        {
            let mut guard = self.table.write().unwrap();
            *guard = Arc::new(new_map);
        }
        {
            let mut ts = self.last_refresh_timestamp.write().unwrap();
            *ts = now_epoch_secs();
        }
    }

    /// Zero-copy `Arc<ServerRecord>` lookup for high-speed data plane routing.
    pub fn get_server_arc(&self, server_name: &str) -> Option<Arc<ServerRecord>> {
        let table = self.table.read().unwrap().clone();
        table.get(server_name).cloned()
    }

    pub fn get_server(&self, server_name: &str) -> Option<ServerRecord> {
        self.get_server_arc(server_name).map(|arc| (*arc).clone())
    }

    pub fn list_servers(&self) -> Vec<ServerRecord> {
        let table = self.table.read().unwrap().clone();
        table.values().map(|arc| (**arc).clone()).collect()
    }

    pub fn server_count(&self) -> usize {
        self.table.read().unwrap().len()
    }

    pub fn start_background_refresh(&self) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = self.clone();
        let interval = Duration::from_secs_f64(self.refresh_interval_seconds);
        let handle = thread::spawn(move || {
            while this.running.load(Ordering::SeqCst) {
                thread::sleep(interval);
                if !this.running.load(Ordering::SeqCst) {
                    break;
                }
                this.refresh_now();
            }
        });
        *self.worker.write().unwrap() = Some(handle);
    }

    pub fn stop_background_refresh(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}
