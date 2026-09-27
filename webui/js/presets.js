// Pool presets. `verified`: probed live on the DGX Spark with accepted shares and no rejects; the
// others are known endpoints of the same pools that the project has not tested yet.

const LUCKY_PIN = 'd0ehDQxaU5IUv4UHWXItQKqdJ8anqZclQXcoIjwF/mk=';

function hero(region, label, verified = false) {
  return { id: `hero-${region}`, name: `HeroMiners ${label}`, host: `${region}.pearl.herominers.com`, port: 1200, tls: 'auto', dialect: 'auto', jsonrpc: 'auto', verified };
}

export const PRESETS = [
  { id: 'kryptex', name: 'Kryptex', host: 'prl-br.kryptex.network', port: 8048, tls: 'on', dialect: 'kryptex', jsonrpc: 'auto', verified: true },
  hero('br', 'BR', true),
  { id: 'lucky-br', name: 'LuckyPool BR', host: 'pearl-br.luckypool.io', port: 3360, tls: 'pinned', spki_pin: LUCKY_PIN, dialect: 'object', jsonrpc: 'on', verified: true },
  hero('us', 'US'),
  hero('us2', 'US2'),
  hero('de', 'DE'),
  hero('fr', 'FR'),
  { id: 'lucky-eu', name: 'LuckyPool EU', host: 'pearl-eu1.luckypool.io', port: 3360, tls: 'pinned', spki_pin: LUCKY_PIN, dialect: 'object', jsonrpc: 'on', verified: false },
];

/** A pool entry (config schema) from a preset. */
export function fromPreset(p) {
  return {
    name: p.name,
    host: p.host,
    port: p.port,
    tls: p.tls,
    spki_pin: p.spki_pin || '',
    dialect: p.dialect || 'auto',
    jsonrpc: p.jsonrpc || 'auto',
    proof: 'auto',
    password: 'x',
    pattern: 'auto',
    enabled: true,
  };
}

/** The preset a pool entry matches (same host and port), if any. */
export function presetOf(pool) {
  return PRESETS.find((p) => p.host === (pool.host || '').trim().toLowerCase() && p.port === Number(pool.port)) || null;
}

/** The default three slots: Kryptex → HeroMiners BR → LuckyPool BR (same as config.rs). */
export function defaultPools() {
  return ['kryptex', 'hero-br', 'lucky-br'].map((id) => fromPreset(PRESETS.find((p) => p.id === id)));
}

/** A pool typed by hand: every hidden setting on its default. */
export function customPool(host, port) {
  return { name: host, host, port, tls: 'auto', spki_pin: '', dialect: 'auto', jsonrpc: 'auto', proof: 'auto', password: 'x', pattern: 'auto', enabled: true };
}
