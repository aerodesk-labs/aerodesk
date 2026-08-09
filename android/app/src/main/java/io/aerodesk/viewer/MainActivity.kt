package io.aerodesk.viewer

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Bundle
import android.util.Log
import android.view.SurfaceView
import android.widget.Button
import android.widget.EditText
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity

class MainActivity : AppCompatActivity() {
    private var viewer = 0L
    private var codec: MediaCodec? = null
    private var running = false
    private var pollThread: Thread? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        val server = findViewById<EditText>(R.id.server)
        val room = findViewById<EditText>(R.id.room)
        val status = findViewById<TextView>(R.id.status)
        val connect = findViewById<Button>(R.id.connect)
        val disconnect = findViewById<Button>(R.id.disconnect)

        status.text = "SDK ${NativeBridge.version()}（Rust JNI）"

        connect.setOnClickListener {
            doConnect(server.text.toString(), room.text.toString(), status)
        }

        disconnect.setOnClickListener {
            stopViewer()
            status.text = "已断开"
        }

        // 模拟器/CI 自测：intent extras 驱动（-e server/-e room/-e autoconnect true）
        val srv = intent.getStringExtra("server")
        val rm = intent.getStringExtra("room")
        if (srv != null) server.setText(srv)
        if (rm != null) room.setText(rm)
        // #201：-e force_relay true → ICE 只通告 relayed 候选（模拟器/NAT 兜底）
        val forceRelay = when (val v = intent.extras?.get("force_relay")) {
            is Boolean -> v
            is String -> v == "true"
            else -> false
        }
        // 兼容 -e autoconnect true（String）与 --ez autoconnect true（Boolean）。
        // 注意：不能用 getBooleanExtra 兜底——extra 为 String 时会抛
        // ClassCastException（模拟器/CI 自测用 -e 传参必现）。
        val auto = when (val v = intent.extras?.get("autoconnect")) {
            is Boolean -> v
            is String -> v == "true"
            else -> false
        }
        if (auto && srv != null && rm != null) {
            status.postDelayed({ doConnect(srv, rm, status, forceRelay) }, 500)
        }
    }

    private fun doConnect(s: String, r: String, status: TextView, forceRelay: Boolean = false) {
        status.text = "连接中…"
        Thread {
            val v = NativeBridge.viewerCreate(s, r, forceRelay)
            runOnUiThread {
                if (v == 0L) {
                    status.text = "连接失败"
                } else {
                    viewer = v
                    status.text = "已连接，收流解码中…"
                    startDecode()
                }
            }
        }.start()
    }

    private fun startDecode() {
        val sv = findViewById<SurfaceView>(R.id.surface)
        val surface = sv.holder.surface
        sv.setOnTouchListener { _, e ->
            when (e.action) {
                android.view.MotionEvent.ACTION_DOWN -> sendTouch(sv, e.x, e.y, "down")
                android.view.MotionEvent.ACTION_MOVE -> sendTouch(sv, e.x, e.y, "move")
                android.view.MotionEvent.ACTION_UP -> sendTouch(sv, e.x, e.y, "up")
            }
            true
        }
        val fmt = MediaFormat.createVideoFormat("video/avc", 1280, 720)
        codec = MediaCodec.createDecoderByType("video/avc").apply {
            configure(fmt, surface, null, 0)
            start()
        }
        running = true
        pollThread = Thread {
            var pts = 0L
            var frames = 0L
            while (running) {
                val frame = NativeBridge.viewerTakeAnnexB(viewer)
                if (frame.isEmpty()) {
                    Thread.sleep(16)
                    continue
                }
                frames += 1
                if (frames % 60 == 0L) {
                    Log.i("AeroDeskE2E", "decoded frames=$frames au_bytes=${frame.size}")
                }
                val c = codec ?: break
                val idx = c.dequeueInputBuffer(10_000)
                if (idx >= 0) {
                    val buf = c.getInputBuffer(idx) ?: continue
                    buf.clear()
                    buf.put(frame)
                    val flags = if (hasSps(frame)) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0
                    c.queueInputBuffer(idx, 0, frame.size, pts, flags)
                    pts += 33_333
                }
            }
        }.apply { start() }
    }

    private fun hasSps(frame: ByteArray): Boolean {
        // AnnexB SPS 起始码：00 00 00 01 67 / 00 00 01 67
        for (i in 0..frame.size - 5) {
            if (frame[i] == 0.toByte() && frame[i + 1] == 0.toByte()
                && frame[i + 2] == 1.toByte() && frame[i + 3] == 0x67.toByte()
            ) return true
            if (frame[i] == 0.toByte() && frame[i + 1] == 0.toByte()
                && frame[i + 2] == 0.toByte() && frame[i + 3] == 1.toByte()
                && frame[i + 4] == 0x67.toByte()
            ) return true
        }
        return false
    }

    // #2 主控端触摸输入：SurfaceView 触摸 → InputFrame JSON → input 通道 → 被控端。
    // 注意：必须发完整 InputFrame（version/seq/timestamp_ms/event 包装，见
    // aerodesk-protocol/src/input.rs）——只发裸 InputEvent JSON 时被控端
    // serde 解析失败会静默丢弃（实测 Android 触摸从未生效的根因）。
    private var inputSeq = 0L
    private fun sendTouch(surface: SurfaceView, rawX: Float, rawY: Float, type: String) {
        Log.i("AeroDeskE2E", "touch $type $rawX,$rawY viewer=$viewer")
        if (viewer == 0L) return
        val r = android.graphics.Rect()
        surface.getHitRect(r)
        val x = if (r.width() > 0) ((rawX - r.left).coerceIn(0f, r.width().toFloat()) / r.width()).toDouble() else 0.5
        val y = if (r.height() > 0) ((rawY - r.top).coerceIn(0f, r.height().toFloat()) / r.height()).toDouble() else 0.5
        val eventJson = when (type) {
            "move" -> """{"type":"mouse_move","x":$x,"y":$y}"""
            "down" -> """{"type":"mouse_button","button":"left","state":"pressed","x":$x,"y":$y}"""
            else -> """{"type":"mouse_button","button":"left","state":"released","x":$x,"y":$y}"""
        }
        val json = """{"version":1,"seq":${++inputSeq},"timestamp_ms":${System.currentTimeMillis()},"event":$eventJson}"""
        NativeBridge.viewerSendInput(viewer, json)
    }

    private fun stopViewer() {
        running = false
        pollThread?.join(1000)
        pollThread = null
        codec?.stop()
        codec?.release()
        codec = null
        if (viewer != 0L) NativeBridge.viewerDestroy(viewer)
        viewer = 0L
    }

    override fun onDestroy() {
        super.onDestroy()
        stopViewer()
    }
}
