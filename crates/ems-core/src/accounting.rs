//! Turning telemetry into money and KPIs. Pure functions over any period, used by the
//! energy bill and the reports.
//!
//! The site buys its expected consumption a day ahead: the **nomination**. It pays the
//! day-ahead price for that position and the imbalance price for whatever it does
//! differently. Nominating per asset makes the bill add up per asset too:
//!
//! ```text
//! asset cost = nominated × day-ahead + (actual - nominated) × imbalance
//! ```

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;

use crate::prices::{co2_g_kwh, day_ahead_eur_mwh, imbalance_eur_mwh, ptu_cost_eur};
use crate::protocol::{GatewayMode, SiteInfo, Telemetry};
use crate::time::PTU_HOURS;
use crate::Kw;

pub const ENERGY_TAX_EUR_PER_KWH: f64 = 0.05;
/// Network fee for the contracted capacity.
pub const TRANSPORT_EUR_PER_KW_DAY: f64 = 0.20;
/// Credits earned per kWh charged into EVs (ERE in the Netherlands), shown as a subsidy.
pub const EV_CREDIT_EUR_PER_KWH: f64 = 0.06;

/// The position bought for one PTU, split by asset (same signs as `Telemetry`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Nomination {
    pub load_kw: Kw,
    pub pv_kw: Kw,
    pub ev_kw: Kw,
    pub battery_kw: Kw,
}

impl Nomination {
    pub fn grid_kw(&self) -> Kw {
        self.load_kw + self.ev_kw + self.battery_kw - self.pv_kw
    }
}

/// Energy cost of one PTU in euros. Measured PTUs settle against the nomination; future
/// ones are a forecast of the nominated position at day-ahead prices.
pub fn ptu_energy_eur(
    ts: DateTime<Utc>,
    nominated_kw: Option<Kw>,
    measured_kw: Option<Kw>,
) -> Option<f64> {
    let nominated = nominated_kw.or(measured_kw)?;
    let deviation = measured_kw.map_or(0.0, |m| m - nominated);
    let eur_mwh = nominated * day_ahead_eur_mwh(ts) + deviation * imbalance_eur_mwh(ts);
    Some(eur_mwh * PTU_HOURS / 1000.0)
}

/// Where the power came from and went to in one PTU (kW), or a sum of them (kWh).
/// Meters can't tell which electron went where, so every source is shared
/// proportionally over all destinations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Flows {
    pub grid_to_load: f64,
    pub grid_to_ev: f64,
    pub grid_to_battery: f64,
    pub pv_to_load: f64,
    pub pv_to_ev: f64,
    pub pv_to_battery: f64,
    pub pv_to_grid: f64,
    pub battery_to_load: f64,
    pub battery_to_ev: f64,
    pub battery_to_grid: f64,
}

impl Flows {
    pub fn of(t: &Telemetry) -> Self {
        let (import, export) = (t.grid_kw.max(0.0), (-t.grid_kw).max(0.0));
        let (charge, discharge) = (t.battery_kw.max(0.0), (-t.battery_kw).max(0.0));
        let supply = import + t.pv_kw + discharge;
        if supply <= 0.0 {
            return Flows::default();
        }
        let share = |source: Kw, sink: Kw| source * sink / supply;
        Flows {
            grid_to_load: share(import, t.load_kw),
            grid_to_ev: share(import, t.ev_kw),
            grid_to_battery: share(import, charge),
            pv_to_load: share(t.pv_kw, t.load_kw),
            pv_to_ev: share(t.pv_kw, t.ev_kw),
            pv_to_battery: share(t.pv_kw, charge),
            pv_to_grid: share(t.pv_kw, export),
            battery_to_load: share(discharge, t.load_kw),
            battery_to_ev: share(discharge, t.ev_kw),
            battery_to_grid: share(discharge, export),
        }
    }

    fn add_energy(&mut self, f: Flows) {
        self.grid_to_load += f.grid_to_load * PTU_HOURS;
        self.grid_to_ev += f.grid_to_ev * PTU_HOURS;
        self.grid_to_battery += f.grid_to_battery * PTU_HOURS;
        self.pv_to_load += f.pv_to_load * PTU_HOURS;
        self.pv_to_ev += f.pv_to_ev * PTU_HOURS;
        self.pv_to_battery += f.pv_to_battery * PTU_HOURS;
        self.pv_to_grid += f.pv_to_grid * PTU_HOURS;
        self.battery_to_load += f.battery_to_load * PTU_HOURS;
        self.battery_to_ev += f.battery_to_ev * PTU_HOURS;
        self.battery_to_grid += f.battery_to_grid * PTU_HOURS;
    }
}

/// One line of the energy bill. Consumption is positive, so solar comes out negative.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct AssetCost {
    pub energy_kwh: f64,
    pub nomination_eur: f64,
    pub imbalance_eur: f64,
}

impl AssetCost {
    fn add(&mut self, ts: DateTime<Utc>, actual_kw: Kw, nominated_kw: Kw) {
        self.energy_kwh += actual_kw * PTU_HOURS;
        self.nomination_eur += nominated_kw * PTU_HOURS * day_ahead_eur_mwh(ts) / 1000.0;
        self.imbalance_eur +=
            (actual_kw - nominated_kw) * PTU_HOURS * imbalance_eur_mwh(ts) / 1000.0;
    }

    fn total_eur(&self) -> f64 {
        self.nomination_eur + self.imbalance_eur
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Bill {
    pub solar: AssetCost,
    pub battery: AssetCost,
    pub charger: AssetCost,
    pub load: AssetCost,
    pub energy_tax_eur: f64,
    pub transport_eur: f64,
    /// Negative: this is income.
    pub subsidies_eur: f64,
}

impl Bill {
    fn add(&mut self, t: &Telemetry, nomination: Option<&Nomination>, info: &SiteInfo) {
        // A PTU without a nomination (just after a restart) simply has no deviation.
        let n = nomination.copied().unwrap_or(Nomination {
            load_kw: t.load_kw,
            pv_kw: t.pv_kw,
            ev_kw: t.ev_kw,
            battery_kw: t.battery_kw,
        });
        self.solar.add(t.ts, -t.pv_kw, -n.pv_kw);
        self.battery.add(t.ts, t.battery_kw, n.battery_kw);
        self.charger.add(t.ts, t.ev_kw, n.ev_kw);
        self.load.add(t.ts, t.load_kw, n.load_kw);
        self.energy_tax_eur += t.grid_kw.max(0.0) * PTU_HOURS * ENERGY_TAX_EUR_PER_KWH;
        self.transport_eur += info.grid_limit_kw * TRANSPORT_EUR_PER_KW_DAY * PTU_HOURS / 24.0;
        self.subsidies_eur -= t.ev_kw * PTU_HOURS * EV_CREDIT_EUR_PER_KWH;
    }

    pub fn total_eur(&self) -> f64 {
        [self.solar, self.battery, self.charger, self.load]
            .iter()
            .map(AssetCost::total_eur)
            .sum::<f64>()
            + self.energy_tax_eur
            + self.transport_eur
            + self.subsidies_eur
    }
}

/// KPIs over a period. "Baseline" is the same site without an EMS: no battery and EVs
/// charging at full demand.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Report {
    pub ptus: u64,
    pub cloud_mode_ptus: u64,
    pub import_kwh: f64,
    pub export_kwh: f64,
    pub flows_kwh: Flows,
    pub actual_cost_eur: f64,
    pub baseline_cost_eur: f64,
    pub actual_peak_kw: Kw,
    pub baseline_peak_kw: Kw,
    pub over_limit_ptus: u64,
    pub baseline_over_limit_ptus: u64,
    pub ev_curtailed_kwh: f64,
    pub co2_kg: f64,
    /// Compared with buying everything from the grid: no solar, no battery.
    pub co2_reduction_kg: f64,
}

impl Report {
    fn add(&mut self, t: &Telemetry, info: &SiteInfo) {
        let baseline = t.load_kw + t.ev_demand_kw - t.pv_kw;
        let over = |kw: Kw| kw > info.grid_limit_kw + 1e-6 || kw < -info.export_limit_kw - 1e-6;
        let kg_per_kwh = co2_g_kwh(t.ts) / 1000.0;
        let imported = t.grid_kw.max(0.0) * PTU_HOURS;

        self.ptus += 1;
        self.cloud_mode_ptus += u64::from(t.mode == GatewayMode::Cloud);
        self.import_kwh += imported;
        self.export_kwh += (-t.grid_kw).max(0.0) * PTU_HOURS;
        self.flows_kwh.add_energy(Flows::of(t));
        self.actual_cost_eur += ptu_cost_eur(t.ts, t.grid_kw);
        self.baseline_cost_eur += ptu_cost_eur(t.ts, baseline);
        self.actual_peak_kw = self.actual_peak_kw.max(t.grid_kw);
        self.baseline_peak_kw = self.baseline_peak_kw.max(baseline);
        self.over_limit_ptus += u64::from(over(t.grid_kw));
        self.baseline_over_limit_ptus += u64::from(over(baseline));
        self.ev_curtailed_kwh += (t.ev_demand_kw - t.ev_kw).max(0.0) * PTU_HOURS;
        self.co2_kg += imported * kg_per_kwh;
        self.co2_reduction_kg += ((t.load_kw + t.ev_kw) * PTU_HOURS - imported) * kg_per_kwh;
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DayTotal {
    pub date: NaiveDate,
    pub total_eur: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Summary {
    pub bill: Bill,
    pub report: Report,
    /// The bill total per day, for the chart view.
    pub days: Vec<DayTotal>,
}

pub fn summarize<'a>(
    readings: impl Iterator<Item = &'a Telemetry>,
    nominations: &BTreeMap<DateTime<Utc>, Nomination>,
    info: &SiteInfo,
) -> Summary {
    let mut summary = Summary::default();
    let mut bills: BTreeMap<NaiveDate, Bill> = BTreeMap::new();
    for t in readings {
        let nomination = nominations.get(&t.ts);
        summary.bill.add(t, nomination, info);
        summary.report.add(t, info);
        bills
            .entry(t.ts.date_naive())
            .or_default()
            .add(t, nomination, info);
    }
    summary.days = bills
        .into_iter()
        .map(|(date, bill)| DayTotal {
            date,
            total_eur: bill.total_eur(),
        })
        .collect();
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use uuid::Uuid;

    fn info() -> SiteInfo {
        SiteInfo {
            site_id: "s".into(),
            name: "Site".into(),
            grid_limit_kw: 100.0,
            export_limit_kw: 50.0,
            pv_kwp: 100.0,
            battery: None,
            chargers: 2,
            charger_kw: 22.0,
            firmware: "test".into(),
        }
    }

    /// Reading with a consistent energy balance.
    fn reading(load: Kw, pv: Kw, ev: Kw, battery: Kw) -> Telemetry {
        Telemetry {
            site_id: "s".into(),
            boot_id: Uuid::nil(),
            seq: 1,
            ts: Utc.with_ymd_and_hms(2026, 11, 5, 12, 0, 0).unwrap(),
            grid_kw: load + ev + battery - pv,
            pv_kw: pv,
            load_kw: load,
            ev_kw: ev,
            ev_demand_kw: ev,
            battery_kw: battery,
            battery_soc_kwh: None,
            mode: GatewayMode::Cloud,
            schedule_id: None,
        }
    }

    #[test]
    fn flows_conserve_energy() {
        // 40 kW of solar and 20 kW of battery cover 50 kW of load; 10 kW is exported.
        let f = Flows::of(&reading(50.0, 40.0, 0.0, -20.0));
        assert!((f.pv_to_load + f.battery_to_load - 50.0).abs() < 1e-9);
        assert!((f.pv_to_grid + f.battery_to_grid - 10.0).abs() < 1e-9);
        assert_eq!(f.grid_to_load, 0.0);
    }

    #[test]
    fn asset_lines_add_up_to_the_grid_settlement() {
        let t = reading(50.0, 20.0, 30.0, 10.0);
        let n = Nomination {
            load_kw: 40.0,
            pv_kw: 30.0,
            ev_kw: 10.0,
            battery_kw: 0.0,
        };
        let nominations = BTreeMap::from([(t.ts, n)]);
        let bill = summarize([&t].into_iter(), &nominations, &info()).bill;
        let assets = bill.solar.total_eur()
            + bill.battery.total_eur()
            + bill.charger.total_eur()
            + bill.load.total_eur();
        let settled = ptu_energy_eur(t.ts, Some(n.grid_kw()), Some(t.grid_kw)).unwrap();
        assert!((assets - settled).abs() < 1e-9);
        assert!(bill.solar.energy_kwh < 0.0, "solar is generation");
    }

    #[test]
    fn following_the_nomination_has_no_imbalance_cost() {
        let t = reading(50.0, 20.0, 30.0, 0.0);
        let bill = summarize([&t].into_iter(), &BTreeMap::new(), &info()).bill;
        assert_eq!(bill.load.imbalance_eur, 0.0);
    }

    #[test]
    fn future_ptus_are_a_forecast_at_day_ahead() {
        let ts = reading(0.0, 0.0, 0.0, 0.0).ts;
        let expected = 100.0 * PTU_HOURS * day_ahead_eur_mwh(ts) / 1000.0;
        assert_eq!(ptu_energy_eur(ts, Some(100.0), None), Some(expected));
        assert_eq!(ptu_energy_eur(ts, None, None), None);
    }

    #[test]
    fn counts_violations_in_both_directions() {
        let mut high = reading(150.0, 0.0, 0.0, -60.0);
        high.ev_demand_kw = 10.0; // baseline 160 kW > 100 kW, actual 90 kW is fine
        let export = reading(0.0, 80.0, 0.0, 0.0); // -80 kW beyond the 50 kW export limit
        let r = summarize([&high, &export].into_iter(), &BTreeMap::new(), &info()).report;
        assert_eq!(r.baseline_over_limit_ptus, 2);
        assert_eq!(r.over_limit_ptus, 1);
    }
}
