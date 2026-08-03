package io.aerodesk.viewer

import android.app.Activity
import android.content.Intent
import android.media.projection.MediaProjectionManager
import android.os.Bundle
import android.widget.Button
import android.widget.EditText
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity

/**
 * 被控端入口：录屏授权 → MediaProjection 采集编码 → Rust 发送。
 */
class PublisherActivity : AppCompatActivity() {
    companion object {
        private const val REQ_PROJECTION = 1001
    }

    private var capture: PublisherCapture? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_publisher)

        val server = findViewById<EditText>(R.id.server)
        val room = findViewById<EditText>(R.id.room)
        val status = findViewById<TextView>(R.id.status)
        val start = findViewById<Button>(R.id.start)
        val stopBtn = findViewById<Button>(R.id.stop)

        start.setOnClickListener {
            val mp = getSystemService(MediaProjectionManager::class.java)
            startActivityForResult(mp.createScreenCaptureIntent(), REQ_PROJECTION)
            status.text = "等待录屏授权…"
        }
        stopBtn.setOnClickListener {
            capture?.stop()
            capture = null
            status.text = "已停止"
        }
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != REQ_PROJECTION) return
        val status = findViewById<TextView>(R.id.status)
        if (resultCode != Activity.RESULT_OK || data == null) {
            status.text = "录屏授权被拒绝"
            return
        }
        val mp = getSystemService(MediaProjectionManager::class.java)
        val projection = mp.getMediaProjection(resultCode, data)
        val server = findViewById<EditText>(R.id.server).text.toString()
        val room = findViewById<EditText>(R.id.room).text.toString()
        capture = PublisherCapture(this, projection, NativeBridge)
        capture?.start(server, room)
        // 输入注入：绑定发布会话指针并启动无障碍服务（需在系统设置开启）。
        InputInjectionService.pendingPtr = capture?.publisherPtr ?: 0L
        startService(android.content.Intent(this, InputInjectionService::class.java))
        status.text = "被控端采集编码中…（1280x720 H.264；请到系统设置开启 AeroDesk 无障碍服务）"
    }

    override fun onDestroy() {
        super.onDestroy()
        capture?.stop()
        capture = null
    }
}
