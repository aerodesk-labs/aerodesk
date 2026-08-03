package io.aerodesk.viewer

import android.os.Bundle
import android.widget.Button
import android.widget.EditText
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity

class MainActivity : AppCompatActivity() {
    private val bridge = NativeBridge()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        val server = findViewById<EditText>(R.id.server)
        val room = findViewById<EditText>(R.id.room)
        val status = findViewById<TextView>(R.id.status)
        val connect = findViewById<Button>(R.id.connect)

        status.text = "SDK ${bridge.version()}（Rust JNI）"

        connect.setOnClickListener {
            status.text = "连接中…"
            val s = server.text.toString()
            val r = room.text.toString()
            Thread {
                val result = bridge.connect(s, r)
                runOnUiThread { status.text = result }
            }.start()
        }
    }
}
