import type { Match } from "../api";

/** The match's excerpt with `start..end` marked; offsets count code points, not UTF-16 units. */
export function Excerpt({ m }: { m: Match }) {
  const cs = Array.from(m.excerpt);
  return (
    <>
      <span className="match-path">{m.path}</span>
      {cs.slice(0, m.start).join("")}<mark>{cs.slice(m.start, m.end).join("")}</mark>{cs.slice(m.end).join("")}
    </>
  );
}
