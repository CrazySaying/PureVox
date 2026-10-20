// PureVox — AI 麦克风降噪工具
// Copyright (C) 2024-2026 a2heng <752848283@qq.com>
//
// PureVox is licensed under the GNU General Public License v3.0 or
// later (GPL-3.0-or-later).  See LICENSE for details.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// The built-in AI models are NOT covered by the GPL; they are the
// property of a2heng and may only be used with PureVox under
// authorization.  See MODEL-LICENSE.md for details.
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! 48 kHz ↔ 16 kHz 流式多相 FIR 重采样（整数比 3:1 / 1:3）。
//!
//! **为什么不能用 [`crate::dsp::resampler`]**：那是 rubato 的 sinc 异步重采样器，
//! 面向「设备原生采样率 → 48 kHz」的任意比例，且带可变比例（时钟伺服要用）。
//! Hush 需要的是**固定 3:1 整数比**、零可变性、每块恒定 480 进 / 160 出的确定性
//! 映射 —— 用异步重采样器会引入它自己的内部缓冲与延迟，10 ms 网格上算不清。
//!
//! 本模块用窗函数 sinc 低通 + 多相分解：抽取前把 8 kHz 以上压掉（否则 8–24 kHz
//! 会整体折叠进 0–8 kHz，直接毁掉输出），插值后把镜像压掉。
//!
//! **10 ms 网格**：480 样本(48k) ↔ 160 样本(16k)，严格 3:1 整数比，与
//! `audio::HOP` 同网格，不引入错位块；有状态但**不漂移**（每块恒定 480 进 / 160 出）。
//!
//! **群延迟**：抽取段 (N-1)/2 = 47.5 个 48k 样本，插值段 3(T-1)/2 = 46.5 个，
//! 合计约 1.96 ms，恒定可补偿；[`PolyphaseResampler3::latency_48k`] 给出该值。

/// 抽取/插值比（48k ÷ 16k）。
const PHASES: usize = 3;
/// 每相抽头数。
const TAPS_PER_PHASE: usize = 32;
/// 原型滤波器总抽头数。
const N_TAPS: usize = PHASES * TAPS_PER_PHASE;
/// 截止频率（相对 48 kHz 奈奎斯特，即 7.8 kHz）。
const CUTOFF: f64 = 0.325;
/// Kaiser 窗 β。
const KAISER_BETA: f64 = 9.0;
/// 抽取段跨块历史长度。
const DEC_KEEP: usize = N_TAPS - 1;
/// 插值段跨块历史长度。
const INT_KEEP: usize = TAPS_PER_PHASE - 1;

/// 窗函数 sinc 低通原型（直流增益归一为 1）。
///
/// 用公式直算而非设计工具：系数跨版本逐位稳定，也不引入额外依赖。
fn design_prototype() -> Vec<f32> {
  let mid = (N_TAPS - 1) as f64 / 2.0;
  let mut h = Vec::with_capacity(N_TAPS);
  for n in 0..N_TAPS {
    let x = n as f64 - mid;
    // sinc(x) = sin(πx)/(πx)，x=0 时取 1
    let sinc = if x.abs() < 1e-12 {
      1.0
    } else {
      let v = std::f64::consts::PI * CUTOFF * x;
      v.sin() / v
    };
    h.push(CUTOFF * sinc * kaiser(n, N_TAPS, KAISER_BETA));
  }
  let sum: f64 = h.iter().sum();
  h.iter().map(|v| (v / sum) as f32).collect()
}

/// 零阶修正贝塞尔函数 I0（Kaiser 窗用；级数展开，收敛足够快）。
fn bessel_i0(x: f64) -> f64 {
  let mut sum = 1.0f64;
  let mut term = 1.0f64;
  let half = x / 2.0;
  for k in 1..40 {
    term *= (half / k as f64) * (half / k as f64);
    sum += term;
    if term < sum * 1e-17 {
      break;
    }
  }
  sum
}

/// Kaiser 窗第 `n` 点（长度 `len`）。
fn kaiser(n: usize, len: usize, beta: f64) -> f64 {
  let m = (len - 1) as f64;
  let r = 2.0 * n as f64 / m - 1.0;
  let arg = beta * (1.0 - r * r).max(0.0).sqrt();
  bessel_i0(arg) / bessel_i0(beta)
}

/// 48 kHz ↔ 16 kHz 流式重采样器；两个方向各自独立状态、互不干扰。
///
/// 用法：
///
/// ```text
/// let mut rs = PolyphaseResampler3::new();
/// let y16 = rs.decimate(&frame480);      // 480 -> 160
/// let y48 = rs.interpolate(&y16);        // 160 -> 480
/// ```
pub struct PolyphaseResampler3 {
  /// 抽取用的点积系数（原型的逆序）
  dec_rev: Vec<f32>,
  /// 插值用的逐相系数：`phase_fir[p] = prototype[p::PHASES]`
  phase_fir: Vec<Vec<f32>>,
  dec_hist: Vec<f32>,
  int_hist: Vec<f32>,
}

impl Default for PolyphaseResampler3 {
  fn default() -> Self {
    Self::new()
  }
}

impl PolyphaseResampler3 {
  pub fn new() -> Self {
    let proto = design_prototype();
    let mut dec_rev = proto.clone();
    dec_rev.reverse();
    let phase_fir = (0..PHASES)
      .map(|p| proto.iter().skip(p).step_by(PHASES).copied().collect())
      .collect();
    PolyphaseResampler3 {
      dec_rev,
      phase_fir,
      dec_hist: vec![0.0; DEC_KEEP],
      int_hist: vec![0.0; INT_KEEP],
    }
  }

  /// 清空两个方向的历史（换流 / 重开会话时调用）。
  pub fn reset(&mut self) {
    self.dec_hist.iter_mut().for_each(|v| *v = 0.0);
    self.int_hist.iter_mut().for_each(|v| *v = 0.0);
  }

  /// 抽取 + 插值的合计群延迟（单位：48k 样本）。
  pub fn latency_48k(&self) -> f64 {
    DEC_KEEP as f64 / 2.0 + PHASES as f64 * INT_KEEP as f64 / 2.0
  }

  /// `n` 个 48k 样本 → `n/3` 个 16k 样本（n 须为 3 的倍数）。
  ///
  /// 滤波：`y[m] = Σ_k h[k]·x[3m-k]`，即「低通后每 3 点取 1」，
  /// 8 kHz 以上先被压掉，不会折叠回带内。
  pub fn decimate(&mut self, x: &[f32]) -> Result<Vec<f32>, String> {
    if !x.len().is_multiple_of(PHASES) {
      return Err(format!(
        "decimate: 输入长度须为 {PHASES} 的倍数，得到 {}",
        x.len()
      ));
    }
    if x.is_empty() {
      return Ok(Vec::new());
    }
    let n_out = x.len() / PHASES;

    // buf[t] = 流上第 t-(N-1) 个样本（前补历史）
    let mut buf = Vec::with_capacity(DEC_KEEP + x.len());
    buf.extend_from_slice(&self.dec_hist);
    buf.extend_from_slice(x);

    let mut out = vec![0.0f32; n_out];
    for (m, dst) in out.iter_mut().enumerate() {
      let base = PHASES * m;
      let mut acc = 0.0f64;
      for (k, &h) in self.dec_rev.iter().enumerate() {
        acc += h as f64 * buf[base + k] as f64;
      }
      *dst = acc as f32;
    }
    self.dec_hist.copy_from_slice(&buf[buf.len() - DEC_KEEP..]);
    Ok(out)
  }

  /// `n` 个 16k 样本 → `3n` 个 48k 样本。
  ///
  /// 滤波：`y[3m+p] = 3·Σ_j h[3j+p]·x[m-j]`，即「插零后低通」的多相等价形式；
  /// ×3 补回插零带来的能量损失，直流增益为 1。
  pub fn interpolate(&mut self, x: &[f32]) -> Result<Vec<f32>, String> {
    if x.is_empty() {
      return Ok(Vec::new());
    }
    let n_in = x.len();

    let mut buf = Vec::with_capacity(INT_KEEP + n_in);
    buf.extend_from_slice(&self.int_hist);
    buf.extend_from_slice(x);

    let mut out = vec![0.0f32; n_in * PHASES];
    let h = TAPS_PER_PHASE - 1;
    for p in 0..PHASES {
      let fir = &self.phase_fir[p];
      for m in 0..n_in {
        // conv 的 (m+h) 号项 = Σ_j fir[j]·buf[m+h-j] = Σ_j h[3j+p]·x[m-j]
        let mut acc = 0.0f64;
        for (j, &c) in fir.iter().enumerate() {
          acc += c as f64 * buf[m + h - j] as f64;
        }
        out[m * PHASES + p] = (PHASES as f64 * acc) as f32;
      }
    }
    self.int_hist.copy_from_slice(&buf[buf.len() - INT_KEEP..]);
    Ok(out)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn decimate_first_sample_matches_reference() {
    // 与 Python 参考实现同一份输入（`hush_in.f32` 前 480 样本）的抽取首样本对拍：
    // Python 打印值为 1.7748454e-07。这条钉住「历史前补零的量、点积索引方向」
    // 两件事——写反了首帧就会差一大截。
    let path =
      std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests-data/hush_in.f32");
    let raw = match std::fs::read(path) {
      Ok(raw) => raw,
      Err(_) => return, // 没有黄金数据就跳过
    };
    let x: Vec<f32> = raw
      .as_chunks::<4>()
      .0
      .iter()
      .take(480)
      .map(|b| f32::from_le_bytes(*b))
      .collect();
    let mut rs = PolyphaseResampler3::new();
    let y = rs.decimate(&x).unwrap();
    assert_eq!(y.len(), 160);
    let expect = 1.774_845_4e-7_f32;
    assert!(
      (y[0] - expect).abs() < 1e-6,
      "抽取首样本应为 {expect:e}，实测 {:e}",
      y[0]
    );
  }

  #[test]
  fn prototype_dc_gain_is_unity() {
    let h = design_prototype();
    let sum: f64 = h.iter().map(|&v| v as f64).sum();
    assert!((sum - 1.0).abs() < 1e-6, "原型直流增益 {sum}");
  }

  #[test]
  fn grid_sizes_are_exact() {
    let mut rs = PolyphaseResampler3::new();
    let x = vec![0.1f32; 480];
    let y = rs.decimate(&x).unwrap();
    assert_eq!(y.len(), 160);
    let z = rs.interpolate(&y).unwrap();
    assert_eq!(z.len(), 480);
  }

  #[test]
  fn anti_alias_rejects_above_8k() {
    // 20 kHz 正弦（在 48k 上）抽取到 16k 时应被压到极低（不折叠成 4 kHz）
    let mut rs = PolyphaseResampler3::new();
    let freq = 20_000.0;
    let mut out = Vec::new();
    // 跳过前几块让滤波器进入稳态
    for block in 0..40 {
      let x: Vec<f32> = (0..480)
        .map(|i| {
          let t = (block * 480 + i) as f64 / 48_000.0;
          (2.0 * std::f64::consts::PI * freq * t).sin() as f32
        })
        .collect();
      let y = rs.decimate(&x).unwrap();
      if block >= 20 {
        out.extend_from_slice(&y);
      }
    }
    let peak = out.iter().fold(0.0f32, |a, &b| a.max(b.abs()));
    let db = 20.0 * (peak.max(1e-12)).log10();
    assert!(db < -60.0, "20 kHz 折叠残留 {db:.1} dBFS（应被抗混叠压掉）");
  }

  #[test]
  fn dc_gain_roundtrip() {
    let mut rs = PolyphaseResampler3::new();
    let mut last = Vec::new();
    for _ in 0..20 {
      let x = vec![0.5f32; 480];
      let y = rs.decimate(&x).unwrap();
      last = rs.interpolate(&y).unwrap();
    }
    // 稳态下直流应还原到 0.5
    let avg = last[240..].iter().map(|&v| v as f64).sum::<f64>() / (last.len() - 240) as f64;
    assert!((avg - 0.5).abs() < 1e-3, "直流往返 {avg}");
  }
}
