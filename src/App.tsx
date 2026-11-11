import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { documentDir, join } from "@tauri-apps/api/path";
import type {
  CloneProgress,
  DiskInfo,
  HistoryEntry,
  ImageEntry,
  ImageHeaderInfo,
  Tab,
} from "./types";
import { isTerminalPhase } from "./types";
import { fmtSize } from "./format";
import DiskCard from "./components/DiskCard";
import DiskList from "./components/DiskList";
import ProgressPanel from "./components/ProgressPanel";
import ConfirmDialog, { type ConfirmRequest } from "./components/ConfirmDialog";

const REFRESH_MS = 10_000;
const BTN_PRIMARY =
  "w-full py-2.5 rounded-xl font-semibold bg-blue-600 hover:bg-blue-500 disabled:bg-slate-800 disabled:text-slate-500 transition";

interface Job {
  op: Tab;
  progress: CloneProgress;
}

export default function App() {
  const [tab, setTab] = useState<Tab>("clone");
  const [disks, setDisks] = useState<DiskInfo[]>([]);
  const [disksLoading, setDisksLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [history, setHistory] = useState<HistoryEntry[]>([]);
  const [images, setImages] = useState<ImageEntry[]>([]);

  // Seleções independentes por aba
  const [cloneSrc, setCloneSrc] = useState("");
  const [cloneDst, setCloneDst] = useState("");
  const [cloneVerify, setCloneVerify] = useState(true);

  const [smart, setSmart] = useState(true);
  const [live, setLive] = useState(false);
  const [imgSrc, setImgSrc] = useState(""); // disco inteiro (smart desligado)
  const [partSel, setPartSel] = useState(""); // "diskPath|index"
  const [imgOut, setImgOut] = useState("");
  const [level, setLevel] = useState(3);

  const [restoreDst, setRestoreDst] = useState("");
  const [restoreVerify, setRestoreVerify] = useState(true);
  const [imgSel, setImgSel] = useState("");
  const [imgInfo, setImgInfo] = useState<ImageHeaderInfo | null>(null);
  const [imgInfoError, setImgInfoError] = useState<string | null>(null);
  const [imgInfoLoading, setImgInfoLoading] = useState(false);

  const [job, setJob] = useState<Job | null>(null);
  const [cancelling, setCancelling] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<ConfirmRequest | null>(null);

  const opRef = useRef<Tab>("clone");
  const noticeTimer = useRef<number | undefined>(undefined);
  const refreshingRef = useRef(false);

  const busy = job !== null && !isTerminalPhase(job.progress.phase);
  const busyRef = useRef(busy);
  busyRef.current = busy;

  // ── Carregamento de dados ────────────────────────────────────────────────
  const refreshDisks = useCallback(async () => {
    if (refreshingRef.current) return;
    refreshingRef.current = true;
    setRefreshing(true);
    try {
      const list = await invoke<DiskInfo[]>("list_disks");
      setDisks(list);
      const has = (p: string) => list.some((d) => d.path === p);
      // mantém a seleção se o disco ainda existe; limpa se sumiu
      setCloneSrc((p) => (has(p) ? p : ""));
      setCloneDst((p) => (has(p) ? p : ""));
      setImgSrc((p) => (has(p) ? p : ""));
      setRestoreDst((p) => (has(p) ? p : ""));
      setPartSel((key) => {
        if (!key) return key;
        const [dp, idx] = key.split("|");
        const d = list.find((x) => x.path === dp);
        return d && d.partitions.some((x) => String(x.index) === idx) ? key : "";
      });
    } catch (e) {
      setError(String(e));
    } finally {
      refreshingRef.current = false;
      setRefreshing(false);
      setDisksLoading(false);
    }
  }, []);

  const refreshLists = useCallback(async () => {
    try {
      setHistory(await invoke<HistoryEntry[]>("list_history"));
      setImages(await invoke<ImageEntry[]>("list_images"));
    } catch (e) {
      setError(String(e));
    }
  }, []);

  const refreshAll = useCallback(() => {
    void refreshDisks();
    void refreshLists();
  }, [refreshDisks, refreshLists]);

  useEffect(() => {
    refreshAll();
    const showNotice = (msg: string) => {
      setNotice(msg);
      window.clearTimeout(noticeTimer.current);
      noticeTimer.current = window.setTimeout(() => setNotice(null), 8000);
    };
    const unProgress = listen<CloneProgress>("clone://progress", (ev) => {
      setJob({ op: opRef.current, progress: ev.payload });
      if (isTerminalPhase(ev.payload.phase)) {
        setCancelling(false);
        refreshAll();
      }
    });
    const unClose = listen("app://close-blocked", () =>
      showNotice("Há uma operação em andamento — cancele-a antes de fechar o CloneForge.")
    );
    return () => {
      unProgress.then((f) => f());
      unClose.then((f) => f());
      window.clearTimeout(noticeTimer.current);
    };
  }, [refreshAll]);

  // Atualização automática dos discos: ao focar a janela e a cada ~10 s (fora de operações)
  useEffect(() => {
    if (busy) return;
    const onFocus = () => void refreshDisks();
    window.addEventListener("focus", onFocus);
    const t = window.setInterval(() => {
      if (!busyRef.current && !document.hidden) void refreshDisks();
    }, REFRESH_MS);
    return () => {
      window.removeEventListener("focus", onFocus);
      window.clearInterval(t);
    };
  }, [busy, refreshDisks]);

  // Caminho padrão: Documentos do usuário
  useEffect(() => {
    (async () => {
      try {
        const p = await join(await documentDir(), "backup.cfimg");
        setImgOut((cur) => (cur === "" ? p : cur));
      } catch {
        /* deixa o campo vazio */
      }
    })();
  }, []);

  // Inspeciona o header da imagem selecionada; Restaurar só habilita com header válido
  useEffect(() => {
    setImgInfo(null);
    setImgInfoError(null);
    if (!imgSel) {
      setImgInfoLoading(false);
      return;
    }
    let stale = false;
    setImgInfoLoading(true);
    invoke<ImageHeaderInfo>("read_image_header", { path: imgSel })
      .then((i) => {
        if (stale) return;
        if (i.valid) setImgInfo(i);
        else setImgInfoError("Imagem inválida: header não reconhecido.");
      })
      .catch((e) => !stale && setImgInfoError(String(e)))
      .finally(() => !stale && setImgInfoLoading(false));
    return () => {
      stale = true;
    };
  }, [imgSel]);

  // ── Seleção de discos ────────────────────────────────────────────────────
  const toggleSource = (path: string) => {
    if (cloneSrc === path) return setCloneSrc("");
    setCloneSrc(path);
    if (cloneDst === path) setCloneDst(""); // origem e destino não podem coincidir
  };
  const toggleTarget = (path: string) => {
    if (cloneDst === path) return setCloneDst("");
    setCloneDst(path);
    if (cloneSrc === path) setCloneSrc("");
  };

  const srcDisk = disks.find((d) => d.path === cloneSrc);
  const dstDisk = disks.find((d) => d.path === cloneDst);
  const imgDisk = disks.find((d) => d.path === imgSrc);
  const restoreDisk = disks.find((d) => d.path === restoreDst);

  // ── Execução ─────────────────────────────────────────────────────────────
  const launch = async (op: Tab, initial: CloneProgress, cmd: string, req: object) => {
    opRef.current = op;
    setCancelling(false);
    setJob({ op, progress: initial });
    try {
      await invoke(cmd, { req });
    } catch (e) {
      const msg = String(e);
      setJob((prev) =>
        prev && prev.op === op && isTerminalPhase(prev.progress.phase)
          ? prev // o evento terminal (erro/cancelado) já descreve o resultado
          : { op, progress: { ...initial, phase: "error", etaSeconds: null, message: msg } }
      );
    }
  };
  const newProgress = (phase: CloneProgress["phase"], total: number): CloneProgress => ({
    phase, bytesDone: 0, totalBytes: total, percent: 0, speedMbps: 0, etaSeconds: null,
  });

  const cancelJob = () => {
    setCancelling(true);
    invoke("cancel_job").catch((e) => {
      setCancelling(false);
      setError(String(e));
    });
  };

  const startClone = () => {
    setError(null);
    if (!srcDisk || !dstDisk) return setError("Selecione origem e destino.");
    if (dstDisk.isSystem) return setError("O destino é o disco do sistema — operação bloqueada.");
    if (dstDisk.sizeBytes < srcDisk.sizeBytes) return setError("O destino é menor que a origem.");
    const s = srcDisk;
    const d = dstDisk;
    setConfirm({
      title: "Confirmar clonagem",
      operation: "Clonagem bloco a bloco do disco de origem para o disco de destino.",
      disks: [
        { role: "Origem (somente leitura)", disk: s, destructive: false },
        { role: "Destino (será apagado)", disk: d, destructive: true },
      ],
      warning: "TODOS os dados do disco de destino serão APAGADOS e não poderão ser recuperados.",
      ackLabel: "Entendo que todos os dados do destino serão apagados",
      confirmLabel: "Iniciar clonagem",
      onConfirm: () => {
        setConfirm(null);
        void launch("clone", newProgress("copying", s.sizeBytes), "start_clone", {
          source: s.path, target: d.path, sourceSize: s.sizeBytes, blockSize: null, verify: cloneVerify,
        });
      },
    });
  };

  const startImage = async () => {
    setError(null);
    let src = "";
    let srcSize = 0;
    let fs: string | null = null;

    if (smart) {
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
      if (!imgDisk) return setError("Selecione o disco de origem.");
      src = imgDisk.path;
      srcSize = imgDisk.sizeBytes;
    }

    const output = imgOut.trim();
    if (!output) return setError("Informe o caminho de saída da imagem.");

    const run = () => {
      setConfirm(null);
      void launch("image", newProgress("compressing", srcSize), "create_image", {
        source: src, sourceSize: srcSize, output, level, smart, fs,
        ...(smart ? { live } : {}),
      });
    };

    let exists = false;
    try {
      exists = await invoke<boolean>("output_exists", { path: output });
    } catch {
      exists = false; // sem a checagem, segue; o backend decide
    }
    if (!exists) return run();
    setConfirm({
      title: "Substituir arquivo existente?",
      operation: "Criação de imagem (.cfimg).",
      details: [["Arquivo", output]],
      warning: "Já existe um arquivo neste caminho. Ele será SUBSTITUÍDO pela nova imagem.",
      ackLabel: "Entendo que o arquivo existente será substituído",
      confirmLabel: "Substituir e criar imagem",
      onConfirm: run,
    });
  };

  const startRestore = () => {
    setError(null);
    if (!imgSel) return setError("Selecione a imagem a restaurar.");
    if (!imgInfo) return setError("A imagem selecionada não teve o header validado.");
    if (!restoreDisk) return setError("Selecione o disco de destino.");
    if (restoreDisk.isSystem) return setError("O destino é o disco do sistema — operação bloqueada.");
    const d = restoreDisk;
    const info = imgInfo;
    const image = imgSel;
    setConfirm({
      title: "Confirmar restauração",
      operation: "Restauração de uma imagem .cfimg sobre o disco de destino.",
      details: [
        ["Imagem", image],
        ["Origem", fmtSize(info.sourceSize)],
      ],
      disks: [{ role: "Destino (será apagado)", disk: d, destructive: true }],
      warning: "TODOS os dados do disco de destino serão APAGADOS e não poderão ser recuperados.",
      ackLabel: "Entendo que todos os dados do destino serão apagados",
      confirmLabel: "Iniciar restauração",
      onConfirm: () => {
        setConfirm(null);
        void launch("restore", newProgress("restoring", info.sourceSize), "restore_image", {
          image, target: d.path, verify: restoreVerify,
        });
      },
    });
  };

  const allPartitions = disks.flatMap((d) => d.partitions.map((p) => ({ disk: d, part: p })));
  const selPart = (() => {
    const [dp, idx] = partSel.split("|");
    return disks.find((x) => x.path === dp)?.partitions.find((x) => String(x.index) === idx);
  })();
  const inconsistentWarning = smart && !live && !!selPart?.mountPoint;

  const jobPanel = (t: Tab) =>
    job && job.op === t ? (
      <ProgressPanel op={t} progress={job.progress} cancelling={cancelling} onCancel={cancelJob} />
    ) : busy && job ? (
      <p className="text-xs text-slate-400">Há uma operação em andamento em outra aba.</p>
    ) : null;

  const tabs: [Tab, string][] = [
    ["clone", "① Clonar disco"],
    ["image", "② Criar imagem (.cfimg)"],
    ["restore", "③ Restaurar imagem"],
  ];

  // ── UI ───────────────────────────────────────────────────────────────────
  return (
    <main className="min-h-screen p-6 max-w-6xl mx-auto space-y-6">
      <header className="flex items-baseline justify-between">
        <h1 className="text-2xl font-bold tracking-tight">
          Clone<span className="text-blue-400">Forge</span>
        </h1>
        <button
          type="button"
          onClick={refreshAll}
          disabled={refreshing}
          className="text-sm px-3 py-1.5 rounded-lg bg-slate-800 hover:bg-slate-700 disabled:text-slate-500"
        >
          {refreshing ? "Reescaneando…" : "↻ Reescanear"}
        </button>
      </header>

      <div role="tablist" aria-label="Operações" className="flex gap-2 flex-wrap">
        {tabs.map(([t, label]) => (
          <button
            key={t}
            role="tab"
            id={`tab-${t}`}
            aria-selected={tab === t}
            aria-controls={`panel-${t}`}
            tabIndex={tab === t ? 0 : -1}
            onClick={() => setTab(t)}
            onKeyDown={(e) => {
              const i = tabs.findIndex(([x]) => x === t);
              const n = e.key === "ArrowRight" ? i + 1 : e.key === "ArrowLeft" ? i - 1 : null;
              if (n === null) return;
              const next = tabs[(n + tabs.length) % tabs.length][0];
              setTab(next);
              document.getElementById(`tab-${next}`)?.focus();
            }}
            className={`px-4 py-2 rounded-lg text-sm font-medium transition ${
              tab === t ? "bg-blue-600 text-white" : "bg-slate-800 text-slate-300 hover:bg-slate-700"
            }`}
          >
            {label}
          </button>
        ))}
      </div>

      {notice && (
        <div role="alert" className="rounded-xl border border-amber-500/40 bg-amber-500/10 px-4 py-3 text-sm text-amber-200">
          {notice}
        </div>
      )}
      {error && (
        <div role="alert" className="rounded-xl border border-red-500/40 bg-red-500/10 px-4 py-3 text-sm text-red-200">
          {error}
        </div>
      )}

      {/* ── Aba: Clonar disco ─────────────────────────────────────────── */}
      {tab === "clone" && (
        <div role="tabpanel" id="panel-clone" aria-labelledby="tab-clone" className="space-y-6">
          <section className="grid md:grid-cols-2 gap-6">
            <div className="space-y-2">
              <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-400">Origem</h2>
              <DiskList
                disks={disks}
                loading={disksLoading}
                renderCard={(d) => (
                  <DiskCard key={`s-${d.id}`} disk={d} selected={cloneSrc === d.path} disabled={busy} onSelect={() => toggleSource(d.path)} />
                )}
              />
            </div>
            <div className="space-y-2">
              <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-400">Destino</h2>
              <DiskList
                disks={disks}
                loading={disksLoading}
                renderCard={(d) => (
                  <DiskCard key={`t-${d.id}`} disk={d} selected={cloneDst === d.path} disabled={busy || d.isSystem} onSelect={() => toggleTarget(d.path)} />
                )}
              />
            </div>
          </section>
          <section className="rounded-xl border border-slate-700 bg-slate-900 p-4 space-y-3">
            <label className="flex items-center gap-2 text-sm">
              <input type="checkbox" checked={cloneVerify} onChange={(e) => setCloneVerify(e.target.checked)} disabled={busy} className="accent-blue-500" />
              Verificar integridade após a clonagem (BLAKE3) — recomendado
            </label>
            {jobPanel("clone")}
            <button type="button" onClick={startClone} disabled={busy || !cloneSrc || !cloneDst} className={BTN_PRIMARY}>
              {busy ? "Operação em andamento…" : "Iniciar clonagem"}
            </button>
          </section>
        </div>
      )}

      {/* ── Aba: Criar imagem ─────────────────────────────────────────── */}
      {tab === "image" && (
        <section role="tabpanel" id="panel-image" aria-labelledby="tab-image" className="space-y-4">
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

            {inconsistentWarning && (
              <p role="alert" className="rounded-lg border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-xs text-amber-200">
                ⚠ A partição selecionada está montada ({selPart?.mountPoint}) e o Live Copy está desligado: a imagem pode ficar
                inconsistente porque o volume está em uso.
              </p>
            )}

            {!smart ? (
              <DiskList
                disks={disks}
                loading={disksLoading}
                className="grid md:grid-cols-2 gap-2"
                renderCard={(d) => (
                  <DiskCard key={`i-${d.id}`} disk={d} selected={imgSrc === d.path} disabled={busy} onSelect={() => setImgSrc(imgSrc === d.path ? "" : d.path)} />
                )}
              />
            ) : disksLoading ? (
              <p role="status" className="text-sm text-slate-400">Carregando discos…</p>
            ) : allPartitions.length === 0 ? (
              <p className="text-sm text-slate-500">Nenhuma partição encontrada.</p>
            ) : (
              <div className="space-y-1 max-h-64 overflow-y-auto pr-1">
                {allPartitions.map(({ disk: d, part: p }) => {
                  const key = `${d.path}|${p.index}`;
                  return (
                    <button
                      type="button"
                      key={key}
                      disabled={busy}
                      aria-pressed={partSel === key}
                      onClick={() => setPartSel(partSel === key ? "" : key)}
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
                  placeholder="D:\backups\win11.cfimg"
                />
              </label>
              <label className="text-sm space-y-1">
                <span className="text-slate-400">Nível zstd: {level} (1 = rápido · 19 = máximo)</span>
                <input type="range" min={1} max={19} value={level} onChange={(e) => setLevel(Number(e.target.value))} disabled={busy} className="w-full accent-blue-500" />
              </label>
            </div>

            {jobPanel("image")}
            <button
              type="button"
              onClick={() => void startImage()}
              disabled={busy || (!smart && !imgSrc) || (smart && !partSel)}
              className={BTN_PRIMARY}
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
        <section role="tabpanel" id="panel-restore" aria-labelledby="tab-restore" className="space-y-4">
          <div className="rounded-xl border border-slate-700 bg-slate-900 p-4 space-y-3">
            <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-400">Imagem de origem</h2>
            <div className="space-y-1 max-h-48 overflow-y-auto pr-1">
              {images.map((im) => (
                <button
                  type="button"
                  key={im.id}
                  disabled={busy}
                  aria-pressed={imgSel === im.path}
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

            {imgInfoLoading && <p role="status" className="text-xs text-slate-400">Lendo header da imagem…</p>}
            {imgInfoError && (
              <p role="alert" className="rounded-lg border border-red-500/40 bg-red-500/10 px-3 py-2 text-xs text-red-200 font-mono break-all">
                Não foi possível ler a imagem: {imgInfoError}
              </p>
            )}
            {imgInfo && (
              <div className="text-xs font-mono rounded-lg bg-slate-950 border border-slate-800 px-3 py-2 text-slate-300">
                header OK · modo {imgInfo.mode === 0 ? "raw" : imgInfo.mode === 1 ? "smart-bitmap" : "partclone"} · origem {fmtSize(imgInfo.sourceSize)} · zstd nível {imgInfo.level} · BLAKE3 {imgInfo.hashHex.slice(0, 24)}…
              </div>
            )}

            <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-400 pt-2">Disco de destino</h2>
            <DiskList
              disks={disks}
              loading={disksLoading}
              className="grid md:grid-cols-2 gap-2"
              renderCard={(d) => (
                <DiskCard key={`r-${d.id}`} disk={d} selected={restoreDst === d.path} disabled={busy || d.isSystem} onSelect={() => setRestoreDst(restoreDst === d.path ? "" : d.path)} />
              )}
            />

            <label className="flex items-center gap-2 text-sm">
              <input type="checkbox" checked={restoreVerify} onChange={(e) => setRestoreVerify(e.target.checked)} disabled={busy} className="accent-blue-500" />
              Verificar hash gravado na imagem após restaurar — recomendado
            </label>

            {jobPanel("restore")}
            <button
              type="button"
              onClick={startRestore}
              disabled={busy || !imgSel || !imgInfo || !restoreDst}
              className={BTN_PRIMARY}
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
                    ) : h.status === "cancelled" ? (
                      <span className="text-amber-400">⚠ cancelado</span>
                    ) : h.status === "interrupted" ? (
                      <span className="text-amber-400">⚠ interrompido (app encerrado)</span>
                    ) : h.status === "running" ? (
                      <span className="text-blue-300">… em andamento</span>
                    ) : (
                      <span className="text-red-400">✖ {h.status === "error" ? "erro" : h.status}</span>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </section>

      {confirm && <ConfirmDialog req={confirm} onCancel={() => setConfirm(null)} />}
    </main>
  );
}
