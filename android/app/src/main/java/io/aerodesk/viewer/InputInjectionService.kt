package io.aerodesk.viewer

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.graphics.Path
import android.os.Handler
import android.os.Looper
import android.view.accessibility.AccessibilityEvent
import org.json.JSONObject

/**
 * 无障碍注入服务：轮询 Rust publisher 收到的观看端输入事件，
 * 通过 dispatchGesture 应用到本设备（被控端）。
 * 需用户在系统设置中开启无障碍服务；本服务由系统绑定，不通过 startService 启动。
 */
class InputInjectionService : AccessibilityService() {
    companion object {
        @Volatile var pendingPtr: Long = 0L

        /** 当前已绑定的服务实例（用户已在系统设置开启无障碍服务时非空）。 */
        @Volatile var instance: InputInjectionService? = null
    }

    private val handler = Handler(Looper.getMainLooper())
    private var ptr = 0L
    private var running = false
    private var screenW = 0
    private var screenH = 0

    override fun onServiceConnected() {
        super.onServiceConnected()
        instance = this
        screenW = resources.displayMetrics.widthPixels
        screenH = resources.displayMetrics.heightPixels
        if (pendingPtr != 0L) {
            val ptr = pendingPtr
            pendingPtr = 0L
            start(ptr)
        }
    }

    /** 绑定发布会话指针后启动轮询。 */
    fun start(ptr: Long) {
        if (ptr == 0L || (this.ptr == ptr && running)) return
        this.ptr = ptr
        running = true
        handler.removeCallbacks(pollRunnable)
        handler.post(pollRunnable)
    }

    fun stop() {
        running = false
        handler.removeCallbacks(pollRunnable)
        ptr = 0L
    }

    private val pollRunnable = object : Runnable {
        override fun run() {
            if (!running || ptr == 0L) return
            val json = NativeBridge.publisherTakeInput(ptr)
            if (json.isNotEmpty()) {
                handleEvent(JSONObject(json))
            }
            handler.postDelayed(this, 16)
        }
    }

    private fun handleEvent(frame: JSONObject) {
        val ev = frame.optJSONObject("event") ?: return
        val x = ev.optDouble("x", 0.5)
        val y = ev.optDouble("y", 0.5)
        val px = (x * screenW).toInt().coerceIn(0, screenW)
        val py = (y * screenH).toInt().coerceIn(0, screenH)
        when (ev.optString("type")) {
            "mouse_button" -> {
                // 观看端 down/up 会发 pressed/released 两帧；无障碍 API 只支持完整
                // 手势，因此仅在 pressed 时注入一次点击，避免 released 重复点击。
                val state = ev.optString("state")
                if (state.isEmpty() || state == "pressed") {
                    tap(px, py)
                }
            }
            "key" -> {
                // 按键注入需系统/root 权限（公开无障碍 API 仅支持手势）；
                // 已记录，待企业签名/系统应用通道实现。
                android.util.Log.d("AeroDesk", "key event: " + ev.optString("code"))
            }
            else -> {
                // mouse_move/wheel/touch 等事件暂不注入：dispatchGesture 无法表示
                // 悬停/滚动语义，贸然注入会产生误触。
                android.util.Log.d("AeroDesk", "ignored input event: " + ev.optString("type"))
            }
        }
    }

    private fun tap(x: Int, y: Int) {
        val path = Path().apply { moveTo(x.toFloat(), y.toFloat()) }
        val builder = GestureDescription.Builder()
        builder.addStroke(GestureDescription.StrokeDescription(path, 0, 1))
        dispatchGesture(builder.build(), null, null)
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {}
    override fun onInterrupt() {}
    override fun onDestroy() {
        instance = null
        stop()
        super.onDestroy()
    }
}
