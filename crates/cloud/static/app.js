// peakpilot panel. No framework: every 1.5 s we fetch what the current page needs and
// re-render it as an HTML string. Reading order: helpers, charts, pages, router.

const PTU_HOURS = 0.25;
const ui = {
  sites: [], site: null,
  date: null,            // selected day; null = follow the live simulated day
  period: 'month',       // day | week | month
  gridView: 'grid', measureView: 'source', marketView: 'dayahead',
  billView: 'list', bill: { energy: true, transport: true, subsidies: true },
  expanded: null,        // asset row opened on the status page
};

// ---------- helpers ----------
const $ = s => document.querySelector(s);
const esc = s => String(s ?? '').replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
const num = (n, d = 1) => n == null || isNaN(n) ? '–' : Number(n).toLocaleString('en-GB', { minimumFractionDigits: d, maximumFractionDigits: d });
const eur = (n, d = 2) => n == null || isNaN(n) ? '–' : (n < 0 ? '-€' : '€') + num(Math.abs(n), d);
const unit = (v, u) => `${v}<small> ${u}</small>`;
const kwh = n => Math.abs(n) >= 1000 ? unit(num(n / 1000), 'MWh') : unit(num(n, 0), 'kWh');
const kw = n => Math.abs(n) >= 1000 ? `${num(n / 1000)} MW` : `${num(n, 0)} kW`;
const hhmm = ts => ts ? ts.slice(11, 16) : '–';
const sum = (rows, f) => rows.reduce((a, r) => a + (f(r) ?? 0), 0);
const json = url => fetch(url).then(r => { if (!r.ok) throw new Error(url); return r.json(); });
const site = () => ui.sites.find(s => s.site_id === ui.site);
const addDays = (date, n) => new Date(Date.parse(date) + n * 864e5).toISOString().slice(0, 10);
const liveDate = () => site()?.latest?.ts.slice(0, 10);
const selDate = () => ui.date ?? liveDate();
const longDate = d => new Date(d).toLocaleDateString('en-GB', { weekday: 'short', day: 'numeric', month: 'short', year: 'numeric', timeZone: 'UTC' });

function period(kind = ui.period) {
  const d = selDate(), js = new Date(d);
  if (kind === 'day') return { from: d, to: d, label: longDate(d), step: 1 };
  if (kind === 'week') {
    const from = addDays(d, -((js.getUTCDay() + 6) % 7));
    return { from, to: addDays(from, 6), label: 'Week of ' + longDate(from), step: 7 };
  }
  const from = d.slice(0, 8) + '01';
  const to = new Date(Date.UTC(js.getUTCFullYear(), js.getUTCMonth() + 1, 0)).toISOString().slice(0, 10);
  return { from, to, label: js.toLocaleDateString('en-GB', { month: 'long', year: 'numeric', timeZone: 'UTC' }), step: 31 };
}

const getDay = id => json(`/api/sites/${id}/day?date=${selDate()}`);
const getSummary = (id, p = period()) => json(`/api/sites/${id}/summary?from=${p.from}&to=${p.to}`);

// ---------- shared widgets ----------
const css = c => `var(--${c})`;
const tile = (href, title, label, value) => `<a class="card tile" href="#/${href}">
  <div class="k t">${title}</div><div class="k">${label}</div><div class="v">${value}</div></a>`;
const stat = (label, value) => `<div class="card"><div class="k">${label}</div><div class="v">${value}</div></div>`;
const stats = list => `<div class="row tiles">${list.map(([l, v]) => stat(l, v)).join('')}</div>`;
const chips = list => `<div class="chips">${list.map(([l, v, c]) => `<div class="chip" style="--c:${c ? css(c) : 'var(--text)'}"><b>${v}</b>${l}</div>`).join('')}</div>`;
const btn = (label, act, on, cls = '') => `<button class="${cls} ${on ? 'on' : ''}" data-act="${act}">${label}</button>`;

function dayBar() {
  const d = selDate();
  return `<div class="card bar">
    ${btn('‹', 'shift:-1', false, 'nav')}<b>${longDate(d)}</b>
    <button class="nav" data-act="shift:1" ${d >= liveDate() ? 'disabled' : ''}>›</button>
    ${btn('Live', 'live', ui.date == null)}</div>`;
}

function periodBar() {
  const p = period();
  return `<div class="card bar">
    ${btn('‹', `shift:-${p.step}`, false, 'nav')}<b>${p.label}</b>
    <button class="nav" data-act="shift:${p.step}" ${p.to >= liveDate() ? 'disabled' : ''}>›</button>
    ${['day', 'week', 'month'].map(x => btn(x[0].toUpperCase() + x.slice(1), `period:${x}`, ui.period === x)).join('')}</div>`;
}

// ---------- charts ----------
const TIPS = {};
let chartSeq = 0;

function niceScale(lo, hi) {
  const raw = (hi - lo || 1) / 5, mag = 10 ** Math.floor(Math.log10(raw));
  const step = [1, 2, 3, 5, 10].map(m => m * mag).find(s => s >= raw);
  return { lo: Math.floor(lo / step) * step, hi: Math.ceil(hi / step) * step, step };
}

// One chart type for the whole panel: stacked bars and/or stepped lines over `rows`.
//   stacks: [{ label, color, v(row), outline? }]    lines: [{ label, color, v(row), dash? }]
//   limits: [kW lines]   now: row index   xLabels: [[index, text]]   tip(row): html
function chart({ rows, stacks = [], lines = [], limits = [], now, xLabels, yFmt = v => num(v, 0), tip, height = 250, labelFmt = yFmt }) {
  const W = 1000, H = height, L = 58, R = 10, T = 24, B = 24;
  const up = rows.map(() => 0), down = rows.map(() => 0), bars = [];
  rows.forEach((r, i) => stacks.forEach(s => {
    const v = s.v(r);
    if (!v) return;
    const acc = v > 0 ? up : down;
    bars.push({ i, s, from: acc[i], to: acc[i] + v });
    acc[i] += v;
  }));
  const lineVals = lines.flatMap(l => rows.map(l.v)).filter(v => v != null);
  const s = niceScale(Math.min(0, ...down, ...lineVals, ...limits), Math.max(0, ...up, ...lineVals, ...limits));
  const slot = (W - L - R) / rows.length;
  const x = i => L + i * slot;
  const y = v => T + (s.hi - v) * (H - T - B) / (s.hi - s.lo);

  const parts = [];
  for (let v = s.lo; v <= s.hi + s.step / 2; v += s.step) {
    parts.push(`<text x="${L - 8}" y="${y(v) + 4}" text-anchor="end">${yFmt(v)}</text>`);
  }
  const labels = xLabels ?? [3, 6, 9, 12, 15, 18, 21].map(h => [h * 4, String(h)]);
  labels.forEach(([i, text]) => parts.push(
    `<line x1="${x(i)}" x2="${x(i)}" y1="${T}" y2="${H - B}" stroke="var(--line)"/><text x="${x(i)}" y="${H - 6}" text-anchor="middle">${text}</text>`));
  parts.push(`<line x1="${L}" x2="${W - R}" y1="${y(0)}" y2="${y(0)}" stroke="var(--muted)" stroke-width=".6"/>`);
  limits.forEach(v => parts.push(`<line x1="${L}" x2="${W - R}" y1="${y(v)}" y2="${y(v)}" stroke="var(--limit)"/>`));

  bars.forEach(b => {
    const top = y(Math.max(b.from, b.to)), h = Math.abs(y(b.from) - y(b.to));
    parts.push(b.s.outline
      ? `<rect x="${x(b.i) + 1}" y="${top}" width="${slot - 2}" height="${h}" fill="none" stroke="${css(b.s.color)}" stroke-width=".8"/>`
      : `<rect x="${x(b.i) + .6}" y="${top}" width="${slot - 1.2}" height="${h}" fill="${css(b.s.color)}"/>`);
  });
  lines.forEach(l => {
    let d = '', prev = null;
    rows.forEach((r, i) => {
      const v = l.v(r);
      if (v != null) d += `${prev == null ? 'M' : 'L'}${x(i)},${y(v)} L${x(i + 1)},${y(v)} `;
      prev = v;
    });
    parts.push(`<path d="${d}" fill="none" stroke="${css(l.color)}" stroke-width="1.5" ${l.dash ? 'stroke-dasharray="5 4"' : ''}/>`);
  });

  // Peak labels, like the small chips on a trading screen.
  const series = rows.map((r, i) => stacks.length ? [up[i], down[i]] : lines.map(l => l.v(r))).map(v => v.filter(n => n != null));
  const flat = series.flatMap((v, i) => v.map(n => [n, i]));
  if (flat.length) {
    const marks = [flat.reduce((a, b) => b[0] > a[0] ? b : a), flat.reduce((a, b) => b[0] < a[0] ? b : a)];
    marks.filter(([v]) => v !== 0).forEach(([v, i]) => {
      const tx = Math.min(Math.max(x(i + .5), L + 40), W - R - 40), ty = Math.max(y(v) + (v >= 0 && stacks.length ? -6 : v < 0 ? 15 : -6), 13);
      parts.push(`<text x="${tx}" y="${ty}" text-anchor="middle" style="font-weight:600;fill:var(--text)">${labelFmt(v)}</text>`);
    });
  }
  if (now != null) {
    const nx = x(now + .5), labelX = Math.min(nx, W - R - 20);
    parts.push(`<line x1="${nx}" x2="${nx}" y1="${T - 6}" y2="${H - B}" stroke="var(--blue)"/>
      <text x="${labelX}" y="12" text-anchor="middle" style="fill:var(--blue);font-weight:600">${hhmm(rows[now].ts)}</text>`);
  }
  const id = 'c' + (chartSeq++ % 16);
  TIPS[id] = { rows, tip, L, slot };
  return `<svg class="chart" data-chart="${id}" viewBox="0 0 ${W} ${H}" role="img">${parts.join('')}</svg>`;
}

const lastMeasured = rows => rows.findLastIndex(r => r.measured);
const nowIndex = rows => ui.date == null && lastMeasured(rows) >= 0 ? lastMeasured(rows) : undefined;

function tipBody(r, lines) {
  const end = new Date(Date.parse(r.ts) + 9e5).toISOString();
  return `<b>${longDate(r.ts)} ${hhmm(r.ts)} – ${hhmm(end)}</b>` +
    lines.filter(([, v]) => v != null).map(([l, v, c]) => `<div style="color:${c ? css(c) : 'inherit'}"><span>${l}</span><span>${v}</span></div>`).join('');
}

function donut(title, parts) {
  const shown = parts.filter(p => p.v > 0.01);
  const total = sum(shown, p => p.v), C = 2 * Math.PI * 40;
  if (!total) return `<div class="donut">${title}<div class="k">No energy yet</div></div>`;
  let offset = 0;
  const arcs = shown.map(p => {
    const len = p.v / total * C;
    const arc = `<circle cx="50" cy="50" r="40" fill="none" stroke="${css(p.c)}" stroke-width="18"
      stroke-dasharray="${len} ${C - len}" stroke-dashoffset="${-offset}" transform="rotate(-90 50 50)"/>`;
    offset += len;
    return arc;
  });
  return `<div class="donut">${title}<svg viewBox="0 0 100 100">${arcs.join('')}</svg>
    <div class="legend">${shown.map(p => `<span style="--c:${css(p.c)}"><i></i>${p.l} ${num(p.v, 0)}</span>`).join('')}</div></div>`;
}

// ---------- chart views over the rows of /day ----------
const M = (r, f) => r.measured ? f(r.measured) : null;
const F = (r, ...keys) => r.flows ? keys.reduce((a, k) => a + r.flows[k], 0) : null;
const neg = v => v == null ? null : -v;
const forecastBars = { label: 'Forecast', color: 'grid', outline: true, total: false, v: r => r.measured ? null : r.forecast_grid_kw };

const VIEWS = {
  grid: { label: 'Grid', stacks: [
    { label: 'Import', color: 'grid', v: r => M(r, t => Math.max(t.grid_kw, 0)) },
    { label: 'Export', color: 'muted', v: r => M(r, t => Math.min(t.grid_kw, 0)) },
    forecastBars] },
  source: { label: 'Source', stacks: [
    { label: 'Solar | Self-consumption', color: 'solar', v: r => F(r, 'pv_to_load', 'pv_to_ev', 'pv_to_battery') },
    { label: 'Solar | Export', color: 'solar', v: r => neg(F(r, 'pv_to_grid')) },
    { label: 'Battery | Self-consumption', color: 'battery', v: r => F(r, 'battery_to_load', 'battery_to_ev') },
    { label: 'Battery | Export', color: 'battery', v: r => neg(F(r, 'battery_to_grid')) },
    { label: 'Import', color: 'grid', v: r => F(r, 'grid_to_load', 'grid_to_ev', 'grid_to_battery') }] },
  destination: { label: 'Destination', stacks: [
    { label: 'Uncontrolled load', color: 'load', v: r => F(r, 'grid_to_load', 'pv_to_load', 'battery_to_load') },
    { label: 'Battery', color: 'battery', v: r => F(r, 'grid_to_battery', 'pv_to_battery') },
    { label: 'Charger', color: 'charger', v: r => F(r, 'grid_to_ev', 'pv_to_ev', 'battery_to_ev') },
    { label: 'Export', color: 'grid', v: r => neg(F(r, 'pv_to_grid', 'battery_to_grid')) }] },
  battery: { label: 'Battery', stacks: [{ label: 'Charged / discharged', color: 'battery', v: r => M(r, t => t.battery_kw) }] },
  chargers: { label: 'Chargers', stacks: [{ label: 'Charged', color: 'charger', v: r => M(r, t => t.ev_kw) }],
    lines: [{ label: 'Demand', color: 'muted', dash: true, v: r => M(r, t => t.ev_demand_kw) }] },
  solar: { label: 'Solar', stacks: [{ label: 'Generated', color: 'solar', v: r => M(r, t => t.pv_kw) }] },
  load: { label: 'Load', stacks: [{ label: 'Uncontrolled load', color: 'load', v: r => M(r, t => t.load_kw) }] },
};

function powerChart(rows, key, modes, actName) {
  const view = VIEWS[key], info = site().info;
  const shown = view.stacks.filter(s => s.total !== false);
  const tip = r => tipBody(r, [...view.stacks, ...(view.lines ?? [])].map(s => {
    const v = s.v(r);
    return [s.label, v == null ? null : `${num(v)} kW`, s.color];
  }));
  return `<div class="split"><div class="modes">${modes.map(m => btn(VIEWS[m].label, `${actName}:${m}`, m === key)).join('')}</div>
    <div>${chart({ rows, stacks: view.stacks, lines: view.lines, now: nowIndex(rows), tip,
      limits: key === 'grid' ? [info.grid_limit_kw, -info.export_limit_kw] : [] })}
    ${chips(shown.map(s => [s.label, kwh(Math.abs(sum(rows, s.v)) * PTU_HOURS), s.color]))}</div></div>`;
}

// ---------- assets (derived from what the gateway announced) ----------
function assets(info) {
  const list = [{ key: 'grid', group: 'Grid connection', color: 'grid', name: 'Main meter', count: 1, csv: ['grid_kw'], details: [
    ['Brand/model', info.firmware], ['Contractual import limit', kw(info.grid_limit_kw)], ['Contractual export limit', kw(info.export_limit_kw)]] }];
  if (info.pv_kwp > 0) list.push({ key: 'solar', group: 'Solar', color: 'solar', name: 'Solar panels', count: 1, csv: ['pv_kw'], details: [['Installed peak power', `${num(info.pv_kwp, 0)} kWp`]] });
  if (info.battery) list.push({ key: 'battery', group: 'Battery', color: 'battery', name: 'Battery', count: 1, control: true, csv: ['battery_kw', 'battery_soc_kwh'], details: [
    ['Max power', kw(info.battery.max_power_kw)], ['Capacity', `${num(info.battery.capacity_kwh, 0)} kWh`], ['Efficiency', `${num(info.battery.efficiency * 100, 0)}%`]] });
  if (info.chargers) list.push({ key: 'charger', group: 'Charger', color: 'charger', name: `${info.chargers} chargers`, count: info.chargers, control: true, csv: ['ev_kw', 'ev_demand_kw'], details: [
    ['Chargers', info.chargers], ['Power per charger', kw(info.charger_kw)]] });
  return list;
}
const assetCount = info => sum(assets(info), a => a.count);

async function downloadCsv(asset) {
  const rows = await json(`/api/sites/${ui.site}/telemetry?limit=100000`);
  const lines = [['ts', ...asset.csv].join(','), ...rows.map(t => [t.ts, ...asset.csv.map(k => t[k] ?? '')].join(','))];
  const a = document.createElement('a');
  a.href = URL.createObjectURL(new Blob([lines.join('\n')], { type: 'text/csv' }));
  a.download = `${ui.site}-${asset.key}-quarter-hourly.csv`;
  a.click();
  URL.revokeObjectURL(a.href);
}

// ---------- PowerBase ----------
function flowDiagram(s) {
  const t = s.latest, i = s.info;
  const icon = {
    grid: `<path class="node" d="M-16 34 L-6 -30 L6 -30 L16 34 M-24 -18 H24 M-20 -6 H20 M-9 -10 L9 12 M9 -10 L-9 12"/>`,
    meter: `<rect class="node" x="-17" y="-28" width="34" height="56" rx="4"/><rect class="node" x="-10" y="-20" width="20" height="13"/><path class="node" d="M-9 2 H9 M-9 10 H9 M-9 18 H9"/>`,
    solar: `<path class="node" d="M-30 18 L-16 -20 H30 L16 18 Z M-23 -1 H23 M-3 -20 L-14 18 M14 -20 L2 18"/>`,
    battery: `<rect class="node" x="-15" y="-24" width="30" height="50" rx="4"/><path class="node" d="M-6 -30 H6"/>`,
    ev: `<path class="node" d="M-30 18 V-14 H8 V18 M8 -6 H22 L30 6 V18 H-30"/><circle class="node" cx="-14" cy="20" r="6"/><circle class="node" cx="16" cy="20" r="6"/>`,
    load: `<path class="node" d="M-28 24 V-8 L-10 -18 V-8 L8 -18 V-8 L28 -18 V24 Z M-14 8 H-4 M6 8 H16"/>`,
  };
  const socPct = t.battery_soc_kwh != null && i.battery ? 100 * t.battery_soc_kwh / i.battery.capacity_kwh : null;
  const node = (x, y, color, draw, label) => `<g transform="translate(${x} ${y})" style="color:${css(color)}">${draw}<text class="flowv" y="56">${label}</text></g>`;
  const offset = `animation-delay:-${performance.now() % 1000}ms`; // keeps the dashes smooth across re-renders
  const flow = (d, color, kwNow, towardsMeter) => {
    const live = Math.abs(kwNow) > 0.05;
    return `<path d="${d}" class="flow ${live ? 'live' : ''} ${towardsMeter ? 'rev' : ''}" stroke="${css(live ? color : 'line')}" style="${offset}"/>`;
  };
  const socFill = socPct == null ? '' : `<rect x="-11" y="${22 - 42 * socPct / 100}" width="22" height="${42 * socPct / 100}" fill="var(--battery)" opacity=".35"/>`;
  return `<svg viewBox="0 0 1000 400" class="chart">
    ${flow('M160 170 H300', 'grid', t.grid_kw, t.grid_kw < 0)}
    ${flow('M360 160 C560 160 560 60 760 60', 'solar', t.pv_kw, true)}
    ${flow('M360 170 H760', 'battery', t.battery_kw, t.battery_kw < 0)}
    ${flow('M360 180 C560 180 560 300 760 300', 'charger', t.ev_kw, false)}
    ${flow('M330 205 V260', 'load', t.load_kw, false)}
    ${node(110, 170, 'grid', icon.grid, `${num(t.grid_kw)} kW`)}
    ${node(330, 170, 'grid', icon.meter, '')}
    ${node(810, 60, 'solar', icon.solar, `${num(t.pv_kw)} kW`)}
    ${i.battery ? node(810, 170, 'battery', socFill + icon.battery, `${num(socPct, 0)}%`) : ''}
    ${node(810, 300, 'charger', icon.ev, `${num(t.ev_kw)} kW`)}
    ${node(330, 310, 'load', icon.load, `${num(t.load_kw)} kW`)}
  </svg>`;
}

function statusLine(s) {
  const fresh = s.last_seen_secs == null ? 'never' : `${num(s.last_seen_secs, 0)} seconds ago`;
  const control = !s.online ? ['', 'offline'] : s.latest?.mode === 'cloud' ? ['ok', 'cloud schedule'] : ['warn', 'local fallback'];
  return { fresh, control };
}

const powerbase = {
  dashboard: {
    load: async id => ({ rows: await getDay(id), month: await getSummary(id, period('month')) }),
    render: ({ rows, month }) => {
      const s = site(), r = month.report, flip = Math.floor(Date.now() / 3000) % 2 === 0;
      const ok = s.online ? assetCount(s.info) : 0;
      const billTotal = sum(month.days, d => d.total_eur);
      return `<div class="row tiles">
        ${tile('powerbase/status', 'Status', 'Assets', `${ok}<small> / ${assetCount(s.info)} ok</small>`)}
        ${tile('powerbase/measurements', 'Measurements', flip ? 'Import' : 'Export', kwh(flip ? r.import_kwh : r.export_kwh))}
        ${tile('bill/location', 'Energy bill', 'Current month', eur(billTotal))}
        ${tile('reporting/report', 'Reporting', flip ? 'Avoided violations' : 'CO₂ reduction',
          flip ? r.baseline_over_limit_ptus - r.over_limit_ptus : unit(num(r.co2_reduction_kg, 0), 'kg'))}
      </div>
      <div class="card">${s.latest ? flowDiagram(s) : '<div class="empty">No readings yet</div>'}</div>
      <div class="card">${powerChart(rows, ui.gridView, ['grid', 'source', 'destination'], 'gridView')}</div>`;
    },
  },
  status: {
    load: async id => ({ rows: await getDay(id) }),
    render: ({ rows }) => {
      const s = site(), { fresh, control } = statusLine(s), past = rows.slice(0, lastMeasured(rows) + 1);
      const okPct = past.length ? 100 * past.filter(r => r.measured).length / past.length : 0;
      const groups = assets(s.info).map(a => {
        const open = ui.expanded === a.key;
        return `<div class="group" style="--c:${css(a.color)}">${a.group}</div>
        <button class="asset" data-act="expand:${a.key}"><span>${esc(a.name)}</span>
          <span><span class="dot ${s.online ? 'ok' : ''}"></span>Collect data · ${fresh}
          ${a.control ? `<br><span class="dot ${control[0]}"></span>Control · ${control[1]}` : ''}</span><span>${open ? '⌄' : '›'}</span></button>
        ${open ? `<div class="ticks">${rows.map(r => `<i class="${r.measured ? (a.control && r.measured.mode !== 'cloud' ? 'warn' : 'ok') : ''}"></i>`).join('')}</div>
          <div class="k">${longDate(selDate())} · Ok ${num(okPct, 0)}%</div>` : ''}`;
      }).join('');
      const cmds = s.commands.map(c => `<tr><td class="mono">${c.command_id.slice(0, 8)}</td><td>${hhmm(c.issued_at)}</td>
        <td>${c.state.state === 'acked' ? c.state.ack.status : c.state.state}</td><td class="n">${c.attempts}</td>
        <td class="n">${c.ack_latency_ms != null ? c.ack_latency_ms + ' ms' : '–'}</td></tr>`).join('');
      return `${dayBar()}<div class="card">${groups}</div>
      <div class="card"><h2>Cloud commands</h2><div class="k">Latest plan took ${num(s.plan_compute_ms)} ms</div>
        <table><tr><th>Command</th><th>Issued</th><th>State</th><th class="n">Attempts</th><th class="n">Ack latency</th></tr>${cmds}</table></div>`;
    },
  },
  measurements: {
    load: async id => ({ rows: await getDay(id) }),
    render: ({ rows }) => `${dayBar()}<div class="card">${powerChart(rows, ui.measureView, Object.keys(VIEWS), 'measureView')}</div>`,
  },
};

// ---------- PowerTrader ----------
const nomGrid = r => r.nomination ? r.nomination.load_kw + r.nomination.ev_kw + r.nomination.battery_kw - r.nomination.pv_kw : null;
const nomSeries = {
  stacks: [
    { label: 'Import', color: 'blue', v: r => M(r, t => Math.max(t.grid_kw, 0)) },
    { label: 'To grid', color: 'charger', v: r => M(r, t => Math.min(t.grid_kw, 0)) }],
  lines: [{ label: 'Nomination', color: 'nomination', dash: true, v: nomGrid }],
  tip: r => tipBody(r, [['Import', r.measured ? `${num(Math.max(r.measured.grid_kw, 0))} kW` : null, 'blue'],
    ['To grid', r.measured ? `${num(Math.min(r.measured.grid_kw, 0))} kW` : null, 'blue'], ['Nomination', r.nomination ? `${num(nomGrid(r), 0)} kW` : null, 'nomination']]),
};
const priceTip = (label, f) => r => tipBody(r, [[label, f(r) == null ? null : eur(f(r)), 'blue']]);
const priceChart = (rows, lines, tip, height) => chart({ rows, lines, now: nowIndex(rows), tip, height, yFmt: v => eur(v, 0), labelFmt: v => eur(v) });
const spread = vals => vals.length ? Math.max(...vals) - Math.min(...vals) : null;
const values = (rows, f) => rows.map(f).filter(v => v != null);

const powertrader = {
  dashboard: {
    load: async id => ({ rows: await getDay(id) }),
    render: ({ rows }) => {
      const imb = r => r.imbalance_eur_mwh, da = r => r.day_ahead_eur_mwh;
      const imp = sum(rows, r => M(r, t => Math.max(t.grid_kw, 0))) * PTU_HOURS;
      const exp = sum(rows, r => M(r, t => Math.max(-t.grid_kw, 0))) * PTU_HOURS;
      const costs = rows.filter(r => r.cost_eur != null);
      const side = (title, label, value) => `<div class="card sidev"><h2>${title}</h2><div><div class="k">${label}</div><div class="v">${value}</div></div></div>`;
      const panel = (title, body) => `<div class="card"><h2>${title}</h2>${body}</div>`;
      const row = (a, b) => `<div class="row side">${a}${b}</div>`;
      return row(side('Nomination', 'Import / export', `${kwh(imp)} / ${kwh(exp)}`),
          panel('Nomination and actual measurements', chart({ rows, ...nomSeries, now: nowIndex(rows), height: 200 })))
        + row(side('Day-ahead', 'Spread', unit(eur(spread(values(rows, da))), '/MWh')),
          panel('Day-ahead', priceChart(rows, [{ color: 'blue', v: da }], priceTip('Price', da), 200)))
        + row(side('Imbalance', 'Spread', unit(eur(spread(values(rows, imb))), '/MWh')),
          panel('Imbalance', priceChart(rows, [{ color: 'charger', v: imb }], priceTip('Price', imb), 200)))
        + row(side('Energy costs', 'Today, including forecast', eur(sum(costs, r => r.cost_eur))),
          panel('Total', chart({ rows, now: nowIndex(rows), height: 200, yFmt: v => eur(v, 0), labelFmt: v => eur(v),
            stacks: [{ color: 'blue', v: r => r.cost_eur > 0 ? r.cost_eur : null }, { color: 'charger', v: r => r.cost_eur < 0 ? r.cost_eur : null }],
            tip: r => tipBody(r, [[r.measured ? 'Costs' : 'Forecast', r.cost_eur == null ? null : eur(r.cost_eur), 'blue']]) })));
    },
  },
  nomination: {
    load: async id => ({ rows: await getDay(id) }),
    render: ({ rows }) => {
      const delta = r => r.measured && r.nomination ? r.measured.grid_kw - nomGrid(r) : null;
      const part = (label, color, f) => ({ label, color, v: r => r.nomination ? f(r.nomination) * PTU_HOURS : null });
      const stacks = [part('Battery', 'battery', n => n.battery_kw), part('Solar', 'solar', n => -n.pv_kw),
        part('Charger', 'charger', n => n.ev_kw), part('Uncontrolled load', 'load', n => n.load_kw)];
      const imp = sum(rows, r => M(r, t => Math.max(t.grid_kw, 0))) * PTU_HOURS;
      const exp = sum(rows, r => M(r, t => Math.max(-t.grid_kw, 0))) * PTU_HOURS;
      return `${dayBar()}
      <div class="card"><h2>Nomination and actual measurements</h2>${chart({ rows, ...nomSeries, now: nowIndex(rows) })}${chips([['Import', kwh(imp), 'blue'], ['To grid', kwh(exp), 'blue']])}</div>
      <div class="card"><h2>Delta with nomination</h2>${chart({ rows, height: 170, now: nowIndex(rows),
        stacks: [{ color: 'nomination', v: delta }], tip: r => tipBody(r, [['Delta', delta(r) == null ? null : `${num(delta(r))} kW`, 'nomination']]) })}</div>
      <div class="card"><h2>Split nomination</h2>${chart({ rows, stacks, height: 220, now: nowIndex(rows), yFmt: v => num(v, 0) + ' kWh',
        tip: r => tipBody(r, stacks.map(s => [s.label, s.v(r) == null ? null : `${num(s.v(r))} kWh`, s.color])) })}
        ${chips(stacks.map(s => [s.label, kwh(sum(rows, s.v)), s.color]))}</div>`;
    },
  },
  markets: {
    load: async id => ({ rows: await getDay(id) }),
    render: ({ rows }) => {
      const m = {
        dayahead: { label: 'Day-ahead', lines: [{ color: 'blue', v: r => r.day_ahead_eur_mwh }] },
        imbalance: { label: 'Imbalance', lines: [{ color: 'charger', v: r => r.imbalance_eur_mwh }] },
        settlement: { label: 'Settlement', lines: [{ color: 'blue', v: r => r.settlement_eur_mwh?.[0] }, { color: 'charger', v: r => r.settlement_eur_mwh?.[1] }] },
        co2: { label: 'CO₂ intensity', lines: [{ color: 'blue', v: r => r.co2_g_kwh }] },
      };
      const view = m[ui.marketView], isCo2 = ui.marketView === 'co2';
      const vals = view.lines.flatMap(l => values(rows, l.v));
      const fmt = v => isCo2 ? unit(num(v, 0), 'g/kWh') : eur(v);
      const avg = vals.length ? sum(vals, v => v) / vals.length : null;
      const body = !vals.length
        ? `<div class="empty">Settlement prices are final a day later. Go back to a previous day.</div>`
        : (isCo2
          ? chart({ rows, lines: view.lines, now: nowIndex(rows), yFmt: v => num(v, 0), tip: r => tipBody(r, [['Emission', `${num(r.co2_g_kwh, 0)} g/kWh`, 'blue']]) })
          : priceChart(rows, view.lines, r => tipBody(r, view.lines.map((l, i) => [view.lines.length > 1 ? ['Import', 'Export'][i] : 'Price', l.v(r) == null ? null : eur(l.v(r)), l.color]))))
          + chips([['Maximum', fmt(Math.max(...vals))], ['Average', fmt(avg)], ['Minimum', fmt(Math.min(...vals))]]);
      return `${dayBar()}<div class="card"><div class="split"><div class="modes">
        ${Object.keys(m).map(k => btn(m[k].label, `marketView:${k}`, k === ui.marketView)).join('')}</div><div>${body}</div></div></div>`;
    },
  },
};

// ---------- Energy bill ----------
function billLines(b) {
  const c = a => ({ cost: a.nomination_eur + a.imbalance_eur, ...a });
  const on = ui.bill;
  const grid = (on.energy ? b.energy_tax_eur : 0) + (on.transport ? b.transport_eur : 0);
  const asset = (label, color, a, extra = 0) => { const x = c(a); return { label, color, ...x, extra, subtotal: on.energy ? x.cost + extra : extra }; };
  return {
    grid, tax: b.energy_tax_eur, transport: b.transport_eur,
    assets: [asset('Solar', 'solar', b.solar), asset('Battery', 'battery', b.battery),
      asset('Charger', 'charger', b.charger, on.subsidies ? b.subsidies_eur : 0), asset('Uncontrollable load', 'load', b.load)],
  };
}
const billTotal = b => { const l = billLines(b); return l.grid + sum(l.assets, a => a.subtotal); };

function billList(name, b, importKwh) {
  const l = billLines(b), on = ui.bill;
  const cell = (label, value) => value == null ? '<div class="blank"></div>' : `<div>${label}<b>${value}</b></div>`;
  const perKwh = a => a.energy_kwh ? eur(a.cost / Math.abs(a.energy_kwh)) : null;
  return `<div class="billhead">${esc(name)}</div>
  <div class="billrow" style="--c:var(--grid)"><div class="blank"><b>Grid connection</b></div>
    ${cell('Energy tax', on.energy ? eur(l.tax) : null)}${cell('Transport costs', on.transport ? eur(l.transport) : null)}
    <div class="blank"></div>${cell('Subtotal', eur(l.grid))}${cell('Energy', unit(num(importKwh, 0), 'kWh'))}<div class="blank"></div></div>
  ${l.assets.map(a => `<div class="billrow" style="--c:${css(a.color)}"><div class="blank"><b>${a.label}</b></div>
    ${cell('Nomination', on.energy ? eur(a.nomination_eur) : null)}${cell('Imbalance', on.energy ? eur(a.imbalance_eur) : null)}
    ${cell('EV credits', a.extra ? eur(a.extra) : null)}${cell('Subtotal', eur(a.subtotal))}
    ${cell('Energy', unit(num(a.energy_kwh, 0), 'kWh'))}${cell('€ / kWh', perKwh(a))}</div>`).join('')}
  <div class="billtotal">Total<b>${eur(billTotal(b))}</b></div>`;
}

function billTable(b) {
  const l = billLines(b);
  return `<table><tr><th>Asset</th><th class="n">Nomination</th><th class="n">Imbalance</th><th class="n">Other</th><th class="n">Subtotal</th><th class="n">Energy</th></tr>
    <tr><td>Grid connection</td><td></td><td></td><td class="n">${eur(l.grid)}</td><td class="n">${eur(l.grid)}</td><td></td></tr>
    ${l.assets.map(a => `<tr><td>${a.label}</td><td class="n">${eur(a.nomination_eur)}</td><td class="n">${eur(a.imbalance_eur)}</td>
      <td class="n">${a.extra ? eur(a.extra) : ''}</td><td class="n">${eur(a.subtotal)}</td><td class="n">${num(a.energy_kwh, 0)} kWh</td></tr>`).join('')}
    <tr><th>Total</th><td></td><td></td><td></td><th class="n">${eur(billTotal(b))}</th><td></td></tr></table>`;
}

function billChart(days) {
  if (!days.length) return '<div class="empty">No data in this period</div>';
  const rows = days.map(d => ({ ...d, ts: d.date + 'T00:00:00Z' }));
  const every = Math.ceil(rows.length / 10);
  return chart({ rows, stacks: [{ color: 'blue', v: r => r.total_eur > 0 ? r.total_eur : null }, { color: 'charger', v: r => r.total_eur < 0 ? r.total_eur : null }],
    xLabels: rows.map((r, i) => [i + .5, r.date.slice(8)]).filter((_, i) => i % every === 0), yFmt: v => eur(v, 0), labelFmt: v => eur(v),
    tip: r => `<b>${longDate(r.date)}</b><div><span>Total</span><span>${eur(r.total_eur)}</span></div>` }) + chips([['Total', eur(sum(days, d => d.total_eur))]]);
}

const billFilters = (views = true) => `<div class="filters">
  ${[['energy', 'Energy bill'], ['transport', 'Transport'], ['subsidies', 'Subsidies']].map(([k, l]) =>
    `<label><input type="checkbox" data-act="bill:${k}" ${ui.bill[k] ? 'checked' : ''}>${l}</label>`).join('')}
  ${views ? `<span class="views">${[['list', 'List'], ['table', 'Table'], ['chart', 'Chart']].map(([k, l]) => btn(l, `billView:${k}`, ui.billView === k)).join('')}</span>` : ''}</div>`;

const energyBill = {
  location: {
    load: async id => ({ s: await getSummary(id) }),
    render: ({ s }) => {
      const info = site().info;
      const body = ui.billView === 'chart' ? billChart(s.days) : ui.billView === 'table' ? billTable(s.bill) : billList(info.name, s.bill, s.report.import_kwh);
      return `${periodBar()}<div class="card">${billFilters()}${body}</div>`;
    },
  },
  portfolio: {
    load: async () => {
      const list = ui.sites.filter(s => s.info);
      return { list, sums: await Promise.all(list.map(s => getSummary(s.site_id))) };
    },
    render: ({ list, sums }) => {
      const rows = list.map((s, i) => { const r = sums[i].report; return `<tr><td>${esc(s.info.name)}</td><td class="n">${num(r.import_kwh, 0)} kWh</td>
        <td class="n">${num(r.export_kwh, 0)} kWh</td><td class="n">${eur(billTotal(sums[i].bill))}</td></tr>`; }).join('');
      const total = sum(sums, x => billTotal(x.bill));
      return `${periodBar()}<div class="card">${billFilters(false)}
        <table><tr><th>Location</th><th class="n">Import</th><th class="n">Export</th><th class="n">Bill</th></tr>${rows}
        <tr><th>Portfolio</th><td></td><td></td><th class="n">${eur(total)}</th></tr></table></div>`;
    },
  },
};

// ---------- Location ----------
const location_ = {
  assets: {
    render: () => {
      const list = assets(site().info);
      return `<div class="card">${list.map(a => `<div class="group" style="--c:${css(a.color)}">${a.group}</div>
        <div class="asset"><span>${esc(a.name)}</span><dl>${a.details.map(([k, v]) => `<dt>${k}</dt><dd>${esc(v)}</dd>`).join('')}</dl>
        <button class="dl" data-act="csv:${a.key}">⬇ Quarter-hourly data</button></div>`).join('')}</div>`;
    },
  },
  topology: {
    render: () => {
      const i = site().info;
      const box = (name, sub = '', c = 'blue') => `<div class="box" style="--c:${css(c)}">${esc(name)}<small>${esc(sub)}</small></div>`;
      // Chargers sit in the middle, right above the EVs that hang off them.
      const row = assets(i).filter(a => a.key !== 'grid' && a.key !== 'charger').map(a => box(a.name, a.details[0][1], a.color));
      row.unshift(box('Gateway', i.firmware, 'grid'));
      row.push(box('Uncontrollable load', 'Sub-meter', 'load'));
      if (i.chargers) row.splice(row.length / 2, 0, box('Chargers', `${i.chargers} × ${kw(i.charger_kw)}`, 'charger'));
      const chargers = Array.from({ length: i.chargers }, (_, n) => box(`EV ${n + 1}`, kw(i.charger_kw), 'charger')).join('');
      return `<div class="card"><div class="tree">
        <div class="lvl">${box(i.name, 'Location')}</div>
        <div class="lvl">${box('Main meter', `${kw(i.grid_limit_kw)} / ${kw(i.export_limit_kw)}`, 'grid')}</div>
        <div class="lvl">${row.join('')}</div>
        ${chargers ? `<div class="lvl">${chargers}</div>` : ''}</div></div>`;
    },
  },
};

// ---------- Reporting ----------
const reporting = {
  report: {
    load: async id => ({ s: await getSummary(id) }),
    render: ({ s }) => {
      const i = site().info, r = s.report, f = r.flows_kwh, b = s.bill;
      const hours = r.ptus * PTU_HOURS;
      const saved = r.baseline_cost_eur - r.actual_cost_eur;
      const selfUsed = f.pv_to_load + f.pv_to_ev + f.pv_to_battery, produced = selfUsed + f.pv_to_grid;
      const fromBattery = f.battery_to_load + f.battery_to_ev + f.battery_to_grid;
      const charged = f.grid_to_ev + f.pv_to_ev + f.battery_to_ev;
      const chargerCost = b.charger.nomination_eur + b.charger.imbalance_eur;
      const solarRevenue = -(b.solar.nomination_eur + b.solar.imbalance_eur);
      return `${periodBar()}
      <h3 style="margin-top:6px">Location</h3>${stats([['CO₂ emissions', unit(num(r.co2_kg, 0), 'kg')], ['CO₂ reduction', unit(num(r.co2_reduction_kg, 0), 'kg')],
        ['Assets', assetCount(i)], ['Energy bill', eur(billTotal(b))], ['Transport', eur(b.transport_eur)]])}
      <h3>Grid connection (${kw(i.grid_limit_kw)} / -${kw(i.export_limit_kw)})</h3>
      ${stats([['Import', kwh(r.import_kwh)], ['Export', kwh(r.export_kwh)], ['Avoided violations', r.baseline_over_limit_ptus - r.over_limit_ptus],
        ['Utilization rate', hours ? unit(num(100 * r.import_kwh / (i.grid_limit_kw * hours), 0), '%') : '–']])}
      <div class="card donuts">
        ${donut('From grid (kWh)', [{ l: 'Load', v: f.grid_to_load, c: 'load' }, { l: 'Charger', v: f.grid_to_ev, c: 'charger' }, { l: 'Battery', v: f.grid_to_battery, c: 'battery' }])}
        ${donut('To grid (kWh)', [{ l: 'Solar', v: f.pv_to_grid, c: 'solar' }, { l: 'Battery', v: f.battery_to_grid, c: 'battery' }])}</div>
      ${i.pv_kwp > 0 ? `<h3 style="color:var(--solar)">Solar (${num(i.pv_kwp, 0)} kWp)</h3>
      ${stats([['Produced', kwh(produced)], ['Self consumed', produced ? unit(num(100 * selfUsed / produced, 0), '%') : '–'], ['Revenues', eur(solarRevenue)]])}
      <div class="card donuts">${donut('From solar (kWh)', [{ l: 'Load', v: f.pv_to_load, c: 'load' }, { l: 'Charger', v: f.pv_to_ev, c: 'charger' }, { l: 'Battery', v: f.pv_to_battery, c: 'battery' }, { l: 'Export', v: f.pv_to_grid, c: 'grid' }])}</div>` : ''}
      ${i.battery ? `<h3 style="color:var(--battery)">Battery (${kw(i.battery.max_power_kw)} / ${num(i.battery.capacity_kwh, 0)} kWh)</h3>
      ${stats([['Discharged', kwh(fromBattery)], ['Result', eur(-(b.battery.nomination_eur + b.battery.imbalance_eur))]])}
      <div class="card donuts">
        ${donut('To battery (kWh)', [{ l: 'Grid', v: f.grid_to_battery, c: 'grid' }, { l: 'Solar', v: f.pv_to_battery, c: 'solar' }])}
        ${donut('From battery (kWh)', [{ l: 'Load', v: f.battery_to_load, c: 'load' }, { l: 'Charger', v: f.battery_to_ev, c: 'charger' }, { l: 'Export', v: f.battery_to_grid, c: 'grid' }])}</div>` : ''}
      ${i.chargers ? `<h3 style="color:var(--charger)">Charging (${kw(i.chargers * i.charger_kw)})</h3>
      ${stats([['Chargers', i.chargers], ['Charged', kwh(charged)], ['Costs', eur(chargerCost)], ['EV credits', eur(-b.subsidies_eur)]])}
      <div class="card donuts">${donut('To charger (kWh)', [{ l: 'Grid', v: f.grid_to_ev, c: 'grid' }, { l: 'Solar', v: f.pv_to_ev, c: 'solar' }, { l: 'Battery', v: f.battery_to_ev, c: 'battery' }])}</div>` : ''}
      <h3>EMS impact (versus no EMS)</h3>
      ${stats([['Saved', `${eur(saved)} <small>${r.baseline_cost_eur > 0 ? num(100 * saved / r.baseline_cost_eur, 0) : '–'}%</small>`],
        ['Peak vs baseline', unit(`${num(r.actual_peak_kw, 0)} / ${num(r.baseline_peak_kw, 0)}`, 'kW')],
        ['PTUs over limit', unit(`${r.over_limit_ptus} / ${r.baseline_over_limit_ptus}`, 'baseline')],
        ['EV charging deferred', kwh(r.ev_curtailed_kwh)],
        ['On cloud schedule', r.ptus ? unit(num(100 * r.cloud_mode_ptus / r.ptus, 0), '%') : '–']])}`;
    },
  },
};

// ---------- router ----------
const ICON = {
  powerbase: '<path d="M3 12h4l3-7 4 14 3-7h4"/>',
  powertrader: '<path d="M4 20V10l5 3V10l5 3V4h6v16z"/>',
  bill: '<path d="M18 6a7 7 0 1 0 0 12M4 10h10M4 14h9"/>',
  location: '<path d="M12 21s7-6.5 7-12a7 7 0 1 0-14 0c0 5.5 7 12 7 12z"/><circle cx="12" cy="9" r="2.5"/>',
  reporting: '<path d="M6 3h9l4 4v14H6zM9 12h7M9 16h7"/>',
};
const MODULES = {
  powerbase: { title: 'PowerBase', pages: powerbase, tabs: [['dashboard', 'Dashboard'], ['status', 'Status'], ['measurements', 'Measurements']] },
  powertrader: { title: 'PowerTrader', pages: powertrader, tabs: [['dashboard', 'Dashboard'], ['nomination', 'Nomination'], ['markets', 'Markets']] },
  bill: { title: 'Energy bill', pages: energyBill, tabs: [['location', 'Location'], ['portfolio', 'Portfolio']] },
  location: { title: 'Location', pages: location_, tabs: [['assets', 'Assets'], ['topology', 'Topology']] },
  reporting: { title: 'Reporting', pages: reporting, tabs: [] },
};

function route() {
  const [, mod = 'powerbase', tab] = location.hash.split('/');
  const module = MODULES[mod] ?? MODULES.powerbase;
  const key = module.pages[tab] ? tab : Object.keys(module.pages)[0];
  return { mod: MODULES[mod] ? mod : 'powerbase', module, tab: key, page: module.pages[key] };
}

function renderRail(mod) {
  $('#rail').innerHTML = Object.entries(MODULES).map(([k, m]) =>
    `<a href="#/${k}" class="${k === mod ? 'on' : ''}"><svg viewBox="0 0 24 24">${ICON[k]}</svg>${m.title}</a>`).join('');
}

function syncSitePicker() {
  const ready = ui.sites.filter(s => s.info);
  if (!ready.some(s => s.site_id === ui.site)) ui.site = ready[0]?.site_id ?? null;
  const key = ready.map(s => s.site_id).join();
  const picker = $('#site');
  if (picker.dataset.key !== key) {
    picker.dataset.key = key;
    picker.innerHTML = ready.map(s => `<option value="${esc(s.site_id)}">${esc(s.info.name)}</option>`).join('');
  }
  picker.value = ui.site ?? '';
}

let generation = 0, pressed = false;

async function refresh() {
  const mine = ++generation;
  try {
    const sites = await json('/api/sites');
    if (mine !== generation) return;
    ui.sites = sites;
    syncSitePicker();
    const { mod, module, tab, page } = route();
    renderRail(mod);
    if (!site()?.latest) { $('#page').innerHTML = '<div class="empty">Waiting for gateways…</div>'; return; }
    const data = page.load ? await page.load(ui.site) : {};
    if (mine !== generation || pressed) return; // superseded, or mid-click: don't swap the DOM
    const s = site();
    const offline = s.online ? '' : ' <span style="color:var(--limit)">· offline</span>';
    const tabs = module.tabs.length ? `<div class="tabs">${module.tabs.map(([k, l]) => `<a href="#/${mod}/${k}" class="${k === tab ? 'on' : ''}">${l}</a>`).join('')}</div>` : '';
    $('#page').innerHTML = `<h1>${module.title} <span>| ${esc(s.info.name)}${offline}</span></h1>${tabs}${page.render(data)}`;
  } catch (e) {
    console.error(e);
    $('#page').innerHTML = '<div class="empty">API unreachable</div>';
  }
}

const ACTIONS = {
  shift: n => {
    const next = addDays(selDate(), Number(n));
    ui.date = next >= liveDate() ? null : next;
  },
  live: () => { ui.date = null; },
  period: p => { ui.period = p; },
  gridView: v => { ui.gridView = v; },
  measureView: v => { ui.measureView = v; },
  marketView: v => { ui.marketView = v; },
  billView: v => { ui.billView = v; },
  bill: k => { ui.bill[k] = !ui.bill[k]; },
  expand: k => { ui.expanded = ui.expanded === k ? null : k; },
  csv: k => downloadCsv(assets(site().info).find(a => a.key === k)),
};

document.addEventListener('click', e => {
  const el = e.target.closest('[data-act]');
  if (!el) return;
  const [name, arg] = el.dataset.act.split(':');
  ACTIONS[name](arg);
  refresh();
});
document.addEventListener('pointerdown', () => { pressed = true; });
document.addEventListener('pointerup', () => { pressed = false; });

document.addEventListener('mousemove', e => {
  const tip = $('#tip'), svg = e.target.closest?.('svg[data-chart]'), c = svg && TIPS[svg.dataset.chart];
  if (!c?.tip) { tip.style.display = 'none'; return; }
  const box = svg.getBoundingClientRect();
  const i = Math.floor(((e.clientX - box.left) / box.width * 1000 - c.L) / c.slot);
  const row = c.rows[i];
  if (!row) { tip.style.display = 'none'; return; }
  tip.innerHTML = c.tip(row);
  tip.style.display = 'block';
  tip.style.left = Math.min(e.clientX + 14, innerWidth - tip.offsetWidth - 8) + 'px';
  tip.style.top = Math.min(e.clientY + 14, innerHeight - tip.offsetHeight - 8) + 'px';
});

function setTheme(theme) {
  document.documentElement.dataset.theme = theme;
  try { localStorage.setItem('theme', theme); } catch { /* private mode: not remembered */ }
}
$('#theme').addEventListener('click', () => setTheme(document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark'));
$('#site').addEventListener('change', e => { ui.site = e.target.value; ui.date = null; ui.expanded = null; refresh(); });
addEventListener('hashchange', refresh);
try { setTheme(localStorage.getItem('theme') ?? 'light'); } catch { setTheme('light'); }
refresh();
setInterval(refresh, 1500);
