//! Synthetic market data, shaped like a sunny Dutch day: a morning bump, cheap or even
//! negative midday (solar floods the grid), a big evening peak. Everything is a pure
//! function of time, so the cloud and the gateways agree without exchanging prices.
//! Swap this for a real feed (ENTSO-E transparency API) as an extension.

use chrono::{DateTime, Utc};

use crate::time::{hour_of_day, PTU_HOURS, PTU_MINUTES};

/// Supplier fee on exported energy.
pub const EXPORT_FEE_EUR_MWH: f64 = 15.0;

fn bump(x: f64, mu: f64, sigma: f64) -> f64 {
    (-((x - mu) / sigma).powi(2) / 2.0).exp()
}

/// Deterministic noise in -1..1, a new value every PTU.
fn noise(ts: DateTime<Utc>) -> f64 {
    let ptu = (ts.timestamp() / (PTU_MINUTES * 60)) as u64;
    let hash = ptu.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 11;
    hash as f64 / (1u64 << 52) as f64 - 1.0
}

pub fn day_ahead_eur_mwh(ts: DateTime<Utc>) -> f64 {
    let h = hour_of_day(ts);
    85.0 + 45.0 * bump(h, 8.0, 1.5) + 110.0 * bump(h, 19.0, 2.0) - 95.0 * bump(h, 13.5, 2.5)
}

/// Preliminary imbalance price: what deviating from your nomination costs (or earns)
/// in this PTU. In reality TenneT publishes it minutes after the fact.
pub fn imbalance_eur_mwh(ts: DateTime<Utc>) -> f64 {
    day_ahead_eur_mwh(ts) + 70.0 * noise(ts)
}

/// Final `(import, export)` imbalance prices, known a day later.
pub fn settlement_eur_mwh(ts: DateTime<Utc>) -> (f64, f64) {
    let price = imbalance_eur_mwh(ts);
    (price + 8.0, price - 8.0)
}

/// CO2 intensity of grid electricity (g/kWh): lowest when solar floods the grid.
pub fn co2_g_kwh(ts: DateTime<Utc>) -> f64 {
    let h = hour_of_day(ts);
    380.0 - 330.0 * bump(h, 13.5, 3.5) + 10.0 * noise(ts)
}

/// Cost of one PTU at `grid_kw` on a plain dynamic contract (negative = revenue).
pub fn ptu_cost_eur(ts: DateTime<Utc>, grid_kw: f64) -> f64 {
    let kwh = grid_kw * PTU_HOURS;
    let eur_mwh = day_ahead_eur_mwh(ts);
    let eur_mwh = if kwh >= 0.0 {
        eur_mwh
    } else {
        eur_mwh - EXPORT_FEE_EUR_MWH
    };
    kwh * eur_mwh / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::ptu;
    use chrono::TimeZone;

    #[test]
    fn imbalance_stays_near_day_ahead_but_moves_every_ptu() {
        let t0 = Utc.with_ymd_and_hms(2026, 11, 5, 0, 0, 0).unwrap();
        let spreads: Vec<f64> = (0..96)
            .map(|i| t0 + ptu() * i)
            .map(|t| imbalance_eur_mwh(t) - day_ahead_eur_mwh(t))
            .collect();
        assert!(spreads.iter().all(|s| s.abs() <= 70.0));
        assert!(spreads.iter().any(|s| *s > 30.0));
        assert!(spreads.iter().any(|s| *s < -30.0));
    }

    #[test]
    fn grid_is_cleanest_at_midday() {
        let at = |h| Utc.with_ymd_and_hms(2026, 11, 5, h, 0, 0).unwrap();
        assert!(co2_g_kwh(at(13)) < co2_g_kwh(at(3)) / 2.0);
    }
}
