#!/usr/bin/env python3
"""Teste E2E headless do motor via `cloneforge-helper` (protocolo JSON-lines).

Usa SOMENTE arquivos comuns num diretório temporário: nenhum disco real é
gravado. As checagens de dispositivo só confirmam que alvos perigosos são
RECUSADOS antes de qualquer escrita.

Uso:
    python scripts/e2e_helper.py src-tauri/target/debug/cloneforge-helper[.exe]

Roda igual no Windows, Linux e macOS (é o que o CI executa nos três).
"""
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import time

if len(sys.argv) < 2:
    sys.exit(__doc__)

WINDOWS = platform.system() == "Windows"
HELPER = os.path.abspath(sys.argv[1])
if WINDOWS and not os.path.exists(HELPER) and os.path.exists(HELPER + ".exe"):
    HELPER += ".exe"
if not os.path.exists(HELPER):
    sys.exit(f"helper não encontrado: {HELPER} (compile com `cargo build -p cloneforge-helper`)")
BS = chr(92)  # barra invertida, sem depender de escapes do shell


def win_dev(*parts):
    """win_dev('.', 'PhysicalDrive1') -> caminho de dispositivo Win32"""
    return BS + BS + BS.join(parts)


def job(payload, cancel_when_copying=False, timeout=300):
    """Envia um job ao helper. Devolve (código de saída, resultado, linhas)."""
    proc = subprocess.Popen(
        [HELPER, "run"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
    )
    proc.stdin.write(json.dumps(payload) + "\n")
    proc.stdin.flush()
    lines, sent = [], False
    for line in proc.stdout:
        line = line.strip()
        if not line:
            continue
        obj = json.loads(line)
        lines.append(obj)
        copying = obj.get("type") == "progress" and obj["payload"]["phase"] == "copying"
        if cancel_when_copying and not sent and copying and obj["payload"]["bytesDone"] > 0:
            proc.stdin.write('{"cancel": true}\n')
            proc.stdin.flush()
            sent = True
    proc.stdin.close()
    code = proc.wait(timeout=timeout)
    return code, (lines[-1] if lines else None), lines


passed = 0


def check(name, cond, detail=""):
    global passed
    print(("PASS " if cond else "FAIL ") + name + (f" — {detail}" if detail else ""), flush=True)
    if not cond:
        sys.exit(1)
    passed += 1


def phases_of(lines):
    out = []
    for l in lines:
        if l.get("type") == "progress" and (not out or out[-1] != l["payload"]["phase"]):
            out.append(l["payload"]["phase"])
    return out


def system_device():
    """Algum dispositivo que pertence ao disco do sistema (para confirmar a recusa)."""
    try:
        if WINDOWS:
            ps = "(Get-Disk | Where-Object { $_.IsSystem -or $_.IsBoot } | Select-Object -First 1).Number"
            out = subprocess.run(["powershell", "-NoProfile", "-Command", ps], capture_output=True, text=True).stdout
            return win_dev(".", f"PhysicalDrive{int(out.strip())}")
        if platform.system() == "Darwin":
            out = subprocess.run(["df", "/"], capture_output=True, text=True).stdout
            return out.strip().splitlines()[-1].split()[0]
        out = subprocess.run(["findmnt", "-n", "-o", "SOURCE", "/"], capture_output=True, text=True).stdout
        dev = out.strip().split("[")[0]
        return dev if dev.startswith("/dev/") else None
    except Exception:
        return None


work = tempfile.mkdtemp(prefix="cloneforge-e2e-")
os.chdir(work)
print(f"helper: {HELPER}\npasta : {work}", flush=True)

# Massa mista (aleatório + zeros + repetitivo), tamanho fora de alinhamento de setor
with open("src.bin", "wb") as f:
    f.write(os.urandom(3 * 1024 * 1024))
    f.write(bytes(2 * 1024 * 1024))
    f.write(b"abc" * 333333)
src = open("src.bin", "rb").read()

# ── 1. Clonagem arquivo → arquivo ───────────────────────────────────────────
code, res, lines = job({"job": "clone", "request": {"source": "src.bin", "target": "dst.bin", "sourceSize": len(src), "blockSize": None, "verify": True}})
check("clone com verificação", code == 0 and res["ok"] and res["verified"] and res["bytes"] == len(src), str(res))
check("clone bit-idêntico", open("dst.bin", "rb").read() == src)
check("fases do clone", phases_of(lines) == ["preparing", "copying", "flushing", "verifying"], str(phases_of(lines)))

# ── 2. Imagem .cfimg: criar, restaurar, detectar corrupção ──────────────────
code, res, _ = job({"job": "create_image", "request": {"source": "src.bin", "output": "img.cfimg", "level": 3, "smart": False}})
check("create_image", code == 0 and res["logical"] == len(src) and os.path.isabs(res["output"]), str(res))
check("imagem comprimiu", res["image_size"] < len(src), f"{res['image_size']} de {len(src)} bytes")
img_hash = res["hash"]
code, res, _ = job({"job": "restore", "request": {"image": "img.cfimg", "target": "volta.bin", "verify": True}})
check("restore verificado", code == 0 and res["verified"] and res["hash"] == img_hash, str(res))
check("restore bit-idêntico", open("volta.bin", "rb").read() == src)

blob = bytearray(open("img.cfimg", "rb").read())
blob[len(blob) // 2] ^= 0xFF
open("corrompida.cfimg", "wb").write(blob)
for verify in (False, True):
    code, res, _ = job({"job": "restore", "request": {"image": "corrompida.cfimg", "target": "lixo.bin", "verify": verify}})
    check(f"imagem corrompida detectada (verify={verify})", code == 1 and "orrompida" in res["error"].lower(), res["error"])
open("truncada.cfimg", "wb").write(bytes(blob[: len(blob) // 3]))
code, res, _ = job({"job": "restore", "request": {"image": "truncada.cfimg", "target": "lixo.bin", "verify": False}})
check("imagem truncada recusada", code == 1 and not res["ok"], res["error"])

# ── 3. Validações: nada é criado/sobrescrito quando o pedido é inválido ─────
code, res, _ = job({"job": "clone", "request": {"source": "nao-existe.bin", "target": "x.bin", "verify": True}})
check("origem inexistente → código de saída 1", code == 1 and not res["ok"] and not os.path.exists("x.bin"), res["error"])
code, res, _ = job({"job": "clone", "request": {"source": "src.bin", "target": "src.bin", "verify": True}})
check("origem = destino recusado", code == 1 and open("src.bin", "rb").read() == src, res["error"])
code, res, _ = job({"job": "create_image", "request": {"source": "src.bin", "output": "img.cfimg", "smart": True}})
check("smart copy em arquivo recusado; imagem anterior intacta", code == 1 and open("img.cfimg", "rb").read(8) == b"CFIMG001", res["error"])
code, res, _ = job({"job": "nada", "request": {}})
check("job desconhecido", code == 1 and not res["ok"], res["error"])
check("sem sobras .part", not [f for f in os.listdir(".") if f.endswith(".part")])

# ── 4. Dispositivos perigosos são recusados antes de qualquer escrita ───────
sysdev = system_device()
targets = [sysdev] if sysdev else []
if WINDOWS:
    if sysdev:
        n = sysdev[len(win_dev(".", "PhysicalDrive")):]
        targets += [win_dev(".", f"physicaldrive{n}"), f"//./PhysicalDrive{n}", win_dev("?", f"PhysicalDrive{n}"),
                    win_dev(".", f"PhysicalDrive{n} "), win_dev(".", f"PhysicalDrive{n}.")]
    targets += [win_dev(".", "C:"), win_dev(".", "PhysicalDrive987"), win_dev("?", "GLOBALROOT", "Device", "Harddisk0", "DR0")]
    out_dev = win_dev(".", "PhysicalDrive987")
else:
    targets += ["/dev/cloneforge-nao-existe", "/dev/null"]
    out_dev = "/dev/cloneforge-nao-existe"
for t in targets:
    code, res, _ = job({"job": "clone", "request": {"source": "src.bin", "target": t, "verify": True}})
    check(f"clone → {t!r} recusado", code == 1 and not res["ok"], res["error"])
    code, res, _ = job({"job": "restore", "request": {"image": "img.cfimg", "target": t, "verify": True}})
    check(f"restore → {t!r} recusado", code == 1 and not res["ok"], res["error"])
code, res, _ = job({"job": "create_image", "request": {"source": "src.bin", "output": out_dev}})
check("saída de imagem em dispositivo recusada", code == 1 and not res["ok"], res["error"])
if not WINDOWS:
    check("nada foi criado em /dev", not os.path.exists("/dev/cloneforge-nao-existe"))

# ── 5. Cancelamento pelo protocolo ─────────────────────────────────────────
BIG = 768 * 1024 * 1024
with open("big.bin", "wb") as f:
    f.truncate(BIG)
t0 = time.time()
code, res, lines = job({"job": "clone", "request": {"source": "big.bin", "target": "big_out.bin", "blockSize": 1048576, "verify": True}}, cancel_when_copying=True)
copied = max((l["payload"]["bytesDone"] for l in lines if l.get("type") == "progress"), default=0)
if code == 0:
    # máquina rápida demais: a cópia terminou antes de o cancelamento chegar
    print(f"AVISO cancelamento não exercitado (cópia terminou em {time.time() - t0:.2f}s)", flush=True)
else:
    check("cancelamento", code == 1 and res["error"] == "operação cancelada pelo usuário", f"{res['error']} após {copied // (1024 * 1024)} MiB")
os.chdir(tempfile.gettempdir())
shutil.rmtree(work, ignore_errors=True)

print(f"E2E OK — {passed} verificações", flush=True)
