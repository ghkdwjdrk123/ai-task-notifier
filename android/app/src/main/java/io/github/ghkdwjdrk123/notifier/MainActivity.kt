package io.github.ghkdwjdrk123.notifier

import android.Manifest
import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Bundle
import android.os.PowerManager
import android.provider.Settings
import android.text.InputType
import android.widget.EditText
import android.widget.LinearLayout
import java.time.Duration
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import kotlin.math.roundToInt

/** 처음에는 맥 등록 화면, 등록 뒤에는 상태 대시보드. */
class MainActivity : Activity() {
    private lateinit var prefs: Prefs
    private var body: LinearLayout? = null
    private val listener: (StatusHub.State) -> Unit = { renderDashboard(it) }
    private val clock = DateTimeFormatter.ofPattern("M/d HH:mm").withZone(ZoneId.systemDefault())

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        prefs = Prefs(this)
        if (prefs.isPaired) showDashboard() else showSetup(null)
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        renderDashboard(StatusHub.state)
    }

    override fun onResume() {
        super.onResume()
        if (prefs.isPaired) {
            if (prefs.statusEnabled) StatusService.start(this)
            StatusHub.listen(listener)
            Updater.visible = true
            checkUpdate()
        }
    }

    // ---------- 앱 업데이트 ----------

    /** 10분에 한 번 맥에 새 버전이 있는지 본다. */
    private fun checkUpdate(force: Boolean = false) {
        if (!force && System.currentTimeMillis() - Updater.lastCheckMs < 10 * 60_000) return
        val api = Api(prefs.serverUrl, prefs.token)
        Thread {
            runCatching { Updater.check(this, api) }
            runOnUiThread { if (!isDestroyed) renderDashboard(StatusHub.state) }
        }.start()
    }

    private fun startUpdate(l: Updater.Latest) {
        if (Updater.running.get()) return
        // 처음 한 번은 '이 앱에서 설치 허용'을 켜야 한다
        if (!packageManager.canRequestPackageInstalls()) {
            AlertDialog.Builder(this)
                .setTitle("설치 허용이 필요해요")
                .setMessage("새 버전을 설치하려면 이 앱에 '출처를 알 수 없는 앱 설치'를 허용해야 해요. 설정에서 켠 뒤 돌아와 다시 누르세요.")
                .setPositiveButton("설정 열기") { _, _ ->
                    startActivity(Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, Uri.parse("package:$packageName")))
                }
                .setNegativeButton("취소", null)
                .show()
            return
        }
        // 한 번에 하나만(화면 회전 등으로 다시 만들어져도)
        if (!Updater.running.compareAndSet(false, true)) return
        Updater.message = "새 버전을 받는 중…"
        renderDashboard(StatusHub.state)
        val api = Api(prefs.serverUrl, prefs.token)
        val app = applicationContext
        Thread {
            val err = runCatching { Updater.downloadAndInstall(app, api, l) }.exceptionOrNull()
            // 성공 문구는 설치 결과(InstallReceiver)가 정한다
            if (err != null) Updater.message = "업데이트 실패: ${err.message}"
            else if (Updater.message == "새 버전을 받는 중…") Updater.message = "설치 화면을 여는 중…"
            Updater.running.set(false)
            runOnUiThread { if (!isDestroyed) renderDashboard(StatusHub.state) }
        }.start()
    }

    override fun onPause() {
        Updater.visible = false
        StatusHub.unlisten(listener)
        super.onPause()
    }

    // ---------- 등록 ----------

    private fun showSetup(error: String?) {
        body = null
        val col = Ui.column(this, top = 4)
        col.addView(Ui.text(this, "맥에서 notifier pair로 만든 코드를 넣으세요. 폰의 Tailscale이 켜져 있어야 합니다.", muted = true))
        val url = EditText(this).apply { setTextColor(Ui.primary(context)); setHintTextColor(Ui.secondary(context));
            hint = "https://<맥 이름>.<tailnet>.ts.net:8443"
            setText(prefs.serverUrl)
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_URI
        }
        val code = EditText(this).apply { setTextColor(Ui.primary(context)); setHintTextColor(Ui.secondary(context));
            hint = "등록 코드(예: ABCDE-FGHJK)"
            inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_CAP_CHARACTERS
        }
        val name = EditText(this).apply { setTextColor(Ui.primary(context)); setHintTextColor(Ui.secondary(context));
            hint = "이 기기 이름"
            setText(prefs.deviceName)
        }
        listOf(Ui.heading(this, "서버 주소"), url, Ui.heading(this, "등록 코드"), code, Ui.heading(this, "기기 이름"), name)
            .forEach(col::addView)
        error?.let { col.addView(Ui.text(this, it).apply { setTextColor(0xFFDC2626.toInt()) }) }
        col.addView(Ui.gap(this, 12))
        col.addView(Ui.button(this, "등록") {
            val u = url.text.toString().trim().trimEnd('/')
            if (!u.startsWith("https://")) {
                showSetup("주소는 https://로 시작해야 합니다")
                return@button
            }
            val c = code.text.toString().trim()
            val n = name.text.toString().trim().ifEmpty { "갤럭시" }
            Thread {
                val result = runCatching { Api(u, null).pair(c, n) }
                runOnUiThread {
                    result.onSuccess { token ->
                        prefs.serverUrl = u
                        prefs.deviceName = n
                        prefs.token = token
                        prefs.statusEnabled = true
                        StatusHub.reset()
                        askNotificationPermission()
                        StatusService.start(this, reconnect = true)
                        showDashboard()
                        StatusHub.listen(listener)
                    }.onFailure { showSetup("등록 실패: ${it.message}") }
                }
            }.start()
        })
        Ui.screen(this, "맥 연결", null, col)
    }

    private fun askNotificationPermission() {
        if (checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 1)
        }
    }

    // ---------- 대시보드 ----------

    private fun showDashboard() {
        val col = Ui.column(this, top = 4)
        body = col
        Ui.screen(this, "AI 작업 알림", null, col)
        renderDashboard(StatusHub.state)
    }

    private fun renderDashboard(st: StatusHub.State) {
        val col = body ?: return
        col.removeAllViews()
        val snap = st.snapshot
        if (snap == null) {
            col.addView(Ui.text(this, "—", 26f, bold = true))
        } else {
            // 세 숫자를 칸으로: 내가 봐야 하는 칸(결정·지시 대기)이 0보다 크면 강조
            col.addView(Ui.statRow(this,
                Triple("진행", snap.running, false),
                Triple("결정 대기", snap.waitingDecisions, snap.waitingDecisions > 0),
                Triple("지시 대기", snap.awaitingInput, snap.awaitingInput > 0),
            ))
        }
        col.addView(Ui.status(this, linkText(st), linkColor(st)))

        Updater.available?.let { l ->
            val card = Ui.card(this, accent = true)
            card.addView(Ui.text(this, "새 버전 ${l.versionName}이 있어요", 16f, bold = true))
            card.addView(Ui.text(this, "지금 ${Updater.installedName(this)} · ${"%.1f".format(l.size / 1_048_576.0)}MB · 맥에서 받아요", 13f, muted = true))
            Updater.message?.let { card.addView(Ui.text(this, it, 14f)) }
            if (!Updater.running.get()) card.addView(Ui.button(this, "업데이트 설치") { startUpdate(l) })
            col.addView(card)
        }

        if (st.link == Link.UNAUTHORIZED) {
            col.addView(Ui.button(this, "다시 등록") {
                prefs.clearPairing()
                StatusService.stop(this)
                StatusHub.reset()
                showSetup("맥에서 연결이 해제됐어요. 새 코드로 다시 등록하세요")
            })
        }

        if (snap != null) {
            // 결정 대기
            if (snap.decisions.isNotEmpty()) {
                col.addView(Ui.heading(this, "내 답이 필요한 결정"))
                snap.decisions.forEach { d ->
                    val card = Ui.card(this, accent = d.state == "open")
                    card.addView(Ui.text(this, d.title, 16f, bold = true))
                    val due = d.deadlineAt?.let { "마감 ${clock.format(it)} (${remaining(it)})" } ?: "마감 없음"
                    card.addView(Ui.text(this, "${Names.provider(d.provider)} · ${d.sessionTitle} · $due", 13f, muted = true))
                    if (d.state != "open") card.addView(Ui.text(this, if (d.state == "confirming") "반영 확인 중" else "전송 중", 13f))
                    card.setOnClickListener { openDecision(d.id) }
                    col.addView(card)
                }
            }

            // 지시 대기: 턴을 마치고 내 다음 지시를 기다리는 세션
            val names = Names.displayTitles(snap.sessions)
            val awaiting = snap.sessions.filter { it.state == "awaiting_input" }
            if (awaiting.isNotEmpty()) {
                col.addView(Ui.heading(this, "내 지시를 기다리는 세션"))
                awaiting.sortedByDescending { it.stateSince }.forEach { s ->
                    val card = Ui.card(this, accent = true)
                    card.addView(Ui.text(this, names[s.id] ?: s.title, 16f, bold = true))
                    val ended = (s.stateSince ?: s.updatedAt)?.let { "${ago(Duration.between(it, Instant.now()))} 작업을 마침" } ?: "작업을 마침"
                    card.addView(Ui.text(this, "${Names.provider(s.provider)} · $ended · 맥에서 이어서 지시하세요", 13f, muted = true))
                    // 일부러 놔둔 세션은 '확인함'으로 수에서 뺀다(다시 일을 마치면 돌아옴)
                    card.addView(Ui.button(this, "확인함") { ackSessions(listOf(s.id)) })
                    col.addView(card)
                }
                if (awaiting.size > 1) col.addView(Ui.button(this, "모두 확인함 (${awaiting.size})") { ackSessions(awaiting.map { it.id }) })
            }

            // 사용량
            col.addView(Ui.heading(this, "구독 사용량"))
            if (snap.usage.isEmpty() && snap.usageErrors.isEmpty()) col.addView(Ui.text(this, "아직 보고된 사용량이 없어요", muted = true))
            // 사용량을 한 번도 못 받은 제공자도 실패 이유는 보여 준다
            val usageByProvider = snap.usage.groupBy { it.provider }.toMutableMap()
            snap.usageErrors.keys.forEach { usageByProvider.putIfAbsent(it, emptyList()) }
            usageByProvider.forEach { (p, ws) ->
                val card = Ui.card(this)
                card.addView(Ui.text(this, Names.provider(p), 16f, bold = true))
                snap.usageErrors[p]?.let { e ->
                    card.addView(Ui.text(this, "⚠ 사용량을 읽지 못하고 있어요: $e", 13f, bold = true).apply { setTextColor(0xFFD97706.toInt()) })
                }
                ws.sortedBy { it.window }.forEach { w ->
                    val pct = w.usedPercent.roundToInt()
                    // 초기화 시각이 지났으면 옛 값 대신 '초기화됨'(새 보고가 오면 다시 숫자)
                    val wasReset = w.resetsAt?.let { !it.isAfter(Instant.now()) } ?: false
                    val line = if (wasReset) {
                        "${Names.window(w.window)} 초기화됨 · 마지막 값 $pct%"
                    } else {
                        "${Names.window(w.window)} $pct%" + (w.resetsAt?.let { " · ${clock.format(it)} 초기화" } ?: "")
                    }
                    card.addView(Ui.text(this, line, 14f))
                    card.addView(Ui.bar(this, if (wasReset) 0 else pct))
                    card.addView(Ui.gap(this, 6))
                }
                if (ws.isNotEmpty()) {
                    val age = Duration.between(ws.maxOf { it.observedAt }, Instant.now())
                    card.addView(Ui.text(this, "마지막 보고: ${ago(age)}", 12f, muted = true))
                }
                col.addView(card)
            }

            // 세션
            col.addView(Ui.heading(this, "활성 세션"))
            if (snap.sessions.isEmpty()) col.addView(Ui.text(this, "활성 세션 없음", muted = true))
            snap.sessions.sortedBy { Names.sessionOrder(it.state) }.forEach { s ->
                col.addView(Ui.text(this, "• ${Names.provider(s.provider)} ${names[s.id] ?: s.title} — ${Names.stateWithElapsed(s)}"))
            }
        }

        // 설정
        col.addView(Ui.heading(this, "설정"))
        if (checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            col.addView(Ui.text(this, "알림 권한이 꺼져 있어 상시 알림이 보이지 않아요.", 14f, bold = true).apply { setTextColor(0xFFDC2626.toInt()) })
            col.addView(Ui.button(this, "알림 설정 열기") {
                startActivity(Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS).putExtra(Settings.EXTRA_APP_PACKAGE, packageName))
            })
        }
        val pm = getSystemService(PowerManager::class.java)
        if (!pm.isIgnoringBatteryOptimizations(packageName)) {
            col.addView(Ui.text(this, "화면이 꺼져도 연결이 유지되려면 배터리 최적화에서 이 앱을 빼야 합니다.", 13f, muted = true))
            col.addView(Ui.button(this, "배터리 최적화 해제") {
                startActivity(Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, Uri.parse("package:$packageName")))
            })
        }
        col.addView(
            Ui.buttonRow(
                this,
                Ui.button(this, "다시 연결") { StatusService.start(this, reconnect = true) },
                Ui.button(this, if (prefs.statusEnabled) "상시 알림 끄기" else "상시 알림 켜기") {
                    prefs.statusEnabled = !prefs.statusEnabled
                    if (prefs.statusEnabled) StatusService.start(this) else StatusService.stop(this)
                    renderDashboard(StatusHub.state)
                },
            ),
        )
        col.addView(Ui.button(this, "업데이트 확인 (지금 ${Updater.installedName(this)})") {
            Updater.message = null
            Thread {
                val r = runCatching { Updater.check(this, Api(prefs.serverUrl, prefs.token)) }
                runOnUiThread {
                    if (isDestroyed) return@runOnUiThread
                    val msg = r.fold({ if (it == null) "최신 버전이에요" else null }, { "확인 실패: ${it.message}" })
                    msg?.let { android.widget.Toast.makeText(this, it, android.widget.Toast.LENGTH_SHORT).show() }
                    renderDashboard(StatusHub.state)
                }
            }.start()
        })
        col.addView(Ui.button(this, "이 폰 등록 해제") {
            AlertDialog.Builder(this)
                .setMessage("이 폰의 연결 정보를 지우고 맥에서도 이 기기 등록을 해제합니다.")
                .setPositiveButton("해제") { _, _ ->
                    val api = prefs.token?.let { Api(prefs.serverUrl, it) }
                    StatusService.stop(this)
                    // 서버에서도 토큰을 무효로 한다(연결이 안 되면 맥에서 notifier revoke 필요)
                    Thread {
                        val ok = runCatching { api?.revokeSelf() }.isSuccess && api != null
                        runOnUiThread {
                            prefs.clearPairing()
                            StatusHub.reset()
                            showSetup(if (ok) null else "폰의 연결 정보는 지웠지만 맥에 알리지 못했어요. 맥에서 notifier revoke로 지워 주세요")
                        }
                    }.start()
                }
                .setNegativeButton("취소", null)
                .show()
        })
        col.addView(Ui.text(this, prefs.serverUrl, 12f, muted = true))
    }

    /** 지시 대기 세션을 '확인함'으로. 결과는 실시간 스트림으로 화면에 돌아온다. */
    private fun ackSessions(ids: List<String>) {
        val api = Api(prefs.serverUrl, prefs.token)
        Thread {
            val failed = ids.count { runCatching { api.ackSession(it) }.isFailure }
            runOnUiThread {
                if (isDestroyed) return@runOnUiThread
                if (failed > 0) android.widget.Toast.makeText(this, "맥에 알리지 못했어요. 연결을 확인하세요", android.widget.Toast.LENGTH_SHORT).show()
            }
        }.start()
    }

    private fun openDecision(id: String) {
        startActivity(Intent(this, DecisionActivity::class.java).putExtra(DecisionActivity.EXTRA_ID, id))
    }

    private fun linkColor(st: StatusHub.State): Int = when {
        st.link == Link.LIVE && st.snapshot?.degraded != true -> 0xFF22C55E.toInt() // 초록
        st.link == Link.LIVE || st.link == Link.CONNECTING -> 0xFFF59E0B.toInt() // 주황
        else -> 0xFFEF4444.toInt() // 빨강
    }

    private fun linkText(st: StatusHub.State): String = when (st.link) {
        Link.LIVE -> if (st.snapshot?.degraded == true) "연결됨 · 일부 제공자 갱신 지연" else "연결됨 · 실시간"
        Link.CONNECTING -> "맥에 연결 중…"
        Link.DISCONNECTED -> "연결 끊김" + (st.receivedAt?.let { " · ${clock.format(it)} 기준 값" } ?: "") + (st.error?.let { " · $it" } ?: "")
        Link.UNAUTHORIZED -> "등록이 해제됐어요"
    }

    private fun remaining(t: Instant): String {
        val d = Duration.between(Instant.now(), t)
        return when {
            d.isNegative -> "지남"
            d.toHours() >= 1 -> "${d.toHours()}시간 ${d.toMinutesPart()}분 남음"
            else -> "${d.toMinutes()}분 남음"
        }
    }

    private fun ago(d: Duration): String = when {
        d.toMinutes() < 1 -> "방금"
        d.toHours() < 1 -> "${d.toMinutes()}분 전"
        d.toDays() < 1 -> "${d.toHours()}시간 전"
        else -> "${d.toDays()}일 전"
    }
}
