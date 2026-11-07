import { type FormEvent, useEffect, useState } from "react";
import { parse } from "./parse";
import { print } from "./print";
import { boxSearch } from "./url";

const unquote = (t: string) => (t.length >= 2 && t.startsWith('"') && t.endsWith('"') ? t.slice(1, -1) : t);

/** One phrase over inputs, outputs and errors; Enter sets the filter's `search("…")` chip, empty removes it. */
export function SearchBox({ value, onSearch }: { value?: string; onSearch: (text?: string) => void }) {
  const [text, setText] = useState(value ?? "");
  const [error, setError] = useState<string | null>(null);
  useEffect(() => { setText(value ?? ""); setError(null); }, [value]);

  const submit = (e: FormEvent) => {
    e.preventDefault();
    const t = unquote(text.trim());
    if (!t) return onSearch(undefined);
    const r = parse(print(boxSearch(t)));
    if (!r.ok) return setError(r.error.message);
    setText(t);
    onSearch(t);
  };

  return (
    <form className="search-box" role="search" onSubmit={submit}>
      <input type="search" aria-label="search" placeholder="search inputs, outputs and errors" value={text}
        onChange={(e) => { setText(e.target.value); setError(null); }} />
      {error && <span role="alert" className="chip-error">{error}</span>}
    </form>
  );
}
