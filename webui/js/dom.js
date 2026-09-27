// Tiny DOM helpers. Everything is built with createElement/textContent: strings that come from
// pools or the network are always rendered as text, never parsed as HTML.

const PROPS = new Set(['value', 'checked', 'disabled', 'selected', 'open', 'hidden']);

export function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === null || v === undefined || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2), v);
    else if (PROPS.has(k)) el[k] = v;
    else el.setAttribute(k, v === true ? '' : String(v));
  }
  append(el, children);
  return el;
}

export function append(el, children) {
  for (const c of [children].flat(Infinity)) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

export function clear(el) {
  while (el.firstChild) el.firstChild.remove();
  return el;
}

export function replace(el, ...children) {
  clear(el);
  return append(el, children);
}

/** Rebuild `el` only when `sig` changed: live data redraws once per second, and a button that
 * is replaced between mousedown and mouseup never receives its click. */
export function keyed(el, sig, build) {
  if (el.dataset.sig === sig) return el;
  el.dataset.sig = sig;
  return replace(el, build());
}

export function fmtTime(ms) {
  if (!ms) return '';
  const d = new Date(ms);
  return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
}

export function fmtDuration(s) {
  s = Math.max(0, Math.floor(s || 0));
  const d = Math.floor(s / 86400), hh = Math.floor((s % 86400) / 3600), mm = Math.floor((s % 3600) / 60), ss = s % 60;
  if (d) return `${d}d ${hh}h ${mm}m`;
  if (hh) return `${hh}h ${mm}m`;
  if (mm) return `${mm}m ${ss}s`;
  return `${ss}s`;
}

export function fmtNum(x, digits = 2) {
  if (x === null || x === undefined || Number.isNaN(x)) return '—';
  return Number(x).toLocaleString(document.documentElement.lang || undefined, { maximumFractionDigits: digits, minimumFractionDigits: digits });
}

/** Credited MAC/s given in tera, with a unit that keeps three significant digits. */
export function fmtRate(tmacs) {
  const v = Number(tmacs) || 0;
  const units = [[1, 'T'], [1e-3, 'G'], [1e-6, 'M'], [1e-9, 'k']];
  for (const [scale, u] of units) {
    if (v >= scale) return `${fmtNum(v / scale, 2)} ${u}-MAC/s`;
  }
  return `${fmtNum(0, 2)} T-MAC/s`;
}

/** `prl1pxxxx…yyyy`: the first 9 and the last 4 characters, to compare with the wallet app. */
export function abbrev(w) {
  if (!w || w.length <= 16) return w || '';
  return `${w.slice(0, 9)}…${w.slice(-4)}`;
}

export function download(name, text) {
  const blob = new Blob([text], { type: 'application/json' });
  const url = URL.createObjectURL(blob);
  const a = h('a', { href: url, download: name });
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

/** Copy `text` to the clipboard (the async API, or a hidden textarea on plain http origins). */
export async function copyText(text) {
  try {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch (_) { /* fall back below */ }
  const ta = h('textarea', { class: 'offscreen', readonly: true });
  ta.value = text;
  document.body.append(ta);
  ta.select();
  let ok = false;
  try { ok = document.execCommand('copy'); } catch (_) { ok = false; }
  ta.remove();
  return ok;
}

/**
 * A modal dialog. `build(close)` returns the dialog's children. Escape and a click on the
 * backdrop close it (unless `sticky`). Returns { el, close }.
 */
export function modal(label, build, { sticky = false, onClose } = {}) {
  const prev = document.activeElement;
  const box = h('div', { class: 'modal', role: 'dialog', 'aria-modal': 'true', 'aria-label': label });
  const backdrop = h('div', { class: 'backdrop' }, box);
  let open = true;
  const close = () => {
    if (!open) return;
    open = false;
    backdrop.remove();
    document.removeEventListener('keydown', onKey);
    if (onClose) onClose();
    if (prev && prev.focus) prev.focus();
  };
  const onKey = (ev) => { if (ev.key === 'Escape' && !sticky) close(); };
  backdrop.addEventListener('mousedown', (ev) => { if (ev.target === backdrop && !sticky) close(); });
  document.addEventListener('keydown', onKey);
  append(box, build(close));
  document.body.append(backdrop);
  const first = box.querySelector('input, select, button.primary, button');
  if (first) first.focus();
  return { el: box, close };
}

/** Ask a yes/no question in a modal; resolves true on the confirm button. */
export function confirmBox(text, yesLabel, noLabel, { danger = false } = {}) {
  return new Promise((resolve) => {
    let answer = false;
    modal(text, (close) => [
      h('p', { class: 'confirm-text' }, text),
      h('div', { class: 'row end' },
        h('button', { type: 'button', onclick: () => close() }, noLabel),
        h('button', { type: 'button', class: danger ? 'danger solid' : 'primary', onclick: () => { answer = true; close(); } }, yesLabel)),
    ], { onClose: () => resolve(answer) });
  });
}
