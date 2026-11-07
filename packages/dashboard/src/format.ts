export function relTime(input?: string | number | null): string {
  if (input === undefined || input === null || input === '') return '—';
  const t = typeof input === 'number' ? input : Date.parse(input);
  if (Number.isNaN(t)) return '—';
  const diff = Date.now() - t;
  const future = diff < 0;
  const s = Math.abs(diff) / 1000;
  const fmt = (n: number, unit: string) => {
    const v = Math.floor(n);
    const text = `${v} ${unit}${v === 1 ? '' : 's'}`;
    return future ? `in ${text}` : `${text} ago`;
  };
  if (s < 60) return future ? 'in a moment' : 'just now';
  if (s < 3600) return fmt(s / 60, 'minute');
  if (s < 86400) return fmt(s / 3600, 'hour');
  if (s < 86400 * 30) return fmt(s / 86400, 'day');
  if (s < 86400 * 365) return fmt(s / (86400 * 30), 'month');
  return fmt(s / (86400 * 365), 'year');
}

export function absTime(input?: string | number | null): string {
  if (input === undefined || input === null || input === '') return '';
  const t = typeof input === 'number' ? input : Date.parse(input);
  return Number.isNaN(t) ? '' : new Date(t).toLocaleString();
}

export function duration(ms: number): string {
  if (ms < 1000) return '<1s';
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  const h = Math.floor(m / 60);
  if (h < 48) return `${h}h ${m % 60}m`;
  return `${Math.floor(h / 24)}d ${h % 24}h`;
}

/** A schedule interval in words: "6 hours", "1 day", "90 minutes". */
export function every(ms: number): string {
  const min = Math.round(ms / 60000);
  const plural = (n: number, u: string) => `${n} ${u}${n === 1 ? '' : 's'}`;
  if (min % 1440 === 0) return plural(min / 1440, 'day');
  if (min % 60 === 0) return plural(min / 60, 'hour');
  return plural(min, 'minute');
}

export const shortId = (id?: string) => (id && id.length > 12 ? `${id.slice(0, 6)}…${id.slice(-4)}` : id ?? '');

export const sum = (m?: Record<string, number>) => Object.values(m ?? {}).reduce((a, b) => a + b, 0);

export const humanize = (s: string) => s.replace(/[_-]+/g, ' ').replace(/^\w/, (c) => c.toUpperCase());

export function preview(text: string, max = 140): string {
  const t = text.replace(/\s+/g, ' ').trim();
  return t.length > max ? `${t.slice(0, max - 1)}…` : t;
}

/** Last path segment of a project root, for display. */
export function baseName(p: string): string {
  const parts = p.split(/[\\/]+/).filter(Boolean);
  return parts[parts.length - 1] ?? p;
}
