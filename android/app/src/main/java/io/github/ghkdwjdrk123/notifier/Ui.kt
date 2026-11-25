package io.github.ghkdwjdrk123.notifier

import android.app.Activity
import android.content.Context
import android.content.res.Configuration
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.Button
import android.widget.LinearLayout
import android.view.WindowInsets
import android.widget.ProgressBar
import android.widget.ScrollView
import android.widget.TextView

/** 코드로 화면을 짜는 작은 도우미(외부 UI 라이브러리 없이). */
object Ui {
    fun dp(ctx: Context, v: Int) = (v * ctx.resources.displayMetrics.density).toInt()

    fun column(ctx: Context, padding: Int = 20, top: Int = padding) = LinearLayout(ctx).apply {
        orientation = LinearLayout.VERTICAL
        val p = dp(ctx, padding)
        setPadding(p, dp(ctx, top), p, p)
    }

    fun row(ctx: Context) = LinearLayout(ctx).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
    }

    private fun isDark(ctx: Context) =
        (ctx.resources.configuration.uiMode and Configuration.UI_MODE_NIGHT_MASK) == Configuration.UI_MODE_NIGHT_YES

    /** 본문 글자색. 기기 테마 기본색(삼성은 회색빛)이 아니라 대비가 충분한 색을 직접 쓴다. */
    fun primary(ctx: Context) = if (isDark(ctx)) Color.parseColor("#F3F4F6") else Color.parseColor("#111827")

    fun secondary(ctx: Context) = if (isDark(ctx)) Color.parseColor("#9CA3AF") else Color.parseColor("#6B7280")

    fun text(ctx: Context, s: CharSequence, sizeSp: Float = 15f, bold: Boolean = false, muted: Boolean = false) =
        TextView(ctx).apply {
            text = s
            textSize = sizeSp
            if (bold) setTypeface(typeface, Typeface.BOLD)
            setTextColor(if (muted) secondary(ctx) else primary(ctx))
            setPadding(0, dp(ctx, 2), 0, dp(ctx, 2))
        }

    fun heading(ctx: Context, s: String) = text(ctx, s, 13f, bold = true, muted = true).apply {
        setPadding(0, dp(ctx, 22), 0, dp(ctx, 4))
        letterSpacing = 0.04f
    }

    /** 가로 폭을 채우는 버튼(위쪽 간격 10dp). */
    fun button(ctx: Context, label: String, onClick: () -> Unit) = Button(ctx).apply {
        text = label
        isAllCaps = false
        setOnClickListener { onClick() }
        layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT).apply {
            topMargin = dp(ctx, 10)
        }
    }

    /**
     * 버튼 여러 개를 같은 폭으로 한 줄에(사이 간격 10dp).
     * 위쪽 간격은 줄에 준다: 가운데 정렬 줄 안의 버튼에 위쪽 여백을 주면 버튼이 아래로 밀려 잘린다.
     */
    fun buttonRow(ctx: Context, vararg buttons: Button) = LinearLayout(ctx).apply {
        orientation = LinearLayout.HORIZONTAL
        layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT).apply {
            topMargin = dp(ctx, 10)
        }
        buttons.forEachIndexed { i, b ->
            b.layoutParams = LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f).apply {
                if (i > 0) marginStart = dp(ctx, 10)
            }
            addView(b)
        }
    }

    /** 색 점이 붙은 한 줄 상태 표시(예: ● 연결됨). */
    fun status(ctx: Context, label: String, dotColor: Int) = text(ctx, android.text.SpannableString("●  $label").apply {
        setSpan(android.text.style.ForegroundColorSpan(dotColor), 0, 1, android.text.Spanned.SPAN_EXCLUSIVE_EXCLUSIVE)
    }, 14f, muted = true)

    /**
     * 화면 틀: 얇은 머리글(뒤로 가기·제목) + 스크롤 본문.
     * 상태바·내비게이션 바·카메라 구멍 영역만큼 안쪽 여백을 줘서 내용이 가려지지 않게 한다(Android 15+ 전체 화면 그리기).
     */
    fun screen(act: Activity, title: String, onBack: (() -> Unit)?, content: View) {
        val ctx: Context = act
        val header = row(ctx).apply {
            setPadding(dp(ctx, if (onBack != null) 4 else 20), dp(ctx, 8), dp(ctx, 20), dp(ctx, 4))
            if (onBack != null) {
                addView(TextView(ctx).apply {
                    text = "‹"
                    textSize = 30f
                    gravity = Gravity.CENTER
                    contentDescription = "뒤로 가기"
                    isClickable = true
                    isFocusable = true
                    background = ctx.getDrawable(android.R.drawable.list_selector_background)
                    setOnClickListener { onBack() }
                    layoutParams = LinearLayout.LayoutParams(dp(ctx, 48), dp(ctx, 48))
                })
            }
            addView(text(ctx, title, 18f, bold = true))
        }
        val scroll = ScrollView(ctx).apply {
            addView(content)
            layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f)
        }
        val root = LinearLayout(ctx).apply {
            orientation = LinearLayout.VERTICAL
            addView(header)
            addView(scroll)
            setOnApplyWindowInsetsListener { v, insets ->
                val b = insets.getInsets(WindowInsets.Type.systemBars() or WindowInsets.Type.displayCutout() or WindowInsets.Type.ime())
                v.setPadding(b.left, b.top, b.right, b.bottom)
                WindowInsets.CONSUMED
            }
        }
        act.setContentView(root)
        root.requestApplyInsets()
    }

    /** 테두리가 있는 카드. 밝은·어두운 화면 모두에서 보이도록 반투명 회색 테두리. */
    fun card(ctx: Context, accent: Boolean = false) = column(ctx, 14).apply {
        background = GradientDrawable().apply {
            cornerRadius = dp(ctx, 12).toFloat()
            setStroke(dp(ctx, 1), if (accent) Color.parseColor("#D97706") else Color.parseColor("#55888888"))
            if (accent) setColor(Color.parseColor("#1AD97706"))
        }
        layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT).apply {
            bottomMargin = dp(ctx, 10)
        }
    }

    fun bar(ctx: Context, percent: Int) = ProgressBar(ctx, null, android.R.attr.progressBarStyleHorizontal).apply {
        max = 100
        progress = percent.coerceIn(0, 100)
        layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, dp(ctx, 10))
    }

    fun gap(ctx: Context, h: Int = 8) = View(ctx).apply {
        layoutParams = LinearLayout.LayoutParams(1, dp(ctx, h))
    }
}
