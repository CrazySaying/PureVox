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
# 重建 Android 端的 libopus 静态库 + libpvopus.so（预编译产物**入库**，平时打包用不到本脚本）。
# Windows 版见同目录的 build_opus_android.ps1 —— 两份是同一件事的两个平台入口。
#
# 为什么要「产物入库、构建不编译」：.so 直接放 app/src/main/jniLibs/arm64-v8a/，
# Gradle 构建就不需要 NDK —— CI 也就不用装 NDK、不用多一步编译。和电脑端的
# server/opus.dll（预编译入库、运行时加载）是同一条路子。
#
# 只在两种时候需要跑本脚本：
#   1. 升级 libopus 版本（改 OPUS_VERSION）
#   2. 改了 cpp/pv_opus.cpp 或 cpp/CMakeLists.txt
#
# 产物：
#   third_party/opus/include/*.h        ← libopus 公开头（JNI 只需要这些）
#   third_party/opus/arm64-v8a/libopus.a
#   jniLibs/arm64-v8a/libpvopus.so
#
# 用法： tools/automation/build_opus_android.sh [版本]
#        加 --keep-tmp 保留下载与编译的临时目录（排查编译问题时用）。
#
# 前置：Android NDK（含 clang/cmake）与 cmake、ninja、curl、tar。

set -euo pipefail

OPUS_VERSION="1.5.2"
KEEP_TMP=0
for a in "$@"; do
  case "$a" in
    --keep-tmp) KEEP_TMP=1 ;;
    [0-9]*) OPUS_VERSION="$a" ;;
    *) echo "未知参数：$a（用法：$0 [版本] [--keep-tmp]）" >&2; exit 2 ;;
  esac
done

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cppdir="$repo/android/app/src/main/cpp"
opusdir="$cppdir/third_party/opus"
jnidir="$repo/android/app/src/main/jniLibs/arm64-v8a"

# ---- NDK 定位 ----
# 优先级：显式环境变量 > $ANDROID_HOME/ndk 里版本号最大的那个 > $HOME 下的默认值。
# 版本号比较用 sort -V（27.0.12077973 > 26.3.11579264，字典序会把 26 当成更大）。
ndk=""
for c in "${ANDROID_NDK_HOME:-}" "${ANDROID_NDK_ROOT:-}"; do
  [ -n "$c" ] && [ -d "$c" ] && { ndk="$c"; break; }
done
if [ -z "$ndk" ]; then
  for root in "${ANDROID_HOME:-}" "${ANDROID_SDK_ROOT:-}" "$HOME/Android/Sdk" "$HOME/Library/Android/sdk"; do
    [ -n "$root" ] && [ -d "$root/ndk" ] || continue
    last="$(find "$root/ndk" -mindepth 1 -maxdepth 1 -type d | sort -V | tail -1)"
    [ -n "$last" ] && { ndk="$last"; break; }
  done
fi
[ -n "$ndk" ] || { echo "找不到 NDK。设 ANDROID_NDK_HOME，或用 sdkmanager \"ndk;27.0.12077973\" 装一个。" >&2; exit 1; }
toolchain="$ndk/build/cmake/android.toolchain.cmake"
[ -f "$toolchain" ] || { echo "NDK 里没有 $toolchain（$ndk 不是完整 NDK？）" >&2; exit 1; }

# cmake/ninja：系统里的优先，否则找 SDK 自带的（SDK 版带的是 Windows 版 ninja，
# Linux 上必须用系统 ninja —— CMake 只按当前平台找 ninja）。
cmake_bin="$(command -v cmake || true)"
[ -n "$cmake_bin" ] || { echo "找不到 cmake。" >&2; exit 1; }
command -v ninja >/dev/null || { echo "找不到 ninja。" >&2; exit 1; }

# llvm-nm 只在 NDK 里，不在 PATH 上
nm_bin="$(find "$ndk/toolchains/llvm/prebuilt" -maxdepth 2 -name llvm-nm -type f | head -1)"
[ -n "$nm_bin" ] || { echo "NDK 里找不到 llvm-nm。" >&2; exit 1; }

echo "NDK    : $ndk"
echo "cmake  : $("$cmake_bin" --version | head -1)"
echo "libopus: $OPUS_VERSION"

tmp="$(mktemp -d "${TMPDIR:-/tmp}/pv_opus_XXXXXX")"
cleanup() { [ "$KEEP_TMP" = 1 ] || rm -rf "$tmp"; }
trap cleanup EXIT

# ---- 1. 取源码 ----
srcdir="$tmp/opus-$OPUS_VERSION"
if [ ! -d "$srcdir" ]; then
  url="https://github.com/xiph/opus/releases/download/v$OPUS_VERSION/opus-$OPUS_VERSION.tar.gz"
  echo
  echo "[1/4] 下载 $url"
  curl -fL --retry 3 -o "$tmp/opus.tar.gz" "$url"
  tar xzf "$tmp/opus.tar.gz" -C "$tmp"
fi
# 发版包里没有 .git，libopus 的 CMake 会退回到 package_version 文件（已验证可用）
[ -d "$srcdir" ] || { echo "源码解压后没找到 $srcdir" >&2; exit 1; }

# ---- 2. 编 libopus.a ----
echo
echo "[2/4] 编译 libopus.a（arm64-v8a, API 24）"
"$cmake_bin" -G Ninja \
  -DCMAKE_TOOLCHAIN_FILE="$toolchain" \
  -DANDROID_ABI=arm64-v8a \
  -DANDROID_PLATFORM=android-24 \
  -DCMAKE_BUILD_TYPE=Release \
  -DBUILD_SHARED_LIBS=OFF \
  -DOPUS_BUILD_PROGRAMS=OFF \
  -DOPUS_BUILD_TESTING=OFF \
  -DOPUS_INSTALL_PKG_CONFIG_MODULE=OFF \
  -S "$srcdir" -B "$tmp/build-opus"
"$cmake_bin" --build "$tmp/build-opus"

# ---- 3. 编 libpvopus.so（JNI 转发层）----
echo
echo "[3/4] 编译 libpvopus.so"
mkdir -p "$opusdir/arm64-v8a" "$opusdir/include"
cp "$tmp/build-opus/libopus.a" "$opusdir/arm64-v8a/libopus.a"
cp "$srcdir"/include/*.h "$opusdir/include/"
# libopus 的许可证文件名各版本不一（COPYING / LICENSE），挨个找
for f in "$srcdir"/COPYING* "$srcdir"/LICENSE*; do
  [ -f "$f" ] && { cp "$f" "$opusdir/LICENSE-opus"; break; }
done

"$cmake_bin" -G Ninja \
  -DCMAKE_TOOLCHAIN_FILE="$toolchain" \
  -DANDROID_ABI=arm64-v8a \
  -DANDROID_PLATFORM=android-24 \
  -DCMAKE_BUILD_TYPE=Release \
  -S "$cppdir" -B "$tmp/build-jni"
"$cmake_bin" --build "$tmp/build-jni"

# ---- 4. 落位 + 校验 ----
echo
echo "[4/4] 落位并校验"
mkdir -p "$jnidir"
cp "$tmp/build-jni/libpvopus.so" "$jnidir/libpvopus.so"

# 必须只导出 JNI 入口：libopus 的 C 符号漏出去会污染 .so 的符号表（更大，
# 还可能与 app 里的同名符号撞车），所以这一条要当门禁看，不是「看一眼」。
stray="$("$nm_bin" -D --defined-only "$jnidir/libpvopus.so" |
  awk '{print $NF}' | grep -E '^(Java_|opus_)' | grep -v '^Java_' || true)"
jni_count="$("$nm_bin" -D --defined-only "$jnidir/libpvopus.so" |
  awk '{print $NF}' | grep -c '^Java_' || true)"
if [ -n "$stray" ]; then
  echo "导出了非 JNI 符号（应为 0）：$(echo "$stray" | tr '\n' ' ')" >&2
  exit 1
fi
if [ "$jni_count" -lt 8 ]; then
  echo "只导出 $jni_count 个 JNI 符号，疑似 visibility 配置丢了" >&2
  exit 1
fi

kb=$(( $(wc -c < "$jnidir/libpvopus.so") / 1024 ))
echo
echo "完成：libpvopus.so ${kb} KB，$jni_count 个 JNI 符号，0 个泄漏。"
echo "打包：cd android && ./gradlew assembleDebug"