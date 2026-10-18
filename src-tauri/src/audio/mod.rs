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

//! 音频子系统（DESIGN.md §1）：L0 设备与流 + 会话（多列）。
//!
//! 输入采集线程（原生→48k hop）→ 列工作线程（混合 + 处理链）→ 输出播放线程。

pub mod capture;
pub mod fanout;
pub mod health;
/// AEC 远端参考的输出回环采集（远端扬声器 → far 参考）。**实现按平台分文件**：
/// `loopback_windows.rs`（WASAPI loopback）/ `loopback_linux.rs`（PipeWire monitor，`parec`）/
/// `loopback_other.rs`（明确不可用）。
#[cfg(windows)]
#[path = "loopback_windows.rs"]
pub mod loopback;
#[cfg(target_os = "linux")]
#[path = "loopback_linux.rs"]
pub mod loopback;
#[cfg(not(any(windows, target_os = "linux")))]
#[path = "loopback_other.rs"]
pub mod loopback;
pub mod playback;
pub mod probe;
pub mod tone;
/// Linux 虚拟麦克风（PipeWire）的创建 / 移除（「驱动」页面）。
pub mod virtual_mic;

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::config;
use crate::debug::SharedHub;
use crate::engine::calib::CalibHub;
use crate::engine::session::Session;
use crate::plan::Plan;
use crate::recorder::RecorderHub;

/// 引擎内部采样率。
pub const SAMPLE_RATE: u32 = 48_000;
/// 10 ms hop，按时间派生（48 kHz 下 480 样本）。
pub const HOP: usize = (SAMPLE_RATE / 100) as usize;

/// Linux 上 Pulse / PipeWire 命令行工具（`parec` / `pacat` / `pactl`）缺失时的提示。
///
/// **只装了 PipeWire 的系统同样需要这个包**：`pipewire-pulse` 只提供兼容服务，而这些客户端
/// 工具在 `pulseaudio-utils` 里（Ubuntu 26.04 默认就不装它，实测回环采集直接
/// 「没有那个文件或目录」）。deb / rpm 已把它放进 `recommends`（apt/dnf 默认会装），
/// AppImage 装不了系统包，所以运行时的报错必须说清**该装什么**。
#[cfg(target_os = "linux")]
pub const PULSE_TOOLS_HINT: &str = "缺少 PulseAudio 命令行工具（parec / pacat / pactl）：Debian / Ubuntu 装 pulseaudio-utils，Fedora 装 pulseaudio-utils";

/// 打开设备时重试：Linux ALSA 的设备是**排他**的，而 cpal 关闭流是异步的
/// （真正的 `snd_pcm_close` 在 cpal 内部线程上），重建会话时紧接着打开会得到
/// 「读取默认格式失败：The requested device is temporarily busy」。
/// 只对「busy」类错误退避重试（最多 ~1.5 s），其它错误立即返回。
pub(crate) fn with_device_retry<T>(mut f: impl FnMut() -> Result<T, String>) -> Result<T, String> {
  let mut last = String::new();
  for attempt in 0..20 {
    match f() {
      Ok(v) => return Ok(v),
      Err(e) => {
        // 设备还在异步关闭（cpal 的 ALSA 真正 close 在它自己的线程上）时会看到
        // 「temporarily busy」或「not available / may have been disconnected」；
        // 都退避重试，别的错误立即返回。
        let retryable = ["busy", "Busy", "not available", "disconnected", "占用"];
        let busy = retryable.iter().any(|k| e.contains(k));
        last = e;
        if !busy {
          return Err(last);
        }
        // 退避 150 ms，给 cpal 把旧流真正关掉
        std::thread::sleep(std::time::Duration::from_millis(150));
        let _ = attempt;
      }
    }
  }
  Err(last)
}

/// 工作线程句柄：置停止标志并等待线程退出。
pub struct WorkerHandle {
  stop: Arc<AtomicBool>,
  join: Option<JoinHandle<()>>,
}

impl WorkerHandle {
  pub fn new(stop: Arc<AtomicBool>, join: JoinHandle<()>) -> Self {
    WorkerHandle {
      stop,
      join: Some(join),
    }
  }

  pub fn stop(mut self) {
    self.stop.store(true, Relaxed);
    if let Some(j) = self.join.take() {
      let _ = j.join();
    }
  }

  /// 线程**起不来**时的退化句柄：`stop` 已是停，join 可缺。
  /// 与 [`WorkerHandle::new`] 分开是因为 `new` 要求一定有 join 句柄。
  pub fn dead(stop: Arc<AtomicBool>) -> Self {
    WorkerHandle { stop, join: None }
  }
}

struct State {
  plan: Plan,
  /// 运行中才有会话；停止时为空
  session: Option<Session>,
  running: bool,
  /// 每次启停自增。校准「临时启动会话」的收尾据此判断：这期间有没有被用户或计划改动过
  /// ——改过就不能再由我们停掉（否则会把用户刚起的会话也停了）。
  epoch: u64,
}

/// 音频子系统：持有当前计划与由其构建的会话（可启动 / 停止）。
pub struct AudioManager {
  hub: SharedHub,
  rec: Arc<RecorderHub>,
  calib: Arc<CalibHub>,
  state: Mutex<State>,
  /// 重建串行化：界面连续改参数会并发调 `apply_plan`，两次重建若交错打开同一 ALSA 设备
  /// 就会「temporarily busy」（同一设备排他）。启动/停止也走这把锁。
  rebuild: Mutex<()>,
}

impl AudioManager {
  pub fn new(hub: SharedHub) -> Self {
    let plan = config::load_plan();
    let rec = Arc::new(RecorderHub::new(hub.clone()));
    let calib = Arc::new(CalibHub::new(hub.clone()));
    AudioManager {
      hub,
      rec,
      calib,
      state: Mutex::new(State {
        plan,
        session: None,
        running: false,
        epoch: 0,
      }),
      rebuild: Mutex::new(()),
    }
  }

  pub fn plan(&self) -> Plan {
    self.state.lock().unwrap().plan.clone()
  }

  pub fn is_running(&self) -> bool {
    self.state.lock().unwrap().running
  }

  /// 当前运行代号（每次启停 +1）。见 [`State::epoch`]。
  pub fn run_epoch(&self) -> u64 {
    self.state.lock().unwrap().epoch
  }

  /// 代号仍是 `expect` 且在运行 → 说明这期间没人动过引擎，我们可以安全收尾。
  fn untouched_since(&self, expect: u64) -> bool {
    let g = self.state.lock().unwrap();
    g.running && g.epoch == expect
  }

  /// 先释放旧会话：Linux 的 ALSA **同一设备不能被打开两次**，旧的还占着，新的
  /// `default_input_config/output_config` 就会报「读取默认格式失败」（Windows 的 WASAPI
  /// 共享模式容忍并发打开，所以那边没暴露）。重建/启动一律先停旧、再建新。
  fn release_session(&self) {
    let old = self.state.lock().unwrap().session.take();
    if let Some(mut s) = old {
      s.stop();
    }
  }

  /// 启动：按当前计划构建并运行会话。返回逐行问题。
  pub fn start(&self) -> Result<Vec<String>, String> {
    let _rebuild = self.rebuild.lock().unwrap();
    let plan = self.plan();
    self.release_session();
    let (session, problems) = Session::build(
      self.hub.clone(),
      &plan,
      self.rec.clone(),
      self.calib.clone(),
    );
    let mut g = self.state.lock().unwrap();
    g.session = Some(session);
    g.running = true;
    g.epoch += 1;
    drop(g);
    self.hub.set_running(true);
    Ok(problems)
  }

  /// 停止：停会话、释放设备。
  pub fn stop(&self) {
    let _rebuild = self.rebuild.lock().unwrap();
    let mut g = self.state.lock().unwrap();
    if let Some(mut s) = g.session.take() {
      s.stop();
    }
    g.running = false;
    g.epoch += 1;
    drop(g);
    self.hub.set_running(false);
    // 清掉列概要，界面显示「列未启动」
    self
      .hub
      .set_column(crate::debug::Probe::unavailable("列未启动".to_string()));
  }

  /// 录制 TSE 参考（降噪后、音量归一化，48 kHz 单声道）；返回目标文件路径。
  pub fn record_reference(&self, seconds: f64) -> Result<String, String> {
    let plan = self.plan();
    let path = config::default_tse_reference();
    self.rec.start(&plan, seconds, path.clone())?;
    Ok(path.display().to_string())
  }

  /// 测 AEC 延时：对计划里第一个 AEC 行采集 1.5 s，并向会话所有输出设备送扫频探针；
  /// 结果见 `/debug` 的 `calib`。
  ///
  /// **不需要先手动启动**：校准必须有列工作线程把「原始近端 hop + 远端窗口」喂进
  /// `calib::feed`（`engine/session.rs`），没有会话就没人喂。所以会话未运行时这里
  /// **临时启动**一个，第二个返回值是 `Some(运行代号)` 表示「本次是临时的，测完要收尾
  /// 停掉」——交给 [`Self::finish_temp_session`]（看门狗线程在 `main.rs`）。
  pub fn calibrate_aec_delay(&self) -> Result<(String, Option<u64>), String> {
    let plan = self.plan();
    let mut target = None;
    'outer: for (ci, c) in plan.columns.iter().enumerate() {
      for (ri, r) in c.rows.iter().enumerate() {
        if r.ptype == "echo_cancel" {
          target = Some((ci, ri));
          break 'outer;
        }
      }
    }
    let (ci, ri) = target.ok_or_else(|| "计划里没有 AEC 行；请先加一个再测延时".to_string())?;
    // 探针送到会话里所有输出设备：保证 far 与 mic 都能收到同一段扫频
    let mut outs: Vec<String> = Vec::new();
    for c in &plan.columns {
      for r in &c.rows {
        if r.kind == crate::plan::RowKind::Output
          && let Some(d) = r.device.as_deref().filter(|d| !d.is_empty())
          && !outs.iter().any(|x| x == d)
        {
          outs.push(d.to_string());
        }
      }
    }
    // 远端是输出回环时：探针必须送到被回环的那只输出（远端与 mic 听同一份，延时才有意义）。
    // Linux 用 `pacat` 直送 Pulse sink（cpal 打不开 sink 名）；Windows 目标本就是 cpal 输出端点。
    let far = plan.columns[ci].rows[ri]
      .params
      .get("far_device")
      .cloned()
      .unwrap_or_default();
    let mut far_sink: Option<String> = None;
    if let Some(id) = crate::engine::aec::far_loopback_id(&far) {
      if crate::audio::probe::NATIVE {
        // Linux：探针**只**从 far 的这只 sink 发出（探针发声的设备 = AEC 要消除的设备），
        // 不走 cpal 的输出（cpal 也打不开 Pulse sink 名）。
        far_sink = Some(id); // "" = 默认 sink
        outs.clear();
      } else {
        let target = if id.is_empty() {
          crate::devices::default_output_id()
        } else {
          Some(id)
        };
        if let Some(t) = target
          && !outs.iter().any(|x| x == &t)
        {
          outs.push(t);
        }
      }
    }
    if outs.is_empty() && far_sink.is_none() {
      return Err("没有输出设备可播放校准探针".into());
    }
    // 会话没在跑 → **临时启动**一个：校准要列线程喂数据，没有会话永远停在「采集中」。
    // 先把「有没有 AEC 行 / 有没有输出可放探针」这些前置条件查完再启动，避免白启一次。
    let temp_epoch = if self.is_running() {
      None
    } else {
      let problems = self.start()?;
      self.hub.push_ui(
        "info",
        format!(
          "校准需要会话在跑（列线程喂近端/远端），已临时启动{}",
          if problems.is_empty() {
            String::new()
          } else {
            format!("（有 {} 行未配置）", problems.len())
          }
        ),
      );
      Some(self.run_epoch())
    };
    // 固定参考延时（见 calib::CALIB_REF_MS）：校准测的是绝对延时，不依赖当前存储值
    if let Err(e) = self
      .calib
      .start(ci, ri, 1.6, 1000.0, crate::engine::calib::CALIB_REF_MS)
    {
      // 启动失败要把临时会话收回去，不能让它一直跑着
      if let Some(epoch) = temp_epoch
        && self.untouched_since(epoch)
      {
        self.stop();
      }
      return Err(e);
    }
    crate::engine::calib::play_probe(self.hub.clone(), outs, far_sink);
    Ok((
      format!("第 {} 列第 {} 行（探针已送出）", ci + 1, ri + 1),
      temp_epoch,
    ))
  }

  /// 校准是否还在进行中（采集中 **或** 正在算）——看门狗据此决定何时收尾。
  pub fn calib_in_flight(&self) -> bool {
    self.calib.in_flight()
  }

  /// 校准跑完后收尾：**只停自己临时启动的会话**。
  ///
  /// - `Some(epoch)`：本次是临时启动的，且代号没变（期间用户没启停过、计划也没重建）
  ///   → 停回未运行，保持用户原本的状态；
  /// - 期间用户自己点了「启动」（代号变了）→ **什么都不做**，不能把用户的会话停掉；
  /// - `None`：本来就是用户在跑的，不动。
  ///
  /// 调用方是看门狗线程：校准**有独立超时**（`calib` 内部 deadline + 这里的硬上限），
  /// 所以这里一定会被调到——不会出现「临时会话一直跑着」。
  pub fn finish_temp_session(&self, temp: Option<u64>) {
    let Some(epoch) = temp else {
      return;
    };
    if !self.untouched_since(epoch) {
      self
        .hub
        .push_ui("info", "校准期间引擎被改动过，保留当前运行状态".into());
      return;
    }
    self.stop();
    self
      .hub
      .push_ui("info", "校准完成，已停掉临时启动的会话".into());
  }

  /// 应用新计划：保存配置；运行中则重建会话（结构性变更），停止时只存计划。
  /// 返回构建过程中的问题（非致命，逐行展示）。应在阻塞线程调用（会打开/关闭设备）。
  pub fn apply_plan(&self, plan: Plan) -> Result<Vec<String>, String> {
    let _rebuild = self.rebuild.lock().unwrap();
    let _ = config::save_plan(&plan);
    let running = self.state.lock().unwrap().running;
    if !running {
      self.state.lock().unwrap().plan = plan;
      return Ok(vec![]);
    }
    // 先停旧会话（Linux ALSA 同一设备不能同时打开，见 `release_session`），再建新会话
    self.release_session();
    let (new_session, problems) = Session::build(
      self.hub.clone(),
      &plan,
      self.rec.clone(),
      self.calib.clone(),
    );
    let mut g = self.state.lock().unwrap();
    g.session = Some(new_session);
    g.plan = plan;
    Ok(problems)
  }

  /// 热参数落盘（**不重建会话**，DESIGN.md §5）：音量 / 续播位置等改动用它。
  /// 同时更新内存计划，保证 `get_plan` 与运行中实例一致。
  pub fn persist_plan(&self, plan: Plan) -> Result<(), String> {
    let _rebuild = self.rebuild.lock().unwrap();
    let _ = config::save_plan(&plan);
    self.state.lock().unwrap().plan = plan;
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 测试用的管理器：**自给自足的计划**，而且**绝不碰用户真实配置**。
  ///
  /// `AudioManager::new` 读的是真实 `~/.purevox/session.json`，直接用会让测试依赖本机
  /// 配置（本机若有 AEC 行 / 选了真实设备，断言就会飘）；更糟的是这里的
  /// `persist_plan` 会**写回**那个真实文件 —— 一次 `cargo test` 就把开发者的会话计划
  /// 覆盖成默认计划（2026-10-18 实际踩到）。所以先把 `config_dir` 指到临时目录。
  fn mgr() -> AudioManager {
    crate::config::use_test_dir();
    let m = AudioManager::new(crate::debug::DebugHub::new());
    m.persist_plan(Plan::default()).expect("写入测试计划");
    m
  }

  /// 临时会话的收尾纪律：`Some(epoch)` 只在**代号没变**时才停回原状。
  ///
  /// 校准期间用户自己点了「启动」就会推高代号；那时若还照停，会把用户刚起的会话
  /// 也停掉——这比「临时会话没收干净」严重得多。
  #[test]
  fn finish_temp_session_respects_run_epoch() {
    let m = mgr();
    assert!(!m.is_running(), "起点应是未运行");
    m.start().expect("启动会话");
    let epoch = m.run_epoch();
    assert!(m.untouched_since(epoch));

    // 用户在校准期间自己启停了一次 → 代号变了
    m.stop();
    m.start().expect("用户自己重新启动");
    assert_ne!(m.run_epoch(), epoch, "启停必须推高代号");
    assert!(!m.untouched_since(epoch), "代号变了就不能再由我们停");

    m.finish_temp_session(Some(epoch));
    assert!(m.is_running(), "期间被用户改动过 → 必须保留用户的运行状态");
    m.stop();
  }

  #[test]
  fn finish_temp_session_is_noop_for_user_started_session() {
    let m = mgr();
    m.start().expect("启动会话");
    // None = 本来就是用户在跑的会话，什么都不做
    m.finish_temp_session(None);
    assert!(m.is_running(), "不该停掉用户自己启动的会话");
    m.stop();
  }

  #[test]
  fn finish_temp_session_stops_untouched_temp_session() {
    let m = mgr();
    m.start().expect("临时启动");
    let epoch = m.run_epoch();
    m.finish_temp_session(Some(epoch));
    assert!(
      !m.is_running(),
      "临时会话测完必须停回未运行，不能一直占着音频设备"
    );
  }

  /// 校准**不必先手动启动**：没有 AEC 行时报的是「没有 AEC 行」而不是「会话未运行」，
  /// 说明前置检查排在临时启动之前，不会白启一次会话。
  #[test]
  fn calibrate_without_aec_row_reports_row_problem_not_running() {
    let m = mgr();
    // 计划由 `mgr()` 置成 `Plan::default()`（一条输入 + 一条输出、没有 AEC 行），
    // 而配置目录已被 `use_test_dir()` 指到临时目录 —— 测试**不读也不写**用户真实配置。
    let e = m.calibrate_aec_delay().expect_err("没有 AEC 行应当报错");
    assert!(e.contains("AEC"), "应报缺 AEC 行，实际：{e}");
    assert!(!m.is_running(), "前置检查失败不该留下运行中的会话");
  }
}
