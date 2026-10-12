#!/usr/bin/env bash
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
#
# 构建版本解析（**版本号 = 年.月.日**；与 version.ps1 同一契约，唯一实现思路一致）：
#   tag 触发  GITHUB_REF_NAME=v2026.10.08.1430
#   本机/手动                                  → 当前 UTC 日期时间
#   产出（全部写入 $GITHUB_ENV / 本机 eval）：
#     TAG     = v2026.10.08.1430        完整时间戳，唯一且按时间全序（**比较更新的依据**）
#     VERSION = 26.10.8                 应用/包版本 = 年.月.日（semver 三段，Tauri/MSI 硬要求）
#     STAMP   = 2026-10-08-1430         完整时间戳的文件名形式（产物重命名用，含时分故唯一）
#     ACODE   = 2610081430              Android versionCode = yyMMddHHmm（单调递增，int32 内）
#     PUREVOX_BUILD_VERSION / PUREVOX_BUILD_STAMP → 编译进程序（见 src/version.rs）
#
# 为什么 VERSION 只有三段：Tauri 要 semver 三段、MSI ProductVersion 限 major≤255 /
# minor≤255 / build≤65535，`2026.10.08` 直译会报 major > 255。故 major=年-2000、minor=月、
# build=日（≤31，远低于上限）。**同日多次发布靠 TAG 的时分区分**，更新检查比较 TAG
# （见 src/update/），因此显示版本重复不影响更新判定。
#
# 同时生成 Tauri 版本覆盖配置 `src-tauri/.build-version.json`（不提交，见该目录 .gitignore），
# 构建时用 `cargo tauri build --config .build-version.json`。
# 用法：本机 `eval "$(bash tools/automation/version.sh)"`；
#       CI（Linux）`bash tools/automation/version.sh >> "$GITHUB_ENV"`。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ref="${GITHUB_REF_NAME:-}"
if [[ "$ref" == v* ]]; then
  IFS='.' read -r y mo d hm <<<"${ref#v}"
else
  IFS='.' read -r y mo d hm <<<"$(date -u +%Y.%m.%d.%H%M)"
fi
# 10# 去掉前导零（bash 把 09 当八进制）：月 09→9、日 09→9、时分 0629→629。
yy=$((10#${y} % 100))
mo_n=$((10#${mo}))
d_n=$((10#${d}))
hm_n=$((10#${hm}))

tag="v${y}.${mo}.${d}.${hm}"
ver="${yy}.${mo_n}.${d_n}"
stamp="${y}-${mo}-${d}-${hm}"
acode="${yy}$(printf '%02d' "$mo_n")$(printf '%02d' "$d_n")${hm}"

printf '{"version":"%s"}\n' "$ver" >"$ROOT/src-tauri/.build-version.json"
printf 'TAG=%s\n' "$tag"
printf 'VERSION=%s\n' "$ver"
printf 'STAMP=%s\n' "$stamp"
printf 'ACODE=%s\n' "$acode"
printf 'PUREVOX_BUILD_VERSION=%s\n' "$ver"
printf 'PUREVOX_BUILD_STAMP=%s\n' "${y}.${mo}.${d}.${hm}"