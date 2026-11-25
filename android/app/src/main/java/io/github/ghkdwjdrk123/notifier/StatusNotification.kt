package io.github.ghkdwjdrk123.notifier

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.PorterDuff
import android.graphics.PorterDuffXfermode
import android.graphics.Typeface
import android.graphics.drawable.Icon
import java.time.Duration
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import kotlin.math.roundToInt

/**
 * 상시 상태 알림. 소리·진동 없이 같은 알림 하나를 계속 고쳐 쓴다.
 * 상태바 아이콘: 결정 대기가 있으면 채운 원 안에 대기 수, 없으면 진행 수 숫자.
 */
object StatusNotification {
    const val CHANNEL = "status"
    const val ID = 1

    private val clock = DateTimeFormatter.ofPattern("HH:mm").withZone(ZoneId.systemDefault())

    /** 알림에 보이는 내용. 같으면 다시 그리지 않는다(지운 알림이 곧바로 되살아나지 않게). */
    data class Content(
        val title: String,
        val text: String,
        val big: String,
        val publicText: String,
        val iconLabel: String,
        val iconFilled: Boolean,
        val singleDecisionId: String?,
    )

    fun ensureChannel(ctx: Context) {
        val nm = ctx.getSystemService(NotificationManager::class.java)
        if (nm.getNotificationChannel(CHANNEL) == null) {
            val ch = NotificationChannel(CHANNEL, "작업 상태", NotificationManager.IMPORTANCE_LOW).apply {
                description = "AI 작업 진행 수·결정 대기·사용량을 계속 보여 줍니다(소리 없음)"
                setShowBadge(false)
                enableVibration(false)
                setSound(null, null)
            }
            nm.createNotificationChannel(ch)
        }
    }

    fun content(st: StatusHub.State, now: Instant = Instant.now()): Content {
        val snap = st.snapshot
        val waiting = snap?.waitingDecisions ?: 0
        val iconLabel = when {
            snap == null -> "…"
            waiting > 0 -> waiting.coerceAtMost(9).toString()
            else -> snap.running.coerceAtMost(99).toString()
        }
        return when {
            st.link == Link.UNAUTHORIZED -> Content(
                "다시 등록이 필요해요", "이 기기 연결이 해제됐어요. 눌러서 다시 등록하세요", "",
                "다시 등록이 필요해요", "!", false, null,
            )
            snap == null -> {
                val title = if (st.link == Link.DISCONNECTED) "맥에 연결하지 못했어요" else "맥에 연결 중…"
                Content(title, st.error ?: "Tailscale이 켜져 있는지 확인하세요", "", title, iconLabel, false, null)
            }
            else -> {
                val title = "진행 ${snap.running} · 결정 대기 ${snap.waitingDecisions}" + staleSuffix(st, snap, now)
                val usage = usageLine(snap, now).ifEmpty { "사용량 정보 없음" }
                val big = StringBuilder(usage)
                snap.decisions.take(3).forEach { big.append("\n• 결정: ").append(it.title) }
                snap.sessions.filter { it.state != "idle" }.take(4).forEach {
                    big.append("\n• ").append(Names.provider(it.provider)).append(" ").append(it.title)
                        .append(" — ").append(Names.sessionState(it.state))
                }
                val single = snap.decisions.singleOrNull()?.takeIf { it.state == "open" }?.id
                Content(title, usage, big.toString(), usage, iconLabel, waiting > 0, single)
            }
        }
    }

    fun build(ctx: Context, c: Content): Notification {
        ensureChannel(ctx)
        val open = Intent(ctx, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP)
        val flags = PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
        // 답할 결정이 딱 하나면 누르자마자 그 답변 화면을 연다(뒤로 가면 대시보드)
        val pi = if (c.singleDecisionId != null) {
            val detail = Intent(ctx, DecisionActivity::class.java).putExtra(DecisionActivity.EXTRA_ID, c.singleDecisionId)
            PendingIntent.getActivities(ctx, 0, arrayOf(open, detail), flags)
        } else {
            PendingIntent.getActivity(ctx, 0, open, flags)
        }
        val icon = smallIcon(c.iconLabel, c.iconFilled)
        // 잠금화면에는 숫자·사용량만(세션·결정 제목, 오류 내용 숨김)
        val public = Notification.Builder(ctx, CHANNEL)
            .setSmallIcon(icon)
            .setContentTitle(c.title)
            .setContentText(c.publicText)
            .build()
        return Notification.Builder(ctx, CHANNEL)
            .setSmallIcon(icon)
            .setContentTitle(c.title)
            .setContentText(c.text)
            .setStyle(Notification.BigTextStyle().bigText(c.big.ifEmpty { c.text }))
            .setContentIntent(pi)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setShowWhen(false)
            .setCategory(Notification.CATEGORY_STATUS)
            .setVisibility(Notification.VISIBILITY_PRIVATE)
            .setPublicVersion(public)
            .setForegroundServiceBehavior(Notification.FOREGROUND_SERVICE_IMMEDIATE)
            .build()
    }

    private fun staleSuffix(st: StatusHub.State, snap: Snapshot, now: Instant): String {
        val at = st.receivedAt
        return when {
            (st.link != Link.LIVE || StatusHub.isStale(st, now)) && at != null -> " · ${clock.format(at)} 기준"
            snap.degraded -> " · 갱신 지연"
            else -> ""
        }
    }

    /** 예: "Claude 5시간 48% · 주간 4%  |  ChatGPT 5시간 12%". 초기화 시각이 지난 창은 '초기화됨'. */
    fun usageLine(snap: Snapshot, now: Instant = Instant.now()): String =
        snap.usage.groupBy { it.provider }.entries.joinToString("  |  ") { (p, ws) ->
            val parts = ws.sortedBy { windowOrder(it.window) }.joinToString(" · ") { w ->
                val reset = w.resetsAt
                val value = if (reset != null && !reset.isAfter(now)) "초기화됨" else "${w.usedPercent.roundToInt()}%"
                "${Names.window(w.window)} $value"
            }
            val age = Duration.between(ws.maxOf { it.observedAt }, now)
            val ageText = when {
                age.toHours() >= 1 -> " (${age.toHours()}시간 전)"
                age.toMinutes() >= 30 -> " (${age.toMinutes()}분 전)"
                else -> ""
            }
            "${Names.provider(p)} $parts$ageText"
        }

    private fun windowOrder(w: String) = when (w) {
        "five_hour" -> 0
        "seven_day" -> 1
        else -> 2
    }

    private fun smallIcon(label: String, filled: Boolean): Icon {
        val size = 96
        val bmp = Bitmap.createBitmap(size, size, Bitmap.Config.ARGB_8888)
        val c = Canvas(bmp)
        val p = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = Color.WHITE
            typeface = Typeface.DEFAULT_BOLD
            textAlign = Paint.Align.CENTER
        }
        if (filled) {
            // 채운 원 + 숫자 모양으로 뚫기(결정 대기 표시)
            c.drawCircle(size / 2f, size / 2f, size / 2f, p)
            p.xfermode = PorterDuffXfermode(PorterDuff.Mode.CLEAR)
        }
        p.textSize = if (label.length > 1) size * 0.62f else size * 0.8f
        val y = size / 2f - (p.descent() + p.ascent()) / 2f
        c.drawText(label, size / 2f, y, p)
        return Icon.createWithBitmap(bmp)
    }
}
