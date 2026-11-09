//! Shared application state.
//!
//! Hot state (latest readings, plans, command log) lives in memory behind a `RwLock`;
//! Postgres is the durable sink, written asynchronously by the db writer task.
//!
//! Rule of thumb used throughout: a `std::sync` lock is never held across an `.await`.
//! Take the lock, copy what you need, drop it, *then* await.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Instant;

use chrono::{DateTime, Utc};
use ems_core::accounting::Nomination;
use ems_core::protocol::{AckStatus, SiteInfo, Telemetry};
use ems_core::scheduler::Plan;
use ems_core::time::PTUS_PER_DAY;
use metrics::counter;
use metrics_exporter_prometheus::PrometheusHandle;
use rumqttc::AsyncClient;
use serde::Serialize;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::config::Config;
use crate::db::{Db, DbWrite};

/// A month of PTUs, for the monthly bill and reports.
pub const HISTORY_LEN: usize = PTUS_PER_DAY * 31;
pub const COMMAND_LOG_LEN: usize = 20;

pub type Shared = Arc<AppState>;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CommandState {
    Pending,
    Acked {
        ack: AckStatus,
    },
    /// A newer plan replaced it before an ack arrived. Normal, not an error.
    Abandoned,
    Failed {
        reason: String,
    },
}

impl CommandState {
    pub fn label(&self) -> &'static str {
        match self {
            CommandState::Pending => "pending",
            CommandState::Acked {
                ack: AckStatus::Accepted,
            } => "accepted",
            CommandState::Acked {
                ack: AckStatus::Duplicate,
            } => "duplicate",
            CommandState::Acked {
                ack: AckStatus::Expired,
            } => "expired",
            CommandState::Acked {
                ack: AckStatus::Superseded,
            } => "superseded",
            CommandState::Acked {
                ack: AckStatus::Rejected { .. },
            } => "rejected",
            CommandState::Abandoned => "abandoned",
            CommandState::Failed { .. } => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CommandRecord {
    pub command_id: Uuid,
    pub issued_at: DateTime<Utc>,
    pub attempts: u32,
    pub state: CommandState,
    pub ack_latency_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CurrentPlan {
    pub command_id: Uuid,
    pub compute_ms: f64,
    pub plan: Plan,
}

#[derive(Debug, Default)]
pub struct SiteState {
    pub info: Option<SiteInfo>,
    /// "online" / "offline" from the gateway's retained status + Last Will.
    pub status: Option<String>,
    pub latest: Option<Telemetry>,
    pub last_seen: Option<Instant>,
    pub history: VecDeque<Telemetry>,
    pub plan: Option<CurrentPlan>,
    /// The position bought per PTU. Fixed once set: see `nominate`.
    pub nominations: BTreeMap<DateTime<Utc>, Nomination>,
    pub commands: VecDeque<CommandRecord>,
}

impl SiteState {
    /// `(boot_id, seq)` is the idempotency key for telemetry.
    pub fn is_duplicate(&self, t: &Telemetry) -> bool {
        self.latest
            .as_ref()
            .is_some_and(|l| l.boot_id == t.boot_id && t.seq <= l.seq)
    }

    pub fn has_pending_command(&self) -> bool {
        self.commands
            .iter()
            .any(|c| c.state == CommandState::Pending)
    }

    pub fn push_telemetry(&mut self, t: Telemetry) {
        self.last_seen = Some(Instant::now());
        if self.history.len() == HISTORY_LEN {
            self.history.pop_front();
        }
        self.history.push_back(t.clone());
        self.latest = Some(t);
    }

    /// Buys the position for PTUs up to a day ahead, once: later plans can't move it.
    /// The plan looks further than that because its last slots are distorted (it only
    /// values what's left in the battery roughly), and those shouldn't be what we buy.
    pub fn nominate(&mut self, plan: &Plan) {
        for slot in plan.slots.iter().take(PTUS_PER_DAY) {
            self.nominations.entry(slot.start).or_insert(Nomination {
                load_kw: slot.load_kw,
                pv_kw: slot.pv_kw,
                ev_kw: slot.ev_kw,
                battery_kw: slot.battery_kw,
            });
        }
        while self.nominations.len() > HISTORY_LEN + PTUS_PER_DAY {
            self.nominations.pop_first();
        }
    }

    pub fn push_command(&mut self, rec: CommandRecord) {
        self.commands.push_front(rec);
        self.commands.truncate(COMMAND_LOG_LEN);
    }
}

pub struct AppState {
    pub cfg: Config,
    pub sites: RwLock<HashMap<String, SiteState>>,
    /// Dispatcher tasks waiting for an ack, keyed by command id.
    pub pending_acks: Mutex<HashMap<Uuid, oneshot::Sender<AckStatus>>>,
    pub mqtt: AsyncClient,
    pub mqtt_connected: AtomicBool,
    pub plan_tx: mpsc::Sender<String>,
    pub db: Option<Db>,
    pub db_tx: Option<mpsc::Sender<DbWrite>>,
    pub metrics: PrometheusHandle,
}

impl AppState {
    // A poisoned lock means another thread panicked mid-update; crashing loudly
    // (and letting Kubernetes restart us) beats serving corrupt state.
    pub fn sites_read(&self) -> RwLockReadGuard<'_, HashMap<String, SiteState>> {
        self.sites.read().expect("sites lock poisoned")
    }
    pub fn sites_write(&self) -> RwLockWriteGuard<'_, HashMap<String, SiteState>> {
        self.sites.write().expect("sites lock poisoned")
    }
    pub fn pending(&self) -> MutexGuard<'_, HashMap<Uuid, oneshot::Sender<AckStatus>>> {
        self.pending_acks
            .lock()
            .expect("pending_acks lock poisoned")
    }

    /// Fire-and-forget persistence. If the writer falls behind we drop and count it
    /// rather than slow down ingestion (back-pressure decision: availability first).
    pub fn persist(&self, w: DbWrite) {
        if let Some(tx) = &self.db_tx {
            if tx.try_send(w).is_err() {
                counter!("peakpilot_db_writes_dropped_total").increment(1);
            }
        }
    }

    /// Mutate a command record in place; returns a copy of it afterwards.
    pub fn update_command(
        &self,
        site_id: &str,
        id: Uuid,
        f: impl FnOnce(&mut CommandRecord),
    ) -> Option<CommandRecord> {
        let mut sites = self.sites_write();
        let rec = sites
            .get_mut(site_id)?
            .commands
            .iter_mut()
            .find(|c| c.command_id == id)?;
        f(rec);
        Some(rec.clone())
    }

    pub fn is_current_plan(&self, site_id: &str, id: Uuid) -> bool {
        self.sites_read()
            .get(site_id)
            .and_then(|s| s.plan.as_ref())
            .is_some_and(|p| p.command_id == id)
    }
}
