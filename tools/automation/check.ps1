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

# 代码质量门禁的唯一实现路径：CI 各 job 与本机开发跑同一份脚本。
#   pwsh tools/automation/check.ps1                 # 全部门禁
#   pwsh tools/automation/check.ps1 -Gate fmt       # 单项（CI 按 step 拆开跑，日志好定位）
# 门禁项：fmt（rustfmt 校验）/ clippy（-D warnings 零容忍）/ build / test / ui（tsc + i18n_lint）
#          / version（版本号推导，两个平台脚本必须同契约）。
param(
  [ValidateSet('fmt', 'clippy', 'build', 'test', 'ui', 'version', 'all')]
  [string]$Gate = 'all'
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$tauri = Join-Path $root 'src-tauri'
# 跨平台：Windows 与 Linux/macOS 的 cargo 都在 $HOME/.cargo/bin（PowerShell 的 $HOME
# 在 Windows 上等于 USERPROFILE）。用 PathSeparator 拼，不能写死 ';'。
$env:Path = "$(Join-Path $HOME '.cargo/bin')$([IO.Path]::PathSeparator)$env:Path"

function Invoke-Gate([string]$name, [scriptblock]$body) {
  Write-Host "==> $name" -ForegroundColor Cyan
  & $body
  if ($LASTEXITCODE -ne 0 -and $null -ne $LASTEXITCODE) {
    throw "门禁 $name 未通过（exit=$LASTEXITCODE）"
  }
}

$gates = switch ($Gate) {
  'all' { @('fmt', 'clippy', 'build', 'test', 'ui', 'version') }
  default { @($Gate) }
}

foreach ($g in $gates) {
  switch ($g) {
    'fmt' {
      Invoke-Gate 'cargo fmt --check' {
        Push-Location $tauri
        try { cargo fmt --check } finally { Pop-Location }
      }
    }
    'clippy' {
      Invoke-Gate 'cargo clippy -D warnings' {
        Push-Location $tauri
        try { cargo clippy --all-targets -- -D warnings } finally { Pop-Location }
      }
    }
    'build' {
      Invoke-Gate 'cargo build' {
        Push-Location $tauri
        try { cargo build } finally { Pop-Location }
      }
    }
    'test' {
      Invoke-Gate 'cargo test' {
        Push-Location $tauri
        try { cargo test } finally { Pop-Location }
      }
    }
    'ui' {
      Invoke-Gate 'tsc (ui/jsconfig.json)' {
        Push-Location (Join-Path $tauri 'ui')
        try { tsc -p jsconfig.json --noEmit } finally { Pop-Location }
      }
      Invoke-Gate 'i18n_lint (ui 字符串门禁)' {
        node (Join-Path $root 'tools/automation/i18n_lint.js')
      }
    }
    'version' {
      # 版本号推导：version.ps1 与 version.sh 是**同一个契约的两份实现**（Windows / Linux 各一），
      # 但它们从不同时被 CI 跑——漂移了没人知道，表现是「Linux 打出的包版本比 Windows 的旧」。
      # 这里对同一批 tag（含 09 月/09 日/凌晨时分等前导零陷阱）断言两者输出**逐字节相同**，
      # 且等于期望值。纯本地、不联网：更新检测的另一半（tag 比对）在 src/update 的单测里。
      # Windows 上 `Get-Command bash` 命中的是 **WSL launcher**（System32\bash.exe），
      # 不是 Git Bash——两者路径风格不同（Git Bash 认 `/d/...`，WSL 认 `/mnt/d/...`），
      # 喂错就是 "No such file"。按 Git Bash → WSL → Linux 原生的顺序显式选，并记下风格。
      # CI 的 windows-latest 必带 Git for Windows，所以那条路是稳定的主路径。
      $bashExe = $null
      $bashMode = 'posix'      # posix | git | wsl
      if ($env:OS -ne 'Windows_NT') {
        $bashExe = (Get-Command bash -ErrorAction Stop).Source
      } else {
        foreach ($cand in @(
            (Join-Path $env:ProgramFiles 'Git\bin\bash.exe'),
            (Join-Path ${env:ProgramFiles(x86)} 'Git\bin\bash.exe'),
            (Join-Path $env:LOCALAPPDATA 'Programs\Git\bin\bash.exe'))) {
          if ($cand -and (Test-Path $cand)) { $bashExe = $cand; $bashMode = 'git'; break }
        }
        if (-not $bashExe) {
          $wslCmd = Get-Command wsl.exe -ErrorAction SilentlyContinue
          if ($wslCmd) { $bashExe = $wslCmd.Source; $bashMode = 'wsl' }
        }
      }
      if (-not $bashExe) {
        throw '找不到可用的 bash（需 Git for Windows 或 WSL），无法交叉校验 version.sh'
      }
      function Convert-ToBashPath([string]$Path) {
        if ($bashMode -eq 'posix') { return $Path }
        $t = $Path -replace '\\', '/'
        if ($t -match '^([A-Za-z]):/(.*)$') {
          $drive = $Matches[1].ToLower()
          if ($bashMode -eq 'git') { return "/$drive/$($Matches[2])" }
          return "/mnt/$drive/$($Matches[2])"
        }
        $t
      }

      $verPs1 = Join-Path $root 'tools/automation/version.ps1'
      $verSh = Join-Path $root 'tools/automation/version.sh'
      $verShBash = Convert-ToBashPath $verSh
      # tag → 期望的 TAG/VERSION/STAMP/ACODE/PUREVOX_BUILD_*
      # STAMP 是文件名形式（连字符），PUREVOX_BUILD_STAMP 是给 version.rs 用的点号形式。
      $cases = @(
        @{ Tag = 'v2026.10.14.1530'; VERSION = '26.10.14'; STAMP = '2026-10-14-1530'; ACODE = '2610141530'; B = '2026.10.14.1530' },
        # 09 月 / 09 日 / 凌晨时分：bash 把 09 当八进制，这里就是防它
        @{ Tag = 'v2026.09.09.0901'; VERSION = '26.9.9';   STAMP = '2026-09-09-0901'; ACODE = '2609090901'; B = '2026.09.09.0901' },
        @{ Tag = 'v2026.01.02.0000'; VERSION = '26.1.2';   STAMP = '2026-01-02-0000'; ACODE = '2601020000'; B = '2026.01.02.0000' },
        # 同一天第二次发版：显示版本相同，靠 tag 时分区分（AGENTS §2.6 / version.rs）
        @{ Tag = 'v2026.10.14.2309'; VERSION = '26.10.14'; STAMP = '2026-10-14-2309'; ACODE = '2610142309'; B = '2026.10.14.2309' }
      )

      Invoke-Gate 'version.ps1 / version.sh 同契约' {
        foreach ($c in $cases) {
          $want = @{
            TAG = $c.Tag; VERSION = $c.VERSION; STAMP = $c.STAMP; ACODE = $c.ACODE
            PUREVOX_BUILD_VERSION = $c.VERSION; PUREVOX_BUILD_STAMP = $c.B
          }
          # GITHUB_REF_NAME 是两个脚本唯一的 tag 入口（CI 里由 workflow 设成 tag 名）
          $env:GITHUB_REF_NAME = $c.Tag
          $outPs1 = & $verPs1 2>&1 | Out-String
          # wsl.exe 需要显式 `bash <脚本>`；Git Bash / Linux 直接给脚本路径
          $outSh = if ($bashMode -eq 'wsl') {
            & $bashExe bash $verShBash 2>&1 | Out-String
          } else {
            & $bashExe $verShBash 2>&1 | Out-String
          }
          Remove-Item Env:\GITHUB_REF_NAME -ErrorAction SilentlyContinue

          foreach ($which in @(@{ n = 'version.ps1'; o = $outPs1 }, @{ n = 'version.sh'; o = $outSh })) {
            $kv = @{}
            foreach ($line in ($which.o -split "`n")) {
              $t = $line.Trim()
              if ($t -match '^([A-Z_]+)=(.*)$') { $kv[$Matches[1]] = $Matches[2] }
            }
            foreach ($k in $want.Keys) {
              if ($kv[$k] -ne $want[$k]) {
                throw "$($which.n) 在 tag $($c.Tag) 下 $k = '$($kv[$k])'，期望 '$($want[$k])'"
              }
            }
          }
          # 两份实现逐字节相同：同一契约不许漂移（Windows 上管道会给 ps1 输出加 CRLF，
          # 比较前统一去掉，否则同一个值会被误判成不同）
          $norm = { param($s) (($s -split "`n" | ForEach-Object { $_.TrimEnd("`r") }) | Where-Object { $_ -match '=' }) -join "`n" }
          $n1 = & $norm $outPs1
          $n2 = & $norm $outSh
          if ($n1 -ne $n2) {
            throw "tag $($c.Tag) 下两份实现输出不同：`n--- version.ps1 ---`n$n1`n--- version.sh ---`n$n2"
          }
          Write-Host "  $($c.Tag) -> VERSION=$($c.VERSION) STAMP=$($c.STAMP) ACODE=$($c.ACODE)  两份一致"
        }
      }
    }
  }
}
Write-Host "门禁全部通过（$Gate）" -ForegroundColor Green
