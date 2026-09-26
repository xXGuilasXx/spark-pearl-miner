// Screens: Dashboard, Pools, Failover, Performance & Power, Fee, Logs, About.
// Each screen has render(main, ctx) (once) and update(ctx) (live data, about once per second).

import { h, replace, fmtTime, fmtDateTime, fmtDuration, fmtNum, fmtRate, abbrev, download } from './dom.js';
import { t, tt } from './i18n.js';
import * as api from './api.js';
import { poolEditor, errorText } from './pooleditor.js';
import { maxPhrase } from './wizard.js';

// ----- shared bits -----

const STATE_CLASS = {
  mining: 'st-ok', active: 'st-ok', hashing: 'st-ok', ready: 'st-ok', standby: 'st-ok',
  starting: 'st-busy', failing_over: 'st-warn', resolving: 'st-busy', connecting: 'st-busy', tls_handshake: 'st-busy',
  authorizing: 'st-busy', awaiting_job: 'st-busy', draining: 'st-busy', waiting_external: 'st-busy',
  paused: 'st-warn', backoff: 'st-warn', setup_required: 'st-warn',
  all_down: 'st-bad', config_error: 'st-bad', quarantined: 'st-bad', faulted: 'st-bad', unavailable: 'st-bad',
};

export function chip(code, prefix = 'state') {
  return h('span', { class: `chip ${STATE_CLASS[code] || ''}` }, tt(`${prefix}.${code}`) || code);
}

/** The failover manager state in the current language. */
export function managerText(v) {
  if (!v || !v.manager_code) return v ? v.manager || '' : '';
  return t(`mgr.${v.manager_code}`, { n: v.manager_to, from: v.manager_from, to: v.manager_to });
}

function card(label, value, sub) {
  return h('div', { class: 'card' }, h('div', { class: 'label' }, label), h('div', { class: 'value' }, value), sub ? h('div', { class: 'sub' }, sub) : null);
}

async function saveConfig(ctx, mutate, msgEl) {
  const cfg = structuredClone(ctx.config);
  mutate(cfg);
  try {
    const r = await api.put('/api/v1/config', cfg);
    ctx.config = cfg;
    replace(msgEl, h('div', { class: 'okmsg' }, r.restart_required && r.restart_required.length ? t('save.ok_restart') : t('save.ok')));
    ctx.toast(t('save.ok'), 'ok');
    return true;
  } catch (e) {
    const fields = (e.data && e.data.fields) || [];
    replace(msgEl, h('div', { class: 'err' }, e.status === 422 && e.data && e.data.error === 'fee_not_configurable' ? t('save.fee_refused') : e.message),
      fields.map((f) => h('div', { class: 'err' }, `${f.path}: ${f.message}`)));
    return false;
  }
}

async function control(ctx, path) {
  try {
    const r = await api.post(path);
    ctx.toast(r.message || 'ok', 'ok');
  } catch (e) {
    ctx.toast(e.message, 'error');
  }
  ctx.poll();
}

// ----- Dashboard -----

function controls(ctx, s) {
  const btn = (key, op, cls) => h('button', { type: 'button', class: cls || '', onclick: () => control(ctx, `/api/v1/mining/${op}`) }, t(key));
  if (!s.running || s.pause_reason === 'hardware_fault') {
    return [btn('ctl.start', 'start', 'primary'), s.running ? btn('ctl.stop', 'stop', 'danger') : null];
  }
  if (s.paused) return [btn('ctl.resume', 'resume', 'primary'), btn('ctl.stop', 'stop', 'danger')];
  return [btn('ctl.pause', 'pause'), btn('ctl.stop', 'stop', 'danger')];
}

const dashboard = {
  render(main, ctx) {
    this.live = h('div');
    this.buttons = h('div', { class: 'row' });
    this.btnSig = null;
    replace(main,
      h('div', { class: 'row' }, h('h1', null, t('nav.dashboard')), h('span', { class: 'spacer' }), this.buttons),
      this.live);
    this.update(ctx);
  },
  update(ctx) {
    const s = ctx.status;
    if (!s || !this.live) return;
    const sig = `${s.running}|${s.paused}|${s.pause_reason}`;
    if (sig !== this.btnSig) {
      this.btnSig = sig;
      replace(this.buttons, controls(ctx, s));
    }
    const active = ctx.pools && ctx.pools.slots ? ctx.pools.slots.find((x) => x.index === s.active_pool) : null;
    const sh = s.shares || {};
    replace(this.live,
      h('div', { class: 'grid' },
        h('div', { class: 'card' }, h('div', { class: 'label' }, t('dash.state')), h('div', { class: 'value' }, chip(s.state)),
          h('div', { class: 'sub' }, s.pause_reason ? (tt(`pause.${s.pause_reason}`) || s.pause_reason) : managerText(s))),
        card(t('dash.pool'), active ? t('pools.slot', { n: active.index }) : '—', active ? `${active.name ? `${active.name} · ` : ''}${active.host}:${active.port}` : t('dash.no_pool')),
        card(t('dash.hashrate'), fmtRate(s.hashrate_tmacs), s.worker && s.worker.simulated ? t('dash.hashrate.sim') : t('dash.hashrate.sub')),
        card(t('dash.shares'), `${sh.accepted || 0} / ${sh.rejected || 0}`, t('dash.shares.sub', { stale: sh.stale || 0, discarded: sh.discarded || 0 })),
        h('div', { class: 'card' }, h('div', { class: 'label' }, t('dash.worker')), h('div', { class: 'value' }, chip(s.worker ? s.worker.state : 'absent', 'worker')),
          h('div', { class: 'sub' }, s.worker && s.worker.device ? s.worker.device : t(`launch.${s.worker ? s.worker.launch : 'spawn'}`))),
        card(t('dash.target'), t(`target.${s.mining_target}`), t('fee.phase', { phase: tt(`feephase.${s.fee_phase}`) || s.fee_phase })),
        card(t('dash.uptime'), fmtDuration(s.uptime_s), t('dash.mining_time', { t: fmtDuration(s.mining_s) })),
        card(t('dash.wallet'), abbrev(s.wallet) || '—', `${t('worker.label')}: ${s.worker_name}`)),
      s.worker && s.worker.last_fault ? h('p', { class: 'err' }, t('dash.last_fault', { msg: s.worker.last_fault })) : null,
      h('h2', null, t('dash.alerts')),
      s.alerts && s.alerts.length
        ? h('ul', { class: 'timeline' }, s.alerts.map((a) => h('li', null, h('time', null, fmtTime(a.at_ms)), h('span', { class: a.level === 'error' ? 'alert' : '' }, a.msg))))
        : h('p', { class: 'muted' }, t('dash.no_alerts')));
  },
};

// ----- Pools -----

function timelineList(items) {
  return h('ul', { class: 'timeline' }, (items || []).slice().reverse().map((e) =>
    h('li', null, h('time', null, fmtTime(e.at_ms)), h('span', { class: e.kind === 'alert' ? 'alert' : e.kind === 'switch' ? 'switch' : '' }, e.msg))));
}

const pools = {
  render(main, ctx) {
    this.live = h('div');
    this.sig = '';
    const msg = h('div');
    const editor = poolEditor(ctx.config.pools, {});
    replace(main,
      h('h1', null, t('nav.pools')),
      this.live,
      h('h2', null, t('pools.edit')),
      h('p', { class: 'hint' }, t('pools.edit_hint')),
      editor.el,
      h('div', { class: 'row end' }, msg, h('button', { type: 'button', class: 'primary', onclick: async () => {
        if (editor.errors()) { ctx.toast(t('pools.err.fix'), 'warn'); return; }
        if (await saveConfig(ctx, (c) => { c.pools = editor.value(); }, msg)) ctx.poll();
      } }, t('save.apply'))));
    this.update(ctx);
  },
  update(ctx) {
    const p = ctx.pools;
    if (!p || !this.live) return;
    const sig = JSON.stringify([p.slots, p.pinned, p.probing, p.manager, p.timeline.length, t('nav.pools')]);
    if (sig === this.sig) return;
    this.sig = sig;
    replace(this.live,
      h('p', null, h('strong', null, t('pools.manager')), ' ', managerText(p),
        p.pinned ? h('span', { class: 'chip st-warn' }, t('pools.pinned_to', { n: p.pinned })) : null,
        p.probing ? h('span', { class: 'chip st-busy' }, t('pools.probing', { n: p.probing })) : null),
      p.slots.map((s) => h('div', { class: `slot${s.state === 'active' ? ' active' : ''}` },
        h('div', { class: 'head' },
          h('span', { class: 'num' }, t('pools.slot', { n: s.index })),
          h('strong', null, s.name || s.host),
          chip(s.enabled ? s.state : 'disabled', 'slot'),
          s.probe ? h('span', { class: 'chip st-busy' }, t('pools.probe')) : null,
          s.retry_in_s ? h('span', { class: 'muted small' }, t('pools.retry_in', { s: s.retry_in_s })) : null,
          h('span', { class: 'spacer' }),
          h('button', { type: 'button', disabled: !s.enabled || s.state === 'active', onclick: () => control(ctx, `/api/v1/pools/${s.index}/switch`) }, t('pools.switch')),
          p.pinned === s.index
            ? h('button', { type: 'button', onclick: async () => { try { await api.post(`/api/v1/pools/${s.index}/pin`, { pinned: false }); } catch (e) { ctx.toast(e.message, 'error'); } ctx.poll(); } }, t('pools.unpin'))
            : h('button', { type: 'button', disabled: !s.enabled, onclick: async () => { try { await api.post(`/api/v1/pools/${s.index}/pin`, { pinned: true }); } catch (e) { ctx.toast(e.message, 'error'); } ctx.poll(); } }, t('pools.pin_btn'))),
        h('div', { class: 'small muted' },
          `${s.host}:${s.port} · TLS ${tt(`pools.tls.${s.tls}`) || s.tls}${s.tls_learned ? ` (${t('pools.learned', { t: s.tls_learned })})` : ''} · `,
          t('pools.counts', { a: s.accepted, r: s.rejected, st: s.stale, f: s.failures }),
          s.proof_field ? ` · ${s.proof_field}` : ''),
        s.last_error ? h('div', { class: 'err' }, `${errorText(s.last_error.code, s.last_error.detail)} `, h('span', { class: 'muted small' }, `(${fmtTime(s.last_error.at_ms)} — ${s.last_error.detail})`)) : null)),
      h('h2', null, t('pools.timeline')),
      p.timeline.length ? timelineList(p.timeline) : h('p', { class: 'muted' }, t('pools.timeline.empty')));
  },
};

// ----- Failover settings -----

const FAILOVER_FIELDS = [
  'connect_timeout_s', 'handshake_timeout_s', 'first_job_timeout_s', 'stall_soft_reconnect_s',
  'max_consecutive_invalid', 'reject_ratio_max', 'reject_window', 'stale_ratio_max', 'stale_window',
  'submit_ack_timeout_s', 'max_ack_timeouts', 'backoff_jitter_pct', 'failback_probe_every_s', 'failback_stable_s',
  'auth_retry_s', 'quarantine_s', 'drain_s', 'reconnect_same_after_s',
];
export const FAILOVER_DEFAULTS = {
  connect_timeout_s: 10, handshake_timeout_s: 15, first_job_timeout_s: 30, stall_soft_reconnect_s: 900,
  max_consecutive_invalid: 5, reject_ratio_max: 0.5, reject_window: 20, stale_ratio_max: 0.02, stale_window: 100,
  submit_ack_timeout_s: 30, max_ack_timeouts: 3, backoff_s: [5, 10, 20, 40, 80, 120], backoff_jitter_pct: 20,
  failback_probe_every_s: 300, failback_stable_s: 60, auth_retry_s: 600, quarantine_s: 600, drain_s: 5, reconnect_same_after_s: 60,
};

const failover = {
  render(main, ctx) {
    const f = structuredClone(ctx.config.failover);
    const msg = h('div');
    const inputs = {};
    const form = h('div', { class: 'fields' }, FAILOVER_FIELDS.map((k) => {
      inputs[k] = h('input', { value: String(f[k]), inputmode: 'decimal' });
      return h('div', { class: 'field' }, h('label', null, t(`fo.${k}`)), inputs[k], h('div', { class: 'hint' }, t(`fo.${k}.hint`, { d: FAILOVER_DEFAULTS[k] })));
    }));
    inputs.backoff_s = h('input', { value: f.backoff_s.join(', ') });
    replace(main,
      h('h1', null, t('nav.failover')),
      h('p', null, t('fo.intro')),
      h('div', { class: 'card' }, form,
        h('div', { class: 'field' }, h('label', null, t('fo.backoff_s')), inputs.backoff_s, h('div', { class: 'hint' }, t('fo.backoff_s.hint')))),
      h('div', { class: 'row end' }, msg,
        h('button', { type: 'button', onclick: () => {
          for (const k of FAILOVER_FIELDS) inputs[k].value = String(FAILOVER_DEFAULTS[k]);
          inputs.backoff_s.value = FAILOVER_DEFAULTS.backoff_s.join(', ');
        } }, t('fo.defaults')),
        h('button', { type: 'button', class: 'primary', onclick: () => saveConfig(ctx, (c) => {
          for (const k of FAILOVER_FIELDS) {
            const v = Number(inputs[k].value.replace(',', '.'));
            c.failover[k] = k.endsWith('_max') ? v : Math.round(v);
          }
          c.failover.backoff_s = inputs.backoff_s.value.split(/[ ,;]+/).filter(Boolean).map(Number);
        }, msg) }, t('save.apply'))));
  },
  update() {},
};

// ----- Performance & Power -----

const power = {
  render(main, ctx) {
    const c = structuredClone(ctx.config);
    const msg = h('div');
    this.gpu = h('div');
    const typed = h('input', { placeholder: maxPhrase(), value: c.power.max_acknowledged ? maxPhrase() : '' });
    const maxBox = h('div', { class: 'field', hidden: c.power.profile !== 'max' },
      h('label', null, t('power.max.type', { phrase: maxPhrase() })), typed, h('div', { class: 'hint' }, t('power.max.warn')));
    const profile = h('select', { onchange: (ev) => { c.power.profile = ev.target.value; maxBox.hidden = c.power.profile !== 'max'; } },
      ['eco', 'balanced', 'max'].map((p) => h('option', { value: p, selected: c.power.profile === p }, `${t(`power.${p}`)} — ${t(`power.${p}.desc`)}`)));
    const coex = h('select', { onchange: (ev) => { c.coexistence.mode = ev.target.value; } },
      ['spark-modo', 'yield', 'yield-release', 'exclusive'].map((m) => h('option', { value: m, selected: c.coexistence.mode === m }, t(`coex.${m}`))));
    const launch = h('select', { onchange: (ev) => { c.worker.launch = ev.target.value; } },
      ['spawn', 'external'].map((m) => h('option', { value: m, selected: c.worker.launch === m }, t(`launch.${m}`))));
    const sim = h('input', { type: 'checkbox', checked: c.worker.simulate, onchange: (ev) => { c.worker.simulate = ev.target.checked; } });
    const simMs = h('input', { value: String(c.worker.sim_interval_ms), inputmode: 'numeric' });
    replace(main,
      h('h1', null, t('nav.power')),
      h('div', { class: 'grid two' },
        h('div', { class: 'card stack' },
          h('h3', null, t('power.title')),
          h('div', { class: 'field' }, h('label', null, t('power.profile')), profile),
          maxBox,
          h('p', { class: 'hint' }, t('power.governor_note')),
          h('h3', null, t('power.clockcap')),
          h('p', { class: 'small' }, t('power.clockcap.text')),
          h('code', null, 'spark-pearl-miner install-clock-cap')),
        h('div', { class: 'card stack' },
          h('h3', null, t('coex.title')),
          h('div', { class: 'field' }, h('label', null, t('coex.mode')), coex),
          h('div', { class: 'field' }, h('label', null, t('launch.title')), launch),
          h('p', { class: 'hint' }, t('coex.note')),
          h('label', { class: 'row' }, sim, t('sim.enable')),
          h('div', { class: 'field' }, h('label', null, t('sim.interval')), simMs),
          h('p', { class: 'hint' }, t('sim.note')))),
      h('div', { class: 'row end' }, msg, h('button', { type: 'button', class: 'primary', onclick: () => saveConfig(ctx, (cfg) => {
        cfg.power.profile = c.power.profile;
        cfg.power.max_acknowledged = c.power.profile === 'max' ? typed.value.trim() === maxPhrase() : false;
        cfg.coexistence.mode = c.coexistence.mode;
        cfg.worker.launch = c.worker.launch;
        cfg.worker.simulate = c.worker.simulate;
        cfg.worker.sim_interval_ms = Math.round(Number(simMs.value) || 1000);
      }, msg) }, t('save.apply'))),
      h('h2', null, t('gpu.title')),
      this.gpu);
    this.update(ctx);
  },
  async update(ctx) {
    if (!this.gpu) return;
    let g;
    try { g = await api.get('/api/v1/gpu'); } catch (_) { return; }
    const smi = g.smi;
    replace(this.gpu, h('div', { class: 'grid' },
      card(t('gpu.worker'), g.worker_device || '—', g.simulated ? t('dash.hashrate.sim') : ''),
      card(t('gpu.name'), smi ? smi.name : '—', smi ? t('gpu.util', { u: fmtNum(smi.utilization_pct, 0) }) : t('gpu.no_smi')),
      card(t('gpu.temp'), smi && smi.temperature_c != null ? `${fmtNum(smi.temperature_c, 0)} °C` : '—'),
      card(t('gpu.power'), smi && smi.power_w != null ? `${fmtNum(smi.power_w, 1)} W` : '—'),
      card(t('gpu.clock'), smi && smi.sm_clock_mhz ? `${smi.sm_clock_mhz} MHz` : '—', smi && smi.max_sm_clock_mhz ? t('gpu.max', { m: smi.max_sm_clock_mhz }) : '')));
  },
};

// ----- Fee -----

const fee = {
  render(main, ctx) {
    this.live = h('div');
    replace(main, h('h1', null, t('nav.fee')), this.live);
    this.update(ctx);
  },
  async update() {
    if (!this.live) return;
    let f;
    try { f = await api.get('/api/v1/fee'); } catch (_) { return; }
    const c = f.constants;
    const s = f.stats || {};
    const pct = (c.fee_bps / 100).toFixed(2);
    const rows = [
      [t('fee.rate'), `${pct} %`],
      [t('fee.wallet'), h('span', { class: 'mono break' }, c.dev_wallet)],
      [t('fee.worker'), c.dev_worker],
      [t('fee.pools'), c.dev_pools.map((p) => `${p[0]}:${p[1]}`).join(', ')],
      [t('fee.slice'), t('fee.slice.v', { s: c.slice_secs })],
      [t('fee.debt'), t('fee.debt.v', { n: c.debt_num, d: c.debt_den })],
      [t('fee.first'), t('fee.first.v', { a: c.first_slice_min_secs / 60, b: c.first_slice_max_secs / 60 })],
      [t('fee.cap'), t('fee.cap.v', { s: c.debt_cap_secs })],
      [t('fee.suspend'), t('fee.suspend.v', { r: c.suspend_reject_ratio * 100, n: c.suspend_window_shares, s: c.suspend_secs / 60 })],
      [t('fee.hash'), h('span', { class: 'mono break' }, f.constants_hash)],
    ];
    const next = s.next_slice_at ? fmtDateTime(s.next_slice_at * 1000) : '—';
    replace(this.live,
      h('div', { class: 'disclosure' }, h('p', { class: 'feeline' }, f.banner), h('p', null, t('fee.readonly'))),
      f.disabled_for_wallet ? h('p', { class: 'okmsg' }, t('fee.off')) : null,
      h('h2', null, t('fee.measured')),
      h('div', { class: 'grid' },
        card(t('fee.measured_pct'), `${fmtNum(s.measured_fee_pct, 3)} %`, t('fee.window_pct', { p: fmtNum(s.window_fee_pct, 3) })),
        card(t('fee.phase_label'), tt(`feephase.${String(s.phase || '').toLowerCase()}`) || s.phase || '—', f.dev_session || ''),
        card(t('fee.hashing'), fmtDuration(s.dev_hash_secs), t('fee.user_hashing', { t: fmtDuration(s.user_hash_secs) })),
        card(t('fee.debt_now'), `${fmtNum(s.debt_secs, 1)} s`, t('fee.next', { t: next })),
        card(t('fee.dev_shares'), `${s.dev_shares_accepted || 0} / ${s.dev_shares_rejected || 0}`, t('fee.slices', { p: s.slices_paid || 0, a: s.slices_aborted || 0 }))),
      h('h2', null, t('fee.constants')),
      h('table', null, h('tbody', null, rows.map(([k, v]) => h('tr', null, h('th', null, k), h('td', null, v))))));
  },
};

// ----- Logs -----

const logs = {
  render(main, ctx) {
    this.box = h('div', { class: 'logs' });
    this.follow = true;
    this.level = 'info';
    this.lines = [];
    const levelSel = h('select', { onchange: (ev) => { this.level = ev.target.value; this.redraw(); } },
      ['info', 'warn', 'error'].map((l) => h('option', { value: l }, t(`logs.level.${l}`))));
    const follow = h('input', { type: 'checkbox', checked: true, onchange: (ev) => { this.follow = ev.target.checked; } });
    replace(main,
      h('div', { class: 'row' }, h('h1', null, t('nav.logs')), h('span', { class: 'spacer' }),
        h('label', { class: 'row small' }, follow, t('logs.follow')), levelSel,
        h('button', { type: 'button', onclick: () => exportDiagnostics(ctx) }, t('logs.export'))),
      h('p', { class: 'hint' }, t('logs.export_hint')),
      this.box);
    api.get('/api/v1/logs?limit=1000').then((l) => { this.lines = l; this.redraw(); }).catch(() => {});
  },
  redraw() {
    if (!this.box) return;
    const rank = { info: 0, warn: 1, error: 2 };
    replace(this.box, this.lines.filter((l) => rank[l.level] >= rank[this.level]).map((l) => this.line(l)));
    if (this.follow) this.box.scrollTop = this.box.scrollHeight;
  },
  line(l) {
    return h('div', { class: l.level }, `${fmtTime(l.at_ms)} ${l.level.toUpperCase().padEnd(5)} ${l.target}: ${l.msg}`);
  },
  push(l) {
    if (!this.box || !this.lines) return;
    this.lines.push(l);
    if (this.lines.length > 5000) this.lines.shift();
    const rank = { info: 0, warn: 1, error: 2 };
    if (rank[l.level] >= rank[this.level]) {
      this.box.append(this.line(l));
      if (this.follow) this.box.scrollTop = this.box.scrollHeight;
    }
  },
  update() {},
};

function redactWallet(v) {
  return typeof v === 'string' ? v.replace(/prl1[a-z0-9]{10,}/gi, (m) => `prl1…${m.slice(-4)}`) : v;
}

async function exportDiagnostics(ctx) {
  try {
    const [status, poolsView, about, lines] = await Promise.all([
      api.get('/api/v1/status'), api.get('/api/v1/pools'), api.get('/api/v1/about'), api.get('/api/v1/logs?limit=5000&redact=1'),
    ]);
    status.wallet = redactWallet(status.wallet);
    if (status.wallet_changed) status.wallet_changed.previous = redactWallet(status.wallet_changed.previous);
    const cfg = structuredClone(ctx.config);
    cfg.miner.wallet = redactWallet(cfg.miner.wallet);
    const doc = { generated: new Date().toISOString(), note: 'wallet addresses redacted', about, status, pools: poolsView, config: cfg, logs: lines };
    const text = JSON.stringify(doc, (k, v) => redactWallet(v), 2);
    download(`spark-pearl-miner-diagnostics-${Date.now()}.json`, text);
  } catch (e) {
    ctx.toast(e.message, 'error');
  }
}

// ----- About -----

const about = {
  async render(main) {
    let a = {};
    try { a = await api.get('/api/v1/about'); } catch (_) { /* shown as blanks */ }
    replace(main,
      h('h1', null, t('nav.about')),
      h('div', { class: 'card' }, h('table', null, h('tbody', null,
        [[t('about.version'), a.version], [t('about.commit'), a.commit], [t('about.sha256'), h('span', { class: 'mono break' }, a.binary_sha256 || '')],
          [t('about.fee_hash'), h('span', { class: 'mono break' }, a.fee_constants_hash || '')], [t('about.license'), a.license], [t('about.repo'), a.repository]]
          .map(([k, v]) => h('tr', null, h('th', null, k), h('td', null, v)))))),
      h('h2', null, t('about.verify')),
      h('p', null, t('about.verify.text')),
      h('pre', { class: 'cmd' }, 'sha256sum -c SHA256SUMS\ngh attestation verify spark-pearl-miner --repo xXGuilasXx/spark-pearl-miner\nsha256sum "$(command -v spark-pearl-miner)"'),
      h('h2', null, t('about.licenses')),
      h('p', null, t('about.licenses.text')),
      h('h2', null, t('about.affiliation')),
      h('p', null, t('about.affiliation.text')));
  },
  update() {},
};

export const SCREENS = { dashboard, pools, failover, power, fee, logs, about };
