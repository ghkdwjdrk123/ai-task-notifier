package io.github.ghkdwjdrk123.notifier

import android.content.Context
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ProgressBar
import android.widget.TextView

/** 코드로 화면을 짜는 작은 도우미(외부 UI 라이브러리 없이). */
object Ui {
    fun dp(ctx: Context, v: Int) = (v * ctx.resources.displayMetrics.density).toInt()

    fun column(ctx: Context, padding: Int = 20) = LinearLayout(ctx).apply {
        orientation = LinearLayout.VERTICAL
        val p = dp(ctx, padding)
        setPadding(p, p, p, p)
    }

    fun row(ctx: Context) = LinearLayout(ctx).apply {
        orientation = LinearLayout.HORIZONTAL
        gravity = Gravity.CENTER_VERTICAL
    }

    fun text(ctx: Context, s: CharSequence, sizeSp: Float = 15f, bold: Boolean = false, muted: Boolean = false) =
        TextView(ctx).apply {
            text = s
            textSize = sizeSp
            if (bold) setTypeface(typeface, Typeface.BOLD)
            if (muted) alpha = 0.65f
            setPadding(0, dp(ctx, 2), 0, dp(ctx, 2))
        }

    fun heading(ctx: Context, s: String) = text(ctx, s, 13f, bold = true, muted = true).apply {
        setPadding(0, dp(ctx, 18), 0, dp(ctx, 6))
        letterSpacing = 0.04f
    }

    fun button(ctx: Context, label: String, onClick: () -> Unit) = Button(ctx).apply {
        text = label
        isAllCaps = false
        setOnClickListener { onClick() }
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
