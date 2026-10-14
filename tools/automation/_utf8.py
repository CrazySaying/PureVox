#!/usr/bin/env python3
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
"""把 stdout / stderr 强制成 UTF-8 —— **在脚本 import 后、任何 print 之前调用一次**。

## 为什么必须有

本仓库的脚本**大量打中文**（门禁项名、报错原因、产物清单）。而 Python 的
`sys.stdout.encoding` 取自**控制台代码页**：

| 环境 | 编码 | 中文能打印吗 |
| --- | --- | --- |
| 本机 Windows 控制台（中文 locale） | cp936 / GBK | ✅ 所以本地永远绿 |
| **GitHub Actions windows runner** | **cp1252** | ❌ `UnicodeEncodeError` |
| Linux runner / WSL | UTF-8 | ✅ |

这就是典型的「本地过、CI 挂」：`print("门禁全部通过")` 在 cp1252 上直接抛
`UnicodeEncodeError: 'charmap' codec can't encode characters`，脚本以退出码 1 结束 ——
**门禁本身跑完了，却被编码错误判成失败**。

## 为什么用 `errors="replace"`

不抛异常优先于「显示得完美」：万一还有别处的非 UTF-8 字符（比如子进程吐出来的
某台机器的 OEM 代码页文本），宁可显示成 `?` 也不能让整个门禁崩掉。

workflow 那边也设了 `PYTHONIOENCODING: utf-8` 作为双保险，但**本机直接跑脚本时
用不上那个环境变量**，所以每个脚本自己也要调一次。

用法：

```python
import _utf8
_utf8.force()
```
"""

from __future__ import annotations

import sys


def force() -> None:
    """把 stdout / stderr 重配成 UTF-8（幂等，可重复调用）。"""
    for stream in (sys.stdout, sys.stderr):
        reconfigure = getattr(stream, "reconfigure", None)
        if reconfigure is None:
            # 被替换成了 StringIO / 某些嵌入式环境的流：没有就不管
            continue
        try:
            reconfigure(encoding="utf-8", errors="replace")
        except (ValueError, OSError, AttributeError):
            pass