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

package com.purevox.mic.update

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.provider.Settings
import androidx.core.content.FileProvider
import java.io.File

/**
 * 把下载好的 APK 交给系统安装器。
 *
 * **必须走 [FileProvider]**（`AndroidManifest.xml` 里 `<provider>`，路径见
 * `res/xml/file_paths.xml`）：Android 7.0 起直接传 `file://` 会抛
 * `FileUriExposedException`。APK 落在应用私有外部目录，FileProvider 只授权它，别的不外泄。
 *
 * **「安装未知应用」权限按平台**（[REQUEST_INSTALL_PACKAGES]）：
 * Android 8.0（API 26）起是**每个应用一份**的开关，未授予时安装器会拒绝；低版本是系统全局
 * 设置，这里一律当已授予。未授予时[install] **不静默失败** —— 界面据 [canInstall]
 * 先把用户引到系统设置（[openInstallSettings]）。
 */
object UpdateInstaller {

    /** 是否已允许本应用安装未知来源的包。API 26+ 才按应用判定。 */
    fun canInstall(ctx: Context): Boolean =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            ctx.packageManager.canRequestPackageInstalls()
        } else {
            true
        }

    /** 打开「安装未知应用」的授权页（API 26+）。 */
    fun openInstallSettings(ctx: Context) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
        val intent = Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES)
            .setData(Uri.parse("package:${ctx.packageName}"))
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        try {
            ctx.startActivity(intent)
        } catch (e: Exception) {
            // 个别 ROM 没有这个页面 → 退到本应用的详情页，用户仍能手动找到开关
            val fallback = Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS)
                .setData(Uri.parse("package:${ctx.packageName}"))
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            ctx.startActivity(fallback)
        }
    }

    /** 调起系统安装器。调用方须先确认 [canInstall]。 */
    fun install(ctx: Context, apk: File) {
        val uri = FileProvider.getUriForFile(ctx, "${ctx.packageName}.fileprovider", apk)
        val intent = Intent(Intent.ACTION_VIEW)
            .setDataAndType(uri, "application/vnd.android.package-archive")
            .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        ctx.startActivity(intent)
    }
}
