import type { ReactNode } from "react";
import type { ParseError } from "./parse";
import { SLOTS, type Slot } from "./url";

export const ROW_LABEL: Record<Slot, string> = { filter: "This run", trace: "Its root", tree: "Any run in its trace" };

/** An error in one slot's text; offsets are UTF-16 indices into `text`. */
export interface SlotError extends ParseError { text: string }

export function TextEditor({ drafts, errors, onDrafts, onRun, extra }: {
  drafts: Record<Slot, string>;
  errors: Partial<Record<Slot, SlotError>>;
  onDrafts: (d: Record<Slot, string>) => void;
  onRun: () => void;
  extra?: Partial<Record<Slot, ReactNode>>;
}) {
  return (
    <div className="text-editor">
      {SLOTS.map((s) => {
        const err = errors[s]?.text === drafts[s] ? errors[s] : undefined;
        return (
          <div key={s} className="where">
            <label className="where-label" htmlFor={`slot-${s}`}>{ROW_LABEL[s]}</label>
            <div className="slot">
              <textarea id={`slot-${s}`} aria-label={`${ROW_LABEL[s]} filter`} rows={1} spellCheck={false}
                className={err ? "bad" : undefined} value={drafts[s]} placeholder='e.g. and(eq(status, "error"), gt(latency, 30))'
                onChange={(e) => onDrafts({ ...drafts, [s]: e.target.value })}
                onKeyDown={(e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); onRun(); } }} />
              {err && (
                <>
                  <div className="err-mirror" aria-hidden>
                    {err.text.slice(0, err.start)}<mark>{err.text.slice(err.start, err.end) || " "}</mark>{err.text.slice(err.end)}
                  </div>
                  <div className="err-msg">{err.message}</div>
                </>
              )}
            </div>
            {extra?.[s]}
          </div>
        );
      })}
      <button className="run" onClick={onRun}>Run</button>
    </div>
  );
}
