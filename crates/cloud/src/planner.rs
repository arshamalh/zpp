//! Planner task: turns "please plan site X" requests into ScheduleCommands.
//! The optimisation is CPU-bound, so it runs on tokio's blocking pool
//! (`spawn_blocking`) instead of stalling the async worker threads.

use std::time::Instant;

use ems_core::forecast::naive_forecast;
use ems_core::protocol::ScheduleCommand;
use ems_core::scheduler::{self, BatteryState, CostModel, PlanError, PlanInput};
use ems_core::time::ptu;
use metrics::{counter, histogram};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::task::JoinError;
use tracing::{info, warn};
use uuid::Uuid;

use crate::db::DbWrite;
use crate::dispatcher;
use crate::state::{CommandRecord, CommandState, CurrentPlan, Shared};

#[derive(Debug, Error)]
pub enum PlannerError {
    #[error("site not ready: {0}")]
    NotReady(&'static str),
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error("planner task panicked: {0}")]
    Join(#[from] JoinError),
    #[error("could not serialise plan: {0}")]
    Json(#[from] serde_json::Error),
}

pub async fn run(state: Shared, mut rx: mpsc::Receiver<String>) {
    while let Some(site_id) = rx.recv().await {
        match plan_site(&state, &site_id).await {
            Ok(()) => {}
            Err(PlannerError::NotReady(why)) => tracing::debug!(%site_id, why, "skip planning"),
            Err(e) => {
                counter!("peakpilot_plan_errors_total").increment(1);
                warn!(%site_id, error = %e, "planning failed");
            }
        }
    }
}

pub async fn plan_site(state: &Shared, site_id: &str) -> Result<(), PlannerError> {
    // 1. Snapshot what we need under the lock, then release it.
    let (info, history, latest) = {
        let sites = state.sites_read();
        let s = sites
            .get(site_id)
            .ok_or(PlannerError::NotReady("unknown site"))?;
        let info = s
            .info
            .clone()
            .ok_or(PlannerError::NotReady("no site info yet"))?;
        let latest = s
            .latest
            .clone()
            .ok_or(PlannerError::NotReady("no telemetry yet"))?;
        (info, s.history.iter().cloned().collect::<Vec<_>>(), latest)
    };

    // 2. Forecast + optimise on the blocking pool. `move` gives the closure ownership.
    let from = latest.ts + ptu();
    let horizon = state.cfg.horizon_ptus;
    let margin = state.cfg.safety_margin_kw;
    let started = Instant::now();
    let plan = tokio::task::spawn_blocking(move || {
        let forecast = naive_forecast(&info, &history, from, horizon);
        let battery = info.battery.as_ref().map(|b| BatteryState {
            capacity_kwh: b.capacity_kwh,
            max_power_kw: b.max_power_kw,
            efficiency: b.efficiency,
            soc_kwh: latest.battery_soc_kwh.unwrap_or(0.0),
            min_soc_kwh: b.capacity_kwh * 0.1,
        });
        scheduler::plan(&PlanInput {
            grid_limit_kw: info.grid_limit_kw,
            export_limit_kw: info.export_limit_kw,
            safety_margin_kw: margin,
            battery,
            forecast: &forecast,
            costs: CostModel::default(),
            levels: 41,
        })
    })
    .await??; // first `?`: did the task panic? second: did planning fail?
    let compute = started.elapsed();
    histogram!("peakpilot_plan_duration_seconds").record(compute.as_secs_f64());

    // 3. Wrap it in a command, record it, persist it, dispatch it.
    let cmd = ScheduleCommand {
        command_id: Uuid::new_v4(),
        site_id: site_id.to_owned(),
        issued_at: latest.ts,
        valid_until: from + ptu() * state.cfg.validity_ptus,
        // The gateway ignores anything past `valid_until`, so don't send it. This also
        // keeps the message well under the MQTT client's 10 KB packet limit.
        slots: plan.setpoints(state.cfg.validity_ptus as usize),
    };
    info!(
        %site_id,
        command_id = %cmd.command_id,
        expected_peak_kw = format!("{:.1}", plan.expected_peak_kw),
        baseline_peak_kw = format!("{:.1}", plan.baseline_peak_kw),
        compute_ms = format!("{:.2}", compute.as_secs_f64() * 1e3),
        "new plan"
    );
    let plan_json = serde_json::to_value(&plan)?;
    if let Some(s) = state.sites_write().get_mut(site_id) {
        s.push_command(CommandRecord {
            command_id: cmd.command_id,
            issued_at: cmd.issued_at,
            attempts: 0,
            state: CommandState::Pending,
            ack_latency_ms: None,
        });
        s.nominate(&plan);
        s.plan = Some(CurrentPlan {
            command_id: cmd.command_id,
            compute_ms: compute.as_secs_f64() * 1e3,
            plan,
        });
    }
    state.persist(DbWrite::Command {
        cmd: cmd.clone(),
        plan: plan_json,
    });
    tokio::spawn(dispatcher::deliver(state.clone(), cmd));
    Ok(())
}
