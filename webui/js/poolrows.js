// The three pool rows of the Settings dialog: one host:port box per slot (Main, Backup 1,
// Backup 2), a Check button, and the merge rules that keep every hidden setting of a pool
// (TLS mode, pinned key, dialect…) that the user did not retype.

import { h, replace } from './dom.js';
import { t, tt } from './i18n.js';
import * as api from './api.js';
import { PRESETS, fromPreset, defaultPools, customPool } from './presets.js';

export const SLOTS = 3;
const HOST_RE = /^(?=.{1,253}$)([A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?)(\.[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?)*$/;
const HIDDEN = ['tls', 'spki_pin', 'dialect', 'jsonrpc', 'proof', 'password', 'pattern', 'enabled'];

/** A pool error code in plain words. */
export function errorText(code) {
  return tt(`err.${code}`) || code || '';
}

/** `host:port` or `[v6]:port` → { host, port }, or null. */
export function parseHostPort(text) {
  const s = (text || '').trim();
  let m = s.match(/^\[([0-9A-Fa-f:.]+)\]:(\d{1,5})$/);
  if (!m) {
    m = s.match(/^([A-Za-z0-9.-]+):(\d{1,5})$/);
    if (!m || !HOST_RE.test(m[1])) return null;
  }
  const port = Number(m[2]);
  if (!Number.isInteger(port) || port < 1 || port > 65535) return null;
  return { host: m[1].toLowerCase(), port };
}

/** The text a pool entry shows in its row. */
export function hostPort(p) {
  return p.host.includes(':') ? `[${p.host}]:${p.port}` : `${p.host}:${p.port}`;
}

const same = (p, hp) => p && p.host.trim().toLowerCase() === hp.host && Number(p.port) === hp.port;

/**
 * The entry a row stands for. Empty text → null (slot removed). The entry saved in the same
 * slot is kept whole when the text still names it; then a saved entry of another slot (the user
 * reordered the rows); then a preset (its full settings); else a custom pool on defaults.
 */
export function mergeRow(text, saved, slot) {
  if (!(text || '').trim()) return { entry: null };
  const hp = parseHostPort(text);
  if (!hp) return { error: t('set.err.hostport') };
  if (same(saved[slot], hp)) return { entry: structuredClone(saved[slot]) };
  const other = saved.find((p) => same(p, hp));
  if (other) return { entry: structuredClone(other) };
  const preset = PRESETS.find((p) => p.host === hp.host && p.port === hp.port);
  if (preset) return { entry: fromPreset(preset) };
  return { entry: customPool(hp.host.slice(0, 40), hp.port) };
}

/** A saved entry whose hidden settings differ from what its preset (or a plain custom pool) has. */
export function hasAdvanced(p) {
  const preset = PRESETS.find((x) => x.host === p.host.trim().toLowerCase() && x.port === Number(p.port));
  const base = preset ? fromPreset(preset) : customPool(p.host, p.port);
  return HIDDEN.some((k) => (p[k] ?? base[k]) !== base[k]);
}

/**
 * @param {object[]} saved the pools in config.toml
 * @returns {{ el, collect: () => { pools, rowOf, errors }, showError: (row, msg) => void }}
 */
export function poolRows(saved) {
  const listId = `pool-presets-${Math.random().toString(36).slice(2, 8)}`;
  const datalist = h('datalist', { id: listId },
    PRESETS.map((p) => h('option', { value: `${p.host}:${p.port}` }, p.verified ? p.name : `${p.name} ${t('set.unverified')}`)));
  const rows = [];
  let forced = [null, null, null];

  for (let i = 0; i < SLOTS; i++) {
    const p = saved[i];
    const input = h('input', {
      value: p ? hostPort(p) : '', placeholder: t('set.row_placeholder'), list: listId, spellcheck: 'false',
      autocomplete: 'off', class: 'mono', 'aria-label': t(`set.row.${i}`),
    });
    const note = h('div', { class: 'hint' }, p && hasAdvanced(p) ? t('set.advanced_row') : null);
    const msg = h('div', { class: 'rowmsg' });
    const check = h('button', { type: 'button', class: 'small-btn' }, t('set.check'));
    const row = { input, note, msg, check };
    input.addEventListener('input', () => {
      forced[i] = null;
      input.classList.remove('invalid');
      replace(msg);
      replace(note, same(saved[i], parseHostPort(input.value) || { host: '', port: 0 }) && hasAdvanced(saved[i]) ? t('set.advanced_row') : null);
    });
    input.addEventListener('blur', () => validateRow(i));
    check.addEventListener('click', () => runCheck(i));
    row.el = h('div', { class: 'poolrow' },
      h('label', { class: 'rowlabel' }, t(`set.row.${i}`)),
      h('div', { class: 'rowbody' }, h('div', { class: 'rowline' }, input, check), note, msg));
    rows.push(row);
  }

  function entryOf(i) {
    if (forced[i]) return { entry: structuredClone(forced[i]) };
    return mergeRow(rows[i].input.value, saved, i);
  }

  function showError(i, text) {
    rows[i].input.classList.toggle('invalid', !!text);
    replace(rows[i].msg, text ? h('div', { class: 'err' }, text) : null);
  }

  function validateRow(i) {
    const r = entryOf(i);
    showError(i, r.error || null);
    return r;
  }

  async function runCheck(i) {
    const r = validateRow(i);
    if (!r.entry) return;
    const row = rows[i];
    row.check.disabled = true;
    replace(row.msg, h('div', { class: 'muted small' }, t('set.checking')));
    try {
      const res = await api.post('/api/v1/pools/test', { pool: r.entry, confirm: false });
      const ms = (res.steps || []).reduce((a, s) => a + (s.ms || 0), 0);
      replace(row.msg, res.ok
        ? h('div', { class: 'okmsg' }, t('set.check_ok', { ms }))
        : h('div', { class: 'err' }, errorText(res.error_code)));
    } catch (e) {
      replace(row.msg, h('div', { class: 'err' }, e.message));
    } finally {
      row.check.disabled = false;
    }
  }

  const restore = h('button', { type: 'button', class: 'link', onclick: () => {
    const d = defaultPools();
    forced = d.map((p) => p);
    rows.forEach((row, i) => {
      row.input.value = hostPort(d[i]);
      showError(i, null);
      replace(row.note);
    });
  } }, t('set.restore'));

  const general = h('div');
  const el = h('div', { class: 'poolrows' }, datalist, rows.map((r) => r.el), h('div', { class: 'row' }, restore), general);

  return {
    el,
    rows,
    showError,
    /** Show a problem that is not about one row (e.g. no pool at all). */
    showGeneral(text) { replace(general, text ? h('div', { class: 'err' }, text) : null); },
    /** The pools to save, the row of each saved index, and whether any row is invalid. */
    collect() {
      const pools = [];
      const rowOf = [];
      let errors = 0;
      const seen = new Set();
      for (let i = 0; i < SLOTS; i++) {
        const r = validateRow(i);
        if (r.error) { errors++; continue; }
        if (!r.entry) continue;
        const key = hostPort(r.entry).toLowerCase();
        if (seen.has(key)) { showError(i, t('set.err.duplicate')); errors++; continue; }
        seen.add(key);
        pools.push(r.entry);
        rowOf.push(i);
      }
      this.showGeneral(!errors && !pools.length ? t('set.err.no_pool') : null);
      if (!pools.length) errors++;
      return { pools, rowOf, errors };
    },
    /** Pre-type a row (display flags for the manual's screenshots). */
    type(i, text) {
      rows[i].input.value = text;
      rows[i].input.dispatchEvent(new Event('input'));
      validateRow(i);
    },
  };
}
