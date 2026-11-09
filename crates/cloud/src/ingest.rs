//! MQTT ingestion: one task drives the rumqttc event loop and routes every incoming
//! message by topic. Handlers are synchronous (no `.await`): they validate, update
//! in-memory state, and hand slow work (DB writes, planning) to other tasks via channels.

use std::sync::atomic::Ordering;
use std::time::Duration;

use ems_core::protocol::topics::{self, Channel};
use ems_core::protocol::{Ack, GatewayMode, SiteInfo, Telemetry, ValidationError};
use metrics::{counter, gauge};
use rumqttc::{Event, EventLoop, Packet, QoS};
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::db::DbWrite;
use crate::state::Shared;

#[derive(Debug, Error)]
enum IngestError {
    #[error("bad json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid: {0}")]
    Invalid(#[from] ValidationError),
    #[error("payload is not utf-8")]
    Utf8,
}

impl IngestError {
    fn label(&self) -> &'static str {
        match self {
            IngestError::Json(_) => "bad_json",
            IngestError::Invalid(_) => "invalid",
            IngestError::Utf8 => "bad_utf8",
        }
    }
}

pub async fn run(state: Shared, mut eventloop: EventLoop) {
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                info!("connected to MQTT broker");
                state.mqtt_connected.store(true, Ordering::Relaxed);
                gauge!("peakpilot_mqtt_connected").set(1.0);
                let client = state.mqtt.clone();
                tokio::spawn(async move {
                    for ch in [
                        Channel::Info,
                        Channel::Status,
                        Channel::Telemetry,
                        Channel::Acks,
                    ] {
                        if let Err(e) = client
                            .subscribe(topics::all_sites(ch), QoS::AtLeastOnce)
                            .await
                        {
                            warn!(error = %e, "subscribe failed");
                        }
                    }
                });
            }
            Ok(Event::Incoming(Packet::Publish(p))) => handle(&state, &p.topic, &p.payload),
            Ok(_) => {}
            Err(e) => {
                state.mqtt_connected.store(false, Ordering::Relaxed);
                gauge!("peakpilot_mqtt_connected").set(0.0);
                warn!(error = %e, "MQTT connection error; reconnecting in 1s");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

fn handle(state: &Shared, topic: &str, payload: &[u8]) {
    let Some((site, ch)) = topics::parse(topic) else {
        counter!("peakpilot_messages_total", "channel" => "unknown", "result" => "bad_topic")
            .increment(1);
        return;
    };
    let result = match ch {
        Channel::Telemetry => on_telemetry(state, site, payload),
        Channel::Info => on_info(state, site, payload),
        Channel::Status => on_status(state, site, payload),
        Channel::Acks => on_ack(state, payload),
        Channel::Commands => Ok("ignored"),
    };
    let label = match &result {
        Ok(outcome) => *outcome,
        Err(e) => {
            warn!(%site, channel = ch.as_str(), error = %e, "rejected message");
            e.label()
        }
    };
    counter!("peakpilot_messages_total", "channel" => ch.as_str(), "result" => label).increment(1);
}

fn on_telemetry(state: &Shared, site: &str, payload: &[u8]) -> Result<&'static str, IngestError> {
    let t: Telemetry = serde_json::from_slice(payload)?;
    t.validate(site)?;

    // Keep the critical section small and await-free.
    let replan = {
        let mut sites = state.sites_write();
        let s = sites.entry(site.to_owned()).or_default();
        if s.is_duplicate(&t) {
            return Ok("duplicate");
        }
        let in_fallback = t.mode == GatewayMode::Fallback;
        let due = t.seq.is_multiple_of(state.cfg.replan_every);
        s.push_telemetry(t.clone());
        s.info.is_some() && (s.plan.is_none() || due || (in_fallback && !s.has_pending_command()))
    };

    gauge!("peakpilot_site_grid_kw", "site" => site.to_owned()).set(t.grid_kw);
    debug!(%site, seq = t.seq, grid_kw = t.grid_kw, "telemetry");
    state.persist(DbWrite::Telemetry(t));
    if replan {
        request_plan(state, site);
    }
    Ok("stored")
}

fn on_info(state: &Shared, site: &str, payload: &[u8]) -> Result<&'static str, IngestError> {
    let info: SiteInfo = serde_json::from_slice(payload)?;
    if info.site_id != site {
        return Err(ValidationError::SiteMismatch {
            payload: info.site_id,
            topic: site.to_owned(),
        }
        .into());
    }
    info!(%site, name = %info.name, limit_kw = info.grid_limit_kw, "site announced");
    state.sites_write().entry(site.to_owned()).or_default().info = Some(info.clone());
    state.persist(DbWrite::Site(info));
    request_plan(state, site);
    Ok("stored")
}

fn on_status(state: &Shared, site: &str, payload: &[u8]) -> Result<&'static str, IngestError> {
    let status = std::str::from_utf8(payload)
        .map_err(|_| IngestError::Utf8)?
        .to_owned();
    info!(%site, %status, "gateway status");
    state
        .sites_write()
        .entry(site.to_owned())
        .or_default()
        .status = Some(status);
    Ok("stored")
}

fn on_ack(state: &Shared, payload: &[u8]) -> Result<&'static str, IngestError> {
    let ack: Ack = serde_json::from_slice(payload)?;
    // Hand the ack to the dispatcher task waiting for it (if any).
    match state.pending().remove(&ack.command_id) {
        Some(waiter) => {
            let _ = waiter.send(ack.status);
            Ok("delivered")
        }
        None => Ok("late"), // e.g. the ack of a retry that already succeeded
    }
}

pub fn request_plan(state: &Shared, site: &str) {
    if state.plan_tx.try_send(site.to_owned()).is_err() {
        counter!("peakpilot_plan_requests_dropped_total").increment(1);
    }
}
