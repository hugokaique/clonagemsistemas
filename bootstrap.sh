#!/usr/bin/env bash
# CloneForge — Bootstrap do ambiente de desenvolvimento (Linux / macOS)
set -euo pipefail
cd "$(dirname "$0")"

info() { printf "\033[36m%s\033[0m\n" "$1"; }
ok()   { printf "  \033[32m[OK]\033[0m %s\n" "$1"; }
warn() { printf "  \033[33m[AVISO]\033[0m %s\n" "$1"; }

need() {
  command -v "$1" >/dev/null 2>&1 && ok "$1" || { echo "[ERRO] '$1' não encontrado. $2"; exit 1; }
}

info "=== 1/4 Toolchain ==="
need node   "Instale o Node.js LTS (>= 18): https://nodejs.org"
need npm    "O npm acompanha o Node.js."
need cargo  "Instale o Rust via rustup: https://rustup.rs"
need rustup "Instale o Rust via rustup: https://rustup.rs"

OS="$(uname -s)"

if [ "$OS" = "Linux" ]; then
  info "=== 1b/4 Dependências de sistema (WebKitGTK) ==="
  if command -v apt-get >/dev/null 2>&1; then
    sudo apt-get update
    sudo apt-get install -y build-essential curl wget file pkg-config \
      libwebkit2gtk-4.1-dev libjavascriptcoregtk-4.1-dev libsoup-3.0-dev \
      libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev libssl-dev \
      util-linux partclone sfdisk
  elif command -v dnf >/dev/null 2>&1; then
    sudo dnf install -y gcc gcc-c++ make curl wget file pkgconf-pkg-config \
      webkit2gtk4.1-devel javascriptcoregtk4.1-devel libsoup3-devel \
      gtk3-devel libappindicator-gtk3-devel librsvg2-devel openssl-devel \
      util-linux partclone
  elif command -v pacman >/dev/null 2>&1; then
    sudo pacman -S --needed --noconfirm base-devel curl wget file pkgconf \
      webkit2gtk-4.1 gtk3 libappindicator-gtk3 librsvg openssl partclone
  else
    warn "Gerenciador de pacotes não reconhecido — instale manualmente: webkit2gtk-4.1, gtk3, libsoup-3.0, openssl, partclone"
  fi
  # Polkit: instale a policy do app em /usr/share/polkit-1/actions/ no empacotamento
fi

if [ "$OS" = "Darwin" ]; then
  info "=== 1b/4 Targets Apple Silicon + Intel (Universal Binary) ==="
  xcode-select --install 2>/dev/null || true
  rustup target add aarch64-apple-darwin x86_64-apple-darwin
  ok "targets aarch64-apple-darwin e x86_64-apple-darwin"
fi

info "=== 2/4 Dependências do frontend ==="
npm install

info "=== 3/4 Ícones ==="
if [ -f "src-tauri/icons/icon.icns" ] || [ -f "src-tauri/icons/icon.ico" ]; then
  ok "ícones já presentes em src-tauri/icons"
elif [ -f "icon-1024.png" ]; then
  npx tauri icon ./icon-1024.png
else
  warn "Nenhum icon-1024.png na raiz. Gere depois com: npx tauri icon <arquivo-1024.png>"
fi

info "=== 4/4 Pronto ==="
cat <<'EOF'

  Desenvolvimento : npm run tauri dev        (Linux: via pkexec/sudo p/ acesso raw)

  Build Linux     : npm run tauri build -- --bundles deb,rpm,appimage
  Build macOS     : npm run tauri build -- --target universal-apple-darwin --bundles app,dmg

  Saída           : src-tauri/target/release/bundle/

EOF
