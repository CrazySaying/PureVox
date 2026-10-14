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
"""发布产物断言（对应旧版「Assert packaged bundle layout」+「bundled smoke」）：

  1. 产物存在且体积合理（内置模型 ~68MB，缺模型只有 ~10MB）
  2. **真实安装/解包**到临时目录，断言可执行文件 + 全部模型落位
  3. `--smoke`：启动安装产物，轮询 `/debug` 就绪后杀死（不留进程，自收尾）

平台差异（产物格式、安装方式、启动前缀）拆到 `assert_bundle_windows.py` /
`assert_bundle_linux.py`，本文件只留两边共用的部分：体积下限、模型计数、
冒烟轮询与自收尾（AGENTS §4：平台特征代码按文件分，不在一个文件里 `if platform`）。

用法：
  python tools/automation/assert_bundle.py --smoke
"""

from __future__ import annotations

import argparse
import importlib
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

# 中文输出必须先切 UTF-8（Windows CI runner 默认 cp1252），见 _utf8.py 的说明。
sys.path.insert(0, str(Path(__file__).resolve().parent))
import _utf8  # noqa: E402

_utf8.force()

MIN_MB = 40          # 产物体积下限（MB）：无模型 ~10MB，带模型 >60MB
DEBUG_URL = "http://127.0.0.1:47821/debug"
SMOKE_POLL = 60      # 轮询次数
SMOKE_INTERVAL = 0.5  # 秒

ROOT = Path(__file__).resolve().parents[2]
BUNDLE = ROOT / "src-tauri" / "target" / "release" / "bundle"


def platform_impl():
    """按当前平台选实现模块。其它平台明确报「不可用」，不静默失败（AGENTS §4）。"""
    if sys.platform == "win32":
        return importlib.import_module("assert_bundle_windows")
    if sys.platform.startswith("linux"):
        return importlib.import_module("assert_bundle_linux")
    raise SystemExit(f"assert_bundle 暂不支持 {sys.platform}（只有 Windows / Linux）")


def count_models(root: Path) -> int:
    return len(list(root.rglob("*.onnx")))


def check_size(path: Path) -> None:
    """产物体积下限：漏打模型时安装包会缩到 ~10MB，肉眼很难发现。"""
    mb = path.stat().st_size / (1024 * 1024)
    if mb < MIN_MB:
        raise SystemExit(
            f"产物过小：{path.name} 仅 {mb:.1f} MB（下限 {MIN_MB}MB，疑似模型未随包）"
        )
    print(f"产物 {path.name}  {mb:.1f} MB")


def check_layout(exe: Path, root: Path, expected: int, candidates: list[str]) -> None:
    """断言可执行文件存在 + 模型齐全。模型落位路径各家不同，交给调用方列候选。"""
    if not exe.exists():
        raise SystemExit(f"安装后缺少可执行文件 {exe}（落位检查：{'；'.join(candidates)}）")
    found = count_models(root)
    if found < expected:
        raise SystemExit(
            f"安装包模型不全：{found}/{expected}（落位检查：{'；'.join(candidates)}）"
        )
    print(f"布局 OK：{exe.name} + 模型 {found}/{expected}")


def debug_ready() -> bool:
    try:
        with urllib.request.urlopen(DEBUG_URL, timeout=2) as r:
            return r.status == 200
    except (urllib.error.URLError, OSError):
        return False


def kill_tree(proc: subprocess.Popen) -> None:
    """杀干净：先杀自己拉起的进程组，再兜底扫一遍同名进程。

    **必须不留残留** —— CI 上残留的进程会占住 /debug 端口，下一轮冒烟连的是上上轮
    的进程，测出来的「就绪」是假的。
    """
    if proc.poll() is None:
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
        except (ProcessLookupError, PermissionError, AttributeError):
            proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
            except (ProcessLookupError, PermissionError, AttributeError):
                proc.kill()
    # 兜底：按可执行文件名扫一遍（Tauri 会另起 webview 子进程）
    name = "purevox.exe" if sys.platform == "win32" else "purevox"
    subprocess.run(["pkill", "-9", "-f", name], capture_output=True)
    time.sleep(0.5)


def smoke(exe: Path, prefix: list[str], log: Path) -> None:
    """启动安装产物 → 轮询 /debug 就绪 → **必杀**。

    只判 `/debug` 起来了，不做功能验证 —— 那是 Rust 单测的事，这里只证明「装出来的
    这个二进制能启动、调试端口能开」。
    """
    cmd = prefix + [str(exe)]
    log.parent.mkdir(parents=True, exist_ok=True)
    with log.open("wb") as fh:
        proc = subprocess.Popen(cmd, stdout=fh, stderr=subprocess.STDOUT, start_new_session=True)
    try:
        for _ in range(SMOKE_POLL):
            if debug_ready():
                print("冒烟 OK：/debug 就绪")
                return
            time.sleep(SMOKE_INTERVAL)
        raise SystemExit(f"冒烟失败：启动后 /debug {SMOKE_POLL * SMOKE_INTERVAL:.0f}s 内未就绪（日志 {log}）")
    finally:
        kill_tree(proc)


def main() -> int:
    ap = argparse.ArgumentParser(description="发布产物断言")
    ap.add_argument("--smoke", action="store_true", help="额外做启动冒烟（装完跑一次 /debug）")
    args = ap.parse_args()

    impl = platform_impl()
    expected = count_models(ROOT / "models")
    tmp = Path(tempfile.mkdtemp(prefix="purevox_bundle_check_"))
    try:
        for artifact in impl.find_artifacts(BUNDLE):
            check_size(artifact)
        exe, root, candidates = impl.install(BUNDLE, tmp)
        check_layout(exe, root, expected, candidates)
        impl.check_extra_artifacts(BUNDLE)
        if args.smoke:
            smoke(exe, impl.smoke_prefix(), tmp / "smoke.log")
    finally:
        # 不管成没成，临时目录都清掉（AGENTS：测试自收尾）
        shutil.rmtree(tmp, ignore_errors=True)
    print("产物断言全部通过")
    return 0


if __name__ == "__main__":
    sys.exit(main())