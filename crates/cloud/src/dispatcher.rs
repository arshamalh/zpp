//! Reliable command delivery over an unreliable link.
//!
//! publish (QoS 1) -> wait for an application-level ack -> on timeout retry with
//! exponential backoff -> give up after `max_attempts`. Retries are safe because the
//! gateway dedupes by `command_id` (idempotency) and ignores expired commands.
//! A retry loop stops early if a newer plan has superseded this command.

use std::time::{Duration, Instant};

use ems_core::protocol::topics::{self, Channel};
use ems_core::protocol::{AckStatus, ScheduleCommand};
use metrics::{counter, histogram};
use rumqttc::QoS;
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::db::DbWrite;
use crate::state::{CommandState, Shared};

pub async fn deliver(state: Shared, cmd: ScheduleCommand) {
    let id = cmd.command_id;
    let site = cmd.site_id.clone();
    let payload = match serde_json::to_vec(&cmd) {
        Ok(p) => p,
        Err(e) => {
            return finish(
                &state,
                &site,
                id,
                0,
                CommandState::Failed {
                    reason: e.to_string(),
                },
            )
        }
    };
    let topic = topics::topic(&site, Channel::Commands);
    let started = Instant::now();

    for attempt in 1..=state.cfg.max_attempts {
        if !state.is_current_plan(&site, id) {
            counter!("peakpilot_commands_abandoned_total").increment(1);
            state.pending().remove(&id);
            return finish(&state, &site, id, attempt - 1, CommandState::Abandoned);
        }

        let (tx, rx) = oneshot::channel::<AckStatus>();
        state.pending().insert(id, tx);
        state.update_command(&site, id, |c| c.attempts = attempt);

        if let Err(e) = state
            .mqtt
            .publish(topic.clone(), QoS::AtLeastOnce, false, payload.clone())
            .await
        {
            warn!(%site, %id, error = %e, "publish failed");
        }
        counter!("peakpilot_commands_sent_total").increment(1);
        if attempt > 1 {
            counter!("peakpilot_command_retries_total").increment(1);
        }

        let wait =
            Duration::from_millis(state.cfg.ack_timeout_ms.saturating_mul(1 << (attempt - 1)));
        match tokio::time::timeout(wait, rx).await {
            Ok(Ok(ack)) => {
                let latency = started.elapsed();
                histogram!("peakpilot_ack_latency_seconds").record(latency.as_secs_f64());
                let ms = latency.as_millis() as u64;
                state.update_command(&site, id, |c| c.ack_latency_ms = Some(ms));
                info!(%site, %id, attempt, ?ack, latency_ms = ms, "command acked");
                return finish(&state, &site, id, attempt, CommandState::Acked { ack });
            }
            // Err(Elapsed) = timeout; Ok(Err(RecvError)) = sender dropped (replaced). Both: retry.
            Ok(Err(_)) | Err(_) => {
                warn!(%site, %id, attempt, wait_ms = wait.as_millis() as u64, "no ack")
            }
        }
    }

    state.pending().remove(&id);
    let reason = format!("no ack after {} attempts", state.cfg.max_attempts);
    finish(
        &state,
        &site,
        id,
        state.cfg.max_attempts,
        CommandState::Failed { reason },
    );
}

fn finish(state: &Shared, site: &str, id: uuid::Uuid, attempts: u32, outcome: CommandState) {
    counter!("peakpilot_command_outcomes_total", "outcome" => outcome.label()).increment(1);
    let label = outcome.label().to_owned();
    state.update_command(site, id, |c| c.state = outcome);
    state.persist(DbWrite::CommandStatus {
        id,
        state: label,
        attempts: attempts as i32,
    });
}
