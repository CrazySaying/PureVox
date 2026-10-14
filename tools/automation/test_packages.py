#!/usr/bin/env python3
# PureVox — AI 麦克风降噪工具
# Copyright (C) 2024-2026 a2heng <752848283@qq.com>
#
# PureVox is licensed under the GNU General Public License v3.0 or
# later (GPL-3.0-or-later).  See LICENSE for details.
#
# This program is free software: you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation, either version 3 of the License, or
# (at your option) any later version.
#
# The built-in AI models are NOT covered by the GPL; they are the
# property of a2heng and may only be used with PureVox under
# authorization.  See MODEL-LICENSE.md for details.
#
# SPDX-License-Identifier: GPL-3.0-or-later
"""跨发行版「装得上、起得来」验证（环境模拟）：在**目标发行版容器**里真实安装我们的
安装包，断言依赖解析、文件落位、动态库链接、可执行文件存在。

  - deb       → ubuntu:24.04（apt）
  - rpm       → fedora:latest（dnf）
  - AppImage  → 本机 `--appimage-extract` 解包断言（免 FUSE，不需要容器）

容器运行时默认 `docker`（GitHub runner 自带）；本机可用 `DOCKER=podman` 覆盖。

用法：
  python tools/automation/test_packages.py
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

# 中文输出必须先切 UTF-8（Windows CI runner 默认 cp1252），见 _utf8.py 的说明。
sys.path.insert(0, str(Path(__file__).resolve().parent))
import _utf8  # noqa: E402

_utf8.force()

ROOT = Path(__file__).resolve().parents[2]
BUNDLE = ROOT / "src-tauri" / "target" / "release" / "bundle"

# 容器内断言脚本：可执行文件存在 + 模型齐全 + 动态库无缺失
CHECK = r"""set -euo pipefail
bin="$(command -v purevox || true)"
[ -n "$bin" ] || bin=/usr/bin/purevox
test -x "$bin" || { echo "缺少可执行文件 $bin"; exit 1; }
n=$(find /usr -name '*.onnx' -type f | wc -l)
[ "$n" -ge "$EXPECTED" ] || { echo "模型不全：$n/$EXPECTED"; exit 1; }
missing=$(ldd "$bin" 2>/dev/null | grep 'not found' || true)
[ -z "$missing" ] || { echo "动态库缺失："; echo "$missing"; exit 1; }
echo "OK: $bin, models=$n, ldd 无缺失"
"""


def say(*parts: str) -> None:
    print(*parts, flush=True)


def run_in_container(runtime: str, image: str, pkg_dir: Path, install_cmd: str, expected: int) -> None:
    args = [runtime, "run", "--rm", "--network=host"]
    # 容器共用宿主网络（CI runner 直连；本机配合代理可用）。设了代理环境变量就透传进去，
    # 否则容器内 apt/dnf 可能连不上官方源。
    if any(os.environ.get(v) for v in ("http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY")):
        args += ["-e", "http_proxy", "-e", "https_proxy", "-e", "HTTP_PROXY",
                 "-e", "HTTPS_PROXY", "-e", "no_proxy", "-e", "NO_PROXY"]
    args += ["-e", f"EXPECTED={expected}", "-e", f"CHECK={CHECK}",
             "-v", f"{pkg_dir}:/pkg:ro", image, "bash", "-c",
             f"{install_cmd} && bash -c \"$CHECK\""]
    say(f"  $ {runtime} run … {image}")
    rc = subprocess.run(args).returncode
    if rc != 0:
        raise SystemExit(f"容器内验证失败（{image}，exit={rc}）")


def main() -> int:
    runtime = os.environ.get("DOCKER", "docker")
    if not shutil.which(runtime):
        say(f"找不到容器运行时 '{runtime}'（用 DOCKER=podman 可覆盖）；跨发行版验证需要它")
        return 1

    expected = len(list((ROOT / "models").rglob("*.onnx")))
    say(f"模型数 {expected}")

    deb_dir = BUNDLE / "deb"
    deb = next(iter(sorted(deb_dir.glob("*.deb"))), None) if deb_dir.is_dir() else None
    if deb:
        say("== deb → ubuntu:24.04 ==")
        run_in_container(runtime, "ubuntu:24.04", deb_dir,
                         "apt-get update -qq && "
                         "apt-get install -y -qq --no-install-recommends /pkg/*.deb >/dev/null",
                         expected)
    else:
        say("跳过 deb（无产物）")

    rpm_dir = BUNDLE / "rpm"
    rpm = next(iter(sorted(rpm_dir.glob("*.rpm"))), None) if rpm_dir.is_dir() else None
    if rpm:
        say("== rpm → fedora:latest ==")
        run_in_container(runtime, "fedora:latest", rpm_dir,
                         "dnf install -y --setopt=install_weak_deps=False /pkg/*.rpm >/dev/null",
                         expected)
    else:
        say("跳过 rpm（无产物）")

    appimg_dir = BUNDLE / "appimage"
    appimg = next(iter(sorted(appimg_dir.glob("*.AppImage"))), None) if appimg_dir.is_dir() else None
    if appimg:
        say("== AppImage → 本机解包 ==")
        with tempfile.TemporaryDirectory(prefix="purevox_appimg_") as td:
            # AppImage 需要在目标目录里解包（它会写 squashfs-root 到 CWD）
            proc = subprocess.run([str(appimg), "--appimage-extract"],
                                  cwd=td, capture_output=True, text=True)
            if proc.returncode != 0:
                raise SystemExit(f"--appimage-extract 失败：{proc.stderr.strip()}")
            root = Path(td) / "squashfs-root"
            apprun = root / "AppRun"
            if not apprun.exists():
                raise SystemExit("AppImage 缺少 AppRun")
            n = len(list(root.rglob("*.onnx")))
            if n < expected:
                raise SystemExit(f"AppImage 模型不全：{n}/{expected}")
            say(f"OK: AppRun + models={n}")
    else:
        say("跳过 AppImage（无产物）")

    say("跨发行版验证全部通过")
    return 0


if __name__ == "__main__":
    sys.exit(main())