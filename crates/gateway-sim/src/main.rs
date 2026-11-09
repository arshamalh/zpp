//! gateway-sim: pretends to be the box on a customer's site.
//!
//! Time is accelerated: every `TICK_MS` of wall-clock time is one simulated 15-minute PTU,
//! so a full day takes 96 seconds by default, which is handy for a live demo.

mod control;
mod sim;

use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use clap::Parser;
use ems_core::prices::{day_ahead_eur_mwh, imbalance_eur_mwh};
use ems_core::protocol::topics::{self, Channel};
use ems_core::protocol::{Ack, GatewayMode, ScheduleCommand, Telemetry};
use ems_core::time::ptu;
use rumqttc::{AsyncClient, ClientError, Event, EventLoop, LastWill, MqttOptions, Packet, QoS};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};
use uuid::Uuid;

use control::{decide, Limits, ScheduleBook};
use sim::{preset, SiteSim, PRESETS};

#[derive(Parser, Debug)]
#[command(about = "Simulated EMS gateway")]
struct Args {
    #[arg(long, env = "SITE_ID")]
    site_id: String,
    /// One of: depot, campus, cold-storage
    #[arg(long, env = "PROFILE", default_value = "depot")]
    profile: String,
    #[arg(long, env = "MQTT_HOST", default_value = "localhost")]
    mqtt_host: String,
    #[arg(long, env = "MQTT_PORT", default_value_t = 1883)]
    mqtt_port: u16,
    /// Wall-clock milliseconds per simulated PTU.
    #[arg(long, env = "TICK_MS", default_value_t = 1000)]
    tick_ms: u64,
    /// Simulated hour of day to start at (UTC).
    #[arg(long, env = "START_HOUR", default_value_t = 6)]
    start_hour: u32,
    #[arg(long, env = "SEED", default_value_t = 7)]
    seed: u64,
    #[arg(long, env = "SAFETY_MARGIN_KW", default_value_t = 3.0)]
    safety_margin_kw: f64,
    /// Chaos: percentage of acks to "lose", to exercise cloud retries + idempotency.
    #[arg(long, env = "DROP_ACK_PCT", default_value_t = 0)]
    drop_ack_pct: u8,
    #[arg(long, env = "LOG_JSON", default_value_t = false)]
    log_json: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    init_tracing(args.log_json);

    let Some(profile) = preset(&args.profile) else {
        bail!(
            "unknown profile {:?}; expected one of {PRESETS:?}",
            args.profile
        );
    };
    let sim = SiteSim::new(args.site_id.clone(), profile, args.seed);
    let info_json = serde_json::to_vec(&sim.info())?;

    // The event loop must be polled continuously; it runs in its own task and forwards
    // incoming commands to the main loop over a channel.
    let (client, eventloop) = mqtt_client(&args);
    let (cmd_tx, mut cmd_rx) = mpsc::channel(32);
    tokio::spawn(run_mqtt_link(
        eventloop,
        client.clone(),
        args.site_id.clone(),
        info_json,
        cmd_tx,
    ));

    let mut gateway = Gateway::new(&args, sim, client)?;
    let mut tick = tokio::time::interval(Duration::from_millis(args.tick_ms));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    info!(
        site = %gateway.site,
        boot_id = %gateway.boot_id,
        profile = %args.profile,
        limit_kw = gateway.limits.import_kw,
        "gateway started"
    );
    loop {
        tokio::select! {
            _ = tick.tick() => gateway.on_tick()?,
            Some(payload) = cmd_rx.recv() => gateway.on_command(&payload)?,
            _ = &mut shutdown => {
                info!("shutting down");
                gateway.go_offline().await;
                return Ok(());
            }
        }
    }
}

/// Builds the MQTT client. The Last Will is published *by the broker* if we vanish.
fn mqtt_client(args: &Args) -> (AsyncClient, EventLoop) {
    let site = &args.site_id;
    let mut opts = MqttOptions::new(format!("gw-{site}"), &args.mqtt_host, args.mqtt_port);
    opts.set_keep_alive(Duration::from_secs(5));
    opts.set_last_will(LastWill::new(
        topics::topic(site, Channel::Status),
        "offline",
        QoS::AtLeastOnce,
        true,
    ));
    AsyncClient::new(opts, 512)
}

/// Drives the MQTT event loop forever, forwarding incoming commands to `cmd_tx`.
async fn run_mqtt_link(
    mut eventloop: EventLoop,
    client: AsyncClient,
    site: String,
    info_json: Vec<u8>,
    cmd_tx: mpsc::Sender<Vec<u8>>,
) {
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                info!(%site, "connected to broker");
                // (Re)subscribe and announce on every (re)connect. Spawned, because
                // awaiting client calls here would stall the loop that drains them.
                let (c, s, info) = (client.clone(), site.clone(), info_json.clone());
                tokio::spawn(async move {
                    if let Err(e) = announce(c, s, info).await {
                        warn!(error = %e, "failed to announce");
                    }
                });
            }
            Ok(Event::Incoming(Packet::Publish(p))) => {
                if matches!(topics::parse(&p.topic), Some((_, Channel::Commands))) {
                    let _ = cmd_tx.send(p.payload.to_vec()).await;
                }
            }
            Ok(_) => {}
            Err(e) => {
                warn!(error = %e, "broker connection lost; running locally, retrying in 1s");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// Subscribes to commands and publishes the retained site info and online status.
async fn announce(c: AsyncClient, site: String, info: Vec<u8>) -> Result<(), ClientError> {
    let topic = |ch| topics::topic(&site, ch);
    c.subscribe(topic(Channel::Commands), QoS::AtLeastOnce)
        .await?;
    c.publish(topic(Channel::Info), QoS::AtLeastOnce, true, info)
        .await?;
    c.publish(topic(Channel::Status), QoS::AtLeastOnce, true, "online")
        .await
}

/// The gateway's control state: everything the main loop mutates between events.
struct Gateway {
    site: String,
    boot_id: Uuid,
    client: AsyncClient,
    sim: SiteSim,
    book: ScheduleBook,
    limits: Limits,
    drop_ack_pct: u8,
    /// Simulated time, advanced one PTU per tick.
    now: DateTime<Utc>,
    seq: u64,
    last_mode: Option<GatewayMode>,
    chaos: u64,
}

impl Gateway {
    fn new(args: &Args, sim: SiteSim, client: AsyncClient) -> Result<Self> {
        let start = Utc::now()
            .date_naive()
            .and_hms_opt(args.start_hour, 0, 0)
            .context("bad START_HOUR")?;
        Ok(Self {
            site: args.site_id.clone(),
            boot_id: Uuid::new_v4(),
            client,
            limits: Limits {
                import_kw: sim.profile().grid_limit_kw,
                export_kw: sim.profile().export_limit_kw,
                margin_kw: args.safety_margin_kw,
            },
            sim,
            book: ScheduleBook::default(),
            drop_ack_pct: args.drop_ack_pct,
            now: Utc.from_utc_datetime(&start),
            seq: 0,
            last_mode: None,
            chaos: args.seed,
        })
    }

    /// Runs one PTU: observe the site, decide setpoints, apply them, report telemetry.
    fn on_tick(&mut self) -> Result<()> {
        let now = self.now;
        let obs = self.sim.observe(now);
        let slot = self.book.slot_for(now).cloned();
        let spread = imbalance_eur_mwh(now) - day_ahead_eur_mwh(now);
        let d = decide(&obs, slot.as_ref(), self.limits, spread);
        self.log_mode_change(d.mode);

        let battery_kw = self.sim.apply(d.battery_kw, &d.ev_alloc);
        let ev_kw: f64 = d.ev_alloc.iter().sum();
        let grid_kw = obs.load_kw + ev_kw + battery_kw - obs.pv_kw;
        self.seq += 1;
        let seq = self.seq;
        let telemetry = Telemetry {
            site_id: self.site.clone(),
            boot_id: self.boot_id,
            seq,
            ts: now,
            grid_kw,
            pv_kw: obs.pv_kw,
            load_kw: obs.load_kw,
            ev_kw,
            ev_demand_kw: obs.ev_wants.iter().sum(),
            battery_kw,
            battery_soc_kwh: obs.soc_kwh,
            mode: d.mode,
            schedule_id: slot.and(self.book.active_id()),
        };
        debug!(seq, ts = %now.format("%H:%M"), grid_kw = format!("{grid_kw:.1}"), mode = ?d.mode, "ptu");

        // try_publish never blocks the control loop; if the outbound buffer is full
        // (long broker outage), we drop. Store-and-forward is an extension exercise.
        if let Err(e) = self.try_publish(Channel::Telemetry, serde_json::to_vec(&telemetry)?) {
            warn!(error = %e, seq, "telemetry dropped");
        }
        self.now += ptu();
        Ok(())
    }

    /// Offers a schedule command to the book and acks the outcome.
    fn on_command(&mut self, payload: &[u8]) -> Result<()> {
        let cmd: ScheduleCommand = match serde_json::from_slice(payload) {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "unparseable command");
                return Ok(());
            }
        };
        let command_id = cmd.command_id;
        let status = self.book.offer(cmd, self.now);
        info!(%command_id, ?status, "schedule command");

        if self.chaos_drops_ack() {
            warn!(%command_id, "chaos: dropping ack");
            return Ok(());
        }
        let ack = Ack { command_id, status };
        if let Err(e) = self.try_publish(Channel::Acks, serde_json::to_vec(&ack)?) {
            warn!(error = %e, "ack dropped");
        }
        Ok(())
    }

    /// Announces a clean exit, so the broker doesn't have to fire the Last Will.
    async fn go_offline(&self) {
        let status = topics::topic(&self.site, Channel::Status);
        let _ = self
            .client
            .publish(status, QoS::AtLeastOnce, true, "offline")
            .await;
        tokio::time::sleep(Duration::from_millis(200)).await; // let the event loop flush
        let _ = self.client.disconnect().await;
    }

    fn log_mode_change(&mut self, mode: GatewayMode) {
        if self.last_mode == Some(mode) {
            return;
        }
        self.last_mode = Some(mode);
        let ts = self.now.format("%H:%M");
        match mode {
            GatewayMode::Cloud => info!(%ts, "following cloud schedule"),
            GatewayMode::Fallback => warn!(%ts, "no valid schedule: local fallback rule"),
        }
    }

    /// Advances the chaos LCG and decides whether this ack gets "lost".
    fn chaos_drops_ack(&mut self) -> bool {
        self.chaos = self
            .chaos
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.chaos >> 33) % 100) < self.drop_ack_pct as u64
    }

    fn try_publish(&self, channel: Channel, payload: Vec<u8>) -> Result<(), ClientError> {
        let topic = topics::topic(&self.site, channel);
        self.client
            .try_publish(topic, QoS::AtLeastOnce, false, payload)
    }
}

fn init_tracing(json: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if json {
        builder.json().init();
    } else {
        builder.init();
    }
}

/// Ctrl-C locally, SIGTERM in Kubernetes.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}
