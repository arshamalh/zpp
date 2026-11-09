//! peakpilot-cloud: the cloud half of a small Energy Management System.
//!
//! ```text
//!   gateways ──MQTT──▶ ingest ──▶ in-memory state ──▶ planner (spawn_blocking DP)
//!                        │                                 │
//!                        └──▶ db writer (batched) ◀────────┤
//!   gateways ◀──MQTT── dispatcher (ack / retry / backoff) ◀┘
//!   browser  ◀──HTTP── axum API + dashboard + /metrics
//! ```

mod api;
mod config;
mod db;
mod dispatcher;
mod ingest;
mod observability;
mod planner;
mod state;

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use rumqttc::{AsyncClient, MqttOptions};
use tokio::sync::mpsc;
use tracing::{info, warn};

use config::Config;
use state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::parse();
    observability::init_tracing(cfg.log_json);
    let metrics = observability::init_metrics()?;

    let db = match cfg.database_url() {
        Some(url) => Some(db::Db::connect(url).await?),
        None => {
            warn!("DATABASE_URL not set: running without persistence");
            None
        }
    };

    let client_id = format!("peakpilot-cloud-{}", uuid::Uuid::new_v4().simple());
    let mut opts = MqttOptions::new(client_id, &cfg.mqtt_host, cfg.mqtt_port);
    opts.set_keep_alive(Duration::from_secs(10));
    opts.set_max_packet_size(1 << 20, 1 << 20);
    let (mqtt, eventloop) = AsyncClient::new(opts, 1024);

    let (plan_tx, plan_rx) = mpsc::channel(256);
    let (db_tx, db_rx) = mpsc::channel(10_000);

    let state = Arc::new(AppState {
        cfg: cfg.clone(),
        sites: RwLock::new(HashMap::new()),
        pending_acks: Mutex::new(HashMap::new()),
        mqtt: mqtt.clone(),
        mqtt_connected: AtomicBool::new(false),
        plan_tx,
        db: db.clone(),
        db_tx: db.as_ref().map(|_| db_tx),
        metrics,
    });

    // Background tasks. Each is a plain async fn owning its own Arc<AppState>.
    tokio::spawn(ingest::run(state.clone(), eventloop));
    tokio::spawn(planner::run(state.clone(), plan_rx));
    tokio::spawn(observability::monitor(state.clone()));
    if let Some(db) = db {
        tokio::spawn(db::writer(db, db_rx));
    }

    let listener = tokio::net::TcpListener::bind(cfg.http_addr)
        .await
        .with_context(|| format!("binding {}", cfg.http_addr))?;
    info!(addr = %cfg.http_addr, mqtt = %format!("{}:{}", cfg.mqtt_host, cfg.mqtt_port), "peakpilot-cloud listening");
    axum::serve(listener, api::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    let _ = mqtt.disconnect().await;
    info!("bye");
    Ok(())
}

/// Ctrl-C locally, SIGTERM from Kubernetes.
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
    info!("shutdown signal received");
}
