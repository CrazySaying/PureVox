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

//! Hush 移植的**参考实现对拍**：把 Python 版（`pvengine/components/hush.py`）
//! 生成的黄金输出与本 Rust 实现逐样本比对。
//!
//! 黄金数据**随仓库入库**（`tests-data/`，三件套共 ~1.1 MB）：
//! `gen_hush_reference.py`（生成脚本，保留在旧 Python 树里）+ `hush_in.f32`
//! （48 kHz 输入）/ `hush_ref.f32`（参考输出）/ `hush_ref.txt`（逐帧 lsnr）。
//! 数据刻意覆盖三条分支：纯噪声段、语音状谐波段、静音段。
//!
//! 用环境变量 `HUSH_PARITY_DIR` 可以改指到别的目录（手工重生成时用）；
//! 未设置就读仓库内 `tests-data/`，读不到才跳过。
//!
//! 判据是**相对误差**而不是逐位相等：两边 ONNX 会话的浮点算子顺序可能因版本
//! 不同而不同，但数值必须一致到听不出来的程度（Python 版对其官方 `weya_nc`
//! 动态库的对拍结果是最大相对误差 4.4e-04）。

use std::path::PathBuf;

use crate::infer::hush::Hush;

/// 相对误差上限。Python 版对官方库是 4.4e-04，这里给同一量级留余量。
/// 实测本移植为 ~5e-06，离上限有两个数量级的余量。
const MAX_REL_ERR: f32 = 5e-3;
/// 逐帧 lsnr 的绝对误差上限（dB）。
const MAX_LSNR_ERR: f32 = 0.05;

fn parity_dir() -> Option<PathBuf> {
  if let Some(dir) = std::env::var_os("HUSH_PARITY_DIR") {
    return Some(PathBuf::from(dir));
  }
  let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests-data");
  if repo.join("hush_in.f32").is_file() {
    Some(repo)
  } else {
    None
  }
}

fn read_f32(path: &PathBuf) -> Vec<f32> {
  let raw = std::fs::read(path).unwrap_or_else(|e| panic!("读 {} 失败：{e}", path.display()));
  assert_eq!(raw.len() % 4, 0, "{} 不是 F32 裸流", path.display());
  raw
    .as_chunks::<4>()
    .0
    .iter()
    .map(|b| f32::from_le_bytes(*b))
    .collect()
}

#[test]
fn reference_parity() {
  let Some(dir) = parity_dir() else {
    eprintln!("HUSH_PARITY_DIR 未设置，跳过 Hush 参考对拍");
    return;
  };
  let input = read_f32(&dir.join("hush_in.f32"));
  let expected = read_f32(&dir.join("hush_ref.f32"));
  let lsnr_txt = std::fs::read_to_string(dir.join("hush_ref.txt")).expect("读 hush_ref.txt");
  let expected_lsnr: Vec<f32> = lsnr_txt
    .trim()
    .split(',')
    .map(|s| s.trim().parse::<f32>().expect("解析 lsnr"))
    .collect();

  let model_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../models/hush");
  let mut hush = Hush::load(&model_dir, 100.0).expect("加载 Hush 模型（先确认 models/hush 在）");

  let hop = 480usize;
  let n = input.len() / hop;
  assert_eq!(n, expected_lsnr.len(), "hop 数与参考 lsnr 条数不一致");
  assert!(expected.len() >= n * hop, "参考输出样本不足");

  let mut max_rel = 0.0f32;
  let mut max_lsnr_err = 0.0f32;
  let mut worst = (0usize, 0usize);
  // 归一化用参考信号的 RMS，避免逐样本除零
  let ref_rms = {
    let s: f64 = expected[..n * hop]
      .iter()
      .map(|&v| (v as f64) * (v as f64))
      .sum();
    (s / (n * hop) as f64).sqrt().max(1e-9)
  };

  for i in 0..n {
    let frame = &input[i * hop..(i + 1) * hop];
    let got = hush.process_48k(frame).expect("推理失败");
    assert_eq!(got.len(), hop, "第 {i} 帧输出长度不对");
    for k in 0..hop {
      let d = (got[k] as f64 - expected[i * hop + k] as f64).abs();
      let rel = (d / ref_rms) as f32;
      if rel > max_rel {
        max_rel = rel;
        worst = (i, k);
      }
    }
    let le = (hush.last_lsnr - expected_lsnr[i]).abs();
    max_lsnr_err = max_lsnr_err.max(le);
  }

  eprintln!(
    "Hush 对拍：{n} hop，最大相对误差 {max_rel:.3e}（第 {} 帧第 {} 样本），lsnr 最大偏差 {max_lsnr_err:.4} dB",
    worst.0, worst.1
  );
  assert!(
    max_rel <= MAX_REL_ERR,
    "Hush 输出与参考实现相对误差 {max_rel:.3e} 超过上限 {MAX_REL_ERR:e}"
  );
  assert!(
    max_lsnr_err <= MAX_LSNR_ERR,
    "Hush lsnr 与参考实现偏差 {max_lsnr_err:.4} dB 超过上限 {MAX_LSNR_ERR}"
  );
}
