export function fmtLatency(ms: number | null): string {
  if (ms == null) return "—";
  if (ms < 1000) return `${ms} ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(s < 10 ? 2 : 1)} s`;
  const m = Math.floor(s / 60);
  return `${m}m ${Math.round(s % 60)}s`;
}

export function fmtTime(iso: string | null): string {
  if (!iso) return "—";
  const d = new Date(iso);
  return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

export function fmtTokens(n: number | null): string {
  return n == null ? "—" : n.toLocaleString();
}

export function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

/** How long ago, roughly: "just now", "5 min ago", "3 h ago", "2 days ago". */
export function fmtAgo(iso: string, now: number): string {
  const s = Math.max(0, (now - new Date(iso).getTime()) / 1000);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`;
  const d = Math.floor(s / 86400);
  return `${d} day${d === 1 ? "" : "s"} ago`;
}

export function elapsedSince(iso: string | null, now: number): number | null {
  return iso ? Math.max(0, now - new Date(iso).getTime()) : null;
}

/** Picodollars as dollars, rounded half up to the cent; null (no price) is a dash, never $0. */
export function fmtCost(pico: number | null): string {
  if (pico == null) return "—";
  const cents = Math.floor(pico / 1e10 + 0.5);
  return `$${Math.floor(cents / 100).toLocaleString("en-US")}.${String(cents % 100).padStart(2, "0")}`;
}

export function fmtPct(r: number | null): string {
  return r == null ? "—" : `${(r * 100).toFixed(1)}%`;
}

export const plural = (n: number, one: string, many = `${one}s`) => `${n.toLocaleString()} ${n === 1 ? one : many}`;
