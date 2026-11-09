//! The peak-shaving scheduler: given a forecast, prices and a battery, choose a battery
//! power and an EV cap for every 15-minute slot so the site stays under its grid
//! connection limit at minimum cost.
//!
//! Method: **dynamic programming over a discretised state of charge.**
//! State = (slot t, SoC level k). Action = SoC level at the end of the slot.
//! Cost of a slot = energy cost + penalty for curtailed EV energy + big penalty for
//! exceeding the grid limit + a small battery-wear cost. Work backwards from the end
//! of the horizon, then walk forwards from today's SoC along the cheapest choices.
//!
//! O(T · L²): 192 slots × 41 levels² ≈ 320k evaluations, about a millisecond.
//! It's optimal *for the forecast*. The gateway still guards the limit in real time,
//! because forecasts are wrong.

use chrono::{DateTime, Utc};
use serde::Serialize;
use thiserror::Error;

use crate::forecast::SlotForecast;
use crate::prices::EXPORT_FEE_EUR_MWH;
use crate::protocol::SlotSetpoint;
use crate::time::PTU_HOURS;
use crate::{Kw, Kwh};

#[derive(Debug, Clone, PartialEq)]
pub struct BatteryState {
    pub capacity_kwh: Kwh,
    pub max_power_kw: Kw,
    pub efficiency: f64,
    pub soc_kwh: Kwh,
    /// Never plan below this (backup reserve).
    pub min_soc_kwh: Kwh,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CostModel {
    pub ev_curtail_eur_per_kwh: f64,
    pub overload_eur_per_kwh: f64,
    pub wear_eur_per_kwh: f64,
}

impl Default for CostModel {
    fn default() -> Self {
        CostModel {
            ev_curtail_eur_per_kwh: 0.30,
            overload_eur_per_kwh: 5.0,
            wear_eur_per_kwh: 0.01,
        }
    }
}

pub struct PlanInput<'a> {
    pub grid_limit_kw: Kw,
    pub export_limit_kw: Kw,
    pub safety_margin_kw: Kw,
    pub battery: Option<BatteryState>,
    pub forecast: &'a [SlotForecast],
    pub costs: CostModel,
    /// SoC discretisation (levels including 0 and full).
    pub levels: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlannedSlot {
    pub start: DateTime<Utc>,
    pub load_kw: Kw,
    pub pv_kw: Kw,
    pub ev_kw: Kw,
    pub battery_kw: Kw,
    pub ev_cap_kw: Kw,
    pub expected_grid_kw: Kw,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Plan {
    pub slots: Vec<PlannedSlot>,
    pub expected_cost_eur: f64,
    pub baseline_cost_eur: f64,
    pub expected_peak_kw: Kw,
    pub baseline_peak_kw: Kw,
}

impl Plan {
    /// What the gateway needs: the setpoints of the first `count` slots.
    pub fn setpoints(&self, count: usize) -> Vec<SlotSetpoint> {
        self.slots
            .iter()
            .take(count)
            .map(|s| SlotSetpoint {
                start: s.start,
                battery_kw: s.battery_kw,
                ev_cap_kw: s.ev_cap_kw,
            })
            .collect()
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum PlanError {
    #[error("forecast is empty")]
    EmptyForecast,
    #[error("invalid input: {0}")]
    InvalidInput(&'static str),
}

/// Outcome of running one slot at a given battery power.
struct SlotOutcome {
    cost: f64,
    grid_kw: Kw,
    ev_kw: Kw,
    ev_cap_kw: Kw,
}

fn energy_cost(grid_kw: Kw, price_eur_mwh: f64) -> f64 {
    let kwh = grid_kw * PTU_HOURS;
    if kwh >= 0.0 {
        kwh * price_eur_mwh / 1000.0
    } else {
        kwh * (price_eur_mwh - EXPORT_FEE_EUR_MWH) / 1000.0
    }
}

fn evaluate(f: &SlotForecast, battery_kw: Kw, input: &PlanInput<'_>) -> SlotOutcome {
    let target = input.grid_limit_kw - input.safety_margin_kw;
    let base = f.load_kw - f.pv_kw + battery_kw;
    let ev_cap_kw = (target - base).max(0.0);
    let ev = f.ev_demand_kw.min(ev_cap_kw);
    let curtailed_kw = f.ev_demand_kw - ev;
    let grid_kw = base + ev;
    let overload_kw =
        (grid_kw - input.grid_limit_kw).max(0.0) + (-grid_kw - input.export_limit_kw).max(0.0);
    let c = &input.costs;
    let cost = energy_cost(grid_kw, f.price_eur_mwh)
        + curtailed_kw * PTU_HOURS * c.ev_curtail_eur_per_kwh
        + overload_kw * PTU_HOURS * c.overload_eur_per_kwh
        + battery_kw.abs() * PTU_HOURS * c.wear_eur_per_kwh;
    SlotOutcome {
        cost,
        grid_kw,
        ev_kw: ev,
        ev_cap_kw,
    }
}

/// Grid-side power needed to move the battery from `from` to `to` kWh in one PTU.
fn battery_power(from: Kwh, to: Kwh, efficiency: f64) -> Kw {
    let delta = to - from;
    if delta >= 0.0 {
        delta / (efficiency * PTU_HOURS)
    } else {
        delta * efficiency / PTU_HOURS
    }
}

pub fn plan(input: &PlanInput<'_>) -> Result<Plan, PlanError> {
    let fc = input.forecast;
    if fc.is_empty() {
        return Err(PlanError::EmptyForecast);
    }
    // Written this way so NaN is rejected too (NaN > 0.0 is false).
    if input.grid_limit_kw.is_nan() || input.grid_limit_kw <= 0.0 {
        return Err(PlanError::InvalidInput("grid_limit_kw must be > 0"));
    }

    // Battery states. No battery = a single level at 0 kWh, so the DP still works.
    let (levels, soc_of, eff, max_p, start_soc) = match &input.battery {
        Some(b) if b.capacity_kwh > 0.0 && input.levels >= 2 => {
            if !(0.0..=1.0).contains(&b.efficiency) || b.efficiency == 0.0 {
                return Err(PlanError::InvalidInput(
                    "battery efficiency must be in (0, 1]",
                ));
            }
            let l = input.levels;
            let cap = b.capacity_kwh;
            (
                l,
                (0..l)
                    .map(|k| cap * k as f64 / (l - 1) as f64)
                    .collect::<Vec<_>>(),
                b.efficiency,
                b.max_power_kw,
                b.soc_kwh,
            )
        }
        _ => (1, vec![0.0], 1.0, 0.0, 0.0),
    };
    let min_soc = input.battery.as_ref().map_or(0.0, |b| b.min_soc_kwh);
    let avg_price = fc.iter().map(|f| f.price_eur_mwh).sum::<f64>() / fc.len() as f64;

    let t_len = fc.len();
    // value[k] = min cost from slot t to the end, starting at level k.
    // Terminal value: stored energy is worth ~ the average price (stops the plan dumping it).
    let mut value: Vec<f64> = soc_of
        .iter()
        .map(|soc| -soc * eff * avg_price / 1000.0)
        .collect();
    let mut choice = vec![vec![0usize; levels]; t_len];

    for t in (0..t_len).rev() {
        let mut next_value = vec![f64::INFINITY; levels];
        for k in 0..levels {
            for j in 0..levels {
                if soc_of[j] + 1e-9 < min_soc && j < k {
                    continue; // may not discharge below the reserve
                }
                let p = battery_power(soc_of[k], soc_of[j], eff);
                if p.abs() > max_p + 1e-9 {
                    continue;
                }
                let total = evaluate(&fc[t], p, input).cost + value[j];
                if total < next_value[k] {
                    next_value[k] = total;
                    choice[t][k] = j;
                }
            }
        }
        value = next_value;
    }

    // Walk forward from the level closest to the current SoC.
    let mut k = (0..levels)
        .min_by(|&a, &b| {
            (soc_of[a] - start_soc)
                .abs()
                .total_cmp(&(soc_of[b] - start_soc).abs())
        })
        .unwrap_or(0);
    let mut slots = Vec::with_capacity(t_len);
    let (mut cost, mut base_cost, mut peak, mut base_peak) = (0.0, 0.0, f64::MIN, f64::MIN);
    for (t, f) in fc.iter().enumerate() {
        let j = choice[t][k];
        let p = battery_power(soc_of[k], soc_of[j], eff);
        let o = evaluate(f, p, input);
        // What the grid would do with no battery and no EV control.
        let baseline_grid_kw = f.load_kw - f.pv_kw + f.ev_demand_kw;
        cost += energy_cost(o.grid_kw, f.price_eur_mwh);
        base_cost += energy_cost(baseline_grid_kw, f.price_eur_mwh);
        peak = peak.max(o.grid_kw);
        base_peak = base_peak.max(baseline_grid_kw);
        slots.push(PlannedSlot {
            start: f.start,
            load_kw: f.load_kw,
            pv_kw: f.pv_kw,
            ev_kw: o.ev_kw,
            battery_kw: p,
            ev_cap_kw: o.ev_cap_kw,
            expected_grid_kw: o.grid_kw,
        });
        k = j;
    }

    Ok(Plan {
        slots,
        expected_cost_eur: cost,
        baseline_cost_eur: base_cost,
        expected_peak_kw: peak,
        baseline_peak_kw: base_peak,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::ptu;
    use chrono::TimeZone;

    fn fc(n: usize, f: impl Fn(usize) -> (Kw, Kw, Kw, f64)) -> Vec<SlotForecast> {
        let t0 = Utc.with_ymd_and_hms(2026, 11, 5, 0, 0, 0).unwrap();
        (0..n)
            .map(|i| {
                let (load, pv, ev, price) = f(i);
                SlotForecast {
                    start: t0 + ptu() * i as i32,
                    pv_kw: pv,
                    load_kw: load,
                    ev_demand_kw: ev,
                    price_eur_mwh: price,
                }
            })
            .collect()
    }

    fn battery(soc: Kwh) -> BatteryState {
        BatteryState {
            capacity_kwh: 100.0,
            max_power_kw: 50.0,
            efficiency: 0.95,
            soc_kwh: soc,
            min_soc_kwh: 0.0,
        }
    }

    fn input(forecast: &[SlotForecast], battery: Option<BatteryState>, limit: Kw) -> PlanInput<'_> {
        PlanInput {
            grid_limit_kw: limit,
            export_limit_kw: 1000.0,
            safety_margin_kw: 0.0,
            battery,
            forecast,
            costs: CostModel::default(),
            levels: 41,
        }
    }

    #[test]
    fn shaves_a_peak_with_the_battery() {
        // Flat 80 kW, with a one-hour 140 kW peak; limit 100 kW.
        let f = fc(16, |i| {
            (
                if (8..12).contains(&i) { 140.0 } else { 80.0 },
                0.0,
                0.0,
                100.0,
            )
        });
        let plan = plan(&input(&f, Some(battery(80.0)), 100.0)).unwrap();
        assert!(plan.baseline_peak_kw > 100.0);
        assert!(
            plan.expected_peak_kw <= 100.0 + 1e-6,
            "peak {}",
            plan.expected_peak_kw
        );
    }

    #[test]
    fn buys_cheap_and_sells_expensive() {
        let f = fc(8, |i| (10.0, 0.0, 0.0, if i < 4 { 20.0 } else { 300.0 }));
        let plan = plan(&input(&f, Some(battery(0.0)), 500.0)).unwrap();
        assert!(plan.slots[..4].iter().all(|s| s.battery_kw >= 0.0));
        assert!(plan.slots[4..].iter().any(|s| s.battery_kw < 0.0));
        assert!(plan.expected_cost_eur < plan.baseline_cost_eur);
    }

    #[test]
    fn caps_evs_when_there_is_no_battery() {
        let f = fc(4, |_| (80.0, 0.0, 66.0, 100.0));
        let plan = plan(&input(&f, None, 120.0)).unwrap();
        for s in &plan.slots {
            assert!((s.ev_cap_kw - 40.0).abs() < 1e-9);
            assert!(s.expected_grid_kw <= 120.0 + 1e-9);
            assert!(s.ev_kw < 66.0, "demand above the cap is curtailed");
        }
    }

    #[test]
    fn stores_solar_rather_than_breaching_the_export_limit() {
        let f = fc(4, |_| (10.0, 70.0, 0.0, 100.0)); // 60 kW surplus
        let mut input = input(&f, Some(battery(0.0)), 100.0);
        input.export_limit_kw = 30.0;
        let plan = plan(&input).unwrap();
        assert!(plan
            .slots
            .iter()
            .all(|s| s.expected_grid_kw >= -30.0 - 1e-6));
    }

    #[test]
    fn never_exceeds_battery_power() {
        let f = fc(96, |i| {
            (
                50.0 + (i % 7) as f64 * 20.0,
                (i % 5) as f64 * 15.0,
                (i % 3) as f64 * 22.0,
                50.0 + (i % 11) as f64 * 20.0,
            )
        });
        let plan = plan(&input(&f, Some(battery(50.0)), 120.0)).unwrap();
        assert!(plan.slots.iter().all(|s| s.battery_kw.abs() <= 50.0 + 1e-6));
    }

    #[test]
    fn setpoints_only_cover_the_requested_slots() {
        let f = fc(96, |_| (10.0, 0.0, 0.0, 100.0));
        let plan = plan(&input(&f, None, 100.0)).unwrap();
        assert_eq!(plan.setpoints(8).len(), 8);
        assert_eq!(plan.setpoints(8)[0].start, plan.slots[0].start);
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(
            plan(&input(&[], None, 100.0)),
            Err(PlanError::EmptyForecast)
        );
        let f = fc(1, |_| (1.0, 0.0, 0.0, 1.0));
        assert!(matches!(
            plan(&input(&f, None, 0.0)),
            Err(PlanError::InvalidInput(_))
        ));
    }
}
