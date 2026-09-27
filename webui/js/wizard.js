// First-run setup: 1 language → 2 wallet → 3 developer fee + Start. Everything else comes from the
// DGX Spark defaults in config.toml. Nothing is saved before the final button.

import { h, replace, abbrev } from './dom.js';
import { t, setLang, getLang } from './i18n.js';
import * as api from './api.js';
import { checkPearlAddress } from './bech32.js';
import { cmdBox, CLOCKCAP_CMD } from './dashboard.js';

/** A valid Pearl address nobody uses (the manual's screenshots never show a real wallet). */
export const PLACEHOLDER_WALLET = 'prl1pg69hxg0gx3dhlqj0nvxt4w833px6vmx6v45esqw8vayn7ky8jxjswf035d';
/** A second one, for the wallet-change confirmation. */
export const PLACEHOLDER_WALLET_2 = 'prl1ppznef6jlza9cxwjmzzx377p78yet48jcelzvcgpgdvpck2hm2u6qspdt85';
/** The first address with one character changed: a checksum error. */
export const PLACEHOLDER_BAD_WALLET = 'prl1pg69hxg0gx3dhlqj0nvxt4w833px6vmx6v45esqw8vayn7ky8jxjswf0q5d';

/** `capped` | `uncapped` | `unknown`: what the miner has seen of the boot-time clock cap. */
export function presetCapStatus(status) {
  const cap = status && status.power && status.power.clock_cap;
  return cap && (cap.status === 'capped' || cap.status === 'uncapped') ? cap.status : 'unknown';
}

/** The manual on the project's repository, in the page language, at `anchor`. */
export function manualUrl(ctx, anchor) {
  const repo = ctx.about && ctx.about.repository;
  if (!repo) return null;
  const dir = getLang() === 'pt-BR' ? 'pt-BR' : 'en';
  return `${repo}/blob/main/docs/${dir}/MANUAL.md${anchor ? `#${anchor}` : ''}`;
}

export function helpLink(ctx, anchor, text, cls = 'help') {
  const href = manualUrl(ctx, anchor);
  if (!href) return null;
  return h('a', { href, target: '_blank', rel: 'noopener noreferrer', class: cls, title: t('wizard.help'), 'aria-label': t('wizard.help') }, text);
}

/**
 * The wallet box (wizard step 2 and the Settings dialog): live bech32m check, plain-language
 * errors, a Paste button, and the abbreviated ends to compare with the wallet app.
 */
export function walletField({ value = '', label, feeWallet = '', onChange } = {}) {
  const input = h('input', { value, placeholder: 'prl1p…', spellcheck: 'false', autocomplete: 'off', autocapitalize: 'off', class: 'mono wallet-input', 'aria-label': label });
  const msg = h('div', { class: 'walletmsg', 'aria-live': 'polite' });
  let valid = false;
  const check = () => {
    const v = input.value.trim();
    const r = v ? checkPearlAddress(v) : { ok: false, reason: 'empty' };
    valid = r.ok;
    input.classList.toggle('invalid', !!v && !r.ok);
    input.classList.toggle('valid', r.ok);
    const lc = v.toLowerCase();
    replace(msg,
      !v ? null
        : r.ok ? [
          h('div', { class: 'okmsg' }, h('span', { class: 'tick' }, '✓'), ' ', t('wallet.ok')),
          h('div', { class: 'ends' }, h('span', { class: 'muted small' }, t('wallet.ends')), h('span', { class: 'mono endsval' }, abbrev(lc))),
          feeWallet && lc === feeWallet.toLowerCase() ? h('div', { class: 'hint' }, t('wallet.fee_wallet')) : null,
        ]
          : h('div', { class: 'err' }, t(`wallet.err.${r.reason}`)));
    if (onChange) onChange(valid ? lc : v, valid);
  };
  input.addEventListener('input', check);
  input.addEventListener('blur', () => {
    const v = input.value.trim();
    // A bech32m address may be written in upper case; the pools and config.toml take lower case.
    const r = checkPearlAddress(v);
    input.value = r.ok ? v.toLowerCase() : v;
    check();
  });
  const canPaste = !!(navigator.clipboard && navigator.clipboard.readText && window.isSecureContext);
  const paste = canPaste ? h('button', { type: 'button', onclick: async () => {
    try {
      input.value = (await navigator.clipboard.readText()).trim();
      check();
      input.dispatchEvent(new Event('blur'));
    } catch (_) { input.focus(); }
  } }, t('wallet.paste')) : null;
  check();
  return {
    el: h('div', { class: 'field' }, label ? h('label', null, label) : null, h('div', { class: 'rowline' }, input, paste), msg),
    input,
    value: () => { const v = input.value.trim(); return valid ? v.toLowerCase() : v; },
    valid: () => valid,
    set(v) { input.value = v; check(); },
  };
}

/** A 422 as plain sentences, each with its path in small print. */
export function fieldErrors(fields) {
  return (fields || []).map((f) => h('li', null, t(`cfgerr.${f.code}`) === `cfgerr.${f.code}` ? f.message : t(`cfgerr.${f.code}`),
    f.path ? h('span', { class: 'muted small mono' }, ` (${f.path})`) : null));
}

// The draft survives re-renders (choosing the language redraws the whole page).
let draft = null;

export function resetWizard() {
  draft = null;
}

export async function renderWizard(main, ctx) {
  if (!draft) {
    draft = { step: 1, lang: getLang(), wallet: (ctx.config && ctx.config.miner.wallet) || '', accepted: false, presetsOpen: false };
    const shot = ctx.shot();
    if (shot === 'wizard-2') Object.assign(draft, { step: 2, wallet: PLACEHOLDER_WALLET });
    if (shot === 'wallet-error') Object.assign(draft, { step: 2, wallet: PLACEHOLDER_BAD_WALLET });
    if (shot === 'wizard-3' || shot === 'wizard-3-presets') Object.assign(draft, { step: 3, wallet: PLACEHOLDER_WALLET, presetsOpen: shot === 'wizard-3-presets' });
  }
  const d = draft;
  const feeWallet = ctx.fee && ctx.fee.constants ? ctx.fee.constants.dev_wallet : '';

  function buttons(...right) {
    return h('div', { class: 'row wizbtns' },
      d.step > 1 ? h('button', { type: 'button', onclick: () => { d.step--; draw(); } }, t('wizard.back')) : null,
      h('span', { class: 'spacer' }),
      right);
  }

  function title(text, anchor) {
    return h('div', { class: 'row' }, h('h1', null, text), h('span', { class: 'spacer' }), helpLink(ctx, anchor, '?', 'help round'));
  }

  function step1() {
    const pick = (l) => () => { d.lang = l; d.step = 2; setLang(l); };
    return [
      title(t('wizard.welcome.title'), 'wizard-1'),
      h('p', { class: 'lead' }, t('wizard.welcome.pitch')),
      h('p', null, t('wizard.welcome.stop')),
      h('p', { class: 'muted' }, t('wizard.welcome.choose')),
      h('div', { class: 'bigbtns' },
        h('button', { type: 'button', class: getLang() === 'en' ? 'primary' : '', onclick: pick('en') }, 'English'),
        h('button', { type: 'button', class: getLang() === 'pt-BR' ? 'primary' : '', onclick: pick('pt-BR') }, 'Português (Brasil)')),
    ];
  }

  function step2() {
    const next = h('button', { type: 'button', class: 'primary', onclick: () => { d.step = 3; draw(); } }, t('wizard.next'));
    const field = walletField({ value: d.wallet, label: t('wallet.label'), feeWallet, onChange: (v, ok) => { d.wallet = v; next.disabled = !ok; } });
    next.disabled = !field.valid();
    setTimeout(() => field.input.focus());
    return [
      title(t('wallet.title'), 'wizard-2'),
      h('p', null, t('wallet.intro')),
      field.el,
      h('p', { class: 'hint' }, t('wallet.hint')),
      h('p', null, helpLink(ctx, 'wallet', t('wallet.where'), 'small')),
      buttons(next),
    ];
  }

  function step3() {
    const errs = h('div');
    const start = h('button', { type: 'button', class: 'primary big', disabled: !d.accepted }, t('fee.start'));
    const box = h('input', { type: 'checkbox', id: 'fee-accept', checked: d.accepted, onchange: (ev) => { d.accepted = ev.target.checked; start.disabled = !d.accepted; } });
    // The power line claims the 2000 MHz cap only when the miner has seen it in force; the cap is
    // an optional root step of the installer, so it may be missing on this Spark.
    const cap = presetCapStatus(ctx.status);
    const presets = h('details', { class: 'presets', open: d.presetsOpen, ontoggle: (ev) => { d.presetsOpen = ev.target.open; } },
      h('summary', null, t('wizard.presets.title')),
      h('ul', null, ['pools', cap === 'capped' ? 'power' : 'power_nocap', 'gpu', 'later'].map((k) => h('li', null, t(`wizard.presets.${k}`)))));
    const uncapped = cap === 'uncapped'
      ? h('div', { class: 'banner warn' }, h('span', null, t('banner.uncapped', { mhz: ctx.status.power.clock_cap.cap_mhz || 2000 })), cmdBox(ctx, CLOCKCAP_CMD))
      : null;
    start.addEventListener('click', async () => {
      start.disabled = true;
      start.classList.add('busy');
      start.textContent = t('fee.starting');
      replace(errs);
      const restore = () => { start.disabled = !d.accepted; start.classList.remove('busy'); start.textContent = t('fee.start'); };
      let cfg;
      try {
        // A fresh copy: only the three fields of the wizard change, everything else stays as the
        // daemon has it (the DGX Spark defaults on a first run).
        cfg = await api.get('/api/v1/config');
        cfg.miner.wallet = d.wallet;
        cfg.miner.disclosure_accepted = true;
        cfg.gui.language = d.lang || getLang();
        await api.put('/api/v1/config', cfg);
        // The wallet was set on this page: no "changed outside this page" banner for it.
        await api.post('/api/v1/wallet/ack').catch(() => {});
      } catch (e) {
        restore();
        if (e.status === 422 && e.data && e.data.fields) {
          replace(errs, h('div', { class: 'errbox' }, h('p', null, t('wizard.fix')), h('ul', null, fieldErrors(e.data.fields))));
        } else {
          ctx.toast(t('toast.save_failed', { code: e.status || '—', message: e.message }), 'error');
        }
        return;
      }
      try {
        await api.post('/api/v1/mining/start');
        ctx.toast(t('toast.started'), 'ok');
      } catch (e) {
        ctx.toast(t('ctl.err', { action: t('ctl.action.start'), message: e.message }), 'error');
      }
      resetWizard();
      await ctx.refresh();
      ctx.go('dashboard');
    });
    return [
      title(t('fee.title'), 'wizard-3'),
      h('div', { class: 'disclosure stack' },
        ctx.fee ? h('p', { class: 'feeline' }, ctx.fee.banner) : null,
        h('p', null, t('fee.disclosure.1')),
        h('p', null, t('fee.disclosure.2')),
        h('p', null, t('fee.disclosure.3'))),
      h('label', { class: 'check', for: 'fee-accept' }, box, h('span', null, t('fee.accept'))),
      presets,
      uncapped,
      errs,
      buttons(start),
    ];
  }

  function draw() {
    const body = d.step === 1 ? step1() : d.step === 2 ? step2() : step3();
    replace(main, h('div', { class: 'wizard' },
      h('div', { class: 'dots', role: 'img', 'aria-label': t('wizard.step', { n: d.step }) },
        [1, 2, 3].map((n) => h('span', { class: n < d.step ? 'done' : n === d.step ? 'current' : '' }))),
      h('div', { class: 'card stack' }, body)));
  }
  draw();
}
