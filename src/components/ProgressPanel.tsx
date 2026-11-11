import type { CloneProgress, Tab } from "../types";
import { isTerminalPhase } from "../types";
import { fmtETA, OP_LABEL, PHASE_LABEL } from "../format";

export default function ProgressPanel({
  op,
  progress,
  cancelling,
  onCancel,
}: {
  op: Tab;
  progress: CloneProgress;
  cancelling: boolean;
  onCancel: () => void;
}) {
  const running = !isTerminalPhase(progress.phase);
  const pct = Number.isFinite(progress.percent) ? Math.min(100, Math.max(0, progress.percent)) : 0;
  const isError = progress.phase === "error";
  const isCancelled = progress.phase === "cancelled";
  const bar = isError ? "bg-red-500" : isCancelled ? "bg-amber-500" : progress.phase === "done" ? "bg-emerald-500" : "bg-blue-500";
  const box = isError
    ? "border-red-500/40 bg-red-500/10"
    : isCancelled
    ? "border-amber-500/40 bg-amber-500/10"
    : "border-slate-800 bg-slate-950";
  const speed = Number.isFinite(progress.speedMbps) ? progress.speedMbps : 0;

  return (
    <div className={`rounded-xl border px-3 py-2.5 space-y-1.5 ${box}`}>
      <div className="text-xs font-semibold uppercase tracking-wide text-slate-400">{OP_LABEL[op]}</div>
      <div className="flex justify-between gap-2 text-sm">
        <span>{PHASE_LABEL[progress.phase] ?? progress.phase}</span>
        <span className="font-mono">
          {pct.toFixed(1)}% · {speed.toFixed(0)} MiB/s · ETA {fmtETA(progress.etaSeconds)}
        </span>
      </div>
      <div
        role="progressbar"
        aria-label={`Progresso — ${OP_LABEL[op]}`}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={Math.round(pct)}
        className="h-2.5 rounded-full bg-slate-800 overflow-hidden"
      >
        <div className={`h-full transition-all ${bar}`} style={{ width: `${pct}%` }} />
      </div>
      {progress.message &&
        (isError ? (
          <p role="alert" className="text-xs text-red-200 font-mono break-all">{progress.message}</p>
        ) : (
          <p className={`text-xs font-mono break-all ${isCancelled ? "text-amber-200" : "text-slate-400"}`}>
            {progress.message}
          </p>
        ))}
      {running && (
        <button
          type="button"
          onClick={onCancel}
          disabled={cancelling}
          className="px-3 py-1.5 rounded-lg text-sm bg-slate-800 hover:bg-slate-700 disabled:text-slate-500"
        >
          {cancelling ? "Cancelando…" : "Cancelar"}
        </button>
      )}
    </div>
  );
}
