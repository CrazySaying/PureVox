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
# 本文件编排 Hush（github.com/pulp-vision/Hush，Apache-2.0）发布的 ONNX 图，
# 其网络结构与流式状态语义来自 DeepFilterNet（github.com/Rikorose/DeepFilterNet，
# 作者 Hendrik Schröter 等，MIT / Apache-2.0 双许可）。移植部分按 GPL-3.0-or-later 分发。

"""Hush 降噪引擎——四个 ONNX 图 + 自研 DSP 的流式编排（16 kHz 原生）。

四个图与状态
------------
    enc_conv  : 卷积编码器，**必须吃 3 帧窗口取末帧**（带 kernel=(3,3) 时域卷积，
                窗口边界处缺 2 帧上文会污染下游 GRU 状态，且污染永不归零）
    enc_gru   : GRU 头，逐帧 + 1 个状态
    erb_dec   : 逐帧 + 1 个状态
    df_dec    : 逐帧 + **3 个状态**

为什么 enc 要拆：见 `tools/hush_stateful_onnx.py` 文件头。`erb_dec` / `df_dec`
的卷积全是 (1,k)，无时域延伸，实测逐帧复现误差 1.8e-7 / 1.9e-7，可直接逐帧。

门控与短路（对齐 weya_nc 的 `apply_stages` / `process`）
    · 帧均方 < 1e-7 → 输出全零、返回 lsnr = -15，**且 DSP 与全部状态都不推进**
    · lsnr < -15 → 零掩码，**不跑** erb_dec / df_dec（状态也不推进）
    · lsnr > 35  → 完全不处理，同样不跑解码器
    · 其余       → 掩码 + 深滤波
"""

import os

import numpy as np

from pvengine.components import hush_dsp as _dsp
from pvengine.dsp.core import make_session
from pvengine.dsp.resample3 import PolyphaseResampler3
from pvengine.stages.base import Stage

# 48 kHz <-> 16 kHz（纯 Python 引擎固定 48k，Hush 原生 16k）
HOP_48 = 480
HOP_16 = _dsp.HOP
SR_48 = 48000

# enc_conv 的窗口长度：编码器时域感受野实测为 3 帧（见 tools/hush_stateful_onnx.py）
ENC_WIN = 3

_FILES = ("enc_conv.onnx", "enc_gru.onnx", "erb_dec.onnx", "df_dec.onnx")


class HushEngine:
    """Hush 的流式引擎：喂 48kHz 的 480 样本帧，出 48kHz 的 480 样本帧。

    也可以直接喂 16 kHz 的 160 样本帧（`process_16k`），对拍时用得上。
    """

    def __init__(self, model_dir: str, atten_lim_db: float = 100.0) -> None:
        paths = {}
        for f in _FILES:
            p = os.path.join(model_dir, f)
            if not os.path.isfile(p):
                raise FileNotFoundError(f"Hush 模型缺失: {p}")
            paths[f] = p
        self.enc_conv = make_session(paths["enc_conv.onnx"])
        self.enc_gru = make_session(paths["enc_gru.onnx"])
        self.erb_dec = make_session(paths["erb_dec.onnx"])
        self.df_dec = make_session(paths["df_dec.onnx"])
        self._dir = model_dir
        self.dsp = _dsp.HushDsp(atten_lim_db=atten_lim_db)
        self._rs = PolyphaseResampler3()
        self.reset()

    def reset(self) -> None:
        """清空全部流式状态（DSP、重采样、模型循环状态）。"""
        self.dsp.reset()
        self._rs.reset()
        z = np.zeros((1, 1, 256), dtype=np.float32)
        self._enc_state = z.copy()
        self._erb_state = z.copy()
        self._df_state = [z.copy(), z.copy(), z.copy()]
        # enc_conv 的 3 帧特征窗口
        self._win_erb: list[np.ndarray] = []
        self._win_spec: list[np.ndarray] = []
        self.last_lsnr = float(MIN_LSNR)

    # ── 16 kHz 侧 ──
    def process_16k(self, frame) -> np.ndarray:
        """160 样本 16kHz 进 → 160 样本 16kHz 出。"""
        x = np.asarray(frame, dtype=np.float32).reshape(-1)
        if len(x) != HOP_16:
            raise ValueError(f"hush: 16k 帧长须为 {HOP_16}，得到 {len(x)}")

        # 静音短路：输出全零，且不推进任何状态（对齐 weya_nc）
        if _dsp.is_silent(x):
            self.last_lsnr = float(MIN_LSNR)
            return np.zeros(HOP_16, dtype=np.float32)

        feat_erb, feat_spec = self.dsp.front(x)

        # ① enc_conv：3 帧窗口（不足则用当前帧补），取末帧
        self._win_erb.append(feat_erb)
        self._win_spec.append(feat_spec)
        while len(self._win_erb) > ENC_WIN:
            self._win_erb.pop(0)
            self._win_spec.pop(0)
        erb_w = np.concatenate(self._win_erb, axis=2)
        spec_w = np.concatenate(self._win_spec, axis=2)
        cv = self._run(self.enc_conv, {"feat_erb": erb_w, "feat_spec": spec_w},
                       ("e0", "e1", "e2", "e3", "c0", "gru_in"))

        # ② enc_gru：逐帧 + 状态
        g = self._run(self.enc_gru,
                      {"gru_in": cv["gru_in"][-1:], "enc_state_in": self._enc_state},
                      ("emb", "lsnr", "enc_state_out"))
        self._enc_state = g["enc_state_out"]
        lsnr = float(np.asarray(g["lsnr"]).reshape(-1)[0])
        self.last_lsnr = lsnr
        mode = _dsp.hush_gate(lsnr)

        if mode == _dsp.MODE_BYPASS:
            return self.dsp.back(None, None, mode)

        if mode == _dsp.MODE_ZEROS:
            return self.dsp.back(None, None, mode)

        # ③ erb_dec：逐帧 + 状态 → 32 个 ERB 增益
        e = self._run(self.erb_dec, {
            "emb": g["emb"],
            "e3": cv["e3"][:, :, -1:, :], "e2": cv["e2"][:, :, -1:, :],
            "e1": cv["e1"][:, :, -1:, :], "e0": cv["e0"][:, :, -1:, :],
            "erb_state_in": self._erb_state,
        }, ("m", "erb_state_out"))
        self._erb_state = e["erb_state_out"]

        # ④ df_dec：逐帧 + 3 个状态 → coefs [1,1,64,10]
        d = self._run(self.df_dec, {
            "emb": g["emb"], "c0": cv["c0"][:, :, -1:, :],
            "df_state0_in": self._df_state[0],
            "df_state1_in": self._df_state[1],
            "df_state2_in": self._df_state[2],
        }, ("coefs", "df_state0_out", "df_state1_out", "df_state2_out"))
        self._df_state = [d["df_state0_out"], d["df_state1_out"], d["df_state2_out"]]

        # coefs [1,1,64,10] -> [DF_ORDER, NB_DF, 2]
        c = np.asarray(d["coefs"], dtype=np.float64).reshape(_dsp.NB_DF, _dsp.DF_ORDER, 2)
        coefs = np.transpose(c, (1, 0, 2))
        gains = np.asarray(e["m"], dtype=np.float64).reshape(-1)
        return self.dsp.back(gains, coefs, mode)

    # ── 48 kHz 侧 ──
    def process_48k(self, frame) -> np.ndarray:
        """480 样本 48kHz 进 → 480 样本 48kHz 出（内部 3:1 重采样）。"""
        x = np.asarray(frame, dtype=np.float32).reshape(-1)
        if len(x) != HOP_48:
            raise ValueError(f"hush: 48k 帧长须为 {HOP_48}，得到 {len(x)}")
        y16 = self._rs.decimate(x)
        out16 = self.process_16k(y16)
        return self._rs.interpolate(out16)

    # ── 内部 ──
    @staticmethod
    def _run(sess, feeds, names):
        outs = sess.run(list(names), feeds)
        return dict(zip(names, outs))


# 静音短路时对外报告的 lsnr（对齐 weya_nc 返回 -15）
MIN_LSNR = -15.0


class HushStage(Stage):
    """Hush 降噪的引擎 Stage——波形 hop 直入直出（480 样本 @48kHz）。

    与 dfnet 系模型不同：Hush 的 STFT 不在模型图内，由本 Stage 内部的
    `HushDsp` 负责，所以这里对外的接口仍是「10ms hop 波形进出」，符合
    `context.HOP_LENGTH` 规约。
    """

    name = "hush"

    def __init__(self, model_dir: str, atten_lim_db: float = 100.0) -> None:
        super().__init__()
        self.engine = HushEngine(model_dir, atten_lim_db=atten_lim_db)

    @property
    def last_lsnr(self) -> float:
        """最近一帧的局部信噪比估计（dB，静音帧为 -15）。"""
        return self.engine.last_lsnr

    def process(self, frame, ctx):
        return self.engine.process_48k(frame)

    def reset(self) -> None:
        self.engine.reset()

    def release(self) -> None:
        for attr in ("enc_conv", "enc_gru", "erb_dec", "df_dec"):
            setattr(self.engine, attr, None)
