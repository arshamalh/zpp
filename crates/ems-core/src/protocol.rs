//! The gateway <-> cloud wire protocol: MQTT topics and JSON payloads.
//!
//! ```text
//! peakpilot/sites/{site}/info       gateway -> cloud   SiteInfo      (retained)
//! peakpilot/sites/{site}/status     gateway -> cloud   "online"/"offline" (retained, LWT)
//! peakpilot/sites/{site}/telemetry  gateway -> cloud   Telemetry     (QoS 1, deduped by seq)
//! peakpilot/sites/{site}/commands   cloud -> gateway   ScheduleCommand (QoS 1, idempotent by id)
//! peakpilot/sites/{site}/acks       gateway -> cloud   Ack           (QoS 1)
//! ```

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{Kw, Kwh};

pub mod topics {
    pub const PREFIX: &str = "peakpilot/sites";

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Channel {
        Info,
        Status,
        Telemetry,
        Commands,
        Acks,
    }

    impl Channel {
        pub fn as_str(self) -> &'static str {
            match self {
                Channel::Info => "info",
                Channel::Status => "status",
                Channel::Telemetry => "telemetry",
                Channel::Commands => "commands",
                Channel::Acks => "acks",
            }
        }
    }

    pub fn topic(site_id: &str, ch: Channel) -> String {
        format!("{PREFIX}/{site_id}/{}", ch.as_str())
    }

    /// Wildcard subscription for one channel across all sites, e.g. `peakpilot/sites/+/telemetry`.
    pub fn all_sites(ch: Channel) -> String {
        format!("{PREFIX}/+/{}", ch.as_str())
    }

    /// Parse `peakpilot/sites/{site}/{channel}`. Returns a borrowed `&str`: no allocation.
    pub fn parse(topic: &str) -> Option<(&str, Channel)> {
        let rest = topic.strip_prefix(PREFIX)?.strip_prefix('/')?;
        let (site, ch) = rest.split_once('/')?;
        let ch = match ch {
            "info" => Channel::Info,
            "status" => Channel::Status,
            "telemetry" => Channel::Telemetry,
            "commands" => Channel::Commands,
            "acks" => Channel::Acks,
            _ => return None,
        };
        (!site.is_empty()).then_some((site, ch))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatterySpec {
    pub capacity_kwh: Kwh,
    pub max_power_kw: Kw,
    pub efficiency: f64,
}

/// Static description of a site, announced by its gateway (retained message).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SiteInfo {
    pub site_id: String,
    pub name: String,
    /// Contracted import capacity (GTV).
    pub grid_limit_kw: Kw,
    /// Contracted export capacity.
    pub export_limit_kw: Kw,
    pub pv_kwp: Kw,
    pub battery: Option<BatterySpec>,
    pub chargers: u32,
    pub charger_kw: Kw,
    pub firmware: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayMode {
    /// Following a valid schedule from the cloud.
    Cloud,
    /// No valid schedule: running the local safe rule.
    Fallback,
}

/// One PTU of averaged measurements from a gateway.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Telemetry {
    pub site_id: String,
    /// Random per gateway process start. `(boot_id, seq)` is the idempotency key, so a
    /// QoS-1 redelivery is dropped but a restarted gateway (seq back at 1) is not.
    pub boot_id: Uuid,
    /// Monotonic within one boot.
    pub seq: u64,
    /// Start of the PTU this reading covers (gateway clock).
    pub ts: DateTime<Utc>,
    pub grid_kw: Kw,
    pub pv_kw: Kw,
    pub load_kw: Kw,
    pub ev_kw: Kw,
    /// What the cars *wanted*; `ev_demand_kw - ev_kw` was curtailed.
    pub ev_demand_kw: Kw,
    pub battery_kw: Kw,
    pub battery_soc_kwh: Option<Kwh>,
    pub mode: GatewayMode,
    pub schedule_id: Option<Uuid>,
}

#[derive(Debug, Error, PartialEq)]
pub enum ValidationError {
    #[error("site_id in payload ({payload}) does not match topic ({topic})")]
    SiteMismatch { payload: String, topic: String },
    #[error("field `{0}` is not a finite number")]
    NotFinite(&'static str),
    #[error("field `{0}` must not be negative")]
    Negative(&'static str),
    #[error("energy balance off by {0:.2} kW (grid != load + ev + battery - pv)")]
    Imbalance(f64),
    #[error("schedule has no slots")]
    EmptySchedule,
}

impl Telemetry {
    /// Cheap sanity checks before we trust a reading. Note the return type:
    /// `Result<(), E>` is Rust's `error` return, and `?` propagates it.
    pub fn validate(&self, topic_site: &str) -> Result<(), ValidationError> {
        if self.site_id != topic_site {
            return Err(ValidationError::SiteMismatch {
                payload: self.site_id.clone(),
                topic: topic_site.to_owned(),
            });
        }
        let fields = [
            ("grid_kw", self.grid_kw),
            ("pv_kw", self.pv_kw),
            ("load_kw", self.load_kw),
            ("ev_kw", self.ev_kw),
            ("ev_demand_kw", self.ev_demand_kw),
            ("battery_kw", self.battery_kw),
        ];
        for (name, v) in fields {
            if !v.is_finite() {
                return Err(ValidationError::NotFinite(name));
            }
        }
        for (name, v) in [
            ("pv_kw", self.pv_kw),
            ("load_kw", self.load_kw),
            ("ev_kw", self.ev_kw),
        ] {
            if v < -1e-6 {
                return Err(ValidationError::Negative(name));
            }
        }
        let balance = self.load_kw + self.ev_kw + self.battery_kw - self.pv_kw - self.grid_kw;
        if balance.abs() > 0.5 {
            return Err(ValidationError::Imbalance(balance));
        }
        Ok(())
    }
}

/// Setpoints for one PTU.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlotSetpoint {
    pub start: DateTime<Utc>,
    /// Battery power: > 0 charge, < 0 discharge.
    pub battery_kw: Kw,
    /// Upper bound for the *sum* of all EV chargers this PTU.
    pub ev_cap_kw: Kw,
}

/// A schedule pushed from the cloud. Idempotent by `command_id`; ignored after `valid_until`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleCommand {
    pub command_id: Uuid,
    pub site_id: String,
    pub issued_at: DateTime<Utc>,
    pub valid_until: DateTime<Utc>,
    pub slots: Vec<SlotSetpoint>,
}

impl ScheduleCommand {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.slots.is_empty() {
            return Err(ValidationError::EmptySchedule);
        }
        for s in &self.slots {
            if !s.battery_kw.is_finite() {
                return Err(ValidationError::NotFinite("battery_kw"));
            }
            if !s.ev_cap_kw.is_finite() {
                return Err(ValidationError::NotFinite("ev_cap_kw"));
            }
        }
        Ok(())
    }

    /// The setpoint covering `ptu`, if the command is still valid then.
    pub fn slot_at(&self, ptu: DateTime<Utc>) -> Option<&SlotSetpoint> {
        if ptu >= self.valid_until {
            return None;
        }
        self.slots.iter().find(|s| s.start == ptu)
    }
}

/// `#[serde(tag = ...)]` gives `{"status":"rejected","reason":"..."}` on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AckStatus {
    Accepted,
    /// Already had this command id: a redelivery. Nothing re-applied.
    Duplicate,
    Expired,
    /// A newer schedule is already active.
    Superseded,
    Rejected {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ack {
    pub command_id: Uuid,
    #[serde(flatten)]
    pub status: AckStatus,
}

#[cfg(test)]
mod tests {
    use super::topics::*;
    use super::*;

    fn reading() -> Telemetry {
        Telemetry {
            site_id: "depot".into(),
            boot_id: Uuid::nil(),
            seq: 1,
            ts: Utc::now(),
            grid_kw: 50.0,
            pv_kw: 20.0,
            load_kw: 40.0,
            ev_kw: 22.0,
            ev_demand_kw: 22.0,
            battery_kw: 8.0,
            battery_soc_kwh: Some(100.0),
            mode: GatewayMode::Cloud,
            schedule_id: None,
        }
    }

    #[test]
    fn topic_roundtrip() {
        let t = topic("depot-1", Channel::Telemetry);
        assert_eq!(parse(&t), Some(("depot-1", Channel::Telemetry)));
        assert_eq!(parse("peakpilot/sites//telemetry"), None);
        assert_eq!(parse("other/x/telemetry"), None);
    }

    #[test]
    fn valid_reading_passes_and_imbalance_fails() {
        assert_eq!(reading().validate("depot"), Ok(()));
        let mut bad = reading();
        bad.grid_kw = 0.0;
        assert!(matches!(
            bad.validate("depot"),
            Err(ValidationError::Imbalance(_))
        ));
        assert!(matches!(
            reading().validate("x"),
            Err(ValidationError::SiteMismatch { .. })
        ));
    }

    #[test]
    fn ack_wire_format() {
        let ack = Ack {
            command_id: Uuid::nil(),
            status: AckStatus::Rejected {
                reason: "nope".into(),
            },
        };
        let json = serde_json::to_value(&ack).unwrap();
        assert_eq!(json["status"], "rejected");
        assert_eq!(json["reason"], "nope");
        let back: Ack = serde_json::from_value(json).unwrap();
        assert_eq!(back, ack);
    }
}
