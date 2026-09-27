// The dashboard: one sentence that says whether the miner works (the hero), one button
// (Start / Stop / Resume / Retry), four cards (rate, shares, power, this Spark), the one-line
// failover status and the alerts. Live data: the 1 Hz `stats` SSE and the 2 s pools poll.

import { h, replace, keyed, fmtTime, fmtDuration, fmtNum, fmtRate, abbrev, copyText, confirmBox, download } from './dom.js';
import { t, tt } from './i18n.js';
import * as api from './api.js';
import { errorText } from './poolrows.js';
import { helpLink } from './wizard.js';

const STATE_CLASS = {
  mining: 'st-ok', starting: 'st-busy', failing_over: 'st-warn', paused: 'st-warn', setup_required: 'st-warn',
  stopped: '', all_down: 'st-bad', offline: 'st-bad',
  hashing: 'st-ok', ready: 'st-ok', absent: '', backoff: 'st-warn', waiting_external: 'st-busy', faulted: 'st-bad', unavailable: 'st-bad',
  running: 'st-ok', idle: '', off: '', no_telemetry: 'st-warn', tripped: 'st-warn', fault: 'st-bad',
};

export function chip(code, prefix = 'state', text) {
  return h('span', { class: `chip ${STATE_CLASS[code] || ''}` }, text || tt(`${prefix}.${code}`) || code);
}

const MEM_STATES = ['refused', 'exit_low_memory', 'exit_pressure', 'unreadable'];

/** "Is it working?": the first matching rule wins. tone: ok | idle | warn | bad | info. */
export function heroOf(s) {
  const p = s.power || {};
  const w = s.worker || {};
  const cx = s.coexist || {};
  const mem = cx.memory || {};
  if (s.setup_required) return { text: t('hero.setup'), tone: 'warn' };
  if (!s.running) return { text: t('hero.stopped'), tone: 'idle' };
  if (p.trip) {
    const reason = tt(`trip.${p.trip.code}`) || p.trip.code;
    return p.trip.resume_in_s != null
      ? { text: t('hero.trip', { reason, s: p.trip.resume_in_s }), tone: 'warn' }
      : { text: t('hero.trip_hold', { reason }), tone: 'warn' };
  }
  if (w.state === 'faulted' || s.pause_reason === 'hardware_fault') return { text: t('hero.faulted', { msg: w.last_fault || '' }), tone: 'bad' };
  if (s.pause_reason === 'power_fault') return { text: t('pause.power_fault'), tone: 'bad' };
  if (MEM_STATES.includes(mem.state)) {
    return { text: t(`mem.${mem.state}`, { avail: fmtNum(mem.available_gib, 1), psi: fmtNum(mem.psi_some_avg10, 1) }), tone: 'warn' };
  }
  if (s.paused && s.pause_reason) return { text: tt(`pause.${s.pause_reason}`) || s.pause_reason, tone: 'warn' };
  if (cx.gate === 'pause' || cx.gate === 'release') return { text: t('hero.yield'), tone: 'info' };
  if (['starting', 'backoff', 'waiting_external'].includes(w.state)) return { text: t(`worker.${w.state}`), tone: 'info' };
  if (['all_down', 'failing_over', 'starting'].includes(s.state)) {
    const text = tt(`mgr.${s.manager_code}`, { n: s.manager_to, from: s.manager_from, to: s.manager_to }) || s.manager;
    return { text, tone: s.state === 'all_down' ? 'warn' : 'info' };
  }
  if (s.mining_target === 'dev') return { text: t('hero.dev'), tone: 'ok', fee: true };
  return { text: t('hero.mining'), tone: 'ok' };
}

/** The one button: what it says and which endpoint it calls. */
function controlOf(s) {
  if (!s.running) return { key: 'ctl.start', op: 'start', action: 'ctl.action.start', cls: 'primary' };
  if (s.pause_reason === 'hardware_fault' || s.pause_reason === 'power_fault') return { key: 'ctl.retry', op: 'start', action: 'ctl.action.start', cls: 'primary' };
  if (s.paused && s.pause_reason === 'user') return { key: 'ctl.resume', op: 'resume', action: 'ctl.action.resume', cls: 'primary' };
  return { key: 'ctl.stop', op: 'stop', action: 'ctl.action.stop', cls: 'danger', confirm: true };
}

export async function stopConfirm() {
  return confirmBox(t('ctl.confirm_stop'), t('ctl.stop'), t('common.cancel'), { danger: true });
}

async function control(ctx, c, btn) {
  if (c.confirm && !(await stopConfirm())) return;
  btn.disabled = true;
  btn.classList.add('busy');
  try {
    await api.post(`/api/v1/mining/${c.op}`);
    ctx.toast(t(c.op === 'stop' ? 'toast.stopped' : 'toast.started'), 'ok');
  } catch (e) {
    ctx.toast(t('ctl.err', { action: t(c.action), message: e.message }), 'error');
  }
  btn.disabled = false;
  btn.classList.remove('busy');
  ctx.poll();
}

function level(v, warn, bad) {
  if (v == null) return '';
  if (v >= bad) return 'lv-bad';
  if (v >= warn) return 'lv-warn';
  return '';
}

function agoText(ms) {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 120) return t('fo1.ago_s', { n: s });
  if (s < 7200) return t('fo1.ago_m', { n: Math.round(s / 60) });
  return t('fo1.ago_h', { n: Math.round(s / 3600) });
}

/** A backup is "ready" when it is eligible and not failing: idle (not contacted yet), a healthy
 * standby session, or the old pool still draining after a switch. A slot being retried or in
 * backoff is not. */
const READY = ['idle', 'standby', 'draining'];

/** The failover status as one line: { tone, text }. */
export function failoverLine(s, p, cfg) {
  if (!p || !p.slots) return { tone: 'idle', text: t('fo1.none') };
  const enabled = p.slots.filter((x) => x.enabled);
  if (s.state === 'all_down' || p.manager_code === 'all_down') {
    const waits = enabled.map((x) => x.retry_in_s).filter((x) => x != null);
    return { tone: 'bad', text: waits.length ? t('fo1.all_down', { s: Math.min(...waits) }) : t('fo1.all_down_now') };
  }
  const active = p.slots.find((x) => x.index === s.active_pool);
  if (!active) return { tone: 'idle', text: t('fo1.none') };
  const backups = enabled.filter((x) => x.index !== active.index);
  const ready = backups.filter((x) => READY.includes(x.state)).length;
  let text = t('fo1.normal', { n: active.index, name: active.name || active.host, host: `${active.host}:${active.port}`, ready, total: backups.length });
  const first = enabled.length ? Math.min(...enabled.map((x) => x.index)) : 1;
  const onBackup = active.index > first;
  if (onBackup) {
    const switches = (p.timeline || []).filter((e) => e.kind === 'switch');
    const last = switches[switches.length - 1];
    const prev = switches[switches.length - 2];
    const from = prev && prev.slot && prev.slot !== active.index ? prev.slot : first;
    text += t('fo1.switched', { from, ago: last ? agoText(last.at_ms) : '' });
  }
  if (p.pinned) text += t('fo1.pinned');
  if (p.probing) text += t('fo1.probing', { n: p.probing, s: cfg && cfg.failover ? cfg.failover.failback_stable_s : 60 });
  for (const x of enabled) {
    if (x.index !== active.index && x.state === 'backoff' && x.last_error) {
      text += t('fo1.backoff', { k: x.index, err: errorText(x.last_error.code).replace(/\.$/, ''), s: x.retry_in_s ?? 0 });
    }
  }
  return { tone: onBackup ? 'warn' : 'ok', text };
}

function redactWallet(v) {
  return typeof v === 'string' ? v.replace(/prl1[a-z0-9]{10,}/gi, (m) => `prl1…${m.slice(-4)}`) : v;
}

/** The redacted diagnostics file (about, status, pools, config and the last 5000 log lines). */
export async function exportDiagnostics(ctx) {
  try {
    const [status, poolsView, about, config, lines] = await Promise.all([
      api.get('/api/v1/status'), api.get('/api/v1/pools'), api.get('/api/v1/about'), api.get('/api/v1/config'), api.get('/api/v1/logs?limit=5000&redact=1'),
    ]);
    const doc = { generated: new Date().toISOString(), note: 'wallet addresses redacted', about, status, pools: poolsView, config, logs: lines };
    download(`spark-pearl-miner-diagnostics-${Date.now()}.json`, JSON.stringify(doc, (k, v) => redactWallet(v), 2));
    ctx.toast(t('toast.diag_exported'), 'ok');
  } catch (e) {
    ctx.toast(e.message, 'error');
  }
}

export async function copyAndToast(ctx, text) {
  ctx.toast(t((await copyText(text)) ? 'toast.copied' : 'toast.copy_failed'), 'ok');
}

/** A command line with a Copy button. */
export function cmdBox(ctx, cmd) {
  return h('div', { class: 'cmdline' }, h('code', null, cmd), h('button', { type: 'button', class: 'small-btn', onclick: () => copyAndToast(ctx, cmd) }, t('common.copy')));
}

export const CLOCKCAP_CMD = 'sudo ~/.local/share/spark-pearl-miner/install-clockcap.sh --apply';

/** Banners above the hero, plain-worded. */
export function drawBanners(el, ctx) {
  const s = ctx.status;
  if (!s || s.setup_required) { keyed(el, '', () => null); return; }
  const p = s.power || {};
  const cap = p.clock_cap || {};
  const sig = JSON.stringify([!!s.wallet_changed, s.wallet, s.worker && s.worker.simulated, s.worker && s.worker.state === 'unavailable',
    cap.status === 'uncapped', cap.cap_mhz, p.fault && p.fault.signature, t('banner.sim')]);
  keyed(el, sig, () => [
    s.wallet_changed ? h('div', { class: 'banner warn' },
      h('span', null, t('banner.wallet_changed', { now: abbrev(s.wallet) })),
      h('button', { type: 'button', onclick: async () => {
        try { await api.post('/api/v1/wallet/ack'); ctx.toast(t('toast.wallet_acked'), 'ok'); } catch (e) { ctx.toast(e.message, 'error'); }
      } }, t('banner.wallet_yes')),
      h('button', { type: 'button', class: 'danger', onclick: async (ev) => control(ctx, { op: 'stop', action: 'ctl.action.stop' }, ev.target) }, t('banner.wallet_stop'))) : null,
    p.fault ? h('div', { class: 'banner bad' }, t('banner.fault', { fault: tt(`fault.${p.fault.signature}`) || p.fault.signature }),
      helpLink(ctx, 'power-faults', '?', 'help round')) : null,
    s.worker && s.worker.state === 'unavailable' ? h('div', { class: 'banner bad' }, t('banner.no_cuda'), helpLink(ctx, 'troubleshooting', '?', 'help round')) : null,
    cap.status === 'uncapped' ? h('div', { class: 'banner warn' }, h('span', null, t('banner.uncapped', { mhz: cap.cap_mhz || 2000 })), cmdBox(ctx, CLOCKCAP_CMD)) : null,
    s.worker && s.worker.simulated ? h('div', { class: 'banner info' }, t('banner.sim')) : null,
  ]);
}

let timelineOpen = false;
let alertsOpen = false;

export function openTimeline() { timelineOpen = true; }
export function openAlerts() { alertsOpen = true; }

const el = {};

export function renderDashboard(main, ctx) {
  el.dot = h('span', { class: 'dot' });
  el.hero = h('span', { class: 'herotext' });
  el.badge = h('span', { class: 'badge', hidden: true }, t('hero.fee_badge'));
  el.ctl = h('div', { class: 'ctl' });
  el.cards = { rate: h('div', { class: 'card' }), shares: h('div', { class: 'card' }), power: h('div', { class: 'card' }), spark: h('div', { class: 'card' }) };
  el.foDot = h('span', { class: 'dot small' });
  el.foText = h('span', { class: 'fotext' });
  el.foToggle = h('button', { type: 'button', class: 'link small', 'aria-expanded': String(timelineOpen), onclick: () => { timelineOpen = !timelineOpen; updateDashboard(ctx, true); } });
  el.foList = h('div');
  el.alerts = h('div', { class: 'alerts' });
  el.alertsToggle = h('button', { type: 'button', class: 'link', onclick: () => { alertsOpen = !alertsOpen; updateDashboard(ctx, true); } });
  el.alertsList = h('div');
  replace(main,
    h('section', { class: 'hero' }, h('div', { class: 'heroline' }, el.dot, el.hero, el.badge), el.ctl),
    h('div', { class: 'grid' }, el.cards.rate, el.cards.shares, el.cards.power, el.cards.spark),
    h('section', { class: 'foline' },
      h('div', { class: 'forow', onclick: (ev) => { if (ev.target !== el.foToggle) el.foToggle.click(); } }, el.foDot, el.foText, el.foToggle),
      el.foList),
    h('section', { class: 'alerts', hidden: true }, el.alertsToggle, el.alertsList));
  el.alertsBox = main.querySelector('section.alerts');
  updateDashboard(ctx, true);
}

function row(label, value, cls) {
  return h('div', { class: `kv ${cls || ''}` }, h('span', { class: 'k' }, label), h('span', { class: 'v' }, value));
}

export function updateDashboard(ctx, force) {
  const s = ctx.status;
  if (!s || !el.hero || !el.hero.isConnected) return;
  if (force) for (const x of Object.values(el)) if (x && x.dataset) delete x.dataset.sig;

  // Hero + control.
  const hero = heroOf(s);
  el.dot.className = `dot ${hero.tone}`;
  el.hero.textContent = hero.text;
  el.badge.hidden = !hero.fee;
  el.badge.textContent = t('hero.fee_badge');
  const c = controlOf(s);
  keyed(el.ctl, `${c.key}|${t(c.key)}`, () => {
    const btn = h('button', { type: 'button', class: `${c.cls} big`, onclick: () => control(ctx, c, btn) }, t(c.key));
    return btn;
  });

  // Card 1: earning rate.
  const sim = s.worker && s.worker.simulated;
  replace(el.cards.rate,
    h('div', { class: 'label' }, t('rate.title')),
    h('div', { class: 'value' }, fmtRate(s.hashrate_tmacs_60s ?? s.hashrate_tmacs)),
    h('div', { class: 'sub' }, t('rate.sub')),
    h('div', { class: 'sub' }, sim ? t('sim.rate') : t('rate.steps')),
    h('div', { class: 'sub muted small' }, t('rate.ref')),
    h('div', { class: 'cardfoot' }, helpLink(ctx, 'balance', t('rate.balance'), 'small')));

  // Card 2: shares.
  const sh = s.shares || {};
  const acc = sh.accepted || 0, rej = sh.rejected || 0;
  const rejCls = rej > 0 ? (acc >= 10 && rej / acc > 0.1 ? 'lv-bad' : 'lv-warn') : '';
  el.cards.shares.title = t('shares.discarded', { n: sh.discarded || 0 });
  replace(el.cards.shares,
    h('div', { class: 'label' }, t('shares.title')),
    h('div', { class: 'value' }, h('span', { class: 'lv-ok' }, String(acc)), h('span', { class: 'muted' }, ' / '), h('span', { class: rejCls }, String(rej))),
    h('div', { class: 'sub' }, `${t('shares.accepted')} / ${t('shares.rejected')}`),
    h('div', { class: 'sub' }, t('shares.stale', { n: sh.stale || 0 })),
    h('div', { class: 'sub muted small' }, t('shares.fee', { n: sh.dev_accepted || 0 })));

  // Card 3: power and temperature.
  const p = s.power || {};
  const tel = p.telemetry;
  const stale = !tel || (s.at_ms && tel.at_ms && s.at_ms - tel.at_ms > 3000);
  const blind = p.state === 'no_telemetry' || p.source === 'none' || stale;
  const cap = p.clock_cap || {};
  const capChip = cap.status === 'capped' ? chip('running', 'x', t('pwr.capped'))
    : cap.status === 'uncapped' ? chip('no_telemetry', 'x', t('pwr.uncapped')) : chip('off', 'x', t('pwr.cap_unknown'));
  let foot = t('pwr.footer', { profile: tt(`pwr.profile.${p.profile}`) || p.profile || '—', w: fmtNum(p.hard_stop_w, 0) });
  if (p.trips_total > 0) foot += t('pwr.trips', { n: p.trips_total });
  if (p.stepped_down) foot += t('pwr.stepped');
  replace(el.cards.power,
    h('div', { class: 'row' }, h('div', { class: 'label' }, t('pwr.title')), h('span', { class: 'spacer' }), p.state ? chip(p.state, 'pwrstate') : null),
    blind ? h('p', { class: 'lv-warn' }, t('pwr.none')) : [
      row(t('pwr.gpu_power'), `${fmtNum(tel.power_w, 1)} W`, level(tel.power_w, p.effective_target_w ?? p.target_w, p.hard_stop_w)),
      row(t('pwr.gpu_temp'), `${fmtNum(tel.temp_gpu_c, 0)} °C`, level(tel.temp_gpu_c, 78, 83)),
      row(t('pwr.board'), tel.temp_acpitz_c != null ? `${fmtNum(tel.temp_acpitz_c, 1)} °C` : '—', level(tel.temp_acpitz_c, 90, 95)),
      row(t('pwr.clock'), t('pwr.clock_v', { mhz: tel.sm_clock_mhz, cap: cap.cap_mhz || '—' })),
      h('div', { class: 'capline' }, capChip),
    ],
    h('div', { class: 'cardfoot muted small' }, foot));

  // Card 4: this Spark.
  const w = s.worker || {};
  keyed(el.cards.spark, JSON.stringify([s.wallet, s.worker_name, w.state, t('spark.title')]), () => [
    h('div', { class: 'label' }, t('spark.title')),
    el.times = h('div', { class: 'sub' }),
    h('div', { class: 'sub' }, t('spark.worker', { w: s.worker_name || '—' })),
    h('div', { class: 'sub walletline' }, `${t('spark.wallet')} `, h('span', { class: 'mono' }, abbrev(s.wallet) || '—'),
      s.wallet ? h('button', { type: 'button', class: 'small-btn', onclick: () => copyAndToast(ctx, s.wallet) }, t('common.copy')) : null),
    h('div', { class: 'sub' }, chip(w.state || 'absent', 'worker')),
  ]);
  el.times.textContent = t('spark.times', { m: fmtDuration(s.mining_s), u: fmtDuration(s.uptime_s) });

  // Failover one-liner and its last events.
  const fo = failoverLine(s, ctx.pools, ctx.config);
  el.foDot.className = `dot small ${fo.tone}`;
  el.foText.textContent = fo.text;
  el.foToggle.textContent = `${timelineOpen ? '▾' : '▸'} ${t('fo1.events')}`;
  el.foToggle.setAttribute('aria-expanded', String(timelineOpen));
  const tl = ((ctx.pools && ctx.pools.timeline) || []).slice(-5).reverse();
  keyed(el.foList, JSON.stringify([timelineOpen, tl.map((e) => e.at_ms), t('fo1.no_events')]), () => (timelineOpen
    ? (tl.length ? h('ul', { class: 'timeline' }, tl.map((e) => h('li', null, h('time', null, fmtTime(e.at_ms)), h('span', { class: e.kind === 'switch' ? 'switch' : e.kind === 'alert' ? 'alert' : '' }, e.msg))))
      : h('p', { class: 'muted small' }, t('fo1.no_events')))
    : null));

  // Alerts.
  const alerts = (s.alerts || []).slice(0, 10); // newest first
  el.alertsBox.hidden = alerts.length === 0;
  el.alertsToggle.textContent = `${alertsOpen ? '▾' : '▸'} ${t('alerts.title', { n: alerts.length })}`;
  keyed(el.alertsList, JSON.stringify([alertsOpen, alerts.map((a) => a.at_ms)]), () => (alertsOpen
    ? h('ul', { class: 'timeline' }, alerts.map((a) => h('li', null, h('time', null, fmtTime(a.at_ms)),
      h('span', { class: a.level === 'error' ? 'lv-bad' : 'lv-warn' }, a.level === 'error' ? '✖ ' : '⚠ ', a.msg))))
    : null));
}
