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

export function fmtTime(ms) {
  if (!ms) return '';
  const d = new Date(ms);
  return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
}

export function fmtDateTime(ms) {
  if (!ms) return '';
  return new Date(ms).toLocaleString();
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
  return Number(x).toLocaleString(undefined, { maximumFractionDigits: digits, minimumFractionDigits: digits });
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

export function abbrev(w) {
  if (!w || w.length <= 16) return w || '';
  return `${w.slice(0, 8)}…${w.slice(-4)}`;
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
