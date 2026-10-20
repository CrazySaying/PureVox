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

//! Hush 降噪组件：把 `infer::hush::Hush` 包成 `Stage`（DESIGN.md §3.4）。
//! 一帧 480 样本进，480 出；内部的 16 kHz 子网格与全部流式状态由引擎实例持有。
//!
//! 与 [`super::denoise`] 的差别只在**模型与 DSP 的归属**：Hush 的 STFT 在 host 侧
//! （`dsp::hush_df`），要自己做 48k↔16k 重采样；对引擎而言两者完全等价。

use crate::engine::stage::{FrameContext, Stage, StageError};
use crate::infer;
use crate::infer::hush::Hush;

pub struct HushStage {
  engine: Hush,
  /// 48k↔16k 多相 FIR 的固定群延迟（48k 样本）。Hush 是全链唯一自带重采样的
  /// 节点，这段延迟必须能在调试面板里看见，否则「为什么比降噪多拖一截」只能靠猜。
  resample_latency_48k: f64,
}

impl HushStage {
  /// 按 `models/` 下的子目录名加载（`hush`）。
  pub fn load(dir_name: &str) -> Result<Self, String> {
    let dir = infer::model_dir_path(dir_name)?;
    Ok(HushStage {
      engine: Hush::load(&dir, 100.0)?,
      resample_latency_48k: crate::dsp::resample3::PolyphaseResampler3::new().latency_48k(),
    })
  }
}

impl Stage for HushStage {
  fn name(&self) -> &'static str {
    "hush"
  }

  fn process(&mut self, frame: &mut [f32], _ctx: &FrameContext) -> Result<(), StageError> {
    let out = self.engine.process_48k(frame).map_err(StageError::Fatal)?;
    frame.copy_from_slice(&out);
    Ok(())
  }

  fn reset(&mut self) {
    self.engine.reset();
  }

  fn status(&self) -> Option<String> {
    Some(format!(
      "lsnr {:.1} dB，重采样延迟 {:.2} ms",
      self.engine.last_lsnr,
      self.resample_latency_48k / (crate::audio::SAMPLE_RATE as f64) * 1000.0
    ))
  }

  fn release(&mut self) {
    self.engine.reset();
  }
}
