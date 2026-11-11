# Política de segurança

## Versões suportadas

Apenas a versão mais recente do CloneForge recebe correções de segurança. Versões anteriores não são mantidas. Antes de relatar um problema, verifique se ele também ocorre na última versão publicada.

## Como reportar uma vulnerabilidade

Não abra issues públicas para vulnerabilidades de segurança.

Relate o problema de forma privada pelo botão **"Report a vulnerability"**, disponível na aba **Security** do repositório no GitHub. Esse canal mantém os detalhes restritos aos mantenedores até que a correção seja publicada.

Ao fazer o relato, inclua, na medida do possível:

- Sistema operacional e versão (por exemplo, Windows 11, macOS 14 ou Ubuntu 24.04);
- Versão do CloneForge utilizada;
- Passos detalhados para reproduzir o problema;
- Impacto esperado, por exemplo: escrita no disco errado, contorno da proteção do disco do sistema ou elevação de privilégios.

Não envie números de série de discos, dados pessoais nem arquivos de imagem de disco com conteúdo sensível.

## Prioridade

O CloneForge escreve diretamente em discos brutos com privilégios de administrador ou root. Por isso, tratamos com alta prioridade problemas que possam:

- fazer o aplicativo gravar dados no disco errado;
- contornar a proteção que impede a escrita no disco do sistema;
- escalar privilégios pelo helper do Linux (`cloneforge-helper`, executado via `pkexec`).

Esses relatos recebem prioridade sobre os demais e serão tratados o mais rápido possível.
