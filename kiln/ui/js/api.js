// The console is a plain client of kiln's API: the same endpoints cu uses,
// authenticated by the session cookie instead of a token.

export class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

/** Kiln refuses cookie-authenticated writes without this header (CSRF). */
const CSRF = { 'x-copper': '1' };

export async function api(method, path, body) {
  const headers = method === 'GET' ? {} : { ...CSRF };
  const init = { method, headers, credentials: 'same-origin' };
  if (body !== undefined) {
    headers['content-type'] = 'application/json';
    init.body = JSON.stringify(body);
  }
  const resp = await fetch(path, init);
  const text = await resp.text();
  let data = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    data = null;
  }
  if (resp.status === 401 && !path.startsWith('/v1/session') && !path.startsWith('/v1/invites')) {
    location.hash = '#/signin';
  }
  if (!resp.ok) throw new ApiError(resp.status, data?.message ?? (text || resp.statusText));
  return data;
}

export const get = (path) => api('GET', path);
export const post = (path, body) => api('POST', path, body);
export const put = (path, body) => api('PUT', path, body);
export const del = (path) => api('DELETE', path);

/** Encode one URL path segment, e.g. a project path like `sam:games`. */
export const enc = encodeURIComponent;
