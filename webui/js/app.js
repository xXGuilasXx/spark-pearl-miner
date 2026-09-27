// spark-pearl-miner web GUI: session, router, live updates (SSE) and the header.

import { h, replace, abbrev, fmtTime } from './dom.js';
import { t, setLang, detect, onLang, getLang } from './i18n.js';
import * as api from './api.js';
import { renderWizard } from './wizard.js';
import { SCREENS, chip } from './screens.js';

const NAV = ['dashboard', 'pools', 'failover', 'power', 'fee', 'logs', 'about'];
const main = document.getElementById('main');
const nav = document.getElementById('nav');
const pill = document.getElementById('state-pill');
const banner = document.getElementById('banner');
const toasts = document.getElementById('toasts');

const ctx = { status: null, pools: null, config: null, toast, go, refresh, poll };
let route = null;
let events = null;
let started = false;

function toast(msg, kind = 'info') {
  const el = h('div', { class: `toast ${kind}` }, msg);
  toasts.append(el);
  setTimeout(() => el.remove(), kind === 'error' ? 9000 : 5000);
}

function go(r) {
  location.hash = `#/${r}`;
}

async function refresh() {
  const [status, pools, config] = await Promise.all([api.get('/api/v1/status'), api.get('/api/v1/pools'), api.get('/api/v1/config')]);
  Object.assign(ctx, { status, pools, config });
}

async function poll() {
  try {
    ctx.pools = await api.get('/api/v1/pools');
    if (route && SCREENS[route] && route === 'pools') SCREENS.pools.update(ctx);
  } catch (_) { /* SSE shows the connection state */ }
}

function drawHeader() {
  document.getElementById('lang-en').classList.toggle('active', getLang() === 'en');
  document.getElementById('lang-pt').classList.toggle('active', getLang() === 'pt-BR');
  if (!started) return;
  nav.hidden = false;
  replace(nav, NAV.map((r) => h('a', { href: `#/${r}`, class: r === route ? 'active' : '' }, t(`nav.${r}`))),
    h('a', { href: '#/setup', class: route === 'setup' ? 'active' : '' }, t('nav.setup')));
  drawStatus();
}

function drawStatus() {
  const s = ctx.status;
  if (!s) return;
  pill.hidden = false;
  pill.className = 'pillbox';
  replace(pill, chip(s.state), s.active_pool ? ` ${t('pools.slot', { n: s.active_pool })}` : '');
  const parts = [];
  if (s.wallet_changed) {
    parts.push(h('span', null, t('banner.wallet_changed', { src: t(`source.${s.wallet_changed.source}`), prev: s.wallet_changed.previous, now: abbrev(s.wallet) })),
      h('button', { type: 'button', onclick: async () => { try { await api.post('/api/v1/wallet/ack'); } catch (e) { toast(e.message, 'error'); } } }, t('banner.wallet_ack')));
  }
  if (s.worker && s.worker.simulated) parts.push(h('span', null, t('banner.sim')));
  if (s.worker && s.worker.state === 'unavailable') parts.push(h('span', null, t('banner.no_cuda')));
  banner.hidden = parts.length === 0;
  banner.className = `banner${s.wallet_changed ? '' : ' sim'}`;
  replace(banner, parts);
}

function render() {
  const wanted = (location.hash.replace(/^#\/?/, '') || 'dashboard').split('?')[0];
  if (ctx.status && ctx.status.setup_required && wanted !== 'about' && wanted !== 'logs') {
    route = 'setup';
  } else {
    route = wanted === 'setup' || SCREENS[wanted] ? wanted : 'dashboard';
  }
  drawHeader();
  if (route === 'setup') {
    renderWizard(main, ctx);
  } else {
    SCREENS[route].render(main, ctx);
  }
}

function connectEvents() {
  if (events) events.close();
  events = new EventSource('/api/v1/events');
  events.addEventListener('stats', (e) => {
    const wasSetup = ctx.status && ctx.status.setup_required;
    ctx.status = JSON.parse(e.data);
    drawStatus();
    if (wasSetup && !ctx.status.setup_required && route === 'setup') return;
    if (route && SCREENS[route] && route !== 'pools' && route !== 'fee' && route !== 'power') SCREENS[route].update(ctx);
  });
  events.addEventListener('alert', (e) => {
    const a = JSON.parse(e.data);
    toast(a.msg, a.level === 'error' ? 'error' : 'warn');
  });
  for (const name of ['fsm', 'timeline']) events.addEventListener(name, () => poll());
  events.addEventListener('log', (e) => {
    if (route === 'logs') SCREENS.logs.push(JSON.parse(e.data));
  });
  events.addEventListener('share', (e) => {
    const s = JSON.parse(e.data);
    if (!s.accepted) toast(t('share.rejected', { pool: s.pool || 'dev', reason: s.reason || '' }), 'warn');
  });
  events.addEventListener('config', async () => {
    try { ctx.config = await api.get('/api/v1/config'); } catch (_) { /* ignore */ }
  });
  events.onerror = () => {
    pill.hidden = false;
    pill.className = 'pillbox';
    replace(pill, chip('offline'));
  };
}

function showLogin(message) {
  started = false;
  nav.hidden = true;
  pill.hidden = true;
  banner.hidden = true;
  if (events) { events.close(); events = null; }
  const input = h('input', { type: 'password', autocomplete: 'off', spellcheck: 'false', class: 'mono', placeholder: '64 hex' });
  const err = h('div');
  const submit = async () => {
    try {
      await api.login(input.value.trim());
      await startApp();
    } catch (_) {
      replace(err, h('div', { class: 'err' }, t('login.bad')));
    }
  };
  input.addEventListener('keydown', (ev) => { if (ev.key === 'Enter') submit(); });
  replace(main, h('div', { class: 'card login stack' },
    h('h1', null, t('login.title')),
    message ? h('p', { class: 'err' }, message) : null,
    h('p', null, t('login.text')),
    h('pre', { class: 'cmd' }, 'spark-pearl-miner gui\n# ssh: spark-pearl-miner gui --print-url'),
    h('p', { class: 'hint' }, t('login.local')),
    h('div', { class: 'field' }, h('label', null, t('login.token')), input, h('div', { class: 'hint' }, t('login.token_hint'))),
    err,
    h('div', { class: 'row end' }, h('button', { type: 'button', class: 'primary', onclick: submit }, t('login.submit')))));
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
  if (!saved && ctx.config.gui && ctx.config.gui.language !== 'auto') await setLang(ctx.config.gui.language, false);
  started = true;
  connectEvents();
  render();
}

/** Log in with a token passed in the URL fragment (never sent to the server), then drop it
 * from the address bar. */
async function fragmentLogin() {
  const m = location.hash.match(/token=([0-9a-fA-F]{64})/);
  if (!m) return false;
  history.replaceState(null, '', `${location.pathname}#/dashboard`);
  try {
    return await api.login(m[1]);
  } catch (_) {
    toast(t('login.bad'), 'error');
    return false;
  }
}

async function boot() {
  await setLang(detect(), false);
  for (const b of document.querySelectorAll('.lang button')) {
    b.addEventListener('click', () => setLang(b.dataset.lang));
  }
  onLang(() => { if (started) render(); else drawHeader(); });
  drawHeader();
  window.addEventListener('hashchange', async () => {
    if (await fragmentLogin()) { await startApp(); return; }
    if (started) render();
  });
  setInterval(() => { if (started && (route === 'dashboard' || route === 'pools')) poll(); }, 2000);
  setInterval(() => { if (started && (route === 'fee' || route === 'power')) SCREENS[route].update(ctx); }, 5000);

  let ok = await fragmentLogin();
  if (!ok) ok = await api.resume();
  if (!ok) { showLogin(); return; }
  await startApp();
}

boot().catch((e) => {
  replace(main, h('p', { class: 'err' }, `${e.message} (${fmtTime(Date.now())})`));
});
