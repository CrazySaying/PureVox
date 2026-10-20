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

"""Hush 参考实现的黄金输出生成器（Rust 移植对拍用）。

产出三样东西，供 `src-tauri/src/infer/hush_parity.rs` 的 `reference_parity` 测试消费：

    hush_in.f32     48 kHz 单声道 F32 裸流（输入）
    hush_ref.f32    同一输入的参考实现输出
    hush_ref.txt    每帧 lsnr（逗号分隔）

输入信号刻意覆盖三类区段，以便一次跑出 Hush 的三条分支：

    ① 纯噪声（白噪 + 低频嗡声）  → lsnr 低，走 MODE_ZEROS 或处理后大幅衰减
    ② 「语音状」谐波 + 包络起伏  → lsnr 居中，走 MODE_PROC（掩码 + 深滤波）
    ③ 静音                      → 均方 < 1e-7，走静音短路（输出全零、状态不推进）

**生成方式**（一次性；产物已随仓库入库，通常不需要重跑）：

参考实现（`HushEngine`）只存在于 Tauri 迁移前的 Python 树里，即本仓库的
`legacy-v2026.09.30.1944/` 快照。把本脚本放到那份快照的 `tests/` 下、
装好 numpy + onnxruntime，再执行：

    python tests/gen_hush_reference.py <输出目录>

然后把三件产物拷回本目录（`tests-data/`）。只有在「参考实现本身改动」
（换模型、改 DSP）时才需要重跑并对拍 —— 那时也应同步更新参考实现。

注意：本脚本依赖 numpy 与 onnxruntime，只用于开发期生成黄金数据，不参与构建。
"""

import os
import sys

import numpy as np

SR = 48000
HOP = 480


def build_input(seconds: float = 3.0) -> np.ndarray:
    rng = np.random.default_rng(20260918)
    n = int(SR * seconds)
    t = np.arange(n, dtype=np.float64) / SR

    # 区段划分（各 1 s）
    seg = np.zeros(n, dtype=np.float64)
    third = n // 3

    # ① 纯噪声：白噪 + 50 Hz 嗡声
    noise = rng.normal(0.0, 0.02, third)
    noise += 0.01 * np.sin(2 * np.pi * 50.0 * t[:third])
    seg[:third] = noise

    # ② 语音状：基频 140 Hz + 若干谐波，带音节包络（4 Hz 起伏）
    tt = t[third:2 * third]
    env = 0.5 * (1.0 - np.cos(2 * np.pi * 4.0 * (tt - tt[0])))
    voice = np.zeros_like(tt)
    for k, amp in enumerate((1.0, 0.5, 0.3, 0.2, 0.12), start=1):
        voice += amp * np.sin(2 * np.pi * 140.0 * k * tt)
    voice *= env * 0.25
    voice += rng.normal(0.0, 0.005, len(tt))     # 叠一点底噪
    seg[third:2 * third] = voice

    # ③ 静音
    seg[2 * third:] = 0.0
    return seg.astype(np.float32)


def main() -> int:
    out_dir = sys.argv[1] if len(sys.argv) > 1 else "."
    os.makedirs(out_dir, exist_ok=True)

    sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    from pvengine.components.hush import HushEngine

    x = build_input()
    model_dir = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                             "models", "hush")
    eng = HushEngine(model_dir)

    n_hop = len(x) // HOP
    out = np.zeros(n_hop * HOP, dtype=np.float32)
    lsnrs = []
    for i in range(n_hop):
        frame = x[i * HOP:(i + 1) * HOP]
        y = eng.process_48k(frame)
        out[i * HOP:(i + 1) * HOP] = y
        lsnrs.append(eng.last_lsnr)

    x[: n_hop * HOP].tofile(os.path.join(out_dir, "hush_in.f32"))
    out.tofile(os.path.join(out_dir, "hush_ref.f32"))
    with open(os.path.join(out_dir, "hush_ref.txt"), "w", encoding="utf-8") as f:
        f.write(",".join(f"{v:.6f}" for v in lsnrs))

    print(f"输入 {n_hop * HOP} 样本 / {n_hop} hop")
    print(f"输出峰值 {np.abs(out).max():.6f}，RMS {np.sqrt(np.mean(out ** 2)):.6f}")
    print(f"lsnr 均值 {np.mean(lsnrs):.2f} dB（{min(lsnrs):.2f} .. {max(lsnrs):.2f}）")
    print(f"已写出 {out_dir}/hush_in.f32, hush_ref.f32, hush_ref.txt")
    return 0


if __name__ == "__main__":
    sys.exit(main())
