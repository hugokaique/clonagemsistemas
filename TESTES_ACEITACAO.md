# CloneForge — Roteiro de Testes de Aceitação

Legenda: ☐ pendente · ☑ aprovado · ✖ reprovado · ↻ aprovado antes, reexecutar no próximo instalador
Cada teste registra: resultado esperado, critério de aprovação e evidência (print/log/hash).

> **Revisão de 2026-11-11.** O motor foi reestruturado (crate `cloneforge-core`, validação fail-closed,
> I/O em blocos alinhados, lock/dismount do destino, cancelamento, novo formato de header). Os itens
> marcados ↻ foram aprovados com o instalador v2 (`0b9e375`) e precisam ser repetidos com o próximo
> instalador, porque tocam em código que mudou (enumeração, manifesto de permissões, empacotamento).
> Nenhum teste com hardware real (seções 3 a 6) foi executado nesta revisão.

---

## 0. Validação automatizada

### 0a. Suíte atual (executada em 2026-11-11)

| # | Teste | Onde rodou | Resultado |
|---|-------|-----------|-----------|
| 0.1 | `cargo test --workspace` — 59 testes (55 do motor + 4 do app) | Windows 11 | ☑ todos passam |
| 0.2 | `cargo clippy --workspace --all-targets -- -D warnings` | Windows 11 | ☑ zero warnings |
| 0.3 | Testes unitários do motor (53; os 2 restantes são específicos do Windows) com binário Linux | Ubuntu 26.04 (WSL2), usuário comum | ☑ todos passam |
| 0.4 | `scripts/e2e_helper.py` — clone, imagem, restore, corrupção, validações, cancelamento | Windows 11 (35 verificações) e Ubuntu/WSL2 (24) | ☑ |
| 0.5 | Clippy do motor + helper para `x86_64-unknown-linux-gnu` e `aarch64-apple-darwin` (compilação cruzada) | Windows 11 | ☑ zero warnings — **macOS só compilado, não executado** |
| 0.6 | Destino = disco do sistema real em 6 grafias (`\\.\PhysicalDrive1`, minúsculas, `//./`, `\\?\`, zero à esquerda) | Windows 11 (enumeração real) | ☑ sempre `BLOQUEADO` |
| 0.7 | Destino = raiz real (`/dev/sdd`) direto e via symlink; `/dev/null`; dispositivo inexistente | Ubuntu/WSL2 (lsblk real) | ☑ recusados, nada criado em `/dev` |
| 0.8 | Imagem `.cfimg` gerada pela v0.1.0 restaurada pela versão atual | Windows 11 | ☑ bit-idêntica |
| 0.9 | Imagem corrompida (1 byte) restaurada com `verify: false` | Windows 11 / Ubuntu | ☑ detectada (antes: reportava sucesso) |
| 0.10 | Cancelamento via protocolo no meio de uma cópia de ~1 GiB | Windows 11 / Ubuntu | ☑ para em < 1 s |
| 0.11 | Helper Linux não linka GTK/WebKit (`ldd`) | Ubuntu/WSL2 | ☑ só libc/libm/pthread/dl |
| 0.12 | Interface (3 abas, progresso, cancelar, histórico) com backend simulado (`npm run dev`) | navegador | ☑ |
| 0.13 | App real (build de release, sem elevação): janela abre com CSP restrita sem erros de console, lista os discos reais, e `start_clone` pelo IPC com o disco do sistema como destino devolve `BLOQUEADO` | Windows 11 | ☑ |
| 0.14 | Banco real da v0.1.0 migrado pelo app novo (`user_version` 1, `integrity_check` ok) | Windows 11 | ☑ |
| 0.15 | Mapeamento volume → disco usado no lock/dismount conferido contra `IOCTL_STORAGE_GET_DEVICE_NUMBER` (somente consulta) | Windows 11 | ☑ |

Reproduzir:
```bash
cd src-tauri
cargo test --workspace
cargo build -p cloneforge-helper
python ../scripts/e2e_helper.py target/debug/cloneforge-helper
```

O CI (`ci.yml`) repete 0.1, 0.2 e 0.4 em Linux, macOS e Windows a cada push.

### 0b. Histórico — validação da v0.1.0 (2026-10-19)

Motor testado via `cloneforge-helper run` com massa sintética de 52.181.648 bytes:

| # | Teste | Resultado | Evidência |
|---|-------|-----------|-----------|
| 0b.1 | Clone file→file com `verify: true` | ☑ `ok:true`, 1033 MB/s | hash BLAKE3 `f464ad21…6c7dd` |
| 0b.2 | Destino byte-idêntico à origem | ☑ comparação binária exata | `destino_clone.bin == fonte_64mb.bin` |
| 0b.3 | Imagem `.cfimg` zstd nível 3 | ☑ 52,2 MB → 33,6 MB (64,3%) | `fonte.cfimg`, mesmo hash da origem |
| 0b.4 | Restore de imagem com verificação | ☑ `ok:true`, restaurado bit-idêntico | `restaurado.bin == fonte_64mb.bin` |
| 0b.5 | **Teste negativo**: 1 byte invertido no frame zstd | ☑ detectado | `corrompida.cfimg` |

---

## 1. Build e instalação

| # | Teste | Esperado | Status |
|---|-------|----------|--------|
| 1.1 | `npm run tauri build -- --bundles msi,nsis` no Windows | Gera `CloneForge_0.1.0_x64-setup.exe` + MSIs pt-BR/en-US | ↻ aprovado no CI run `39499079111`; nesta revisão só o build de release sem empacotar (`--no-bundle`) foi refeito |
| 1.2 | Instalar NSIS como usuário padrão | Instalador pede UAC; app instalado em `C:\Program Files\CloneForge` | ↻ 2026-10-21 |
| 1.3 | Abrir o app instalado | **UAC exigido na abertura** (manifesto `requireAdministrator`); janela abre, lista os discos e o rodapé mostra o caminho do log | ↻ 2026-10-26 (bug do `TaskDialogIndirect` corrigido em `5a2947b`). Reexecutar: a janela agora usa o plugin de diálogos e CSP restrita |
| 1.4 | `./bootstrap.sh && npm run tauri build -- --bundles deb,rpm,appimage` no Linux | 3 pacotes gerados; o bootstrap não falha no `apt-get` | ☐ o bootstrap antigo abortava em Ubuntu/Debian (pacote `sfdisk` inexistente) — corrigido; pacotes ↻ |
| 1.5 | `npm run tauri build -- --target universal-apple-darwin --bundles app,dmg` no macOS | `.dmg` universal (`lipo -archs` mostra x86_64 + arm64) | ☐ falta verificar `lipo -archs` num Mac |
| 1.6 | Conteúdo do `.deb` | `dpkg -c` lista `/usr/bin/cloneforge-helper` e `/usr/share/polkit-1/actions/com.cloneforge.helper.policy`; `Depends` inclui `pkexec \| policykit-1` | ☐ conferido automaticamente pelo job `linux-package` do CI |

## 2. Enumeração de discos e proteção

| # | Teste | Esperado | Status |
|---|-------|----------|--------|
| 2.1 | Abrir a aba ① | Todos os discos físicos listados com modelo, tamanho, barramento, partições e volumes em uso | ↻ 2026-10-26 (bug do array duplo corrigido em `0b9e375`). O script de enumeração ganhou filesystem/rótulo/offset — conferido nesta máquina sem elevação (2× NVMe) |
| 2.2 | Disco do sistema (C:) | Badge vermelho "DISCO DO SISTEMA" e **desabilitado como destino** | ↻ 2026-10-26 |
| 2.3 | Pendrive USB | Badge "removível", elegível como origem e destino | ↻ 2026-10-26 |
| 2.4 | "↻ Reescanear" com pendrive recém-inserido | Aparece sem reiniciar o app; seleção de disco removido é limpa | ☐ |
| 2.5 | Forçar destino = disco do sistema por fora da UI | Backend recusa: "BLOQUEADO: o destino é o disco do sistema em execução" | ☑ automatizado (0.6/0.7 + testes unitários `safety::*`): bloqueado em qualquer grafia, inclusive partições e symlinks |
| 2.6 | Disco somente-leitura (chave de proteção de cartão SD) | Badge "somente leitura", desabilitado como destino | ☐ |
| 2.7 | Destino menor que a origem | Card do destino desabilitado com "menor que a origem (X GB)" | ☐ (validação do backend: ☑ teste unitário) |

## 3. Clonagem raw (disco → disco)

Pré-requisito: 2 pendrives/SSDs USB (origem ≤ destino), nenhum deles do sistema.

| # | Teste | Esperado | Status |
|---|-------|----------|--------|
| 3.1 | Clonar pendrive 8 GB → SSD 128 GB com verificação | Progresso em tempo real (%, MB/s, ETA); fases preparing→copying→flushing→verifying→done; log mostra "destino: volume … travado e desmontado" | ☐ **prioritário** — primeiro teste real do lock/dismount no Windows |
| 3.2 | Boot do clone | Pendrive bootável clonado → SSD inicializa o SO | ☐ |
| 3.3 | Destino menor que origem | Recusa com mensagem de tamanho ANTES de gravar | ☐ (backend: ☑ teste unitário `pares_invalidos`) |
| 3.4 | Remover destino no meio da clonagem | Erro limpo na UI + registro `erro` no histórico; origem intacta | ☐ |
| 3.5 | Histórico pós-clone | Linha "Clonagem" com duração, hash BLAKE3 e `✔ ok (verificado)` | ☐ |
| 3.6 | Destino com volume aberto no Explorer | Clonagem conclui (volume desmontado à força); log avisa se não conseguiu travar | ☐ |
| 3.7 | Origem com tamanho não múltiplo de 4096 → destino 4Kn | Conclui; cauda completada com zeros até o setor | ☐ |

## 4. Smart Copy + imagem .cfimg (Windows)

| # | Teste | Esperado | Status |
|---|-------|----------|--------|
| 4.1 | Aba ②, Smart Copy ON, partição NTFS 100 GB com 30 GB usados | Imagem gerada lendo ~30 GB (não 100); log da operação mostra "smart copy: X/Y clusters alocados" | ☐ |
| 4.2 | Ratio de compressão | .cfimg ≪ dados lógicos se volume tem espaço livre/zeros | ☐ |
| 4.3 | **Live Copy (VSS)**: imagem da partição C: em uso | Log mostra "VSS snapshot {…} criado"; imagem conclui; shadow apagado ao final (`vssadmin list shadows` vazio) | ☐ |
| 4.4 | Restaurar imagem Smart Copy em pendrive ≥ origem | done + "verificado por releitura do destino" (antes: **sempre** falhava com "stream restaurado com X bytes, mas o header declara Y") | ☐ (lógica: ☑ testes `smart_copy_grava_o_tamanho_real_no_header` e `imagens_da_v010_continuam_restauraveis`) |
| 4.5 | Header da imagem na aba ③ | "header OK · partição/volume · <origem>" + modo, tamanhos, nível zstd, data e BLAKE3 | ☐ (UI: ☑ 0.12) |
| 4.6 | Imagem corrompida (flip de 1 byte) | Restore ABORTA com "imagem corrompida… NÃO inicialize por este disco", com ou sem "verificar" | ☑ automatizado (0.9) |
| 4.7 | Smart Copy em partição FAT32/exFAT | Partição aparece desabilitada com "exige NTFS/ReFS"; o backend também recusa | ☐ |
| 4.8 | Imagem raw de disco inteiro → restaurar em outro disco → boot | Disco restaurado inicializa | ☐ |
| 4.9 | Cancelar a criação da imagem | Arquivo `.part` removido; imagem anterior no mesmo caminho intacta | ☑ automatizado (teste `falha_ou_cancelamento_…`) |
| 4.10 | "Abrir arquivo…" na aba ③ com imagem fora do catálogo (outra máquina) | Header exibido; restauração funciona | ☐ |

## 5. Linux (VM ou hardware)

| # | Teste | Esperado | Status |
|---|-------|----------|--------|
| 5.1 | App como usuário normal, iniciar clonagem | Diálogo Polkit (pkexec) pede senha admin UMA vez (`auth_admin_keep`) | ☐ |
| 5.2 | Progresso via helper | Mesma barra/telemetria do modo local (GUI não-elevada reemite eventos) | ☐ |
| 5.3 | Policy instalada | `/usr/share/polkit-1/actions/com.cloneforge.helper.policy` e `/usr/bin/cloneforge-helper` presentes pós-instalação do .deb (antes **não eram empacotados**) | ☐ (ver 1.6) |
| 5.4 | Smart Copy ext4 via partclone → restaurar | Modo "smart copy (partclone)" no header; restore passa pelo `partclone.restore` e a partição monta (antes o stream era gravado cru = lixo) | ☐ **prioritário** — caminho só compilado, nunca executado |
| 5.5 | Negar a senha no pkexec | Erro "autorização negada ou cancelada — nada foi gravado"; destino intacto | ☐ |
| 5.6 | Raiz em LVM/LUKS | Disco físico da raiz marcado "DISCO DO SISTEMA" (antes não era) | ☐ (parser: ☑ teste `linux_parse_e_disco_do_sistema`) |
| 5.7 | Destino com partição montada | Partições desmontadas antes da gravação (log "destino: umount …"); se ocupada, erro claro | ☐ |
| 5.8 | Cancelar durante a clonagem | Helper (root) para; histórico registra "cancelado" | ☐ (protocolo: ☑ 0.10) |
| 5.9 | Imagem criada via helper | Arquivo pertence ao usuário, não ao root | ☐ |

## 6. macOS (Intel e Apple Silicon)

> O app ainda não eleva privilégio sozinho no macOS: execute
> `sudo /Applications/CloneForge.app/Contents/MacOS/cloneforge` para os testes de disco.
> Todo o código macOS desta revisão foi apenas compilado (0.5), nunca executado.

| # | Teste | Esperado | Status |
|---|-------|----------|--------|
| 6.1 | Universal Binary | `lipo -archs CloneForge.app/…/cloneforge` → `x86_64 arm64` | ☐ |
| 6.2 | Smart Copy → hdiutil | `.dmg` UDZO criado e catalogado (formato "dmg") | ☐ |
| 6.3 | Restore de `.dmg` via `asr` em disco externo | APFS recriado; volume inicializável (Intel); em Apple Silicon, dados íntegros (antes o `.dmg` era recusado como "não é .cfimg") | ☐ |
| 6.4 | Sem root / sem Full Disk Access | Erro claro orientando a executar com `sudo` e conceder Acesso Total ao Disco | ☐ |
| 6.5 | Lista de discos | Disco interno (APFS) marcado "DISCO DO SISTEMA"; container sintetizado não aparece; modelo/barramento reais | ☐ (parser: ☑ teste `macos_apfs_marca_o_disco_fisico_como_sistema`, com plist sintético) |

## 7. Auditoria e banco local

| # | Teste | Esperado | Status |
|---|-------|----------|--------|
| 7.1 | `cloneforge.db` em `%APPDATA%\com.cloneforge.app\` | Tabelas `clone_jobs`, `images`, `audit_log` (WAL); banco da v0.1.0 é migrado sem perder linhas | ☑ automatizado (teste `migra_banco_da_v010_…`); conferir no app instalado: ☐ |
| 7.2 | Kill do app no meio da clonagem | Banco íntegro ao reabrir; job aparece como "⚠ interrompido" | ☑ lógica automatizada; no app instalado: ☐ |
| 7.3 | Catálogo de imagens | Re-criar imagem no mesmo path faz UPSERT (não duplica) | ☑ automatizado |
| 7.4 | Banco corrompido | App abre mesmo assim; arquivo antigo preservado como `cloneforge.corrompido-*.db` | ☑ automatizado (teste `banco_corrompido_…`) |
| 7.5 | Log | `cloneforge.log` na pasta de dados registra início, erros e resumo do smart copy | ☐ |

## 8. Interface e robustez

| # | Teste | Esperado | Status |
|---|-------|----------|--------|
| 8.1 | Botão **Cancelar** durante clonagem/imagem/restore | Operação para em segundos; painel mostra "Cancelado"; histórico "⏹ cancelado" | ☐ (motor: ☑ 0.10; UI simulada: ☑ 0.12) |
| 8.2 | Fechar a janela com operação em andamento | Janela não fecha; aviso pede para cancelar antes | ☐ |
| 8.3 | Diálogos de confirmação | Diálogo nativo com origem, destino, tamanho e aviso de apagamento | ☐ |
| 8.4 | "Procurar…" na aba ② | Diálogo "Salvar como"; caminho relativo digitado vai para Documentos; arquivo existente pede confirmação | ☐ |
| 8.5 | Imagem do catálogo apagada do disco | Entrada aparece como "arquivo não encontrado" com botão "Remover do catálogo" | ☐ (UI simulada: ☑) |

---

### Notas de ambiente
- Testes 3.x/4.x exigem execução como Administrador (UAC já embutido no exe).
- O helper `cloneforge-helper` **não** carrega manifesto UAC (crate isolado em `src-tauri/helper/`) — no Linux ele só deve rodar via `pkexec`. Ele refaz todas as validações de segurança antes de tocar em qualquer disco.
- macOS: assinatura + notarização obrigatórias para distribuição fora de dev (`APPLE_SIGNING_IDENTITY`, `APPLE_TEAM_ID`, etc.).
- `npm run dev` num navegador comum sobe a interface com backend simulado — útil para os itens de UI sem risco para discos reais.
