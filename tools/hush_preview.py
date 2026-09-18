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

"""Hush 降噪试听工具——把一段 wav 过一遍「Hush AI 降噪」纯 Python 实现。

用途：接入 UI 之前先听效果。**重点听 8 kHz 带宽上限**——Hush 是 16 kHz
原生模型，输出带宽被硬性截断在 8 kHz，比 PureVox 自研的 48 kHz 降噪明显更「闷」。
这是模型固有属性（telephony-native 设计），不是实现缺陷。

用法::

    python tools/hush_preview.py 输入.wav 输出.wav

会同时输出一份 `输出_dry48.wav`（重采样到 48k 但**未降噪**的干信号），
方便 A/B 对比时排除重采样带来的差异。

依赖：numpy + onnxruntime（PureVox 运行时依赖），模型在 models/hush/。
"""

import argparse
import os
import sys
import time
import wave

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

HOP = 480          # 48kHz 10ms


def read_wav_mono(path: str):
    """读 wav，混合为单声道 float32（-1..1），返回 (数据, 采样率)。"""
    with wave.open(path, "rb") as w:
        sr = w.getframerate()
        ch = w.getnchannels()
        sw = w.getsampwidth()
        n = w.getnframes()
        raw = w.readframes(n)
    if sw == 2:
        a = np.frombuffer(raw, dtype="<i2").astype(np.float64) / 32768.0
    elif sw == 4:
        a = np.frombuffer(raw, dtype="<i4").astype(np.float64) / 2147483648.0
    elif sw == 1:
        a = (np.frombuffer(raw, dtype=np.uint8).astype(np.float64) - 128.0) / 128.0
    else:
        raise ValueError(f"不支持的位深: {sw * 8} bit")
    if ch > 1:
        a = a.reshape(-1, ch).mean(axis=1)
    return a.astype(np.float32), sr


def write_wav_mono(path: str, data, sr: int) -> None:
    x = np.clip(np.asarray(data, dtype=np.float64), -1.0, 1.0)
    pcm = (x * 32767.0).astype("<i2").tobytes()
    with wave.open(path, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(sr)
        w.writeframes(pcm)


def to_48k(x, sr: int):
    """把任意采样率转成 48kHz 单声道（仅用于试听前的素材准备）。"""
    if sr == 48000:
        return x, "原样 48kHz"
    if sr == 16000:
        from pvengine.dsp.resample3 import PolyphaseResampler3
        rs = PolyphaseResampler3()
        n = len(x) // 160 * 160
        y = np.concatenate([rs.interpolate(x[i * 160:(i + 1) * 160])
                            for i in range(n // 160)])
        return y.astype(np.float32), "16k 经多相 FIR 升到 48k"
    from pvengine.dsp.resampler import Resampler
    rs = Resampler()
    y = rs.process(x, 48000.0 / sr, True)
    return np.asarray(y, dtype=np.float32), f"{sr}Hz 经三次插值升到 48k（试听素材准备，非主链）"


def band_energy(x, sr, lo, hi):
    """[lo, hi) 频段能量占比。"""
    if len(x) < 4096:
        return 0.0
    w = np.hanning(len(x))
    S = np.abs(np.fft.rfft(x * w)) ** 2
    fr = np.fft.rfftfreq(len(x), 1 / sr)
    tot = float(S.sum()) + 1e-30
    return float(S[(fr >= lo) & (fr < hi)].sum()) / tot


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description="Hush 降噪试听")
    ap.add_argument("input", help="输入 wav")
    ap.add_argument("output", nargs="?", default=None, help="输出 wav（默认 <输入>_hush.wav）")
    ap.add_argument("--model-dir", default=None, help="模型目录（默认 <仓库根>/models/hush）")
    ap.add_argument("--atten-limit-db", type=float, default=100.0,
                    help="最大衰减 dB，100=不限（默认）")
    args = ap.parse_args(argv)

    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    model_dir = args.model_dir or os.path.join(root, "models", "hush")
    out_path = args.output or os.path.splitext(args.input)[0] + "_hush.wav"
    dry_path = os.path.splitext(out_path)[0] + "_dry48.wav"

    x, sr = read_wav_mono(args.input)
    x48, how = to_48k(x, sr)
    print(f"输入      : {args.input}")
    print(f"            {sr} Hz 单声道，{len(x)} 样本 = {len(x)/sr:.2f} s")
    print(f"处理素材  : {how}，48k 下 {len(x48)} 样本")

    frames = len(x48) // HOP
    x48 = x48[:frames * HOP]

    from pvengine.components.hush import HushEngine
    t0 = time.perf_counter()
    eng = HushEngine(model_dir, atten_lim_db=args.atten_limit_db)
    t_load = time.perf_counter() - t0

    y = np.zeros_like(x48)
    t0 = time.perf_counter()
    for i in range(frames):
        y[i * HOP:(i + 1) * HOP] = eng.process_48k(x48[i * HOP:(i + 1) * HOP])
    dt = time.perf_counter() - t0

    write_wav_mono(out_path, y, 48000)
    write_wav_mono(dry_path, x48, 48000)

    audio_s = frames * 0.01
    print()
    print(f"模型加载  : {t_load:.2f} s（4 个 ONNX 会话）")
    print(f"处理      : {frames} 帧 / {audio_s:.2f} s 音频，耗时 {dt:.2f} s")
    print(f"实时率    : {audio_s/dt:.2f}x 实时（{dt/frames*1000:.2f} ms/10ms 帧）")

    print()
    print("带宽与噪声（能量占比，干 vs 湿）")
    for lo, hi, tag in ((0, 8000, "0–8 kHz 语音带"),
                        (8000, 16000, "8–16 kHz"),
                        (16000, 24000, "16–24 kHz")):
        d = band_energy(x48, 48000, lo, hi)
        w_ = band_energy(y, 48000, lo, hi)
        print(f"   {tag:<12} 干 {d*100:6.2f}%   湿 {w_*100:6.2f}%")

    rms_d = float(np.sqrt(np.mean(x48 ** 2)))
    rms_w = float(np.sqrt(np.mean(y ** 2)))
    print()
    print(f"RMS 干 {rms_d:.5f} → 湿 {rms_w:.5f}（比值 {rms_w/max(rms_d,1e-12):.3f}）")
    print()
    print(f"已写出：{out_path}")
    print(f"        {dry_path}   ← 未降噪的 48k 干信号，用于公平 A/B")
    print()
    print("注意：Hush 原生 16 kHz，输出带宽上限 8 kHz；多相 FIR 群延迟 1.96 ms"
          " + Hush 自身 STFT 1 hop(10ms) + 算法延迟。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
