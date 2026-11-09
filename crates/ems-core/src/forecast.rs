//! Deliberately naive forecasts, good enough to plan with and easy to replace.
//!
//! - load / EV demand: average for the same PTU-of-day in history, else the overall mean
//! - solar: clear-sky curve × kWp × a "cloudiness" ratio estimated from recent readings

use std::collections::HashMap;
use std::f64::consts::PI;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::prices::day_ahead_eur_mwh;
use crate::protocol::{SiteInfo, Telemetry};
use crate::time::{hour_of_day, ptu, ptu_of_day};
use crate::Kw;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SlotForecast {
    pub start: DateTime<Utc>,
    pub pv_kw: Kw,
    pub load_kw: Kw,
    pub ev_demand_kw: Kw,
    pub price_eur_mwh: f64,
}

/// Clear-sky output as a fraction of kWp: a bell between 06:00 and 20:00.
pub fn clear_sky(ts: DateTime<Utc>) -> f64 {
    let h = hour_of_day(ts);
    if (6.0..20.0).contains(&h) {
        (PI * (h - 6.0) / 14.0).sin()
    } else {
        0.0
    }
}

/// Mean per PTU-of-day, built with a fold over the history.
fn profile(history: &[Telemetry], value: impl Fn(&Telemetry) -> f64) -> (HashMap<usize, f64>, f64) {
    let mut acc: HashMap<usize, (f64, u32)> = HashMap::new();
    for t in history {
        let e = acc.entry(ptu_of_day(t.ts)).or_default();
        e.0 += value(t);
        e.1 += 1;
    }
    let overall = if history.is_empty() {
        0.0
    } else {
        history.iter().map(&value).sum::<f64>() / history.len() as f64
    };
    let means = acc
        .into_iter()
        .map(|(k, (s, n))| (k, s / n as f64))
        .collect();
    (means, overall)
}

/// Observed / clear-sky ratio over recent daylight readings (1.0 = perfectly sunny).
fn cloudiness(info: &SiteInfo, history: &[Telemetry]) -> f64 {
    let ratios: Vec<f64> = history
        .iter()
        .rev()
        .filter(|t| clear_sky(t.ts) > 0.2)
        .take(4)
        .map(|t| t.pv_kw / (info.pv_kwp * clear_sky(t.ts)))
        .collect();
    if ratios.is_empty() || info.pv_kwp <= 0.0 {
        0.8
    } else {
        (ratios.iter().sum::<f64>() / ratios.len() as f64).clamp(0.1, 1.1)
    }
}

pub fn naive_forecast(
    info: &SiteInfo,
    history: &[Telemetry],
    from: DateTime<Utc>,
    slots: usize,
) -> Vec<SlotForecast> {
    let (load_profile, load_mean) = profile(history, |t| t.load_kw);
    let (ev_profile, ev_mean) = profile(history, |t| t.ev_demand_kw);
    let ratio_now = cloudiness(info, history);

    (0..slots)
        .map(|i| {
            let start = from + ptu() * i as i32;
            let k = ptu_of_day(start);
            // Trust the current cloudiness for ~2 hours, then drift toward a climatological 0.8.
            let w = (i as f64 / 8.0).min(1.0);
            let ratio = ratio_now * (1.0 - w) + 0.8 * w;
            SlotForecast {
                start,
                pv_kw: info.pv_kwp * clear_sky(start) * ratio,
                load_kw: load_profile.get(&k).copied().unwrap_or(load_mean),
                ev_demand_kw: ev_profile.get(&k).copied().unwrap_or(ev_mean),
                price_eur_mwh: day_ahead_eur_mwh(start),
            }
        })
        .collect()
}
