// REST client. The session cookie is HttpOnly (the page never sees it); every mutation carries
// the CSRF value returned by the login in the X-SPM-CSRF header.

let csrf = null;
let onUnauthorized = () => {};

export function setUnauthorizedHandler(f) {
  onUnauthorized = f;
}

export class ApiError extends Error {
  constructor(message, status, data) {
    super(message);
    this.status = status;
    this.data = data;
  }
}

async function call(method, path, body) {
  const opts = { method, credentials: 'same-origin', headers: {}, cache: 'no-store' };
  if (body !== undefined) {
    opts.headers['Content-Type'] = 'application/json';
    opts.body = JSON.stringify(body);
  }
  if (method !== 'GET' && csrf) opts.headers['X-SPM-CSRF'] = csrf;
  const r = await fetch(path, opts);
  let data = null;
  try { data = await r.json(); } catch (_) { data = null; }
  if (r.status === 401 && path !== '/api/v1/session') onUnauthorized();
  if (!r.ok) throw new ApiError((data && (data.message || data.error)) || r.statusText, r.status, data);
  return data;
}

export const get = (p) => call('GET', p);
export const post = (p, b) => call('POST', p, b === undefined ? {} : b);
export const put = (p, b) => call('PUT', p, b);

/** Exchange the API token for a session. */
export async function login(token) {
  const d = await call('POST', '/api/v1/session', { token });
  csrf = d.csrf;
  return true;
}

/** Re-use an existing session cookie (page reload). */
export async function resume() {
  try {
    const d = await call('GET', '/api/v1/session');
    csrf = d.csrf;
    return true;
  } catch (_) {
    return false;
  }
}

export async function logout() {
  try { await call('DELETE', '/api/v1/session'); } catch (_) { /* ignore */ }
  csrf = null;
}
