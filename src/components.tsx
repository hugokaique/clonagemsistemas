import type { ReactNode } from "react";
import type { CloneProgress, DiskInfo, HistoryEntry } from "./types";
import { KIND_LABEL, PHASE_LABEL, fmtDate, fmtDuration, fmtETA, fmtSize, isTerminal } from "./format";

export function Badge({ tone, children }: { tone: "red" | "green" | "amber" | "slate"; children: ReactNode }) {
  const tones = {
    red: "bg-red-500/20 text-red-300",
    green: "bg-emerald-500/20 text-emerald-300",
    amber: "bg-amber-500/20 text-amber-300",
    slate: "bg-slate-700 text-slate-300",
  };
  return <span className={`px-1.5 py-0.5 rounded ${tones[tone]}`}>{children}</span>;
}

export function DiskCard({
  disk,
  selected,
  disabled,
  blockedReason,
  onSelect,
}: {
  disk: DiskInfo;
  selected: boolean;
  disabled: boolean;
  /** Motivo exibido quando o disco não pode ser escolhido neste papel */
  blockedReason?: string | null;
  onSelect: () => void;
}) {
  const mounted = disk.mounts.filter((m) => !m.mountPoint.startsWith("["));
  return (
    <button
      type="button"
      onClick={onSelect}
      disabled={disabled}
      aria-pressed={selected}
      className={`w-full text-left rounded-xl border p-3 transition
        ${selected ? "border-blue-500 bg-blue-500/10" : "border-slate-700 bg-slate-900 hover:border-slate-500"}
        ${disabled ? "opacity-40 cursor-not-allowed" : ""}`}
    >
      <div className="flex items-center justify-between gap-2">
        <span className="font-medium truncate">{disk.model}</span>
        <span className="text-xs text-slate-400 whitespace-nowrap">
          {fmtSize(disk.sizeBytes)} · {disk.bus}
        </span>
      </div>
      <div className="text-xs text-slate-400 font-mono truncate">{disk.path}</div>
      <div className="mt-1 flex flex-wrap gap-2 text-[11px]">
        {disk.isSystem && <Badge tone="red">DISCO DO SISTEMA — bloqueado como destino</Badge>}
        {disk.isReadOnly && <Badge tone="amber">somente leitura</Badge>}
        {disk.isRemovable && <Badge tone="green">removível</Badge>}
        <Badge tone="slate">
          {disk.partitions.length} {disk.partitions.length === 1 ? "partição" : "partições"}
        </Badge>
        {mounted.length > 0 && (
          <Badge tone="slate">em uso: {mounted.map((m) => m.mountPoint).join(", ")}</Badge>
        )}
        {blockedReason && <Badge tone="amber">{blockedReason}</Badge>}
      </div>
    </button>
  );
}

export function ProgressPanel({
  progress,
  log,
  cancelling,
  onCancel,
}: {
  progress: CloneProgress;
  log: string[];
  cancelling: boolean;
  onCancel: () => void;
}) {
  const running = !isTerminal(progress.phase);
  const indeterminate = running && progress.totalBytes === 0;
  const pct = Math.min(100, Math.max(0, progress.percent));
  const bar =
    progress.phase === "error"
      ? "bg-red-500"
      : progress.phase === "cancelled"
      ? "bg-amber-500"
      : progress.phase === "done"
      ? "bg-emerald-500"
      : "bg-blue-500";
  return (
    <div className="space-y-1.5" role="status" aria-live="polite">
      <div className="flex justify-between gap-3 text-sm">
        <span>{cancelling && running ? "Cancelando…" : PHASE_LABEL[progress.phase] ?? progress.phase}</span>
        <span className="font-mono text-right">
          {indeterminate ? (
            "em andamento…"
          ) : (
            <>
              {pct.toFixed(1)}% · {fmtSize(progress.bytesDone)} / {fmtSize(progress.totalBytes)}
              {running && (
                <>
                  {" "}
                  · {progress.speedMbps.toFixed(0)} MB/s · ETA {fmtETA(progress.etaSeconds)}
                </>
              )}
            </>
          )}
        </span>
      </div>
      <div className="flex items-center gap-3">
        <div
          className="h-2.5 flex-1 rounded-full bg-slate-800 overflow-hidden"
          role="progressbar"
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={indeterminate ? undefined : Math.round(pct)}
        >
          <div
            className={`h-full transition-all ${bar} ${indeterminate ? "animate-pulse" : ""}`}
            style={{ width: indeterminate ? "100%" : `${pct}%` }}
          />
        </div>
        {running && (
          <button
            type="button"
            onClick={onCancel}
            disabled={cancelling}
            className="text-xs px-3 py-1 rounded-lg border border-red-500/50 text-red-200 hover:bg-red-500/10 disabled:opacity-50"
          >
            {cancelling ? "Cancelando…" : "Cancelar"}
          </button>
        )}
      </div>
      {log.length > 0 && (
        <ul className="text-xs text-slate-400 font-mono space-y-0.5 max-h-28 overflow-y-auto">
          {log.map((line, i) => (
            <li key={i} className="break-all">
              {line}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function StatusCell({ h }: { h: HistoryEntry }) {
  switch (h.status) {
    case "ok":
      return (
        <span className="text-emerald-400">
          ✔ ok{h.verifyOk ? " (verificado)" : h.verifyOk === false ? " (sem verificação)" : ""}
        </span>
      );
    case "running":
      return <span className="text-blue-300">⏳ em andamento</span>;
    case "cancelled":
      return <span className="text-amber-300">⏹ cancelado</span>;
    case "interrupted":
      return (
        <span className="text-amber-300" title={h.message ?? ""}>
          ⚠ interrompido
        </span>
      );
    default:
      return (
        <span className="text-red-400" title={h.message ?? ""}>
          ✖ {h.status === "error" ? "erro" : h.status}
        </span>
      );
  }
}

export function HistoryTable({ history }: { history: HistoryEntry[] }) {
  return (
    <div className="rounded-xl border border-slate-800 overflow-x-auto">
      <table className="w-full text-xs">
        <thead className="bg-slate-900 text-slate-400">
          <tr>
            <th className="text-left px-3 py-2">Início</th>
            <th className="text-left px-3 py-2">Operação</th>
            <th className="text-left px-3 py-2">Origem → Destino</th>
            <th className="text-right px-3 py-2">Dados</th>
            <th className="text-right px-3 py-2">Duração</th>
            <th className="text-left px-3 py-2">Hash</th>
            <th className="text-left px-3 py-2">Status</th>
          </tr>
        </thead>
        <tbody>
          {history.length === 0 && (
            <tr>
              <td colSpan={7} className="px-3 py-4 text-center text-slate-500">
                Nenhuma operação registrada ainda.
              </td>
            </tr>
          )}
          {history.map((h) => (
            <tr key={h.id} className="border-t border-slate-800">
              <td className="px-3 py-2 whitespace-nowrap">{fmtDate(h.startedAt)}</td>
              <td className="px-3 py-2 whitespace-nowrap">{KIND_LABEL[h.kind] ?? h.kind}</td>
              <td className="px-3 py-2 font-mono truncate max-w-[260px]" title={`${h.source} → ${h.target}`}>
                {h.source} → {h.target}
              </td>
              <td className="px-3 py-2 text-right whitespace-nowrap">
                {h.bytesWritten > 0 ? fmtSize(h.bytesWritten) : "—"}
              </td>
              <td className="px-3 py-2 text-right whitespace-nowrap">{fmtDuration(h.startedAt, h.finishedAt)}</td>
              <td className="px-3 py-2 font-mono truncate max-w-[160px]" title={h.hashHex ?? ""}>
                {h.hashHex ? `${h.hashAlgo}:${h.hashHex.slice(0, 12)}…` : "—"}
              </td>
              <td className="px-3 py-2 whitespace-nowrap">
                <StatusCell h={h} />
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
