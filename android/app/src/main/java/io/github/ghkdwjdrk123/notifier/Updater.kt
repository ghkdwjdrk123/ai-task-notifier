package io.github.ghkdwjdrk123.notifier

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.os.Build
import android.widget.Toast
import java.io.File
import java.io.IOException
import java.util.concurrent.atomic.AtomicBoolean

/**
 * 앱 안에서 업데이트: 맥 서버가 올려 둔 새 APK(GET /v1/app/latest·/v1/app/apk)를 받아
 * 안드로이드 설치 화면으로 넘긴다. 서명이 같은 APK만 기존 앱 위에 설치된다(안드로이드가 확인).
 */
object Updater {
    data class Latest(val versionCode: Long, val versionName: String, val size: Long, val sha256: String)

    /** 설치된 것보다 새 버전(없으면 null). */
    @Volatile var available: Latest? = null
    @Volatile var lastCheckMs: Long = 0
    /** 받는 중·설치 넘기는 중(화면이 다시 만들어져도 한 번만). */
    val running = AtomicBoolean(false)
    /** 대시보드 카드에 보일 진행·오류 문구. */
    @Volatile var message: String? = null
    /** 앱 화면이 보이는지(설치 확인 화면을 바로 띄울 수 있는지). MainActivity가 갱신. */
    @Volatile var visible = false

    private const val CHANNEL = "update"
    private const val NOTIFY_ID = 2

    fun installedCode(ctx: Context): Long = ctx.packageManager.getPackageInfo(ctx.packageName, 0).longVersionCode

    fun installedName(ctx: Context): String = ctx.packageManager.getPackageInfo(ctx.packageName, 0).versionName ?: "?"

    /** 새 버전을 확인한다(막는 호출: UI 스레드 밖에서). 맥에 올려 둔 것이 없으면 null. */
    fun check(ctx: Context, api: Api): Latest? {
        val l = try {
            api.appLatest()
        } catch (e: ApiException) {
            if (e.status == 404) null else throw e
        }
        // 성공했을 때만 시각을 남긴다(실패하면 다음에 화면을 열 때 다시 확인)
        lastCheckMs = System.currentTimeMillis()
        available = l?.takeIf { it.versionCode > installedCode(ctx) }
        return available
    }

    /** 받고 확인한 뒤 설치 세션을 넘긴다(막는 호출). 사용자 확인 화면은 [InstallReceiver]가 띄운다. */
    fun downloadAndInstall(ctx: Context, api: Api, l: Latest) {
        val dir = File(ctx.cacheDir, "update").apply { mkdirs() }
        val apk = File(dir, "update.apk")
        try {
            api.downloadApk(apk, l.size, l.sha256)
            val installer = ctx.packageManager.packageInstaller
            val params = PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL).apply {
                setAppPackageName(ctx.packageName)
                setSize(apk.length())
            }
            val id = installer.createSession(params)
            try {
                installer.openSession(id).use { s ->
                    s.openWrite("base.apk", 0, apk.length()).use { out ->
                        apk.inputStream().use { it.copyTo(out) }
                        s.fsync(out)
                    }
                    val intent = Intent(ctx, InstallReceiver::class.java)
                    // 설치기가 결과를 채워 넣으므로 변경 가능 PendingIntent여야 한다(명시적 Intent)
                    val pi = PendingIntent.getBroadcast(ctx, id, intent, PendingIntent.FLAG_MUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
                    s.commit(pi.intentSender)
                }
            } catch (e: Exception) {
                runCatching { installer.abandonSession(id) }
                throw e
            }
        } finally {
            apk.delete()
        }
    }

    /**
     * 설치 확인 화면을 보여 준다. 앱 화면이 보이면 바로 띄우고, 아니면(받는 사이 홈으로 나감)
     * Android 14+의 백그라운드 화면 시작 제한에 걸리므로 눌러서 여는 알림으로 대신한다.
     */
    fun showConfirm(ctx: Context, confirm: Intent) {
        confirm.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        if (visible && runCatching { ctx.startActivity(confirm) }.isSuccess) {
            message = "설치 화면에서 '업데이트'를 누르세요"
            return
        }
        val nm = ctx.getSystemService(NotificationManager::class.java)
        if (nm.getNotificationChannel(CHANNEL) == null) {
            nm.createNotificationChannel(NotificationChannel(CHANNEL, "앱 업데이트", NotificationManager.IMPORTANCE_DEFAULT))
        }
        val pi = PendingIntent.getActivity(ctx, NOTIFY_ID, confirm, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        nm.notify(NOTIFY_ID, Notification.Builder(ctx, CHANNEL)
            .setSmallIcon(android.R.drawable.stat_sys_download_done)
            .setContentTitle("앱 업데이트 준비됨")
            .setContentText("눌러서 새 버전을 설치하세요")
            .setContentIntent(pi)
            .setAutoCancel(true)
            .build())
        message = "알림을 눌러 설치를 마치세요"
    }
}

/** 설치 세션 결과: 사용자 확인이 필요하면 설치 확인 화면을 띄운다. */
class InstallReceiver : BroadcastReceiver() {
    override fun onReceive(ctx: Context, intent: Intent) {
        when (val status = intent.getIntExtra(PackageInstaller.EXTRA_STATUS, -999)) {
            PackageInstaller.STATUS_PENDING_USER_ACTION -> {
                val confirm = if (Build.VERSION.SDK_INT >= 33) {
                    intent.getParcelableExtra(Intent.EXTRA_INTENT, Intent::class.java)
                } else {
                    @Suppress("DEPRECATION")
                    intent.getParcelableExtra(Intent.EXTRA_INTENT)
                } ?: return
                Updater.showConfirm(ctx, confirm)
            }
            PackageInstaller.STATUS_SUCCESS -> Updater.message = null // 앱이 새 버전으로 다시 시작된다
            else -> {
                val msg = intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE) ?: "코드 $status"
                Updater.message = "설치되지 않았어요: $msg"
                Toast.makeText(ctx, "업데이트 설치 안 됨: $msg", Toast.LENGTH_LONG).show()
            }
        }
    }
}

/** 받은 APK가 서버가 알려 준 크기·해시와 다를 때. */
class UpdateCheckFailed(message: String) : IOException(message)
