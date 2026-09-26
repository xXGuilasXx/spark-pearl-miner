// The 3-slot pool editor (setup wizard and Pools screen): presets, host/port/TLS, advanced
// dialect/encoding/password/pattern, reorder, and "Test connection".

import { h, replace } from './dom.js';
import { t, tt } from './i18n.js';
import * as api from './api.js';
import { PRESETS, fromPreset, presetOf, customPool } from './presets.js';

const HOST_RE = /^(?=.{1,253}$)([A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?)(\.[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?)*$/;
const IPV6_RE = /^[0-9A-Fa-f:.]+$/;
const PIN_RE = /^[A-Za-z0-9+/]{43}=$/;
const MAX = 3;

export function errorText(code, detail) {
  const s = tt(`err.${code}`);
  return s || (detail ? `${code}: ${detail}` : code);
}

function clean(p) {
  const out = { ...p };
  out.host = (out.host || '').trim();
  out.port = Number(out.port) || 0;
  out.name = (out.name || '').trim();
  if (out.tls !== 'pinned') out.spki_pin = '';
  else out.spki_pin = (out.spki_pin || '').trim();
  return out;
}

function validate(p) {
  const e = {};
  const host = (p.host || '').trim();
  if (!host || !(HOST_RE.test(host) || (host.includes(':') && IPV6_RE.test(host)))) e.host = t('pools.err.host');
  const port = Number(p.port);
  if (!Number.isInteger(port) || port < 1 || port > 65535) e.port = t('pools.err.port');
  if (p.tls === 'pinned' && !PIN_RE.test((p.spki_pin || '').trim())) e.pin = t('pools.err.pin');
  if ((p.password || '').length > 64 || /\s/.test(p.password || '')) e.password = t('pools.err.password');
  return e;
}

function select(value, options, onchange) {
  return h('select', { onchange: (ev) => onchange(ev.target.value) },
    options.map(([v, label]) => h('option', { value: v, selected: v === value }, label)));
}

/**
 * @param {object[]} initial pool entries (config schema)
 * @param {{onChange?: Function}} opts
 */
export function poolEditor(initial, opts = {}) {
  let pools = (initial && initial.length ? initial : [customPool()]).map((p) => ({ ...p }));
  const root = h('div', { class: 'pool-editor' });
  const changed = () => opts.onChange && opts.onChange(pools.map(clean));

  function render() {
    replace(root,
      pools.map((p, i) => slotCard(p, i)),
      pools.length < MAX
        ? h('button', { type: 'button', onclick: () => { pools.push(customPool()); render(); changed(); } }, t('pools.add', { n: pools.length + 1 }))
        : h('p', { class: 'hint' }, t('pools.max')));
  }

  function move(i, d) {
    const j = i + d;
    if (j < 0 || j >= pools.length) return;
    [pools[i], pools[j]] = [pools[j], pools[i]];
    render();
    changed();
  }

  function slotCard(p, i) {
    const preset = presetOf(p);
    const errs = h('div');
    const showErrors = () => {
      const e = validate(p);
      replace(errs, Object.values(e).map((m) => h('div', { class: 'err' }, m)));
    };
    const bind = (key, numeric = false) => (ev) => {
      p[key] = numeric ? ev.target.value.replace(/[^0-9]/g, '') : ev.target.value;
      showErrors();
      changed();
    };
    const pinField = h('div', { class: 'field', hidden: p.tls !== 'pinned' },
      h('label', null, t('pools.pin')),
      h('input', { value: p.spki_pin || '', placeholder: 'base64 SHA-256', spellcheck: 'false', oninput: bind('spki_pin'), class: 'mono' }),
      h('div', { class: 'hint' }, t('pools.pin_hint')));
    const result = h('div');
    const authBox = h('input', { type: 'checkbox' });

    const presetSel = select(preset ? preset.id : 'custom',
      [...PRESETS.map((x) => [x.id, x.unverified ? `${x.name} (${t('pools.unverified')})` : x.name]), ['custom', t('pools.custom')]],
      (id) => {
        if (id === 'custom') {
          pools[i] = { ...customPool(), enabled: p.enabled };
        } else {
          pools[i] = { ...fromPreset(PRESETS.find((x) => x.id === id)), enabled: p.enabled };
        }
        render();
        changed();
      });

    async function test() {
      const e = validate(p);
      if (Object.keys(e).length) { showErrors(); return; }
      let confirm = false;
      if (authBox.checked) {
        confirm = window.confirm(t('pools.test.confirm'));
        if (!confirm) return;
      }
      replace(result, h('p', { class: 'muted' }, t('pools.test.running')));
      try {
        const r = await api.post('/api/v1/pools/test', { pool: clean(p), confirm });
        replace(result,
          h('ul', { class: 'steps' }, r.steps.map((s) => h('li', { class: s.ok ? 'ok' : 'fail' },
            `${t(`pools.test.step.${s.step}`)} — ${s.detail} (${s.ms} ms)`))),
          r.ok
            ? h('div', { class: 'okmsg' }, r.authorize_tested ? t('pools.test.ok_auth') : t('pools.test.ok'))
            : h('div', { class: 'err' }, errorText(r.error_code)));
      } catch (err) {
        replace(result, h('div', { class: 'err' }, err.message));
      }
    }

    const card = h('div', { class: 'slot' },
      h('div', { class: 'head' },
        h('span', { class: 'num' }, t('pools.slot', { n: i + 1 })),
        h('strong', null, p.name || (preset ? preset.name : t('pools.custom'))),
        preset && preset.unverified ? h('span', { class: 'chip st-warn' }, t('pools.unverified')) : null,
        h('span', { class: 'spacer' }),
        h('label', { class: 'row' }, h('input', { type: 'checkbox', checked: p.enabled !== false, onchange: (ev) => { p.enabled = ev.target.checked; changed(); } }), t('pools.enabled')),
        h('button', { type: 'button', title: t('pools.up'), disabled: i === 0, onclick: () => move(i, -1) }, '↑'),
        h('button', { type: 'button', title: t('pools.down'), disabled: i === pools.length - 1, onclick: () => move(i, 1) }, '↓'),
        pools.length > 1 ? h('button', { type: 'button', class: 'danger', onclick: () => { pools.splice(i, 1); render(); changed(); } }, t('pools.remove')) : null),
      h('div', { class: 'fields' },
        h('div', { class: 'field' }, h('label', null, t('pools.preset')), presetSel),
        h('div', { class: 'field' }, h('label', null, t('pools.host')), h('input', { value: p.host || '', spellcheck: 'false', autocomplete: 'off', oninput: bind('host') })),
        h('div', { class: 'field' }, h('label', null, t('pools.port')), h('input', { value: p.port ? String(p.port) : '', inputmode: 'numeric', oninput: bind('port', true) })),
        h('div', { class: 'field' }, h('label', null, t('pools.tls')),
          select(p.tls, [['auto', t('pools.tls.auto')], ['on', t('pools.tls.on')], ['off', t('pools.tls.off')], ['pinned', t('pools.tls.pinned')]], (v) => {
            p.tls = v;
            pinField.hidden = v !== 'pinned';
            showErrors();
            changed();
          }))),
      pinField,
      h('details', null,
        h('summary', null, t('pools.advanced')),
        h('div', { class: 'fields' },
          h('div', { class: 'field' }, h('label', null, t('pools.name')), h('input', { value: p.name || '', maxlength: 40, oninput: bind('name') })),
          h('div', { class: 'field' }, h('label', null, t('pools.dialect')),
            select(p.dialect, [['auto', t('pools.auto')], ['object', 'object (HeroMiners, LuckyPool)'], ['kryptex', 'kryptex'], ['kryptex-v2', 'kryptex-v2 (gzip)']], (v) => { p.dialect = v; changed(); })),
          h('div', { class: 'field' }, h('label', null, t('pools.jsonrpc')),
            select(p.jsonrpc, [['auto', t('pools.auto')], ['on', t('pools.on')], ['off', t('pools.off')]], (v) => { p.jsonrpc = v; changed(); })),
          h('div', { class: 'field' }, h('label', null, t('pools.encoding')),
            select(p.proof, [['auto', t('pools.encoding.auto')], ['plain', 'plain_proof'], ['zstd', 'plain_proof_zst (zstd)']], (v) => { p.proof = v; changed(); })),
          h('div', { class: 'field' }, h('label', null, t('pools.password')), h('input', { value: p.password ?? 'x', maxlength: 64, spellcheck: 'false', oninput: bind('password') })),
          h('div', { class: 'field' }, h('label', null, t('pools.pattern')),
            select(p.pattern, [['auto', t('pools.pattern.auto')], ['official', t('pools.pattern.official')]], (v) => { p.pattern = v; changed(); }))),
        h('p', { class: 'hint' }, t('pools.advanced_hint'))),
      h('div', { class: 'row' },
        h('button', { type: 'button', onclick: test }, t('pools.test')),
        h('label', { class: 'row small' }, authBox, t('pools.test.auth'))),
      result,
      errs);
    showErrors();
    return card;
  }

  render();
  return {
    el: root,
    value: () => pools.map(clean),
    errors: () => pools.map(validate).filter((e) => Object.keys(e).length).length,
    set(ps) { pools = ps.map((p) => ({ ...p })); render(); },
  };
}
