package io.github.ghkdwjdrk123.notifier

import android.app.ForegroundServiceStartNotAllowedException
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.Network
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.util.Log
import java.net.HttpURLConnection
import java.util.concurrent.atomic.AtomicReference

/**
 * 상시 상태 알림을 유지하는 포그라운드 서비스.
 * 맥 서버의 실시간 스트림(/v1/stream) 하나를 붙잡고, 스냅샷이 올 때마다 알림을 고친다.
 * 끊기면 2초부터 최대 60초까지 늘려 가며 다시 붙고, 끊긴 상태에서 네트워크가 돌아오면 바로 다시 붙는다.
 * 푸시는 쓰지 않는다.
 */
class StatusService : Service() {
    /** 서비스가 살아 있는 동안 true(onDestroy에서 false). */
    @Volatile private var running = true
    /** 기다리는 중에 다시 연결하라는 신호가 왔는지(wait 전에 온 신호도 놓치지 않게). */
    @Volatile private var kicked = false
    private var worker: Thread? = null
    private val current = AtomicReference<HttpURLConnection?>(null)
    private val wake = Object()
    private var netCallback: ConnectivityManager.NetworkCallback? = null
    private val main = Handler(Looper.getMainLooper())
    private var shown: StatusNotification.Content? = null

    private val listener: (StatusHub.State) -> Unit = { st -> show(st) }

    /** 30초마다 다시 그려 '○○ 기준'·'초기화됨' 같은 시간 표시를 고친다(내용이 같으면 건너뜀). */
    private val ticker = object : Runnable {
        override fun run() {
            StatusHub.tick()
            main.postDelayed(this, 30_000)
        }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        StatusHub.listen(listener)
        val cm = getSystemService(ConnectivityManager::class.java)
        netCallback = object : ConnectivityManager.NetworkCallback() {
            // 연결이 살아 있으면 건드리지 않는다(등록 직후 콜백이 막 열린 연결을 끊지 않게)
            override fun onAvailable(network: Network) {
                if (StatusHub.state.link != Link.LIVE) kick()
            }
        }.also { cm.registerDefaultNetworkCallback(it) }
        main.postDelayed(ticker, 30_000)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val content = StatusNotification.content(StatusHub.state)
        try {
            startForeground(
                StatusNotification.ID,
                StatusNotification.build(this, content),
                ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE,
            )
            shown = content
        } catch (e: ForegroundServiceStartNotAllowedException) {
            // 백그라운드 시작이 막힌 경우: 다음에 앱을 열 때 다시 시작한다
            Log.w(TAG, "포그라운드 서비스 시작 불가", e)
            stopSelf()
            return START_NOT_STICKY
        }
        if (intent?.action == ACTION_RECONNECT) kick()
        // 작업 스레드가 없거나(첫 시작) 등록 해제로 끝났으면(다시 등록 후) 새로 띄운다
        if (worker?.isAlive != true) {
            worker = Thread({ loop() }, "status-stream").apply { isDaemon = true; start() }
        }
        return START_STICKY
    }

    private fun show(st: StatusHub.State) {
        val content = StatusNotification.content(st)
        if (content == shown) return
        shown = content
        getSystemService(NotificationManager::class.java)
            .notify(StatusNotification.ID, StatusNotification.build(this, content))
    }

    /** 기다리는 중이면 바로 다시 연결한다. */
    private fun kick() {
        kicked = true
        current.get()?.disconnect()
        synchronized(wake) { wake.notifyAll() }
    }

    private fun loop() {
        val prefs = Prefs(this)
        var backoff = 2_000L
        while (running) {
            kicked = false
            val url = prefs.serverUrl
            val token = prefs.token
            if (url.isEmpty() || token == null) {
                StatusHub.update { it.copy(link = Link.UNAUTHORIZED, error = null) }
                return
            }
            StatusHub.update { it.copy(link = if (it.snapshot == null) Link.CONNECTING else it.link) }
            var first = true
            try {
                Api(url, token).stream(
                    onConnection = { current.set(it) },
                    isCancelled = { !running },
                ) { snap ->
                    backoff = 2_000L
                    StatusHub.received(snap, firstOnConnection = first)
                    first = false
                }
                StatusHub.update { it.copy(link = Link.DISCONNECTED, error = "서버가 연결을 닫았어요") }
            } catch (e: ApiException) {
                if (e.status == 401) {
                    StatusHub.update { it.copy(link = Link.UNAUTHORIZED, error = e.message) }
                    return // 다시 등록해야 함: 재시도하지 않는다
                }
                StatusHub.update { it.copy(link = Link.DISCONNECTED, error = e.message) }
            } catch (e: Exception) {
                StatusHub.update { it.copy(link = Link.DISCONNECTED, error = "연결 끊김: ${e.javaClass.simpleName}") }
            } finally {
                current.set(null)
            }
            if (!running) break
            synchronized(wake) {
                if (!kicked) wake.wait(backoff)
            }
            backoff = if (kicked) 2_000L else (backoff * 2).coerceAtMost(60_000L)
        }
    }

    override fun onDestroy() {
        running = false
        main.removeCallbacks(ticker)
        StatusHub.unlisten(listener)
        netCallback?.let { getSystemService(ConnectivityManager::class.java).unregisterNetworkCallback(it) }
        kick()
        super.onDestroy()
    }

    companion object {
        private const val TAG = "StatusService"
        const val ACTION_RECONNECT = "io.github.ghkdwjdrk123.notifier.RECONNECT"

        /** 서비스를 시작한다. 시스템이 막으면(백그라운드 제한) false. */
        fun start(ctx: Context, reconnect: Boolean = false): Boolean {
            val i = Intent(ctx, StatusService::class.java)
            if (reconnect) i.action = ACTION_RECONNECT
            return try {
                ctx.startForegroundService(i)
                true
            } catch (e: ForegroundServiceStartNotAllowedException) {
                Log.w(TAG, "포그라운드 서비스 시작이 막혔습니다", e)
                false
            }
        }

        fun stop(ctx: Context) {
            ctx.stopService(Intent(ctx, StatusService::class.java))
        }
    }
}
