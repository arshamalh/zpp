# Domain guide: what is the concern behind peakpilot?

Peakpilot is a simulated EMS (energy management system). It's users are usually energy provider businesses and they use this software to have a clear overview of their users and their supply and demand cycles, which leads to managed peak shaving and saving their profit.

As an example, in netherlands, if your usage pass a certain amount once in a month, the whole month energy price will calculate based on that peak!

Netherlands had the problem of grid congestion in the recent years, there are businesses that are queued for getting more electricity for years! the supply chain goes its way, but businesses can optimize their process for quicker access to more affordable energy. We are here to help with the optimization part.

There are three main contributors to this grid congestion story:
- By the raise of AI, EV (electrical vehicles), and HP (Heat Pumps), the need for energy just skyrocket.
- Russia and Iran war, cut two main supply chains of cheap energy.
- Solar panels on the other hand, are the new source of energy, but they should be integrated in to the grid `carefully`.

---

## 1. Electricity (Energy) Reminder
| Unit | Name | Explanation |
| --- | --- | --- |
| kW | Power | |
| kWh | Energy* | The amount of energy generated, consumed or stored in a battery |
| kWp | kilowatt-peak | The peak of a solar system output under lab situation, which in real-world will be usually lower |

Energy = Power * time => 100 kW for 15 minutes (1/4 of an hour) = 25 kWh.
With 25 kWh, you can ride a Tesla Model 3 for 165 km, or if your 10 Watt lamp be on for 10 hours per day, you can light it for 2500 * 60 / 10 / 10 = 1500 days. 

**Sign convention.** peakpilot uses the meter's view everywhere (`ems-core/src/lib.rs`):

```
grid_kw    > 0  importing from the grid      < 0  exporting
battery_kw > 0  charging                     < 0  discharging
pv_kw, load_kw, ev_kw  >= 0

grid = load + ev + battery - pv        (the energy balance)
```

`Telemetry::validate` rejects any reading that breaks this balance by more than 0.5 kW. A real
meter and a sum of sub-meters never agree exactly, which is why there's a tolerance.


## 2. The PTU (ISP): 15 minutes
PTU's (by netherlands, which are called ISPs elsewhere) are the 15 minutes time-span for planning, pricing, and reporting. By that, we'll have 96 PTU's per day. each PTU can present the average electricity usage of that time period.

For simulation purposes, in our platform, each PTU passes in one second.


**Simplification:** a real meter reports every few seconds, and a real limit can be breached for 30 seconds
inside a PTU whose average looks fine. Real EMSs control at second-level resolution at the edge
and plan at PTU resolution in the cloud.

---

## 3. The grid connection: the hard limit

A business connection (*grootverbruik*, roughly above 3×80 A) comes with a contract with the grid
operator, the **ATO** (*aansluit- en transportovereenkomst*). The key number in it is the
**contracted transport capacity**, *gecontracteerd transportvermogen* (**GTV**), in kW: the
maximum power the company may draw at any moment.

Why peaks are expensive, even before congestion:

- **Capacity charges.** Network tariffs for large connections charge per kW of contracted capacity and per kW of the
  highest peak measured in a period (**kW-max**, per month or week). One badly timed peak where all
  vans charge at 08:00 sets the bill for the whole month.
- **Exceeding the GTV** is a contract breach. In congestion areas grid operators have enforced it,
  including in court.
- **You can't easily get more.** In congested areas, requests for more capacity join a waiting
  list that can take years.

Recent rule changes add **alternative transport rights**. These are non-firm or time-bound contracts (for example capacity
only in certain time blocks, or for a limited number of hours) that are cheaper or available sooner, *if* you can
reliably respect them. An EMS is what makes a company able to accept such a contract.

In peakpilot:
- `SiteInfo.grid_limit_kw` is the GTV.
- `SiteInfo.export_limit_kw` is the same idea for feed-in, which matters a lot for big solar sites.
- The scheduler adds a large penalty for every kWh beyond either limit (`CostModel::overload_eur_per_kwh`).
- The gateway enforces `limit - safety_margin` every PTU and pushes surplus into the battery
  (`gateway-sim/src/control.rs`).
- KPIs: `over_limit_ptus` vs. `baseline_over_limit_ptus`, `actual_peak_kw` vs. `baseline_peak_kw`.

**Simplification:** real tariffs use kW-max per month or week. peakpilot charges a flat fee on
contracted capacity instead, so modelling kW-max is a good extension. There's also no solar
curtailment, so an export limit only holds while the battery has room.

---

## 4. Grid congestion and the Congestion Service Provider

### Who runs the grid

| Role | Who (NL) | What they do |
|---|---|---|
| **TSO** (transmission system operator) | TenneT | High-voltage national grid, system balance |
| **DSO** (distribution system operator) | Liander, Stedin, Enexis (and smaller ones) | Regional and local grids, where most business sites connect |
| Regulator | ACM | Sets tariffs and the grid codes |

**Congestion** means a part of the network (a cable, a transformer) would be overloaded at certain
moments, either by too much demand (*afname*) or too much feed-in from solar and wind (*invoeding*).
Netbeheer Nederland publishes a capacity map per area.

### Congestion management

Instead of saying "no" to new connections, grid operators can buy **flexibility**: paying
connected companies to use less (or more) at specific times and places. The main instruments are:

- **Redispatch**: bids to raise or lower power in a congested area, traded through **GOPACS**, the
  grid operators' joint congestion management platform, with matching on energy exchanges.
- **Capacity limiting contract** (**CBC**, *capaciteitsbeperkingscontract*): the company agrees in
  advance that the operator may limit its capacity in certain periods, for a fee.
- **Mandatory participation**: in congested areas, operators may oblige large connections (from
  about 1 MW contracted capacity) to offer flexibility.
- **Group contracts**: several neighbouring companies share and coordinate capacity.

### The Congestion Service Provider (CSP)

Since a 2022 change to the electricity grid code, the Netherlands has a market role called the
**Congestion Service Provider** (*congestiemanagementdienstverlener*). A CSP is a recognised
intermediary between the grid operator and large business customers. It offers flexibility on their
behalf, can bid via the exchanges connected to GOPACS, and coordinates groups. To be recognised, a
party must pass prequalification with the grid operator, which checks its expertise and technical and
administrative capability. TenneT keeps the register.

**Zympler is a certified CSP.** In software terms, that means:
1. A congestion signal or contract arrives: "between 16:00 and 19:00, site X may use at most N kW".
2. The EMS must **turn that into setpoints** and **prove it delivered** with metering data.
3. Failing to deliver costs money and trust, so reliability (section 7) isn't optional.

In peakpilot: not yet implemented. It's extension 2 in the README. The natural design is a
`CongestionSignal { site_id, from, to, max_kw }` message that temporarily lowers `grid_limit_kw`
for those slots in the planner, plus a report that compares delivered vs. promised power.

---

## 5. Energy prices and markets

### The markets an EMS cares about

| Market | Timing | Role for an EMS |
|---|---|---|
| **Day-ahead** (EPEX SPOT, coupled across Europe as SDAC) | Auction around noon for the next day, 15-minute products since Oct 2025 | The price curve the planner optimises against |
| **Intraday** | Continuous trading up to shortly before delivery | Re-optimise when forecasts change |
| **Imbalance** | Settled per PTU afterwards by TenneT | The penalty price for deviating from what your BRP planned |

Two more roles appear here:
- **Supplier** (*leverancier*): sells you energy. With a **dynamic contract** you pay roughly the
  day-ahead price per PTU, which is what makes price-based optimisation pay off.
- **BRP** (balance responsible party): must predict its portfolio's consumption per PTU and pays for
  imbalance. Zympler mentions **reducing imbalance costs for suppliers**: if the EMS makes customers'
  consumption more predictable, or deliberately shifts it to help the BRP's position, that's worth money.

### The shape of a modern price day

With a lot of solar on the grid, a sunny Dutch day looks roughly like this:

```
€/MWh
 250 |                                           ___
 150 |      _                                  _/   \_
  85 |_____/ \__                           ___/       \____
   0 |          \___                  ____/
 -10 |              \______(solar)___/
     +--------------------------------------------------------
      00    06    08          13:30            19          24
```

Prices are cheap or **negative** at midday (too much solar) and peak in the evening. That's why
"charge at noon, discharge at 19:00" works, and why exporting solar at noon can *cost* you money.

In peakpilot: `ems-core/src/prices.rs` builds exactly this synthetic curve
(`day_ahead_eur_mwh`). Export is paid at the spot price minus a supplier fee.
`ptu_cost_eur` turns one PTU at a given grid power into euros.

### Nomination and imbalance

The **nomination** is the position you bought for each PTU in advance. You pay the day-ahead price for
it, and the imbalance price for whatever you do differently:

```
cost per PTU = nominated × day-ahead + (actual - nominated) × imbalance
```

So deviating isn't automatically bad. When the imbalance price is favourable, deviating on purpose earns
money. When it's unfavourable, you want to stay close to the nomination. That's what the portal's PowerTrader
module does by steering flexible assets.

In peakpilot:
- `SiteState::nominate` fixes each PTU once, from the first plan that reaches it within a day. Plans look
  48 h ahead because their last slots are distorted by the rough value of energy left in the battery.
- Nominations are split per asset, so the same formula gives each asset its own bill line
  (`accounting::Bill`). It adds up to the grid settlement exactly.
- `control::imbalance_steer` moves the battery when the price is far from day-ahead. Otherwise the gateway
  stays on the plan, which only approximates "keep close to the nomination" because plans keep improving.
- `imbalance_eur_mwh` is day-ahead plus noise; `settlement_eur_mwh` is the final price, shown a day later.
  The bill uses the preliminary price.

**Coming change:** grid operators and the ACM are moving network tariffs for large connections to
**time-dependent** rates, so using the grid at peak hours costs more on the network side too. That
adds a second price signal for the planner. It's not modelled in peakpilot.

---

## 6. The assets

### Solar PV

- Rated in **kWp**; real output depends on sun angle, clouds, temperature and orientation.
- Controlled through its **inverter** (via Modbus/SunSpec in practice). The main control is
  **curtailment**: limiting output, which is useful when prices are negative or export is limited.
- In peakpilot: `forecast::clear_sky()` is a bell curve from 06:00 to 20:00, multiplied by a
  cloudiness ratio estimated from recent readings. The simulator adds drifting clouds.
  Curtailment isn't implemented.

### Battery (BESS)

| Term | Meaning | In peakpilot |
|---|---|---|
| **Capacity** | Energy it can store, kWh | `BatterySpec.capacity_kwh` |
| **Power** | Max charge/discharge rate, kW | `max_power_kw` |
| **C-rate** | power ÷ capacity. 100 kW / 250 kWh = 0.4C | Implied by the depot preset |
| **SoC** (state of charge) | Energy stored now | `battery_soc_kwh` |
| **Efficiency** | Losses when charging and discharging | 0.95 each way, about 0.90 round trip |
| **Reserve** | Minimum SoC kept for backup or peaks | `min_soc_kwh` = 10% |
| **Degradation** | Every cycle wears the cells | `wear_eur_per_kwh`, a small cost per kWh moved |

A battery stacks several kinds of value: **peak shaving** (stay under the GTV and lower kW-max),
**self-consumption** (store solar instead of exporting it cheaply), **arbitrage** (buy cheap, use
expensive) and **flexibility services** (congestion or balancing markets). The planner chooses
between these automatically, because they all show up as costs in one objective.

### EV charging

- A **session** is a car plugged in from arrival to departure, needing some energy (kWh) before it leaves.
- Chargers are controlled with **OCPP** (Open Charge Point Protocol). Smart charging uses
  *charging profiles* that set a current or power limit per charger or per group.
- Physical detail peakpilot ignores: a charger can't run at any arbitrary power. The IEC 61851 standard has a
  **minimum of 6 A**, about 4.1 kW on three phases, so a real allocation must round to "0 or ≥ min".
- **Deferring vs. not delivering:** holding a car back at 08:00 is fine if it's full by departure.
  The real constraint is energy delivered before departure, not power at every moment.
- In peakpilot: the scheduler sets an EV cap per PTU (`ev_cap_kw`). The gateway splits it fairly among
  cars with **max-min water-filling** (`ems-core/src/alloc.rs`). The KPI `Report::ev_curtailed_kwh` adds up
  demand that was postponed in each PTU. It measures *deferral*, not energy missing at departure.
  Departure-aware ("cars leaving soonest go first") scheduling is a natural next step.

### Base load and heat pumps

- **Base load**: lighting, cooling, machines, the inflexible part. Cold storage has a high, flat base
  load. Offices peak during working hours. Forecast from history (`forecast::profile`, the
  same-PTU-of-day average).
- **Heat pumps** are flexible because buildings store heat: you can pre-heat before a peak. They're
  not in peakpilot. They would add a thermal state variable, much like battery SoC.

---

## 7. EMS architecture: cloud and edge

```
       metering + device protocols                       planning + data + UI
  ┌────────────────────────────────┐   MQTT   ┌──────────────────────────────────┐
  │ GATEWAY (on site)              │◀────────▶│ CLOUD                            │
  │ - reads meter, PV, BESS, EVs   │          │ - stores time series              │
  │   (Modbus/SunSpec, OCPP, P1)   │          │ - forecasts load, solar, prices   │
  │ - applies setpoints            │          │ - optimises 24 h, every hour      │
  │ - ALWAYS guards the limit      │          │ - dispatches schedules, tracks acks│
  │ - falls back if cloud is gone  │          │ - reports, billing, CSP delivery  │
  └────────────────────────────────┘          └──────────────────────────────────┘
```

The core split: **the cloud optimises, the edge guarantees safety.** Forecasts are wrong and links
drop, but the GTV must hold every second. So:

- The cloud sends a **schedule**: setpoints per PTU (`ScheduleCommand`, `SlotSetpoint`), covering
  only the PTUs it stays valid for.
- Each schedule has `valid_until`. A gateway never follows stale instructions.
- The gateway's **real-time guard** (`control::decide`) overrides the schedule when reality differs:
  it stops charging the battery or caps the EVs further if the load is higher than forecast.
- Without a valid schedule the gateway runs a **fallback rule**: self-consumption plus peak
  shaving, with no price awareness. It's less profitable but safe.
- The gateway announces itself (`SiteInfo`, retained). If it vanishes, the broker publishes its
  MQTT **Last Will** (`status = offline`).

Reliability rules that come straight from the domain:

| Domain need | Mechanism |
|---|---|
| Don't double-apply a schedule | `command_id` dedupe on the gateway (`ScheduleBook`) |
| Don't double-count energy | Telemetry deduped by `(boot_id, seq)`. DB writes are upserts on `(site_id, ts)`. |
| Old schedule must not override new | `issued_at` ordering → `Superseded` |
| Prove delivery (CSP, billing) | Every PTU's telemetry stored with `mode` and `schedule_id` |

---

## 8. Optimisation: how a schedule is chosen

1. **Forecast** the next 192 PTUs: load, EV demand, solar and price (`forecast::naive_forecast`).
2. **Optimise** battery power and EV cap per PTU to minimise:

```
  energy cost (import at price, export at price - fee)
+ EV deferral penalty      (don't hold cars back without reason)
+ overload penalty         (exceeding the GTV: very expensive)
+ battery wear             (don't cycle for pennies)
- value of energy left in the battery at the end (don't dump it to "save" money)
```

   The method is dynamic programming over a discretised SoC (`ems-core/src/scheduler.rs`). Because
   battery charge carries from one PTU to the next, you can't plan each PTU on its own.
3. **Rolling horizon**: re-plan every hour (`REPLAN_EVERY_PTUS = 4`) with fresh measurements and
   send only the new plan. Each plan covers 48 h, but only its first part is ever executed.
4. **Baseline**: "no EMS" means no battery and EVs charging at full demand. Every saving and peak KPI
   is measured against this baseline, both in the plan (`baseline_*` fields) and from real telemetry
   (`accounting::Report`).

Real-world upgrades: LP/MILP solvers when more assets and integer constraints appear (EV minimum current, on/off
heat pumps), stochastic or robust planning for forecast uncertainty, and ML forecasts.

---

## 9. KPIs a customer or Zympler cares about

| KPI | Meaning | peakpilot |
|---|---|---|
| Peak reduction | Highest kW vs. baseline | `actual_peak_kw` / `baseline_peak_kw` |
| Limit compliance | PTUs above GTV | `over_limit_ptus` |
| Cost savings | € vs. baseline | `baseline_cost_eur - actual_cost_eur` |
| Solar self-consumption | Share of PV used on site instead of exported | `Report::flows_kwh` |
| CO₂ | Emissions, and the reduction vs. buying everything from the grid | `co2_kg`, `co2_reduction_kg` |
| Extra EV capacity | More chargers on the same connection | shown by the campus profile |
| Delivery reliability | Share of PTUs following a valid cloud schedule | `cloud_mode_ptus / ptus` |

Zympler's published figures with partner 50five are good benchmarks to mention: up to 25% lower
energy bills, +50% solar self-consumption, and 2× EV charging capacity on the same connection.

---

## 10. Glossary (English / Dutch)

| Term | Dutch | Meaning |
|---|---|---|
| Grid operator (DSO) | netbeheerder | Liander, Stedin, Enexis… |
| TSO | landelijk netbeheerder | TenneT |
| Grid congestion | netcongestie | Network full at certain times and places |
| Connection & transport agreement | ATO (aansluit- en transportovereenkomst) | The connection contract |
| Contracted transport capacity | GTV (gecontracteerd transportvermogen) | The kW limit |
| Large / small consumer | grootverbruiker / kleinverbruiker | Above / up to 3×80 A |
| Waiting list | wachtrij | Queue for new capacity |
| Capacity limiting contract | CBC (capaciteitsbeperkingscontract) | Operator may limit you, for a fee |
| Congestion Service Provider | CSP / congestiemanagementdienstverlener | Recognised flexibility intermediary |
| GOPACS | — | Grid operators' congestion platform |
| Programme time unit | PTU (programmatijdseenheid) | 15-minute settlement block |
| Balance responsible party | BRP (balanceringsverantwoordelijke) | Pays for imbalance |
| Feed-in / offtake | invoeding / afname | Export / import |
| Day-ahead market | day-aheadmarkt | Next-day auction, EPEX SPOT |

---

## 11. Where the domain lives in the code

| Domain concept | File |
|---|---|
| Units, sign convention, energy balance | `ems-core/src/lib.rs`, `protocol.rs` (`Telemetry::validate`) |
| PTU time grid | `ems-core/src/time.rs` |
| Day-ahead, imbalance and CO₂ curves | `ems-core/src/prices.rs` |
| Nomination, energy flows, bill, KPIs | `ems-core/src/accounting.rs` |
| Solar / load / EV forecasts | `ems-core/src/forecast.rs` |
| Peak shaving + arbitrage optimiser | `ems-core/src/scheduler.rs` |
| Fair EV power sharing | `ems-core/src/alloc.rs` |
| Gateway ↔ cloud protocol | `ems-core/src/protocol.rs` |
| Site physics (PV, BESS, EV sessions) | `gateway-sim/src/sim.rs` |
| Edge safety guard + fallback | `gateway-sim/src/control.rs` |
| Fixing the nomination | `cloud/src/state.rs` (`nominate`) |
| Planning loop | `cloud/src/planner.rs` |
| Reliable delivery | `cloud/src/dispatcher.rs` |

## 12. What peakpilot simplifies (know these before the interview)

- No kW-max tariff, no time-dependent network tariffs, no solar curtailment.
- PTU-level control. Real edges react within seconds.
- No EV minimum current, no departure deadlines, no per-charger phases.
- Synthetic prices, CO₂, taxes and subsidies, and naive forecasts. There's no ENTSO-E feed and no weather data.
- Imbalance is simplified: one noisy price, and a simple threshold rule for trading it.
- No congestion signals or CSP delivery reports.
- UTC is used as local time. The real Dutch market runs on CET/CEST, including DST days with 92 or 100 PTUs.

---

## Sources

- Liander, Congestie Service Provider: <https://www.liander.nl/grootzakelijk/energietransitie/alles-over-netcongestie/congestie-service-provider>
- Partners in Energie, Participating in congestion management: <https://www.partnersinenergie.nl/en-GB/participating-in-congestion-management>
- BritNed, CSP and GOPACS explainer: <https://www.britned.com/knowledge-base/services/>
- RVO, Flexibel contract (mandatory participation above 1 MW, alternative rights): <https://www.rvo.nl/onderwerpen/netcongestie/flexibel-contract>
- Netbeheer Nederland, Flex products catalogue (GTV options, kW-max): <https://www.rvo.nl/sites/default/files/2025-04/20241118_NBNL_Overzicht-Flexproducten_Productcatalogus.pdf>
- ACM, draft decision on time-dependent tariffs for large connections (Oct 2026): <https://www.acm.nl/nl/publicaties/ontwerp-codebesluit-tijdsafhankelijke-tarieven-voor-grote-aansluitingen>
- EPEX SPOT, 15-minute MTU go-live in SDAC: <https://www.epexspot.com/en/news/market-coupling-steering-committee-confirms-go-live-15-minute-mtu-sdac-trading-day-30>
- Solar Magazine, Liander enforcing contracted capacity in court: <https://solarmagazine.nl/nieuws-zonne-energie/i25904/grootverbruiker-overschrijdt-contract-liander-wint-rechtszaak-over-transportbeperking>
- Zympler / 50five figures: see the project brief sources (presseportal.de press release)
