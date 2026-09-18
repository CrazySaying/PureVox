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

"""48 kHz <-> 16 kHz 流式多相 FIR 重采样（整数比 3:1 / 1:3）。

**为什么不能复用 `dsp.resampler.Resampler`**：那是三次 Hermite 插值，
docstring 自己写明质量只够「AEC far-end 参考与 TSE 参考音频」；它在 3:1
抽取时**没有任何抗混叠滤波**，8–24 kHz 会整体折叠进 0–8 kHz，直接毁掉输出。
本模块用窗函数 sinc 低通 + 多相分解：抽取前把 8 kHz 以上压掉，插值后把
镜像压掉。

**10ms 网格**：480 样本(48k) <-> 160 样本(16k)，严格 3:1 整数比，与
`context.HOP_LENGTH` 同网格，不引入错位块；有状态但**不漂移**（每块恒定
480 进 / 160 出）。

**群延迟**：抽取段 (N-1)/2 = 47.5 个 48k 样本，插值段 3(T-1)/2 = 46.5 个，
合计约 1.96 ms，恒定可补偿；`latency_48k` 给出该值。
"""

import numpy as np

# ── 原型低通设计参数 ──
# 截止取 0.325（相对 48kHz 奈奎斯特，即 7.8 kHz）而非理论 1/3（8 kHz）：
# 给 Kaiser 过渡带留余量，确保 8 kHz 以上（8–24 kHz 折叠源）真被压住；
# 代价是 7.8–8 kHz 边缘轻微滚降——Hush 带宽上限本就是 8 kHz，可接受。
_PHASES = 3
_TAPS_PER_PHASE = 32
_N_TAPS = _PHASES * _TAPS_PER_PHASE      # 96
_CUTOFF = 0.325
_KAISER_BETA = 9.0
_DEC_KEEP = _N_TAPS - 1                  # 抽取段跨块历史长度
_INT_KEEP = _TAPS_PER_PHASE - 1          # 插值段跨块历史长度


def _design_prototype() -> np.ndarray:
    """窗函数 sinc 低通原型（直流增益归一为 1）。

    用公式直算而非 scipy.signal.firwin：系数跨 scipy 版本逐位稳定，
    也不引入额外依赖（numpy 已在依赖内）。
    """
    mid = (_N_TAPS - 1) / 2.0
    n = np.arange(_N_TAPS, dtype=np.float64)
    h = _CUTOFF * np.sinc(_CUTOFF * (n - mid))
    h *= np.kaiser(_N_TAPS, _KAISER_BETA)
    h /= h.sum()
    return h.astype(np.float32)


_PROTOTYPE = _design_prototype()
_DEC_REV = _PROTOTYPE[::-1].copy()                   # 抽取用的点积系数
_PHASE_FIR = [_PROTOTYPE[p::_PHASES].copy() for p in range(_PHASES)]  # h[3j+p]
# 抽取输出位置的逐相索引矩阵（n_out, N），复用以避免每帧重建
_DEC_IDX_HOP = 480                                   # 与 context.HOP_LENGTH 一致
_DEC_IDX_CACHE: dict[int, np.ndarray] = {}


def _dec_index(n_out: int) -> np.ndarray:
    """(n_out, _N_TAPS) 的取样索引：第 m 行是 buf[3m .. 3m+N-1]。"""
    idx = _DEC_IDX_CACHE.get(n_out)
    if idx is None:
        idx = _PHASES * np.arange(n_out, dtype=np.int64)[:, None] \
            + np.arange(_N_TAPS, dtype=np.int64)[None, :]
        _DEC_IDX_CACHE[n_out] = idx
    return idx


class PolyphaseResampler3:
    """48 kHz <-> 16 kHz 流式重采样器；两个方向各自独立状态、互不干扰。

    用法::

        rs = PolyphaseResampler3()
        y16 = rs.decimate(frame480)     # 480 -> 160
        y48 = rs.interpolate(y16)       # 160 -> 480
    """

    def __init__(self) -> None:
        self.reset()

    def reset(self) -> None:
        """清空两个方向的历史（换流 / 重开会话时调用）。"""
        self._dec_hist = np.zeros(_DEC_KEEP, dtype=np.float32)
        self._int_hist = np.zeros(_INT_KEEP, dtype=np.float32)

    @property
    def latency_48k(self) -> float:
        """抽取 + 插值的合计群延迟（单位：48k 样本）。"""
        return _DEC_KEEP / 2.0 + _PHASES * _INT_KEEP / 2.0

    # ── 48k -> 16k ──
    def decimate(self, x) -> np.ndarray:
        """n 个 48k 样本 -> n/3 个 16k 样本（n 须为 3 的倍数）。

        滤波：y[m] = Σ_k h[k]·x[3m-k]，即「低通后每 3 点取 1」，
        8 kHz 以上先被压掉，不会折叠回带内。
        """
        x = np.asarray(x, dtype=np.float32).reshape(-1)
        n_in = len(x)
        if n_in % _PHASES != 0:
            raise ValueError(f"decimate: 输入长度须为 {_PHASES} 的倍数，得到 {n_in}")
        if n_in == 0:
            return np.zeros(0, dtype=np.float32)
        n_out = n_in // _PHASES

        # buf[t] = 流上第 t-(N-1) 个样本（前补历史）
        buf = np.concatenate([self._dec_hist, x])
        out = (_buf_slice_view(buf, _dec_index(n_out)) @ _DEC_REV).astype(np.float32)
        self._dec_hist = buf[-_DEC_KEEP:].copy()
        return out

    # ── 16k -> 48k ──
    def interpolate(self, x) -> np.ndarray:
        """n 个 16k 样本 -> 3n 个 48k 样本。

        滤波：y[3m+p] = 3·Σ_j h[3j+p]·x[m-j]，即「插零后低通」的多相等价
        形式；×3 补回插零带来的能量损失，直流增益为 1。
        """
        x = np.asarray(x, dtype=np.float32).reshape(-1)
        n_in = len(x)
        if n_in == 0:
            return np.zeros(0, dtype=np.float32)

        buf = np.concatenate([self._int_hist, x])
        out = np.zeros(n_in * _PHASES, dtype=np.float32)
        h = _TAPS_PER_PHASE - 1
        for p in range(_PHASES):
            conv = np.convolve(buf, _PHASE_FIR[p])
            # conv 的 (m+h) 号项 = Σ_j h[3j+p]·buf[m+h-j] = Σ_j h[3j+p]·x[m-j]
            out[p::_PHASES] = _PHASES * conv[h:h + n_in]
        self._int_hist = buf[-_INT_KEEP:].copy()
        return out


def _buf_slice_view(buf: np.ndarray, idx: np.ndarray):
    """按索引矩阵取 buf 的窗口（避免 fancy-index 复制之外的额外开销）。"""
    return buf[idx]
