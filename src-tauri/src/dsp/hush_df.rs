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
//
// ── 第三方来源声明 ──
// 本文件是 DeepFilterNet（github.com/Rikorose/DeepFilterNet，作者 Hendrik
// Schröter 等，MIT / Apache-2.0 双许可）中 libdf 前后端 DSP 的 Rust 移植，
// 算法语义逐条对照其 `df/src/lib.rs` 与 `df/src/transforms.rs` 实现，
// 以复现 Hush（pulp-vision/Hush，Apache-2.0）官方发布包
// `weya_nc` 的流式行为。移植部分仍按 GPL-3.0-or-later 分发。

//! Hush 的 libdf 前后端 DSP（16 kHz、10 ms hop）。
//!
//! 为什么需要它
//! ------------
//! Hush 发布的 ONNX 只有 `enc_conv` / `enc_gru` / `erb_dec` / `df_dec` 四个神经网络
//! 图，**STFT、ERB 滤波器组、归一化、掩码展开、深滤波、ISTFT 全部在 host 侧**，
//! 官方是打包进预编译动态库（`weya_nc`，内部用 libdf + tract）提供的。
//! 本文件就是这一段。
//!
//! 契约（来自 `models/hush/config.ini`）
//! ------------------------------------
//! ```text
//! sr = 16000   hop = 160 (10ms)   fft = 320   nb_erb = 32   nb_df = 64
//! df_order = 5   norm_tau = 1.0   min_nb_erb_freqs = 2
//! ```
//!
//! hop × 2 == fft，所以重叠结构退化到最简：分析缓存恰为上一帧 160 样本，
//! 合成缓存恰为 IFFT 后半帧。Vorbis 窗为功率互补，分析侧乘 `wnorm = 1/fft`，
//! 合成侧用未归一化 IFFT（乘回 n），往返增益为 1。
//!
//! 每帧数据流
//! ----------
//! ```text
//! front(frame160) -> (feat_erb[1,1,1,32], feat_spec[1,2,1,64])
//!     · 分析：rfft([上帧·w[:160], 本帧·w[160:]]) · (1/320)
//!     · ERB 带能量（带内均值）→ 10log10(v+1e-10) → 减指数均值 → /40
//!     · 前 64 bin 复数单位归一 → re/im 两通道
//!     同时把本帧「含噪谱」推入 5 帧环（深滤波需要）
//! back(gains32, coefs, mode) -> frame160
//!     · 掩码经 0/1 的 erb_inv_fb 展开成逐 bin 增益
//!     · 深滤波：前 64 bin 用最近 5 帧含噪谱做 5 阶复 FIR；64..160 用掩码结果
//!     · 合成：未归一化 irfft → 乘窗 → 与合成缓存重叠相加
//! ```

use realfft::RealFftPlanner;
use realfft::RealToComplex;

/// 模型原生采样率（16 kHz）。
pub const SR: usize = 16_000;
/// 模型原生 hop（10 ms）。
pub const HOP: usize = SR / 100;
/// 模型原生窗长（2 × hop，COLA 要求）。
pub const FFT: usize = 2 * HOP;
/// ERB 带数。
pub const NB_ERB: usize = 32;
/// 深滤波频点数。
pub const NB_DF: usize = 64;
/// 深滤波阶数（帧数）。
pub const DF_ORDER: usize = 5;
/// 指数归一化时间常数（秒）。
const NORM_TAU: f64 = 1.0;
/// ERB 频点最小宽度。
const MIN_NB_ERB_FREQS: usize = 2;

/// rfft 的 bin 数（含奈奎斯特）。
pub const N_FREQS: usize = FFT / 2 + 1;

// ── 幅度门限（对齐 weya_nc 的 RuntimeParams）──
/// lsnr 低于此值：整帧零掩码。
const MIN_DB_THRESH: f64 = -15.0;
/// lsnr 高于此值：完全不处理（直通）。
const MAX_DB_ERB_THRESH: f64 = 35.0;
/// 与上者相等 ⇒ 中间档在 weya_nc 下不可达。
///
/// 保留常量是为了与 weya_nc 的 `RuntimeParams` 逐项对照（改模型门限时要一起看）；
/// 当前 `gate()` 用不到它，故显式标注。
#[allow(dead_code)]
const MAX_DB_DF_THRESH: f64 = 35.0;
/// 帧均方低于此值：直接静音输出。
const SILENCE_MEAN_SQ: f64 = 1e-7;

// ── 指数归一初值（libdf MEAN_NORM_INIT / UNIT_NORM_INIT）──
const MEAN_NORM_INIT: (f64, f64) = (-60.0, -90.0);
const UNIT_NORM_INIT: (f64, f64) = (0.001, 0.0001);

/// 三种后端模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
  /// 掩码 + 深滤波（正常降噪）。
  Proc,
  /// 零掩码（判为纯噪声）。
  Zeros,
  /// 完全不处理。
  Bypass,
}

/// 指数归一系数，对齐 DeepFilterNet `get_norm_alpha`。
pub fn norm_alpha() -> f64 {
  (-(HOP as f64 / SR as f64) / NORM_TAU).exp()
}

/// libdf 的 Vorbis 窗：`sin(pi/2 * sin^2(pi*(i+0.5)/(N/2)))`。
///
/// **不是** hann：DF3 的 PyTorch 侧 `DfNetSE.stft` 用的是 hann，但那是训练/整文件
/// 推理路径；部署路径（libdf）用 Vorbis。要与 `weya_nc` 对齐就必须用 Vorbis。
pub fn vorbis_window(fft_size: usize) -> Vec<f32> {
  let half = fft_size as f64 / 2.0;
  (0..fft_size)
    .map(|i| {
      let s = (0.5 * std::f64::consts::PI * (i as f64 + 0.5) / half).sin();
      (0.5 * std::f64::consts::PI * s * s).sin() as f32
    })
    .collect()
}

fn freq2erb(f: f32) -> f32 {
  9.265_f32 * (1.0 + f / (24.7_f32 * 9.265_f32)).ln()
}

fn erb2freq(e: f32) -> f32 {
  (24.7_f32 * 9.265_f32) * ((e / 9.265_f32).exp() - 1.0)
}

/// 矩形 ERB 滤波器组的各带宽度（逐行移植 libdf `erb_fb`）。
///
/// 注意用 **libdf 的 9.265 公式**，而不是 Hush `dfnet_se.py` 里那个 Slaney 兜底
/// 公式（`21.4*log10(1+f/229)`）——后者只在拿不到 libdf 时使用，模型权重是按
/// libdf 宽度训练的，选错会让前端特征整体偏掉。全程 f32 运算以对齐 Rust 的舍入。
pub fn erb_widths() -> Vec<usize> {
  let nyq_freq = (SR / 2) as f32;
  let freq_width = SR as f32 / FFT as f32;
  let erb_low = freq2erb(0.0);
  let erb_high = freq2erb(nyq_freq);
  let step = (erb_high - erb_low) / NB_ERB as f32;

  let mut widths = vec![0usize; NB_ERB];
  let mut prev_freq = 0usize;
  let mut freq_over = 0usize;
  for i in 1..=NB_ERB {
    let f = erb2freq(erb_low + i as f32 * step);
    // Rust 的 f32::round() 是「四舍五入、远离零」；此处 f/freq_width >= 0
    let fb = (f / freq_width + 0.5).floor() as isize;
    // **有符号**：参考实现里 `n = fb - prev_freq - freq_over` 可能为负（当相邻
    // 带边界因舍入提前时），此时它按「不足 min_nb_erb_freqs」处理并累加 freq_over。
    // 用无符号减法会静默饱和到 0，把负值当成「恰好 0 个频点」，带宽表从此偏掉。
    let n = fb - prev_freq as isize - freq_over as isize;
    if n < MIN_NB_ERB_FREQS as isize {
      freq_over = (MIN_NB_ERB_FREQS as isize - n) as usize;
      widths[i - 1] = MIN_NB_ERB_FREQS;
    } else {
      freq_over = 0;
      widths[i - 1] = n as usize;
    }
    prev_freq = fb as usize;
  }

  // 频点数比整数带宽多 1（含奈奎斯特）
  widths[NB_ERB - 1] += 1;
  let total: usize = widths.iter().sum();
  let expect = FFT / 2 + 1;
  if total > expect {
    widths[NB_ERB - 1] -= total - expect;
  }
  widths
}

/// 逐 bin 的所属 ERB 带号（长度 161）—— 等价 libdf 的 apply_interp_band_gain。
pub fn erb_band_of_bin(widths: &[usize]) -> Vec<usize> {
  let mut v = Vec::with_capacity(N_FREQS);
  for (band, w) in widths.iter().enumerate() {
    for _ in 0..*w {
      v.push(band);
    }
  }
  v
}

/// 帧均方低于门限（对齐 weya_nc 的静音短路：输出全零、lsnr 记 -15）。
pub fn is_silent(frame: &[f32]) -> bool {
  if frame.is_empty() {
    return true;
  }
  let sum: f64 = frame.iter().map(|&x| (x as f64) * (x as f64)).sum();
  sum / (frame.len() as f64) < SILENCE_MEAN_SQ
}

/// 按 lsnr 决定后端模式，逐行对齐 weya_nc 的 `apply_stages`。
///
/// weya_nc 的 RuntimeParams 是 (-15, 35, 35, ReduceMask::MAX)，其中
/// max_db_erb_thresh == max_db_df_thresh == 35，所以「只做掩码不做深滤波」
/// 那一档实际不可达；等价行为只有三档。`ReduceMask::MAX` 是沿通道轴取
/// 最大值，而 weya_nc 恒为单声道（n_ch=1），故是恒等操作，无需实现。
pub fn gate(lsnr: f64) -> Mode {
  if lsnr < MIN_DB_THRESH {
    Mode::Zeros
  } else if lsnr > MAX_DB_ERB_THRESH {
    Mode::Bypass
  } else {
    // weya_nc 的 max_db_erb_thresh == max_db_df_thresh == 35，所以「只做掩码
    // 不做深滤波」那一档不可达；中段一律「掩码 + 深滤波」。
    Mode::Proc
  }
}

/// Hush 的前后端 DSP 状态机（16 kHz、10 ms hop）。
///
/// 与模型的配合顺序（每帧）：
///
/// ```text
/// (feat_erb, feat_spec) = dsp.front(frame);
/// ... 四个 ONNX 图 ...
/// mode = gate(lsnr);
/// out = dsp.back(gains, coefs, mode);
/// ```
///
/// `front` 会把本帧含噪谱推入 5 帧环，`back` 的深滤波依赖它，故二者必须配对调用。
pub struct HushDsp {
  alpha: f64,
  atten_lim: Option<f64>,
  prev_frame: Vec<f64>,
  syn_mem: Vec<f64>,
  mean_state: Vec<f64>,
  unit_state: Vec<f64>,
  /// 最近 DF_ORDER 帧的含噪谱（深滤波用），最旧在前、当前在后
  spec_ring: Vec<Vec<(f64, f64)>>,
  spec_noisy: Vec<(f64, f64)>,
  vorbis: Vec<f32>,
  widths: Vec<usize>,
  band_of_bin: Vec<usize>,
  fft_fwd: std::sync::Arc<dyn RealToComplex<f32>>,
  fft_inv: std::sync::Arc<dyn realfft::ComplexToReal<f32>>,
  scratch_in: Vec<f32>,
  scratch_spec: Vec<realfft::num_complex::Complex<f32>>,
  scratch_out: Vec<f32>,
}

impl HushDsp {
  pub fn new(atten_lim_db: f64) -> Self {
    let lim = atten_lim_db.abs();
    // >= 100 dB 与 < 0.01 dB 都等同于「不限衰减」（对齐 weya_nc 的 RuntimeParams）
    let atten_lim = if lim >= 100.0 || lim < 0.01 {
      None
    } else {
      Some(10.0_f64.powf(-lim / 20.0))
    };
    let mut planner = RealFftPlanner::<f32>::new();
    let fft_fwd = planner.plan_fft_forward(FFT);
    let fft_inv = planner.plan_fft_inverse(FFT);
    let widths = erb_widths();
    let band_of_bin = erb_band_of_bin(&widths);
    let mut dsp = HushDsp {
      alpha: norm_alpha(),
      atten_lim,
      prev_frame: vec![0.0; HOP],
      syn_mem: vec![0.0; HOP],
      mean_state: Vec::new(),
      unit_state: Vec::new(),
      spec_ring: Vec::new(),
      spec_noisy: vec![(0.0, 0.0); N_FREQS],
      vorbis: vorbis_window(FFT),
      widths,
      band_of_bin,
      scratch_in: fft_fwd.make_input_vec(),
      scratch_spec: fft_fwd.make_output_vec(),
      scratch_out: fft_inv.make_output_vec(),
      fft_fwd,
      fft_inv,
    };
    dsp.reset();
    dsp
  }

  /// 清空全部流式状态（换流 / 重开会话时调用）。
  pub fn reset(&mut self) {
    self.prev_frame.iter_mut().for_each(|v| *v = 0.0);
    self.syn_mem.iter_mut().for_each(|v| *v = 0.0);
    // 指数归一状态：均值态按 linspace(-60,-90,32)，单位态按 linspace(1e-3,1e-4,64)
    self.mean_state = linspace(MEAN_NORM_INIT.0, MEAN_NORM_INIT.1, NB_ERB);
    self.unit_state = linspace(UNIT_NORM_INIT.0, UNIT_NORM_INIT.1, NB_DF);
    self.spec_ring.clear();
    self.spec_noisy.iter_mut().for_each(|v| *v = (0.0, 0.0));
  }

  /// 喂一帧 160 样本，返回 `(feat_erb[1,1,1,32], feat_spec[1,2,1,64])` 的扁平数据。
  pub fn front(&mut self, frame: &[f32]) -> Result<(Vec<f32>, Vec<f32>), String> {
    if frame.len() != HOP {
      return Err(format!("hush dsp: 帧长须为 {HOP}，得到 {}", frame.len()));
    }

    // 分析：上半窗乘上一帧、下半窗乘本帧
    for (i, &w) in self.vorbis[..HOP].iter().enumerate() {
      self.scratch_in[i] = (self.prev_frame[i] * w as f64) as f32;
    }
    for (i, &x) in frame.iter().enumerate() {
      self.scratch_in[HOP + i] = (x as f64 * self.vorbis[HOP + i] as f64) as f32;
    }
    self
      .fft_fwd
      .process(&mut self.scratch_in, &mut self.scratch_spec)
      .map_err(|e| format!("hush dsp: 前向 FFT 失败：{e}"))?;

    let wnorm = 1.0 / FFT as f64;
    for (dst, src) in self.spec_noisy.iter_mut().zip(self.scratch_spec.iter()) {
      *dst = (src.re as f64 * wnorm, src.im as f64 * wnorm);
    }
    self.prev_frame.copy_from_slice_frame(frame);

    // 深滤波环：保留最近 DF_ORDER 帧（含本帧）
    self.spec_ring.push(self.spec_noisy.clone());
    if self.spec_ring.len() > DF_ORDER {
      self.spec_ring.remove(0);
    }

    // ① ERB 特征：带内平均功率 → dB → 减指数均值 → /40
    let mut feat_erb = vec![0.0f32; NB_ERB];
    let mut start = 0usize;
    for (b, w) in self.widths.iter().enumerate() {
      let mut sum = 0.0f64;
      for k in start..start + w {
        let (re, im) = self.spec_noisy[k];
        sum += re * re + im * im;
      }
      let band = sum / *w as f64;
      start += w;
      let band_db = 10.0 * (band + 1e-10).log10();
      self.mean_state[b] = band_db * (1.0 - self.alpha) + self.mean_state[b] * self.alpha;
      feat_erb[b] = ((band_db - self.mean_state[b]) / 40.0) as f32;
    }

    // ② 复数特征：前 NB_DF 个 bin 单位归一（re 整块在前、im 整块在后）
    let mut feat_spec = vec![0.0f32; 2 * NB_DF];
    for k in 0..NB_DF {
      let (re, im) = self.spec_noisy[k];
      let mag = (re * re + im * im).sqrt();
      self.unit_state[k] = mag * (1.0 - self.alpha) + self.unit_state[k] * self.alpha;
      let scale = 1.0 / self.unit_state[k].sqrt();
      feat_spec[k] = (re * scale) as f32;
      feat_spec[NB_DF + k] = (im * scale) as f32;
    }
    Ok((feat_erb, feat_spec))
  }

  /// 掩码 + 深滤波 + 合成，返回一帧 160 样本。
  ///
  /// `gains`：32 个 ERB 增益；`coefs`：`[DF_ORDER, NB_DF, 2]`（仅 `Mode::Proc` 用）。
  pub fn back(
    &mut self,
    gains: Option<&[f32]>,
    coefs: Option<&[f32]>,
    mode: Mode,
  ) -> Result<Vec<f32>, String> {
    match mode {
      Mode::Bypass => {
        let noisy = self.spec_noisy.clone();
        return self.synthesize(&noisy);
      }
      Mode::Zeros => {
        return self.synthesize(&vec![(0.0, 0.0); N_FREQS]);
      }
      Mode::Proc => {}
    }

    let g = gains.ok_or_else(|| "hush dsp: proc 模式需要 ERB 增益".to_string())?;
    if g.len() != NB_ERB {
      return Err(format!(
        "hush dsp: 需要 {NB_ERB} 个 ERB 增益，得到 {}",
        g.len()
      ));
    }

    // 掩码：逐 bin 取所属 ERB 带的增益（libdf apply_interp_band_gain）
    let mut out: Vec<(f64, f64)> = (0..N_FREQS)
      .map(|k| {
        let band = self.band_of_bin[k];
        let gg = g[band] as f64;
        let (re, im) = self.spec_noisy[k];
        (re * gg, im * gg)
      })
      .collect();

    // 深滤波：前 NB_DF 个 bin 用最近 DF_ORDER 帧含噪谱做复数 FIR
    let c = coefs.ok_or_else(|| "hush dsp: proc 模式需要深滤波系数".to_string())?;
    if c.len() != DF_ORDER * NB_DF * 2 {
      return Err(format!(
        "hush dsp: coefs 长度应为 {}，得到 {}",
        DF_ORDER * NB_DF * 2,
        c.len()
      ));
    }
    let ring = &self.spec_ring;
    // 流开头不足 5 帧：前面按 0 补（等价 Python 的 spec_pad 前置补零）。
    // 顺序与参考实现逐条对齐：`frames[o]` 里 o=0 是**最旧**的一帧，o=DF_ORDER-1
    // 是当前帧；补零补在 `frames` 的**前面**，所以最旧的那几阶系数对应零输入。
    let pad = DF_ORDER.saturating_sub(ring.len());
    let mut frames: Vec<Option<&[(f64, f64)]>> = Vec::with_capacity(DF_ORDER);
    for _ in 0..pad {
      frames.push(None);
    }
    for f in ring.iter() {
      frames.push(Some(f.as_slice()));
    }
    for (k, dst) in out.iter_mut().take(NB_DF).enumerate() {
      let mut acc = (0.0f64, 0.0f64);
      for o in 0..DF_ORDER {
        let (re, im) = match frames[o] {
          Some(f) => f[k],
          None => (0.0, 0.0),
        };
        let cr = c[o * NB_DF * 2 + k * 2] as f64;
        let ci = c[o * NB_DF * 2 + k * 2 + 1] as f64;
        // (re + i·im) · (cr + i·ci)
        acc.0 += re * cr - im * ci;
        acc.1 += re * ci + im * cr;
      }
      *dst = acc;
    }

    // 衰减限（默认不限，故一般不触发）：干湿混合回一部分含噪信号
    if let Some(lim) = self.atten_lim {
      for (dst, &(nre, nim)) in out.iter_mut().zip(self.spec_noisy.iter()) {
        *dst = (
          dst.0 * (1.0 - lim) + nre * lim,
          dst.1 * (1.0 - lim) + nim * lim,
        );
      }
    }
    self.synthesize(&out)
  }

  /// 合成：逆变换 → 乘窗 → 与合成缓存重叠相加。
  ///
  /// **归一化**：realfft 的 `ComplexToReal` 与 rustfft 的逆变换一样**不做** 1/N
  /// 归一化，所以这里不需要像 numpy 那样「乘回 N」——直接用它就是「未归一化
  /// irfft」的等价形式（分析侧已经乘过 1/FFT，往返增益为 1）。
  fn synthesize(&mut self, spec_out: &[(f64, f64)]) -> Result<Vec<f32>, String> {
    for (dst, src) in self.scratch_spec.iter_mut().zip(spec_out.iter()) {
      dst.re = src.0 as f32;
      dst.im = src.1 as f32;
    }
    // 实数信号的谱：直流与奈奎斯特 bin 的虚部必须为 0（realfft 会校验并报错）
    self.scratch_spec[0].im = 0.0;
    let last = self.scratch_spec.len() - 1;
    self.scratch_spec[last].im = 0.0;
    self
      .fft_inv
      .process(&mut self.scratch_spec, &mut self.scratch_out)
      .map_err(|e| format!("hush dsp: 逆 FFT 失败：{e}"))?;

    let mut out = vec![0.0f32; HOP];
    let mut next_mem = vec![0.0f64; HOP];
    for (i, &w) in self.vorbis.iter().enumerate() {
      let x = self.scratch_out[i] as f64 * w as f64;
      if i < HOP {
        out[i] = (x + self.syn_mem[i]) as f32;
      } else {
        next_mem[i - HOP] = x;
      }
    }
    self.syn_mem = next_mem;
    Ok(out)
  }
}

fn linspace(from: f64, to: f64, n: usize) -> Vec<f64> {
  if n == 1 {
    return vec![from];
  }
  (0..n)
    .map(|i| from + (to - from) * i as f64 / (n - 1) as f64)
    .collect()
}

/// 便于把 `&[f32]` 拷到 `Vec<f64>`（分析缓存）。
trait CopyFrame {
  fn copy_from_slice_frame(&mut self, src: &[f32]);
}

impl CopyFrame for Vec<f64> {
  fn copy_from_slice_frame(&mut self, src: &[f32]) {
    for (d, s) in self.iter_mut().zip(src.iter()) {
      *d = *s as f64;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn erb_widths_sum_to_bins() {
    let w = erb_widths();
    assert_eq!(w.len(), NB_ERB);
    assert_eq!(w.iter().sum::<usize>(), N_FREQS);
    // 与 Python 参考实现逐带一致（`pvengine/components/hush_dsp.py` 的 `_ERB_WIDTHS`）：
    // 前 20 带各 2 个频点，之后 3,6,7,7,8,8,10,12,12,14,16,18。
    let expect = [
      2usize, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 6, 7, 7, 8, 8, 10, 12,
      12, 14, 16, 18,
    ];
    assert_eq!(w, expect, "ERB 带宽与参考实现不一致");
  }

  #[test]
  fn vorbis_window_power_complementary() {
    // hop×2 == fft 时，重叠一半的功率互补条件是 w[i]^2 + w[i+HOP]^2 == 1。
    // （分析侧取「上帧后半 + 本帧前半」，合成侧重叠相加，两者成立才往返为 1。）
    let w = vorbis_window(FFT);
    let mut max_err = 0.0f64;
    for i in 0..HOP {
      let a = w[i] as f64;
      let b = w[i + HOP] as f64;
      max_err = max_err.max((a * a + b * b - 1.0).abs());
    }
    assert!(max_err < 1e-6, "Vorbis 窗功率互补最大偏差 {max_err:e}");
  }

  #[test]
  fn gate_three_modes() {
    assert_eq!(gate(-20.0), Mode::Zeros);
    assert_eq!(gate(0.0), Mode::Proc);
    assert_eq!(gate(40.0), Mode::Bypass);
  }

  #[test]
  fn front_first_frame_matches_reference() {
    // 与 Python 参考实现同一份输入（`tests-data/hush_in.f32` 前 480 样本 → 抽取 160）
    // 的第 0 帧特征对拍。参考值（Python 打印）：
    //   feat_erb[:4]     = [0.06564015, -0.09749794, -0.24902768, -0.14017358]
    //   feat_spec re[:4] = [0.04581058, -0.00201727, -0.02414283, 0.00546831]
    //   feat_spec im[:4] = [0.0, 0.03986746, -0.0044568, -0.00591848]
    let path =
      std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests-data/hush_in.f32");
    let Ok(raw) = std::fs::read(path) else {
      return; // 没有黄金数据就跳过
    };
    let x: Vec<f32> = raw
      .as_chunks::<4>()
      .0
      .iter()
      .take(480)
      .map(|b| f32::from_le_bytes(*b))
      .collect();
    let mut rs = crate::dsp::resample3::PolyphaseResampler3::new();
    let y16 = rs.decimate(&x).unwrap();
    let mut dsp = HushDsp::new(100.0);
    let (fe, fs) = dsp.front(&y16).unwrap();

    let ref_erb = [0.06564015f32, -0.09749794, -0.24902768, -0.14017358];
    let ref_re = [0.04581058f32, -0.00201727, -0.02414283, 0.00546831];
    let ref_im = [0.0f32, 0.03986746, -0.0044568, -0.00591848];
    for i in 0..4 {
      assert!(
        (fe[i] - ref_erb[i]).abs() < 1e-5,
        "feat_erb[{i}] 应为 {}，实测 {}",
        ref_erb[i],
        fe[i]
      );
      assert!(
        (fs[i] - ref_re[i]).abs() < 1e-5,
        "feat_spec re[{i}] 应为 {}，实测 {}",
        ref_re[i],
        fs[i]
      );
      assert!(
        (fs[NB_DF + i] - ref_im[i]).abs() < 1e-5,
        "feat_spec im[{i}] 应为 {}，实测 {}",
        ref_im[i],
        fs[NB_DF + i]
      );
    }
  }

  #[test]
  fn analysis_synthesis_roundtrip_single_frame() {
    // 单帧恒等：front 之后立刻 back(Bypass) 的第一段输出，应当等于
    // 「上一帧(=0) 的后半窗 + 本帧的前半窗」经合成窗后的重建 —— 对首帧而言，
    // 输出的前 HOP 样本全部来自 0*prev，故应为 0；第二段的 half 才是本帧内容。
    // 这里只钉住「窗与 FFT 的归一化彼此抵消」：把一个纯直流帧送进去，
    // 稳态下 Bypass 输出应当还原同一个直流值。
    let mut dsp = HushDsp::new(100.0);
    let frame = vec![0.25f32; HOP];
    for _ in 0..10 {
      dsp.front(&frame).unwrap();
      dsp.back(None, None, Mode::Bypass).unwrap();
    }
    let out = dsp.back(None, None, Mode::Bypass).unwrap();
    let avg: f64 = out.iter().map(|&v| v as f64).sum::<f64>() / HOP as f64;
    assert!(
      (avg - 0.25).abs() < 5e-3,
      "直流帧往返后应为 0.25，实测 {avg:.4}（说明窗/FFT 归一化没互补）"
    );
  }

  #[test]
  fn bypass_is_crossfade_of_adjacent_frames() {
    // hop×2 == fft 且窗功率互补时，Bypass 的重建是**延迟 1 帧的恒等**：
    //
    //     out_m[i] = x_m[i] + x_{m-1}[i+HOP]
    //              = w[i]²·frame_{m-1}[i] + w[i+HOP]²·frame_{m-1}[i]
    //              = frame_{m-1}[i]          （因 w[i]² + w[i+HOP]² == 1）
    //
    // 这里同时钉住窗归一化、FFT 归一化与合成缓存的叠加顺序。
    let mut dsp = HushDsp::new(100.0);
    let mut rng: u32 = 0x1234_5678;
    let mut next = || {
      rng = rng.wrapping_mul(1_103_515_245).wrapping_add(12_345);
      ((rng >> 16) as f32 / 32768.0) - 1.0
    };
    let frames = 12;
    let mut input: Vec<Vec<f32>> = Vec::new();
    let mut output: Vec<Vec<f32>> = Vec::new();
    for _ in 0..frames {
      let f: Vec<f32> = (0..HOP).map(|_| next()).collect();
      input.push(f.clone());
      dsp.front(&f).unwrap();
      output.push(dsp.back(None, None, Mode::Bypass).unwrap());
    }
    let mut max_err = 0.0f64;
    for m in 1..frames {
      for i in 0..HOP {
        let d = (output[m][i] as f64 - input[m - 1][i] as f64).abs();
        if d > max_err {
          max_err = d;
        }
      }
    }
    assert!(
      max_err < 1e-5,
      "Bypass 应为延迟 1 帧的恒等，最大偏差 {max_err:e}"
    );
  }
}
