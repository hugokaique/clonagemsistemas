import type { ReactNode } from "react";
import type { DiskInfo } from "../types";

/** Lista de discos com estados de carregamento e vazio. */
export default function DiskList({
  disks,
  loading,
  className = "space-y-2",
  renderCard,
}: {
  disks: DiskInfo[];
  loading: boolean;
  className?: string;
  renderCard: (d: DiskInfo) => ReactNode;
}) {
  if (loading) {
    return (
      <p role="status" className="text-sm text-slate-400 py-3">
        Carregando discos…
      </p>
    );
  }
  if (disks.length === 0) {
    return (
      <p className="text-sm text-slate-500 py-3">
        Nenhum disco encontrado. Conecte um dispositivo e use “Reescanear”.
      </p>
    );
  }
  return <div className={className}>{disks.map(renderCard)}</div>;
}
