# CloneForge

**Clonagem, backup e deploy de sistemas operacionais — multiplataforma, nativo e open source.**

Alternativa moderna ao balenaEtcher, Rescuezilla e Macrium Reflect: clone discos inteiros bit a bit, gere imagens compactadas com verificação criptográfica e restaure em qualquer máquina — tudo em um app desktop leve, com UI em tempo real e auditoria local de cada operação.

[![Release](https://img.shields.io/github/v/release/hugokaique/clonagemsistemas)](https://github.com/hugokaique/clonagemsistemas/releases/latest)
[![CI](https://github.com/hugokaique/clonagemsistemas/actions/workflows/ci.yml/badge.svg)](https://github.com/hugokaique/clonagemsistemas/actions/workflows/ci.yml)
[![Release build](https://github.com/hugokaique/clonagemsistemas/actions/workflows/release.yml/badge.svg)](https://github.com/hugokaique/clonagemsistemas/actions/workflows/release.yml)

---

## Download (v0.1.0)

| Plataforma | Pacote | Tamanho |
|---|---|---|
| 🪟 **Windows 10/11 (x64)** | [Instalador NSIS `.exe`](https://github.com/hugokaique/clonagemsistemas/releases/latest/download/CloneForge_0.1.0_x64-setup.exe) | 2,9 MB |
| | [MSI `.msi` (pt-BR)](https://github.com/hugokaique/clonagemsistemas/releases/latest/download/CloneForge_0.1.0_x64_pt-BR.msi) · [MSI (en-US)](https://github.com/hugokaique/clonagemsistemas/releases/latest/download/CloneForge_0.1.0_x64_en-US.msi) | 4,0 MB |
| 🐧 **Linux (x86_64)** | [`.deb`](https://github.com/hugokaique/clonagemsistemas/releases/latest/download/CloneForge_0.1.0_amd64.deb) · [`.rpm`](https://github.com/hugokaique/clonagemsistemas/releases/latest/download/CloneForge-0.1.0-1.x86_64.rpm) · [AppImage](https://github.com/hugokaique/clonagemsistemas/releases/latest/download/CloneForge_0.1.0_amd64.AppImage) | 4,2–82 MB |
| 🍎 **macOS (Universal: Intel + Apple Silicon)** | [`.dmg`](https://github.com/hugokaique/clonagemsistemas/releases/latest/download/CloneForge_0.1.0_universal.dmg) | 7,8 MB |

> **Notas de instalação**
> - **Windows:** o app exige privilégio de Administrador (UAC) na abertura — necessário para acesso a disco bruto.
> - **Linux:** a primeira operação de escrita pede elevação via Polkit (`pkexec`).
> - **macOS:** o `.dmg` é **não assinado** (build ad-hoc). Na primeira abertura: clique com o botão direito → *Abrir* → *Abrir*, ou libere em *Ajustes do Sistema → Privacidade e Segurança*. Veja [Assinatura do macOS](#assinatura-do-macos) para builds assinados/notarizados.

---

## Funcionalidades

- **Enumeração de discos** — lista discos físicos e partições com modelo, tamanho, barramento (NVMe/SATA/USB) e flag de removível.
- **Proteção anti-desastre** — o disco do sistema operacional é detectado e **bloqueado como destino** no backend (não só na UI).
- **Clonagem raw (bit a bit)** — disco → disco, disco → imagem, imagem → disco, com buffer otimizado para NVMe/SSD.
- **Progresso em tempo real** — porcentagem, velocidade em MB/s e tempo estimado, emitidos como eventos para a UI.
- **Verificação criptográfica** — hash **BLAKE3** calculado on-the-fly durante a cópia e revalidado após a escrita (`flushing → verifying → done`).
- **Imagens `.cfimg`** — compressão **Zstandard** com o hash da origem gravado no container; restore com verificação automática.
- **Histórico e auditoria local** — cada operação (sucesso, erro, hashes, duração) registrada em **SQLite** (modo WAL), consultável na aba Histórico.
- **Multiplataforma nativo** — Windows, Linux e macOS (Universal Binary: Intel `x86_64` + Apple Silicon `arm64` em um único `.dmg`).

### Roteiro (roadmap)

- [ ] Smart Copy (copiar apenas blocos alocados — NTFS via bitmap, ext4/XFS/Btrfs via `FIEMAP`/`partclone`)
- [ ] Clonagem a quente do sistema em execução (Windows VSS; macOS `asr` com snapshots APFS)
- [ ] Suporte a imagens de terceiros (`.iso`, `.img`, `.wim` via WIMGAPI/DISM)
- [ ] Deploy em lote (uma imagem → N destinos)
- [ ] Assinatura de código: Authenticode (Windows) e Developer ID + notarização (macOS)

---

## Arquitetura

```
┌─────────────────────────────────────────────────────────┐
│  Frontend — React 18 + Vite + Tailwind CSS              │
│  (abas: Clonar · Imagem · Histórico; eventos Tauri)     │
└───────────────────────┬─────────────────────────────────┘
                        │ IPC (comandos + eventos de progresso)
┌───────────────────────▼─────────────────────────────────┐
│  Core — Rust + Tokio (assíncrono, I/O em blocos)        │
│  disk.rs · clone.rs · image.rs · smart.rs · db.rs       │
│  vss.rs (Windows) · privilege.rs (elevação por SO)      │
└───────┬─────────────────────────────┬───────────────────┘
        │ app (UAC/raiz total)        │ helper (crate separado,
        │ operações em disco          │ protocolo JSON-lines,
        ▼                             ▼ testável headless)
   \\.\PhysicalDriveX            /dev/sdX · /dev/nvmeXn1 · /dev/rdiskX
```

**Por que esta stack?** Comparada a Electron (200+ MB de instalador, ~500 MB de RAM) e C++/Qt (produtividade baixa, tooling fragmentado), **Tauri v2 + Rust** entrega instalador de **~3 MB**, consumo de RAM na casa de **dezenas de MB**, I/O de disco nativo sem GC e acesso direto às APIs de baixo nível de cada SO — medido em **>1 GB/s** de throughput de clonagem em NVMe (ver Testes).

| Camada | Tecnologia |
|---|---|
| Framework desktop | Tauri v2 (WebView nativa do SO) |
| Core engine | Rust + Tokio (I/O assíncrono em blocos) |
| UI | React 18 + Vite + Tailwind CSS |
| Banco local | SQLite via `rusqlite` (modo WAL) |
| Compressão | Zstandard (`zstd`, nível 3) |
| Checksum | BLAKE3 (on-the-fly, durante a cópia) |

### Modelo de privilégio por SO

| SO | Acesso a disco | Elevação | Particularidades |
|---|---|---|---|
| **Windows** | `\\.\PhysicalDriveX` | Manifesto `requireAdministrator` (UAC na abertura) | VSS para snapshots; NTFS/ReFS/FAT32 |
| **Linux** | `/dev/sdX`, `/dev/nvmeXn1` | Polkit (`pkexec`) por operação | GPT/MBR (`sfdisk`), ext4/XFS/Btrfs/LVM |
| **macOS** | `/dev/rdiskX` (raw) | `osascript` with administrator privileges | Containers APFS; boot Intel (EFI) vs Apple Silicon (iBoot, volumes selados); `asr` |

---

## Desenvolvimento

### Pré-requisitos

- **Node.js 22+** e **Rust stable** (`rustup`)
- Windows: MSVC Build Tools 2022 · Linux: `sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev build-essential pkg-config libssl-dev` · macOS: Xcode Command Line Tools

### Subir o ambiente

```bash
# Windows (PowerShell)
.\bootstrap.ps1

# Linux / macOS
./bootstrap.sh
```

O bootstrap gera a árvore de diretórios, instala dependências do frontend e valida o toolchain Rust.

### Rodar em modo dev

```bash
npm run tauri dev
```

### Build local

```bash
npm run tauri build -- --bundles msi,nsis          # Windows
npm run tauri build -- --bundles deb,rpm,appimage  # Linux
npm run tauri build -- --target universal-apple-darwin --bundles app,dmg  # macOS universal
```

### Testes headless do motor (sem GUI)

O binário `cloneforge-helper` fala JSON-lines em stdin/stdout — permite testar o motor em CI ou via shell:

```bash
echo '{"job":"clone","request":{"source":"origem.bin","target":"destino.bin","sourceSize":52181648,"blockSize":null,"verify":true}}' \
  | src-tauri/target/debug/cloneforge-helper run
```

### CI/CD

- **CI** (`ci.yml`): lint + build em push para `main` (clippy com `-D warnings`).
- **Release** (`release.yml`): tag `v*` → build em matrix (Windows / Ubuntu 22.04 / macOS) e publicação direta dos 7 artefatos no GitHub Release. Repositório público = minutos de Actions ilimitados.

### Assinatura do macOS

O `.dmg` publicado é **ad-hoc** (sem certificado). Para distribuir assinado e notarizado, cadastre os secrets no repositório e reative o bloco `env` do step macOS em `release.yml`:

`APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`

---

## Testes de aceitação

Validação headless já executada (massa sintética de 52 MB, detalhes em [`TESTES_ACEITACAO.md`](TESTES_ACEITACAO.md)):

- ✅ Clone file→file com verificação: **1.033 MB/s**, destino byte-idêntico (BLAKE3)
- ✅ Imagem `.cfimg` zstd: 52,2 MB → 33,6 MB; restore **bit-idêntico**
- ✅ Teste negativo: 1 byte corrompido no frame zstd → **detectado** na verificação pós-restore

Os testes com GUI e hardware real (pendrives, boot de clone) seguem o roteiro no mesmo arquivo.

---

## Licença

[MIT](LICENSE) — use, modifique e distribua livremente.
