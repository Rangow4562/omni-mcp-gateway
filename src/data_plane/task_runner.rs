//! Postgres HA-Backed MCP Tasks Runner (`io.modelcontextprotocol/tasks`, GA, v6.8.0).
//!
//! Features:
//! - Bounded concurrent task execution (`OMNI_MAX_CONCURRENT_TASKS=64`)
//! - Synchronous grace-window promotion (`OMNI_TASK_SYNC_GRACE_MS=250`): fast executions return
//!   immediately; slower executions promote cleanly to `resultType: "task"` backed by `omni_mcp_tasks`
//! - Cooperative cancellation (`cancel_flags` map) woken both locally and cross-pod via
//!   PostgreSQL `LISTEN omni_task_changed`
//! - Group / service scope enforcement on `tasks/get`, `tasks/update`, and `tasks/cancel`
//! - Background 60s TTL reaper + Prometheus metrics (`omni_mcp_tasks_active`, `omni_mcp_tasks_total`).

use crate::control_plane::MCPRegistry;
use crate::json::JsonValue;
use crate::logging::now_epoch_secs;
use crate::models::{generate_ulid, McpTaskRecord, McpTaskStatus};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::Duration;

/// Outcome of submitting a potentially long-running execution to `TaskManager`.
pub enum TaskDispatchOutcome {
    /// Completed within the synchronous grace window; caller can return the result directly.
    CompletedSync(JsonValue),
    /// Exceeded the grace window or was explicitly requested as an async task; promoted to
    /// `resultType: "task"` with a durable `McpTaskRecord`.
    PromotedAsync(Box<McpTaskRecord>),
}

/// Prometheus-compatible task lifecycle metrics.
pub struct TaskMetrics {
    pub active: AtomicUsize,
    pub total_completed: AtomicU64,
    pub total_failed: AtomicU64,
    pub total_cancelled: AtomicU64,
    pub total_input_required: AtomicU64,
}

impl Default for TaskMetrics {
    fn default() -> Self {
        Self {
            active: AtomicUsize::new(0),
            total_completed: AtomicU64::new(0),
            total_failed: AtomicU64::new(0),
            total_cancelled: AtomicU64::new(0),
            total_input_required: AtomicU64::new(0),
        }
    }
}

/// Simple pure-Rust counting semaphore (works identically with `--no-default-features`).
struct CountingSemaphore {
    max: usize,
    current: Mutex<usize>,
    cv: Condvar,
}

impl CountingSemaphore {
    fn new(max: usize) -> Self {
        Self {
            max: max.max(1),
            current: Mutex::new(0),
            cv: Condvar::new(),
        }
    }

    fn try_acquire(&self) -> bool {
        let mut g = self.current.lock().unwrap();
        if *g < self.max {
            *g += 1;
            true
        } else {
            false
        }
    }

    fn release(&self) {
        let mut g = self.current.lock().unwrap();
        *g = g.saturating_sub(1);
        self.cv.notify_one();
    }
}

/// Event callback invoked when a task changes state (wired to `EventBus` for Studio UI SSE).
pub type TaskEventSink = Arc<dyn Fn(&str, &McpTaskRecord) + Send + Sync>;

/// Distributed MCP Task Manager (`v6.8.0`).
#[derive(Clone)]
pub struct TaskManager {
    registry: MCPRegistry,
    pod_id: String,
    grace_ms: u64,
    ttl_sec: u64,
    semaphore: Arc<CountingSemaphore>,
    cancel_flags: Arc<RwLock<HashMap<String, Arc<AtomicBool>>>>,
    pub metrics: Arc<TaskMetrics>,
    event_sink: Arc<RwLock<Option<TaskEventSink>>>,
    reaper_running: Arc<AtomicBool>,
}

impl TaskManager {
    pub fn new(
        registry: MCPRegistry,
        pod_id: impl Into<String>,
        max_concurrent_tasks: usize,
        grace_ms: u64,
        ttl_sec: u64,
    ) -> Self {
        Self {
            registry,
            pod_id: pod_id.into(),
            grace_ms,
            ttl_sec: ttl_sec.max(60),
            semaphore: Arc::new(CountingSemaphore::new(max_concurrent_tasks)),
            cancel_flags: Arc::new(RwLock::new(HashMap::new())),
            metrics: Arc::new(TaskMetrics::default()),
            event_sink: Arc::new(RwLock::new(None)),
            reaper_running: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn set_event_sink(&self, sink: TaskEventSink) {
        if let Ok(mut g) = self.event_sink.write() {
            *g = Some(sink);
        }
    }

    fn emit_event(&self, kind: &str, task: &McpTaskRecord) {
        if let Ok(g) = self.event_sink.read() {
            if let Some(ref sink) = *g {
                sink(kind, task);
            }
        }
    }

    /// Starts the 60-second background TTL reaper thread (idempotent).
    pub fn start_ttl_reaper(&self) {
        if self.reaper_running.swap(true, Ordering::SeqCst) {
            return;
        }
        let reg = self.registry.clone();
        let running = self.reaper_running.clone();
        thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                for _ in 0..60 {
                    if !running.load(Ordering::SeqCst) {
                        return;
                    }
                    thread::sleep(Duration::from_secs(1));
                }
                let _ = reg.purge_expired_mcp_tasks(now_epoch_secs());
            }
        });
    }

    pub fn stop_ttl_reaper(&self) {
        self.reaper_running.store(false, Ordering::SeqCst);
    }

    /// Handles a cross-pod `NOTIFY omni_task_changed` payload without touching the server catalog.
    pub fn handle_remote_task_notification(&self, payload: &str) {
        if let Some(task_id) = payload.strip_prefix("cancel:") {
            if let Ok(guard) = self.cancel_flags.read() {
                if let Some(flag) = guard.get(task_id) {
                    flag.store(true, Ordering::SeqCst);
                }
            }
        }
        // Emit SSE update if we can load the task record
        let task_id = payload
            .strip_prefix("cancel:")
            .or_else(|| {
                payload
                    .strip_prefix("task:")
                    .and_then(|rest| rest.split(':').next())
            })
            .unwrap_or("");
        if !task_id.is_empty() {
            if let Some(task) = self.registry.get_mcp_task(task_id) {
                self.emit_event("mcp.task.updated", &task);
            }
        }
    }

    /// Registers an already-constructed `McpTaskRecord` (e.g., for `input_required` elicitation tasks).
    pub fn record_task(&self, task: &McpTaskRecord) {
        if task.status == McpTaskStatus::InputRequired {
            self.metrics
                .total_input_required
                .fetch_add(1, Ordering::Relaxed);
        }
        self.registry.save_mcp_task(task);
        self.emit_event("mcp.task.updated", task);
    }

    /// Runs `work(cancel_flag)` in a bounded worker with a synchronous grace window (`grace_ms`).
    /// If `force_async` is true or `work` does not finish within `grace_ms`, returns
    /// `TaskDispatchOutcome::PromotedAsync(record)` while the worker continues in the background.
    #[allow(clippy::too_many_arguments)]
    pub fn run_with_grace<F>(
        &self,
        caller_actor: &str,
        group_slug: Option<String>,
        server_scope: Option<String>,
        force_async: bool,
        grace_override_ms: Option<u64>,
        work: F,
    ) -> Result<TaskDispatchOutcome, String>
    where
        F: FnOnce(Arc<AtomicBool>) -> Result<JsonValue, (i32, String)> + Send + 'static,
    {
        if !self.semaphore.try_acquire() {
            return Err(format!(
                "Maximum concurrent MCP tasks ({}) exceeded; retry later",
                self.semaphore.max
            ));
        }

        let task_id = format!("task_{}", generate_ulid());
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let mut initial = McpTaskRecord::new(
            task_id.clone(),
            self.pod_id.clone(),
            caller_actor,
            group_slug,
            server_scope,
            self.ttl_sec,
        );
        initial.status_message = Some("Executing tool request".to_string());
        self.registry.save_mcp_task(&initial);

        {
            let mut flags = self.cancel_flags.write().unwrap();
            flags.insert(task_id.clone(), cancel_flag.clone());
        }
        self.metrics.active.fetch_add(1, Ordering::Relaxed);
        self.emit_event("mcp.task.created", &initial);

        let (tx, rx) = mpsc::channel::<Result<JsonValue, (i32, String)>>();
        let mgr = self.clone();
        let tid = task_id.clone();
        let flag_for_worker = cancel_flag.clone();

        thread::spawn(move || {
            let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::data_plane::translators::with_active_cancel_flag(
                    flag_for_worker.clone(),
                    || work(flag_for_worker.clone()),
                )
            })) {
                Ok(res) => res,
                Err(_) => Err((-32603, "Task worker panicked".to_string())),
            };
            let was_cancelled = flag_for_worker.load(Ordering::SeqCst);
            let mut rec = mgr.registry.get_mcp_task(&tid).unwrap_or(initial);
            rec.updated_at = now_epoch_secs();

            if was_cancelled || rec.cancel_requested {
                rec.status = McpTaskStatus::Cancelled;
                rec.cancel_requested = true;
                rec.status_message = Some("Cancelled cooperatively".to_string());
                mgr.metrics.total_cancelled.fetch_add(1, Ordering::Relaxed);
            } else {
                match &outcome {
                    Ok(val) => {
                        rec.status = McpTaskStatus::Completed;
                        rec.progress_pct = 100;
                        rec.status_message = Some("Completed".to_string());
                        rec.result_payload = Some(val.clone());
                        mgr.metrics.total_completed.fetch_add(1, Ordering::Relaxed);
                    }
                    Err((code, msg)) => {
                        rec.status = McpTaskStatus::Failed;
                        rec.status_message = Some(msg.clone());
                        rec.error_payload = Some(json!({
                            "code": *code,
                            "message": msg.clone()
                        }));
                        mgr.metrics.total_failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }

            mgr.registry.save_mcp_task(&rec);
            mgr.emit_event("mcp.task.updated", &rec);
            if let Ok(mut flags) = mgr.cancel_flags.write() {
                flags.remove(&tid);
            }
            mgr.metrics.active.fetch_sub(1, Ordering::Relaxed);
            mgr.semaphore.release();
            let _ = tx.send(outcome);
        });

        let wait_ms = if force_async {
            0
        } else {
            grace_override_ms.unwrap_or(self.grace_ms)
        };

        if wait_ms > 0 {
            match rx.recv_timeout(Duration::from_millis(wait_ms)) {
                Ok(Ok(val)) => return Ok(TaskDispatchOutcome::CompletedSync(val)),
                Ok(Err((_code, msg))) => return Err(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {}
            }
        }

        let current = self.registry.get_mcp_task(&task_id).unwrap_or_else(|| {
            McpTaskRecord::new(
                task_id,
                &self.pod_id,
                caller_actor,
                None,
                None,
                self.ttl_sec,
            )
        });
        Ok(TaskDispatchOutcome::PromotedAsync(Box::new(current)))
    }

    /// Checks whether a task is authorized to be accessed from the given `(group_slug, server_scope)`.
    pub fn check_scope(
        task: &McpTaskRecord,
        req_group_slug: Option<&str>,
        req_server_scope: Option<&str>,
    ) -> Result<(), String> {
        if let Some(req_grp) = req_group_slug {
            if task.group_slug.as_deref() != Some(req_grp) {
                return Err(format!(
                    "Task '{}' does not belong to tool group '{}'",
                    task.task_id, req_grp
                ));
            }
        }
        if let Some(req_srv) = req_server_scope {
            if task.server_scope.as_deref() != Some(req_srv) {
                return Err(format!(
                    "Task '{}' does not belong to service '{}'",
                    task.task_id, req_srv
                ));
            }
        }
        Ok(())
    }

    pub fn get_task_scoped(
        &self,
        task_id: &str,
        req_group_slug: Option<&str>,
        req_server_scope: Option<&str>,
    ) -> Result<McpTaskRecord, String> {
        let task = self
            .registry
            .get_mcp_task(task_id)
            .ok_or_else(|| format!("Task '{}' not found", task_id))?;
        Self::check_scope(&task, req_group_slug, req_server_scope)?;
        Ok(task)
    }

    pub fn list_tasks(&self) -> Vec<McpTaskRecord> {
        self.registry.list_mcp_tasks()
    }

    /// Requests cooperative cancellation of `task_id` both locally and across replicas.
    pub fn cancel_task_scoped(
        &self,
        task_id: &str,
        req_group_slug: Option<&str>,
        req_server_scope: Option<&str>,
    ) -> Result<McpTaskRecord, String> {
        let existing = self.get_task_scoped(task_id, req_group_slug, req_server_scope)?;
        if let Ok(flags) = self.cancel_flags.read() {
            if let Some(flag) = flags.get(task_id) {
                flag.store(true, Ordering::SeqCst);
            }
        }
        let updated = self
            .registry
            .request_mcp_task_cancel(&existing.task_id)
            .unwrap_or(existing);
        self.metrics.total_cancelled.fetch_add(1, Ordering::Relaxed);
        self.emit_event("mcp.task.cancelled", &updated);
        Ok(updated)
    }

    /// Records `inputResponses` on an `input_required` task via atomic CAS.
    pub fn submit_input_responses_scoped(
        &self,
        task_id: &str,
        input_responses: JsonValue,
        req_group_slug: Option<&str>,
        req_server_scope: Option<&str>,
    ) -> Result<McpTaskRecord, String> {
        let mut task = self.get_task_scoped(task_id, req_group_slug, req_server_scope)?;
        if task.status != McpTaskStatus::InputRequired {
            return Err(format!(
                "Task '{}' is in status '{}', not 'input_required'",
                task_id,
                task.status.as_str()
            ));
        }
        task.input_responses = Some(input_responses);
        task.status = McpTaskStatus::Working;
        task.status_message = Some("Input received; resuming task".to_string());
        task.updated_at = now_epoch_secs();
        let saved =
            self.registry
                .update_mcp_task_cas(task_id, McpTaskStatus::InputRequired, &task)?;
        self.emit_event("mcp.task.updated", &saved);
        Ok(saved)
    }

    pub fn prometheus_lines(&self) -> String {
        format!(
            "# HELP omni_mcp_tasks_active Currently running MCP tasks on this replica\n\
             # TYPE omni_mcp_tasks_active gauge\n\
             omni_mcp_tasks_active {}\n\
             # HELP omni_mcp_tasks_total Total MCP tasks by terminal/suspended status\n\
             # TYPE omni_mcp_tasks_total counter\n\
             omni_mcp_tasks_total{{status=\"completed\"}} {}\n\
             omni_mcp_tasks_total{{status=\"failed\"}} {}\n\
             omni_mcp_tasks_total{{status=\"cancelled\"}} {}\n\
             omni_mcp_tasks_total{{status=\"input_required\"}} {}\n",
            self.metrics.active.load(Ordering::Relaxed),
            self.metrics.total_completed.load(Ordering::Relaxed),
            self.metrics.total_failed.load(Ordering::Relaxed),
            self.metrics.total_cancelled.load(Ordering::Relaxed),
            self.metrics.total_input_required.load(Ordering::Relaxed),
        )
    }
}
