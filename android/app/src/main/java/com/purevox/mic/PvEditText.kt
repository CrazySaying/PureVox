/*
 * PureVox — AI 麦克风降噪工具
 * Copyright (C) 2024-2026 a2heng <752848283@qq.com>
 *
 * PureVox is licensed under the GNU General Public License v3.0 or
 * later (GPL-3.0-or-later).  See LICENSE for details.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * The built-in AI models are NOT covered by the GPL; they are the
 * property of a2heng and may only be used with PureVox under
 * authorization.  See MODEL-LICENSE.md for details.
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

package com.purevox.mic

import android.content.Context
import android.util.AttributeSet
import android.view.KeyEvent
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputConnectionWrapper
import androidx.appcompat.widget.AppCompatEditText

/**
 * 输入框：`etText` 的唯一实现，比 [android.widget.EditText] 多做一件事 ——
 * **框里没字时把「删除」转发给电脑**。
 *
 * 为什么要这个：上方「打字同步」把已组合的文本发走后会把本框清空，于是用户想改错字时
 * 框里是空的，按退格什么也不会发生。真正该删的是**电脑上刚打进去的那个字**，所以要把
 * Android 的删除键（[KeyEvent.KEYCODE_DEL]，电脑端 `keymap.rs` 映射成 PC Backspace）
 * 透传过去。框里**有**字时按老规矩本地删，不打扰电脑 —— 一条路径，两种情形，规则就一句
 * 「空框才转发」。
 *
 * 拦截点是 [InputConnection]：软键盘的退格不走 [KeyEvent]，而是走
 * `deleteSurroundingText`（可能还有 `deleteSurroundingTextInCodePoints`），少数输入法
 * 走 `sendKeyEvent(KEYCODE_DEL)`。三个都拦，规则同上。[onDeleteWhenEmpty] 返回 `true`
 * 表示「已转发、请吞掉」，返回 `false` 就交回默认行为（例如远程输入没开时什么都不做）。
 */
class PvEditText @JvmOverloads constructor(
    context: Context,
    attrs: AttributeSet? = null,
    defStyleAttr: Int = androidx.appcompat.R.attr.editTextStyle,
) : AppCompatEditText(context, attrs, defStyleAttr) {

    /**
     * 框里为空时按删除键的回调。返回 `true` = 已转发给电脑、吞掉这次删除；
     * 返回 `false`（或未设置）= 走默认行为。由界面在 `applyState` 里根据
     * 「远程输入开关 + ready」现场决定，本控件不认识协议。
     */
    var onDeleteWhenEmpty: (() -> Boolean)? = null

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection? {
        val base = super.onCreateInputConnection(outAttrs) ?: return null
        return PvInputConnection(base)
    }

    private inner class PvInputConnection(target: InputConnection) :
        InputConnectionWrapper(target, true) {

        override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
            if (shouldForward(beforeLength, afterLength)) return true
            return super.deleteSurroundingText(beforeLength, afterLength)
        }

        override fun deleteSurroundingTextInCodePoints(beforeLength: Int, afterLength: Int): Boolean {
            if (shouldForward(beforeLength, afterLength)) return true
            return super.deleteSurroundingTextInCodePoints(beforeLength, afterLength)
        }

        override fun sendKeyEvent(event: KeyEvent): Boolean {
            val isDel = event.keyCode == KeyEvent.KEYCODE_DEL
            if (isDel && text.isNullOrEmpty() && (onDeleteWhenEmpty?.invoke() == true)) {
                return true
            }
            return super.sendKeyEvent(event)
        }

        /** 空框 + 确实是删除（前/后至少删 1 个字符）→ 转发。 */
        private fun shouldForward(beforeLength: Int, afterLength: Int): Boolean =
            (beforeLength > 0 || afterLength > 0) &&
                text.isNullOrEmpty() &&
                onDeleteWhenEmpty?.invoke() == true
    }
}
