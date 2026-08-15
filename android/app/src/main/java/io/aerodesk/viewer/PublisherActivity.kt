package io.aerodesk.viewer

import android.app.Activity
import android.content.Intent
import android.media.projection.MediaProjectionManager
import android.provider.Settings
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

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_publisher)

        val server = findViewById<EditText>(R.id.server)
        val room = findViewById<EditText>(R.id.room)
        val status = findViewById<TextView>(R.id.status)
        val start = findViewById<Button>(R.id.start)
        val stopBtn = findViewById<Button>(R.id.stop)
        val a11y = findViewById<Button>(R.id.a11y)
        a11y.setOnClickListener {
            startActivity(Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS))
        }

        start.setOnClickListener {
            val mp = getSystemService(MediaProjectionManager::class.java)
            startActivityForResult(mp.createScreenCaptureIntent(), REQ_PROJECTION)
            status.text = "等待录屏授权…"
        }
        stopBtn.setOnClickListener {
            // 采集/编码实际运行在 ProjectionService；Activity 只负责拉起与停止前台服务。
            stopService(Intent(this, ProjectionService::class.java))
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
        val server = findViewById<EditText>(R.id.server).text.toString()
        val room = findViewById<EditText>(R.id.room).text.toString()
        // Android 14+：MediaProjection 必须在前台服务（mediaProjection 类型）中创建。
        val svc = Intent(this, ProjectionService::class.java).apply {
            putExtra("resultCode", resultCode)
            putExtra("data", data)
            putExtra("server", server)
            putExtra("room", room)
        }
        startForegroundService(svc)
        // 输入注入：无障碍服务由系统绑定（用户开启后 onServiceConnected），
        // 不要用 startService 启动；发布线程拿到 Rust 会话指针后再交给服务轮询。
        InputInjectionService.instance?.stop()
        InputInjectionService.pendingPtr = 0L
        status.text = "被控端采集编码中…（1280x720 H.264；请到系统设置开启 AeroDesk 无障碍服务）"
    }
}
