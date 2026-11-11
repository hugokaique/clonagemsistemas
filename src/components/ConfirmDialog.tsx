import { useEffect, useId, useRef, useState } from "react";
import type { DiskInfo } from "../types";
import { fmtSize } from "../format";

export interface ConfirmDiskItem {
  role: string; // ex.: "Origem (somente leitura)" / "Destino (será apagado)"
  disk: DiskInfo;
  destructive: boolean;
}
export interface ConfirmRequest {
  title: string;
  operation: string;
  details?: [string, string][];
  disks?: ConfirmDiskItem[];
  warning: string;
  ackLabel: string;
  confirmLabel: string;
  onConfirm: () => void;
}

export default function ConfirmDialog({ req, onCancel }: { req: ConfirmRequest; onCancel: () => void }) {
  const [ack, setAck] = useState(false);
  const cancelRef = useRef<HTMLButtonElement>(null);
  const boxRef = useRef<HTMLDivElement>(null);
  const titleId = useId();

  useEffect(() => {
    const prev = document.activeElement as HTMLElement | null;
    cancelRef.current?.focus();
    return () => prev?.focus?.();
  }, []);

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      onCancel();
    } else if (e.key === "Enter") {
      // Enter nunca confirma; só ativa o botão focado (Cancelar/Confirmar) por clique explícito.
      const t = e.target as HTMLElement;
      if (t.tagName !== "BUTTON") e.preventDefault();
    } else if (e.key === "Tab" && boxRef.current) {
      const els = Array.from(
        boxRef.current.querySelectorAll<HTMLElement>("button:not([disabled]), input:not([disabled])")
      );
      if (els.length === 0) return;
      const first = els[0];
      const last = els[els.length - 1];
      if (e.shiftKey && document.activeElement === first) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && document.activeElement === last) {
        e.preventDefault();
        first.focus();
      }
    }
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/70 p-4" onKeyDown={onKeyDown}>
      <div
        ref={boxRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        className="w-full max-w-2xl max-h-[90vh] overflow-y-auto rounded-2xl border border-red-500/40 bg-slate-900 p-5 space-y-4 shadow-2xl"
      >
        <h2 id={titleId} className="text-lg font-bold text-red-300">{req.title}</h2>
        <p className="text-sm text-slate-300">{req.operation}</p>

        {req.details && req.details.length > 0 && (
          <dl className="text-sm grid grid-cols-[auto_1fr] gap-x-3 gap-y-1">
            {req.details.map(([k, v]) => (
              <div key={k} className="contents">
                <dt className="text-slate-400">{k}</dt>
                <dd className="font-mono break-all">{v}</dd>
              </div>
            ))}
          </dl>
        )}

        {req.disks?.map(({ role, disk, destructive }) => (
          <div
            key={`${role}-${disk.path}`}
            className={`rounded-xl border p-3 space-y-1.5 ${destructive ? "border-red-500/50 bg-red-500/10" : "border-slate-700 bg-slate-950"}`}
          >
            <div className={`text-xs font-semibold uppercase tracking-wide ${destructive ? "text-red-300" : "text-slate-400"}`}>{role}</div>
            <div className="flex flex-wrap items-baseline justify-between gap-2">
              <span className="font-medium">{disk.model}</span>
              <span className="text-xs text-slate-300">{fmtSize(disk.sizeBytes)} · {disk.bus}</span>
            </div>
            <div className="text-xs font-mono text-slate-400">{disk.path}</div>
            {disk.partitions.length === 0 ? (
              <p className="text-xs text-slate-500">Sem partições.</p>
            ) : (
              <ul className="text-xs space-y-0.5">
                {disk.partitions.map((p) => (
                  <li key={p.index} className="flex justify-between gap-2 font-mono">
                    <span className="truncate">
                      #{p.index} {p.label ? `“${p.label}”` : "(sem rótulo)"} · {p.fs ?? "?"}
                      {p.mountPoint ? ` · ${p.mountPoint}` : ""}
                    </span>
                    <span className="whitespace-nowrap text-slate-400">{fmtSize(p.sizeBytes)}</span>
                  </li>
                ))}
              </ul>
            )}
          </div>
        ))}

        <p className="text-sm text-red-200">⚠ {req.warning}</p>

        <label className="flex items-center gap-2 text-sm">
          <input type="checkbox" checked={ack} onChange={(e) => setAck(e.target.checked)} className="accent-red-500" />
          {req.ackLabel}
        </label>

        <div className="flex justify-end gap-2">
          <button
            ref={cancelRef}
            type="button"
            onClick={onCancel}
            className="px-4 py-2 rounded-lg text-sm bg-slate-800 hover:bg-slate-700"
          >
            Cancelar
          </button>
          <button
            type="button"
            disabled={!ack}
            onClick={req.onConfirm}
            className="px-4 py-2 rounded-lg text-sm font-semibold bg-red-600 hover:bg-red-500 disabled:bg-slate-800 disabled:text-slate-500"
          >
            {req.confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
