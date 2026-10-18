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

//! 会话：按 [`Plan`] 构建并运行若干独立的列（DESIGN.md §3、§5）。
//!
//! 每列一个工作线程：系统时钟每 10 ms 自上而下执行该列的行——
//! 输入行取 1 hop 并按列内输入数等权混合、处理行就地处理、输出行把当前信号推给该行扇出。
//! 结构性变更（增删/排序行、换设备、改型号、增删列）由上层重建整个 Session。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rtrb::Consumer;

use super::aec::{AecRow, FAR_HIST_SAMPLES, FarHistory, FarWin, spawn_far_pump};
use super::calib::CalibHub;
use super::media::{self, MediaCmd, MediaSlot};
use super::registry::{self, ParamValue, Params};
use super::stage::{FrameContext, Stage, StageError};
use crate::audio::fanout::Fanout;
use crate::audio::{HOP, WorkerHandle, capture, loopback};
use crate::debug::SharedHub;
use crate::dsp::meter::RateMeter;
use crate::infer::aec::Aec;
use crate::plan::{ColumnSpec, Plan, RowKind};
use crate::recorder::{Phase, RecorderHub};

/// 列工作线程节拍（= 1 hop）。
const TICK: Duration = Duration::from_millis(10);
/// 列概要发布周期。
const PUBLISH: Duration = Duration::from_millis(200);
/// 行输入环水位：目标 2 hop，超过 4 hop 丢最旧（DESIGN.md §8）。
const ROW_TARGET: usize = 2 * HOP;
const ROW_CAP: usize = 4 * HOP;

struct Row {
  kind: RowKind,
  label: String,
  enabled: bool,
  /// 上一次的 enabled（列工作线程据此把开关翻转转发给媒体行）
  enabled_prev: bool,
  error: Option<String>,
  /// 未配置（如未选择设备）：不是错误，只是还没配好
  unconfigured: bool,
  /// 处理行：本行推理耗时 EMA / 最大（ms），进调试概要
  inf_ms: f32,
  inf_max_ms: f32,
  /// 输入行：本行源的 48k hop 读端
  input: Option<Consumer<f32>>,
  /// 输入行：行输入环累计丢弃的样本数（水位超 `ROW_CAP` 丢最旧）。
  ///
  /// 这个计数以前没有 ——源「一大块到达」时环被瞬间灌满、按策略丢最旧，**丢多少完全看不见**，
  /// 用户看到的只是「声音规律地卡」（2026-10-18 实测：Linux 回环 100 ms 一块，每块丢掉大半）。
  ring_dropped: u64,
  /// 行输入环丢弃的实测速率（3 s 窗口；累计只涨不降，速率才看得出还在不在丢）
  ring_drop_rate: RateMeter,
  /// 输入行（loopback）：行内增益（系统混音偏响，默认 -6 dB）——**热参数**
  /// （`engine/hot.rs`：界面改它只写这个单元格，不重建会话）
  gain: Option<Arc<crate::engine::hot::HotGain>>,
  /// 本行绑的设备 ID（重启用它按需取回设备租约；输入/输出行才有）
  device: Option<String>,
  /// 本行的采集是否**池化**（普通采集行 = 是；AEC 行 / 回环行各自持有采集路径，见 §6.5 待做）
  pooled_input: bool,
  /// 本行的输出是否池化（普通输出行 = 是）
  pooled_output: bool,
  /// 输入行的设备租约（池持有设备，行只拿这个；停用即放掉 → 引用归零才真关）
  in_lease: Option<crate::audio::device_pool::InputLease>,
  /// 输出行的设备租约（内含该设备唯一那条混音的这一路源）
  out_lease: Option<crate::audio::device_pool::OutputLease>,
  /// AEC 输入行：mic 读端 + far 历史 + 本行模型状态
  aec: Option<AecRow>,
  /// `remote_mic` 输入行：网络入站队列（DESIGN.md §4.1）
  net_input: Option<Arc<crate::net::NetHub>>,
  /// 处理行
  stage: Option<Box<dyn Stage>>,
  /// 媒体行（soundpad / player）：运行时命令通道（界面经 MediaHub 寻址）
  media_slot: Option<Arc<MediaSlot>>,
  /// 输出行：本行扇出（该行的播放线程从这里订阅）
  output: Option<Arc<Fanout>>,
  /// `remote_speaker` 输出行：推给网络中枢（无扇出、无播放线程）
  net_output: bool,
}

struct ColumnRuntime {
  summary: Arc<Mutex<String>>,
  stop: Arc<AtomicBool>,
  join: Option<JoinHandle<()>>,
  /// 本列拥有的流：Session 停止时一并关闭
  captures: Vec<WorkerHandle>,
  playbacks: Vec<WorkerHandle>,
}

/// 一个已构建并正在运行的会话。
pub struct Session {
  columns: Vec<ColumnRuntime>,
  publisher_stop: Arc<AtomicBool>,
  publisher: Option<JoinHandle<()>>,
  /// 热参数单元格：`(列, 行, 参数名) → 值`。`set_hot_param` 写它，行工作线程下一 hop 生效
  /// （见 `engine/hot.rs`：热参数**不重建会话**）。
  hot: std::collections::HashMap<(usize, usize, String), Arc<crate::engine::hot::HotGain>>,
}

impl Session {
  /// 按计划构建并启动；返回构建过程中收集到的问题（非致命）。不修改计划本身。
  pub fn build(
    hub: SharedHub,
    plan: &Plan,
    rec: Arc<RecorderHub>,
    calib: Arc<CalibHub>,
  ) -> (Session, Vec<String>) {
    let mut problems = crate::plan::validate(plan);
    // 媒体槽位按 (列,行) 重建：先清空旧会话的注册，避免行被删后残留
    media::hub().clear();
    let mut columns = Vec::new();
    let mut hot = std::collections::HashMap::new();
    for (i, spec) in plan.columns.iter().enumerate() {
      columns.push(build_column(
        &hub,
        i,
        spec,
        &mut problems,
        &rec,
        &calib,
        &mut hot,
      ));
    }

    // 汇总各列概要，定期发布到调试接口
    let summaries: Vec<Arc<Mutex<String>>> = columns.iter().map(|c| c.summary.clone()).collect();
    let publisher_stop = Arc::new(AtomicBool::new(false));
    let (ps, hub2) = (publisher_stop.clone(), hub.clone());
    let publisher = std::thread::Builder::new()
      .name("session-publish".into())
      .spawn(move || {
        while !ps.load(Relaxed) {
          let parts: Vec<String> = summaries
            .iter()
            .map(|s| s.lock().unwrap().clone())
            .collect();
          hub2.set_column(crate::debug::Probe::ok(parts.join(" ｜ ")));
          std::thread::sleep(Duration::from_millis(200));
        }
      })
      .ok();

    (
      Session {
        columns,
        publisher_stop,
        publisher,
        hot,
      },
      problems,
    )
  }

  /// 运行中改一个热参数（见 `engine/hot.rs`）：写单元格，行工作线程**下一 hop**生效。
  /// 计划里没有这个 `(列,行,参数名)` → `None`（调用方据此报「这一行没有这个热参数」）。
  pub fn set_hot_param(&self, col: usize, row: usize, key: &str, value: f32) -> Option<()> {
    let cell = self.hot.get(&(col, row, key.to_string()))?;
    cell.set_db(value);
    Some(())
  }

  pub fn stop(&mut self) {
    self.publisher_stop.store(true, Relaxed);
    if let Some(j) = self.publisher.take() {
      let _ = j.join();
    }
    for c in &mut self.columns {
      c.stop.store(true, Relaxed);
      if let Some(j) = c.join.take() {
        let _ = j.join();
      }
      for h in c.playbacks.drain(..) {
        h.stop();
      }
      for h in c.captures.drain(..) {
        h.stop();
      }
    }
    self.columns.clear();
    // 媒体槽位随会话释放（界面再发命令只会得到「未运行」）
    media::hub().clear();
  }
}

/// 网络麦克风行勾了 AEC 时要用的参数（一包，免得 `build_net_aec_row` 参数列太长）。
struct NetAecParams {
  model: String,
  far: String,
  delay_ms: f64,
  mic_gain: f32,
  far_gain: f32,
  bypass: bool,
}

impl NetAecParams {
  fn from(row: &crate::plan::RowSpec) -> Self {
    let num = |k: &str| {
      row
        .params
        .get(k)
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(0.0)
    };
    Self {
      model: row
        .params
        .get("model")
        .cloned()
        .unwrap_or_else(|| crate::infer::aec::MODEL_AEC.to_string()),
      far: row
        .params
        .get("far_device")
        .cloned()
        .unwrap_or_else(|| "loopback".into()),
      delay_ms: num("far_delay_ms"),
      mic_gain: 10f32.powf((num("mic_gain_db") / 20.0) as f32),
      far_gain: 10f32.powf((num("far_gain_db") / 20.0) as f32),
      bypass: row
        .params
        .get("bypass")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false),
    }
  }
}

/// 加载模型并挂到行上（网络 AEC 用）。近端固定取 `near` 扇出 —— 网络麦克风的近端
/// 就是它自己，卡片上不可改。
fn aec_attach(
  row: &mut Row,
  p: &NetAecParams,
  hist: Arc<Mutex<FarHistory>>,
  near: &Arc<Fanout>,
) -> Result<(), String> {
  let engine = Aec::load(&p.model)?;
  row.aec = Some(AecRow {
    engine,
    hist,
    // 延时取整到 10 ms（= 1 hop 的整数倍），far 窗口与 hop 网格对齐
    delay_samples: (p.delay_ms / 10.0).round() as i64 * HOP as i64,
    mic_hops: 0,
    mic: near.subscribe("net-mic"),
    mic_gain: p.mic_gain,
    far_gain: p.far_gain,
    bypass: p.bypass,
    exact: 0,
    latest: 0,
    pass: 0,
    echo: crate::engine::aec::EchoMetrics::default(),
  });
  Ok(())
}

/// 网络麦克风行勾了 AEC（`params.aec == "1"`）时建行：近端 = 网络入站（锁死），
/// 远端按 `far_device` 取。
///
/// 近/远端都经一层 `Fanout`（`spawn_net_pumps`），所以这一支和 `echo_cancel` 设备行
/// 完全同构 —— 差别只是扇出的内容来自网络而不是采集设备，`AecRow` 一个字段都不用改。
///
/// far 选设备回环时**不能**拿网络泵那个没人灌的 far 扇出当参考（历史一直空，AEC 会
/// 以为没有回声、静默失效），所以那种情况改用设备回环自己的扇出。
fn build_net_aec_row(
  row: &mut Row,
  spec: &crate::plan::RowSpec,
  hub: &SharedHub,
  captures: &mut Vec<WorkerHandle>,
  epoch: Arc<std::sync::atomic::AtomicU64>,
  tag: String,
) {
  let p = NetAecParams::from(spec);
  let net_far = p.far == crate::engine::aec::NET_FAR;
  let near_fan = Arc::new(Fanout::new(format!("{tag}-netmic")));
  let net = crate::net::hub();

  // —— 情况 1：远端取网络出站。近/远端由同一个网络泵在同一次 tick 各推一格，——
  //    所以两条时间轴天然锁在一起，`far_delay_ms` 才有个稳定基准。
  if net_far {
    let far_fan = Arc::new(Fanout::new(format!("{tag}-netfar")));
    let hist = Arc::new(Mutex::new(FarHistory::new(FAR_HIST_SAMPLES)));
    captures.push(crate::engine::aec::spawn_far_pump(
      &far_fan,
      hist.clone(),
      &tag,
      epoch,
    ));
    match aec_attach(row, &p, hist, &near_fan) {
      Ok(()) => {
        captures.push(crate::engine::aec::spawn_net_pumps(net, near_fan, far_fan));
        row.label = "网络麦克风 + AEC（远端 网络出站）".into();
      }
      Err(e) => row.error = Some(e),
    }
    return;
  }

  // —— 情况 2：远端取设备（输出回环，或直接填了一个输入设备）——
  let opened: Result<crate::audio::capture::Capture, String> =
    match crate::engine::aec::far_loopback_id(&p.far) {
      Some(id) => loopback::spawn(
        hub.clone(),
        if id.is_empty() { None } else { Some(id) },
        format!("{tag}-f"),
      ),
      None => Err("未选择远端设备".into()),
    };
  let cap = match opened {
    Ok(c) => {
      captures.push(c.handle);
      c.fanout
    }
    Err(_) => match capture::spawn(hub.clone(), p.far.clone(), format!("{tag}-f")) {
      Ok(c) => {
        captures.push(c.handle);
        c.fanout
      }
      Err(e) => {
        row.error = Some(e);
        return;
      }
    },
  };
  let hist = Arc::new(Mutex::new(FarHistory::new(FAR_HIST_SAMPLES)));
  captures.push(crate::engine::aec::spawn_far_pump(
    &cap,
    hist.clone(),
    &tag,
    epoch,
  ));
  match aec_attach(row, &p, hist, &near_fan) {
    Ok(()) => {
      // far 由设备提供，只让网络泵灌近端；第二个扇出没人灌、不参与
      captures.push(crate::engine::aec::spawn_net_pumps(
        net,
        near_fan,
        Arc::new(Fanout::new(format!("{tag}-unused-far"))),
      ));
      row.label = "网络麦克风 + AEC".into();
    }
    Err(e) => row.error = Some(e),
  }
}

fn to_params(map: &BTreeMap<String, String>) -> Params {
  map
    .iter()
    .map(|(k, v)| (k.clone(), ParamValue::Text(v.clone())))
    .collect()
}

/// 从行输入环取 1 hop（超上限丢最旧到目标）；不足一 hop 返回 false。
/// 行输入环丢弃注记（空串 = 没丢过）。
///
/// **丢了不报 = 只剩一句「声音卡」**：丢弃发生在 10 ms 网格之外，累计数只涨不降，
/// 所以速率一起给（速率才看得出「现在还在不在丢」）。
fn ring_drop_note(dropped: u64, rate: &crate::debug::Probe<f64>) -> String {
  if dropped == 0 {
    return String::new();
  }
  match rate {
    crate::debug::Probe::Ok { value } => format!("，环丢 {dropped}（{value:.0}/s）"),
    _ => format!("，环丢 {dropped}"),
  }
}

/// 从行输入环取一个 hop。
///
/// 水位超过 [`ROW_CAP`] 就丢最旧、降到 [`ROW_TARGET`]（DESIGN.md §8）—— 顺带把丢了多少
/// 报出来：丢样本发生在 10 ms 网格之外，**不报出来就只是一句「声音卡」**。
/// 返回 `(是否取到 hop, 本轮丢弃的样本数)`。
fn take_hop(cons: &mut Consumer<f32>, out: &mut [f32; HOP]) -> (bool, usize) {
  let mut dropped = 0usize;
  if cons.slots() > ROW_CAP {
    let drop = cons.slots() - ROW_TARGET;
    if let Ok(c) = cons.read_chunk(drop) {
      c.commit_all();
      dropped = drop;
    }
  }
  if cons.slots() >= HOP
    && let Ok(c) = cons.read_chunk(HOP)
  {
    let (a, b) = c.as_slices();
    out[..a.len()].copy_from_slice(a);
    out[a.len()..].copy_from_slice(b);
    c.commit_all();
    return (true, dropped);
  }
  (false, dropped)
}

/// 输入行的设备租约：启用就按需取回、停用就放掉（**一处实现**，建会话与运行中开关共用）。
///
/// 这就是「列/行开关 = 订阅与引用的挂摘」（DESIGN.md §6.5）：停用不删行、保留设备选择，
/// 只放掉这一份引用；引用归零时池才真关设备（异步真关，`WorkerHandle::stop` 会 join）。
fn apply_input_lease(r: &mut Row, hub: &SharedHub, idx: usize, ri: usize) {
  if !r.pooled_input {
    return;
  }
  let Some(dev) = r.device.clone() else {
    return;
  };
  if r.enabled {
    if r.in_lease.is_none() {
      match crate::audio::device_pool::hub().acquire_input(hub.clone(), &dev) {
        Ok(lease) => {
          r.input = Some(lease.subscribe(&format!("col{idx}row{ri}")));
          r.in_lease = Some(lease);
          r.error = None;
        }
        Err(e) => r.error = Some(e),
      }
    }
  } else if r.in_lease.is_some() {
    r.in_lease = None;
    r.input = None;
  }
}

/// 输出行的设备租约（同上；顺带把这一列接进该设备**唯一**那条混音）。
fn apply_output_lease(r: &mut Row, hub: &SharedHub, idx: usize, ri: usize) {
  if !r.pooled_output {
    return;
  }
  let Some(dev) = r.device.clone() else {
    return;
  };
  if r.enabled {
    if r.out_lease.is_none() {
      match crate::audio::device_pool::hub().acquire_output(hub.clone(), &dev) {
        Ok(lease) => {
          r.output = Some(lease.out.clone());
          r.out_lease = Some(lease);
          r.error = None;
        }
        Err(e) => r.error = Some(e),
      }
    }
  } else if r.out_lease.is_some() {
    r.out_lease = None;
    r.output = None;
  }
  let _ = (idx, ri);
}

fn build_column(
  hub: &SharedHub,
  idx: usize,
  spec: &ColumnSpec,
  problems: &mut Vec<String>,
  rec: &Arc<RecorderHub>,
  calib: &Arc<CalibHub>,
  hot: &mut std::collections::HashMap<(usize, usize, String), Arc<crate::engine::hot::HotGain>>,
) -> ColumnRuntime {
  let mut rows: Vec<Row> = Vec::new();
  let mut captures = Vec::new();
  let playbacks = Vec::new();
  // `remote_mic` 入站是单队列，全会话只允许一行（SPEC 约束）
  let mut remote_mic_used = false;

  // 列头开关：整列停用 = 这一列不产出、也不占设备引用（行级开关照旧各自生效）
  let column_on = spec.enabled;
  for (ri, r) in spec.rows.iter().enumerate() {
    // 热参数登记口：本行建出来的单元格（增益 / 音量）收进会话，供运行中改值
    let mut sink = crate::engine::hot::HotSink::new();
    let mut row = Row {
      kind: r.kind,
      label: r.ptype.clone(),
      enabled: r.enabled && column_on,
      enabled_prev: r.enabled,
      error: None,
      unconfigured: false,
      inf_ms: 0.0,
      inf_max_ms: 0.0,
      input: None,
      gain: None,
      device: None,
      pooled_input: false,
      pooled_output: false,
      in_lease: None,
      out_lease: None,
      aec: None,
      net_input: None,
      ring_dropped: 0,
      ring_drop_rate: RateMeter::new(),
      stage: None,
      media_slot: None,
      output: None,
      net_output: false,
    };
    let device = r.device.as_deref().filter(|d| !d.is_empty());
    match r.kind {
      RowKind::Input if r.ptype == "echo_cancel" => {
        // AEC 输入行：本行 device = mic，params.far_device = 远端参考（回环或另一路输入）。
        let far = r.params.get("far_device").cloned().unwrap_or_default();
        let delay_ms: f64 = r
          .params
          .get("far_delay_ms")
          .and_then(|v| v.trim().parse().ok())
          .unwrap_or(0.0);
        let model = r
          .params
          .get("model")
          .cloned()
          .unwrap_or_else(|| crate::infer::aec::MODEL_AEC.to_string());
        let db = |k: &str| {
          r.params
            .get(k)
            .and_then(|v| v.trim().parse::<f64>().ok())
            .unwrap_or(0.0)
        };
        let mic_gain = 10f32.powf((db("mic_gain_db") / 20.0) as f32);
        let far_gain = 10f32.powf((db("far_gain_db") / 20.0) as f32);
        let bypass = r
          .params
          .get("bypass")
          .map(|v| {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true")
          })
          .unwrap_or(false);
        match device {
          None => row.unconfigured = true,
          Some(mic_dev) => {
            // 远端：扬声器（输出设备）→ 自动 WASAPI 回环；也可填输入设备当参考
            let far_loop = crate::engine::aec::far_loopback_id(&far);
            let mic = crate::devices::find(mic_dev, true);
            let far_ok: Result<String, String> = if let Some(id) = &far_loop {
              if id.is_empty() {
                Ok("默认输出回环".to_string())
              } else {
                Ok(
                  crate::devices::find(id, false)
                    .map(|(_, n)| n)
                    .unwrap_or_else(|_| "输出回环".to_string()),
                )
              }
            } else if far.is_empty() {
              Err("未选择远端设备".to_string())
            } else {
              crate::devices::find(&far, true).map(|(_, n)| n)
            };
            match (mic, far_ok) {
              (Ok((_, mic_name)), Ok(far_name)) => {
                match capture::spawn(hub.clone(), mic_dev.to_string(), format!("c{idx}r{ri}m")) {
                  Ok(mic_cap) => {
                    let far_cap = match &far_loop {
                      Some(id) => loopback::spawn(
                        hub.clone(),
                        if id.is_empty() {
                          None
                        } else {
                          Some(id.clone())
                        },
                        format!("c{idx}r{ri}f"),
                      ),
                      None => capture::spawn(hub.clone(), far.clone(), format!("c{idx}r{ri}f")),
                    };
                    match far_cap {
                      Ok(far_cap) => {
                        let hist = Arc::new(Mutex::new(FarHistory::new(FAR_HIST_SAMPLES)));
                        let pump = spawn_far_pump(
                          &far_cap.fanout,
                          hist.clone(),
                          &format!("c{idx}r{ri}"),
                          calib.epoch(),
                        );
                        match Aec::load(&model) {
                          Ok(engine) => {
                            row.label = format!("AEC：{mic_name}（远端 {far_name}）");
                            row.aec = Some(AecRow {
                              engine,
                              hist,
                              // 延时取整到 10 ms（= 1 hop 的整数倍），far 窗口与 hop 网格对齐
                              delay_samples: (delay_ms / 10.0).round() as i64 * HOP as i64,
                              mic_hops: 0,
                              mic: mic_cap.fanout.subscribe(&format!("c{idx}r{ri}-mic")),
                              mic_gain,
                              far_gain,
                              bypass,
                              exact: 0,
                              latest: 0,
                              pass: 0,
                              echo: crate::engine::aec::EchoMetrics::default(),
                            });
                          }
                          Err(e) => row.error = Some(e),
                        }
                        captures.push(mic_cap.handle);
                        captures.push(far_cap.handle);
                        captures.push(pump);
                      }
                      Err(e) => {
                        captures.push(mic_cap.handle);
                        row.error = Some(e);
                      }
                    }
                  }
                  Err(e) => row.error = Some(e),
                }
              }
              (Err(e), _) | (_, Err(e)) => row.error = Some(e),
            }
          }
        }
      }
      RowKind::Input if r.ptype == "tone" => {
        // 测试音源（全局共享线程），无需设备
        row.label = "测试音 1 kHz".into();
        row.input = Some(crate::audio::tone::shared().subscribe(&format!("col{idx}row{ri}")));
      }
      RowKind::Input if r.ptype == "remote_mic" => {
        // 网络麦克风：不需要虚拟声卡驱动，网络流直接进引擎（DESIGN.md §4.1）
        // 允许一个：入站是单队列，多行会互相抢样本
        //
        // **勾了 AEC（`params.aec == "1"`）时**（2026-10-12 加）：不再是直通，而是建
        // 一张 AEC 行 —— 近端锁死为网络（卡片上不可改，这正是这张卡的意义），远端按
        // `far_device` 取。近/远端各经一层 `Fanout`（`spawn_net_pumps`），所以 AEC
        // 主循环与设备麦克风那条路完全同构。
        if remote_mic_used {
          row.error = Some("网络麦克风只允许一个输入行".into());
        } else {
          remote_mic_used = true;
          if r.params.get("aec").map(|v| v == "1").unwrap_or(false) {
            build_net_aec_row(
              &mut row,
              r,
              hub,
              &mut captures,
              calib.epoch(),
              format!("c{idx}r{ri}"),
            );
          } else {
            row.label = "网络麦克风".into();
            row.net_input = Some(crate::net::hub().clone());
          }
        }
      }
      RowKind::Input if r.ptype == "loopback" => {
        // 电脑播放音频：捕获输出设备正在播放的系统混音（复用 AEC 的回环采集）。
        // `device` = `loopback`（默认输出）/ `loopback:<端点ID｜sink>`；行内增益默认 -6 dB。
        let db: f64 = r
          .params
          .get("volume_db")
          .and_then(|v| v.trim().parse().ok())
          .unwrap_or(-6.0);
        row.gain = Some(sink.gain("volume_db", db));
        let target = match device {
          None => None,
          Some(d) if d == "loopback" || d.is_empty() => None,
          Some(d) => Some(d.strip_prefix("loopback:").unwrap_or(d).to_string()),
        };
        let name = match &target {
          None => "系统默认输出".to_string(),
          Some(id) => crate::devices::find(id, false)
            .map(|(_, n)| n)
            .unwrap_or_else(|_| id.clone()),
        };
        row.label = format!("电脑播放音频（{name}）");
        match loopback::spawn(hub.clone(), target, format!("c{idx}r{ri}")) {
          Ok(cap) => {
            row.input = Some(cap.fanout.subscribe(&format!("col{idx}row{ri}")));
            captures.push(cap.handle);
          }
          Err(e) => row.error = Some(e),
        }
      }
      RowKind::Input => match device {
        None => row.unconfigured = true,
        Some(dev) => match crate::devices::find(dev, true) {
          Err(e) => row.error = Some(e),
          Ok((_, name)) => {
            row.label = name;
            // 设备由**池**持有（同一设备多列共用一条采集流），行只拿租约（DESIGN.md §6.5）
            row.device = Some(dev.to_string());
            row.pooled_input = true;
            crate::engine::session::apply_input_lease(&mut row, hub, idx, ri);
          }
        },
      },
      RowKind::Process => {
        // 媒体行（音效板 / 播放器）：注册 (列,行) 槽位，供界面投递运行时命令
        let slot = match r.ptype.as_str() {
          "soundpad" => Some(media::hub().register(idx, ri, "soundpad")),
          "player" => Some(media::hub().register(idx, ri, "player")),
          _ => None,
        };
        row.media_slot = slot.clone();
        match registry::create_stage(&r.ptype, &to_params(&r.params), slot, &mut sink) {
          Ok(st) => {
            row.label = registry::get_spec(&r.ptype)
              .map(|s| s.label.to_string())
              .unwrap_or(r.ptype.clone());
            row.stage = Some(st);
          }
          Err(e) => row.error = Some(e),
        }
      }
      RowKind::Output if r.ptype == "remote_speaker" => {
        // 网络扬声器：按位置 tap 取信号，交给网络中枢广播给订阅客户端
        row.label = "网络扬声器".into();
        row.net_output = true;
      }
      RowKind::Output => match device {
        None => row.unconfigured = true,
        Some(dev) => match crate::devices::find(dev, false) {
          Err(e) => row.error = Some(e),
          Ok((_, name)) => {
            row.label = name;
            // 输出也由池持有：一个设备**一条**混音，本行只是往里登记一路源（DESIGN.md §6.5）
            row.device = Some(dev.to_string());
            row.pooled_output = true;
            crate::engine::session::apply_output_lease(&mut row, hub, idx, ri);
          }
        },
      },
    }
    if let Some(e) = &row.error {
      problems.push(format!("第 {} 列第 {} 行：{e}", idx + 1, ri + 1));
    }
    // 热参数单元格收进会话（(列,行,参数名) 寻址；见 `engine/hot.rs`）
    for (key, cell) in sink.take() {
      hot.insert((idx, ri, key), cell);
    }
    rows.push(row);
  }

  let rows = Arc::new(Mutex::new(rows));
  let summary = Arc::new(Mutex::new(format!("列 {}：构建中", idx + 1)));
  let stop = Arc::new(AtomicBool::new(false));
  let (rows2, sum2, stop2) = (rows.clone(), summary.clone(), stop.clone());
  let rec2 = rec.clone();
  let calib2 = calib.clone();
  let hub3 = hub.clone();
  let join = std::thread::Builder::new()
    .name(format!("column-{}", idx + 1))
    .spawn(move || run_column(idx, rows2, sum2, stop2, rec2, calib2, hub3))
    .ok();

  ColumnRuntime {
    summary,
    stop,
    join,
    captures,
    playbacks,
  }
}

fn run_column(
  idx: usize,
  rows: Arc<Mutex<Vec<Row>>>,
  summary: Arc<Mutex<String>>,
  stop: Arc<AtomicBool>,
  rec: Arc<RecorderHub>,
  calib: Arc<CalibHub>,
  hub: SharedHub,
) {
  let ctx = FrameContext::default();
  let mut tmp = [0.0f32; HOP];
  let mut last_publish = Instant::now() - PUBLISH;
  // 绝对时刻节拍：每轮对齐到 10 ms 网格，消除 sleep 过冲的累积漂移
  // （固定 sleep(10ms-work) 会每轮多睡一点，实测产 hop 只有 ~96/s，输出侧 ±3% 伺服补不回来 → 周期性欠载）。
  let mut next = Instant::now() + TICK;
  let mut ticks: u64 = 0;
  let mut last_ticks: u64 = 0;
  // 重置代号：会话起步 / 每次校准都让 far 网格与 mic 序号从同一原点计时
  let calib_epoch = calib.epoch();
  let mut epoch_seen = calib_epoch.load(Relaxed);
  let mut aec_boot = true;

  while !stop.load(Relaxed) {
    ticks += 1;
    let mut acc = [0.0f32; HOP];
    {
      let mut g = rows.lock().unwrap();
      let n_inputs = g
        .iter()
        .filter(|r| {
          r.kind == RowKind::Input
            && (r.input.is_some() || r.aec.is_some() || r.net_input.is_some())
        })
        .count();
      let k = 1.0 / n_inputs.max(1) as f32;
      let recording = rec.active();
      let calibrating = calib.active();

      for (ri, r) in g.iter_mut().enumerate() {
        // 行启用开关翻转 → 转发给媒体行（音效板淡出在播声部；播放器硬旁路）
        if r.enabled != r.enabled_prev {
          r.enabled_prev = r.enabled;
          if let Some(slot) = r.media_slot.as_ref() {
            slot.send(MediaCmd::Enabled(r.enabled));
          }
        }
        match r.kind {
          RowKind::Input => {
            // 启用/停用：挂摘设备租约（停用即释放引用；重新启用可能失败 → 明确原因）
            apply_input_lease(r, &hub, idx, ri);
            if let Some(aec) = r.aec.as_mut() {
              // AEC 行：mic hop 与 far 窗口对齐后过模型；far 历史不足则直通 mic
              let (got, dropped) = take_hop(&mut aec.mic, &mut tmp);
              r.ring_dropped += dropped as u64;
              if got {
                // 会话起步：让 far 泵的时钟与 mic 序号同起点（否则两者相差设备打开时间，
                // 校准会测出负延时且每次会话不同）；校准开始时代号变化 → 序号同样归零。
                if aec_boot {
                  aec_boot = false;
                  calib_epoch.fetch_add(1, Relaxed);
                }
                let e = calib_epoch.load(Relaxed);
                if e != epoch_seen {
                  epoch_seen = e;
                  aec.mic_hops = 0;
                }
                let hop_idx = aec.mic_hops;
                aec.mic_hops += 1;
                if aec.bypass {
                  // 直通：跳过 AEC，直接过 mic（仍应用近端增益）
                  if aec.mic_gain != 1.0 {
                    for x in tmp.iter_mut() {
                      *x *= aec.mic_gain;
                    }
                  }
                  for i in 0..HOP {
                    acc[i] += tmp[i] * k;
                  }
                } else {
                  // 校准时用固定参考延时取 far 窗口（与存储值无关，见 calib::CALIB_REF_MS）
                  let delay_samples = if calibrating {
                    crate::engine::calib::CALIB_REF_SAMPLES
                  } else {
                    aec.delay_samples
                  };
                  let start = hop_idx as i64 * HOP as i64 - delay_samples;
                  let mut far = [0.0f32; HOP];
                  let win = {
                    let h = aec.hist.lock().unwrap();
                    h.window_or_latest(start, &mut far)
                  };
                  // 校准：喂「原始近端 + 当前延时下的远端窗口」（与运行同一坐标、且在增益前）。
                  // 只让精确窗口参与测量：回退段没有按延时偏移，量到的是绝对偏移，会把延时
                  // 越加越大（far 网格已按实时推进，精确窗口正常可用；一直不可用由 calib 超时兜底）。
                  if calibrating {
                    calib.feed(idx, ri, &tmp, &far, win == FarWin::Exact);
                  }
                  // 端侧增益（进模型前）
                  if aec.mic_gain != 1.0 {
                    for x in tmp.iter_mut() {
                      *x *= aec.mic_gain;
                    }
                  }
                  if aec.far_gain != 1.0 {
                    for x in far.iter_mut() {
                      *x *= aec.far_gain;
                    }
                  }
                  match win {
                    FarWin::None => {
                      aec.pass += 1;
                      for i in 0..HOP {
                        acc[i] += tmp[i] * k;
                      }
                    }
                    w => {
                      if w == FarWin::Latest {
                        aec.latest += 1;
                      } else {
                        aec.exact += 1;
                      }
                      match aec.engine.process(&tmp, &far) {
                        Ok(out) => {
                          aec.echo.push(&tmp, &far, out);
                          for i in 0..HOP {
                            acc[i] += out[i] * k;
                          }
                        }
                        Err(e) => r.error = Some(e),
                      }
                    }
                  }
                }
              }
            } else if let Some(net) = r.net_input.as_ref() {
              // 网络麦克风：按引擎 10 ms 节拍从网络队列取一个 hop（欠载补静音）
              net.take_rx_hop(&mut tmp);
              for i in 0..HOP {
                acc[i] += tmp[i] * k;
              }
            } else if let Some(cons) = r.input.as_mut() {
              let (got, dropped) = take_hop(cons, &mut tmp);
              r.ring_dropped += dropped as u64;
              if got {
                // 行内增益（loopback 源：系统混音偏响）
                if let Some(g) = r.gain.as_ref() {
                  let g = g.linear();
                  for x in tmp.iter_mut() {
                    *x *= g;
                  }
                }
                for i in 0..HOP {
                  acc[i] += tmp[i] * k;
                }
              }
            }
          }
          RowKind::Process => {
            if r.enabled
              && let Some(st) = r.stage.as_mut()
            {
              // 录音采样点：处理前 = 上游输出（TSE 行的输入即降噪后）
              if recording {
                rec.tap(idx, ri, Phase::Pre, &acc);
              }
              let t = Instant::now();
              if let Err(StageError::Fatal(m)) = st.process(&mut acc, &ctx) {
                r.error = Some(m);
              }
              let ms = t.elapsed().as_secs_f32() * 1000.0;
              r.inf_ms = if r.inf_ms == 0.0 {
                ms
              } else {
                r.inf_ms * 0.9 + ms * 0.1
              };
              if ms > r.inf_max_ms {
                r.inf_max_ms = ms;
              }
              if recording {
                rec.tap(idx, ri, Phase::Post, &acc);
              }
            }
          }
          RowKind::Output => {
            // 启用/停用：挂摘设备租约（停用即从混音注销 + 释放引用）
            apply_output_lease(r, &hub, idx, ri);
            if r.enabled {
              if let Some(f) = r.output.as_ref() {
                f.push_hop(&acc);
              }
              if r.net_output {
                // 网络扬声器：位置 tap（放到哪一行就取到哪里的信号）
                crate::net::hub().push_tx_hop(&acc);
              }
            }
          }
        }
      }

      let now = Instant::now();
      if now.duration_since(last_publish) >= PUBLISH {
        // 速率要按**实测**间隔算：拿固定 PUBLISH 去除，publish 偶尔晚一拍就会报出 105 hop/s
        // （2026-10-18 实测：同一段会话里 100 / 105 交替出现 —— 是量法的问题，不是节拍的问题）
        let elapsed = now.duration_since(last_publish).as_secs_f64().max(1e-3);
        last_publish = now;
        let n_in = g
          .iter()
          .filter(|r| {
            r.kind == RowKind::Input
              && (r.input.is_some() || r.aec.is_some() || r.net_input.is_some())
          })
          .count();
        let n_out = g
          .iter()
          .filter(|r| r.kind == RowKind::Output && (r.output.is_some() || r.net_output))
          .count();
        let n_proc = g.iter().filter(|r| r.kind == RowKind::Process).count();
        let unconf = g.iter().filter(|r| r.unconfigured).count();
        let errs = g.iter().filter(|r| r.error.is_some()).count();
        let mut s = format!("列 {}：输入 {n_in}，处理 {n_proc}，输出 {n_out}", idx + 1);
        let rate = (ticks - last_ticks) as f64 / elapsed;
        last_ticks = ticks;
        s.push_str(&format!("，{rate:.0} hop/s"));
        if unconf > 0 {
          s.push_str(&format!("，未配置 {unconf}"));
        }
        if errs > 0 {
          s.push_str(&format!("，异常 {errs}"));
        }
        // 行状态：处理行（模型/参考 + 推理耗时）、AEC 行（对齐/延时）、输入行（环水位/丢弃），
        // 进调试接口便于观测
        let mut notes: Vec<String> = Vec::new();
        for r in g.iter_mut() {
          // 失败的行必须把原因带出来（AGENTS.md：不静默失败；否则只能看到「异常 N」无从定位）
          if let Some(e) = r.error.as_ref() {
            notes.push(format!("⚠ {}：{e}", r.label));
            continue;
          }
          if r.kind == RowKind::Process {
            let st = r
              .stage
              .as_ref()
              .and_then(|s| s.status())
              .unwrap_or_else(|| "运行中".to_string());
            notes.push(format!(
              "{}：{st}，{:.1} ms（最大 {:.1}）",
              r.label, r.inf_ms, r.inf_max_ms
            ));
          } else if let Some(aec) = r.aec.as_ref() {
            let ftotal = aec.hist.lock().map(|h| h.total()).unwrap_or(0);
            let mic_idx = aec.mic_hops * crate::audio::HOP as u64;
            let rate = r.ring_drop_rate.push(now, r.ring_dropped);
            notes.push(format!(
              "{}：精确 {}，回退 {}，直通 {}，延时 {} ms ｜ 远端解释度 ρ={:.2}，回声抑制 {:.1} dB ｜ 网格 far={} mic={}（差 {}）{}",
              r.label,
              aec.exact,
              aec.latest,
              aec.pass,
              aec.delay_samples / 48,
              aec.echo.rho,
              aec.echo.supp_db,
              ftotal,
              mic_idx,
              mic_idx as i64 - ftotal as i64,
              ring_drop_note(r.ring_dropped, &rate)
            ));
          } else if let Some(net) = r.net_input.as_ref() {
            // 网络行状态：入站水位与欠载，便于判断「手机没在说话」还是「缓冲欠载」
            let ns = net.stats();
            notes.push(format!(
              "{}：入站 {} 包 / {:.0} ms，欠载 {}，已取 {} hop",
              r.label,
              ns.rx_packets,
              ns.rx_level as f64 / crate::audio::SAMPLE_RATE as f64 * 1000.0,
              ns.rx_underruns,
              ns.rx_hops
            ));
          } else if r.net_output {
            notes.push(format!(
              "{}：已发 {} 包，订阅 {}",
              r.label,
              crate::net::hub().stats().tx_packets,
              crate::net::hub().sub_count()
            ));
          } else if let Some(cons) = r.input.as_mut() {
            // 输入环水位：源「一大块到达」（Linux 回环 100 ms 一块）时，水位会周期性顶到
            // ROW_CAP 再掉到 0 —— 掉到 0 就是列这一拍取不到数据（补静音），听感是规律地卡。
            let ms = cons.slots() as f64 / crate::audio::SAMPLE_RATE as f64 * 1000.0;
            let rate = r.ring_drop_rate.push(now, r.ring_dropped);
            notes.push(format!(
              "{}：输入环 {ms:.0} ms{}",
              r.label,
              ring_drop_note(r.ring_dropped, &rate)
            ));
          }
        }
        if !notes.is_empty() {
          s.push_str(&format!(" ｜ {}", notes.join("；")));
        }
        *summary.lock().unwrap() = s;
      }
    }

    let now = Instant::now();
    if next > now {
      std::thread::sleep(next - now);
      next += TICK;
    } else if now.duration_since(next) > Duration::from_millis(200) {
      next = now + TICK; // 长时间被挂起后重新对齐，不补发积压
    } else {
      next += TICK; // 只落后一点：立刻跑下一轮追平平均速率
    }
  }
}
