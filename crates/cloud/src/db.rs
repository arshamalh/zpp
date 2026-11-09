//! Postgres persistence. Ingestion never waits for the database: writes go through a
//! bounded channel to this writer task, which batches telemetry into multi-row INSERTs.
//! Every write is an upsert, so replays and redeliveries are harmless.

use std::time::{Duration, Instant};

use anyhow::Context;
use ems_core::protocol::{GatewayMode, ScheduleCommand, SiteInfo, Telemetry};
use metrics::{counter, histogram};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres, QueryBuilder};
use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::Uuid;

const BATCH: usize = 200;

#[derive(Debug)]
pub enum DbWrite {
    Site(SiteInfo),
    Telemetry(Telemetry),
    Command {
        cmd: ScheduleCommand,
        plan: serde_json::Value,
    },
    CommandStatus {
        id: Uuid,
        state: String,
        attempts: i32,
    },
}

#[derive(Clone)]
pub struct Db {
    pool: PgPool,
}

impl Db {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(5))
            .connect(url)
            .await
            .context("connecting to Postgres")?;
        // Migrations are embedded in the binary at compile time.
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("running migrations")?;
        info!("connected to Postgres, migrations applied");
        Ok(Db { pool })
    }

    pub async fn ping(&self) -> bool {
        sqlx::query("SELECT 1").execute(&self.pool).await.is_ok()
    }
}

fn mode_str(m: GatewayMode) -> &'static str {
    match m {
        GatewayMode::Cloud => "cloud",
        GatewayMode::Fallback => "fallback",
    }
}

pub async fn writer(db: Db, mut rx: mpsc::Receiver<DbWrite>) {
    let mut batch: Vec<Telemetry> = Vec::with_capacity(BATCH);
    let mut flush_every = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(DbWrite::Telemetry(t)) => {
                    batch.push(t);
                    if batch.len() >= BATCH {
                        flush(&db, &mut batch).await;
                    }
                }
                Some(other) => {
                    if let Err(e) = exec(&db, other).await {
                        counter!("peakpilot_db_errors_total").increment(1);
                        warn!(error = %e, "db write failed");
                    }
                }
                None => {
                    flush(&db, &mut batch).await; // channel closed: shutting down
                    return;
                }
            },
            _ = flush_every.tick() => if !batch.is_empty() { flush(&db, &mut batch).await },
        }
    }
}

async fn flush(db: &Db, batch: &mut Vec<Telemetry>) {
    if batch.is_empty() {
        return;
    }
    let started = Instant::now();
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "INSERT INTO telemetry (site_id, ts, boot_id, seq, grid_kw, pv_kw, load_kw, ev_kw, ev_demand_kw, \
         battery_kw, battery_soc_kwh, mode, schedule_id) ",
    );
    qb.push_values(batch.iter(), |mut b, t| {
        b.push_bind(&t.site_id)
            .push_bind(t.ts)
            .push_bind(t.boot_id)
            .push_bind(t.seq as i64)
            .push_bind(t.grid_kw)
            .push_bind(t.pv_kw)
            .push_bind(t.load_kw)
            .push_bind(t.ev_kw)
            .push_bind(t.ev_demand_kw)
            .push_bind(t.battery_kw)
            .push_bind(t.battery_soc_kwh)
            .push_bind(mode_str(t.mode))
            .push_bind(t.schedule_id);
    });
    // A restarted (simulated) gateway can replay timestamps: last write wins.
    qb.push(
        " ON CONFLICT (site_id, ts) DO UPDATE SET boot_id = EXCLUDED.boot_id, seq = EXCLUDED.seq, \
         grid_kw = EXCLUDED.grid_kw, pv_kw = EXCLUDED.pv_kw, load_kw = EXCLUDED.load_kw, ev_kw = EXCLUDED.ev_kw, \
         ev_demand_kw = EXCLUDED.ev_demand_kw, battery_kw = EXCLUDED.battery_kw, \
         battery_soc_kwh = EXCLUDED.battery_soc_kwh, mode = EXCLUDED.mode, schedule_id = EXCLUDED.schedule_id",
    );
    match qb.build().execute(&db.pool).await {
        Ok(_) => {
            counter!("peakpilot_db_rows_written_total", "table" => "telemetry")
                .increment(batch.len() as u64);
            histogram!("peakpilot_db_flush_seconds").record(started.elapsed().as_secs_f64());
        }
        Err(e) => {
            counter!("peakpilot_db_errors_total").increment(1);
            warn!(error = %e, rows = batch.len(), "telemetry batch insert failed; dropping batch");
        }
    }
    batch.clear();
}

async fn exec(db: &Db, w: DbWrite) -> Result<(), sqlx::Error> {
    match w {
        DbWrite::Site(info) => {
            let json = serde_json::to_value(&info).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
            sqlx::query(
                "INSERT INTO sites (site_id, info) VALUES ($1, $2) \
                 ON CONFLICT (site_id) DO UPDATE SET info = EXCLUDED.info, updated_at = now()",
            )
            .bind(&info.site_id)
            .bind(json)
            .execute(&db.pool)
            .await?;
        }
        DbWrite::Command { cmd, plan } => {
            sqlx::query(
                "INSERT INTO commands (command_id, site_id, issued_at, valid_until, plan) \
                 VALUES ($1, $2, $3, $4, $5) ON CONFLICT (command_id) DO NOTHING",
            )
            .bind(cmd.command_id)
            .bind(&cmd.site_id)
            .bind(cmd.issued_at)
            .bind(cmd.valid_until)
            .bind(plan)
            .execute(&db.pool)
            .await?;
        }
        DbWrite::CommandStatus {
            id,
            state,
            attempts,
        } => {
            sqlx::query("UPDATE commands SET state = $2, attempts = $3, updated_at = now() WHERE command_id = $1")
                .bind(id)
                .bind(state)
                .bind(attempts)
                .execute(&db.pool)
                .await?;
        }
        DbWrite::Telemetry(_) => unreachable!("telemetry is batched in writer()"),
    }
    counter!("peakpilot_db_rows_written_total", "table" => "other").increment(1);
    Ok(())
}
