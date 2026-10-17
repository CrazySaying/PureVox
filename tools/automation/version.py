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
"""构建版本解析（**版本号 = 年.月.日**；跨平台唯一实现）。

产出（全部写到 stdout，CI 重定向进 $GITHUB_ENV / 本机 eval）：

  TAG     = v2026.10.08.1430   完整时间戳，唯一且按时间全序（**比较更新的依据**）
  VERSION = 26.10.8            应用/包版本 = 年.月.日（semver 三段，Tauri 硬要求）
  STAMP   = 2026-10-08-1430    完整时间戳的文件名形式（产物重命名用，含时分故唯一）
  ACODE   = 2610081430         Android versionCode = yyMMddHHmm（单调递增，int32 内）
  PUREVOX_BUILD_VERSION / PUREVOX_BUILD_STAMP → 编译进程序（见 src/version.rs）

为什么 VERSION 只有三段：Tauri 要 semver 三段；历史上还要出 MSI，MSI ProductVersion 限
major≤255 / minor≤255 / build≤65535，`2026.10.08` 直译会报 major > 255。故 major=年-2000、
minor=月、build=日（≤31，远低于上限）。**现在 Windows 只发 NSIS（exe）**，但映射保持不变
（显示版本 / 产物名 / 更新说明都按它走）。**同日多次发布靠 TAG 的时分区分**，更新检查
比较 TAG（见 src/update/），因此显示版本重复不影响更新判定。

同时生成 Tauri 版本覆盖配置 `src-tauri/.build-version.json`（不提交，见该目录
.gitignore），构建时用 `cargo tauri build --config .build-version.json`。

tag 触发时 GITHUB_REF_NAME=v2026.10.08.1430；否则取当前 **UTC**（不是本地时间：
CI 的 runner 与发版机器时区各不相同，只有 UTC 能让「同一天」的含义一致）。

用法：
  本机    eval "$(python3 tools/automation/version.py)"
  CI      python3 tools/automation/version.py >> "$GITHUB_ENV"

只依赖标准库。取代了曾经的 version.ps1 / version.sh 两份实现 —— 那两份从不同时被 CI
跑到，漂移了没人知道，表现是「Linux 打出的包版本比 Windows 的旧」。见 tests.yml 的
`Gate · version`：那份门禁对同一批 tag 断言输出固定。
"""

from __future__ import annotations

import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def derive(ref: str) -> tuple[str, int, int, int, int]:
    """从 tag（或「现在」）推出 (年, 月, 日, 时分HHmm, …)。

    返回的月/日/时分都是**整数**（已去前导零）：bash 的 `$((10#$x))` 和 PowerShell 的
    `[int]$x` 都做同一件事——bash 不加 `10#` 会把 `09` 当八进制，`09` 直接语法错误。
    """
    if ref.startswith("v"):
        parts = ref[1:].split(".")
        if len(parts) != 4:
            raise SystemExit(f"tag 格式应为 v<yyyy.MM.dd.HHmm>，收到：{ref}")
        y, mo, d, hm = parts
        return y, int(mo), int(d), int(hm)
    now = datetime.now(timezone.utc)
    return now.strftime("%Y"), now.month, now.day, now.hour * 100 + now.minute


def main() -> int:
    ref = os.environ.get("GITHUB_REF_NAME", "")
    y, mo, d, hm = derive(ref)
    yy = int(y) % 100

    # tag 里的月/日/时分可能是 `08`、`09`，拼回去时补零（文件名与 versionCode 都要定长）
    mo2, d2, hm4 = f"{mo:02d}", f"{d:02d}", f"{hm:04d}"
    tag = f"v{y}.{mo2}.{d2}.{hm4}"
    ver = f"{yy}.{mo}.{d}"                      # 显示版本：整数，不补零（26.9.9）
    stamp = f"{y}-{mo2}-{d2}-{hm4}"             # 文件名形式
    acode = f"{yy:02d}{mo2}{d2}{hm4}"           # Android versionCode，定长单调

    # Tauri 的版本覆盖配置（不提交；构建时 --config 传进去）
    cfg = ROOT / "src-tauri" / ".build-version.json"
    cfg.write_text(json.dumps({"version": ver}) + "\n", encoding="utf-8", newline="\n")

    for line in (
        f"TAG={tag}",
        f"VERSION={ver}",
        f"STAMP={stamp}",
        f"ACODE={acode}",
        f"PUREVOX_BUILD_VERSION={ver}",
        f"PUREVOX_BUILD_STAMP={y}.{mo2}.{d2}.{hm4}",
    ):
        print(line)
    return 0


if __name__ == "__main__":
    sys.exit(main())