//! HTTP API (axum) + the dashboard page + health and metrics endpoints.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use ems_core::accounting::{ptu_energy_eur, summarize, Flows, Nomination, Summary};
use ems_core::prices::{co2_g_kwh, day_ahead_eur_mwh, imbalance_eur_mwh, settlement_eur_mwh};
use ems_core::protocol::{SiteInfo, Telemetry};
use ems_core::time::{ptu, PTUS_PER_DAY};
use ems_core::Kw;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tower_http::trace::TraceLayer;

use crate::state::{CommandRecord, Shared, SiteState};

pub fn router(state: Shared) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../static/index.html")) }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript")],
                    include_str!("../static/app.js"),
                )
            }),
        )
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .route("/api/sites", get(list_sites))
        .route("/api/sites/{id}/telemetry", get(get_telemetry))
        .route("/api/sites/{id}/day", get(get_day))
        .route("/api/sites/{id}/summary", get(get_summary))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// API errors become JSON responses via `IntoResponse`, so handlers can just use `?`.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("site {0} not found")]
    SiteNotFound(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = match self {
            ApiError::SiteNotFound(_) => StatusCode::NOT_FOUND,
        };
        (code, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

#[derive(Serialize)]
pub struct SiteSummary {
    site_id: String,
    info: Option<SiteInfo>,
    online: bool,
    last_seen_secs: Option<f64>,
    latest: Option<Telemetry>,
    plan_compute_ms: Option<f64>,
    commands: Vec<CommandRecord>,
}

fn summarize_site(site_id: &str, s: &SiteState, offline_after: Duration) -> SiteSummary {
    let age = s.last_seen.map(|t| t.elapsed());
    let online = s.status.as_deref() != Some("offline") && age.is_some_and(|a| a < offline_after);
    SiteSummary {
        site_id: site_id.to_owned(),
        info: s.info.clone(),
        online,
        last_seen_secs: age.map(|a| a.as_secs_f64()),
        latest: s.latest.clone(),
        plan_compute_ms: s.plan.as_ref().map(|p| p.compute_ms),
        commands: s.commands.iter().cloned().collect(),
    }
}

async fn list_sites(State(state): State<Shared>) -> Json<Vec<SiteSummary>> {
    let offline_after = Duration::from_secs(state.cfg.offline_after_secs);
    let sites = state.sites_read();
    let mut out: Vec<_> = sites
        .iter()
        .map(|(id, s)| summarize_site(id, s, offline_after))
        .collect();
    out.sort_by(|a, b| a.site_id.cmp(&b.site_id));
    Json(out)
}

#[derive(Deserialize)]
struct TelemetryQuery {
    limit: Option<usize>,
}

async fn get_telemetry(
    State(state): State<Shared>,
    Path(id): Path<String>,
    Query(q): Query<TelemetryQuery>,
) -> Result<Json<Vec<Telemetry>>, ApiError> {
    let limit = q.limit.unwrap_or(96).min(crate::state::HISTORY_LEN);
    let sites = state.sites_read();
    let s = sites
        .get(&id)
        .ok_or_else(|| ApiError::SiteNotFound(id.clone()))?;
    let skip = s.history.len().saturating_sub(limit);
    Ok(Json(s.history.iter().skip(skip).cloned().collect()))
}

/// One PTU of a day: what was measured, what was bought, what the market did.
/// Prices only appear once the market would really have published them.
#[derive(Serialize)]
struct DayRow {
    ts: DateTime<Utc>,
    measured: Option<Telemetry>,
    flows: Option<Flows>,
    nomination: Option<Nomination>,
    forecast_grid_kw: Option<Kw>,
    day_ahead_eur_mwh: f64,
    imbalance_eur_mwh: Option<f64>,
    settlement_eur_mwh: Option<(f64, f64)>,
    co2_g_kwh: f64,
    cost_eur: Option<f64>,
}

#[derive(Deserialize)]
struct DayQuery {
    date: NaiveDate,
}

async fn get_day(
    State(state): State<Shared>,
    Path(id): Path<String>,
    Query(q): Query<DayQuery>,
) -> Result<Json<Vec<DayRow>>, ApiError> {
    let sites = state.sites_read();
    let s = sites
        .get(&id)
        .ok_or_else(|| ApiError::SiteNotFound(id.clone()))?;
    Ok(Json(day_rows(s, q.date)))
}

fn day_rows(s: &SiteState, date: NaiveDate) -> Vec<DayRow> {
    // A restarted gateway can replay timestamps: the latest reading wins.
    let measured: HashMap<_, _> = s
        .history
        .iter()
        .filter(|t| t.ts.date_naive() == date)
        .map(|t| (t.ts, t))
        .collect();
    let forecast: HashMap<_, _> = s
        .plan
        .iter()
        .flat_map(|p| &p.plan.slots)
        .map(|slot| (slot.start, slot.expected_grid_kw))
        .collect();
    let now = s.latest.as_ref().map(|t| t.ts);
    let midnight = date.and_time(NaiveTime::MIN).and_utc();

    (0..PTUS_PER_DAY as i32)
        .map(|i| {
            let ts = midnight + ptu() * i;
            let reading = measured.get(&ts).copied();
            let nomination = s.nominations.get(&ts).copied();
            let published = now.is_some_and(|now| ts <= now);
            let settled = now.is_some_and(|now| date < now.date_naive());
            DayRow {
                ts,
                measured: reading.cloned(),
                flows: reading.map(Flows::of),
                nomination,
                forecast_grid_kw: forecast.get(&ts).copied(),
                day_ahead_eur_mwh: day_ahead_eur_mwh(ts),
                imbalance_eur_mwh: published.then(|| imbalance_eur_mwh(ts)),
                settlement_eur_mwh: settled.then(|| settlement_eur_mwh(ts)),
                co2_g_kwh: co2_g_kwh(ts),
                cost_eur: ptu_energy_eur(
                    ts,
                    nomination.map(|n| n.grid_kw()),
                    reading.map(|t| t.grid_kw),
                ),
            }
        })
        .collect()
}

#[derive(Deserialize)]
struct SummaryQuery {
    from: NaiveDate,
    to: NaiveDate,
}

/// The energy bill and the KPIs for a period (both dates inclusive).
async fn get_summary(
    State(state): State<Shared>,
    Path(id): Path<String>,
    Query(q): Query<SummaryQuery>,
) -> Result<Json<Summary>, ApiError> {
    let sites = state.sites_read();
    let s = sites
        .get(&id)
        .ok_or_else(|| ApiError::SiteNotFound(id.clone()))?;
    let info = s
        .info
        .as_ref()
        .ok_or_else(|| ApiError::SiteNotFound(id.clone()))?;
    let period = q.from..=q.to;
    let readings = s
        .history
        .iter()
        .filter(|t| period.contains(&t.ts.date_naive()));
    Ok(Json(summarize(readings, &s.nominations, info)))
}

/// Readiness: only take traffic when our dependencies are reachable.
/// (Liveness, `/healthz`, stays dumb: restarting won't fix a broker outage.)
async fn readyz(State(state): State<Shared>) -> (StatusCode, &'static str) {
    if !state.mqtt_connected.load(Ordering::Relaxed) {
        return (StatusCode::SERVICE_UNAVAILABLE, "mqtt not connected");
    }
    if let Some(db) = &state.db {
        if !db.ping().await {
            return (StatusCode::SERVICE_UNAVAILABLE, "database not reachable");
        }
    }
    (StatusCode::OK, "ready")
}

async fn metrics(State(state): State<Shared>) -> String {
    state.metrics.render()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex, RwLock};

    use axum::body::Body;
    use axum::http::Request;
    use chrono::TimeZone;
    use clap::Parser;
    use ems_core::protocol::GatewayMode;
    use ems_core::scheduler::{Plan, PlannedSlot};
    use http_body_util::BodyExt;
    use metrics_exporter_prometheus::PrometheusBuilder;
    use rumqttc::{AsyncClient, MqttOptions};
    use tower::ServiceExt; // for `oneshot`
    use uuid::Uuid;

    use super::*;
    use crate::config::Config;
    use crate::state::AppState;

    /// A real AppState with no broker behind it: `AsyncClient::new` doesn't connect.
    fn test_state() -> Shared {
        let (mqtt, _eventloop) = AsyncClient::new(MqttOptions::new("test", "localhost", 1883), 10);
        let (plan_tx, _plan_rx) = tokio::sync::mpsc::channel(10);
        Arc::new(AppState {
            cfg: Config::parse_from(["test"]),
            sites: RwLock::new(HashMap::new()),
            pending_acks: Mutex::new(HashMap::new()),
            mqtt,
            mqtt_connected: AtomicBool::new(false),
            plan_tx,
            db: None,
            db_tx: None,
            metrics: PrometheusBuilder::new().build_recorder().handle(),
        })
    }

    async fn get(app: Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let res = app
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn unknown_site_is_404_json() {
        let (status, body) = get(router(test_state()), "/api/sites/nope/telemetry").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "site nope not found");
    }

    #[tokio::test]
    async fn lists_known_sites_and_reports_not_ready() {
        let state = test_state();
        state
            .sites_write()
            .insert("depot-1".into(), SiteState::default());
        let (status, body) = get(router(state.clone()), "/api/sites").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body[0]["site_id"], "depot-1");
        assert_eq!(body[0]["online"], false);
        let res = router(state)
            .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    fn plan_at(ts: DateTime<Utc>, load_kw: Kw) -> Plan {
        Plan {
            slots: vec![PlannedSlot {
                start: ts,
                load_kw,
                pv_kw: 0.0,
                ev_kw: 0.0,
                battery_kw: 0.0,
                ev_cap_kw: 0.0,
                expected_grid_kw: load_kw,
            }],
            expected_cost_eur: 0.0,
            baseline_cost_eur: 0.0,
            expected_peak_kw: load_kw,
            baseline_peak_kw: load_kw,
        }
    }

    fn reading(ts: DateTime<Utc>, load_kw: Kw) -> Telemetry {
        Telemetry {
            site_id: "s".into(),
            boot_id: Uuid::nil(),
            seq: 1,
            ts,
            grid_kw: load_kw,
            pv_kw: 0.0,
            load_kw,
            ev_kw: 0.0,
            ev_demand_kw: 0.0,
            battery_kw: 0.0,
            battery_soc_kwh: None,
            mode: GatewayMode::Cloud,
            schedule_id: None,
        }
    }

    #[test]
    fn a_nomination_is_never_moved_by_later_plans() {
        let ts = Utc.with_ymd_and_hms(2026, 11, 5, 12, 0, 0).unwrap();
        let mut s = SiteState::default();
        s.nominate(&plan_at(ts, 10.0));
        s.nominate(&plan_at(ts, 99.0));
        assert_eq!(s.nominations[&ts].load_kw, 10.0);
    }

    #[test]
    fn a_day_has_96_ptus_and_hides_prices_the_market_has_not_published() {
        let now = Utc.with_ymd_and_hms(2026, 11, 5, 12, 0, 0).unwrap();
        let mut s = SiteState::default();
        s.nominate(&plan_at(now, 40.0));
        s.push_telemetry(reading(now, 50.0));

        let rows = day_rows(&s, now.date_naive());
        assert_eq!(rows.len(), 96);
        let noon = &rows[48];
        assert_eq!(noon.measured.as_ref().map(|t| t.grid_kw), Some(50.0));
        assert!(noon.imbalance_eur_mwh.is_some() && noon.cost_eur.is_some());
        assert!(noon.settlement_eur_mwh.is_none(), "known a day later");
        assert!(rows[49].imbalance_eur_mwh.is_none(), "still in the future");

        let yesterday = day_rows(&s, now.date_naive().pred_opt().unwrap());
        assert!(yesterday[0].settlement_eur_mwh.is_some());
    }
}
