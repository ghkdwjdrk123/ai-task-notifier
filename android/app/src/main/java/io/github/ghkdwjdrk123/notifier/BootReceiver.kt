package io.github.ghkdwjdrk123.notifier

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/** 폰을 켜거나 앱을 업데이트한 뒤 상시 알림을 다시 띄운다. */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val ok = intent.action == Intent.ACTION_BOOT_COMPLETED || intent.action == Intent.ACTION_MY_PACKAGE_REPLACED
        val prefs = Prefs(context)
        if (ok && prefs.isPaired && prefs.statusEnabled) {
            StatusService.start(context)
        }
    }
}
