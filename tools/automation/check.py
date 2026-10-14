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
"""代码质量门禁的**唯一实现**：CI 各 job 与本机开发跑同一份脚本。

  python tools/automation/check.py                  # 全部门禁
  python tools/automation/check.py -Gate fmt        # 单项（CI 按 step 拆开跑，日志好定位）

门禁项：fmt（rustfmt 校验）/ clippy（-D warnings 零容忍）/ build / test /
ui（tsc + i18n_lint）/ version（版本号推导）。

为什么是 Python：门禁要在 Windows / Linux / macOS 上跑，而 PowerShell 只有前两者
装得像样、还得单独装（CI runner 自带，本机要自己搞）。Python 三边都是系统自带，
冷启动 ~20 ms（PowerShell ~330 ms），只用标准库、不需要 pip install。

只依赖标准库。改了门禁项要同步 `.github/workflows/tests.yml`（CI 按 step 拆开跑）。
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

# 必须在任何 print 之前把 stdout 改成 UTF-8（Windows CI runner 的默认是 cp1252，
# 打中文会 UnicodeEncodeError —— 门禁跑完了却被编码错误判成失败）。
# 同目录导入：脚本以 `python tools/automation/check.py` 调用时，脚本所在目录在 sys.path 上。
sys.path.insert(0, str(Path(__file__).resolve().parent))
import _utf8  # noqa: E402  （刻意放在 path 注入之后）

_utf8.force()

ROOT = Path(__file__).resolve().parents[2]
TAURI = ROOT / "src-tauri"
UI = TAURI / "ui"

GATES = ("fmt", "clippy", "build", "test", "ui", "version")

# 版本号推导的固定样本。09 月 / 09 日 / 凌晨时分是**前导零陷阱**：bash 的 `$((10#09))`
# 不加 10# 会当八进制，`09` 直接语法错误 —— 现在只有 Python 一份实现，但样本仍留着，
# 哪天有人「优化」成字符串拼接就会当场露馅。
VERSION_CASES = (
    # tag,                    VERSION, STAMP,            ACODE,      PUREVOX_BUILD_STAMP
    ("v2026.10.14.1530", "26.10.14", "2026-10-14-1530", "2610141530", "2026.10.14.1530"),
    ("v2026.09.09.0901", "26.9.9",   "2026-09-09-0901", "2609090901", "2026.09.09.0901"),
    ("v2026.01.02.0000", "26.1.2",   "2026-01-02-0000", "2601020000", "2026.01.02.0000"),
    # 同一天第二次发版：显示版本相同，靠 tag 的时分区分（AGENTS §2.6 / version.rs）
    ("v2026.10.14.2309", "26.10.14", "2026-10-14-2309", "2610142309", "2026.10.14.2309"),
)


class GateError(Exception):
    """门禁未通过。"""


class ToolMissing(GateError):
    """工具不在 PATH 上（与「跑了但没过」分开报，别混成一句 command not found）。"""


def _which(name: str, hint: str) -> str:
    found = shutil.which(name)
    if not found:
        raise ToolMissing(f"找不到 {name}（{hint}）")
    return found


def _env() -> dict[str, str]:
    """把 cargo 所在目录加进 PATH。

    Linux/macOS 上 cargo 装在 ~/.cargo/bin，多数情况已在 PATH；但 GUI 启动的终端、
    某些 CI step 会漏。Windows 上 PATH 用 `;` 分隔、其他平台用 `:`，不能写死。
    """
    env = dict(os.environ)
    cargo_bin = str(Path.home() / ".cargo" / "bin")
    sep = ";" if os.name == "nt" else ":"
    if cargo_bin not in env.get("PATH", "").split(sep):
        env["PATH"] = cargo_bin + sep + env.get("PATH", "")
    return env


def run(cmd: list[str], cwd: Path, *, env: dict[str, str] | None = None) -> str:
    """跑一条命令，失败即抛；返回 stdout。"""
    print(f"  $ {' '.join(str(c) for c in cmd)}   (cwd={cwd.name})", flush=True)
    proc = subprocess.run(
        [str(c) for c in cmd], cwd=str(cwd), env=env or _env(),
        capture_output=True, text=True, encoding="utf-8", errors="replace",
    )
    out = (proc.stdout or "") + (proc.stderr or "")
    if proc.returncode != 0:
        print(out, flush=True)
        raise GateError(f"命令失败（exit={proc.returncode}）：{' '.join(str(c) for c in cmd)}")
    return out


# --------------------------------------------------------------------------
# 各道门禁
# --------------------------------------------------------------------------
def gate_fmt() -> None:
    run(["cargo", "fmt", "--check"], TAURI)


def gate_clippy() -> None:
    run(["cargo", "clippy", "--all-targets", "--", "-D", "warnings"], TAURI)


def gate_build() -> None:
    run(["cargo", "build"], TAURI)


def gate_test() -> None:
    out = run(["cargo", "test"], TAURI)
    for line in out.splitlines():
        if line.startswith("test result:"):
            print(f"  {line.strip()}")


def gate_ui() -> None:
    # Windows 上 tsc 常常只以 .cmd/.ps1 形式存在（npm 全局包装脚本），shutil.which 只认
    # 可执行扩展名。这里显式把三种形态都试一遍，找不到再报「工具缺失」而不是
    # FileNotFoundError 的裸栈。
    run([_which("tsc", "npm install -g typescript@<TS_VER>"), "-p", "jsconfig.json", "--noEmit"], UI)
    # i18n_lint 用 node 跑（它就是 node 生态的脚本，不必为了门禁改写成 Python）
    run([_which("node", "装 Node.js"), ROOT / "tools" / "automation" / "i18n_lint.js"], ROOT)


def gate_version() -> None:
    """版本号推导：喂固定 tag，断言输出**逐字**符合预期。

    版本推导错了的表现是包名 / 产物文件名里的日期不对 —— 要等用户装上、或去 release
    页面找安装包时才发现。所以在这钉死，不依赖联网。
    """
    script = ROOT / "tools" / "automation" / "version.py"
    cfg = TAURI / ".build-version.json"
    for tag, version, stamp, acode, bstamp in VERSION_CASES:
        env = _env()
        env["GITHUB_REF_NAME"] = tag
        out = run([sys.executable, script], ROOT, env=env)
        kv = dict(
            line.split("=", 1) for line in out.splitlines() if "=" in line.strip()
        )
        want = {
            "TAG": tag,
            "VERSION": version,
            "STAMP": stamp,
            "ACODE": acode,
            "PUREVOX_BUILD_VERSION": version,
            "PUREVOX_BUILD_STAMP": bstamp,
        }
        for k, v in want.items():
            got = kv.get(k)
            if got != v:
                raise GateError(f"tag {tag}：{k} = {got!r}，期望 {v!r}")
        # Tauri 的版本覆盖配置要跟着走（构建时 --config 传进去，忘了写就还是 0.1.0）
        got_cfg = json.loads(cfg.read_text(encoding="utf-8")).get("version")
        if got_cfg != version:
            raise GateError(
                f"tag {tag}：.build-version.json 的 version = {got_cfg!r}，期望 {version!r}"
            )
        print(f"  {tag} -> VERSION={version}  STAMP={stamp}  ACODE={acode}")


GATE_FUNCS = {
    "fmt": gate_fmt,
    "clippy": gate_clippy,
    "build": gate_build,
    "test": gate_test,
    "ui": gate_ui,
    "version": gate_version,
}


def main() -> int:
    ap = argparse.ArgumentParser(description="PureVox 代码质量门禁")
    ap.add_argument(
        "-Gate", default="all", choices=("all", *GATES),
        help="跑哪几道门禁（默认 all）",
    )
    args = ap.parse_args()
    names = list(GATES) if args.Gate == "all" else [args.Gate]

    failed: list[str] = []
    for name in names:
        print(f"==> {name}", flush=True)
        try:
            GATE_FUNCS[name]()
        except GateError as e:
            print(f"!! 门禁 {name} 未通过：{e}", file=sys.stderr, flush=True)
            failed.append(name)

    if failed:
        print(f"\n未通过：{', '.join(failed)}", file=sys.stderr)
        return 1
    print(f"\n门禁全部通过（{args.Gate}）")
    return 0


if __name__ == "__main__":
    sys.exit(main())