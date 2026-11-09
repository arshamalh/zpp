//! Fake physics for one site: base load, solar with drifting clouds, a battery, and
//! EV chargers with arrival/departure sessions. Synchronous and deterministic per seed.

use chrono::{DateTime, NaiveDate, Utc};
use ems_core::forecast::clear_sky;
use ems_core::protocol::{BatterySpec, SiteInfo};
use ems_core::time::{hour_of_day, ptu_of_day, PTUS_PER_DAY, PTU_HOURS};
use ems_core::{Kw, Kwh};

#[derive(Debug, Clone)]
pub struct Profile {
    pub name: &'static str,
    pub grid_limit_kw: Kw,
    pub export_limit_kw: Kw,
    pub pv_kwp: Kw,
    pub base_load_kw: Kw,
    pub battery: Option<(Kwh, Kw)>,
    pub chargers: u32,
    pub charger_kw: Kw,
}

pub const PRESETS: &[&str] = &["depot", "campus", "cold-storage"];

pub fn preset(name: &str) -> Option<Profile> {
    let p = match name {
        "depot" => Profile {
            name: "Logistics depot",
            grid_limit_kw: 110.0,
            export_limit_kw: 40.0,
            pv_kwp: 120.0,
            base_load_kw: 60.0,
            battery: Some((250.0, 100.0)),
            chargers: 12,
            charger_kw: 22.0,
        },
        "campus" => Profile {
            name: "Campus",
            grid_limit_kw: 170.0,
            export_limit_kw: 350.0,
            pv_kwp: 500.0,
            base_load_kw: 140.0,
            battery: None,
            chargers: 20,
            charger_kw: 11.0,
        },
        "cold-storage" => Profile {
            name: "Cold storage",
            grid_limit_kw: 120.0,
            export_limit_kw: 40.0,
            pv_kwp: 120.0,
            base_load_kw: 100.0,
            battery: Some((200.0, 100.0)),
            chargers: 4,
            charger_kw: 22.0,
        },
        _ => return None,
    };
    Some(p)
}

/// Tiny xorshift PRNG so the simulator needs no extra crate.
#[derive(Debug, Clone)]
struct Rng(u64);

impl Rng {
    fn next_f64(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.next_f64()
    }
}

#[derive(Debug, Clone)]
struct Session {
    arrive: usize,
    depart: usize,
    remaining: Kwh,
}

/// What the gateway "measures" at the start of a PTU.
#[derive(Debug, Clone)]
pub struct Observed {
    pub pv_kw: Kw,
    pub load_kw: Kw,
    /// Per-charger demand (0 = no car).
    pub ev_wants: Vec<Kw>,
    pub soc_kwh: Option<Kwh>,
    pub max_charge_kw: Kw,
    pub max_discharge_kw: Kw,
}

pub struct SiteSim {
    site_id: String,
    profile: Profile,
    rng: Rng,
    soc: Kwh,
    efficiency: f64,
    sessions: Vec<Vec<Session>>,
    day: Option<NaiveDate>,
    cloud: f64,
    ptu: usize,
}

impl SiteSim {
    pub fn new(site_id: String, profile: Profile, seed: u64) -> Self {
        let soc = profile.battery.map_or(0.0, |(cap, _)| cap * 0.5);
        SiteSim {
            site_id,
            profile,
            rng: Rng(seed.max(1)),
            soc,
            efficiency: 0.95,
            sessions: Vec::new(),
            day: None,
            cloud: 0.9,
            ptu: 0,
        }
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    pub fn info(&self) -> SiteInfo {
        SiteInfo {
            site_id: self.site_id.clone(),
            name: self.profile.name.to_owned(),
            grid_limit_kw: self.profile.grid_limit_kw,
            export_limit_kw: self.profile.export_limit_kw,
            pv_kwp: self.profile.pv_kwp,
            battery: self
                .profile
                .battery
                .map(|(capacity_kwh, max_power_kw)| BatterySpec {
                    capacity_kwh,
                    max_power_kw,
                    efficiency: self.efficiency,
                }),
            chargers: self.profile.chargers,
            charger_kw: self.profile.charger_kw,
            firmware: concat!("gateway-sim/", env!("CARGO_PKG_VERSION")).to_owned(),
        }
    }

    fn new_day(&mut self) {
        let ptu = |h: f64| ((h / PTU_HOURS) as usize).min(PTUS_PER_DAY);
        let mut all = Vec::new();
        for _ in 0..self.profile.chargers {
            let mut v = Vec::new();
            let a = self.rng.range(7.0, 10.0);
            let stay = self.rng.range(3.0, 6.0);
            v.push(Session {
                arrive: ptu(a),
                depart: ptu(a + stay),
                remaining: self.rng.range(20.0, 60.0),
            });
            if self.rng.next_f64() < 0.5 {
                let a = self.rng.range(15.0, 17.0);
                let stay = self.rng.range(2.0, 5.0);
                v.push(Session {
                    arrive: ptu(a),
                    depart: ptu(a + stay),
                    remaining: self.rng.range(15.0, 40.0),
                });
            }
            all.push(v);
        }
        self.sessions = all;
    }

    fn battery_limits(&self) -> (Kw, Kw) {
        match self.profile.battery {
            Some((cap, power)) => (
                power.min((cap - self.soc).max(0.0) / (PTU_HOURS * self.efficiency)),
                power.min(self.soc * self.efficiency / PTU_HOURS),
            ),
            None => (0.0, 0.0),
        }
    }

    pub fn observe(&mut self, ts: DateTime<Utc>) -> Observed {
        if self.day != Some(ts.date_naive()) {
            self.day = Some(ts.date_naive());
            self.new_day();
        }
        self.ptu = ptu_of_day(ts);
        self.cloud = (self.cloud + self.rng.range(-0.08, 0.08)).clamp(0.25, 1.0);
        let pv_kw = self.profile.pv_kwp * clear_sky(ts) * self.cloud;
        let working = (7.0..19.0).contains(&hour_of_day(ts));
        let load_kw =
            self.profile.base_load_kw * if working { 1.4 } else { 0.5 } * self.rng.range(0.9, 1.1);
        let (p, max_kw) = (self.ptu, self.profile.charger_kw);
        let ev_wants = self
            .sessions
            .iter()
            .map(|sessions| {
                sessions
                    .iter()
                    .find(|s| (s.arrive..s.depart).contains(&p))
                    .map_or(0.0, |s| (s.remaining / PTU_HOURS).clamp(0.0, max_kw))
            })
            .collect();
        let (max_charge_kw, max_discharge_kw) = self.battery_limits();
        Observed {
            pv_kw,
            load_kw,
            ev_wants,
            soc_kwh: self.profile.battery.map(|_| self.soc),
            max_charge_kw,
            max_discharge_kw,
        }
    }

    /// Run the PTU. Returns the battery power actually achieved (clamped to physics).
    pub fn apply(&mut self, battery_kw: Kw, ev_alloc: &[Kw]) -> Kw {
        let (max_c, max_d) = self.battery_limits();
        let p = battery_kw.clamp(-max_d, max_c);
        if let Some((cap, _)) = self.profile.battery {
            self.soc += if p >= 0.0 {
                p * PTU_HOURS * self.efficiency
            } else {
                p * PTU_HOURS / self.efficiency
            };
            self.soc = self.soc.clamp(0.0, cap);
        }
        let now = self.ptu;
        for (sessions, &kw) in self.sessions.iter_mut().zip(ev_alloc) {
            if let Some(s) = sessions
                .iter_mut()
                .find(|s| (s.arrive..s.depart).contains(&now))
            {
                s.remaining -= kw * PTU_HOURS;
            }
        }
        p
    }
}
