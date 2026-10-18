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

//! 音频流的体检结论：把一堆计数与速率翻成一句人话（含关键数字）。
//!
//! 为什么单列一处：累计计数（「欠载 12345 次」）只会越涨越大，看不出**现在**稳不稳；
//! 速率能看出当下，但对着五六个数字挨个判读又太慢。判定只在这里做一份 —— 采集侧
//! （[`super::capture`]）与播放侧（[`super::playback`]）都调 [`annotate`] 填同一个
//! `health` 字段，面板与 HTTP 接口渲染的也是同一份（AGENTS.md 1.3「单一数据源」）。
//!
//! 阈值全部集中在下面几个常量里：调判定只动这一处，别散到调用点。

use crate::debug::{Health, Probe, StreamInfo};

/// 设备侧实测速率与原生采样率的容许偏差：超了说明设备没按标称速率出/收数据
/// （开错速率、时钟漂移、假设备）。3 s 窗口的正常抖动远小于 1%。
const RATE_TOLERANCE: f64 = 0.01;
/// 事件速率（次/秒）超过这个值才算「正在发生」：3 s 窗口一格 ≈ 0.33/s，留点余量。
const EVENT_EPS: f64 = 0.5;

/// 给一路流填上 `health`。调用点在 `state` 与各计数/速率都算好之后。
pub fn annotate(s: &mut StreamInfo) {
  s.health = judge(s);
}

/// 全部流的汇总结论（最坏优先）：有不可用就报不可用，其次有 warn 就报 warn。
pub fn overall(streams: &[StreamInfo]) -> Probe<Health> {
  if streams.is_empty() {
    return Probe::unavailable("没有活动的流（引擎未启动，或计划里没有输入/输出行）");
  }
  let down: Vec<&StreamInfo> = streams
    .iter()
    .filter(|s| matches!(s.health, Probe::Unavailable { .. }))
    .collect();
  if !down.is_empty() {
    let first = down.iter().find_map(|s| reason_of(s));
    return Probe::unavailable(format!(
      "{}/{} 路异常（{}）",
      down.len(),
      streams.len(),
      first.unwrap_or_else(|| "原因见各路流".to_string())
    ));
  }
  let warns: Vec<&StreamInfo> = streams
    .iter()
    .filter(|s| matches!(&s.health, Probe::Ok { value } if value.level == "warn"))
    .collect();
  if let Some(s) = warns.first() {
    return Probe::ok(Health::warn(format!(
      "{}/{} 路需注意（{}：{}）",
      warns.len(),
      streams.len(),
      s.device_name,
      text_of(s)
    )));
  }
  Probe::ok(Health::ok(format!("{} 路全部稳定", streams.len())))
}

fn reason_of(s: &StreamInfo) -> Option<String> {
  match &s.health {
    Probe::Unavailable { reason } => Some(format!("{}：{reason}", s.device_name)),
    _ => None,
  }
}

fn text_of(s: &StreamInfo) -> String {
  match &s.health {
    Probe::Ok { value } => value.text.clone(),
    _ => String::new(),
  }
}

fn judge(s: &StreamInfo) -> Probe<Health> {
  let state = match &s.state {
    // 流本身不可用（停顿 / 设备错误 / 源已停止）时，结论就是那个原因，不再叠加别的判断
    Probe::Unavailable { reason } => return Probe::unavailable(reason.clone()),
    Probe::Pending => return Probe::Pending,
    Probe::Ok { value } => value,
  };
  // 状态串不是 running 说明还在预热（状态里带着进度），照搬即可
  if state != "running" {
    return Probe::ok(Health::warn(state.clone()));
  }

  let mut issues: Vec<String> = Vec::new();
  // 设备侧实测速率 vs 原生：对不上是最容易被忽略的错（声音在，但节奏不对）
  let dev_rate = if s.direction == "output" {
    value_of(&s.measured_output_rate)
  } else {
    value_of(&s.measured_input_rate)
  };
  if let Some(v) = dev_rate {
    let want = s.sample_rate as f64;
    let off = (v - want).abs() / want;
    if off > RATE_TOLERANCE {
      issues.push(format!(
        "设备侧速率 {v:.0} Hz 与原生 {want:.0} Hz 差 {:.1}%",
        off * 100.0
      ));
    }
  }
  push_rate(&mut issues, "欠载（补静音）", &s.underrun_per_s);
  push_rate(&mut issues, "重同步", &s.resync_per_s);
  push_rate(&mut issues, "丢弃", &s.overrun_per_s);
  push_rate(&mut issues, "流错误", &s.stream_error_per_s);

  if issues.is_empty() {
    Probe::ok(Health::ok(stable_text(s, dev_rate)))
  } else {
    Probe::ok(Health::warn(issues.join("；")))
  }
}

fn value_of<T: Copy>(p: &Probe<T>) -> Option<T> {
  match p {
    Probe::Ok { value } => Some(*value),
    _ => None,
  }
}

fn push_rate(issues: &mut Vec<String>, name: &str, p: &Probe<f64>) {
  if let Some(v) = value_of(p)
    && v > EVENT_EPS
  {
    issues.push(format!("{name} {v:.1} 次/秒"));
  }
}

/// 一切正常时也要给数字：数都来自本路流，不猜、不填假值。
fn stable_text(s: &StreamInfo, dev_rate: Option<f64>) -> String {
  let rate = dev_rate
    .map(|v| format!("，设备侧 {v:.0} Hz"))
    .unwrap_or_default();
  if s.direction == "output" {
    format!("稳定（缓冲 {:.0} ms{rate}）", s.buffer_level_ms)
  } else {
    format!("稳定（无丢弃{rate}）")
  }
}
