//! `ems-core`: everything both the cloud and the gateway agree on.
//!
//! This crate is deliberately free of I/O and async: pure functions and data types are
//! the easiest Rust to write, test and reason about. The binaries add the plumbing.
//!
//! Sign conventions on the wire (all powers in kW, averaged over one 15-minute PTU):
//! - `grid_kw`    > 0 import from the grid, < 0 export
//! - `pv_kw`      >= 0 solar generation
//! - `load_kw`    >= 0 non-flexible site consumption
//! - `ev_kw`      >= 0 total EV charging power
//! - `battery_kw` > 0 charging, < 0 discharging
//!
//! Energy balance: `grid = load + ev + battery - pv`.

pub mod accounting;
pub mod alloc;
pub mod forecast;
pub mod prices;
pub mod protocol;
pub mod scheduler;
pub mod time;

pub type Kw = f64;
pub type Kwh = f64;
