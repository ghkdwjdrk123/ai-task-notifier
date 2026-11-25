package io.github.ghkdwjdrk123.notifier

import android.os.Handler
import android.os.Looper
import java.time.Instant
import java.util.concurrent.CopyOnWriteArraySet

/** 연결 상태. */
enum class Link { CONNECTING, LIVE, DISCONNECTED, UNAUTHORIZED }

/** 서비스가 받은 최신 상태를 화면들과 나눈다(같은 프로세스 안). */
object StatusHub {
    data class State(
        val snapshot: Snapshot?,
        val link: Link,
        /** 마지막으로 스냅샷을 받은 시각(폰 시계). */
        val receivedAt: Instant?,
        val error: String?,
    )

    private val main = Handler(Looper.getMainLooper())
    private val listeners = CopyOnWriteArraySet<(State) -> Unit>()

    @Volatile
    var state = State(null, Link.CONNECTING, null, null)
        private set

    /** 연결·오류 상태만 바꾼다(스냅샷은 그대로). */
    fun update(f: (State) -> State) {
        synchronized(this) { state = f(state) }
        publish()
    }

    /**
     * 스냅샷을 받았다. 같은 연결 안에서는 옛 버전으로 되돌아가지 않는다(F09).
     * 새 연결의 첫 스냅샷은 무조건 받는다(서버 DB를 새로 만들어 버전이 낮아진 경우 등).
     */
    fun received(snap: Snapshot, firstOnConnection: Boolean) {
        synchronized(this) {
            val old = state.snapshot
            val keep = !firstOnConnection && old != null && snap.version < old.version
            state = State(if (keep) old else snap, Link.LIVE, Instant.now(), null)
        }
        publish()
    }

    /** 시간이 지나 표시만 바뀌어야 할 때(갱신 지연·초기화 시각 경과) 다시 그리게 한다. */
    fun tick() = publish()

    private fun publish() {
        val s = state
        main.post { listeners.forEach { it(s) } }
    }

    fun listen(l: (State) -> Unit) {
        listeners.add(l)
        l(state)
    }

    fun unlisten(l: (State) -> Unit) {
        listeners.remove(l)
    }

    fun reset() {
        update { State(null, Link.CONNECTING, null, null) }
    }

    /** 스트림이 살아 있어도 이 시간 넘게 새 스냅샷이 없으면 값이 멈춘 것으로 본다(서버는 30초마다 보냄). */
    const val STALE_AFTER_SECONDS = 90L

    fun isStale(st: State, now: Instant = Instant.now()): Boolean =
        st.receivedAt?.let { now.epochSecond - it.epochSecond > STALE_AFTER_SECONDS } ?: false
}
