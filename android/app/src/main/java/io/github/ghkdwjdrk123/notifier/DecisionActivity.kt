package io.github.ghkdwjdrk123.notifier

import android.app.Activity
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.widget.CheckBox
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.RadioButton
import android.widget.RadioGroup
import android.widget.ScrollView
import org.json.JSONArray
import org.json.JSONObject
import java.time.Duration
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import java.util.UUID

/**
 * 결정 요청 상세와 답변.
 * 같은 답을 다시 보낼 때(네트워크 오류 등)는 같은 제출 ID를 써서 서버가 한 번만 반영하게 한다.
 */
class DecisionActivity : Activity() {
    private lateinit var api: Api
    private lateinit var decisionId: String
    private var submissionId: String = UUID.randomUUID().toString()
    private val main = Handler(Looper.getMainLooper())
    private val clock = DateTimeFormatter.ofPattern("M/d HH:mm").withZone(ZoneId.systemDefault())
    private var polling = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val prefs = Prefs(this)
        val token = prefs.token
        decisionId = intent.getStringExtra(EXTRA_ID) ?: return finish()
        if (token == null) {
            setContentView(Ui.column(this).apply {
                addView(Ui.text(this@DecisionActivity, "연결 정보가 없어요. 앱을 열어 맥에 다시 등록해 주세요."))
            })
            return
        }
        savedInstanceState?.getString(KEY_SUB)?.let { submissionId = it }
        api = Api(prefs.serverUrl, token)
        title = "결정"
        load(null)
    }

    override fun onSaveInstanceState(out: Bundle) {
        super.onSaveInstanceState(out)
        out.putString(KEY_SUB, submissionId)
    }

    override fun onDestroy() {
        polling = false
        super.onDestroy()
    }

    private fun load(message: String?) {
        if (isDestroyed) return
        setContentView(Ui.column(this).apply { addView(Ui.text(this@DecisionActivity, "불러오는 중…", muted = true)) })
        Thread {
            val r = runCatching { api.decision(decisionId) }
            runOnUiThread {
                r.onSuccess { render(it, message) }
                    .onFailure { e ->
                        setContentView(Ui.column(this).apply {
                            addView(Ui.text(this@DecisionActivity, "불러오지 못했어요: ${e.message}"))
                            addView(Ui.button(this@DecisionActivity, "다시 시도") { load(null) })
                        })
                    }
            }
        }.start()
    }

    private fun render(d: DecisionDetail, message: String?) {
        val col = Ui.column(this)
        col.addView(Ui.text(this, d.summary.title, 20f, bold = true))
        val due = d.summary.deadlineAt?.let { "마감 ${clock.format(it)} · ${remaining(it)}" } ?: "마감 없음"
        col.addView(Ui.text(this, "${Names.provider(d.summary.provider)} · ${d.summary.sessionTitle} · $due", 13f, muted = true))
        d.rationale?.let { col.addView(Ui.card(this).apply { addView(Ui.text(this@DecisionActivity, "추천 이유: $it", 14f)) }) }
        message?.let { col.addView(Ui.card(this, accent = true).apply { addView(Ui.text(this@DecisionActivity, it, 15f, bold = true)) }) }

        if (!d.answerable) {
            // 이미 닫혔거나 전송·확인 중
            val state = when (d.summary.state) {
                "sending" -> "답을 전송 중이에요"
                "confirming" -> "반영 확인 중이에요. 원본에서 결과를 확인하는 중입니다"
                else -> Names.closedReason(d.closedReason)
            }
            col.addView(Ui.card(this).apply { addView(Ui.text(this@DecisionActivity, state, 15f)) })
            d.latestSubmission?.let { col.addView(Ui.text(this, "마지막 제출: ${Names.submissionState(it.state)}", 13f, muted = true)) }
            d.questions.forEach { q -> col.addView(Ui.text(this, "• ${q.text}", 14f, muted = true)) }
            if (d.summary.state == "sending" || d.summary.state == "confirming") {
                col.addView(Ui.button(this, "새로고침") { load(null) })
            }
            setContentView(ScrollView(this).apply { addView(col) })
            return
        }

        // 질문별 입력
        val readers = mutableListOf<() -> Pair<String, Any?>>()
        d.questions.forEach { q ->
            val card = Ui.card(this)
            q.header?.let { card.addView(Ui.text(this, it, 12f, bold = true, muted = true)) }
            card.addView(Ui.text(this, q.text, 16f, bold = true))
            val free = if (d.allowFreeText) EditText(this).apply { hint = "직접 입력(선택지 대신)" } else null
            if (q.multiSelect) {
                val boxes = q.options.map { o -> CheckBox(this).apply { text = label(o) } }
                boxes.forEach(card::addView)
                readers += {
                    val picked = q.options.filterIndexed { i, _ -> boxes[i].isChecked }.map { it.label }
                    q.text to (if (picked.isEmpty()) null else JSONArray(picked))
                }
            } else {
                val group = RadioGroup(this)
                q.options.forEachIndexed { i, o -> group.addView(RadioButton(this).apply { id = 1000 + i; text = label(o) }) }
                card.addView(group)
                readers += {
                    val typed = free?.text?.toString()?.trim().orEmpty()
                    val idx = group.checkedRadioButtonId - 1000
                    q.text to when {
                        typed.isNotEmpty() -> typed
                        idx in q.options.indices -> q.options[idx].label
                        else -> null
                    }
                }
            }
            free?.let(card::addView)
            col.addView(card)
        }

        val status = Ui.text(this, "", 14f)
        col.addView(status)
        val send = Ui.button(this, "보내기") {}
        send.setOnClickListener {
            val answers = JSONObject()
            for (read in readers) {
                val (key, value) = read()
                if (value == null) {
                    status.text = "모든 질문에 답해 주세요"
                    return@setOnClickListener
                }
                answers.put(key, value)
            }
            send.isEnabled = false
            status.text = "전송 중…"
            submit(d, answers, status) { send.isEnabled = true }
        }
        col.addView(send)
        setContentView(ScrollView(this).apply { addView(col) })
    }

    private fun submit(d: DecisionDetail, answers: JSONObject, status: android.widget.TextView, reenable: () -> Unit) {
        Thread {
            val r = runCatching { api.submit(decisionId, submissionId, d.summary.version, answers) }
            runOnUiThread {
                if (isDestroyed) return@runOnUiThread
                r.onSuccess { sub ->
                    status.text = Names.submissionState(sub.state)
                    poll(sub.id, 0)
                }.onFailure { e ->
                    when {
                        e is ApiException && e.code == "stale_version" -> {
                            submissionId = UUID.randomUUID().toString()
                            load("요청이 바뀌었어요. 다시 확인하고 보내 주세요")
                        }
                        e is ApiException && e.code == "submission_id_reused" -> {
                            // 앞서 보낸(응답을 못 받은) 답과 내용이 다름: 이전 제출 결과를 먼저 보여 주고 새 ID로 다시 보낼 수 있게
                            val old = submissionId
                            submissionId = UUID.randomUUID().toString()
                            Thread {
                                val prev = runCatching { api.submission(decisionId, old) }.getOrNull()
                                runOnUiThread {
                                    if (!isDestroyed) load(prev?.let { "앞서 보낸 답: ${Names.submissionState(it.state)}" } ?: e.message)
                                }
                            }.start()
                        }
                        e is ApiException && e.code == "not_open" -> load(e.message)
                        else -> {
                            // 네트워크 오류: 같은 제출 ID로 다시 보낼 수 있게 둔다
                            status.text = "보내지 못했어요(${e.message}). 다시 누르면 같은 답을 한 번만 보냅니다"
                            reenable()
                        }
                    }
                }
            }
        }.start()
    }

    /** 반영 결과가 나올 때까지 1초 간격으로 최대 60초 확인. */
    private fun poll(subId: String, n: Int) {
        polling = true
        Thread {
            val r = runCatching { api.submission(decisionId, subId) }
            runOnUiThread {
                if (!polling || isDestroyed) return@runOnUiThread
                val s = r.getOrNull()
                when {
                    s != null && s.state in setOf("applied", "rejected", "cancelled") -> {
                        if (s.state == "rejected") submissionId = UUID.randomUUID().toString()
                        load(Names.submissionState(s.state) + (s.detail?.takeIf { s.state != "applied" }?.let { " ($it)" } ?: ""))
                    }
                    n >= 60 -> load("아직 결과를 모르고 있어요. 잠시 뒤 다시 확인해 주세요")
                    else -> main.postDelayed({ if (!isDestroyed) poll(subId, n + 1) }, 1_000)
                }
            }
        }.start()
    }

    private fun label(o: Question.Option) = if (o.description.isNullOrBlank()) o.label else "${o.label}\n${o.description}"

    private fun remaining(t: Instant): String {
        val d = Duration.between(Instant.now(), t)
        return when {
            d.isNegative -> "지남"
            d.toHours() >= 1 -> "${d.toHours()}시간 ${d.toMinutesPart()}분 남음"
            else -> "${d.toMinutes()}분 남음"
        }
    }

    companion object {
        const val EXTRA_ID = "decision_id"
        private const val KEY_SUB = "submission_id"
    }
}
