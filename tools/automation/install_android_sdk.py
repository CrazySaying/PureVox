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
"""Android SDK 安装（CI 与本机同一份；`sdkmanager` 由调用方保证在 PATH 上）。

`sdkmanager` 的默认包列表里有**已下架的 `tools`**，必须显式给平台与 build-tools；
`android-actions/setup-android` 只为它自己的 `packages` 装包，平台/build-tools 由本脚本负责。

版本号来自 `tools/automation/versions.env`（唯一来源），不在这里硬编码。

注意：本工程**构建时不需要 NDK / CMake**，但原因不是「没有 JNI」——`libpvopus.so`
（libopus 1.5.2 + `cpp/pv_opus.cpp` 的 JNI 转发层）是用 NDK **预编译并入库**的产物，
放在 `android/app/src/main/jniLibs/arm64-v8a/`，Gradle 不跑 native 代码，所以 CI 与本机
都不必装 NDK。要重建那个 .so 才需要 NDK + CMake，走
`tools/automation/build_opus_android.py`。**勿在本脚本里加 NDK。**

用法：
  python tools/automation/install_android_sdk.py
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

# 中文输出必须先切 UTF-8（Windows CI runner 默认 cp1252），见 _utf8.py 的说明。
sys.path.insert(0, str(Path(__file__).resolve().parent))
import _utf8  # noqa: E402

_utf8.force()

ROOT = Path(__file__).resolve().parents[2]
VERSIONS = ROOT / "tools" / "automation" / "versions.env"


def load_versions() -> dict[str, str]:
    """读 versions.env（KEY=VALUE，bash 与 pwsh 都能解析的简单格式）。"""
    out = {}
    for line in VERSIONS.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1)
        out[k.strip()] = v.strip()
    return out


def _probe_java(java_home: str) -> int | None:
    """问 java 自己要版本号（主版本号，8/11/17…）。问不到返回 None。"""
    java = Path(java_home) / "bin" / ("java.exe" if os.name == "nt" else "java")
    if not java.is_file():
        found = shutil.which("java")
        if not found:
            return None
        java = Path(found)
    try:
        out = subprocess.run([str(java), "-version"], capture_output=True, text=True,
                             encoding="utf-8", errors="replace", timeout=30)
    except (OSError, subprocess.SubprocessError):
        return None
    # java -version 打在 stderr，形如：openjdk version "17.0.11" 2024-04-16
    for stream in (out.stderr, out.stdout):
        m = re.search(r'version "(\d+)', stream or "")
        if m:
            return int(m.group(1))
    return None


def main() -> int:
    v = load_versions()
    for key in ("ANDROID_PLATFORM", "ANDROID_BUILD_TOOLS"):
        if key not in v:
            raise SystemExit(f"versions.env 缺少 {key}")

    sdkmanager = shutil.which("sdkmanager")
    if not sdkmanager:
        # Windows 上它只以 .bat 存在，而 shutil.which 会跳过 .bat（PATHEXT 不含），
        # 所以显式再找一次 .bat / .cmd。
        sdkmanager = shutil.which("sdkmanager.bat") or shutil.which("sdkmanager.cmd")
    if not sdkmanager:
        # PATH 里没有就退回 ANDROID_HOME/sdk_root 下的 cmdline-tools —— 本机装 SDK 时
        # 通常只设了 ANDROID_HOME，没把 bin 加进 PATH（shutil.which 因此找不到）。
        for var in ("ANDROID_HOME", "ANDROID_SDK_ROOT"):
            root = os.environ.get(var)
            if not root:
                continue
            for name in ("sdkmanager", "sdkmanager.bat", "sdkmanager.cmd"):
                cand = Path(root) / "cmdline-tools" / "latest" / "bin" / name
                if cand.is_file():
                    sdkmanager = str(cand)
                    break
            if sdkmanager:
                break
    if not sdkmanager:
        raise SystemExit(
            "找不到 sdkmanager。CI 里由 android-actions/setup-android 提供；"
            "本机请设 ANDROID_HOME（或把它加进 PATH）。"
        )

    # **sdkmanager 自身对 JDK 版本有要求**：它需要 JDK 17（class file version 61）。
    # 本机的 JAVA_HOME 可能指向 JDK 8/11（报错是 UnsupportedClassVersionError，
    # 说「this version of the Java Runtime only recognizes class file versions up to
    # 52.0」—— 52 就是 Java 8），而 CI 里 setup-java 已经给了 17。
    # 所以这里显式检查，不合格就报清楚，而不是把一屏 Java 栈丢出来。
    java_home = os.environ.get("JAVA_HOME", "")
    java_ver = _probe_java(java_home)
    if java_ver and java_ver < 17:
        raise SystemExit(
            f"JDK 版本过低（{java_ver}），sdkmanager 需要 **JDK 17**。\n"
            f"  JAVA_HOME={java_home or '(未设置)'}\n"
            "  改：CI 里由 actions/setup-java 提供（versions.env 的 JDK_VER）；\n"
            "      本机请把 JAVA_HOME 指到 JDK 17 再跑。"
        )

    # 接受 licenses：sdkmanager 没有 --yes，非交互下靠 yes 喂 stdin
    env = dict(os.environ)
    subprocess.run(
        [sdkmanager, "--licenses"], stdin=subprocess.DEVNULL,
        capture_output=True, text=True, env=env,
    )
    # 已是最新时退出码非 0 也无所谓，这里只求「别卡在等输入」

    pkgs = [f"platforms;{v['ANDROID_PLATFORM']}", f"build-tools;{v['ANDROID_BUILD_TOOLS']}"]
    print(f"安装 {', '.join(pkgs)}", flush=True)
    rc = subprocess.run([sdkmanager, *pkgs], env=env).returncode
    if rc != 0:
        raise SystemExit(f"sdkmanager 失败（exit={rc}）")
    print(f"Android SDK ready: {', '.join(pkgs)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())