<#
.SYNOPSIS
  CloneForge — Bootstrap do ambiente de desenvolvimento (Windows).

.DESCRIPTION
  1. Verifica Node.js, Rust/rustup e MSVC Build Tools.
  2. Instala as dependências do frontend (npm install).
  3. Gera os ícones do app (se houver um icon-1024.png na raiz).
  4. Exibe os comandos de build.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File .\bootstrap.ps1
#>
#requires -Version 5.1
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

function Assert-Cmd([string]$Name, [string]$Hint) {
  if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
    throw "[ERRO] '$Name' não encontrado. $Hint"
  }
  Write-Host "  [OK] $Name" -ForegroundColor Green
}

Write-Host "`n=== 1/4 Toolchain ===" -ForegroundColor Cyan
Assert-Cmd node   "Instale o Node.js LTS (>= 18): https://nodejs.org"
Assert-Cmd npm    "O npm acompanha o Node.js."
Assert-Cmd cargo  "Instale o Rust via rustup: https://rustup.rs"
Assert-Cmd rustup "Instale o Rust via rustup: https://rustup.rs"

$vsPaths = @("${env:ProgramFiles}\Microsoft Visual Studio", "${env:ProgramFiles(x86)}\Microsoft Visual Studio")
if (-not ($vsPaths | Where-Object { Test-Path $_ })) {
  Write-Warning "Visual Studio Build Tools não detectado. O target MSVC precisa da workload 'Desktop development with C++': winget install --id Microsoft.VisualStudio.2022.BuildTools"
}

Write-Host "`n=== 2/4 Dependências do frontend ===" -ForegroundColor Cyan
npm install

Write-Host "`n=== 3/4 Ícones ===" -ForegroundColor Cyan
if (Test-Path "src-tauri\icons\icon.ico") {
  Write-Host "  [OK] ícones já presentes em src-tauri\icons" -ForegroundColor Green
} elseif (Test-Path "icon-1024.png") {
  npx tauri icon .\icon-1024.png
} else {
  Write-Warning "Nenhum icon-1024.png na raiz. Gere depois com: npx tauri icon <arquivo-1024.png>"
}

Write-Host "`n=== 4/4 Pronto ===" -ForegroundColor Cyan
Write-Host @"

  Desenvolvimento : npm run tauri dev        (execute como Administrador p/ acesso raw)
  Build Windows   : npm run tauri build -- --bundles msi,nsis
  Saída           : src-tauri\target\release\bundle\

"@ -ForegroundColor Yellow
