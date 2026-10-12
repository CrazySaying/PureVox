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

//! 媒体控制中枢（DESIGN.md §4）：音效板 / 播放器的**运行时命令**与状态快照。
//!
//! 播放/停止/seek/音量不是计划参数（改它们不重建会话）：界面经 Tauri 命令把
//! [`MediaCmd`] 投递到按 (列, 行) 寻址的 [`MediaSlot`]，由列工作线程里的 Stage
//! 在自己的 `process` 内 drain 执行（音频面单线程，无锁竞争）。
//! 与 [`crate::net::NetHub`] 同款全局单例；会话构建时注册、停止时清空。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

/// 媒体行的运行时命令（界面 → Stage）。serde 派生供 Tauri 命令反序列化。
#[derive(Clone, Debug, serde::Deserialize)]
pub enum MediaCmd {
  /// 音效板：播放第 `index` 个音效（重播 = 旧声部淡出、新声部从头起播）
  SoundpadPlay(usize),
  /// 音效板：停止第 `index` 个音效（淡出）
  SoundpadStop(usize),
  /// 音效板：全部停止（淡出）
  SoundpadStopAll,
  /// 播放器：播放 / 续播
  PlayerPlay,
  /// 播放器：暂停（位置与缓冲保留）
  PlayerPause,
  /// 播放器：定位到 `sec` 秒
  PlayerSeek(f64),
  /// 行内增益（dB）。音效板：`index` = 音效序号（实时更新在播声部）；
  /// 播放器：`index` 忽略（整行一个音量）。
  Volume { index: Option<usize>, db: f64 },
  /// 行启用开关变化（列工作线程在 `enabled` 翻转时转发）：音效板淡出在播声部。
  Enabled(bool),
}

/// 一个音效的运行状态（供界面显示「在播」）。
#[derive(Clone, Debug, serde::Serialize)]
pub struct PadStatus {
  pub name: String,
  pub playing: bool,
  pub volume_db: f64,
}

/// 媒体行的状态快照（界面 2 Hz 轮询 `media_status`）。
#[derive(Clone, Debug, serde::Serialize)]
pub struct MediaStatus {
  /// `soundpad` / `player`
  pub kind: &'static str,
  /// 播放器：是否正在播放（未暂停）
  pub playing: bool,
  pub paused: bool,
  /// 播放位置（秒）
  pub pos: f64,
  /// 曲目时长（秒；未知为 0）
  pub dur: f64,
  /// 行级故障原因（文件缺失/格式不符等）；None = 正常
  pub error: Option<String>,
  /// 音效板：各音效状态
  pub pads: Vec<PadStatus>,
}

impl MediaStatus {
  pub fn new(kind: &'static str) -> Self {
    MediaStatus {
      kind,
      playing: false,
      paused: false,
      pos: 0.0,
      dur: 0.0,
      error: None,
      pads: Vec::new(),
    }
  }
}

/// 一个媒体行的槽位：Stage 持有它（drain 命令、更新状态），界面经中枢投递/读取。
pub struct MediaSlot {
  ctl: Mutex<VecDeque<MediaCmd>>,
  status: Mutex<MediaStatus>,
}

impl MediaSlot {
  pub fn new(kind: &'static str) -> Self {
    MediaSlot {
      ctl: Mutex::new(VecDeque::new()),
      status: Mutex::new(MediaStatus::new(kind)),
    }
  }

  /// 界面侧：投递一条命令（不阻塞音频线程）。
  pub fn send(&self, cmd: MediaCmd) {
    self.ctl.lock().unwrap().push_back(cmd);
  }

  /// 音频侧：取走当前全部命令。
  pub fn drain(&self) -> Vec<MediaCmd> {
    std::mem::take(&mut *self.ctl.lock().unwrap())
      .into_iter()
      .collect()
  }

  /// 音频侧：更新状态快照。
  pub fn set_status(&self, f: impl FnOnce(&mut MediaStatus)) {
    f(&mut self.status.lock().unwrap());
  }

  /// 界面侧：读取状态快照。
  pub fn status(&self) -> MediaStatus {
    self.status.lock().unwrap().clone()
  }
}

/// 全局媒体中枢：(列, 行) → 槽位。
pub struct MediaHub {
  slots: Mutex<HashMap<(usize, usize), Arc<MediaSlot>>>,
}

impl Default for MediaHub {
  fn default() -> Self {
    MediaHub {
      slots: Mutex::new(HashMap::new()),
    }
  }
}

impl MediaHub {
  /// 会话构建时注册（同一 (列,行) 重复注册 = 覆盖旧槽位）。
  pub fn register(&self, col: usize, row: usize, kind: &'static str) -> Arc<MediaSlot> {
    let slot = Arc::new(MediaSlot::new(kind));
    self.slots.lock().unwrap().insert((col, row), slot.clone());
    slot
  }

  /// 清空（会话停止 / 重建前）。
  pub fn clear(&self) {
    self.slots.lock().unwrap().clear();
  }

  /// 界面侧按 (列,行) 取槽位；未注册（未运行/该行不是媒体行）返回 None。
  pub fn slot(&self, col: usize, row: usize) -> Option<Arc<MediaSlot>> {
    self.slots.lock().unwrap().get(&(col, row)).cloned()
  }
}

static HUB: OnceLock<Arc<MediaHub>> = OnceLock::new();

/// 全局媒体中枢（与测试音 / 网络中枢同款单例）。
pub fn hub() -> Arc<MediaHub> {
  HUB.get_or_init(|| Arc::new(MediaHub::default())).clone()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn register_send_drain_roundtrip() {
    let h = MediaHub::default();
    let slot = h.register(1, 2, "soundpad");
    assert!(h.slot(1, 2).is_some());
    assert!(h.slot(0, 0).is_none());
    slot.send(MediaCmd::SoundpadPlay(0));
    slot.send(MediaCmd::SoundpadStopAll);
    let cmds = slot.drain();
    assert_eq!(cmds.len(), 2);
    assert!(slot.drain().is_empty(), "drain 后应取空");
    h.clear();
    assert!(h.slot(1, 2).is_none());
  }

  #[test]
  fn status_roundtrip() {
    let slot = MediaSlot::new("player");
    slot.set_status(|s| {
      s.playing = true;
      s.pos = 12.5;
      s.dur = 60.0;
    });
    let st = slot.status();
    assert_eq!(st.kind, "player");
    assert!(st.playing);
    assert_eq!(st.pos, 12.5);
  }
}
