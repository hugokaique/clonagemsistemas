// Tipos espelhando o backend (serde camelCase)

export interface PartitionInfo {
  index: number;
  /** Caminho de I/O do volume: `\\.\C:` · `\\?\Volume{guid}` · `/dev/sdb1` · `/dev/disk4s2` */
  path?: string | null;
  /** Windows: identidade do volume (GUID); ausente nos demais SOs */
  volume?: string | null;
  sizeBytes: number;
  offsetBytes?: number | null;
  fs?: string | null;
  label?: string | null;
  mountPoint?: string | null;
}

export interface MountInfo {
  device: string;
  mountPoint: string;
}

export interface DiskInfo {
  id: string;
  path: string;
  model: string;
  sizeBytes: number;
  bus: string;
  isSystem: boolean;
  isRemovable: boolean;
  isReadOnly: boolean;
  sectorSize: number;
  partitions: PartitionInfo[];
  mounts: MountInfo[];
}

export type Phase =
  | "preparing"
  | "copying"
  | "flushing"
  | "verifying"
  | "compressing"
  | "restoring"
  | "done"
  | "error"
  | "cancelled";

export interface CloneProgress {
  phase: Phase;
  bytesDone: number;
  totalBytes: number;
  percent: number;
  speedMbps: number;
  /** null = desconhecido */
  etaSeconds: number | null;
  message?: string | null;
}

export type JobKind = "clone" | "image" | "restore";

export interface HistoryEntry {
  id: number;
  kind: JobKind | string;
  source: string;
  target: string;
  startedAt: string;
  finishedAt?: string | null;
  bytesWritten: number;
  hashAlgo: string;
  hashHex?: string | null;
  verifyOk?: boolean | null;
  /** running | ok | error | cancelled | interrupted */
  status: string;
  message?: string | null;
}

export interface ImageEntry {
  id: number;
  path: string;
  format?: string | null;
  sizeBytes: number;
  hashHex?: string | null;
  createdAt: string;
  /** false = arquivo movido/apagado depois de catalogado */
  exists: boolean;
}

export interface ImageHeaderInfo {
  valid: boolean;
  /** 0 = raw · 1 = smart-bitmap (Windows) · 2 = partclone (Linux) */
  mode: number;
  sourceSize: number;
  blockSize: number;
  level: number;
  hashHex: string;
  /** 0 = desconhecido · 1 = disco inteiro · 2 = partição/volume */
  sourceKind: number;
  /** segundos Unix; 0 = desconhecida */
  createdUnix: number;
  label: string;
  fileSize: number;
}

export interface AppInfo {
  os: "windows" | "linux" | "macos" | string;
  version: string;
  logPath: string;
}

export interface OutputPath {
  path: string;
  exists: boolean;
}

export type Tab = "clone" | "image" | "restore";
