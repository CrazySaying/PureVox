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

//! 热参数：**与模型无关、每 hop 读一次**的标量（增益 / 音量），运行中改它**不重建会话**。
//!
//! 为什么必须区分：结构性变更（换设备 / 换模型 / 换参考音频）要重建整个会话 —— 关掉全部
//! 设备再重开。Linux 上 ALSA 是**排他**的，一次重建就是几百毫秒起步（旧流异步关闭还要
//! 退避重试）。改个增益每 1 dB 重建一次，界面会积起一长串重建命令，表现就是「改音量卡死、
//! 连停止都点不动」（2026-10-18 实测：2 列撞在同一张卡上时一次重建 7.25 s）。
//!
//! 所以热参数走 [`.set_row_param`](crate::audio::AudioManager::set_row_param)：写一个原子
//! 单元格 + 落盘计划，行工作线程**下一 hop** 就读到新值。哪些参数算热参数由注册表说了算
//! （[`super::registry::is_hot`]），界面照它决定走热参数还是 `apply_plan`。
//!
//! 为什么不用一把锁存 `Params`：数据面不许加锁（AGENTS.md §1.3）。这里一个
//! [`AtomicU32`] 存 f32 位，读就是一次 relaxed load，代价可以忽略。

use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

/// 可热改的增益（dB）。行 / 组件持有 `Arc<HotGain>`，工作线程每 hop 读。
#[derive(Debug)]
pub struct HotGain {
  db_bits: AtomicU32,
}

impl HotGain {
  pub fn new(db: f32) -> Self {
    HotGain {
      db_bits: AtomicU32::new(db.to_bits()),
    }
  }

  /// 改成新的 dB 值（运行中**实时**生效，不重建会话）。
  pub fn set_db(&self, db: f32) {
    self.db_bits.store(db.to_bits(), Relaxed);
  }

  pub fn db(&self) -> f32 {
    f32::from_bits(self.db_bits.load(Relaxed))
  }

  /// 线性倍数。每 hop 现算 —— `powf` 每秒一百次可以忽略，换来的是「改了立刻准」，
  /// 不用维护「dB 与倍数两份状态」。
  pub fn linear(&self) -> f32 {
    10f32.powf(self.db() / 20.0)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn hot_gain_reads_back_latest_value() {
    let g = HotGain::new(-6.0);
    assert!((g.linear() - 0.5011872).abs() < 1e-4);
    g.set_db(6.0);
    assert!((g.db() - 6.0).abs() < 1e-6, "改了立刻读得到");
    // 6 dB = 1.9953（不是 2.0 —— 2.0 对应 6.02 dB）
    assert!((g.linear() - 1.9952623).abs() < 1e-4);
  }

  #[test]
  fn hot_gain_zero_db_is_unity() {
    assert!((HotGain::new(0.0).linear() - 1.0).abs() < 1e-6);
  }
}

/// 建会话时的**热参数登记口**：行/组件建单元格时在这里登记，会话据此把
/// `(列, 行, 参数名) → 单元格` 收起来，供 `set_row_param` 运行中改值。
///
/// 为什么不让组件自己持有并让会话反查：单元格必须**在组件之前**存在（组件持有它），
/// 而「哪个单元格属于哪一行的哪个参数」只有建会话这一层知道（列/行序号在那里）。
#[derive(Default)]
pub struct HotSink {
  out: Vec<(String, std::sync::Arc<HotGain>)>,
}

impl HotSink {
  pub fn new() -> Self {
    HotSink::default()
  }

  /// 登记一个增益单元格（同一次建会话里同名参数只会调一次）。
  pub fn gain(&mut self, key: &str, db: f64) -> std::sync::Arc<HotGain> {
    let cell = std::sync::Arc::new(HotGain::new(db as f32));
    self.out.push((key.to_string(), cell.clone()));
    cell
  }

  /// 取走本次登记的全部单元格（建完这一行后由会话收进热参数表）。
  pub fn take(&mut self) -> Vec<(String, std::sync::Arc<HotGain>)> {
    std::mem::take(&mut self.out)
  }
}
