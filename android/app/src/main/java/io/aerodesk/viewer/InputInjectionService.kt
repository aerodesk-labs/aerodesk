package io.aerodesk.viewer

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.graphics.Path
import android.graphics.Rect
import android.os.Handler
import android.os.Looper
import android.view.KeyEvent
import android.view.accessibility.AccessibilityEvent
import org.json.JSONObject

/**
 * 无障碍注入服务：轮询 Rust publisher 收到的观看端输入事件，
 * 通过 dispatchGesture / injectInputEvent 应用到本设备（被控端）。
 * 需用户在系统设置中开启无障碍服务。
 */
class InputInjectionService : AccessibilityService() {
    companion object {
        @Volatile var pendingPtr: Long = 0L
    }
    private val handler = Handler(Looper.getMainLooper())
    private var ptr = 0L
    private var running = false
    private var screenW = 0
    private var screenH = 0

    override fun onServiceConnected() {
        super.onServiceConnected()
        screenW = resources.displayMetrics.widthPixels
        screenH = resources.displayMetrics.heightPixels
        if (pendingPtr != 0L) {
            start(pendingPtr)
            pendingPtr = 0L
        }
    }

    /** 绑定发布会话指针后启动轮询。 */
    fun start(ptr: Long) {
        this.ptr = ptr
        running = true
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
            "mouse_move" -> gesture(px, py, down = true, up = false)
            "mouse_button" -> gesture(px, py, down = true, up = true)
            "key" -> {
                // 按键注入需系统/root 权限（公开无障碍 API 仅支持手势）；
                // 已记录，待企业签名/系统应用通道实现。
                android.util.Log.d("AeroDesk", "key event: " + ev.optString("code"))
            }
        }
    }

    private fun gesture(x: Int, y: Int, down: Boolean, up: Boolean) {
        val path = Path().apply { moveTo(x.toFloat(), y.toFloat()) }
        val builder = GestureDescription.Builder()
        if (down) builder.addStroke(GestureDescription.StrokeDescription(path, 0, 1))
        if (up) builder.addStroke(GestureDescription.StrokeDescription(path, 20, 1))
        dispatchGesture(builder.build(), null, null)
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {}
    override fun onInterrupt() {}
    override fun onDestroy() {
        super.onDestroy()
        stop()
    }
}
