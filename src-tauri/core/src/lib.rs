//! cloneforge-core — motor do CloneForge, independente de GUI.
//!
//! Módulos:
//! - `disk`:      enumeração de discos físicos/partições por SO, com marcação de disco de sistema.
//! - `safety`:    validação fail-closed de origem/destino (usada pela GUI e pelo helper).
//! - `prepare`:   trava/desmonta os volumes do destino antes da gravação raw.
//! - `clone`:     motor de cópia em blocos + verificação BLAKE3.
//! - `image`:     imagens `.cfimg` (zstd + BLAKE3), criação e restauração.
//! - `smart`:     Smart Copy (bitmap NTFS / partclone / hdiutil).
//! - `vss`:       snapshots VSS (Windows) para cópia a quente.
//! - `privilege`: elevação via pkexec (Linux).
//! - `exec`:      roteia cada operação (local × helper elevado).
//! - `progress`:  telemetria e cancelamento.
//!
//! Todo o código específico de sistema operacional do projeto vive neste
//! crate; o app Tauri e o helper só o consomem.

pub mod clone;
pub mod disk;
pub mod exec;
pub mod image;
pub mod io;
pub mod prepare;
pub mod privilege;
pub mod progress;
pub mod safety;
pub mod smart;
pub mod vss;
