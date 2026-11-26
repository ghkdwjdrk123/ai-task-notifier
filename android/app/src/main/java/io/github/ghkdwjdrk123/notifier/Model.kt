package io.github.ghkdwjdrk123.notifier

import org.json.JSONArray
import org.json.JSONObject
import java.time.Instant

/** 서버 스냅샷(GET /v1/snapshot, 스트림의 snapshot 이벤트). */
data class Snapshot(
    val version: Long,
    val serverTime: Instant,
    val running: Int,
    val waitingDecisions: Int,
    /** 턴을 마치고 내 다음 지시를 기다리는 세션 수(서버 0.2 이상). */
    val awaitingInput: Int,
    val degraded: Boolean,
    val sessions: List<SessionItem>,
    val decisions: List<DecisionItem>,
    val usage: List<UsageItem>,
) {
    companion object {
        fun parse(j: JSONObject) = Snapshot(
            version = j.getLong("version"),
            serverTime = Instant.parse(j.getString("server_time")),
            running = j.getInt("running"),
            waitingDecisions = j.getInt("waiting_decisions"),
            awaitingInput = j.optInt("awaiting_input"),
            degraded = j.optBoolean("degraded"),
            sessions = j.optJSONArray("sessions").objects().map(SessionItem::parse),
            decisions = j.optJSONArray("decisions").objects().map(DecisionItem::parse),
            usage = j.optJSONArray("usage").objects().map(UsageItem::parse),
        )
    }
}

data class SessionItem(
    val id: String,
    val provider: String,
    val title: String,
    val state: String,
    val updatedAt: Instant?,
) {
    companion object {
        fun parse(j: JSONObject) = SessionItem(
            j.getString("id"), j.getString("provider"), j.getString("title"), j.getString("state"),
            j.optStringOrNull("updated_at")?.let(Instant::parse),
        )
    }
}

data class DecisionItem(
    val id: String,
    val provider: String,
    val sessionTitle: String,
    val title: String,
    val state: String,
    val deadlineAt: Instant?,
    val version: Long,
) {
    companion object {
        fun parse(j: JSONObject) = DecisionItem(
            id = j.getString("id"),
            provider = j.getString("provider"),
            sessionTitle = j.getString("session_title"),
            title = j.getString("title"),
            state = j.getString("state"),
            deadlineAt = j.optStringOrNull("deadline_at")?.let(Instant::parse),
            version = j.getLong("version"),
        )
    }
}

data class UsageItem(
    val provider: String,
    val window: String,
    val usedPercent: Double,
    val resetsAt: Instant?,
    val status: String?,
    val observedAt: Instant,
) {
    companion object {
        fun parse(j: JSONObject) = UsageItem(
            provider = j.getString("provider"),
            window = j.getString("window"),
            usedPercent = j.getDouble("used_percent"),
            resetsAt = j.optStringOrNull("resets_at")?.let(Instant::parse),
            status = j.optStringOrNull("status"),
            observedAt = Instant.parse(j.getString("observed_at")),
        )
    }
}

/** 결정 상세(GET /v1/decisions/{id}). */
data class DecisionDetail(
    val summary: DecisionItem,
    val questions: List<Question>,
    val allowFreeText: Boolean,
    val rationale: String?,
    val closedReason: String?,
    val answerable: Boolean,
    val latestSubmission: Submission?,
) {
    companion object {
        fun parse(j: JSONObject) = DecisionDetail(
            summary = DecisionItem.parse(j),
            questions = j.getJSONArray("questions").objects().map(Question::parse),
            allowFreeText = j.optBoolean("allow_free_text"),
            rationale = j.optStringOrNull("rationale"),
            closedReason = j.optStringOrNull("closed_reason"),
            answerable = j.optBoolean("answerable"),
            latestSubmission = j.optJSONObject("latest_submission")?.let(Submission::parse),
        )
    }
}

data class Question(val text: String, val header: String?, val options: List<Option>, val multiSelect: Boolean) {
    data class Option(val label: String, val description: String?)

    companion object {
        fun parse(j: JSONObject) = Question(
            text = j.getString("question"),
            header = j.optStringOrNull("header"),
            options = j.optJSONArray("options").objects()
                .map { Option(it.getString("label"), it.optStringOrNull("description")) },
            multiSelect = j.optBoolean("multiSelect"),
        )
    }
}

data class Submission(val id: String, val state: String, val detail: String?) {
    companion object {
        fun parse(j: JSONObject) = Submission(j.getString("id"), j.getString("state"), j.optStringOrNull("detail"))
    }
}

fun JSONArray?.objects(): List<JSONObject> =
    if (this == null) emptyList() else (0 until length()).map { getJSONObject(it) }

fun JSONObject.optStringOrNull(name: String): String? =
    if (isNull(name) || !has(name)) null else getString(name)

/** 화면 표시용 이름. */
object Names {
    fun provider(p: String) = when (p) {
        "claude-code" -> "Claude"
        "codex" -> "ChatGPT"
        else -> p
    }

    fun window(w: String) = when {
        w == "five_hour" -> "5시간"
        w == "seven_day" -> "주간"
        w.startsWith("seven_day_") -> "주간(${w.removePrefix("seven_day_").replaceFirstChar { it.uppercase() }})"
        w.startsWith("window_") && w.endsWith("m") -> w.removePrefix("window_").removeSuffix("m").toIntOrNull()
            ?.let { m -> if (m % 1440 == 0) "${m / 1440}일" else if (m % 60 == 0) "${m / 60}시간" else "${m}분" } ?: w
        else -> w
    }

    /** 맨 위 요약 한 줄. */
    fun summary(s: Snapshot) = "진행 ${s.running} · 결정 대기 ${s.waitingDecisions} · 지시 대기 ${s.awaitingInput}"

    /** 세션 목록 순서: 내가 봐야 하는 것부터. */
    fun sessionOrder(s: String) = when (s) {
        "waiting_decision" -> 0
        "awaiting_input" -> 1
        "waiting_permission" -> 2
        "running" -> 3
        else -> 4
    }

    fun sessionState(s: String) = when (s) {
        "running" -> "진행 중"
        "awaiting_input" -> "지시 대기"
        "idle" -> "유휴"
        "waiting_decision" -> "결정 대기"
        "waiting_permission" -> "입력 대기(맥에서 확인)"
        else -> s
    }

    fun submissionState(s: String) = when (s) {
        "queued", "delivering" -> "전송 중"
        "applied" -> "반영 완료"
        "rejected" -> "반영되지 않음 — 다시 보낼 수 있어요"
        "unknown" -> "반영 확인 중"
        "cancelled" -> "취소됨"
        else -> s
    }

    fun closedReason(r: String?) = when (r) {
        "this_app" -> "이 앱의 답을 전달했어요"
        "elsewhere" -> "다른 곳(맥)에서 먼저 답했어요"
        "withdrawn" -> "요청이 철회됐어요"
        "expired" -> "마감이 지났어요"
        "session_ended" -> "세션이 끝났어요"
        "gone" -> "요청이 사라졌어요"
        "unconfirmed" -> "반영 여부를 확인하지 못했어요 — 원본에서 확인해 주세요"
        else -> r ?: ""
    }
}
