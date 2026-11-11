import type { DiskInfo } from "../types";
import { fmtSize } from "../format";

export default function DiskCard({
  disk,
  selected,
  disabled,
  onSelect,
}: {
  disk: DiskInfo;
  selected: boolean;
  disabled: boolean;
  onSelect: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onSelect}
      disabled={disabled}
      aria-pressed={selected}
      className={`w-full text-left rounded-xl border p-3 transition
        ${selected ? "border-blue-500 bg-blue-500/10" : "border-slate-700 bg-slate-900 hover:border-slate-500"}
        ${disabled ? "cursor-not-allowed" : ""}`}
    >
      {/* O conteúdo principal escurece, mas o selo do sistema permanece legível */}
      <div className={disabled ? "opacity-50" : ""}>
        <div className="flex items-center justify-between gap-2">
          <span className="font-medium truncate">{disk.model}</span>
          <span className="text-xs text-slate-400 whitespace-nowrap">
            {fmtSize(disk.sizeBytes)} · {disk.bus}
          </span>
        </div>
        <div className="text-xs text-slate-400 font-mono truncate">{disk.path}</div>
      </div>
      <div className="mt-1 flex flex-wrap gap-2 text-[11px]">
        {disk.isSystem && (
          <span className="px-1.5 py-0.5 rounded bg-red-500/25 text-red-200 font-medium">
            DISCO DO SISTEMA — bloqueado como destino
          </span>
        )}
        <span className={`flex gap-2 ${disabled ? "opacity-50" : ""}`}>
          {disk.isRemovable && (
            <span className="px-1.5 py-0.5 rounded bg-emerald-500/20 text-emerald-300">removível</span>
          )}
          <span className="px-1.5 py-0.5 rounded bg-slate-700 text-slate-300">
            {disk.partitions.length} partições
          </span>
        </span>
      </div>
    </button>
  );
}
