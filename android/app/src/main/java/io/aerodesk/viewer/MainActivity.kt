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
        // 兼容 -e autoconnect true（String）与 --ez autoconnect true（Boolean）
        val auto = intent.getBooleanExtra("autoconnect", false)
                || intent.getStringExtra("autoconnect") == "true"
        if (auto && srv != null && rm != null) {
            status.postDelayed({ doConnect(srv, rm, status) }, 500)
        }
    }

    private fun doConnect(s: String, r: String, status: TextView) {
        status.text = "连接中…"
        Thread {
            val v = NativeBridge.viewerCreate(s, r)
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
        val surface = findViewById<SurfaceView>(R.id.surface).holder.surface
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
