// Backend simulado para desenvolvimento da UI no navegador (`npm run dev`
// fora do Tauri). NUNCA entra no build de produção — ver main.tsx.
//
// Simula a enumeração de discos, o histórico, o catálogo e uma operação com
// progresso em fases (incluindo cancelamento), sem tocar em nenhum disco.

import { emit } from "@tauri-apps/api/event";
import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import type { CloneProgress, DiskInfo, HistoryEntry, ImageEntry, ImageHeaderInfo, Phase } from "./types";

const GB = 1_073_741_824;

const disks: DiskInfo[] = [
  {
    id: "PhysicalDrive1",
    path: "\\\\.\\PhysicalDrive1",
    model: "KINGSTON SNV2S1000G",
    sizeBytes: 931.5 * GB,
    bus: "NVMe",
    isSystem: true,
    isRemovable: false,
    isReadOnly: false,
    sectorSize: 512,
    partitions: [
      { index: 1, path: "\\\\?\\Volume{11111111-2222-3333-4444-555555555555}", sizeBytes: 100 * 1_048_576, fs: "FAT32", label: "System" },
      { index: 2, path: null, sizeBytes: 16 * 1_048_576, fs: null, label: "Reserved" },
      { index: 3, path: "\\\\.\\C:", sizeBytes: 930.5 * GB, fs: "NTFS", label: "Windows", mountPoint: "C:\\" },
    ],
    mounts: [{ device: "\\\\.\\C:", mountPoint: "C:\\" }],
  },
  {
    id: "PhysicalDrive0",
    path: "\\\\.\\PhysicalDrive0",
    model: "Samsung SSD 990 PRO 2TB",
    sizeBytes: 1907.7 * GB,
    bus: "NVMe",
    isSystem: false,
    isRemovable: false,
    isReadOnly: false,
    sectorSize: 512,
    partitions: [{ index: 1, path: "\\\\.\\D:", sizeBytes: 1907.7 * GB, fs: "NTFS", label: "Dados", mountPoint: "D:\\" }],
    mounts: [{ device: "\\\\.\\D:", mountPoint: "D:\\" }],
  },
  {
    id: "PhysicalDrive2",
    path: "\\\\.\\PhysicalDrive2",
    model: "SanDisk Cruzer Fit",
    sizeBytes: 14.3 * GB,
    bus: "USB",
    isSystem: false,
    isRemovable: true,
    isReadOnly: false,
    sectorSize: 512,
    partitions: [{ index: 1, path: "\\\\.\\E:", sizeBytes: 14.3 * GB, fs: "exFAT", label: "PENDRIVE", mountPoint: "E:\\" }],
    mounts: [{ device: "\\\\.\\E:", mountPoint: "E:\\" }],
  },
  {
    id: "PhysicalDrive3",
    path: "\\\\.\\PhysicalDrive3",
    model: "Leitor SD (protegido)",
    sizeBytes: 29.7 * GB,
    bus: "SD",
    isSystem: false,
    isRemovable: true,
    isReadOnly: true,
    sectorSize: 512,
    partitions: [],
    mounts: [],
  },
];

const HASH = "842f3ff0d9881c874e0214c5e1d2367dd13bf8eb7074a92e2c3f2e80ee1bad06";

const history: HistoryEntry[] = [
  { id: 4, kind: "restore", source: "D:\\backups\\win11.cfimg", target: "\\\\.\\PhysicalDrive2", startedAt: "2026-11-10T14:02:11Z", finishedAt: "2026-11-10T14:09:48Z", bytesWritten: 14.3 * GB, hashAlgo: "BLAKE3", hashHex: HASH, verifyOk: true, status: "ok" },
  { id: 3, kind: "image", source: "\\\\.\\C:", target: "D:\\backups\\win11.cfimg", startedAt: "2026-11-10T12:30:00Z", finishedAt: "2026-11-10T12:30:41Z", bytesWritten: 0, hashAlgo: "BLAKE3", hashHex: null, verifyOk: null, status: "cancelled", message: "operação cancelada pelo usuário" },
  { id: 2, kind: "clone", source: "\\\\.\\PhysicalDrive2", target: "\\\\.\\PhysicalDrive0", startedAt: "2026-11-09T09:15:00Z", finishedAt: "2026-11-09T09:15:20Z", bytesWritten: 0, hashAlgo: "BLAKE3", hashHex: null, verifyOk: null, status: "error", message: "escrita no destino @ offset 1048576: Acesso negado." },
  // linha no formato da v0.1.0 (data SQLite, início = fim)
  { id: 1, kind: "clone", source: "\\\\.\\PhysicalDrive2", target: "\\\\.\\PhysicalDrive0", startedAt: "2026-10-26T18:40:02+00:00", finishedAt: "2026-10-26 18:40:02", bytesWritten: 14.3 * GB, hashAlgo: "BLAKE3", hashHex: HASH, verifyOk: true, status: "ok" },
];

const images: ImageEntry[] = [
  { id: 2, path: "D:\\backups\\win11.cfimg", format: "cfimg", sizeBytes: 38.2 * GB, hashHex: HASH, createdAt: "2026-11-10T12:58:03Z", exists: true },
  { id: 1, path: "E:\\antiga\\pendrive.cfimg", format: "cfimg", sizeBytes: 3.1 * GB, hashHex: HASH, createdAt: "2026-10-19 12:00:00", exists: false },
];

const header: ImageHeaderInfo = {
  valid: true,
  mode: 1,
  sourceSize: 12.4 * GB,
  blockSize: 8 * 1_048_576,
  level: 3,
  hashHex: HASH,
  sourceKind: 2,
  createdUnix: 1_794_315_483,
  label: "C:\\ — KINGSTON SNV2S1000G (PhysicalDrive1)",
  fileSize: 38.2 * GB,
};

let cancelRequested = false;
let running = false;

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Simula uma operação: emite progresso por fases e termina em done/cancelled. */
async function simulate(phases: Phase[], total: number, doneMessage: string, notes: string[] = []): Promise<string> {
  if (running) throw "já existe uma operação em andamento — aguarde o término ou cancele antes de iniciar outra";
  running = true;
  cancelRequested = false;
  const send = (p: Partial<CloneProgress> & { phase: Phase }) =>
    emit("clone://progress", { bytesDone: 0, totalBytes: total, percent: 0, speedMbps: 0, etaSeconds: null, message: null, ...p });
  try {
    await send({ phase: "preparing" });
    for (const note of notes) {
      await sleep(250);
      await send({ phase: "preparing", message: note });
    }
    for (const phase of phases) {
      const steps = 25;
      for (let i = 1; i <= steps; i++) {
        await sleep(160);
        if (cancelRequested) {
          await send({ phase: "cancelled", message: "operação cancelada pelo usuário" });
          throw "operação cancelada pelo usuário";
        }
        const done = (total * i) / steps;
        await send({ phase, bytesDone: done, percent: (i / steps) * 100, speedMbps: 1180 + Math.random() * 240, etaSeconds: ((steps - i) * 160) / 1000 });
      }
    }
    await send({ phase: "done", bytesDone: total, percent: 100, message: doneMessage });
    return HASH;
  } finally {
    running = false;
  }
}

export function installDevMock() {
  mockWindows("main");
  mockIPC(
    async (cmd, payload) => {
      const args = (payload ?? {}) as Record<string, any>;
      switch (cmd) {
        case "app_info":
          return { os: "windows", version: "0.1.0-dev", logPath: "C:\\Users\\dev\\AppData\\Roaming\\com.cloneforge.app\\cloneforge.log" };
        case "suggest_image_path":
          return "C:\\Users\\dev\\Documents\\cloneforge-20261111-1030.cfimg";
        case "resolve_output":
          return { path: args.path, exists: String(args.path).includes("win11") };
        case "list_disks":
          await sleep(300);
          return disks;
        case "list_history":
          return history;
        case "list_images":
          return images;
        case "forget_image":
          images.splice(images.findIndex((i) => i.id === args.id), 1);
          return null;
        case "read_image_header":
          if (String(args.path).includes("ruim")) throw "não é um arquivo .cfimg (magic inválido)";
          return header;
        case "cancel_operation":
          cancelRequested = true;
          return running;
        case "start_clone": {
          const total = disks.find((d) => d.path === args.req.source)?.sizeBytes ?? GB;
          void simulate(args.req.verify ? ["copying", "flushing", "verifying"] : ["copying", "flushing"], total, `clonagem OK (verificada) — BLAKE3 ${HASH}`, [
            "destino: volume \\\\?\\Volume{99999999-0000-0000-0000-100000000000} travado e desmontado",
          ]).catch(() => undefined);
          return "job-simulado";
        }
        case "create_image":
          return simulate(["compressing", "flushing"], 12.4 * GB, `imagem OK — 12.4 GiB → 5.1 GiB (41.2%) em ${args.req.output} · BLAKE3 ${HASH}`, [
            "criando snapshot VSS do volume…",
            "VSS snapshot {3F2504E0-4F89-11D3-9A0C-0305E82C3301} criado",
            "smart copy: 3251200/243924992 clusters alocados (1.3% do volume, cluster de 4096 bytes)",
          ]);
        case "restore_image":
          return simulate(args.req.verify ? ["restoring", "flushing", "verifying"] : ["restoring", "flushing"], 12.4 * GB, `restore OK (verificado por releitura do destino) — BLAKE3 ${HASH}`);
        // plugin de diálogos: sempre "confirma" (devolve o rótulo do botão OK)
        // e devolve caminhos fixos
        case "plugin:dialog|message": {
          const b = args.buttons;
          if (b && typeof b === "object") return b.OkCancelCustom?.[0] ?? b.OkCustom ?? b.YesNoCancelCustom?.[0] ?? "Ok";
          return b === "YesNo" || b === "YesNoCancel" ? "Yes" : "Ok";
        }
        case "plugin:dialog|save":
          return "D:\\backups\\nova-imagem.cfimg";
        case "plugin:dialog|open":
          return "Z:\\rede\\externa.cfimg";
        default:
          console.warn("[devMock] comando não simulado:", cmd, args);
          return null;
      }
    },
    { shouldMockEvents: true }
  );
  console.info("[devMock] backend simulado ativo — nenhum disco real é acessado");
}
