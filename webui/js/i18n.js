// English / Português (Brasil). Auto-detected from the browser, toggled in the header, remembered
// per browser. Keys are identical in both files (a test checks it).

let dict = {};
let fallback = {};
let lang = 'en';
const listeners = [];

export const LANGS = ['en', 'pt-BR'];

export function detect(configured) {
  let saved = null;
  try { saved = localStorage.getItem('spm.lang'); } catch (_) { /* storage disabled */ }
  if (saved && LANGS.includes(saved)) return saved;
  if (configured && LANGS.includes(configured)) return configured;
  const nav = (navigator.languages && navigator.languages[0]) || navigator.language || 'en';
  return nav.toLowerCase().startsWith('pt') ? 'pt-BR' : 'en';
}

async function load(l) {
  const r = await fetch(`i18n/${l}.json`, { credentials: 'same-origin' });
  if (!r.ok) throw new Error(`i18n ${l}: ${r.status}`);
  return r.json();
}

export async function setLang(l, remember = true) {
  if (!LANGS.includes(l)) l = 'en';
  if (!Object.keys(fallback).length) fallback = await load('en');
  dict = l === 'en' ? fallback : await load(l);
  lang = l;
  document.documentElement.lang = l;
  if (remember) {
    try { localStorage.setItem('spm.lang', l); } catch (_) { /* ignore */ }
  }
  for (const f of listeners) f(l);
}

export function getLang() {
  return lang;
}

export function onLang(f) {
  listeners.push(f);
}

/** Translate `key`, replacing `{name}` placeholders from `vars`. Unknown keys show the key. */
export function t(key, vars) {
  let s = dict[key] ?? fallback[key] ?? key;
  if (vars) for (const [k, v] of Object.entries(vars)) s = s.split(`{${k}}`).join(String(v));
  return s;
}

/** `t()` if the key exists, else `null`. */
export function tt(key, vars) {
  return (key in dict || key in fallback) ? t(key, vars) : null;
}
