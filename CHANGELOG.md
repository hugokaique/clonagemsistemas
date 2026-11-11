# Changelog

Todas as mudanças relevantes do CloneForge são registradas aqui.
O formato segue o [Keep a Changelog](https://keepachangelog.com/pt-BR/1.1.0/).

## [Não lançado]

### Segurança
- A trava do disco do sistema passou a ser fail-closed: sem enumeração confiável nada é gravado, e o destino tem de ser um disco inteiro da enumeração (apelidos do mesmo disco são reconhecidos).
- macOS: o SSD físico por trás de um container APFS é marcado como disco do sistema. Linux: raiz em LVM/LUKS/RAID/ZFS e mídias de sistemas live são detectadas.
- O helper elevado do Linux valida todo job: só aceita discos e partições da própria enumeração, nunca grava no disco do sistema e abre os arquivos de imagem com a identidade de quem chamou o `pkexec`.
- Injeção de PowerShell no VSS corrigida: o volume é validado na forma exata `X:\` antes de entrar no script.
- Content Security Policy ativada; plugin `shell` e suas permissões removidos.
- A origem de uma imagem tem de ser um disco, partição ou volume enumerado; o filesystem do partclone vem de uma lista fechada.
- Imagens novas são criadas com permissão 0600; `pkexec` e o helper são chamados por caminho absoluto.

### Corrigido
- O restore segue o modo gravado no header: imagens partclone vão para o `partclone.restore` em vez de serem gravadas cruas no disco; versão, modo ou algoritmo desconhecidos recusam a imagem antes de tocar no destino.
- O hash da imagem é conferido em todo restore, e "verificar" passou a reler o destino do disco.
- Criar imagem não destrói mais o backup anterior: a escrita vai para um arquivo `.partial`, promovido só quando completo; erro de `fsync` deixou de ser ignorado.
- Smart Copy no Linux não trava mais (o stderr do partclone é drenado).
- "verificação OK" não aparece mais quando a verificação está desligada; o histórico só marca "verificado" quando houve verificação.
- O erro da última escrita da clonagem não é mais perdido; a verificação no Linux lê do disco, não do page cache; velocidade e ETA da verificação usam cronômetro próprio.
- O snapshot VSS é apagado só depois de o leitor fechar; resposta do helper sem hash deixou de contar como sucesso.
- macOS: o nome do `.dmg` é fixado antes do `hdiutil`, para o hash e o catálogo apontarem para o arquivo real.

### Adicionado
- Cancelamento de operações (botão na interface; no Linux o pedido segue para o helper).
- Uma operação por vez; o fechamento da janela é barrado durante uma operação.
- Histórico registra o job como `running` ao começar e marca como interrompido o que ficou pela metade.
- Desmontagem dos volumes do destino antes da escrita no macOS e no Linux.
- Modal de confirmação com modelo, caminho, tamanho e partições; seleção de discos independente por aba; atualização automática da lista de discos.
- Compressão zstd em várias threads e checksum no frame.
- Testes unitários em Rust; CI nos três sistemas com `cargo test`; release em rascunho com `SHA256SUMS.txt`; pacotes `.deb`/`.rpm` com o helper e a policy do Polkit.

### Alterado
- O helper termina com código de saída 1 quando o job falha ou é cancelado.
- Imagens criadas pelo helper passam a pertencer ao usuário, não a root.
- Tamanhos são exibidos em unidades binárias rotuladas (MiB/GiB/TiB).
