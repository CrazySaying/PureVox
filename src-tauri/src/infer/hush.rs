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
// 本文件编排 Hush（github.com/pulp-vision/Hush，Apache-2.0）发布的 ONNX 图，
// 其网络结构与流式状态语义来自 DeepFilterNet（github.com/Rikorose/DeepFilterNet，
// 作者 Hendrik Schröter 等，MIT / Apache-2.0 双许可）。移植部分按 GPL-3.0-or-later 分发。

//! Hush 降噪流式推理：四个 ONNX 图 + host 侧 DSP 的编排（16 kHz 原生）。
//!
//! 与 dfnet 系模型（[`crate::infer::denoise`]）的关键差别：**STFT 不在模型图内**，
//! 由 [`crate::dsp::hush_df::HushDsp`] 在 host 侧完成；因此对外接口仍是
//! 「10 ms hop 波形进出」，内部再做 48k↔16k 重采样。
//!
//! 四个图与状态
//! ------------
//! ```text
//! enc_conv  : 卷积编码器，**必须吃 3 帧窗口取末帧**（带 kernel=(3,3) 时域卷积，
//!             窗口边界处缺 2 帧上文会污染下游 GRU 状态，且污染永不归零）
//! enc_gru   : GRU 头，逐帧 + 1 个状态
//! erb_dec   : 逐帧 + 1 个状态
//! df_dec    : 逐帧 + **3 个状态**
//! ```
//!
//! 为什么 enc 要拆：见 `tools/hush_stateful_onnx.py` 文件头。`erb_dec` / `df_dec`
//! 的卷积全是 `(1,k)`，无时域延伸，实测逐帧复现误差 1.8e-7 / 1.9e-7，可直接逐帧。
//!
//! 门控与短路（对齐 weya_nc 的 `apply_stages` / `process`）
//! -----------------------------------------------------
//! - 帧均方 < 1e-7 → 输出全零、返回 lsnr = -15，**且 DSP 与全部状态都不推进**
//! - lsnr < -15 → 零掩码，**不跑** erb_dec / df_dec（状态也不推进）
//! - lsnr > 35  → 完全不处理，同样不跑解码器
//! - 其余       → 掩码 + 深滤波

use std::path::Path;

use ort::session::Session;
use ort::value::Tensor;

use crate::audio::HOP;
use crate::dsp::hush_df::{self as df, HushDsp, Mode};
use crate::dsp::resample3::PolyphaseResampler3;

/// 模型目录下的四个图文件名（缺一不可）。
const FILES: [&str; 4] = [
  "enc_conv.onnx",
  "enc_gru.onnx",
  "erb_dec.onnx",
  "df_dec.onnx",
];

/// 判定「这个目录确实是 Hush 模型目录」的探路文件（[`crate::infer::model_dir_path`] 用）。
pub const REQUIRED_FILE: &str = "enc_conv.onnx";

/// enc_conv 的窗口长度上限：编码器时域感受野实测为 3 帧
/// （见 `tools/hush_stateful_onnx.py`）。流开头不足 3 帧时按**实际帧数**跑
/// （模型图接受动态 S），与 Python 参考实现逐帧一致。
const ENC_WIN: usize = 3;

/// 静音短路时对外报告的 lsnr（对齐 weya_nc 返回 -15）。
pub const MIN_LSNR: f32 = -15.0;

/// 状态张量的形状（四个图的状态都是 `[1,1,256]`）。
const STATE_DIM: usize = 256;

/// 一帧的模型输入特征：`(feat_erb[32], feat_spec[2×64])`。
type Feat = (Vec<f32>, Vec<f32>);

/// Hush 的流式引擎。
pub struct Hush {
  enc_conv: Session,
  enc_gru: Session,
  erb_dec: Session,
  df_dec: Session,
  dsp: HushDsp,
  rs: PolyphaseResampler3,
  enc_state: Vec<f32>,
  erb_state: Vec<f32>,
  df_state: [Vec<f32>; 3],
  /// enc_conv 的特征窗口（最多 [`ENC_WIN`] 帧，最旧在前）
  win: Vec<Feat>,
  /// 最近一帧的局部信噪比估计（dB，静音帧为 -15）
  pub last_lsnr: f32,
}

impl Hush {
  /// 从模型目录加载四个图。`model_dir` 下必须同时存在 [`FILES`] 中的四个文件。
  pub fn load(model_dir: &Path, atten_lim_db: f64) -> Result<Self, String> {
    let mut sessions = Vec::with_capacity(FILES.len());
    for f in FILES {
      let p = model_dir.join(f);
      if !p.is_file() {
        return Err(format!("Hush 模型缺失：{}", p.display()));
      }
      sessions.push(crate::infer::build_session(&p)?);
    }
    let mut it = sessions.into_iter();
    let mut me = Hush {
      enc_conv: it.next().expect("按 FILES 同序构建"),
      enc_gru: it.next().expect("按 FILES 同序构建"),
      erb_dec: it.next().expect("按 FILES 同序构建"),
      df_dec: it.next().expect("按 FILES 同序构建"),
      dsp: HushDsp::new(atten_lim_db),
      rs: PolyphaseResampler3::new(),
      enc_state: vec![0.0; STATE_DIM],
      erb_state: vec![0.0; STATE_DIM],
      df_state: [
        vec![0.0; STATE_DIM],
        vec![0.0; STATE_DIM],
        vec![0.0; STATE_DIM],
      ],
      win: Vec::with_capacity(ENC_WIN),
      last_lsnr: MIN_LSNR,
    };
    me.reset();
    Ok(me)
  }

  /// 清空全部流式状态（DSP、重采样、模型循环状态）。
  pub fn reset(&mut self) {
    self.dsp.reset();
    self.rs.reset();
    self.enc_state.iter_mut().for_each(|v| *v = 0.0);
    self.erb_state.iter_mut().for_each(|v| *v = 0.0);
    for st in self.df_state.iter_mut() {
      for v in st.iter_mut() {
        *v = 0.0;
      }
    }
    self.win.clear();
    self.last_lsnr = MIN_LSNR;
  }

  /// 480 样本 48 kHz 进 → 480 样本 48 kHz 出（内部 3:1 重采样）。
  pub fn process_48k(&mut self, frame: &[f32]) -> Result<Vec<f32>, String> {
    if frame.len() != HOP {
      return Err(format!("hush: 48k 帧长须为 {HOP}，得到 {}", frame.len()));
    }
    let y16 = self.rs.decimate(frame)?;
    let out16 = self.process_16k(&y16)?;
    self.rs.interpolate(&out16)
  }

  /// 160 样本 16 kHz 进 → 160 样本 16 kHz 出。
  pub fn process_16k(&mut self, frame: &[f32]) -> Result<Vec<f32>, String> {
    if frame.len() != df::HOP {
      return Err(format!(
        "hush: 16k 帧长须为 {}，得到 {}",
        df::HOP,
        frame.len()
      ));
    }

    // 静音短路：输出全零，且不推进任何状态（对齐 weya_nc）
    if df::is_silent(frame) {
      self.last_lsnr = MIN_LSNR;
      return Ok(vec![0.0; df::HOP]);
    }

    let (feat_erb, feat_spec) = self.dsp.front(frame)?;

    // ① enc_conv：3 帧窗口（不足则用已有的几帧），取末帧
    self.win.push((feat_erb, feat_spec));
    while self.win.len() > ENC_WIN {
      self.win.remove(0);
    }
    let s = self.win.len();
    let mut erb_w = Vec::with_capacity(s * df::NB_ERB);
    let mut spec_w = Vec::with_capacity(s * 2 * df::NB_DF);
    for (e, _) in &self.win {
      erb_w.extend_from_slice(e);
    }
    // `feat_erb` 是 [1,1,S,32]、`feat_spec` 是 [1,2,S,64]，两者拼接轴都是 S，
    // 但内存布局的维度顺序不同：erb 的帧在倒数第二维（逐帧连续），
    // spec 的**通道在帧之前** —— 必须先放全部帧的实部、再放全部帧的虚部。
    // 按「逐帧连续」拼 spec 只在 S==1 时碰巧正确，S>1 会让模型读到错位的特征。
    for (_, sp) in &self.win {
      spec_w.extend_from_slice(&sp[..df::NB_DF]);
    }
    for (_, sp) in &self.win {
      spec_w.extend_from_slice(&sp[df::NB_DF..]);
    }

    let cv = run(
      &mut self.enc_conv,
      "enc_conv",
      vec![
        (
          "feat_erb",
          make_tensor(&erb_w, &[1, 1, s as i64, df::NB_ERB as i64])?,
        ),
        (
          "feat_spec",
          make_tensor(&spec_w, &[1, 2, s as i64, df::NB_DF as i64])?,
        ),
      ],
    )?;
    // 形状都是 [1,16,S,K]：通道在时间之前，故按「每通道取末帧」抽取
    let e0 = last_frame_channel_major(cv.get("e0"), 16, s, 32, "e0")?;
    let e1 = last_frame_channel_major(cv.get("e1"), 16, s, 16, "e1")?;
    let e2 = last_frame_channel_major(cv.get("e2"), 16, s, 8, "e2")?;
    let e3 = last_frame_channel_major(cv.get("e3"), 16, s, 8, "e3")?;
    let c0 = last_frame_channel_major(cv.get("c0"), 16, s, 64, "c0")?;
    // gru_in 的实测形状是 [S,1,256]（时间在最前），取末帧即末尾 256 个元素
    let gru_last = last_frame_time_major(cv.get("gru_in"), s, "gru_in")?;

    // ② enc_gru：逐帧 + 状态
    let g = run(
      &mut self.enc_gru,
      "enc_gru",
      vec![
        (
          "gru_in",
          make_tensor(&gru_last, &[1, 1, gru_last.len() as i64])?,
        ),
        (
          "enc_state_in",
          make_tensor(&self.enc_state, &[1, 1, STATE_DIM as i64])?,
        ),
      ],
    )?;
    #[cfg(test)]
    if std::env::var_os("HUSH_PARITY_INPUTS").is_some() {
      let raw_gru = cv.get("gru_in").map(|v| v.len()).unwrap_or(0);
      eprintln!(
        "  [dbg] gru_in raw len={raw_gru} last[:3]={:?}",
        &gru_last[..3.min(gru_last.len())]
      );
    }
    let emb = take(&g, "emb")?;
    let lsnr = take(&g, "lsnr")?.first().copied().unwrap_or(MIN_LSNR) as f64;
    self.enc_state = take(&g, "enc_state_out")?;
    self.last_lsnr = lsnr as f32;
    let mode = df::gate(lsnr);

    if mode != Mode::Proc {
      return self.dsp.back(None, None, mode);
    }

    // ③ erb_dec：逐帧 + 状态 → 32 个 ERB 增益
    let emb_n = emb.len() as i64;
    let e = run(
      &mut self.erb_dec,
      "erb_dec",
      vec![
        ("emb", make_tensor(&emb, &[1, 1, emb_n])?),
        ("e3", make_tensor(&e3, &[1, 16, 1, 8])?),
        ("e2", make_tensor(&e2, &[1, 16, 1, 8])?),
        ("e1", make_tensor(&e1, &[1, 16, 1, 16])?),
        ("e0", make_tensor(&e0, &[1, 16, 1, 32])?),
        (
          "erb_state_in",
          make_tensor(&self.erb_state, &[1, 1, STATE_DIM as i64])?,
        ),
      ],
    )?;
    let gains = take(&e, "m")?;
    self.erb_state = take(&e, "erb_state_out")?;

    // ④ df_dec：逐帧 + 3 个状态 → coefs [1,1,64,10]
    let d = run(
      &mut self.df_dec,
      "df_dec",
      vec![
        ("emb", make_tensor(&emb, &[1, 1, emb_n])?),
        ("c0", make_tensor(&c0, &[1, 16, 1, 64])?),
        (
          "df_state0_in",
          make_tensor(&self.df_state[0], &[1, 1, STATE_DIM as i64])?,
        ),
        (
          "df_state1_in",
          make_tensor(&self.df_state[1], &[1, 1, STATE_DIM as i64])?,
        ),
        (
          "df_state2_in",
          make_tensor(&self.df_state[2], &[1, 1, STATE_DIM as i64])?,
        ),
      ],
    )?;
    let coefs_raw = take(&d, "coefs")?;
    self.df_state = [
      take(&d, "df_state0_out")?,
      take(&d, "df_state1_out")?,
      take(&d, "df_state2_out")?,
    ];

    let coefs = reorder_coefs(&coefs_raw)?;
    self.dsp.back(Some(&gains), Some(&coefs), mode)
  }
}

impl std::fmt::Debug for Hush {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "Hush(lsnr={:.1})", self.last_lsnr)
  }
}

/// 跑一个图，按输出名收集全部扁平 f32。
fn run(
  sess: &mut Session,
  tag: &str,
  feeds: Vec<(&'static str, Tensor<f32>)>,
) -> Result<std::collections::HashMap<String, Vec<f32>>, String> {
  let outputs = sess
    .run(feeds)
    .map_err(|e| format!("Hush 推理失败（{tag}）：{e}"))?;
  let mut out = std::collections::HashMap::new();
  for (name, val) in outputs.iter() {
    let (_, data) = val
      .try_extract_tensor::<f32>()
      .map_err(|e| format!("读取 {name} 失败：{e}"))?;
    out.insert(name.to_string(), data.to_vec());
  }
  Ok(out)
}

fn take(out: &std::collections::HashMap<String, Vec<f32>>, name: &str) -> Result<Vec<f32>, String> {
  out
    .get(name)
    .cloned()
    .ok_or_else(|| format!("Hush 输出缺少 {name}"))
}

/// 从 `[1,C,S,K]` 扁平数据里取**时间轴最后一帧** → `[1,C,1,K]`（供逐帧解码器）。
///
/// 维度顺序是「通道在时间之前」，所以末帧不是「末尾 K 个元素」，而是每个通道各取
/// 自己那段里的最后一帧。取错（按扁平尾部截断）只会拿到最后一个通道的数据，
/// 且 S>1 时必然错位 —— 早期版本就踩过这个坑。
fn last_frame_channel_major(
  flat: Option<&Vec<f32>>,
  c: usize,
  s: usize,
  k: usize,
  name: &str,
) -> Result<Vec<f32>, String> {
  let flat = flat.ok_or_else(|| format!("Hush 输出缺少 {name}"))?;
  if c == 0 || s == 0 || k == 0 || flat.len() != c * s * k {
    return Err(format!(
      "Hush 张量 {name} 长度 {} 与形状 [1,{c},{s},{k}] 不符",
      flat.len()
    ));
  }
  let mut out = Vec::with_capacity(c * k);
  for ch in 0..c {
    let base = ch * s * k + (s - 1) * k;
    out.extend_from_slice(&flat[base..base + k]);
  }
  Ok(out)
}

/// 从 `[S,1,D]`（`gru_in` 的实际形状）取最后一帧 → `[D]`。
fn last_frame_time_major(
  flat: Option<&Vec<f32>>,
  s: usize,
  name: &str,
) -> Result<Vec<f32>, String> {
  let flat = flat.ok_or_else(|| format!("Hush 输出缺少 {name}"))?;
  if s == 0 || flat.is_empty() || flat.len() % s != 0 {
    return Err(format!(
      "Hush 张量 {name} 长度 {} 与帧数 {s} 不整除",
      flat.len()
    ));
  }
  let d = flat.len() / s;
  Ok(flat[flat.len() - d..].to_vec())
}

fn make_tensor(data: &[f32], shape: &[i64]) -> Result<Tensor<f32>, String> {
  let expect: i64 = shape.iter().product();
  if expect != data.len() as i64 {
    return Err(format!(
      "Hush 张量形状 {shape:?} 需要 {expect} 个元素，得到 {}",
      data.len()
    ));
  }
  // 固定长度数组而不是 Vec：`ort` 的 `from_array` 对 `Vec<i64>` 与 `[i64; N]`
  // 走同一条路径，但数组能让维度在编译期就确定下来，避免把 shape 当成数据。
  match shape.len() {
    2 => {
      let dims: [i64; 2] = [shape[0], shape[1]];
      Tensor::from_array((dims, data.to_vec())).map_err(|e| format!("构造张量失败：{e}"))
    }
    3 => {
      let dims: [i64; 3] = [shape[0], shape[1], shape[2]];
      Tensor::from_array((dims, data.to_vec())).map_err(|e| format!("构造张量失败：{e}"))
    }
    4 => {
      let dims: [i64; 4] = [shape[0], shape[1], shape[2], shape[3]];
      Tensor::from_array((dims, data.to_vec())).map_err(|e| format!("构造张量失败：{e}"))
    }
    n => Err(format!("Hush 张量维度 {n} 不支持")),
  }
}

/// `coefs [1,1,64,10]` → `[DF_ORDER, NB_DF, 2]`。
///
/// 模型最后一维是 10 = 5 阶 × 2（每阶 re/im 交错）。Python 参考实现做的是
/// `reshape(NB_DF, DF_ORDER, 2)` 再 `transpose(1, 0, 2)`：
/// `raw[k, o, c]`（k 最外层）→ `c[o, k] = (re, im)`。
fn reorder_coefs(raw: &[f32]) -> Result<Vec<f32>, String> {
  let expect = df::NB_DF * df::DF_ORDER * 2;
  if raw.len() != expect {
    return Err(format!("Hush coefs 长度应为 {expect}，得到 {}", raw.len()));
  }
  let mut out = vec![0.0f32; expect];
  for k in 0..df::NB_DF {
    for o in 0..df::DF_ORDER {
      for c in 0..2 {
        let src = k * df::DF_ORDER * 2 + o * 2 + c;
        let dst = o * df::NB_DF * 2 + k * 2 + c;
        out[dst] = raw[src];
      }
    }
  }
  Ok(out)
}
