// Tamanhos em unidades binárias (1 KiB = 1024 B), rotuladas como tal.
const KiB = 1024;
const MiB = KiB * 1024;
const GiB = MiB * 1024;
const TiB = GiB * 1024;

export const fmtSize = (b: number) => {
  if (b >= TiB) return `${(b / TiB).toFixed(2)} TiB`;
  if (b >= GiB) return `${(b / GiB).toFixed(1)} GiB`;
  return `${(b / MiB).toFixed(1)} MiB`;
};

/** ETA desconhecido (null/NaN/Infinity/negativo) → "—"; >= 1 h → H:MM:SS. */
export const fmtETA = (s: number | null | undefined) => {
  if (s == null || !Number.isFinite(s) || s < 0) return "—";
  const total = Math.floor(s);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const ss = total % 60;
  const p2 = (n: number) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${p2(m)}:${p2(ss)}` : `${p2(m)}:${p2(ss)}`;
};

export const PHASE_LABEL: Record<string, string> = {
  copying: "Copiando blocos…",
  flushing: "Gravando cache no disco (fsync)…",
  verifying: "Verificando integridade (BLAKE3)…",
  compressing: "Compactando imagem (zstd)…",
  restoring: "Restaurando imagem…",
  done: "Concluído ✔",
  error: "Erro ✖",
  cancelled: "Cancelado ⚠",
};

export const OP_LABEL: Record<string, string> = {
  clone: "Clonagem de disco",
  image: "Criação de imagem",
  restore: "Restauração de imagem",
};
