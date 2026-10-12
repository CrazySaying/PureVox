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
# 构建版本解析（Windows / pwsh 用；与 version.sh 同一契约，产出同名变量）：
#   TAG     = v2026.10.08.1430   完整时间戳，唯一且按时间全序（比较更新的依据）
#   VERSION = 26.10.8            应用/包版本 = 年.月.日（semver 三段）
#   STAMP   = 2026-10-08-1430    产物文件名形式（含时分故唯一）
#   ACODE   = 2610081430         Android versionCode = yyMMddHHmm（单调递增）
#   PUREVOX_BUILD_VERSION / PUREVOX_BUILD_STAMP → 编译进程序（见 src/version.rs）
# 为什么 VERSION 只有三段见 version.sh 头注（Tauri semver / MSI ProductVersion 上限）。
#   tag 触发 GITHUB_REF_NAME=v2026.10.08.1430；否则取当前 UTC。
#   同时写 `src-tauri/.build-version.json`（Tauri `--config`，不提交）。
# 用法：CI `./tools/automation/version.ps1 | Out-File -FilePath $env:GITHUB_ENV -Append -Encoding utf8`
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$ref = $env:GITHUB_REF_NAME
if ($ref -and $ref.StartsWith('v')) {
  $p = $ref.Substring(1).Split('.')   # v2026.10.08.1430 → y=2026 mo=10 d=08 hm=1430
  $y = $p[0]; $mo = $p[1]; $d = $p[2]; $hm = $p[3]
} else {
  $t = (Get-Date).ToUniversalTime()
  $y = $t.ToString('yyyy'); $mo = $t.ToString('MM'); $d = $t.ToString('dd'); $hm = $t.ToString('HHmm')
}
# [int] 去掉前导零（PowerShell 不像 bash 把 09 当八进制，但保持两边写法一致）：
$yy = [int]$y % 100
$mo_n = [int]$mo
$d_n = [int]$d
$hm_n = [int]$hm
$hm4 = $hm.PadLeft(4, '0')

$tag = "v$y.$mo.$d.$hm"
$ver = "$yy.$mo_n.$d_n"
$stamp = "$y-$mo-$d-$hm"
$acode = "$yy$($mo_n.ToString('00'))$($d_n.ToString('00'))$hm4"

$cfg = Join-Path $root 'src-tauri/.build-version.json'
$json = '{"version":"' + $ver + '"}' + "`n"
[IO.File]::WriteAllText($cfg, $json, (New-Object Text.UTF8Encoding $false))

"TAG=$tag"
"VERSION=$ver"
"STAMP=$stamp"
"ACODE=$acode"
"PUREVOX_BUILD_VERSION=$ver"
"PUREVOX_BUILD_STAMP=$y.$mo.$d.$hm"