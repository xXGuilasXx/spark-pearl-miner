// Pool presets. Endpoints marked `unverified` were not probed by the project yet.

const LUCKY_PIN = 'd0ehDQxaU5IUv4UHWXItQKqdJ8anqZclQXcoIjwF/mk=';

function hero(region, label) {
  return { id: `hero-${region}`, name: `HeroMiners ${label}`, host: `${region}.pearl.herominers.com`, port: 1200, tls: 'auto', dialect: 'auto', jsonrpc: 'auto' };
}

export const PRESETS = [
  hero('br', 'BR'),
  hero('us', 'US'),
  hero('us2', 'US2'),
  hero('de', 'DE'),
  hero('fr', 'FR'),
  { id: 'lucky-br', name: 'LuckyPool BR', host: 'pearl-br.luckypool.io', port: 3360, tls: 'pinned', spki_pin: LUCKY_PIN, dialect: 'object', jsonrpc: 'on' },
  { id: 'lucky-eu', name: 'LuckyPool EU', host: 'pearl-eu1.luckypool.io', port: 3360, tls: 'pinned', spki_pin: LUCKY_PIN, dialect: 'object', jsonrpc: 'on', unverified: true },
  { id: 'kryptex', name: 'Kryptex', host: 'prl.kryptex.network', port: 8048, tls: 'on', dialect: 'kryptex', jsonrpc: 'auto' },
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

/** The default three slots: HeroMiners BR → LuckyPool BR → Kryptex. */
export function defaultPools() {
  return ['hero-br', 'lucky-br', 'kryptex'].map((id) => fromPreset(PRESETS.find((p) => p.id === id)));
}

export function customPool() {
  return { name: '', host: '', port: 0, tls: 'auto', spki_pin: '', dialect: 'auto', jsonrpc: 'auto', proof: 'auto', password: 'x', pattern: 'auto', enabled: true };
}
