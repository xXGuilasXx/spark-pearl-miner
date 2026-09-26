// First-run setup wizard: language → wallet → worker → pools → fee disclosure → power →
// coexistence → summary → Start.

import { h, replace } from './dom.js';
import { t, setLang, getLang } from './i18n.js';
import * as api from './api.js';
import { checkPearlAddress } from './bech32.js';
import { poolEditor } from './pooleditor.js';
import { defaultPools } from './presets.js';

const STEPS = ['language', 'wallet', 'worker', 'pools', 'fee', 'power', 'coexistence', 'summary'];
const WORKER_RE = /^[A-Za-z0-9_-]{1,32}$/;

export function maxPhrase() {
  return t('power.max.phrase');
}

export function walletMessage(v) {
  const r = checkPearlAddress(v);
  return r.ok ? null : t(`wallet.err.${r.reason}`);
}

// The draft survives re-renders (a language switch redraws the whole page).
let draft = null;

export function resetWizard() {
  draft = null;
}

export async function renderWizard(main, ctx) {
  if (!draft) {
    const cfg = structuredClone(ctx.config);
    if (!cfg.pools || !cfg.pools.length) cfg.pools = defaultPools();
    draft = { cfg, step: 0, accepted: !!cfg.miner.disclosure_accepted, maxTyped: cfg.power.max_acknowledged ? maxPhrase() : '' };
  }
  const d = draft;
  const cfg = d.cfg;
  let fee = null;
  try { fee = await api.get('/api/v1/fee'); } catch (_) { fee = null; }
  const editor = poolEditor(cfg.pools, { onChange: (ps) => { cfg.pools = ps; } });

  function nav(canNext, nextLabel, onNext) {
    return h('div', { class: 'row end' },
      d.step > 0 ? h('button', { type: 'button', onclick: () => { d.step--; draw(); } }, t('wizard.back')) : null,
      h('span', { class: 'spacer' }),
      h('button', { type: 'button', class: 'primary', disabled: !canNext, onclick: onNext || (() => { d.step++; draw(); }) }, nextLabel || t('wizard.next')));
  }

  function body() {
    switch (STEPS[d.step]) {
      case 'language':
        return [
          h('h1', null, t('wizard.welcome')),
          h('p', null, t('wizard.intro')),
          h('div', { class: 'bigbtns' },
            h('button', { type: 'button', class: getLang() === 'en' ? 'primary' : '', onclick: () => { cfg.gui.language = 'en'; d.step = 1; setLang('en'); } }, 'English'),
            h('button', { type: 'button', class: getLang() === 'pt-BR' ? 'primary' : '', onclick: () => { cfg.gui.language = 'pt-BR'; d.step = 1; setLang('pt-BR'); } }, 'Português (Brasil)')),
        ];
      case 'wallet': {
        const msg = h('div');
        const next = h('div');
        const input = h('input', { value: cfg.miner.wallet || '', placeholder: 'prl1p…', spellcheck: 'false', autocomplete: 'off', class: 'mono' });
        const check = () => {
          const v = input.value.trim();
          const err = v ? walletMessage(v) : null;
          input.classList.toggle('invalid', !!err);
          input.classList.toggle('valid', !!v && !err);
          replace(msg, err ? h('div', { class: 'err' }, err) : v ? h('div', { class: 'okmsg' }, t('wallet.ok')) : null);
          cfg.miner.wallet = err ? v : v.toLowerCase();
          replace(next, nav(v && !err));
        };
        input.addEventListener('input', check);
        setTimeout(check);
        return [
          h('h1', null, t('wallet.title')),
          h('p', null, t('wallet.intro')),
          h('div', { class: 'field' }, h('label', null, t('wallet.label')), input, msg),
          h('p', { class: 'hint' }, t('wallet.hint')),
          next,
        ];
      }
      case 'worker': {
        const msg = h('div');
        const next = h('div');
        const input = h('input', { value: cfg.miner.worker || 'spark', maxlength: 32, spellcheck: 'false' });
        const check = () => {
          const ok = WORKER_RE.test(input.value);
          cfg.miner.worker = input.value;
          input.classList.toggle('invalid', !ok);
          replace(msg, ok ? null : h('div', { class: 'err' }, t('worker.err')));
          replace(next, nav(ok));
        };
        input.addEventListener('input', check);
        setTimeout(check);
        return [h('h1', null, t('worker.title')), h('p', null, t('worker.intro')), h('div', { class: 'field' }, h('label', null, t('worker.label')), input, msg), next];
      }
      case 'pools':
        return [
          h('h1', null, t('wizard.pools.title')),
          h('p', null, t('wizard.pools.intro')),
          editor.el,
          nav(true, null, () => {
            if (editor.errors()) { ctx.toast(t('pools.err.fix'), 'warn'); return; }
            cfg.pools = editor.value();
            d.step++;
            draw();
          }),
        ];
      case 'fee': {
        const box = h('input', { type: 'checkbox', checked: d.accepted, onchange: (ev) => { d.accepted = ev.target.checked; draw(); } });
        return [
          h('h1', null, t('fee.title')),
          h('div', { class: 'disclosure stack' },
            h('p', null, t('fee.disclosure.1')),
            fee ? h('p', { class: 'feeline' }, fee.banner) : null,
            h('p', null, t('fee.disclosure.2')),
            h('p', null, t('fee.disclosure.3')),
            h('p', null, t('fee.disclosure.4'))),
          h('label', { class: 'row' }, box, t('fee.accept')),
          nav(d.accepted),
        ];
      }
      case 'power': {
        const choice = (id) => h('label', { class: `choice${cfg.power.profile === id ? ' selected' : ''}` },
          h('input', { type: 'radio', name: 'profile', checked: cfg.power.profile === id, onchange: () => { cfg.power.profile = id; draw(); } }),
          h('strong', null, t(`power.${id}`)), h('span', { class: 'muted small' }, t(`power.${id}.desc`)));
        const typed = h('input', { value: d.maxTyped, placeholder: maxPhrase(), oninput: (ev) => { d.maxTyped = ev.target.value; cfg.power.max_acknowledged = d.maxTyped.trim() === maxPhrase(); replace(nextBox, nav(cfg.power.profile !== 'max' || cfg.power.max_acknowledged)); } });
        const nextBox = h('div', null, nav(cfg.power.profile !== 'max' || cfg.power.max_acknowledged));
        return [
          h('h1', null, t('power.title')),
          h('p', null, t('power.intro')),
          choice('eco'), choice('balanced'), choice('max'),
          cfg.power.profile === 'max'
            ? h('div', { class: 'field' }, h('label', null, t('power.max.type', { phrase: maxPhrase() })), typed, h('div', { class: 'hint' }, t('power.max.warn')))
            : null,
          nextBox,
        ];
      }
      case 'coexistence': {
        const modes = ['spark-modo', 'yield', 'yield-release', 'exclusive'];
        const choice = (id) => h('label', { class: `choice${cfg.coexistence.mode === id ? ' selected' : ''}` },
          h('input', { type: 'radio', name: 'coex', checked: cfg.coexistence.mode === id, onchange: () => {
            cfg.coexistence.mode = id;
            cfg.worker.launch = id === 'spark-modo' ? 'external' : 'spawn';
            draw();
          } }),
          h('strong', null, t(`coex.${id}`)),
          h('span', { class: 'muted small' }, t(`coex.${id}.desc`)));
        return [
          h('h1', null, t('coex.title')),
          ctx.status.spark_modo_present ? h('p', { class: 'okmsg' }, t('coex.detected')) : null,
          modes.map(choice),
          h('p', { class: 'hint' }, t('coex.note')),
          nav(true),
        ];
      }
      case 'summary': {
        const err = h('div');
        const rows = [
          [t('wallet.label'), cfg.miner.wallet],
          [t('worker.label'), cfg.miner.worker],
          ...cfg.pools.map((p, i) => [t('pools.slot', { n: i + 1 }), `${p.name ? `${p.name} — ` : ''}${p.host}:${p.port} (TLS ${p.tls})${p.enabled === false ? ` — ${t('pools.disabled')}` : ''}`]),
          [t('power.title'), t(`power.${cfg.power.profile}`)],
          [t('coex.title'), t(`coex.${cfg.coexistence.mode}`)],
          [t('fee.title'), fee ? fee.banner : '2.00%'],
        ];
        async function start() {
          cfg.miner.disclosure_accepted = true;
          cfg.gui.language = getLang();
          replace(err);
          try {
            await api.put('/api/v1/config', cfg);
            await api.post('/api/v1/mining/start');
            ctx.toast(t('wizard.started'), 'ok');
            resetWizard();
            await ctx.refresh();
            ctx.go('dashboard');
          } catch (e) {
            const fields = (e.data && e.data.fields) || [];
            replace(err, h('div', { class: 'err' }, e.message), fields.map((f) => h('div', { class: 'err' }, `${f.path}: ${f.message}`)));
          }
        }
        return [
          h('h1', null, t('summary.title')),
          h('table', null, h('tbody', null, rows.map(([k, v]) => h('tr', null, h('th', null, k), h('td', { class: 'break' }, v))))),
          err,
          nav(true, t('summary.start'), start),
        ];
      }
      default:
        return null;
    }
  }

  function draw() {
    replace(main, h('div', { class: 'wizard' },
      h('div', { class: 'progress' }, STEPS.map((_, i) => h('span', { class: i <= d.step ? 'done' : '' }))),
      h('div', { class: 'card stack' }, body())));
  }
  draw();
}
