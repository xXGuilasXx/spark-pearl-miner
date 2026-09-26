// Pearl address check in the browser: bech32m (BIP-350), HRP "prl", witness version 1,
// 32-byte program (P2TR). The daemon checks again with the same rules before saving.

const CHARSET = 'qpzry9x8gf2tvdw0s3jn54khce6mua7l';
const GEN = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];
const BECH32M = 0x2bc830a3;

function polymod(values) {
  let chk = 1;
  for (const v of values) {
    const top = chk >>> 25;
    chk = ((chk & 0x1ffffff) << 5) ^ v;
    for (let i = 0; i < 5; i++) if ((top >>> i) & 1) chk ^= GEN[i];
  }
  return chk >>> 0;
}

function hrpExpand(hrp) {
  const out = [];
  for (let i = 0; i < hrp.length; i++) out.push(hrp.charCodeAt(i) >> 5);
  out.push(0);
  for (let i = 0; i < hrp.length; i++) out.push(hrp.charCodeAt(i) & 31);
  return out;
}

function convertBits(data, from, to) {
  let acc = 0, bits = 0;
  const out = [], maxv = (1 << to) - 1;
  for (const v of data) {
    acc = (acc << from) | v;
    bits += from;
    while (bits >= to) {
      bits -= to;
      out.push((acc >> bits) & maxv);
    }
  }
  if (bits >= from || ((acc << (to - bits)) & maxv)) return null;
  return out;
}

/**
 * Validate a Pearl mainnet address. Returns { ok: true } or { ok: false, reason } where reason is
 * one of: empty, mixed_case, too_long, format, charset, checksum, hrp, version, length.
 */
export function checkPearlAddress(input) {
  const addr = (input || '').trim();
  if (!addr) return { ok: false, reason: 'empty' };
  if (addr !== addr.toLowerCase() && addr !== addr.toUpperCase()) return { ok: false, reason: 'mixed_case' };
  if (addr.length > 90) return { ok: false, reason: 'too_long' };
  const a = addr.toLowerCase();
  const pos = a.lastIndexOf('1');
  if (pos < 1 || pos + 7 > a.length) return { ok: false, reason: 'format' };
  const hrp = a.slice(0, pos);
  const data = [];
  for (const c of a.slice(pos + 1)) {
    const d = CHARSET.indexOf(c);
    if (d < 0) return { ok: false, reason: 'charset' };
    data.push(d);
  }
  if (hrp !== 'prl') return { ok: false, reason: 'hrp' };
  if (polymod(hrpExpand(hrp).concat(data)) !== BECH32M) return { ok: false, reason: 'checksum' };
  const values = data.slice(0, -6);
  if (values[0] !== 1) return { ok: false, reason: 'version' };
  const prog = convertBits(values.slice(1), 5, 8);
  if (!prog || prog.length !== 32) return { ok: false, reason: 'length' };
  return { ok: true };
}
