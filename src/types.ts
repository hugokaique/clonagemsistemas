// Tipos espelhando o backend (serde camelCase)
export interface PartitionInfo {
  index: number;
  path?: string | null;
  sizeBytes: number;
  fs?: string | null;
  label?: string | null;
  mountPoint?: string | null;
}
export interface DiskInfo {
  id: string;
  path: string;
  model: string;
  sizeBytes: number;
  bus: string;
  isSystem: boolean;
  isRemovable: boolean;
  partitions: PartitionInfo[];
}
export type Phase =
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
  etaSeconds: number | null;
  message?: string | null;
}
export interface HistoryEntry {
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
export interface ImageEntry {
  id: number;
  path: string;
  format?: string | null;
  sizeBytes: number;
  hashHex?: string | null;
  createdAt: string;
}
export interface ImageHeaderInfo {
  valid: boolean;
  mode: number;
  sourceSize: number;
  blockSize: number;
  level: number;
  hashHex: string;
}

export type Tab = "clone" | "image" | "restore";

export const isTerminalPhase = (p: Phase) => p === "done" || p === "error" || p === "cancelled";
