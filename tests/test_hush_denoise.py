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

"""Hush 降噪合成测试：python tests/test_hush_denoise.py

分两层，前 7 项只依赖 numpy（不需要模型与 onnxruntime）：

1. ERB 滤波器组（libdf 9.265 公式）宽度与逐 bin 映射；
2. Vorbis 窗功率互补（OLA 无损重构前提）；
3. 48k<->16k 多相 FIR：10ms 网格不漂移、非 3 倍数拒绝；
4. 抗混叠：48k 高频经抽取不得折回带内（与滤波器自响应交叉验证）；
5. 直流增益两向为 1；
6. 分析→合成往返无损，且滞后恰为 1 hop（Hush 固有 STFT 延迟）；
7. lsnr 门控与静音短路门限。

后 2 项需要 models/hush/ 与 onnxruntime，缺失时跳过（打印 SKIP）：

8. 引擎逐帧跑通：输出有限、帧长不变、静音帧输出全零、reset 可用；
9. 节点注册：「Hush AI 降噪」出现在处理类清单里，且不在默认链中。
"""

import math
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import numpy as np

from pvengine.components import hush_dsp as dsp
from pvengine.dsp.resample3 import PolyphaseResampler3

HOP48, HOP16 = 480, 160
FS48, FS16 = 48000, 16000

_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
_MODEL_DIR = os.path.join(_ROOT, "models", "hush")


def _models_available():
    return all(os.path.isfile(os.path.join(_MODEL_DIR, f))
               for f in ("enc_conv.onnx", "enc_gru.onnx", "erb_dec.onnx", "df_dec.onnx"))


def _amp_at(sig, fs, freq):
    """正交相关测 sig 在 freq 处的幅度。"""
    n = np.arange(len(sig))
    s = np.asarray(sig, dtype=np.float64) - float(np.mean(sig))
    c = np.cos(2 * np.pi * freq / fs * n)
    q = np.sin(2 * np.pi * freq / fs * n)
    return 2.0 * math.hypot(float(np.dot(s, c)), float(np.dot(s, q))) / len(sig)


def _dec_all(x48, frames):
    rs = PolyphaseResampler3()
    return np.concatenate([rs.decimate(x48[i * HOP48:(i + 1) * HOP48])
                           for i in range(frames)])


# ── 1. ERB 滤波器组 ──
def test_erb_filterbank():
    w = dsp._ERB_WIDTHS
    assert len(w) == dsp.NB_ERB == 32, f"带数 {len(w)}"
    assert int(w.sum()) == dsp.N_FREQS == 161, f"合计 {int(w.sum())}"
    assert int(w.min()) >= dsp.MIN_NB_ERB_FREQS, f"最小带宽 {int(w.min())}"
    m = dsp._ERB_BAND_OF_BIN
    assert len(m) == dsp.N_FREQS, f"逐 bin 映射长度 {len(m)}"
    assert int(m.max()) == 31 and int(m.min()) == 0
    # 每个带被恰好覆盖 width 个 bin
    for b in range(dsp.NB_ERB):
        assert int((m == b).sum()) == int(w[b]), f"带 {b} 覆盖数不符"
    print(f"  ERB: 32 带合计 {int(w.sum())} bin，最小宽 {int(w.min())}  OK")


# ── 2. Vorbis 窗 ──
def test_vorbis_window_cola():
    v = dsp._VORBIS.astype(np.float64)
    assert len(v) == dsp.FFT
    err = float(np.abs(v[:dsp.HOP] ** 2 + v[dsp.HOP:] ** 2 - 1.0).max())
    assert err < 1e-6, f"功率互补偏差 {err:.3e}"
    assert v.min() > 0.0, "窗出现零/负值"
    print(f"  Vorbis 窗: w^2+w^2-1 最大偏差 {err:.2e}  OK")


# ── 3. 重采样网格 ──
def test_resampler_grid():
    rs = PolyphaseResampler3()
    for _ in range(500):
        assert rs.decimate(np.zeros(HOP48, np.float32)).shape == (HOP16,)
        assert rs.interpolate(np.zeros(HOP16, np.float32)).shape == (HOP48,)
    try:
        rs.decimate(np.zeros(HOP48 + 1, np.float32))
        raise AssertionError("非 3 倍数输入未被拒绝")
    except ValueError:
        pass
    print("  网格: 500 帧 480->160->480 形状稳定，非 3 倍数被拒  OK")


# ── 4. 抗混叠（与滤波器自响应交叉验证）──
def test_resampler_anti_alias():
    n = 400
    t = np.arange(n * HOP48)
    # 通带 1kHz 应基本无损
    x = np.sin(2 * np.pi * 1000 / FS48 * t)
    d = _dec_all(x, n)
    a = _amp_at(d[80:], FS16, 1000)
    assert abs(a - 1.0) < 0.02, f"1kHz 幅度 {a:.4f}"

    # 20kHz 应被压掉：若朴素抽取会折到 4kHz
    x = np.sin(2 * np.pi * 20000 / FS48 * t)
    d = _dec_all(x, n)
    res = _amp_at(d[80:], FS16, 4000)
    db = 20 * math.log10(max(res, 1e-12))
    assert db < -80.0, f"20kHz 折回残余 {db:.1f} dBFS（应 < -80）"

    # 与原型滤波器在 20kHz 的衰减交叉验证（允许 15dB 测量余量）
    from pvengine.dsp import resample3 as _r3
    Hf = np.abs(np.fft.rfft(_r3._PROTOTYPE.astype(np.float64), 65536))
    fr = np.fft.rfftfreq(65536, 1 / FS48)
    pred = 20 * math.log10(max(float(Hf[int(np.argmin(np.abs(fr - 20000)))]), 1e-12))
    assert db - pred < 15.0, f"实测 {db:.1f} 与预测 {pred:.1f} 差过大"
    print(f"  抗混叠: 1kHz 幅度 {a:.4f}；20kHz 折回 {db:.1f} dBFS（原型预测 {pred:.1f}）  OK")


# ── 5. 直流增益 ──
def test_resampler_dc():
    rs = PolyphaseResampler3()
    d = np.concatenate([rs.decimate(np.full(HOP48, 0.5, np.float32)) for _ in range(50)])
    rs = PolyphaseResampler3()
    u = np.concatenate([rs.interpolate(np.full(HOP16, 0.5, np.float32)) for _ in range(50)])
    assert abs(d[-160:].mean() - 0.5) < 1e-6, f"抽取直流 {d[-160:].mean()}"
    assert abs(u[-480:].mean() - 0.5) < 1e-6, f"插值直流 {u[-480:].mean()}"
    print("  直流增益: 两向 0.5 -> 0.5  OK")


# ── 6. 分析→合成往返 ──
def test_dsp_roundtrip():
    T = 60
    t = np.arange(T * dsp.HOP)
    src = (0.35 * np.sin(2 * np.pi * 440 / dsp.SR * t)
           + 0.2 * np.sin(2 * np.pi * 3000 / dsp.SR * t))
    h = dsp.HushDsp()
    out = np.zeros(T * dsp.HOP)
    for i in range(T):
        h.front(src[i * dsp.HOP:(i + 1) * dsp.HOP])
        out[i * dsp.HOP:(i + 1) * dsp.HOP] = h.back(None, None, dsp.MODE_BYPASS)
    # 固有延迟恰为 1 hop
    skip = 4 * dsp.HOP
    err = float(np.abs(out[skip:] - src[skip - dsp.HOP:-dsp.HOP]).max())
    assert err < 1e-5, f"往返误差 {err:.3e}"
    print(f"  往返: 误差 {err:.2e}，滞后恰为 1 hop({dsp.HOP} 样本)  OK")


# ── 7. 门控与静音 ──
def test_gate_and_silence():
    assert dsp.hush_gate(-25.0) == dsp.MODE_ZEROS
    assert dsp.hush_gate(-15.0) == dsp.MODE_PROC
    assert dsp.hush_gate(0.0) == dsp.MODE_PROC
    assert dsp.hush_gate(35.0) == dsp.MODE_PROC
    assert dsp.hush_gate(35.1) == dsp.MODE_BYPASS
    assert dsp.is_silent(np.zeros(dsp.HOP, np.float32))
    assert not dsp.is_silent(np.full(dsp.HOP, 0.5, np.float32))
    assert abs(dsp.norm_alpha() - math.exp(-160 / 16000)) < 1e-12
    print("  gating: -15/35 dB 门限 + 静音判定 + alpha  OK")


# ── 8. 引擎（需模型）──
def test_engine_pipeline():
    if not _models_available():
        print("  SKIP: models/hush/ 缺失")
        return
    from pvengine.components.hush import HushStage

    st = HushStage(_MODEL_DIR)
    rng = np.random.default_rng(0)

    # 静音帧 → 输出全零
    out = st.process(np.zeros(HOP48, np.float32), None)
    assert out.shape == (HOP48,)
    assert float(np.abs(out).max()) == 0.0, "静音帧未输出全零"

    # 有声帧 → 有限、同长
    n = 30 * HOP48
    sig = (0.25 * np.sin(2 * np.pi * 600 / FS48 * np.arange(n))
           + 0.03 * rng.standard_normal(n)).astype(np.float32)
    y = np.concatenate([st.process(sig[i * HOP48:(i + 1) * HOP48], None)
                        for i in range(n // HOP48)])
    assert y.shape == (n,)
    assert np.isfinite(y).all(), "输出出现 NaN/Inf"
    assert float(np.abs(y).max()) <= 1.0 + 1e-6, "输出越界"
    st.reset()
    print(f"  引擎: 静音帧全零、有声帧有限同长（输出峰值 {float(np.abs(y).max()):.4f}）  OK")


# ── 9. 节点注册 ──
def test_node_registered():
    if not _models_available():
        print("  SKIP: models/hush/ 缺失")
        return
    from pvengine import plugins as P

    specs = {s.name: s for s in P.all_specs()}
    sp = specs.get("hush_denoise")
    assert sp is not None, "hush_denoise 未注册"
    assert sp.label == "Hush AI 降噪", f"label={sp.label!r}"
    assert sp.kind == "fx", f"kind={sp.kind!r}"
    assert sp.tier == "toggle", f"tier={sp.tier!r}"
    assert "denoiser" in specs, "原有 denoiser 节点丢失"
    types = [c["type"] for c in P.DEFAULT_CHAIN]
    assert "hush_denoise" not in types, "Hush 不应进默认链"

    cache = {}
    pl = P.create_plugin("hush_denoise", {}, stage_cache=cache)
    assert pl is not None and type(pl).__name__ == "HushDenoiserPlugin"
    print(f"  注册: label={sp.label!r} kind={sp.kind!r} tier={sp.tier!r}，不在默认链  OK")


def main() -> int:
    tests = [
        test_erb_filterbank,
        test_vorbis_window_cola,
        test_resampler_grid,
        test_resampler_anti_alias,
        test_resampler_dc,
        test_dsp_roundtrip,
        test_gate_and_silence,
        test_engine_pipeline,
        test_node_registered,
    ]
    for fn in tests:
        fn()
    print("test_hush_denoise: 全部通过")
    return 0


if __name__ == "__main__":
    sys.exit(main())
