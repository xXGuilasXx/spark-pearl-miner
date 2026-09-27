// The Settings dialog (gear icon): wallet, worker name, language and the three pool rows. Save
// reads a fresh copy of the configuration and changes only these four things; every other
// setting lives in config.toml.

import { h, replace, abbrev, modal, confirmBox } from './dom.js';
import { t, setLang, detect } from './i18n.js';
import * as api from './api.js';
import { walletField, fieldErrors, PLACEHOLDER_WALLET_2 } from './wizard.js';
import { poolRows } from './poolrows.js';
import { cmdBox } from './dashboard.js';

const WORKER_RE = /^[A-Za-z0-9_-]{1,32}$/;
export const OPEN_CMD = 'xdg-open ~/.config/spark-pearl-miner/config.toml';
export const RESTART_CMD = 'systemctl --user restart spark-pearl-miner';

let openNow = null;

/**
 * @param {object} ctx the app context
 * @param {{ shot?: string }} opts display flags for the manual's screenshots (never saved)
 */
export async function openSettings(ctx, { shot } = {}) {
  if (openNow) return openNow;
  let cfg;
  try {
    cfg = await api.get('/api/v1/config');
  } catch (e) {
    ctx.toast(e.message, 'error');
    return null;
  }
  const feeWallet = ctx.fee && ctx.fee.constants ? ctx.fee.constants.dev_wallet : '';
  const savedWallet = cfg.miner.wallet;
  const wallet = walletField({ value: savedWallet, label: t('set.wallet'), feeWallet });
  const worker = h('input', { value: cfg.miner.worker, maxlength: 32, spellcheck: 'false', autocomplete: 'off' });
  const workerMsg = h('div');
  const checkWorker = () => {
    const ok = WORKER_RE.test(worker.value);
    worker.classList.toggle('invalid', !ok);
    replace(workerMsg, ok ? null : h('div', { class: 'err' }, t('set.err.worker')));
    return ok;
  };
  worker.addEventListener('input', checkWorker);
  const lang = h('select', null,
    [['auto', t('set.lang.auto')], ['en', 'English'], ['pt-BR', 'Português (Brasil)']].map(([v, label]) =>
      h('option', { value: v, selected: cfg.gui.language === v }, label)));
  const langMsg = h('div');
  const pools = poolRows(cfg.pools || []);
  const general = h('div');
  const path = (ctx.about && ctx.about.config_path) || '~/.config/spark-pearl-miner/config.toml';
  const save = h('button', { type: 'button', class: 'primary' }, t('set.save'));

  const dialog = modal(t('set.title'), (close) => {
    save.addEventListener('click', () => submit(close));
    return [
      h('div', { class: 'row' }, h('h2', null, t('set.title')), h('span', { class: 'spacer' }),
        h('button', { type: 'button', class: 'iconbtn', 'aria-label': t('common.close'), title: t('common.close'), onclick: () => close() }, '✕')),
      wallet.el,
      h('div', { class: 'field' }, h('label', null, t('set.worker')), worker, h('div', { class: 'hint' }, t('set.worker_hint')), workerMsg),
      h('div', { class: 'field' }, h('label', null, t('set.language')), lang, langMsg),
      h('div', { class: 'field' }, h('label', null, t('set.pools')), h('p', { class: 'hint' }, t('set.pools_hint')), pools.el),
      h('div', { class: 'filenote' },
        h('p', { class: 'small' }, t('set.file', { path })),
        h('div', { class: 'small muted' }, t('set.open_cmd')), cmdBox(ctx, OPEN_CMD),
        h('div', { class: 'small muted' }, t('set.restart_cmd')), cmdBox(ctx, RESTART_CMD)),
      general,
      h('div', { class: 'row end' }, h('button', { type: 'button', onclick: () => close() }, t('common.cancel')), save),
    ];
  }, { onClose: () => { openNow = null; } });
  dialog.el.classList.add('wide');
  openNow = dialog;

  async function submit(close) {
    replace(general);
    const okWorker = checkWorker();
    const rows = pools.collect();
    if (!wallet.valid() || !okWorker || rows.errors) {
      replace(general, h('div', { class: 'err' }, t('set.fix')));
      return;
    }
    const newWallet = wallet.value();
    if (shot) return; // display flags never save
    if (newWallet !== savedWallet && !(await confirmBox(t('set.confirm_wallet', { w: abbrev(newWallet) }), t('set.confirm_yes'), t('common.cancel')))) return;
    save.disabled = true;
    save.textContent = t('set.saving');
    try {
      const fresh = await api.get('/api/v1/config');
      fresh.miner.wallet = newWallet;
      fresh.miner.worker = worker.value;
      fresh.gui.language = lang.value;
      fresh.pools = rows.pools;
      const r = await api.put('/api/v1/config', fresh);
      ctx.config = fresh;
      // A wallet changed on this page is not a change "outside this page": acknowledge it.
      if (newWallet !== savedWallet) await api.post('/api/v1/wallet/ack').catch(() => {});
      close();
      const restart = (r && r.restart_required) || [];
      if (restart.length) ctx.toast(t('toast.saved_restart', { restart_required: restart.join(', ') }), 'warn', RESTART_CMD);
      else ctx.toast(t('toast.saved'), 'ok');
      if (lang.value !== cfg.gui.language) {
        if (lang.value === 'auto') {
          try { localStorage.removeItem('spm.lang'); } catch (_) { /* ignore */ }
          await setLang(detect(), false);
        } else {
          await setLang(lang.value);
        }
      }
      ctx.poll();
    } catch (e) {
      save.disabled = false;
      save.textContent = t('set.save');
      const fields = (e.data && e.data.fields) || [];
      if (e.status === 422 && fields.length) {
        const rest = [];
        for (const f of fields) {
          const m = /^pools\[(\d+)\]/.exec(f.path || '');
          const text = t(`cfgerr.${f.code}`) === `cfgerr.${f.code}` ? f.message : t(`cfgerr.${f.code}`);
          if (m && rows.rowOf[Number(m[1])] !== undefined) pools.showError(rows.rowOf[Number(m[1])], text);
          else if (f.path === 'miner.worker') replace(workerMsg, h('div', { class: 'err' }, text));
          else if (f.path === 'gui.language') replace(langMsg, h('div', { class: 'err' }, text));
          else rest.push(f);
        }
        if (rest.length) replace(general, h('ul', { class: 'errbox' }, fieldErrors(rest)));
      } else {
        ctx.toast(t('toast.save_failed', { code: e.status || '—', message: e.message }), 'error');
      }
    }
  }

  if (shot === 'settings-error') {
    pools.type(1, 'pool.example.com:99999');
  } else if (shot === 'settings-wallet-confirm') {
    wallet.set(PLACEHOLDER_WALLET_2);
    confirmBox(t('set.confirm_wallet', { w: abbrev(PLACEHOLDER_WALLET_2) }), t('set.confirm_yes'), t('common.cancel'));
  }
  return dialog;
}
