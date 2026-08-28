package io.aerodesk.viewer

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.IBinder

/**
 * Android 14+ 被控端前台服务：MediaProjection 必须在
 * foregroundServiceType="mediaProjection" 的服务中创建（否则 SecurityException）。
 */
class ProjectionService : Service() {
    private var capture: PublisherCapture? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val resultCode = intent?.getIntExtra("resultCode", 0) ?: 0
        val data = if (Build.VERSION.SDK_INT >= 33) {
            intent?.getParcelableExtra("data", Intent::class.java)
        } else {
            @Suppress("DEPRECATION")
            intent?.getParcelableExtra("data") as Intent?
        }
        val server = intent?.getStringExtra("server") ?: ""
        val room = intent?.getStringExtra("room") ?: ""
        // #598 P1d：-e token → Digest 口令（被控端发布）。
        val token = intent?.getStringExtra("token")

        startForegroundCompat()

        if (data == null) {
            stopSelf()
            return START_NOT_STICKY
        }
        val mp = getSystemService(MediaProjectionManager::class.java)
        val projection: MediaProjection = mp.getMediaProjection(resultCode, data)
        capture = PublisherCapture(this, projection, NativeBridge)
        capture?.start(server, room, token)
        return START_NOT_STICKY
    }

    private fun startForegroundCompat() {
        val channel = NotificationChannel(
            "aerodesk-projection", "AeroDesk 被控端", NotificationManager.IMPORTANCE_LOW
        )
        getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
        val n: Notification = Notification.Builder(this, "aerodesk-projection")
            .setContentTitle("AeroDesk 被控端")
            .setContentText("屏幕采集编码中…")
            .setSmallIcon(android.R.drawable.ic_media_play)
            .build()
        startForeground(1, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION)
    }

    override fun onDestroy() {
        capture?.stop()
        capture = null
        super.onDestroy()
    }
}
