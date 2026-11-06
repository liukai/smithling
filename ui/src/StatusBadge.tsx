import type { Status } from "./api";

const LABEL: Record<Status, string> = { running: "● run", ok: "✓ ok", error: "✕ err" };

export function StatusBadge({ status }: { status: Status }) {
  return <span className={`status status-${status}`}>{LABEL[status]}</span>;
}
