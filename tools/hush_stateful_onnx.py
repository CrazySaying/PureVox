# PureVox — AI 麦克风降噪工具
# Copyright (C) 2024-2026 a2heng <752848283@qq.com>
#
# PureVox is licensed under the GNU General Public License v3.0 or
# later (GPL-3.0-or-later).  See LICENSE for details.
# 
# This program is free software: you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation, either version 3 of the License, or
# (at your option) any later version.
# 
# The built-in AI models are NOT covered by the GPL; they are the
# property of a2heng and may only be used with PureVox under
# authorization.  See MODEL-LICENSE.md for details.
# 
# SPDX-License-Identifier: GPL-3.0-or-later

"""把 Hush 的 tract 版 ONNX 改造成 **带显式循环状态** 的 ONNX（一次性构建工具）。

背景（M0 实测结论）
-------------------
Hush 官方 bundle 里的 `enc.onnx` / `erb_dec.onnx` / `df_dec.onnx` 是给
DeepFilterNet 的 Rust 后端 **tract** 用的：每个 GRU 节点的 `initial_h` 都接在
常量零上、`Y_h` 悬空不接线，**循环状态完全不出现在图接口上**。tract 用
`PulsedModel` 在帧间自动维护隐状态，所以它能工作；而 onnxruntime 没有等价机制，
逐帧调用会让每个 GRU 每帧都从零初态起步 —— 实测第 1~7 帧的输出差异高达
0.78~1.60（第 0 帧为 0，因为两者初态都是零），等于彻底丢掉时间记忆。

本工具做的改造（每个 GRU 节点）：
  1. `initial_h`（第 6 个输入）改接一个新的 **图输入** `<base>_state_in`；
  2. 原本就存在但悬空的 `Y_h`（第 2 个输出）提升为 **图输出** `<base>_state_out`。

这样调用方就可以按帧回灌状态：把上一帧的 `_state_out` 喂给下一帧的 `_state_in`，
零初态表示新流开始。改造是机械的、可复现的，**不重新导出、不改权重**。

用法::

    python tools/hush_stateful_onnx.py --src <原始目录> --dst models/hush

依赖 `onnx`（仅构建时需要，运行时不依赖）。
"""

import argparse
import os
import sys

import onnx
from onnx import TensorProto, helper

# ── enc 的时域卷积问题与拆分 ──
# M2a 实测：enc.onnx 带 2 个 kernel=(3,3) 的卷积（时域延伸 3），而 erb_dec /
# df_dec 的卷积全是 (1,k)（无时域延伸）。后果：
#   · erb_dec / df_dec 只要 GRU 状态外露，就能严格逐帧（S=1）—— 实测 1.8e-7；
#   · enc 不行：窗口边界处卷积缺 2 帧上文，其输出污染 GRU 状态，且污染会随
#     状态一直带下去（窗口越长越淡但永不归零：L=3 → 1.3e-1，L=24 → 2.1e-3，
#     只有 L=整段才是 0）。所以 enc 必须**拆成两半**：
#       enc_conv : 卷积编码器，吃 3 帧窗口、取末帧 —— 末帧卷积上文完整，精确；
#       enc_gru  : GRU 头，逐帧 + 状态回灌 —— 精确。
# 拆点 = GRU 节点的第 0 个输入（GRU 的 X）。
_SPLIT_SPECS = {
    "enc": {
        "conv_out": ["e0", "e1", "e2", "e3", "c0"],
        "gru_out": ["emb", "lsnr"],
    },
}

# 单 GRU 的图用一个 base 名；多 GRU（df_dec 有 3 个）自动加序号后缀
_BASE_BY_FILE = {
    "enc": "enc_state",
    "erb_dec": "erb_state",
    "df_dec": "df_state",
}


def _consumers(graph) -> dict:
    """值名 -> 消费它的节点名列表。"""
    out = {}
    for node in graph.node:
        for name in node.input:
            if name:
                out.setdefault(name, []).append(node.name)
    return out


def _gru_attrs(node):
    hidden, direction = None, "forward"
    for a in node.attribute:
        if a.name == "hidden_size":
            hidden = a.i
        elif a.name == "direction":
            direction = a.s.decode("utf-8")
    if hidden is None:
        raise SystemExit(f"GRU 节点 {node.name!r} 缺少 hidden_size 属性")
    n_dir = 2 if direction == "bidirectional" else 1
    return hidden, n_dir, direction


def patch(src: str, dst: str, base: str) -> list:
    """改造单个 ONNX；返回 (输入名, 输出名, 形状) 列表。"""
    model = onnx.load(src)
    graph = model.graph
    grus = sorted((n for n in graph.node if n.op_type == "GRU"),
                  key=lambda n: n.name)
    if not grus:
        raise SystemExit(f"{src}: 未找到 GRU 节点，无需改造？")

    used_consumers = _consumers(graph)
    existing_inputs = {vi.name for vi in graph.input}
    existing_outputs = {vo.name for vo in graph.output}
    rows = []
    multi = len(grus) > 1

    for idx, node in enumerate(grus):
        hidden, n_dir, direction = _gru_attrs(node)
        stem = f"{base}{idx}" if multi else base
        in_name, out_name = f"{stem}_in", f"{stem}_out"
        if in_name in existing_inputs or out_name in existing_outputs:
            raise SystemExit(f"{src}: 名称冲突 {in_name!r} / {out_name!r}")

        # ── 1. initial_h -> 图输入 ──
        while len(node.input) < 6:
            node.input.append("")
        old_h = node.input[5]
        node.input[5] = in_name

        # ── 2. Y_h -> 图输出 ──
        while len(node.output) < 2:
            node.output.append("")
        old_yh = node.output[1]
        if old_yh and used_consumers.get(old_yh):
            # 已被其它节点消费：不能改名，直接沿用原名做图输出
            out_name = old_yh
        else:
            node.output[1] = out_name

        shape = [n_dir, 1, hidden]      # [方向数, batch=1, hidden]
        graph.input.append(
            helper.make_tensor_value_info(in_name, TensorProto.FLOAT, shape))
        graph.output.append(
            helper.make_tensor_value_info(out_name, TensorProto.FLOAT, shape))
        existing_inputs.add(in_name)
        existing_outputs.add(out_name)
        rows.append((node.name, direction, old_h, in_name, out_name, shape))

    # 清理：原 initial_h 的生产者若已无人消费，断开其输出引用以便优化器回收
    # （保留节点本身无害，ORT 会做死代码消除；这里只做提示）
    onnx.checker.check_model(model)
    os.makedirs(os.path.dirname(os.path.abspath(dst)), exist_ok=True)
    onnx.save(model, dst)
    return rows


def _ancestors(graph, keep_outputs, cut_values=()):
    """从 keep_outputs 向上回溯，返回 (保留下来的节点名, 当作输入的值名)。

    cut_values 中的值即使有生产者也不再回溯 —— 它会被提升为图输入（拆点）。
    """
    producer = {}
    for node in graph.node:
        for out in node.output:
            if out:
                producer[out] = node
    keep_nodes = set()
    cut = set()
    stack = list(keep_outputs)
    seen = set(keep_outputs)
    while stack:
        val = stack.pop()
        if val in cut_values:
            cut.add(val)
            continue
        node = producer.get(val)
        if node is None:            # 图输入 / initializer
            continue
        if node.name in keep_nodes:
            continue
        keep_nodes.add(node.name)
        for inp in node.input:
            if inp and inp not in seen:
                seen.add(inp)
                stack.append(inp)
    return keep_nodes, cut


def extract(model, keep_outputs, cut_values=(), out_names=None):
    """按输出集合裁剪子图；cut_values 提升为图输入。返回新 ModelProto。"""
    graph = model.graph
    keep_nodes, cut = _ancestors(graph, keep_outputs, cut_values)

    init_by_name = {t.name: t for t in graph.initializer}
    vi_by_name = {vi.name: vi for vi in graph.value_info}
    vi_by_name.update({vi.name: vi for vi in graph.input})
    vi_by_name.update({vi.name: vi for vi in graph.output})

    nodes = [n for n in graph.node if n.name in keep_nodes]

    # 被保留节点引用到的 initializer
    referenced = set()
    for n in nodes:
        referenced.update(i for i in n.input if i)
    referenced.update(o for o in keep_outputs)

    inits = [t for t in graph.initializer if t.name in referenced]
    init_names = {t.name for t in inits}

    # 图输入 = 原图输入中仍被引用者 + 拆点值
    inputs = []
    for vi in graph.input:
        if vi.name in referenced and vi.name not in init_names:
            inputs.append(vi)
    for name in sorted(cut):
        if name in vi_by_name:
            inputs.append(vi_by_name[name])
        else:
            # 形状推断拿不到时给动态 rank-3（S,1,H）
            inputs.append(helper.make_tensor_value_info(
                name, TensorProto.FLOAT, ["S", 1, 256]))

    outputs = []
    for name in keep_outputs:
        vi = vi_by_name.get(name)
        if vi is None or not vi.type.HasField("tensor_type"):
            # 形状推断拿不到时给动态 rank-3（S,1,hidden）——GRU 输入就是这个布局
            vi = helper.make_tensor_value_info(name, TensorProto.FLOAT, [None, None, None])
        outputs.append(vi)

    new_graph = helper.make_graph(nodes, graph.name + "_sub", inputs, outputs,
                                  initializer=inits)
    sub = helper.make_model(new_graph, opset_imports=model.opset_import,
                            ir_version=model.ir_version)
    sub.producer_name = model.producer_name
    return sub


def split_enc(src: str, dst_dir: str, spec: dict, base: str):
    """把 enc.onnx 拆成 enc_conv（窗口卷积）+ enc_gru（逐帧 GRU 头）。"""
    model = onnx.load(src)
    try:
        model = onnx.shape_inference.infer_shapes(model)
    except Exception:
        pass
    graph = model.graph
    grus = sorted((n for n in graph.node if n.op_type == "GRU"), key=lambda n: n.name)
    if len(grus) != 1:
        raise SystemExit(f"{src}: 期望恰好 1 个 GRU，实际 {len(grus)}")
    gru = grus[0]
    cut_val = gru.input[0]
    state_in, state_out = f"{base}_in", f"{base}_out"
    # 注意：enc.onnx 此刻已是「状态已外露」的版本，GRU 的 initial_h = state_in
    if gru.input[5] != state_in:
        raise SystemExit(f"{src}: 预期 initial_h={state_in!r}，实际 {gru.input[5]!r}")

    # ── 卷积半：输出 skip 连接 + GRU 的输入张量 ──
    # 注意：在**源图**里要用真实张量名 cut_val 做保留集合，之后再改名成 gru_in
    conv = extract(model, list(spec["conv_out"]) + [cut_val])
    for node in conv.graph.node:
        for i, o in enumerate(node.output):
            if o == cut_val:
                node.output[i] = "gru_in"
    for vi in list(conv.graph.output):
        if vi.name == cut_val:
            vi.name = "gru_in"
    conv_path = os.path.join(dst_dir, "enc_conv.onnx")
    onnx.checker.check_model(conv)
    onnx.save(conv, conv_path)

    # ── GRU 半：逐帧 + 状态 ──
    gru_outs = list(spec["gru_out"]) + [state_out]
    head = extract(model, gru_outs, cut_values=(cut_val,))
    for node in head.graph.node:
        for i, inp in enumerate(node.input):
            if inp == cut_val:
                node.input[i] = "gru_in"
    for node in head.graph.node:
        for i, o in enumerate(node.output):
            if o == cut_val:
                node.output[i] = "gru_in"
    for vi in head.graph.input:
        if vi.name == cut_val:
            vi.name = "gru_in"
    gru_path = os.path.join(dst_dir, "enc_gru.onnx")
    onnx.checker.check_model(head)
    onnx.save(head, gru_path)

    return conv_path, gru_path


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description="Hush ONNX 循环状态外露改造")
    ap.add_argument("--src", required=True, help="原始 ONNX 目录（含 config.ini）")
    ap.add_argument("--dst", required=True, help="输出目录")
    args = ap.parse_args(argv)

    for name, base in _BASE_BY_FILE.items():
        src = os.path.join(args.src, f"{name}.onnx")
        dst = os.path.join(args.dst, f"{name}.onnx")
        if not os.path.isfile(src):
            print(f"[skip] 找不到 {src}", file=sys.stderr)
            continue
        rows = patch(src, dst, base)
        in_b = os.path.getsize(src)
        out_b = os.path.getsize(dst)
        print(f"\n=== {name}.onnx  ({in_b:,} -> {out_b:,} bytes) ===")
        for node_name, direction, old_h, in_name, out_name, shape in rows:
            print(f"  {node_name}")
            print(f"    方向      {direction}")
            print(f"    initial_h {old_h!r}")
            print(f"      -> 输入 {in_name!r}  {shape}")
            print(f"      -> 输出 {out_name!r}  {shape}")

        if name in _SPLIT_SPECS:
            conv_path, gru_path = split_enc(dst, args.dst, _SPLIT_SPECS[name], base)
            print(f"  ── 因带时域卷积，已拆分为两半（见文件头说明）──")
            print(f"    {os.path.basename(conv_path):<16} "
                  f"{os.path.getsize(conv_path):,} bytes")
            print(f"    {os.path.basename(gru_path):<16} "
                  f"{os.path.getsize(gru_path):,} bytes")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
