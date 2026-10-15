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
ui（tsc + i18n_lint）/ version（版本号推导）/ bundle（deb 依赖清单非空 + rpm 必须靠自动生成）。

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

GATES = ("fmt", "clippy", "build", "test", "ui", "version", "bundle")

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

    **跑完必须把 `.build-version.json` 删掉**：version.py 每跑一次就重写它，而这道门禁
    喂的是 4 个**假** tag。留着的话本机会永远带着一个测试日期（本机实测踩到过：文件里
    停在 `26.10.14`），随后 `cargo tauri build --config .build-version.json` 会静默打出一
    个日期错的包 —— 比不打版本号更难查。删掉是无损的：真实构建流程总是先跑一次
    `version.py`（release.yml 的 `Set app version` 就在 `cargo tauri build` 之前），
    谁构建谁重新生成。
    """
    script = ROOT / "tools" / "automation" / "version.py"
    cfg = TAURI / ".build-version.json"
    try:
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
    finally:
        if cfg.unlink(missing_ok=True) is not None:
            print("  已删掉门禁留下的 .build-version.json（里面是假 tag 的日期，会污染下一次构建）")


def gate_bundle() -> None:
    """打包配置自检：**deb 的依赖清单不能空，且 rpm 必须交给 rpmbuild 自动生成**。

    两种包格式的机制不同，门禁也就不同 —— 把它们当同一件事正是这次 `soup3` 那个错的由来：

    * **deb**：dpkg **没有**自动依赖机制，`Depends:` 完全来自 `tauri.conf.json` 的
      `bundle.linux.deb.depends`，**没有内置默认值**（`Option<Vec<String>>`
      → `unwrap_or_default()` → 空）。所以忘配的后果不是「构建报错」，而是
      **发出去一个 `apt install` 得上、但一启动就 `error while loading shared libraries`
      的包** —— 用户侧才发现，CI 全绿。
    * **rpm**：`rpmbuild` 从 ELF 的 `DT_NEEDED` **自动生成** `Requires`，生成的名字经
      filelist 反查必然正确。手写反而是错的来源（曾把 Fedora 的 `libsoup3` 写成 `soup3`，
      安装直接 `nothing provides soup3`；而它在第一个错名就停下，后面的错还没暴露）。
      所以这里反过来断言 **depends 必须为空**，防止有人手滑再加回去。

    这道门禁只挡「忘配」；依赖清单**对不对**由 `test_packages.py` 在干净的
    ubuntu:24.04 / ubuntu:26.04 / fedora 容器里用 `ld.so --list` 实测
    （它会报缺库与缺符号版本 —— 别用 `ldd`，它只看库文件在不在）。
    """
    cfg = json.loads((TAURI / "tauri.conf.json").read_text(encoding="utf-8"))
    linux = cfg.get("bundle", {}).get("linux", {})

    need = {
        "libwebkit2gtk-4.1-0": "WebKitGTK 4.1（wry 运行时，缺了整个应用起不来）",
        "libgtk-3-0": "GTK3",
        "libasound2": "ALSA（cpal 采集/播放）",
    }
    declared = (linux.get("deb") or {}).get("depends") or []
    if not declared:
        raise GateError(
            f"bundle.linux.deb.depends 是空的 —— 生成的包里不会有依赖声明，"
            f"用户装得上却起不来。至少要写：{'、'.join(need)}"
        )
    missing = [f"{p}（{why}）" for p, why in need.items() if p not in declared]
    if missing:
        raise GateError(
            f"bundle.linux.deb.depends 缺：{'；'.join(missing)}"
            f"（当前 {len(declared)} 条：{'、'.join(declared)}）"
        )
    print(f"  deb: {len(declared)} 条依赖，核心项齐")

    rpm_deps = (linux.get("rpm") or {}).get("depends") or []
    if rpm_deps:
        raise GateError(
            f"bundle.linux.rpm.depends 不该手写（现有 {'、'.join(rpm_deps)}）—— "
            f"rpmbuild 会从 DT_NEEDED 自动生成 Requires，手写的名字只在某一个 Fedora 上"
            f"成立（曾把 libsoup3 写成 soup3）。真要加不可自动发现的依赖，进 "
            f"bundle.linux.rpm.recommends 或 requires（那是给 dlopen 的东西用的）。"
        )
    print("  rpm: 交给 rpmbuild 自动生成（depends 为空 = 正确）")

    # 发版那一步的真实实现也归这道门禁管：它此前**从没成功执行过**，第一次真打 tag
    # 才暴露出「收集完文件名后 `cd ..` 回上一层 → 相对路径全失效 → gh 报
    # no matches found」。用假产物树 + 假 gh 在本地跑通三个场景（齐全 / 缺产物 /
    # 缺 notes），保证「测的」与「发布的」是同一份 `release_stage.sh`。
    harness = ROOT / "tools" / "automation" / "run_release_stage.py"
    proc = subprocess.run(
        [sys.executable, str(harness)], cwd=str(ROOT),
        capture_output=True, text=True, encoding="utf-8", errors="replace",
    )
    if proc.returncode != 0:
        detail = ((proc.stdout or "") + (proc.stderr or "")).rstrip()
        raise GateError(
            "发版脚本台子没过（release_stage.sh 的 stage/改名/gh 调用）："
            + chr(10) + detail
        )
    for line in (proc.stdout or "").splitlines():
        if "个场景通过" in line:
            print(f"  发版脚本: {line.strip()}")

    # opus 是运行时 dlopen（不是 DT_NEEDED），自动依赖看不见，漏在 Recommends 里
    # 只会静默降级网络音频（其余功能正常，所以只提示不拦）
    for kind, key in (("deb", "libopus0"), ("rpm", "opus")):
        rec = (linux.get(kind) or {}).get("recommends") or []
        if key not in rec:
            print(f"  提示：{kind} 的 recommends 里没有 {key} —— 缺它时网络音频降级，"
                  f"其余功能正常（不是致命项）")


GATE_FUNCS = {
    "fmt": gate_fmt,
    "clippy": gate_clippy,
    "build": gate_build,
    "test": gate_test,
    "ui": gate_ui,
    "version": gate_version,
    "bundle": gate_bundle,
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