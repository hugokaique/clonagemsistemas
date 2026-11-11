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
> - **Linux:** cada operação em disco pede elevação via Polkit (`pkexec`). Prefira o `.deb`/`.rpm`: eles instalam o helper privilegiado e a policy do Polkit.
> - **macOS:** o `.dmg` é **não assinado** (build ad-hoc). Na primeira abertura: clique com o botão direito → *Abrir* → *Abrir*, ou libere em *Ajustes do Sistema → Privacidade e Segurança*. Veja [Assinatura do macOS](#assinatura-do-macos) para builds assinados/notarizados. Operações em disco exigem executar como root (ver [Limitações conhecidas](#limitações-conhecidas)).

---

## Funcionalidades

- **Enumeração de discos** — discos físicos e partições com modelo, tamanho, barramento (NVMe/SATA/USB), filesystem, volumes em uso e flags de removível/somente-leitura.
- **Proteção anti-desastre (fail-closed)** — o disco do sistema nunca é aceito como destino, em nenhuma grafia de caminho (`\\.\physicaldrive0`, `//./…`, symlinks, partições). O destino precisa ser um disco físico presente na enumeração; se a enumeração falhar, a operação é abortada. A validação roda no backend e de novo no helper privilegiado.
- **Clonagem raw (bit a bit)** — disco → disco, em blocos de 8 MiB, com os volumes do destino travados/desmontados antes da gravação. No Windows a suspensão automática fica bloqueada enquanto a operação roda.
- **Imagens `.cfimg`** — compressão **Zstandard** multi-thread com checksum de frame e hash **BLAKE3** do conteúdo gravado no container. Gravação atômica (`.part` → nome final): falha ou cancelamento não deixa imagem truncada.
- **Smart Copy** — copia só os blocos alocados de uma partição: bitmap NTFS/ReFS no Windows, `partclone` no Linux, `hdiutil` (`.dmg`) no macOS.
- **Live Copy (Windows)** — snapshot **VSS** para gerar imagem consistente de um volume em uso, inclusive o do sistema.
- **Verificação de ponta a ponta** — a integridade da imagem é sempre conferida no restore; com "verificar", o destino é **relido** e comparado ao hash (pega erro de gravação na mídia).
- **Progresso e cancelamento** — porcentagem, velocidade, ETA e log da operação em tempo real; botão **Cancelar** em qualquer fase; uma operação por vez; fechar a janela é bloqueado enquanto algo está gravando.
- **Histórico e auditoria local** — cada operação (clonagem, imagem, restauração) com início, duração, hash e resultado (`ok`, `erro`, `cancelado`, `interrompido`) em **SQLite** (modo WAL).
- **Multiplataforma nativo** — Windows, Linux e macOS (Universal Binary: Intel `x86_64` + Apple Silicon `arm64` em um único `.dmg`).

### Limitações conhecidas

- **macOS:** ainda não há elevação automática de privilégio. Operações em disco bruto exigem executar o binário como root (`sudo /Applications/CloneForge.app/Contents/MacOS/cloneforge`) e conceder *Acesso Total ao Disco*.
- **Restauração de imagem de partição:** o destino é sempre um disco inteiro. Uma imagem Smart Copy (partição) é gravada no início do disco, sem tabela de partições — para um clone inicializável use a imagem do disco inteiro (Smart Copy desmarcado).
- **Smart Copy sem Live Copy** em volume montado pode capturar arquivos em alteração; no Windows deixe o Live Copy ligado.

### Roteiro (roadmap)

- [x] Smart Copy (NTFS/ReFS via bitmap, ext4/XFS/Btrfs/NTFS/FAT/exFAT via `partclone`, APFS via `hdiutil`)
- [x] Imagem a quente do volume em execução (Windows VSS)
- [ ] Elevação automática no macOS (helper privilegiado)
- [ ] Restaurar imagem de partição em uma partição de destino
- [ ] Suporte a imagens de terceiros (`.iso`, `.img`, `.wim` via WIMGAPI/DISM)
- [ ] Deploy em lote (uma imagem → N destinos)
- [ ] Assinatura de código: Authenticode (Windows) e Developer ID + notarização (macOS)

---

## Arquitetura

```
┌──────────────────────────────────────────────────────────────┐
│  Frontend — React 18 + Vite + Tailwind CSS                   │
│  (abas: Clonar · Criar imagem · Restaurar; histórico)        │
└───────────────────────┬──────────────────────────────────────┘
                        │ IPC (comandos + eventos de progresso)
┌───────────────────────▼──────────────────────────────────────┐
│  App Tauri — src-tauri/src                                   │
│  commands.rs · jobs.rs (1 operação por vez, cancelamento)    │
│  db.rs (SQLite: histórico, catálogo, auditoria)              │
└───────────────────────┬──────────────────────────────────────┘
                        │
┌───────────────────────▼──────────────────────────────────────┐
│  cloneforge-core — src-tauri/core  (sem dependência de GUI)  │
│  disk · safety · prepare · clone · image · smart · vss       │
│  exec (local × helper) · privilege (pkexec) · progress       │
└───────┬──────────────────────────────┬───────────────────────┘
        │ execução local               │ cloneforge-helper (Linux, root via
        │ (Windows elevado / root)     │ pkexec) — linka só o core,
        ▼                              ▼ protocolo JSON-lines, testável headless
   \\.\PhysicalDriveX            /dev/sdX · /dev/nvmeXn1 · /dev/rdiskX
```

Todo o código específico de sistema operacional vive no crate `cloneforge-core`, que não depende do Tauri: o helper que roda como root não carrega a pilha de GUI, e o motor é verificado nos três SOs pelo CI.

**Por que esta stack?** Comparada a Electron (200+ MB de instalador, ~500 MB de RAM) e C++/Qt (produtividade baixa, tooling fragmentado), **Tauri v2 + Rust** entrega instalador de **~3 MB**, consumo de RAM na casa de **dezenas de MB**, I/O de disco nativo sem GC e acesso direto às APIs de baixo nível de cada SO — medido em **>1 GB/s** de throughput de clonagem em NVMe (ver Testes).

| Camada | Tecnologia |
|---|---|
| Framework desktop | Tauri v2 (WebView nativa do SO) |
| Core engine | Rust (I/O síncrono em blocos de 8 MiB, em threads dedicadas) |
| UI | React 18 + Vite + Tailwind CSS |
| Banco local | SQLite via `rusqlite` (modo WAL) |
| Compressão | Zstandard (`zstd`, nível 3 por padrão, multi-thread) |
| Checksum | BLAKE3 (on-the-fly, durante a cópia) |

### Modelo de privilégio por SO

| SO | Acesso a disco | Elevação | Particularidades |
|---|---|---|---|
| **Windows** | `\\.\PhysicalDriveX`, `\\.\X:` | Manifesto `requireAdministrator` (UAC na abertura) | Lock/dismount dos volumes do destino; VSS para snapshots; Smart Copy em NTFS/ReFS |
| **Linux** | `/dev/sdX`, `/dev/nvmeXn1` | Polkit (`pkexec`) por operação, via `cloneforge-helper` | Desmonta o destino (`umount`/`swapoff`); Smart Copy via `partclone` |
| **macOS** | `/dev/rdiskX` (raw) | Manual (`sudo`) — elevação automática no roadmap | `diskutil unmountDisk`; containers APFS; `hdiutil`/`asr` |

### Formato `.cfimg`

```
[ header 128 B ][ frame zstd (com checksum) ][ footer 32 B = BLAKE3 do conteúdo ]
```

O header guarda modo (raw / smart-bitmap / partclone), tamanho real do conteúdo, nível zstd, tipo da origem (disco ou partição), data de criação e uma descrição da origem. Imagens geradas pela v0.1.0 continuam restauráveis.

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

O bootstrap verifica o toolchain, instala as dependências de sistema (Linux) e do frontend.

### Rodar em modo dev

```bash
npm run tauri dev   # app completo (Windows: terminal como Administrador)
npm run dev         # só a interface, no navegador, com backend simulado (sem discos reais)
```

### Build local

```bash
npm run tauri build -- --bundles msi,nsis          # Windows
npm run tauri build -- --bundles deb,rpm,appimage  # Linux
npm run tauri build -- --target universal-apple-darwin --bundles app,dmg  # macOS universal
```

No Linux o build também compila o `cloneforge-helper` e o empacota em `/usr/bin`, junto com a policy do Polkit em `/usr/share/polkit-1/actions/` (ver `src-tauri/tauri.linux.conf.json`).

### Testes

```bash
cd src-tauri
cargo test --workspace                                    # testes unitários (motor + app)
cargo clippy --workspace --all-targets -- -D warnings     # lint

cargo build -p cloneforge-helper
python ../scripts/e2e_helper.py target/debug/cloneforge-helper   # E2E headless (só arquivos)
```

Os testes cobrem, entre outros: bloqueio do disco do sistema em todas as grafias de caminho, parsers de `Get-Disk`/`lsblk`/`diskutil`, clonagem e imagem bit-idênticas, detecção de imagem corrompida, compatibilidade com imagens da v0.1.0, cancelamento e o ciclo de vida do histórico.

### Motor headless (sem GUI)

O binário `cloneforge-helper` fala JSON-lines em stdin/stdout — permite testar o motor em CI ou via shell. Código de saída: `0` sucesso, `1` falha.

```bash
echo '{"job":"clone","request":{"source":"origem.bin","target":"destino.bin","verify":true}}' \
  | src-tauri/target/debug/cloneforge-helper run
```

Jobs: `clone`, `create_image`, `restore`. Para cancelar, envie `{"cancel":true}` no stdin durante a execução.

### Logs

O app grava `cloneforge.log` no diretório de dados (`%APPDATA%\com.cloneforge.app\` no Windows, `~/.local/share/com.cloneforge.app/` no Linux, `~/Library/Application Support/com.cloneforge.app/` no macOS). O caminho aparece no rodapé da janela.

### CI/CD

- **CI** (`ci.yml`): em todo push/PR — motor + helper em **Linux, macOS e Windows** (clippy `-D warnings`, testes unitários, E2E headless); app Tauri completo nos três SOs; e um job que gera o `.deb` e confere que o helper e a policy do Polkit estão no pacote.
- **Release** (`release.yml`): tag `v*` → build em matrix (Windows / Ubuntu 22.04 / macOS) e publicação direta dos 7 artefatos no GitHub Release. Repositório público = minutos de Actions ilimitados.

### Assinatura do macOS

O `.dmg` publicado é **ad-hoc** (sem certificado). Para distribuir assinado e notarizado, cadastre os secrets no repositório e reative o bloco `env` do step macOS em `release.yml`:

`APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`

---

## Testes de aceitação

Validação automatizada (detalhes em [`TESTES_ACEITACAO.md`](TESTES_ACEITACAO.md)):

- ✅ Clone file→file com verificação, destino byte-idêntico (BLAKE3) — Windows e Linux
- ✅ Imagem `.cfimg` zstd e restore **bit-idêntico**, com releitura do destino
- ✅ Testes negativos: imagem corrompida/truncada **detectada** mesmo sem "verificar"; disco do sistema recusado em qualquer grafia de caminho; cancelamento no meio da cópia

Os testes com GUI e hardware real (pendrives, boot de clone, Smart Copy, VSS) seguem o roteiro no mesmo arquivo.

---

## Licença

[MIT](LICENSE) — use, modifique e distribua livremente.
