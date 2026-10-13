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

<#
.SYNOPSIS
    重建 Android 端的 libopus 静态库 + libpvopus.so（预编译产物**入库**，平时打包用不到本脚本）。

.DESCRIPTION
    为什么要「产物入库、构建不编译」：.so 直接放 app/src/main/jniLibs/arm64-v8a/，
    Gradle 构建就不需要 NDK——CI 也就不用装 NDK、不用多一步编译。和电脑端的
    server/opus.dll（预编译入库、运行时加载）是同一条路子。

    只在两种时候需要跑本脚本：
      1. 升级 libopus 版本（改 $OpusVersion）
      2. 改了 cpp/pv_opus.cpp 或 cpp/CMakeLists.txt

    产物：
      third_party/opus/include/*.h        ← libopus 公开头（JNI 只需要这些）
      third_party/opus/arm64-v8a/libopus.a
      jniLibs/arm64-v8a/libpvopus.so

.PARAMETER OpusVersion
    libopus 版本号，默认 1.5.2。改这里等于换库，**必须同步** LICENSE-THIRD-PARTY.txt。

.PARAMETER KeepTemp
    保留下载与编译的临时目录（排查编译问题时用）。

.EXAMPLE
    pwsh tools/automation/build_opus_android.ps1
#>

[CmdletBinding()]
param(
    [string]$OpusVersion = '1.5.2',
    [switch]$KeepTemp
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repo     = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$cppDir   = Join-Path $repo 'android\app\src\main\cpp'
$opusDir  = Join-Path $cppDir 'third_party\opus'
$jniDir   = Join-Path $repo 'android\app\src\main\jniLibs\arm64-v8a'
$sdk      = if ($env:ANDROID_HOME) { $env:ANDROID_HOME } else { Join-Path $env:LOCALAPPDATA 'Android\Sdk' }

# ---- 工具链 ----
$ndk = Get-ChildItem (Join-Path $sdk 'ndk') -Directory -ErrorAction SilentlyContinue |
       Sort-Object Name -Descending | Select-Object -First 1
if (-not $ndk) { throw "找不到 NDK（$sdk\ndk）。用 sdkmanager \"ndk;27.0.12077973\" 装一个。" }
$cmakeRoot = Get-ChildItem (Join-Path $sdk 'cmake') -Directory -ErrorAction SilentlyContinue |
             Sort-Object Name -Descending | Select-Object -First 1
if (-not $cmakeRoot) { throw "找不到 cmake（$sdk\cmake）。用 sdkmanager \"cmake;3.22.1\" 装一个。" }

$cmake = Join-Path $cmakeRoot.FullName 'bin\cmake.exe'
$ninja = Join-Path $cmakeRoot.FullName 'bin\ninja.exe'
# CMake 的 -G Ninja 只在 PATH 里找 ninja.exe；不预置它就会报
# "CMake was unable to find a build program corresponding to Ninja"。
$env:PATH = "$(Split-Path $ninja);$env:PATH"

Write-Host "NDK    : $($ndk.Name)"
Write-Host "cmake  : $(& $cmake --version | Select-Object -First 1)"
Write-Host "libopus: $OpusVersion"

$tmp = Join-Path ([IO.Path]::GetTempPath()) "pv_opus_$OpusVersion"
if ($KeepTemp) {
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
} else {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
}

try {
    # ---- 1. 取源码 ----
    $srcDir = Join-Path $tmp "opus-$OpusVersion"
    if (-not (Test-Path $srcDir)) {
        $tar = Join-Path $tmp 'opus.tar.gz'
        $url = "https://github.com/xiph/opus/releases/download/v$OpusVersion/opus-$OpusVersion.tar.gz"
        Write-Host "`n[1/4] 下载 $url"
        Invoke-WebRequest -Uri $url -OutFile $tar -TimeoutSec 300 -UseBasicParsing
        Push-Location $tmp
        try { tar xzf $tar } finally { Pop-Location }
    }
    # 发版包里没有 .git，libopus 的 CMake 会退回到 package_version 文件（已验证可用）
    if (-not (Test-Path $srcDir)) { throw "源码解压后没找到 $srcDir" }

    # ---- 2. 编 libopus.a ----
    Write-Host "`n[2/4] 编译 libopus.a（arm64-v8a, API 24）"
    $opusBuild = Join-Path $tmp 'build-opus'
    & $cmake -G Ninja `
        "-DCMAKE_TOOLCHAIN_FILE=$(Join-Path $ndk.FullName 'build\cmake\android.toolchain.cmake')" `
        -DANDROID_ABI=arm64-v8a `
        -DANDROID_PLATFORM=android-24 `
        -DCMAKE_BUILD_TYPE=Release `
        -DBUILD_SHARED_LIBS=OFF `
        -DOPUS_BUILD_PROGRAMS=OFF `
        -DOPUS_BUILD_TESTING=OFF `
        -DOPUS_INSTALL_PKG_CONFIG_MODULE=OFF `
        -S $srcDir -B $opusBuild
    if ($LASTEXITCODE -ne 0) { throw "libopus configure 失败（exit $LASTEXITCODE）" }
    & $cmake --build $opusBuild
    if ($LASTEXITCODE -ne 0) { throw "libopus 编译失败（exit $LASTEXITCODE）" }

    # ---- 3. 编 libpvopus.so（JNI 转发层）----
    Write-Host "`n[3/4] 编译 libpvopus.so"
    New-Item -ItemType Directory -Force -Path (Join-Path $opusDir 'arm64-v8a') | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $opusDir 'include') | Out-Null
    Copy-Item (Join-Path $opusBuild 'libopus.a') (Join-Path $opusDir 'arm64-v8a\libopus.a') -Force
    Copy-Item (Join-Path $srcDir 'include\*.h') (Join-Path $opusDir 'include') -Force
    # libopus 的许可证文件名各版本不一（COPYING / LICENSE），挨个找
    $lic = Get-ChildItem $srcDir -File | Where-Object { $_.Name -match '^(COPYING|LICENSE)' } | Select-Object -First 1
    if ($lic) { Copy-Item $lic.FullName (Join-Path $opusDir 'LICENSE-opus') -Force }

    $jniBuild = Join-Path $tmp 'build-jni'
    & $cmake -G Ninja `
        "-DCMAKE_TOOLCHAIN_FILE=$(Join-Path $ndk.FullName 'build\cmake\android.toolchain.cmake')" `
        -DANDROID_ABI=arm64-v8a `
        -DANDROID_PLATFORM=android-24 `
        -DCMAKE_BUILD_TYPE=Release `
        -S $cppDir -B $jniBuild
    if ($LASTEXITCODE -ne 0) { throw "JNI configure 失败（exit $LASTEXITCODE）" }
    & $cmake --build $jniBuild
    if ($LASTEXITCODE -ne 0) { throw "JNI 编译失败（exit $LASTEXITCODE）" }

    # ---- 4. 落位 + 校验 ----
    Write-Host "`n[4/4] 落位并校验"
    New-Item -ItemType Directory -Force -Path $jniDir | Out-Null
    Copy-Item (Join-Path $jniBuild 'libpvopus.so') (Join-Path $jniDir 'libpvopus.so') -Force

    # 必须只导出 JNI 入口：libopus 的 C 符号漏出去会污染 .so 的符号表（更大，
    # 还可能与 app 里的同名符号撞车），所以这一条要当门禁看，不是「看一眼」。
    $nm = Join-Path $ndk.FullName 'toolchains\llvm\prebuilt\windows-x86_64\bin\llvm-nm.exe'
    $so = Join-Path $jniDir 'libpvopus.so'
    $exported = & $nm -D --defined-only $so |
        ForEach-Object { ($_ -split '\s+')[-1] } |
        Where-Object { $_ -match '^(Java_|opus_)' }
    $stray = @($exported | Where-Object { $_ -notlike 'Java_*' })
    $jniCount = @($exported | Where-Object { $_ -like 'Java_*' }).Count
    if ($stray.Count -gt 0) {
        throw "导出了非 JNI 符号（应为 0）：$($stray -join ', ')"
    }
    if ($jniCount -lt 8) {
        throw "只导出 $jniCount 个 JNI 符号，疑似 visibility 配置丢了"
    }

    $kb = [math]::Round((Get-Item $so).Length / 1KB)
    Write-Host "`n完成：libpvopus.so ${kb} KB，$jniCount 个 JNI 符号，0 个泄漏。"
    Write-Host "打包：cd android && gradlew assembleDebug"
} finally {
    if (-not $KeepTemp) { Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue }
}