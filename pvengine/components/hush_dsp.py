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
# ── 第三方来源声明 ──
# 本文件是 DeepFilterNet（github.com/Rikorose/DeepFilterNet，作者 Hendrik
# Schröter 等，MIT / Apache-2.0 双许可）中 libdf 前后端 DSP 的 numpy 移植，
# 算法语义逐条对照其 `df/src/lib.rs` 与 `df/src/transforms.rs` 实现，
# 以复现 Hush（pulp-vision/Hush，Apache-2.0）官方发布包
# `weya_nc` 的流式行为。移植部分仍按 GPL-3.0-or-later 分发。

"""Hush 的 libdf 前后端 DSP（纯 numpy，16kHz 专用）。

为什么需要它
------------
Hush 发布的 ONNX 只有 `enc_conv` / `enc_gru` / `erb_dec` / `df_dec` 四个神经网络
图，**STFT、ERB 滤波器组、归一化、掩码展开、深滤波、ISTFT 全部在 host 侧**，
官方是打包进预编译 Rust 动态库（`weya_nc`，内部用 libdf + tract）提供的。PureVox
是纯 Python 引擎、不带自编译二进制，所以这一段必须自己实现——本文件就是它。

契约（来自 `config.ini`）
------------------------
    sr = 16000   hop = 160 (10ms)   fft = 320   nb_erb = 32   nb_df = 64
    df_order = 5   norm_tau = 1.0   min_nb_erb_freqs = 2

hop × 2 == fft，所以重叠结构退化到最简：分析缓存恰为上一帧 160 样本，
合成缓存恰为 IFFT 后半帧。Vorbis 窗为功率互补，分析侧乘 `wnorm = 1/fft`，
合成侧用未归一化 IFFT（numpy 的 irfft 自带 1/n，故乘回 n），往返增益为 1。

每帧数据流
----------
    front(frame160) -> (feat_erb[1,1,1,32], feat_spec[1,2,1,64])
        · 分析：rfft([上帧·w[:160], 本帧·w[160:]]) · (1/320)
        · ERB 带能量（带内均值）→ 10log10(v+1e-10) → 减指数均值 → /40
        · 前 64 bin 复数单位归一 → re/im 两通道
        同时把本帧「含噪谱」推入 5 帧环（深滤波需要）
    back(gains32, coefs, mode) -> frame160
        · 掩码经 0/1 的 erb_inv_fb 展开成逐 bin 增益
        · 深滤波：前 64 bin 用最近 5 帧含噪谱做 5 阶复 FIR；64..160 用掩码结果
        · 合成：未归一化 irfft → 乘窗 → 与合成缓存重叠相加
"""

import numpy as np

# ── 模型契约常量（与 models/hush/config.ini 一致）──
SR = 16000
HOP = 160
FFT = 320
NB_ERB = 32
NB_DF = 64
DF_ORDER = 5
NORM_TAU = 1.0
MIN_NB_ERB_FREQS = 2

N_FREQS = FFT // 2 + 1            # 161

# ── 幅度门限（对齐 weya_nc 的 RuntimeParams）──
MIN_DB_THRESH = -15.0             # lsnr 低于此值：整帧零掩码
MAX_DB_ERB_THRESH = 35.0          # lsnr 高于此值：完全不处理（直通）
MAX_DB_DF_THRESH = 35.0           # 与上者相等 ⇒ 中间档在 weya_nc 下不可达
SILENCE_MEAN_SQ = 1e-7            # 帧均方低于此值：直接静音输出

# ── 指数归一初值（libdf MEAN_NORM_INIT / UNIT_NORM_INIT）──
_MEAN_NORM_INIT = (-60.0, -90.0)
_UNIT_NORM_INIT = (0.001, 0.0001)

# 三种后端模式
MODE_PROC = "proc"        # 掩码 + 深滤波（正常降噪）
MODE_ZEROS = "zeros"      # 零掩码（判为纯噪声）
MODE_BYPASS = "bypass"    # 完全不处理


def norm_alpha(sr: int = SR, hop: int = HOP, tau: float = NORM_TAU) -> float:
    """指数归一系数，对齐 DeepFilterNet `get_norm_alpha`。"""
    return float(np.exp(-hop / float(sr) / tau))


def vorbis_window(fft_size: int = FFT) -> np.ndarray:
    """libdf 的 Vorbis 窗：`sin(pi/2 * sin^2(pi*(i+0.5)/(N/2)))`。

    **不是** numpy 的 hann：DF3 的 PyTorch 侧 `DfNetSE.stft` 用的是 hann，
    但那是训练/整文件推理路径；部署路径（libdf）用 Vorbis。要与 `weya_nc`
    对齐就必须用 Vorbis。
     """
    half = fft_size // 2
    i = np.arange(fft_size, dtype=np.float64)
    s = np.sin(0.5 * np.pi * (i + 0.5) / half)
    return np.sin(0.5 * np.pi * s * s).astype(np.float32)


def _freq2erb(f: np.float32) -> np.float32:
    return np.float32(9.265) * np.log1p(f / (np.float32(24.7) * np.float32(9.265)))


def _erb2freq(e: np.float32) -> np.float32:
    return (np.float32(24.7) * np.float32(9.265)) * \
        (np.exp(e / np.float32(9.265)) - np.float32(1.0))


def erb_widths(sr: int = SR, fft_size: int = FFT, nb_bands: int = NB_ERB,
               min_nb_freqs: int = MIN_NB_ERB_FREQS) -> np.ndarray:
    """矩形 ERB 滤波器组的各带宽度（逐行移植 libdf `erb_fb`）。

    注意用 **libdf 的 9.265 公式**，而不是 Hush `dfnet_se.py` 里那个
    Slaney 兜底公式（`21.4*log10(1+f/229)`）——后者只在拿不到 libdf 时使用，
    模型权重是按 libdf 宽度训练的，选错会让前端特征整体偏掉。
    全程 f32 运算以对齐 Rust 的舍入。
    """
    nyq_freq = sr // 2
    freq_width = np.float32(sr) / np.float32(fft_size)
    erb_low = _freq2erb(np.float32(0.0))
    erb_high = _freq2erb(np.float32(nyq_freq))
    step = (erb_high - erb_low) / np.float32(nb_bands)

    widths = [0] * nb_bands
    prev_freq = 0
    freq_over = 0
    for i in range(1, nb_bands + 1):
        f = _erb2freq(erb_low + np.float32(i) * step)
        # Rust 的 f32::round() 是「四舍五入、远离零」；此处 f/freq_width >= 0
        fb = int(np.floor(np.float32(f / freq_width) + np.float32(0.5)))
        n = fb - prev_freq - freq_over
        if n < min_nb_freqs:
            freq_over = min_nb_freqs - n
            n = min_nb_freqs
        else:
            freq_over = 0
        widths[i - 1] = n
        prev_freq = fb

    widths[nb_bands - 1] += 1        # 频点数比整数带宽多 1（含奈奎斯特）
    too_large = sum(widths) - (fft_size // 2 + 1)
    if too_large > 0:
        widths[nb_bands - 1] -= too_large
    return np.asarray(widths, dtype=np.int64)


_ERB_WIDTHS = erb_widths()
# 逐 bin 的所属 ERB 带号（长度 161）—— 等价 libdf 的 apply_interp_band_gain
_ERB_BAND_OF_BIN = np.repeat(np.arange(NB_ERB, dtype=np.int64), _ERB_WIDTHS)
_VORBIS = vorbis_window()


class HushDsp:
    """Hush 的前后端 DSP 状态机（16kHz、10ms hop）。

    与模型的配合顺序（每帧）::

        feat_erb, feat_spec = dsp.front(frame)
        ... 四个 ONNX 图 ...
        mode = hush_gate(lsnr)
        out = dsp.back(gains, coefs, mode)

    `front` 会把本帧含噪谱推入 5 帧环，`back` 的深滤波依赖它，故二者必须配对调用。
    """

    def __init__(self, alpha: float | None = None,
                 atten_lim_db: float = 100.0) -> None:
        self.alpha = np.float32(norm_alpha() if alpha is None else alpha)
        # atten_lim_db >= 100 视为「不限衰减」（对齐 weya_nc）
        lim = abs(atten_lim_db)
        self.atten_lim = None if lim >= 100.0 else \
            (None if lim < 0.01 else float(10.0 ** (-lim / 20.0)))
        self.reset()

    def reset(self) -> None:
        """清空全部流式状态（换流 / 重开会话时调用）。"""
        self._prev_frame = np.zeros(HOP, dtype=np.float64)
        self._syn_mem = np.zeros(HOP, dtype=np.float64)
        # 指数归一状态：均值态按 linspace(-60,-90,32)，单位态按 linspace(1e-3,1e-4,64)
        self._mean_state = np.linspace(_MEAN_NORM_INIT[0], _MEAN_NORM_INIT[1],
                                       NB_ERB).astype(np.float64)
        self._unit_state = np.linspace(_UNIT_NORM_INIT[0], _UNIT_NORM_INIT[1],
                                       NB_DF).astype(np.float64)
        # 最近 DF_ORDER 帧的含噪谱（深滤波用），最旧在前、当前在后
        self._spec_ring: list[np.ndarray] = []
        self._spec_noisy = np.zeros(N_FREQS, dtype=np.complex128)

    # ── 前端 ──
    def front(self, frame) -> tuple[np.ndarray, np.ndarray]:
        """喂一帧 160 样本，返回 (feat_erb[1,1,1,32], feat_spec[1,2,1,64])。"""
        x = np.asarray(frame, dtype=np.float64).reshape(-1)
        if len(x) != HOP:
            raise ValueError(f"hush dsp: 帧长须为 {HOP}，得到 {len(x)}")

        # 分析：上半窗乘上一帧、下半窗乘本帧
        buf = np.empty(FFT, dtype=np.float64)
        buf[:HOP] = self._prev_frame * _VORBIS[:HOP]
        buf[HOP:] = x * _VORBIS[HOP:]
        spec = np.fft.rfft(buf) * (1.0 / FFT)
        self._prev_frame = x
        self._spec_noisy = spec

        # 深滤波环：保留最近 DF_ORDER 帧（含本帧）
        self._spec_ring.append(spec)
        if len(self._spec_ring) > DF_ORDER:
            del self._spec_ring[0]

        # ① ERB 特征：带内平均功率 → dB → 减指数均值 → /40
        power = np.abs(spec) ** 2
        band = np.empty(NB_ERB, dtype=np.float64)
        start = 0
        for b, w in enumerate(_ERB_WIDTHS):
            band[b] = power[start:start + w].mean()
            start += int(w)
        band_db = 10.0 * np.log10(band + 1e-10)
        self._mean_state = band_db * (1.0 - self.alpha) + self._mean_state * self.alpha
        feat_erb = (band_db - self._mean_state) / 40.0

        # ② 复数特征：前 NB_DF 个 bin 单位归一
        xs = spec[:NB_DF].copy()
        self._unit_state = np.abs(xs) * (1.0 - self.alpha) + self._unit_state * self.alpha
        xs /= np.sqrt(self._unit_state)
        feat_spec = np.empty((1, 2, 1, NB_DF), dtype=np.float32)
        feat_spec[0, 0, 0, :] = xs.real
        feat_spec[0, 1, 0, :] = xs.imag
        return (feat_erb.reshape(1, 1, 1, NB_ERB).astype(np.float32), feat_spec)

    # ── 后端 ──
    def back(self, gains, coefs, mode: str = MODE_PROC) -> np.ndarray:
        """掩码 + 深滤波 + 合成，返回一帧 160 样本。

        mode: MODE_PROC 正常处理 / MODE_ZEROS 零掩码 / MODE_BYPASS 直通。
        gains: 32 个 ERB 增益；coefs: [DF_ORDER, NB_DF, 2]（仅 MODE_PROC 用）。
        """
        noisy = self._spec_noisy
        if mode == MODE_BYPASS:
            return self._synthesize(noisy)

        if mode == MODE_ZEROS:
            return self._synthesize(np.zeros(N_FREQS, dtype=np.complex128))

        g = np.asarray(gains, dtype=np.float64).reshape(-1)
        if len(g) != NB_ERB:
            raise ValueError(f"hush dsp: 需要 {NB_ERB} 个 ERB 增益，得到 {len(g)}")

        # 掩码：逐 bin 取所属 ERB 带的增益（libdf apply_interp_band_gain）
        masked = noisy * g[_ERB_BAND_OF_BIN]
        out = masked.copy()

        # 深滤波：前 NB_DF 个 bin 用最近 DF_ORDER 帧含噪谱做复数 FIR
        c = np.asarray(coefs, dtype=np.float64)
        if c.shape != (DF_ORDER, NB_DF, 2):
            raise ValueError(f"hush dsp: coefs 形状应为 {(DF_ORDER, NB_DF, 2)}，得到 {c.shape}")
        ring = self._spec_ring
        if len(ring) < DF_ORDER:
            # 流开头不足 5 帧：前面按 0 补（等价 spec_pad 的前置补零）
            pad = DF_ORDER - len(ring)
            frames = [np.zeros(N_FREQS, dtype=np.complex128)] * pad + list(ring)
        else:
            frames = ring
        acc = np.zeros(NB_DF, dtype=np.complex128)
        for o in range(DF_ORDER):
            acc += frames[o][:NB_DF] * (c[o, :, 0] + 1j * c[o, :, 1])
        out[:NB_DF] = acc

        # 衰减限（默认不限，故一般不触发）：干湿混合回一部分含噪信号
        if self.atten_lim is not None:
            lim = self.atten_lim
            out = out * (1.0 - lim) + noisy * lim
        return self._synthesize(out)

    def _synthesize(self, spec_out: np.ndarray) -> np.ndarray:
        """合成：未归一化 irfft → 乘窗 → 与合成缓存重叠相加。"""
        x = np.fft.irfft(spec_out, FFT) * FFT     # numpy 自带 1/N，乘回 N
        x *= _VORBIS
        out = x[:HOP] + self._syn_mem
        self._syn_mem = x[HOP:].copy()
        return out.astype(np.float32)


def hush_gate(lsnr: float) -> str:
    """按 lsnr 决定后端模式，逐行对齐 weya_nc 的 `apply_stages`。

    weya_nc 的 RuntimeParams 是 (-15, 35, 35, ReduceMask::MAX)，其中
    max_db_erb_thresh == max_db_df_thresh == 35，所以「只做掩码不做深滤波」
    那一档实际不可达；等价行为只有三档。`ReduceMask::MAX` 是沿通道轴取
    最大值，而 weya_nc 恒为单声道（n_ch=1），故是恒等操作，无需实现。
    """
    if lsnr < MIN_DB_THRESH:
        return MODE_ZEROS
    if lsnr > MAX_DB_ERB_THRESH:
        return MODE_BYPASS
    if lsnr > MAX_DB_DF_THRESH:
        return MODE_PROC          # 不可达（两门限相等），保留以对齐语义
    return MODE_PROC


def is_silent(frame) -> bool:
    """帧均方低于门限（对齐 weya_nc 的静音短路：输出全零、lsnr 记 -15）。"""
    x = np.asarray(frame, dtype=np.float64)
    return float(np.mean(x * x)) < SILENCE_MEAN_SQ
