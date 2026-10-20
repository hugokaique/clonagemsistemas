# CloneForge — Roteiro de Testes de Aceitação

Legenda: ☐ pendente · ☑ aprovado · ✖ reprovado
Cada teste registra: resultado esperado, critério de aprovação e evidência (print/log/hash).

---

## 0. Validação automatizada headless (JÁ EXECUTADA — 2026-10-19)

Motor testado sem GUI via `cloneforge-helper run` (protocolo JSON-lines) com massa
sintética de 52.181.648 bytes (32 MiB aleatórios + 16 MiB zeros + ~1,9 MiB repetitivos):

| # | Teste | Resultado | Evidência |
|---|-------|-----------|-----------|
| 0.1 | Clone file→file com `verify: true` | ☑ `ok:true`, 1033 MB/s | hash BLAKE3 `f464ad21…6c7dd` |
| 0.2 | Destino byte-idêntico à origem | ☑ comparação binária exata | `destino_clone.bin == fonte_64mb.bin` |
| 0.3 | Imagem `.cfimg` zstd nível 3 | ☑ 52,2 MB → 33,6 MB (64,3% — os 32 MiB aleatórios são incompressíveis; zeros/texto ≈ 0) | `fonte.cfimg`, mesmo hash da origem |
| 0.4 | Restore de imagem com verificação | ☑ `ok:true`, restaurado bit-idêntico | `restaurado.bin == fonte_64mb.bin` |
| 0.5 | **Teste negativo**: 1 byte invertido no frame zstd | ☑ detectado — `VERIFICAÇÃO FALHOU — hash do restore ≠ hash gravado na imagem` | `corrompida.cfimg` |

Massa de teste preservada em `testdata/`.

Reproduzir:
```powershell
echo '{"job":"clone","request":{"source":"testdata/fonte_64mb.bin","target":"testdata/out.bin","sourceSize":52181648,"blockSize":null,"verify":true}}' | src-tauri\target\debug\cloneforge-helper.exe run
```

---

## 1. Build e instalação

| # | Teste | Esperado | Status |
|---|-------|----------|--------|
| 1.1 | `npm run tauri build -- --bundles msi,nsis` no Windows | Gera `CloneForge_0.1.0_x64-setup.exe` + MSIs pt-BR/en-US | ☐ |
| 1.2 | Instalar NSIS como usuário padrão | Instalador pede UAC; app instalado em `C:\Program Files\CloneForge` | ☐ |
| 1.3 | Abrir o app instalado | **UAC exigido na abertura** (manifesto `requireAdministrator`) | ☐ |
| 1.4 | `./bootstrap.sh && npm run tauri build -- --bundles deb,rpm,appimage` no Linux | 3 pacotes gerados | ☐ |
| 1.5 | `npm run tauri build -- --target universal-apple-darwin --bundles app,dmg` no macOS | `.dmg` universal (`lipo -archs` mostra x86_64 + arm64) | ☐ |

## 2. Enumeração de discos e proteção

| # | Teste | Esperado | Status |
|---|-------|----------|--------|
| 2.1 | Abrir a aba ① | Todos os discos físicos listados com modelo, tamanho, barramento e partições | ☐ |
| 2.2 | Disco do sistema (C:) | Badge vermelho "DISCO DO SISTEMA" e **desabilitado como destino** | ☐ |
| 2.3 | Pendrive USB | Badge "removível", elegível como origem e destino | ☐ |
| 2.4 | "↻ Reescanear" com pendrive recém-inserido | Aparece sem reiniciar o app | ☐ |
| 2.5 | Forçar destino = disco do sistema via IPC direto | Backend recusa: "BLOQUEADO: o destino é o disco do sistema" | ☐ |

## 3. Clonagem raw (disco → disco)

Pré-requisito: 2 pendrives/SSDs USB (origem ≤ destino), nenhum deles do sistema.

| # | Teste | Esperado | Status |
|---|-------|----------|--------|
| 3.1 | Clonar pendrive 8 GB → SSD 128 GB com verificação | Progresso em tempo real (%, MB/s, ETA); fases copying→flushing→verifying→done | ☐ |
| 3.2 | Boot do clone | Pendrive bootável clonado → SSD inicializa o SO | ☐ |
| 3.3 | Destino menor que origem | Recusa com mensagem de tamanho ANTES de gravar | ☐ |
| 3.4 | Remover destino no meio da clonagem | Erro limpo na UI + registro `error` no histórico; origem intacta | ☐ |
| 3.5 | Histórico pós-clone | Linha no SQLite com hash BLAKE3 e `✔ ok (verificado)` | ☐ |

## 4. Smart Copy + imagem .cfimg (Windows)

| # | Teste | Esperado | Status |
|---|-------|----------|--------|
| 4.1 | Aba ②, Smart Copy ON, partição NTFS 100 GB com 30 GB usados | Imagem gerada lendo ~30 GB (não 100); log "smart copy: X/Y clusters alocados" | ☐ |
| 4.2 | Ratio de compressão | .cfimg ≪ dados lógicos se volume tem espaço livre/zeros | ☐ |
| 4.3 | **Live Copy (VSS)**: imagem da partição C: em uso | Snapshot criado (log "VSS snapshot {…}"), imagem conclui, shadow apagado ao final (`vssadmin list shadows` vazio) | ☐ |
| 4.4 | Restaurar .cfimg em SSD ≥ origem | done + "verificado"; partição montável/bootável | ☐ |
| 4.5 | Header da imagem na aba ③ | "header OK · modo smart-bitmap · zstd nível N · BLAKE3 …" | ☐ |
| 4.6 | Imagem corrompida (flip de 1 byte) | Restore ABORTA com "VERIFICAÇÃO FALHOU… NÃO inicialize por este disco" | ☐ (automatizado: ☑) |

## 5. Linux (VM ou hardware)

| # | Teste | Esperado | Status |
|---|-------|----------|--------|
| 5.1 | App como usuário normal, iniciar clonagem | Diálogo Polkit (pkexec) pede senha admin UMA vez (`auth_admin_keep`) | ☐ |
| 5.2 | Progresso via helper | Mesma barra/telemetria do modo local (GUI não-elevada reemite eventos) | ☐ |
| 5.3 | Policy instalada | `/usr/share/polkit-1/actions/com.cloneforge.helper.policy` presente pós-instalação do .deb | ☐ |
| 5.4 | Smart Copy ext4 via partclone | Modo 2 no header; imagem sparse-aware | ☐ |
| 5.5 | Negar a senha no pkexec | Erro limpo na UI; nada gravado no destino | ☐ |

## 6. macOS (Intel e Apple Silicon)

| # | Teste | Esperado | Status |
|---|-------|----------|--------|
| 6.1 | Universal Binary | `lipo -archs CloneForge.app/…/cloneforge` → `x86_64 arm64` | ☐ |
| 6.2 | Smart Copy → hdiutil | `.dmg` UDZO criado e catalogado (formato "dmg") | ☐ |
| 6.3 | Restore via `asr` em disco externo | APFS recriado; volume inicializável (Intel); em Apple Silicon, dados íntegros | ☐ |
| 6.4 | Sem Full Disk Access | Erro claro orientando a conceder permissão em Ajustes do Sistema | ☐ |

## 7. Auditoria e banco local

| # | Teste | Esperado | Status |
|---|-------|----------|--------|
| 7.1 | `cloneforge.db` em `%APPDATA%\com.cloneforge.app\` | Tabelas `clone_jobs`, `images`, `audit_log` (WAL) | ☐ |
| 7.2 | Kill do app no meio da clonagem | Banco íntegro ao reabrir (WAL recovery); job ausente ou marcado erro | ☐ |
| 7.3 | Catálogo de imagens | Re-criar imagem no mesmo path faz UPSERT (não duplica) | ☐ |

---

### Notas de ambiente
- Testes 3.x/4.x exigem execução como Administrador (UAC já embutido no exe).
- O helper `cloneforge-helper` **não** carrega manifesto UAC (crate isolado em `src-tauri/helper/`) — no Linux ele só deve rodar via `pkexec`.
- macOS: assinatura + notarização obrigatórias para distribuição fora de dev (`APPLE_SIGNING_IDENTITY`, `APPLE_TEAM_ID`, etc.).
