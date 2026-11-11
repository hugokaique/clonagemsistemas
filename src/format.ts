import type { PartitionInfo, Phase } from "./types";

const GIB = 1_073_741_824;
const MIB = 1_048_576;

/** Tamanho legível. Discos pequenos não viram "0.0 GB". */
export const fmtSize = (b: number): string => {
  if (!Number.isFinite(b) || b <= 0) return "0 MB";
  if (b >= GIB) return `${(b / GIB).toFixed(1)} GB`;
  if (b >= MIB) return `${(b / MIB).toFixed(1)} MB`;
  return `${Math.max(1, Math.round(b / 1024))} KB`;
};

/** ETA em mm:ss ou h:mm:ss; `null`/infinito = desconhecido. */
export const fmtETA = (s: number | null | undefined): string => {
  if (s == null || !Number.isFinite(s) || s < 0) return "--:--";
  const total = Math.round(s);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const ss = total % 60;
  const mm = String(m).padStart(2, "0");
  const sss = String(ss).padStart(2, "0");
  return h > 0 ? `${h}:${mm}:${sss}` : `${mm}:${sss}`;
};

/**
 * Datas do banco: RFC 3339 (`2026-10-19T12:00:00Z`) nas linhas novas e
 * `2026-10-19 12:00:00` (UTC, formato do SQLite) nas antigas. O formato com
 * espaço não é aceito pelo `Date` do WebKit (Linux/macOS) — normalizamos.
 */
export const parseDbDate = (s: string | null | undefined): Date | null => {
  if (!s) return null;
  const iso = /^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/.test(s) ? `${s.replace(" ", "T")}Z` : s;
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? null : d;
};

export const fmtDate = (s: string | null | undefined): string =>
  parseDbDate(s)?.toLocaleString("pt-BR") ?? "—";

export const fmtUnix = (secs: number): string =>
  secs > 0 ? new Date(secs * 1000).toLocaleString("pt-BR") : "—";

/** Duração entre duas datas do banco ("1 h 02 min", "3 min 05 s", "12 s"). */
export const fmtDuration = (start: string, end?: string | null): string => {
  const a = parseDbDate(start);
  const b = parseDbDate(end);
  if (!a || !b) return "—";
  const secs = Math.round((b.getTime() - a.getTime()) / 1000);
  if (secs <= 0) return "—"; // linhas antigas gravavam início = fim
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  if (h > 0) return `${h} h ${String(m).padStart(2, "0")} min`;
  if (m > 0) return `${m} min ${String(s).padStart(2, "0")} s`;
  return `${s} s`;
};

export const PHASE_LABEL: Record<Phase, string> = {
  preparing: "Preparando…",
  copying: "Copiando blocos…",
  flushing: "Gravando cache no disco (fsync)…",
  verifying: "Verificando integridade (BLAKE3)…",
  compressing: "Compactando imagem (zstd)…",
  restoring: "Restaurando imagem…",
  done: "Concluído ✔",
  error: "Erro ✖",
  cancelled: "Cancelado",
};

export const isTerminal = (phase: Phase): boolean =>
  phase === "done" || phase === "error" || phase === "cancelled";

const PARTCLONE_FS = ["ext2", "ext3", "ext4", "xfs", "btrfs", "ntfs", "vfat", "exfat", "f2fs"];

/**
 * A partição pode ser origem de Smart Copy neste SO?
 * Retorna `null` se sim, ou o motivo (para exibir) se não.
 */
export const smartBlocker = (os: string, p: PartitionInfo): string | null => {
  if (!p.path) return "sem volume acessível";
  const fs = (p.fs ?? "").toLowerCase();
  if (os === "windows") {
    return fs === "ntfs" || fs === "refs" ? null : `exige NTFS/ReFS${p.fs ? ` (é ${p.fs})` : ""}`;
  }
  if (os === "linux") {
    return PARTCLONE_FS.includes(fs) ? null : `sem suporte no partclone${p.fs ? ` (${p.fs})` : ""}`;
  }
  return null;
};

export const KIND_LABEL: Record<string, string> = {
  clone: "Clonagem",
  image: "Imagem",
  restore: "Restauração",
};

export const MODE_LABEL = ["raw (bit a bit)", "smart copy (bitmap NTFS)", "smart copy (partclone)"];
export const SOURCE_KIND_LABEL = ["origem desconhecida", "disco inteiro", "partição/volume"];
