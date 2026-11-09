//! The gateway's local brain. Three jobs:
//! 1. follow the cloud schedule when there is a valid one, otherwise a safe fallback rule;
//! 2. trade: when the imbalance price is favourable, deliberately deviate from the
//!    schedule with the battery (otherwise it stays on the plan the nomination came from);
//! 3. ALWAYS enforce the grid limits in real time, because forecasts are wrong and the
//!    cloud link can drop. The cloud optimises; the edge keeps the site safe.

use std::collections::VecDeque;

use chrono::{DateTime, Utc};
use ems_core::alloc::water_fill;
use ems_core::protocol::{AckStatus, GatewayMode, ScheduleCommand, SlotSetpoint};
use ems_core::Kw;
use uuid::Uuid;

use crate::sim::Observed;

/// How far the imbalance price must be from day-ahead before deviating is worth it.
const STEER_SPREAD_EUR_MWH: f64 = 40.0;
const STEER_KW: Kw = 20.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub import_kw: Kw,
    pub export_kw: Kw,
    pub margin_kw: Kw,
}

/// Extra battery power to trade on the imbalance price: `spread` is imbalance minus
/// day-ahead. Energy is expensive now, so sell; cheap now, so buy.
fn imbalance_steer(spread_eur_mwh: f64) -> Kw {
    if spread_eur_mwh > STEER_SPREAD_EUR_MWH {
        -STEER_KW
    } else if spread_eur_mwh < -STEER_SPREAD_EUR_MWH {
        STEER_KW
    } else {
        0.0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub battery_kw: Kw,
    pub ev_alloc: Vec<Kw>,
    pub mode: GatewayMode,
}

pub fn decide(
    obs: &Observed,
    slot: Option<&SlotSetpoint>,
    limits: Limits,
    imbalance_spread_eur_mwh: f64,
) -> Decision {
    let target = limits.import_kw - limits.margin_kw;
    let ev_demand: Kw = obs.ev_wants.iter().sum();

    let (battery_kw, ev_cap, mode) = match slot {
        Some(s) => (
            s.battery_kw + imbalance_steer(imbalance_spread_eur_mwh),
            s.ev_cap_kw,
            GatewayMode::Cloud,
        ),
        None => {
            // Fallback: soak up solar surplus, shave peaks, no price awareness.
            let net = obs.load_kw - obs.pv_kw + ev_demand;
            let b = if net < 0.0 {
                -net
            } else if net > target {
                -(net - target)
            } else {
                0.0
            };
            (b, f64::INFINITY, GatewayMode::Fallback)
        }
    };
    let mut battery_kw = battery_kw.clamp(-obs.max_discharge_kw, obs.max_charge_kw);

    // Real-time guard: if load is higher than planned, first stop charging / discharge more.
    let other = obs.load_kw - obs.pv_kw + battery_kw;
    if other > target {
        battery_kw = (battery_kw - (other - target)).max(-obs.max_discharge_kw);
    }
    // Then give the EVs whatever headroom remains (never more than the schedule allows).
    let other = obs.load_kw - obs.pv_kw + battery_kw;
    let ev_budget = ev_cap.min((target - other).max(0.0));
    let ev_alloc = water_fill(ev_budget, &obs.ev_wants);

    // Export guard: soak up what would go over the export limit, if the battery can.
    let grid = other + ev_alloc.iter().sum::<f64>();
    if grid < -limits.export_kw {
        battery_kw = (battery_kw - limits.export_kw - grid).min(obs.max_charge_kw);
    }

    Decision {
        battery_kw,
        ev_alloc,
        mode,
    }
}

/// Holds the active schedule and remembers recent command ids, which makes command
/// handling idempotent under QoS 1 redelivery and cloud-side retries.
#[derive(Default)]
pub struct ScheduleBook {
    active: Option<ScheduleCommand>,
    seen: VecDeque<Uuid>,
}

const REMEMBER: usize = 64;

impl ScheduleBook {
    pub fn offer(&mut self, cmd: ScheduleCommand, now: DateTime<Utc>) -> AckStatus {
        if self.seen.contains(&cmd.command_id) {
            return AckStatus::Duplicate;
        }
        self.seen.push_back(cmd.command_id);
        if self.seen.len() > REMEMBER {
            self.seen.pop_front();
        }
        if let Err(e) = cmd.validate() {
            return AckStatus::Rejected {
                reason: e.to_string(),
            };
        }
        if cmd.valid_until <= now {
            return AckStatus::Expired;
        }
        if self
            .active
            .as_ref()
            .is_some_and(|a| a.issued_at > cmd.issued_at)
        {
            return AckStatus::Superseded; // out-of-order delivery: keep the newer one
        }
        self.active = Some(cmd);
        AckStatus::Accepted
    }

    /// The setpoint for this PTU, if the active schedule is still valid.
    pub fn slot_for(&self, ptu: DateTime<Utc>) -> Option<&SlotSetpoint> {
        self.active.as_ref()?.slot_at(ptu)
    }

    pub fn active_id(&self) -> Option<Uuid> {
        self.active.as_ref().map(|c| c.command_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn obs(load: Kw, pv: Kw, evs: Vec<Kw>) -> Observed {
        Observed {
            pv_kw: pv,
            load_kw: load,
            ev_wants: evs,
            soc_kwh: Some(50.0),
            max_charge_kw: 50.0,
            max_discharge_kw: 50.0,
        }
    }

    fn limits(import_kw: Kw) -> Limits {
        Limits {
            import_kw,
            export_kw: 40.0,
            margin_kw: 0.0,
        }
    }

    fn cmd(issued_h: u32, valid_h: u32) -> ScheduleCommand {
        let t = |h| Utc.with_ymd_and_hms(2026, 11, 5, h, 0, 0).unwrap();
        ScheduleCommand {
            command_id: Uuid::new_v4(),
            site_id: "s".into(),
            issued_at: t(issued_h),
            valid_until: t(valid_h),
            slots: vec![SlotSetpoint {
                start: t(issued_h),
                battery_kw: 10.0,
                ev_cap_kw: 20.0,
            }],
        }
    }

    #[test]
    fn fallback_stays_under_limit() {
        let d = decide(&obs(90.0, 0.0, vec![22.0; 4]), None, limits(110.0), 0.0);
        assert_eq!(d.mode, GatewayMode::Fallback);
        let grid = 90.0 + d.battery_kw + d.ev_alloc.iter().sum::<f64>();
        assert!(grid <= 110.0 + 1e-9, "grid {grid}");
    }

    #[test]
    fn guard_overrides_a_stale_schedule() {
        // Schedule says "charge 50 kW, EVs may use 60 kW" but the load spiked.
        let slot = SlotSetpoint {
            start: Utc::now(),
            battery_kw: 50.0,
            ev_cap_kw: 60.0,
        };
        let d = decide(
            &obs(100.0, 0.0, vec![22.0, 22.0]),
            Some(&slot),
            limits(110.0),
            0.0,
        );
        let grid = 100.0 + d.battery_kw + d.ev_alloc.iter().sum::<f64>();
        assert!(grid <= 110.0 + 1e-9);
        assert_eq!(d.mode, GatewayMode::Cloud);
    }

    #[test]
    fn trades_the_battery_only_on_a_favourable_imbalance_price() {
        let slot = SlotSetpoint {
            start: Utc::now(),
            battery_kw: 0.0,
            ev_cap_kw: 0.0,
        };
        let battery =
            |spread| decide(&obs(50.0, 0.0, vec![]), Some(&slot), limits(110.0), spread).battery_kw;
        assert_eq!(battery(10.0), 0.0);
        assert_eq!(battery(60.0), -STEER_KW);
        assert_eq!(battery(-60.0), STEER_KW);
        // The fallback rule has no market knowledge.
        let d = decide(&obs(50.0, 0.0, vec![]), None, limits(110.0), 60.0);
        assert_eq!(d.battery_kw, 0.0);
    }

    #[test]
    fn guard_keeps_export_under_the_limit() {
        let slot = SlotSetpoint {
            start: Utc::now(),
            battery_kw: 0.0,
            ev_cap_kw: 0.0,
        };
        let d = decide(&obs(10.0, 80.0, vec![]), Some(&slot), limits(110.0), 0.0);
        assert!((d.battery_kw - 30.0).abs() < 1e-9, "{}", d.battery_kw);
    }

    #[test]
    fn commands_are_idempotent_and_ordered() {
        let mut book = ScheduleBook::default();
        let now = Utc.with_ymd_and_hms(2026, 11, 5, 9, 0, 0).unwrap();
        let newer = cmd(9, 11);
        assert_eq!(book.offer(newer.clone(), now), AckStatus::Accepted);
        assert_eq!(book.offer(newer.clone(), now), AckStatus::Duplicate);
        assert_eq!(book.offer(cmd(8, 11), now), AckStatus::Superseded);
        assert_eq!(book.offer(cmd(7, 8), now), AckStatus::Expired);
        assert_eq!(book.active_id(), Some(newer.command_id));
        assert!(book.slot_for(now).is_some());
        assert!(book.slot_for(now + Duration::hours(3)).is_none());
    }
}
