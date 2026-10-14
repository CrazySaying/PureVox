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
"""`assert_bundle.py` 的 Windows 实现（MSI / NSIS）。

拆文件而不是在主脚本里 `if platform`：产物格式、安装方式、启动方式三边都不一样，
塞进一个函数只会变成一堆互相穿插的分支（AGENTS §4）。
"""

from __future__ import annotations

import subprocess
from pathlib import Path


def _newest(dir_path: Path, pattern: str) -> Path | None:
    """取最新的一个产物。

    **必须挑最新的，不能拿排序第一个**：`target/release/bundle/` 会**累积**历次构建的
    产物（我这台机器上有 4 个 setup.exe、3 个 msi，全是不同日期的）。按名字排序取第一个
    会拿到几个月前那个旧的，断言它的体积/布局 —— 测的其实是上次的包，不是刚打的包。
    按 mtime 选才是「刚出的那个」。
    """
    if not dir_path.is_dir():
        return None
    hits = [p for p in dir_path.glob(pattern) if p.is_file()]
    if not hits:
        return None
    return max(hits, key=lambda p: p.stat().st_mtime)


def find_artifacts(bundle: Path) -> list[Path]:
    """NSIS 的 .exe 与 MSI 的 .msi，两种都要在。"""
    out = []
    exe = _newest(bundle / "nsis", "*.exe")
    if exe is None:
        raise SystemExit(f"找不到 NSIS 产物（{bundle / 'nsis'}/*.exe）")
    out.append(exe)

    msi = _newest(bundle / "msi", "*.msi")
    if msi is None:
        raise SystemExit(f"找不到 MSI 产物（{bundle / 'msi'}/*.msi）")
    out.append(msi)
    return out


def install(bundle: Path, tmp: Path) -> tuple[Path, Path, list[str]]:
    """NSIS 静默安装到 tmp（**验证真实安装布局**，不是解包）。"""
    nsis = find_artifacts(bundle)[0]
    print(f"静默安装到 {tmp}")
    # /S 静默；/D 必须在最后且不能带引号（NSIS 的老规矩，带引号会把路径当字面量）
    subprocess.run([str(nsis), "/S", f"/D={tmp}"], check=True)

    exe = tmp / "PureVox.exe"
    # 资源落位可能是 <安装目录>/models 或 <安装目录>/resources/models
    # （model_path 两者都认），所以模型检查扫整个 tmp。
    return exe, tmp, ["<安装目录>/models", "<安装目录>/resources/models"]


def check_extra_artifacts(bundle: Path) -> None:
    """Windows 只有这两种产物，都已在 find_artifacts 里查过，没有额外项。"""
    return


def smoke_prefix() -> list[str]:
    """Windows 上直接起 exe，不需要显示环境。"""
    return []