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
"""`assert_bundle.py` 的 Linux 实现（deb / rpm / AppImage）。

拆文件而不是在主脚本里 `if platform`：产物格式、安装方式、启动方式三边都不一样
（AGENTS §4）。
"""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path


def _newest(dir_path: Path, pattern: str) -> Path | None:
    """取最新的一个产物（同 assert_bundle_windows.py：bundle 目录会累积历次构建）。"""
    if not dir_path.is_dir():
        return None
    hits = [p for p in dir_path.glob(pattern) if p.is_file()]
    if not hits:
        return None
    return max(hits, key=lambda p: p.stat().st_mtime)


def find_artifacts(bundle: Path) -> list[Path]:
    """deb 是主产物（要解包断言布局）；rpm 与 AppImage 只查存在性。"""
    deb = _newest(bundle / "deb", "*.deb")
    if deb is None:
        raise SystemExit(f"找不到 deb 产物（{bundle / 'deb'}/*.deb）")
    return [deb]


def install(bundle: Path, tmp: Path) -> tuple[Path, Path, list[str]]:
    """解包 deb（不真装 —— 真装要 root，而且会污染 runner）。"""
    deb = find_artifacts(bundle)[0]
    dpkg = shutil.which("dpkg-deb")
    if not dpkg:
        raise SystemExit("找不到 dpkg-deb（apt 包 dpkg），无法解包 deb")
    subprocess.run([dpkg, "-x", str(deb), str(tmp)], check=True)

    # Tauri 的 deb 把可执行文件装到 /usr/bin/purevox
    cands = sorted(p for p in tmp.rglob("purevox") if p.is_file() and p.parent.name == "bin")
    if not cands:
        raise SystemExit("deb 内缺少可执行文件（*/bin/purevox）")
    # 模型随 deb 落在 usr/lib/<productName>
    return cands[0], tmp, ["usr/lib/<productName>/*.onnx"]


def check_extra_artifacts(bundle: Path) -> None:
    """rpm 必须有；AppImage 是 best-effort —— 缺了只告警不判失败（旧版就这规矩）。"""
    if _newest(bundle / "rpm", "*.rpm") is None:
        raise SystemExit(f"缺少 rpm 产物（{bundle / 'rpm'}/*.rpm）")

    if _newest(bundle / "appimage", "*.AppImage") is not None:
        print("AppImage OK")
    else:
        print("警告：无 AppImage（best-effort，不判失败）")


def smoke_prefix() -> list[str]:
    """CI runner 没有 X server，无头跑要挂 xvfb-run。"""
    if shutil.which("xvfb-run"):
        return ["xvfb-run", "-a"]
    return []