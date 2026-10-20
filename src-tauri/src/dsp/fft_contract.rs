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

//! realfft 缩放约定的固定测试：确认 forward 与 inverse 都是**未归一化**的，
//! 即 `inverse(forward(x)) == N·x`。Hush 的 DSP 依赖这一点（分析侧乘 1/N、
//! 合成侧直接用 inverse，往返增益才是 1）；若将来换 FFT 库，这个测试会先红。

use realfft::RealFftPlanner;

#[test]
fn realfft_is_unnormalized_both_ways() {
  let n = 320usize;
  let mut planner = RealFftPlanner::<f32>::new();
  let fwd = planner.plan_fft_forward(n);
  let inv = planner.plan_fft_inverse(n);

  let x: Vec<f32> = (0..n)
    .map(|i| ((i as f32) * 0.037).sin() + 0.25 * ((i as f32) * 0.11).cos())
    .collect();
  let mut buf = x.clone();
  let mut spec = fwd.make_output_vec();
  fwd.process(&mut buf, &mut spec).expect("forward");

  let mut out = inv.make_output_vec();
  inv.process(&mut spec, &mut out).expect("inverse");

  let mut max_err = 0.0f32;
  for i in 0..n {
    max_err = max_err.max((out[i] / n as f32 - x[i]).abs());
  }
  assert!(
    max_err < 1e-5,
    "inverse(forward(x)) 应为 N·x（最大偏差 {max_err:e}）"
  );
}
