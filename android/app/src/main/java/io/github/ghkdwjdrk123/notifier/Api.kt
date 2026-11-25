package io.github.ghkdwjdrk123.notifier

import org.json.JSONObject
import java.io.BufferedReader
import java.io.IOException
import java.io.InputStreamReader
import java.net.HttpURLConnection
import java.net.URL

/** 서버 오류 응답({error, message}). */
class ApiException(val status: Int, val code: String, message: String) : IOException(message)

/**
 * 맥 서버 API. 모든 호출은 막는(blocking) 호출이라 UI 스레드에서 부르면 안 된다.
 * 주소는 https만 받는다(Tailscale Serve 인증서).
 */
class Api(private val baseUrl: String, private val token: String?) {

    private fun open(path: String, method: String): HttpURLConnection {
        require(baseUrl.startsWith("https://")) { "서버 주소는 https://로 시작해야 합니다" }
        val c = URL(baseUrl + path).openConnection() as HttpURLConnection
        c.requestMethod = method
        c.connectTimeout = 10_000
        c.readTimeout = 20_000
        c.useCaches = false
        c.instanceFollowRedirects = false
        c.setRequestProperty("Accept", "application/json")
        token?.let { c.setRequestProperty("Authorization", "Bearer $it") }
        return c
    }

    private fun send(path: String, method: String, body: JSONObject?): JSONObject {
        val c = open(path, method)
        try {
            if (body != null) {
                c.doOutput = true
                c.setRequestProperty("Content-Type", "application/json")
                c.outputStream.use { it.write(body.toString().toByteArray(Charsets.UTF_8)) }
            }
            val code = c.responseCode
            val stream = if (code in 200..299) c.inputStream else c.errorStream
            val text = stream?.bufferedReader(Charsets.UTF_8)?.use { it.readText() } ?: ""
            if (code !in 200..299) {
                val j = runCatching { JSONObject(text) }.getOrNull()
                throw ApiException(code, j?.optString("error") ?: "http_$code", j?.optString("message") ?: "HTTP $code")
            }
            return if (text.isBlank()) JSONObject() else JSONObject(text)
        } finally {
            c.disconnect()
        }
    }

    /** 1회용 코드로 등록하고 기기 토큰을 받는다. */
    fun pair(code: String, name: String): String {
        val res = send("/v1/pairing/exchange", "POST", JSONObject().put("code", code).put("name", name).put("token_in_body", true))
        return res.getString("token")
    }

    /** 서버에서 이 기기의 등록을 해제한다(토큰 무효화). */
    fun revokeSelf() {
        send("/v1/device", "DELETE", null)
    }

    fun snapshot(): Snapshot = Snapshot.parse(send("/v1/snapshot", "GET", null))

    fun decision(id: String): DecisionDetail = DecisionDetail.parse(send("/v1/decisions/${enc(id)}", "GET", null))

    /** 답을 보낸다. 반환: 제출 상태. */
    fun submit(decisionId: String, submissionId: String, version: Long, answers: JSONObject): Submission {
        val body = JSONObject().put("submission_id", submissionId).put("decision_version", version).put("answers", answers)
        return Submission.parse(send("/v1/decisions/${enc(decisionId)}/answers", "POST", body))
    }

    fun submission(decisionId: String, submissionId: String): Submission =
        Submission.parse(send("/v1/decisions/${enc(decisionId)}/answers/${enc(submissionId)}", "GET", null))

    /**
     * 실시간 스트림(SSE)을 읽는다. 연결이 끊기거나 [isCancelled]가 true가 될 때까지 돌아오지 않는다.
     * 서버는 15초마다 keep-alive 주석을 보내므로 읽기 제한(45초)을 넘기면 끊긴 것으로 본다.
     */
    fun stream(onConnection: (HttpURLConnection) -> Unit, isCancelled: () -> Boolean, onSnapshot: (Snapshot) -> Unit) {
        val c = open("/v1/stream", "GET")
        c.readTimeout = 45_000
        c.setRequestProperty("Accept", "text/event-stream")
        onConnection(c)
        try {
            val code = c.responseCode
            if (code != 200) {
                val text = c.errorStream?.bufferedReader()?.use { it.readText() } ?: ""
                val j = runCatching { JSONObject(text) }.getOrNull()
                throw ApiException(code, j?.optString("error") ?: "http_$code", j?.optString("message") ?: "HTTP $code")
            }
            BufferedReader(InputStreamReader(c.inputStream, Charsets.UTF_8)).use { r ->
                var event = ""
                val data = StringBuilder()
                while (!isCancelled()) {
                    val line = r.readLine() ?: break
                    when {
                        line.isEmpty() -> {
                            if (event == "snapshot" && data.isNotEmpty()) onSnapshot(Snapshot.parse(JSONObject(data.toString())))
                            event = ""
                            data.setLength(0)
                        }
                        line.startsWith(":") -> {} // keep-alive
                        line.startsWith("event:") -> event = line.substring(6).trim()
                        line.startsWith("data:") -> {
                            if (data.isNotEmpty()) data.append('\n')
                            data.append(line.substring(5).trimStart())
                        }
                    }
                }
            }
        } finally {
            c.disconnect()
        }
    }

    private fun enc(s: String) = java.net.URLEncoder.encode(s, "UTF-8")
}
