// Spark Pearl Miner web GUI: session, the single route (#/dashboard), the setup wizard, live
// updates (SSE + a 2 s pools poll), the header, the footer and the toasts.

import { h, replace, fmtTime } from './dom.js';
import { t, setLang, detect, onLang, getLang, LANGS } from './i18n.js';
import * as api from './api.js';
import { renderWizard, manualUrl } from './wizard.js';
import { renderDashboard, updateDashboard, drawBanners, chip, exportDiagnostics, copyAndToast, stopConfirm, openTimeline, openAlerts } from './dashboard.js';
import { openSettings } from './settings.js';

const main = document.getElementById('main');
const pill = document.getElementById('state-pill');
const banners = document.getElementById('banners');
const toasts = document.getElementById('toasts');
const footer = document.getElementById('footer');
const overlay = document.getElementById('overlay');
const gear = document.getElementById('gear');
const help = document.getElementById('help');

// Display flags (client side only, never saved): ?lang=en|pt-BR picks the language of this page
// load; ?shot=… opens a dialog or expands a section for the manual's screenshots, and only on a
// simulated miner (worker.simulate = true), so it is inert on a real one.
const query = new URLSearchParams(location.search);
const queryLang = LANGS.includes(query.get('lang')) ? query.get('lang') : null;
const queryShot = query.get('shot');

const ctx = {
  status: null, pools: null, config: null, about: null, fee: null,
  toast, go, refresh, poll,
  /** The ?shot flag, honoured only while the worker is simulated. */
  shot: () => (queryShot && ctx.status && ctx.status.worker && ctx.status.worker.simulated ? queryShot : null),
};
let view = null; // 'wizard' | 'dashboard' | 'login'
let events = null;
let started = false;
let shotDone = false;
let downSince = null;
let reconnectTimer = null;

function toast(msg, kind = 'info', cmd) {
  const el = h('div', { class: `toast ${kind}`, role: kind === 'error' ? 'alert' : 'status' },
    h('div', null, msg),
    cmd ? h('div', { class: 'cmdline' }, h('code', null, cmd), h('button', { type: 'button', class: 'small-btn', onclick: () => copyAndToast(ctx, cmd) }, t('common.copy'))) : null);
  toasts.append(el);
  setTimeout(() => el.remove(), kind === 'error' || cmd ? 12000 : 5000);
}

function go(r) {
  if (r === 'dashboard' && location.hash !== '#/dashboard') history.replaceState(null, '', `${location.pathname}${location.search}#/dashboard`);
  render();
}

async function refresh() {
  const [status, pools, config] = await Promise.all([api.get('/api/v1/status'), api.get('/api/v1/pools'), api.get('/api/v1/config')]);
  Object.assign(ctx, { status, pools, config });
  if (!ctx.about) { try { ctx.about = await api.get('/api/v1/about'); } catch (_) { /* footer shows less */ } }
  if (!ctx.fee) { try { ctx.fee = await api.get('/api/v1/fee'); } catch (_) { /* no fee line */ } }
}

async function poll() {
  try {
    ctx.pools = await api.get('/api/v1/pools');
    if (view === 'dashboard') updateDashboard(ctx);
  } catch (_) { /* the SSE shows the connection state */ }
}

function drawHeader() {
  document.getElementById('lang-en').classList.toggle('active', getLang() === 'en');
  document.getElementById('lang-pt').classList.toggle('active', getLang() === 'pt-BR');
  document.querySelector('.lang').setAttribute('aria-label', t('hdr.lang'));
  const dash = view === 'dashboard';
  gear.hidden = !dash;
  gear.title = t('hdr.settings');
  gear.setAttribute('aria-label', t('hdr.settings'));
  const url = dash ? manualUrl(ctx, '') : null;
  help.hidden = !url;
  if (url) help.href = url;
  help.title = t('hdr.manual');
  help.setAttribute('aria-label', t('hdr.manual'));
  drawPill();
}

function drawPill() {
  const s = ctx.status;
  if (!s || view !== 'dashboard') { pill.hidden = true; return; }
  pill.hidden = false;
  const running = s.state === 'mining' && s.active_pool;
  replace(pill, chip(downSince ? 'offline' : s.state), running && !downSince ? t('hdr.pool', { n: s.active_pool }) : '');
}

function drawFooter() {
  if (view !== 'dashboard') { footer.hidden = true; return; }
  footer.hidden = false;
  const a = ctx.about || {};
  const path = a.config_path || '~/.config/spark-pearl-miner/config.toml';
  const manual = manualUrl(ctx, '');
  replace(footer,
    ctx.fee ? h('p', { class: 'feeline small' }, ctx.fee.banner) : null,
    h('p', { class: 'footlinks small' },
      a.version ? h('span', null, t('foot.version', { v: a.version, c: a.commit || '?' })) : null,
      manual ? h('a', { href: manual, target: '_blank', rel: 'noopener noreferrer' }, t('foot.manual')) : null,
      h('button', { type: 'button', class: 'link', onclick: () => exportDiagnostics(ctx) }, t('foot.export')),
      h('span', null, t('foot.config', { path }), ' ',
        h('button', { type: 'button', class: 'link', onclick: () => copyAndToast(ctx, path) }, t('foot.copy_path'))),
      a.repository ? h('a', { href: `${a.repository}/blob/main/LICENSE`, target: '_blank', rel: 'noopener noreferrer' }, t('foot.license')) : null));
}

function render() {
  if (!started) return;
  if (location.hash !== '#/dashboard' && !/token=/.test(location.hash)) {
    history.replaceState(null, '', `${location.pathname}${location.search}#/dashboard`);
  }
  view = ctx.status && ctx.status.setup_required ? 'wizard' : 'dashboard';
  drawHeader();
  drawFooter();
  if (view === 'wizard') {
    const sim = ctx.status && ctx.status.worker && ctx.status.worker.simulated;
    replace(banners, sim ? h('div', { class: 'banner info' }, t('banner.sim')) : null);
    renderWizard(main, ctx);
  } else {
    drawBanners(banners, ctx);
    renderDashboard(main, ctx);
  }
  applyShot();
}

/** Open what a ?shot flag asks for, once, after the first render with data. */
function applyShot() {
  const shot = ctx.shot();
  if (!shot || shotDone || view !== 'dashboard' || !ctx.pools) return;
  shotDone = true;
  if (shot === 'settings' || shot === 'settings-error' || shot === 'settings-wallet-confirm') openSettings(ctx, { shot });
  else if (shot === 'stop-confirm') stopConfirm();
  else if (shot === 'timeline') { openTimeline(); updateDashboard(ctx, true); }
  else if (shot === 'alerts') { openAlerts(); updateDashboard(ctx, true); }
}

function connectionDown() {
  if (downSince) return;
  downSince = Date.now();
  toast(t('conn.lost'), 'warn');
  drawPill();
  setTimeout(() => {
    if (downSince && Date.now() - downSince >= 10000) {
      overlay.hidden = false;
      replace(overlay, h('div', { class: 'card stack' }, h('h2', null, t('conn.down_title')), h('p', null, t('conn.down')),
        h('pre', { class: 'cmd' }, 'spark-pearl-miner status')));
    }
  }, 10500);
}

function connectionUp() {
  if (!downSince) return;
  downSince = null;
  overlay.hidden = true;
  drawPill();
}

function onStats(status) {
  connectionUp();
  const was = ctx.status && ctx.status.setup_required;
  ctx.status = status;
  if (was !== ctx.status.setup_required) { render(); return; }
  drawPill();
  if (view === 'dashboard') {
    drawBanners(banners, ctx);
    updateDashboard(ctx);
    applyShot();
  }
}

/** Screenshot mode (?shot= on a simulated miner): poll instead of holding an event stream open,
 * so a headless browser sees an idle network and takes the picture. */
function pollStats() {
  setInterval(async () => {
    try { onStats(await api.get('/api/v1/status')); } catch (_) { connectionDown(); }
  }, 1000);
}

function connectEvents() {
  if (events) events.close();
  events = new EventSource('/api/v1/events');
  events.addEventListener('stats', (e) => onStats(JSON.parse(e.data)));
  events.addEventListener('alert', (e) => {
    const a = JSON.parse(e.data);
    toast(a.msg, a.level === 'error' ? 'error' : 'warn');
  });
  for (const name of ['fsm', 'timeline']) events.addEventListener(name, () => poll());
  events.addEventListener('share', (e) => {
    const s = JSON.parse(e.data);
    if (!s.accepted) toast(t('share.rejected', { n: s.pool || 'dev', reason: s.reason || '' }), 'warn');
  });
  events.addEventListener('config', async () => {
    try { ctx.config = await api.get('/api/v1/config'); } catch (_) { /* ignore */ }
  });
  events.onerror = () => {
    connectionDown();
    // A closed stream (the daemon restarted and the session is gone) is not retried by the
    // browser: resume the session and reconnect ourselves.
    if (events.readyState === EventSource.CLOSED && !reconnectTimer) {
      reconnectTimer = setTimeout(async () => {
        reconnectTimer = null;
        if (await api.resume()) connectEvents();
        else events.onerror();
      }, 3000);
    }
  };
}

function showLogin(message) {
  started = false;
  view = 'login';
  drawHeader();
  footer.hidden = true;
  replace(banners);
  if (events) { events.close(); events = null; }
  const input = h('input', { type: 'password', autocomplete: 'off', spellcheck: 'false', class: 'mono', placeholder: '0123…cdef', 'aria-label': t('login.token') });
  const err = h('div');
  const submit = async () => {
    try {
      await api.login(input.value.trim());
      await startApp();
    } catch (e) {
      replace(err, h('div', { class: 'err' }, e && e.status ? t('login.err.bad_token') : t('login.err.network')));
    }
  };
  input.addEventListener('keydown', (ev) => { if (ev.key === 'Enter') submit(); });
  replace(main, h('div', { class: 'card login stack' },
    h('h1', null, t('login.title')),
    message ? h('p', { class: 'err' }, message) : null,
    h('p', null, t('login.text')),
    h('div', { class: 'field' }, h('label', null, t('login.token')), input, h('div', { class: 'hint' }, t('login.token_hint'))),
    err,
    h('div', { class: 'row end' }, h('button', { type: 'button', class: 'primary', onclick: submit }, t('login.submit'))),
    h('p', { class: 'hint' }, t('login.cmd_hint')),
    h('pre', { class: 'cmd' }, 'spark-pearl-miner gui\nspark-pearl-miner gui --print-url')));
}

async function startApp() {
  // On this machine the daemon may open a new session by itself (same user account): try that
  // before asking for the token.
  api.setUnauthorizedHandler(async () => {
    if (!(await api.resume())) showLogin(t('login.expired'));
  });
  await refresh();
  let saved = null;
  try { saved = localStorage.getItem('spm.lang'); } catch (_) { /* ignore */ }
  if (!queryLang && !saved && ctx.config.gui && ctx.config.gui.language !== 'auto') await setLang(ctx.config.gui.language, false);
  started = true;
  if (ctx.shot()) pollStats();
  else connectEvents();
  render();
}

/** Log in with a token passed in the URL fragment (never sent to the server), then drop it
 * from the address bar. */
async function fragmentLogin() {
  const m = location.hash.match(/token=([0-9a-fA-F]{64})/);
  if (!m) return false;
  history.replaceState(null, '', `${location.pathname}${location.search}#/dashboard`);
  try {
    return await api.login(m[1]);
  } catch (_) {
    toast(t('login.err.bad_token'), 'error');
    return false;
  }
}

async function boot() {
  await setLang(queryLang || detect(), false);
  for (const b of document.querySelectorAll('.lang button')) {
    b.addEventListener('click', () => setLang(b.dataset.lang));
  }
  gear.addEventListener('click', () => openSettings(ctx));
  onLang(() => { if (started) render(); else if (view === 'login') showLogin(); else drawHeader(); });
  drawHeader();
  window.addEventListener('hashchange', async () => {
    if (await fragmentLogin()) { await startApp(); return; }
    if (started && location.hash !== '#/dashboard') render();
  });
  setInterval(() => { if (started && view === 'dashboard' && !downSince) poll(); }, 2000);

  let ok = await fragmentLogin();
  if (!ok) ok = await api.resume();
  if (!ok) { showLogin(); return; }
  await startApp();
}

boot().catch((e) => {
  replace(main, h('p', { class: 'err' }, `${e.message} (${fmtTime(Date.now())})`));
});
