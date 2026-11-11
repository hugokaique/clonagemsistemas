import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { ask, open, save } from "@tauri-apps/plugin-dialog";

import type {
  AppInfo,
  CloneProgress,
  DiskInfo,
  HistoryEntry,
  ImageEntry,
  ImageHeaderInfo,
  OutputPath,
  Phase,
  Tab,
} from "./types";
import {
  MODE_LABEL,
  SOURCE_KIND_LABEL,
  fmtDate,
  fmtSize,
  fmtUnix,
  isTerminal,
  smartBlocker,
} from "./format";
import { DiskCard, HistoryTable, ProgressPanel } from "./components";

const TABS: [Tab, string][] = [
  ["clone", "① Clonar disco"],
  ["image", "② Criar imagem (.cfimg)"],
  ["restore", "③ Restaurar imagem"],
];

const errText = (e: unknown) => (e instanceof Error ? e.message : String(e));

const startProgress = (phase: Phase, totalBytes: number): CloneProgress => ({
  phase,
  bytesDone: 0,
  totalBytes,
  percent: 0,
  speedMbps: 0,
  etaSeconds: null,
});

export default function App() {
  const [tab, setTab] = useState<Tab>("clone");
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [disks, setDisks] = useState<DiskInfo[]>([]);
  const [scanning, setScanning] = useState(false);

  // ① clonar
  const [source, setSource] = useState("");
  const [target, setTarget] = useState("");
  const [verify, setVerify] = useState(true);
  // ② imagem
  const [smart, setSmart] = useState(true);
  const [live, setLive] = useState(true);
  const [imgDisk, setImgDisk] = useState("");
  const [partSel, setPartSel] = useState(""); // "diskPath|index"
  const [imgOut, setImgOut] = useState("");
  const [level, setLevel] = useState(3);
  // ③ restaurar
  const [images, setImages] = useState<ImageEntry[]>([]);
  const [imgSel, setImgSel] = useState("");
  const [imgInfo, setImgInfo] = useState<ImageHeaderInfo | null>(null);
  const [imgInfoError, setImgInfoError] = useState<string | null>(null);
  const [restoreTarget, setRestoreTarget] = useState("");
  const [restoreVerify, setRestoreVerify] = useState(true);

  const [progress, setProgress] = useState<CloneProgress | null>(null);
  /** Aba que iniciou a operação exibida em `progress` */
  const [opTab, setOpTab] = useState<Tab>("clone");
  const [log, setLog] = useState<string[]>([]);
  const [cancelling, setCancelling] = useState(false);
  const [history, setHistory] = useState<HistoryEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const os = info?.os ?? "";
  const busy = progress !== null && !isTerminal(progress.phase);
  const srcDisk = disks.find((d) => d.path === source);
  const dstDisk = disks.find((d) => d.path === target);
  const restoreDisk = disks.find((d) => d.path === restoreTarget);

  // Cada lista é independente: uma falha na enumeração de discos não pode
  // esconder o histórico (e vice-versa).
  const refresh = useCallback(async () => {
    setScanning(true);
    const [d, h, i] = await Promise.allSettled([
      invoke<DiskInfo[]>("list_disks"),
      invoke<HistoryEntry[]>("list_history"),
      invoke<ImageEntry[]>("list_images"),
    ]);
    if (d.status === "fulfilled") setDisks(d.value);
    else setError(`Não foi possível listar os discos: ${errText(d.reason)}`);
    if (h.status === "fulfilled") setHistory(h.value);
    if (i.status === "fulfilled") setImages(i.value);
    setScanning(false);
  }, []);

  useEffect(() => {
    refresh();
    invoke<AppInfo>("app_info").then(setInfo).catch(() => undefined);
    invoke<string>("suggest_image_path").then(setImgOut).catch(() => undefined);

    const unlistenProgress = listen<CloneProgress>("clone://progress", (ev) => {
      const p = ev.payload;
      setProgress(p);
      const msg = p.message;
      if (msg) setLog((l) => (l[l.length - 1] === msg ? l : [...l, msg].slice(-40)));
      if (isTerminal(p.phase)) {
        setCancelling(false);
        setNotice(null);
        refresh();
        if (p.phase === "error") setError(msg ?? "falha na operação");
      }
    });
    const unlistenClose = listen("app://close-blocked", () => {
      setNotice("Há uma operação em andamento — cancele-a antes de fechar o CloneForge.");
    });
    return () => {
      unlistenProgress.then((f) => f());
      unlistenClose.then((f) => f());
    };
  }, [refresh]);

  // Seleções de discos que sumiram depois de um reescaneamento são limpas.
  useEffect(() => {
    const has = (p: string) => disks.some((d) => d.path === p);
    if (source && !has(source)) setSource("");
    if (target && !has(target)) setTarget("");
    if (imgDisk && !has(imgDisk)) setImgDisk("");
    if (restoreTarget && !has(restoreTarget)) setRestoreTarget("");
    if (partSel && !disks.some((d) => d.partitions.some((p) => `${d.path}|${p.index}` === partSel))) setPartSel("");
  }, [disks, source, target, imgDisk, restoreTarget, partSel]);

  // Inspeciona o header da imagem selecionada (catálogo ou arquivo externo)
  useEffect(() => {
    setImgInfo(null);
    setImgInfoError(null);
    if (!imgSel || imgSel.toLowerCase().endsWith(".dmg")) return;
    let stale = false;
    invoke<ImageHeaderInfo>("read_image_header", { path: imgSel })
      .then((h) => !stale && setImgInfo(h))
      .catch((e) => !stale && setImgInfoError(errText(e)));
    return () => {
      stale = true;
    };
  }, [imgSel]);

  const allPartitions = useMemo(
    () => disks.flatMap((d) => d.partitions.map((p) => ({ disk: d, part: p }))),
    [disks]
  );
  const selectedPart = allPartitions.find(({ disk, part }) => `${disk.path}|${part.index}` === partSel);

  // ── Ações ────────────────────────────────────────────────────────────────
  const begin = (phase: Phase, total: number) => {
    setError(null);
    setNotice(null);
    setLog([]);
    setCancelling(false);
    setOpTab(tab);
    setProgress(startProgress(phase, total));
  };

  /** Falha antes de o job começar (validação): não há evento final. */
  const failed = (e: unknown) => {
    setError(errText(e));
    setProgress((p) => (p && isTerminal(p.phase) ? p : null));
  };

  const cancel = async () => {
    setCancelling(true);
    try {
      await invoke<boolean>("cancel_operation");
    } catch (e) {
      setCancelling(false);
      setError(errText(e));
    }
  };

  const mountedNote = (d: DiskInfo) => {
    const mps = d.mounts.map((m) => m.mountPoint).filter((m) => !m.startsWith("["));
    return mps.length ? `\nOs volumes em uso no destino (${mps.join(", ")}) serão desmontados.` : "";
  };

  const startClone = async () => {
    setError(null);
    if (!srcDisk || !dstDisk) return setError("Selecione origem e destino.");
    if (dstDisk.isSystem) return setError("O destino é o disco do sistema — operação bloqueada.");
    if (dstDisk.sizeBytes < srcDisk.sizeBytes) return setError("O destino é menor que a origem.");
    const ok = await ask(
      `Origem : ${srcDisk.model} (${srcDisk.path}) — ${fmtSize(srcDisk.sizeBytes)}\n` +
        `Destino: ${dstDisk.model} (${dstDisk.path}) — ${fmtSize(dstDisk.sizeBytes)}\n\n` +
        `TODOS os dados do destino serão APAGADOS.${mountedNote(dstDisk)}\n\nContinuar?`,
      { title: "Confirmar clonagem", kind: "warning", okLabel: "Apagar destino e clonar", cancelLabel: "Cancelar" }
    );
    if (!ok) return;
    begin("preparing", srcDisk.sizeBytes);
    try {
      await invoke("start_clone", { req: { source: srcDisk.path, target: dstDisk.path, verify } });
    } catch (e) {
      failed(e);
    }
  };

  const browseOutput = async () => {
    try {
      const picked = await save({
        title: "Salvar imagem como",
        defaultPath: imgOut || undefined,
        filters: [{ name: "Imagem CloneForge", extensions: ["cfimg"] }],
      });
      if (picked) setImgOut(picked);
    } catch (e) {
      setError(errText(e));
    }
  };

  const startImage = async () => {
    setError(null);
    let src = "";
    let srcSize = 0;
    let fs: string | null = null;

    if (smart) {
      // Smart copy opera por partição/volume (bitmap do filesystem)
      if (!selectedPart) return setError("Selecione a partição de origem para o Smart Copy.");
      const blocker = smartBlocker(os, selectedPart.part);
      if (blocker) return setError(`Smart Copy indisponível para esta partição: ${blocker}. Desmarque Smart Copy para cópia raw do disco.`);
      src = selectedPart.part.path!;
      srcSize = selectedPart.part.sizeBytes;
      fs = selectedPart.part.fs ?? null;
    } else {
      const d = disks.find((x) => x.path === imgDisk);
      if (!d) return setError("Selecione o disco de origem.");
      src = d.path;
      srcSize = d.sizeBytes;
    }
    if (!imgOut.trim()) return setError("Informe o caminho de saída da imagem.");

    let output = imgOut.trim();
    try {
      // O backend resolve o caminho final (relativo → Documentos) e diz se já existe.
      const resolved = await invoke<OutputPath>("resolve_output", { path: output });
      output = resolved.path;
      if (resolved.exists) {
        const overwrite = await ask(`O arquivo já existe:\n${output}\n\nSubstituir?`, {
          title: "Sobrescrever imagem",
          kind: "warning",
          okLabel: "Substituir",
          cancelLabel: "Cancelar",
        });
        if (!overwrite) return;
      }
    } catch (e) {
      return setError(errText(e));
    }

    begin("preparing", srcSize);
    try {
      await invoke("create_image", {
        req: { source: src, output, level, smart, fs, live: smart && os === "windows" ? live : false },
      });
    } catch (e) {
      failed(e);
    }
  };

  const browseImage = async () => {
    try {
      const picked = await open({
        title: "Abrir imagem",
        multiple: false,
        directory: false,
        filters: [{ name: "Imagens CloneForge", extensions: os === "macos" ? ["cfimg", "dmg"] : ["cfimg"] }],
      });
      if (typeof picked === "string") setImgSel(picked);
    } catch (e) {
      setError(errText(e));
    }
  };

  const forgetImage = async (im: ImageEntry) => {
    try {
      await invoke("forget_image", { id: im.id });
      if (imgSel === im.path) setImgSel("");
      refresh();
    } catch (e) {
      setError(errText(e));
    }
  };

  const startRestore = async () => {
    setError(null);
    if (!imgSel) return setError("Selecione a imagem a restaurar.");
    if (!restoreDisk) return setError("Selecione o disco de destino.");
    if (restoreDisk.isSystem) return setError("O destino é o disco do sistema — operação bloqueada.");
    const partitionNote =
      imgInfo && (imgInfo.sourceKind === 2 || imgInfo.mode !== 0)
        ? "\n\nATENÇÃO: esta imagem é de uma PARTIÇÃO. Ela será gravada no início do disco, sem tabela de partições."
        : "";
    const ok = await ask(
      `Imagem : ${imgSel}\nDestino: ${restoreDisk.model} (${restoreDisk.path}) — ${fmtSize(restoreDisk.sizeBytes)}\n\n` +
        `TODOS os dados do destino serão APAGADOS.${mountedNote(restoreDisk)}${partitionNote}\n\nContinuar?`,
      { title: "Confirmar restauração", kind: "warning", okLabel: "Apagar destino e restaurar", cancelLabel: "Cancelar" }
    );
    if (!ok) return;
    begin("preparing", imgInfo?.sourceSize ?? 0);
    try {
      await invoke("restore_image", { req: { image: imgSel, target: restoreDisk.path, verify: restoreVerify } });
    } catch (e) {
      failed(e);
    }
  };

  // Motivo pelo qual um disco não pode ser destino (null = pode)
  const targetBlocker = (d: DiskInfo, minSize: number | null, otherPath: string): string | null => {
    if (d.isSystem) return null; // o badge vermelho já explica
    if (d.isReadOnly) return ""; // bloqueado; o badge "somente leitura" já aparece
    if (otherPath && d.path === otherPath) return "é a origem";
    if (minSize !== null && d.sizeBytes < minSize) return `menor que a origem (${fmtSize(minSize)})`;
    return null;
  };

  // O painel aparece na aba que iniciou a operação; nas outras, só um aviso
  // enquanto ela roda (um resultado antigo de outra aba confundiria).
  const progressPanel =
    progress &&
    (opTab === tab ? (
      <ProgressPanel progress={progress} log={log} cancelling={cancelling} onCancel={cancel} />
    ) : busy ? (
      <p className="text-xs text-amber-200">
        Há uma operação em andamento na aba {TABS.find(([t]) => t === opTab)?.[1]}.{" "}
        <button type="button" onClick={() => setTab(opTab)} className="underline hover:text-white">
          Ver progresso
        </button>
      </p>
    ) : null);
  const actionClass =
    "w-full py-2.5 rounded-xl font-semibold bg-blue-600 hover:bg-blue-500 disabled:bg-slate-800 disabled:text-slate-500 transition";

  // ── UI ───────────────────────────────────────────────────────────────────
  return (
    <main className="min-h-screen p-6 max-w-6xl mx-auto space-y-6">
      <header className="flex items-baseline justify-between gap-4">
        <h1 className="text-2xl font-bold tracking-tight">
          Clone<span className="text-blue-400">Forge</span>
          {info && <span className="ml-2 text-xs font-normal text-slate-500">v{info.version}</span>}
        </h1>
        <button
          type="button"
          onClick={refresh}
          disabled={scanning}
          className="text-sm px-3 py-1.5 rounded-lg bg-slate-800 hover:bg-slate-700 disabled:opacity-60"
        >
          {scanning ? "Escaneando…" : "↻ Reescanear"}
        </button>
      </header>

      <nav className="flex flex-wrap gap-2">
        {TABS.map(([t, label]) => (
          <button
            type="button"
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

      {notice && (
        <div className="rounded-xl border border-amber-500/40 bg-amber-500/10 px-4 py-3 text-sm text-amber-100">
          {notice}
        </div>
      )}
      {error && (
        <div className="flex items-start justify-between gap-3 rounded-xl border border-red-500/40 bg-red-500/10 px-4 py-3 text-sm text-red-200">
          <span className="break-words min-w-0">{error}</span>
          <button type="button" onClick={() => setError(null)} className="text-red-300 hover:text-white" aria-label="Fechar aviso de erro">
            ✕
          </button>
        </div>
      )}

      {/* ── Aba: Clonar disco ─────────────────────────────────────────── */}
      {tab === "clone" && (
        <>
          <section className="grid md:grid-cols-2 gap-6">
            <div className="space-y-2">
              <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-400">Origem</h2>
              {disks.map((d) => (
                <DiskCard
                  key={`s-${d.id}`}
                  disk={d}
                  selected={source === d.path}
                  disabled={busy || target === d.path}
                  blockedReason={target === d.path ? "é o destino" : null}
                  onSelect={() => setSource(d.path)}
                />
              ))}
              {disks.length === 0 && !scanning && <p className="text-sm text-slate-500">Nenhum disco encontrado.</p>}
            </div>
            <div className="space-y-2">
              <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-400">Destino</h2>
              {disks.map((d) => {
                const blocker = targetBlocker(d, srcDisk?.sizeBytes ?? null, source);
                return (
                  <DiskCard
                    key={`t-${d.id}`}
                    disk={d}
                    selected={target === d.path}
                    disabled={busy || d.isSystem || blocker !== null}
                    blockedReason={blocker}
                    onSelect={() => setTarget(d.path)}
                  />
                );
              })}
            </div>
          </section>
          <section className="rounded-xl border border-slate-700 bg-slate-900 p-4 space-y-3">
            {srcDisk && (srcDisk.isSystem || srcDisk.mounts.length > 0) && (
              <p className="text-xs text-amber-200">
                A origem está em uso{srcDisk.isSystem ? " (é o disco do sistema)" : ""}: uma clonagem raw a quente pode
                capturar arquivos pela metade. Para o volume do sistema prefira a aba ② com Smart Copy + Live Copy.
              </p>
            )}
            <label className="flex items-center gap-2 text-sm">
              <input type="checkbox" checked={verify} onChange={(e) => setVerify(e.target.checked)} disabled={busy} className="accent-blue-500" />
              Verificar integridade após a clonagem (relê o destino e compara o BLAKE3) — recomendado
            </label>
            {progressPanel}
            <button type="button" onClick={startClone} disabled={busy || !source || !target} className={actionClass}>
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
                <strong>Smart Copy</strong> — copiar apenas blocos alocados de uma partição
                <span className="text-slate-400"> (Windows: bitmap NTFS/ReFS · Linux: partclone · macOS: hdiutil)</span>
              </span>
            </label>

            {smart && os === "windows" && (
              <label className="flex items-center gap-2 text-sm pl-6">
                <input type="checkbox" checked={live} onChange={(e) => setLive(e.target.checked)} disabled={busy} className="accent-blue-500" />
                <span>
                  <strong>Live Copy</strong> — snapshot VSS antes de ler
                  <span className="text-slate-400"> (imagem consistente de volume em uso, inclusive o do sistema — recomendado)</span>
                </span>
              </label>
            )}
            {smart && os === "macos" && (
              <p className="text-xs text-slate-400 pl-6">No macOS o Smart Copy gera um <span className="font-mono">.dmg</span> (hdiutil), restaurado via asr.</p>
            )}

            {!smart ? (
              <div className="grid md:grid-cols-2 gap-2">
                {disks.map((d) => (
                  <DiskCard key={`i-${d.id}`} disk={d} selected={imgDisk === d.path} disabled={busy} onSelect={() => setImgDisk(d.path)} />
                ))}
              </div>
            ) : (
              <div className="space-y-1 max-h-64 overflow-y-auto pr-1">
                {allPartitions.length === 0 && <p className="text-sm text-slate-500">Nenhuma partição encontrada.</p>}
                {allPartitions.map(({ disk: d, part: p }) => {
                  const key = `${d.path}|${p.index}`;
                  const blocker = smartBlocker(os, p);
                  return (
                    <button
                      type="button"
                      key={key}
                      disabled={busy || blocker !== null}
                      onClick={() => setPartSel(key)}
                      aria-pressed={partSel === key}
                      className={`w-full text-left rounded-lg border px-3 py-2 text-sm transition disabled:opacity-40 disabled:cursor-not-allowed ${
                        partSel === key ? "border-blue-500 bg-blue-500/10" : "border-slate-700 bg-slate-950 hover:border-slate-500"
                      }`}
                    >
                      <span className="font-mono">{p.mountPoint ?? p.path ?? `${d.id} · partição ${p.index}`}</span>
                      <span className="text-slate-400">
                        {" "}
                        · {[p.fs, p.label].filter(Boolean).join(" · ") || "?"} · {fmtSize(p.sizeBytes)} · {d.model}
                        {blocker && <span className="text-amber-300"> — {blocker}</span>}
                      </span>
                    </button>
                  );
                })}
              </div>
            )}

            <div className="grid md:grid-cols-2 gap-3">
              <label className="text-sm space-y-1">
                <span className="text-slate-400">Arquivo de saída</span>
                <span className="flex gap-2">
                  <input
                    value={imgOut}
                    onChange={(e) => setImgOut(e.target.value)}
                    disabled={busy}
                    spellCheck={false}
                    className="w-full min-w-0 rounded-lg bg-slate-950 border border-slate-700 px-3 py-2 font-mono text-sm"
                    placeholder={os === "windows" ? "D:\\backups\\win11.cfimg" : "/backups/sistema.cfimg"}
                  />
                  <button type="button" onClick={browseOutput} disabled={busy} className="px-3 rounded-lg bg-slate-800 hover:bg-slate-700 text-sm whitespace-nowrap disabled:opacity-60">
                    Procurar…
                  </button>
                </span>
              </label>
              <label className="text-sm space-y-1">
                <span className="text-slate-400">Nível zstd: {level} (1 = rápido · 19 = máximo)</span>
                <input type="range" min={1} max={19} value={level} onChange={(e) => setLevel(Number(e.target.value))} disabled={busy} className="w-full accent-blue-500" />
              </label>
            </div>

            {progressPanel}
            <button type="button" onClick={startImage} disabled={busy || (smart ? !partSel : !imgDisk)} className={actionClass}>
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
                    <tr>
                      <td colSpan={4} className="px-3 py-4 text-center text-slate-500">Nenhuma imagem catalogada.</td>
                    </tr>
                  )}
                  {images.map((im) => (
                    <tr key={im.id} className={`border-t border-slate-800 ${im.exists ? "" : "text-slate-500"}`}>
                      <td className="px-3 py-2 whitespace-nowrap">{fmtDate(im.createdAt)}</td>
                      <td className="px-3 py-2 font-mono truncate max-w-[320px]" title={im.path}>
                        {im.path}
                        {!im.exists && <span className="text-amber-300"> (arquivo não encontrado)</span>}
                      </td>
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
            <div className="flex items-center justify-between gap-3">
              <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-400">Imagem de origem</h2>
              <button type="button" onClick={browseImage} disabled={busy} className="text-sm px-3 py-1.5 rounded-lg bg-slate-800 hover:bg-slate-700 disabled:opacity-60">
                Abrir arquivo…
              </button>
            </div>
            <div className="space-y-1 max-h-48 overflow-y-auto pr-1">
              {imgSel && !images.some((im) => im.path === imgSel) && (
                <div className="rounded-lg border border-blue-500 bg-blue-500/10 px-3 py-2 text-sm">
                  <span className="font-mono break-all">{imgSel}</span>
                  <span className="text-slate-400"> · arquivo externo</span>
                </div>
              )}
              {images.map((im) => (
                <div key={im.id} className="flex gap-2">
                  <button
                    type="button"
                    disabled={busy || !im.exists}
                    onClick={() => setImgSel(im.path)}
                    aria-pressed={imgSel === im.path}
                    className={`flex-1 min-w-0 text-left rounded-lg border px-3 py-2 text-sm transition disabled:opacity-40 disabled:cursor-not-allowed ${
                      imgSel === im.path ? "border-blue-500 bg-blue-500/10" : "border-slate-700 bg-slate-950 hover:border-slate-500"
                    }`}
                  >
                    <span className="font-mono break-all">{im.path}</span>
                    <span className="text-slate-400">
                      {" "}
                      · {im.format ?? "?"} · {fmtSize(im.sizeBytes)}
                      {!im.exists && <span className="text-amber-300"> — arquivo não encontrado</span>}
                    </span>
                  </button>
                  {!im.exists && (
                    <button type="button" onClick={() => forgetImage(im)} disabled={busy} className="text-xs px-2 rounded-lg bg-slate-800 hover:bg-slate-700 whitespace-nowrap">
                      Remover do catálogo
                    </button>
                  )}
                </div>
              ))}
              {images.length === 0 && !imgSel && (
                <p className="text-sm text-slate-500">Catálogo vazio — crie uma imagem na aba ② ou use “Abrir arquivo…”.</p>
              )}
            </div>

            {imgInfoError && (
              <div className="text-xs rounded-lg bg-red-500/10 border border-red-500/40 px-3 py-2 text-red-200">
                Imagem inválida: {imgInfoError}
              </div>
            )}
            {imgInfo && (
              <div className="text-xs font-mono rounded-lg bg-slate-950 border border-slate-800 px-3 py-2 text-slate-300 space-y-0.5">
                <div>
                  header OK · {SOURCE_KIND_LABEL[imgInfo.sourceKind] ?? "?"}
                  {imgInfo.label ? ` · ${imgInfo.label}` : ""}
                </div>
                <div>
                  modo {MODE_LABEL[imgInfo.mode] ?? imgInfo.mode} · conteúdo {fmtSize(imgInfo.sourceSize)} · arquivo{" "}
                  {fmtSize(imgInfo.fileSize)} · zstd nível {imgInfo.level}
                  {imgInfo.createdUnix > 0 ? ` · criada em ${fmtUnix(imgInfo.createdUnix)}` : ""}
                </div>
                <div className="break-all">BLAKE3 {imgInfo.hashHex}</div>
              </div>
            )}
            {imgInfo && (imgInfo.sourceKind === 2 || imgInfo.mode !== 0) && (
              <p className="text-xs text-amber-200">
                Esta imagem é de uma partição/volume. Restaurada em um disco inteiro ela ocupa o início do disco, sem
                tabela de partições — use para discos removíveis ou para inspeção; para clonar um sistema inicializável
                prefira a imagem do disco inteiro (Smart Copy desmarcado).
              </p>
            )}

            <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-400 pt-2">Disco de destino</h2>
            <div className="grid md:grid-cols-2 gap-2">
              {disks.map((d) => {
                const need = imgInfo && imgInfo.mode !== 2 ? imgInfo.sourceSize : null;
                const blocker = d.isSystem
                  ? null
                  : d.isReadOnly
                  ? "" // bloqueado; o badge "somente leitura" já aparece
                  : need !== null && d.sizeBytes < need
                  ? `menor que a imagem (${fmtSize(need)})`
                  : null;
                return (
                  <DiskCard
                    key={`r-${d.id}`}
                    disk={d}
                    selected={restoreTarget === d.path}
                    disabled={busy || d.isSystem || blocker !== null}
                    blockedReason={blocker}
                    onSelect={() => setRestoreTarget(d.path)}
                  />
                );
              })}
            </div>

            <label className="flex items-center gap-2 text-sm">
              <input type="checkbox" checked={restoreVerify} onChange={(e) => setRestoreVerify(e.target.checked)} disabled={busy} className="accent-blue-500" />
              Verificar após restaurar (relê o destino e compara com o hash da imagem) — recomendado
            </label>
            <p className="text-xs text-slate-500">A integridade da própria imagem é sempre conferida durante a restauração.</p>

            {progressPanel}
            <button type="button" onClick={startRestore} disabled={busy || !imgSel || !restoreTarget || imgInfoError !== null} className={actionClass}>
              {busy ? "Operação em andamento…" : "Restaurar imagem"}
            </button>
          </div>
        </section>
      )}

      {/* ── Histórico (sempre visível) ────────────────────────────────── */}
      <section className="space-y-2">
        <h2 className="text-sm font-semibold uppercase tracking-wide text-slate-400">Histórico (SQLite local)</h2>
        <HistoryTable history={history} />
        {info?.logPath && (
          <p className="text-[11px] text-slate-600 font-mono break-all">log: {info.logPath}</p>
        )}
      </section>
    </main>
  );
}
