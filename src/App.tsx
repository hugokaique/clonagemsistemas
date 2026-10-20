import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// ── Tipos espelhando o backend (serde camelCase) ──────────────────────────
interface PartitionInfo {
  index: number;
  path?: string | null;
  sizeBytes: number;
  fs?: string | null;
  label?: string | null;
  mountPoint?: string | null;
}
interface DiskInfo {
  id: string;
  path: string;
  model: string;
  sizeBytes: number;
  bus: string;
  isSystem: boolean;
  isRemovable: boolean;
  partitions: PartitionInfo[];
}
interface CloneProgress {
  phase: "copying" | "flushing" | "verifying" | "compressing" | "restoring" | "done" | "error";
  bytesDone: number;
  totalBytes: number;
  percent: number;
  speedMbps: number;
  etaSeconds: number;
  message?: string | null;
}
interface HistoryEntry {
  id: number;
  source: string;
  target: string;
  startedAt: string;
  finishedAt?: string | null;
  bytesWritten: number;
  hashAlgo: string;
  hashHex?: string | null;
  verifyOk?: boolean | null;
  status: string;
}
interface ImageEntry {
  id: number;
  path: string;
  format?: string | null;
  sizeBytes: number;
  hashHex?: string | null;
  createdAt: string;
}
interface ImageHeaderInfo {
  valid: boolean;
  mode: number;
  sourceSize: number;
  blockSize: number;
  level: number;
  hashHex: string;
}

type Tab = "clone" | "image" | "restore";

const fmtGB = (b: number) => `${(b / 1_073_741_824).toFixed(1)} GB`;
const fmtMB = (b: number) => `${(b / 1_048_576).toFixed(1)} MB`;
const fmtSize = (b: number) => (b >= 1_073_741_824 ? fmtGB(b) : fmtMB(b));
const fmtETA = (s: number) => {
  if (!isFinite(s) || s < 0) return "--:--";
  const m = Math.floor(s / 60);
  const ss = Math.floor(s % 60);
  return `${String(m).padStart(2, "0")}:${String(ss).padStart(2, "0")}`;
};
const PHASE_LABEL: Record<string, string> = {
  copying: "Copiando blocos…",
  flushing: "Gravando cache no disco (fsync)…",
  verifying: "Verificando integridade (BLAKE3)…",
  compressing: "Compactando imagem (zstd)…",
  restoring: "Restaurando imagem…",
  done: "Concluído ✔",
  error: "Erro ✖",
};

function DiskCard({
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
      onClick={onSelect}
      disabled={disabled}
      className={`w-full text-left rounded-xl border p-3 transition
        ${selected ? "border-blue-500 bg-blue-500/10" : "border-slate-700 bg-slate-900 hover:border-slate-500"}
        ${disabled ? "opacity-40 cursor-not-allowed" : ""}`}
    >
      <div className="flex items-center justify-between gap-2">
        <span className="font-medium truncate">{disk.model}</span>
        <span className="text-xs text-slate-400 whitespace-nowrap">
          {fmtGB(disk.sizeBytes)} · {disk.bus}
        </span>
      </div>
      <div className="text-xs text-slate-400 font-mono truncate">{disk.path}</div>
      <div className="mt-1 flex flex-wrap gap-2 text-[11px]">
        {disk.isSystem && (
          <span className="px-1.5 py-0.5 rounded bg-red-500/20 text-red-300">
            DISCO DO SISTEMA — bloqueado como destino
          </span>
        )}
        {disk.isRemovable && (
          <span className="px-1.5 py-0.5 rounded bg-emerald-500/20 text-emerald-300">removível</span>
        )}
        <span className="px-1.5 py-0.5 rounded bg-slate-700 text-slate-300">
          {disk.partitions.length} partições
        </span>
      </div>
    </button>
  );
}

function ProgressPanel({ progress }: { progress: CloneProgress }) {
  return (
    <div className="space-y-1.5">
      <div className="flex justify-between text-sm">
        <span>{PHASE_LABEL[progress.phase] ?? progress.phase}</span>
        <span className="font-mono">
          {progress.percent.toFixed(1)}% · {progress.speedMbps.toFixed(0)} MB/s · ETA{" "}
          {fmtETA(progress.etaSeconds)}
        </span>
      </div>
      <div className="h-2.5 rounded-full bg-slate-800 overflow-hidden">
        <div
          className={`h-full transition-all ${
            progress.phase === "error"
              ? "bg-red-500"
              : progress.phase === "done"
              ? "bg-emerald-500"
              : "bg-blue-500"
          }`}
          style={{ width: `${progress.percent}%` }}
        />
      </div>
      {progress.message && (
        <p className="text-xs text-slate-400 font-mono break-all">{progress.message}</p>
      )}
    </div>
  );
}

export default function App() {
  const [tab, setTab] = useState<Tab>("clone");
  const [disks, setDisks] = useState<DiskInfo[]>([]);
  const [source, setSource] = useState("");
  const [target, setTarget] = useState("");
  const [verify, setVerify] = useState(true);
  // imagem
  const [smart, setSmart] = useState(true);
  const [live, setLive] = useState(false);
  const [partSel, setPartSel] = useState(""); // "diskPath|index"
  const [imgOut, setImgOut] = useState("backup.cfimg");
  const [level, setLevel] = useState(3);
  // restore
  const [images, setImages] = useState<ImageEntry[]>([]);
  const [imgSel, setImgSel] = useState("");
  const [imgInfo, setImgInfo] = useState<ImageHeaderInfo | null>(null);

  const [progress, setProgress] = useState<CloneProgress | null>(null);
  const [history, setHistory] = useState<HistoryEntry[]>([]);
  const [error, setError] = useState<string | null>(null);

  const busy = progress !== null && !["done", "error"].includes(progress.phase);
  const srcDisk = disks.find((d) => d.path === source);
  const dstDisk = disks.find((d) => d.path === target);

  const refresh = async () => {
    try {
      setDisks(await invoke<DiskInfo[]>("list_disks"));
      setHistory(await invoke<HistoryEntry[]>("list_history"));
      setImages(await invoke<ImageEntry[]>("list_images"));
    } catch (e) {
      setError(String(e));
    }
  };

  useEffect(() => {
    refresh();
    const unlisten = listen<CloneProgress>("clone://progress", (ev) => {
      setProgress(ev.payload);
      if (ev.payload.phase === "done" || ev.payload.phase === "error") {
        refresh();
        if (ev.payload.phase === "error") setError(ev.payload.message ?? "falha na operação");
      }
    });
    return () => {
      unlisten.then((f) => f());
    };
  }, []);

  // Inspeciona o header da imagem selecionada no catálogo
  useEffect(() => {
    setImgInfo(null);
    if (imgSel.endsWith(".cfimg")) {
      invoke<ImageHeaderInfo>("read_image_header", { path: imgSel })
        .then(setImgInfo)
        .catch(() => setImgInfo(null));
    }
  }, [imgSel]);

  // ── Ações ────────────────────────────────────────────────────────────────
  const startClone = async () => {
    setError(null);
    if (!srcDisk || !dstDisk) return setError("Selecione origem e destino.");
    if (dstDisk.isSystem) return setError("O destino é o disco do sistema — operação bloqueada.");
    if (dstDisk.sizeBytes < srcDisk.sizeBytes) return setError("O destino é menor que a origem.");
    if (
      !window.confirm(
        `CONFIRMAR CLONAGEM\n\nOrigem : ${srcDisk.model} (${srcDisk.path})\nDestino: ${dstDisk.model} (${dstDisk.path})\n\n⚠ TODOS os dados do destino serão APAGADOS. Continuar?`
      )
    )
      return;
    setProgress({ phase: "copying", bytesDone: 0, totalBytes: srcDisk.sizeBytes, percent: 0, speedMbps: 0, etaSeconds: Infinity });
    try {
      await invoke("start_clone", {
        req: { source: srcDisk.path, target: dstDisk.path, sourceSize: srcDisk.sizeBytes, blockSize: null, verify },
      });
    } catch (e) {
      setError(String(e));
      setProgress(null);
    }
  };

  const startImage = async () => {
    setError(null);
    let src = "";
    let srcSize = 0;
    let fs: string | null = null;

    if (smart) {
      // Smart copy opera por partição/volume (bitmap do filesystem)
      const [diskPath, idx] = partSel.split("|");
      const d = disks.find((x) => x.path === diskPath);
      const p = d?.partitions.find((x) => String(x.index) === idx);
      if (!d || !p) return setError("Selecione a partição de origem para o smart copy.");
      // Windows: volume NTFS precisa do caminho \\.\X: a partir da letra
      if (p.mountPoint && /^[A-Za-z]:\\?$/.test(p.mountPoint)) {
        src = `\\\\.\\${p.mountPoint.replace("\\", "")}`;
      } else if (p.path) {
        src = p.path; // Linux /dev/sdXN · macOS /dev/diskNsM
      } else {
        return setError("Partição sem caminho de volume utilizável para smart copy.");
      }
      srcSize = p.sizeBytes;
      fs = p.fs ?? null;
    } else {
      if (!srcDisk) return setError("Selecione o disco de origem.");
      src = srcDisk.path;
      srcSize = srcDisk.sizeBytes;
    }

    if (!imgOut.trim()) return setError("Informe o caminho de saída da imagem.");
    setProgress({ phase: "compressing", bytesDone: 0, totalBytes: srcSize, percent: 0, speedMbps: 0, etaSeconds: Infinity });
    try {
      await invoke("create_image", {
        req: { source: src, sourceSize: srcSize, output: imgOut.trim(), level, smart, fs, live },
      });
    } catch (e) {
      setError(String(e));
      setProgress(null);
    }
  };

  const startRestore = async () => {
    setError(null);
    if (!imgSel) return setError("Selecione a imagem a restaurar.");
    if (!dstDisk) return setError("Selecione o disco de destino.");
    if (dstDisk.isSystem) return setError("O destino é o disco do sistema — operação bloqueada.");
    if (
      !window.confirm(
        `CONFIRMAR RESTAURAÇÃO\n\nImagem : ${imgSel}\nDestino: ${dstDisk.model} (${dstDisk.path})\n\n⚠ TODOS os dados do destino serão APAGADOS. Continuar?`
      )
    )
      return;
    const total = imgInfo?.sourceSize ?? dstDisk.sizeBytes;
    setProgress({ phase: "restoring", bytesDone: 0, totalBytes: total, percent: 0, speedMbps: 0, etaSeconds: Infinity });
    try {
      await invoke("restore_image", { req: { image: imgSel, target: dstDisk.path, verify } });
    } catch (e) {
      setError(String(e));
      setProgress(null);
    }
  };

  const allPartitions = disks.flatMap((d) =>
    d.partitions.map((p) => ({ disk: d, part: p }))
  );

  // ── UI ───────────────────────────────────────────────────────────────────
  return (
    <main className="min-h-screen p-6 max-w-6xl mx-auto space-y-6">
      <header className="flex items-baseline justify-between">
        <h1 className="text-2xl font-bold tracking-tight">
          Clone<span className="text-blue-400">Forge</span>
        </h1>
        <button onClick={refresh} className="text-sm px-3 py-1.5 rounded-lg bg-slate-800 hover:bg-slate-700">
          ↻ Reescanear
        </button>
      </header>

      <nav className="flex gap-2">
        {(
          [
            ["clone", "① Clonar disco"],
            ["image", "② Criar imagem (.cfimg)"],
            ["restore", "③ Restaurar imagem"],
          ] as [Tab, string][]
        ).map(([t, label]) => (
          <button
            key={t}
            onClick={() => setTab(t)}
            className={`px-4 py-2 rounded-lg text-sm font-medium transition ${
              tab === t ? "bg-blue-600 text-white" : "bg-slate-800 text-slate-300 hover:bg-slate-700"
            }`}
          >
            {label}
          </button>
        ))}
      </nav>

      {error && (
        <div className="rounded-xl border border-red-500/40 bg-red-500/10 px-4 py-3 text-sm text-red-200">
          {error}
        </div>
      )}

      {/* ── Aba: Clonar disco ─────────────────────────────────────────── */}
      {tab === "clone" && (
        <>
          <section className="grid md:grid-cols-2 gap-6">
            <div className="space-y-2">
              <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-400">Origem</h2>
              {disks.map((d) => (
                <DiskCard key={`s-${d.id}`} disk={d} selected={source === d.path} disabled={busy || target === d.path} onSelect={() => setSource(d.path)} />
              ))}
            </div>
            <div className="space-y-2">
              <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-400">Destino</h2>
              {disks.map((d) => (
                <DiskCard key={`t-${d.id}`} disk={d} selected={target === d.path} disabled={busy || d.isSystem || source === d.path} onSelect={() => setTarget(d.path)} />
              ))}
            </div>
          </section>
          <section className="rounded-xl border border-slate-700 bg-slate-900 p-4 space-y-3">
            <label className="flex items-center gap-2 text-sm">
              <input type="checkbox" checked={verify} onChange={(e) => setVerify(e.target.checked)} disabled={busy} className="accent-blue-500" />
              Verificar integridade após a clonagem (BLAKE3) — recomendado
            </label>
            {progress && <ProgressPanel progress={progress} />}
            <button
              onClick={startClone}
              disabled={busy || !source || !target}
              className="w-full py-2.5 rounded-xl font-semibold bg-blue-600 hover:bg-blue-500 disabled:bg-slate-800 disabled:text-slate-500 transition"
            >
              {busy ? "Operação em andamento…" : "Iniciar clonagem"}
            </button>
          </section>
        </>
      )}

      {/* ── Aba: Criar imagem ─────────────────────────────────────────── */}
      {tab === "image" && (
        <section className="space-y-4">
          <div className="rounded-xl border border-slate-700 bg-slate-900 p-4 space-y-3">
            <label className="flex items-center gap-2 text-sm">
              <input type="checkbox" checked={smart} onChange={(e) => setSmart(e.target.checked)} disabled={busy} className="accent-blue-500" />
              <span>
                <strong>Smart Copy</strong> — copiar apenas blocos alocados
                <span className="text-slate-400"> (Windows: bitmap NTFS/ReFS · Linux: partclone · macOS: hdiutil)</span>
              </span>
            </label>

            {smart && (
              <label className="flex items-center gap-2 text-sm pl-6">
                <input type="checkbox" checked={live} onChange={(e) => setLive(e.target.checked)} disabled={busy} className="accent-blue-500" />
                <span>
                  <strong>Live Copy</strong> — snapshot VSS antes de ler
                  <span className="text-slate-400"> (Windows: permite imagem consistente do volume em uso, incl. o do sistema)</span>
                </span>
              </label>
            )}

            {!smart ? (
              <div className="grid md:grid-cols-2 gap-2">
                {disks.map((d) => (
                  <DiskCard key={`i-${d.id}`} disk={d} selected={source === d.path} disabled={busy} onSelect={() => setSource(d.path)} />
                ))}
              </div>
            ) : (
              <div className="space-y-1 max-h-64 overflow-y-auto pr-1">
                {allPartitions.map(({ disk: d, part: p }) => {
                  const key = `${d.path}|${p.index}`;
                  return (
                    <button
                      key={key}
                      disabled={busy}
                      onClick={() => setPartSel(key)}
                      className={`w-full text-left rounded-lg border px-3 py-2 text-sm transition ${
                        partSel === key ? "border-blue-500 bg-blue-500/10" : "border-slate-700 bg-slate-950 hover:border-slate-500"
                      }`}
                    >
                      <span className="font-mono">{p.mountPoint ?? p.path ?? `${d.id}p${p.index}`}</span>
                      <span className="text-slate-400"> · {p.fs ?? p.label ?? "?"} · {fmtSize(p.sizeBytes)} · {d.model}</span>
                    </button>
                  );
                })}
              </div>
            )}

            <div className="grid md:grid-cols-2 gap-3">
              <label className="text-sm space-y-1">
                <span className="text-slate-400">Arquivo de saída</span>
                <input
                  value={imgOut}
                  onChange={(e) => setImgOut(e.target.value)}
                  disabled={busy}
                  className="w-full rounded-lg bg-slate-950 border border-slate-700 px-3 py-2 font-mono text-sm"
                  placeholder="D:\\backups\\win11.cfimg"
                />
              </label>
              <label className="text-sm space-y-1">
                <span className="text-slate-400">Nível zstd: {level} (1 = rápido · 19 = máximo)</span>
                <input type="range" min={1} max={19} value={level} onChange={(e) => setLevel(Number(e.target.value))} disabled={busy} className="w-full accent-blue-500" />
              </label>
            </div>

            {progress && <ProgressPanel progress={progress} />}
            <button
              onClick={startImage}
              disabled={busy || (!smart && !source) || (smart && !partSel)}
              className="w-full py-2.5 rounded-xl font-semibold bg-blue-600 hover:bg-blue-500 disabled:bg-slate-800 disabled:text-slate-500 transition"
            >
              {busy ? "Operação em andamento…" : "Criar imagem"}
            </button>
          </div>

          <div className="space-y-2">
            <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-400">Catálogo de imagens</h2>
            <div className="rounded-xl border border-slate-800 overflow-x-auto">
              <table className="w-full text-xs">
                <thead className="bg-slate-900 text-slate-400">
                  <tr>
                    <th className="text-left px-3 py-2">Criada em</th>
                    <th className="text-left px-3 py-2">Arquivo</th>
                    <th className="text-right px-3 py-2">Tamanho</th>
                    <th className="text-left px-3 py-2">Hash</th>
                  </tr>
                </thead>
                <tbody>
                  {images.length === 0 && (
                    <tr><td colSpan={4} className="px-3 py-4 text-center text-slate-500">Nenhuma imagem catalogada.</td></tr>
                  )}
                  {images.map((im) => (
                    <tr key={im.id} className="border-t border-slate-800">
                      <td className="px-3 py-2 whitespace-nowrap">{new Date(im.createdAt + "Z").toLocaleString("pt-BR")}</td>
                      <td className="px-3 py-2 font-mono truncate max-w-[260px]" title={im.path}>{im.path}</td>
                      <td className="px-3 py-2 text-right whitespace-nowrap">{fmtSize(im.sizeBytes)}</td>
                      <td className="px-3 py-2 font-mono">{im.hashHex ? `${im.hashHex.slice(0, 16)}…` : "—"}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </div>
        </section>
      )}

      {/* ── Aba: Restaurar imagem ─────────────────────────────────────── */}
      {tab === "restore" && (
        <section className="space-y-4">
          <div className="rounded-xl border border-slate-700 bg-slate-900 p-4 space-y-3">
            <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-400">Imagem de origem</h2>
            <div className="space-y-1 max-h-48 overflow-y-auto pr-1">
              {images.map((im) => (
                <button
                  key={im.id}
                  disabled={busy}
                  onClick={() => setImgSel(im.path)}
                  className={`w-full text-left rounded-lg border px-3 py-2 text-sm transition ${
                    imgSel === im.path ? "border-blue-500 bg-blue-500/10" : "border-slate-700 bg-slate-950 hover:border-slate-500"
                  }`}
                >
                  <span className="font-mono">{im.path}</span>
                  <span className="text-slate-400"> · {im.format ?? "?"} · {fmtSize(im.sizeBytes)}</span>
                </button>
              ))}
              {images.length === 0 && <p className="text-sm text-slate-500">Catálogo vazio — crie uma imagem na aba ②.</p>}
            </div>

            {imgInfo && (
              <div className="text-xs font-mono rounded-lg bg-slate-950 border border-slate-800 px-3 py-2 text-slate-300">
                header OK · modo {imgInfo.mode === 0 ? "raw" : imgInfo.mode === 1 ? "smart-bitmap" : "partclone"} · origem {fmtSize(imgInfo.sourceSize)} · zstd nível {imgInfo.level} · BLAKE3 {imgInfo.hashHex.slice(0, 24)}…
              </div>
            )}

            <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-400 pt-2">Disco de destino</h2>
            <div className="grid md:grid-cols-2 gap-2">
              {disks.map((d) => (
                <DiskCard key={`r-${d.id}`} disk={d} selected={target === d.path} disabled={busy || d.isSystem} onSelect={() => setTarget(d.path)} />
              ))}
            </div>

            <label className="flex items-center gap-2 text-sm">
              <input type="checkbox" checked={verify} onChange={(e) => setVerify(e.target.checked)} disabled={busy} className="accent-blue-500" />
              Verificar hash gravado na imagem após restaurar — recomendado
            </label>

            {progress && <ProgressPanel progress={progress} />}
            <button
              onClick={startRestore}
              disabled={busy || !imgSel || !target}
              className="w-full py-2.5 rounded-xl font-semibold bg-blue-600 hover:bg-blue-500 disabled:bg-slate-800 disabled:text-slate-500 transition"
            >
              {busy ? "Operação em andamento…" : "Restaurar imagem"}
            </button>
          </div>
        </section>
      )}

      {/* ── Histórico (sempre visível) ────────────────────────────────── */}
      <section className="space-y-2">
        <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-400">Histórico (SQLite local)</h2>
        <div className="rounded-xl border border-slate-800 overflow-x-auto">
          <table className="w-full text-xs">
            <thead className="bg-slate-900 text-slate-400">
              <tr>
                <th className="text-left px-3 py-2">Data</th>
                <th className="text-left px-3 py-2">Origem → Destino</th>
                <th className="text-right px-3 py-2">Dados</th>
                <th className="text-left px-3 py-2">Hash</th>
                <th className="text-left px-3 py-2">Status</th>
              </tr>
            </thead>
            <tbody>
              {history.length === 0 && (
                <tr><td colSpan={5} className="px-3 py-4 text-center text-slate-500">Nenhuma operação registrada ainda.</td></tr>
              )}
              {history.map((h) => (
                <tr key={h.id} className="border-t border-slate-800">
                  <td className="px-3 py-2 whitespace-nowrap">{new Date(h.startedAt).toLocaleString("pt-BR")}</td>
                  <td className="px-3 py-2 font-mono truncate max-w-[220px]" title={`${h.source} → ${h.target}`}>{h.source} → {h.target}</td>
                  <td className="px-3 py-2 text-right whitespace-nowrap">{fmtSize(h.bytesWritten)}</td>
                  <td className="px-3 py-2 font-mono truncate max-w-[160px]" title={h.hashHex ?? ""}>
                    {h.hashHex ? `${h.hashAlgo}:${h.hashHex.slice(0, 12)}…` : "—"}
                  </td>
                  <td className="px-3 py-2">
                    {h.status === "ok" ? (
                      <span className="text-emerald-400">✔ ok{h.verifyOk ? " (verificado)" : ""}</span>
                    ) : (
                      <span className="text-red-400">✖ {h.status}</span>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </section>
    </main>
  );
}
