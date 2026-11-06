import { useEffect, useState } from "react";

/** All view state lives in the URL, so every view is shareable. */
export function useLocation(): URL {
  const [url, setUrl] = useState(() => new URL(window.location.href));
  useEffect(() => {
    const on = () => setUrl(new URL(window.location.href));
    window.addEventListener("popstate", on);
    return () => window.removeEventListener("popstate", on);
  }, []);
  return url;
}

export function navigate(to: string, replace = false) {
  if (replace) history.replaceState(null, "", to);
  else history.pushState(null, "", to);
  window.dispatchEvent(new PopStateEvent("popstate"));
}

export function withParams(base: string, params: Record<string, string | undefined>): string {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) if (v) q.set(k, v);
  const s = q.toString();
  return s ? `${base}?${s}` : base;
}
