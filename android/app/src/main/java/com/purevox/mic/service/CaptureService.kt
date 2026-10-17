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

package com.purevox.mic.service

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import android.util.Log
import androidx.core.app.NotificationCompat
import com.purevox.mic.MainActivity
import com.purevox.mic.R

/**
 * 会话前台服务（**保活**）。
 *
 * 职责很窄：挂一个常驻通知 + 持一把 PARTIAL_WAKE_LOCK，让系统别把本进程冻结 ——
 * 音频与网络线程都在界面进程里，没有前台服务，Android 9+ 后台拿不到麦克风、Android 12+
 * 还会被系统直接掐掉。
 *
 * **服务范围 = 整个会话，不是只有麦克风**：只要「已连接」或「正在推流」就常驻，
 * 通知里的两个按钮（[BROADCAST_NOTIF_STOP] / [BROADCAST_NOTIF_DISCONNECT]）由界面
 * （[MainActivity]）接收并处理 —— 服务本身不认识协议，**界面是唯一的编排者**。
 *
 * 前台服务类型：麦克风在用时 `microphone|dataSync`，只是保活连接时 `dataSync`。
 * 哪个在用就报哪个（Android 14 起按实际用途校验，不能超报）。
 */
class CaptureService : Service() {

    companion object {
        private const val TAG = "PureVoxSvc"

        const val ACTION_START = "com.purevox.mic.action.START"
        const val ACTION_UPDATE = "com.purevox.mic.action.UPDATE"
        const val ACTION_STOP = "com.purevox.mic.action.STOP"

        /**
         * 通知按钮发回界面的广播（**不是**服务自己的 action）。
         * 「停止推流」= 关麦克风链路；「断开连接」= 断开 WebSocket。两件事分开。
         */
        const val BROADCAST_NOTIF_STOP = "com.purevox.mic.notif.STOP"
        const val BROADCAST_NOTIF_DISCONNECT = "com.purevox.mic.notif.DISCONNECT"

        /** 通知正文（由界面刷新，用来显示已推包数 / 采集格式 / 保活状态）。 */
        const val EXTRA_TEXT = "text"

        /** 本条通知是否包含 `microphone` 前台服务类型。 */
        const val EXTRA_MIC = "mic"

        const val CHANNEL_ID = "purevox_mic_stream"
        const val NOTIFICATION_ID = 1001

        private const val WAKE_LOCK_TAG = "PureVoxMic:AudioWakeLock"

        fun start(ctx: Context, text: String, mic: Boolean) =
            deliver(ctx, ACTION_START, text, mic)

        fun update(ctx: Context, text: String, mic: Boolean) =
            deliver(ctx, ACTION_UPDATE, text, mic)

        fun stop(ctx: Context) {
            ctx.stopService(Intent(ctx, CaptureService::class.java))
        }

        private fun deliver(ctx: Context, action: String, text: String, mic: Boolean) {
            val intent = Intent(ctx, CaptureService::class.java).apply {
                this.action = action
                putExtra(EXTRA_TEXT, text)
                putExtra(EXTRA_MIC, mic)
            }
            try {
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                    ctx.startForegroundService(intent)
                } else {
                    ctx.startService(intent)
                }
            } catch (e: Throwable) {
                // Android 12+ 从后台起前台服务会抛 ForegroundServiceStartNotAllowedException。
                // 这里不能让它炸掉界面：保活失败只是「没通知」，连接与推流照常。日志留痕。
                Log.e(TAG, "起前台服务失败（保活未生效）", e)
            }
        }
    }

    private var wakeLock: PowerManager.WakeLock? = null

    /** 本次会话当前是否含麦克风（决定前台服务类型掩码）。 */
    @Volatile
    private var micActive = false

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        createChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_START, ACTION_UPDATE -> {
                val text = intent.getStringExtra(EXTRA_TEXT) ?: getString(R.string.notif_default)
                micActive = intent.getBooleanExtra(EXTRA_MIC, false)
                acquireWakeLock()
                startForegroundCompat(text, micActive)
            }
            ACTION_STOP -> {
                releaseWakeLock()
                stopForegroundCompat()
                stopSelf()
            }
        }
        // 不复活：麦克风不能悄悄重启，用户得自己再开一次
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        releaseWakeLock()
        stopForegroundCompat()
        super.onDestroy()
    }

    // ---------------------------------------------------------------- 内部

    @Suppress("DEPRECATION")
    private fun startForegroundCompat(text: String, mic: Boolean) {
        val notification = buildNotification(text)
        val type = when {
            Build.VERSION.SDK_INT < Build.VERSION_CODES.Q -> 0   // 走两参重载
            mic -> ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE or
                ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC
            else -> ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC
        }
        try {
            if (type != 0) {
                startForeground(NOTIFICATION_ID, notification, type)
            } else {
                startForeground(NOTIFICATION_ID, notification)
            }
        } catch (e: Throwable) {
            // Android 12+ 从后台起前台服务会抛；界面会把它显示出来，不静默
            Log.e(TAG, "startForeground 失败", e)
            stopSelf()
        }
    }

    @Suppress("DEPRECATION")
    private fun stopForegroundCompat() {
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.N) {
                stopForeground(Service.STOP_FOREGROUND_REMOVE)
            } else {
                stopForeground(true)
            }
        } catch (e: Throwable) {
            Log.w(TAG, "stopForeground 异常", e)
        }
    }

    private fun createChannel() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
        val channel = NotificationChannel(
            CHANNEL_ID,
            getString(R.string.notif_channel_name),
            NotificationManager.IMPORTANCE_LOW,
        ).apply {
            description = getString(R.string.notif_channel_desc)
            setShowBadge(false)
        }
        getSystemService(NotificationManager::class.java)?.createNotificationChannel(channel)
    }

    private fun buildNotification(text: String): Notification {
        val launch = packageManager.getLaunchIntentForPackage(packageName)
        val contentPi = launch?.let {
            PendingIntent.getActivity(this, 0, it, PendingIntent.FLAG_IMMUTABLE)
        }
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle(getString(R.string.app_name))
            .setContentText(text)
            .setSmallIcon(R.drawable.ic_stat_mic)
            .setContentIntent(contentPi)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .addAction(0, getString(R.string.notif_action_stop), broadcast(BROADCAST_NOTIF_STOP, 1))
            .addAction(
                0,
                getString(R.string.notif_action_disconnect),
                broadcast(BROADCAST_NOTIF_DISCONNECT, 2),
            )
            .build()
    }

    /** 通知按钮 → 广播（限本应用 `setPackage`，接收端是 MainActivity 的动态接收器）。 */
    private fun broadcast(action: String, requestCode: Int): PendingIntent {
        val intent = Intent(action).setPackage(packageName)
        return PendingIntent.getBroadcast(this, requestCode, intent, PendingIntent.FLAG_IMMUTABLE)
    }

    private fun acquireWakeLock() {
        if (wakeLock != null) return
        val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
        wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, WAKE_LOCK_TAG).apply {
            setReferenceCounted(false)
            // 长推流场景不给上限，但设一个远大于任何会话的兜底，避免永久持有
            acquire(4 * 60 * 60 * 1000L)
        }
    }

    private fun releaseWakeLock() {
        val l = wakeLock ?: return
        wakeLock = null
        try {
            if (l.isHeld) l.release()
        } catch (e: Throwable) {
            Log.w(TAG, "释放 WakeLock 异常", e)
        }
    }
}
