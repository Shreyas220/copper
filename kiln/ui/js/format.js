// Numbers and times the way the console says them: short, lowercase.

const now = () => Math.floor(Date.now() / 1000);

/** `40s`, `12m`, `3h`, `2d` since a unix time. */
export function ago(unix) {
  if (!unix) return '—';
  const s = Math.max(0, now() - unix);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}

/** `2d 4h`, `12m`: how long something has been going. */
export function since(unix) {
  if (!unix) return '—';
  const s = Math.max(0, now() - unix);
  const d = Math.floor(s / 86400), hr = Math.floor((s % 86400) / 3600), m = Math.floor((s % 3600) / 60);
  if (d) return hr ? `${d}d ${hr}h` : `${d}d`;
  if (hr) return m ? `${hr}h ${m}m` : `${hr}h`;
  return m ? `${m}m` : `${s}s`;
}

export function gb(mb) {
  const v = mb / 1024;
  return `${Number.isInteger(v) ? v : v.toFixed(1)} gb`;
}

export function bytes(n) {
  const units = ['b', 'kb', 'mb', 'gb', 'tb'];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
  return `${n >= 10 || i === 0 ? Math.round(n) : n.toFixed(1)} ${units[i]}`;
}

/** `2 vcpu · 2 gb · 20 gb` */
export function shape(i) {
  return `${i.vcpus} vcpu · ${gb(i.memory_mb)} · ${i.disk_gb} gb`;
}

export const percent = (permille) => `${Math.round(permille / 10)}%`;

export const plural = (n, one, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

/** States that want attention, and states that are on their way somewhere. */
export const NEEDS_LOOK = new Set(['failed', 'unknown']);
export const BUSY = new Set(['starting', 'stopping', 'deleting']);
