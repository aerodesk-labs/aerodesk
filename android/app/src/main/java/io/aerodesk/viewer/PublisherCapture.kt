package io.aerodesk.viewer

import android.content.Context
import android.graphics.PixelFormat
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.ImageReader
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.media.projection.MediaProjection
import android.os.Handler
import android.os.Looper
import java.nio.ByteBuffer

/**
 * 被控端：MediaProjection 采集 → MediaCodec H.264 硬编 → Rust publisherFeedAnnexB。
 * 需真机（录屏授权 + 硬件编码器）。RGBA → I420 转换在此完成。
 */
class PublisherCapture(
    private val context: Context,
    private val projection: MediaProjection,
    private val bridge: NativeBridge,
) {
    companion object {
        const val W = 1280
        const val H = 720
        const val FPS = 30
        const val BITRATE = 4_000_000
    }

    @Volatile var publisherPtr: Long = 0L
    private val handler = Handler(Looper.getMainLooper())
    private var encoder: MediaCodec? = null
    private var display: VirtualDisplay? = null
    private var imageReader: ImageReader? = null
    private var running = false
    private var csd: ByteArray? = null
    private var ptsUs = 0L

    fun start(server: String, room: String) {
        running = true
        Thread {
            val ptr = bridge.publisherCreate(server, room)
            publisherPtr = ptr
            if (ptr == 0L) {
                running = false
                return@Thread
            }
            try {
                runLoop(ptr)
            } finally {
                bridge.publisherDestroy(ptr)
                stop()
            }
        }.start()
    }

    fun stop() {
        running = false
        display?.release()
        display = null
        imageReader?.close()
        imageReader = null
        try { encoder?.stop() } catch (_: Exception) {}
        encoder?.release()
        encoder = null
    }

    private fun runLoop(ptr: Long) {
        val fmt = MediaFormat.createVideoFormat("video/avc", W, H).apply {
            setInteger(
                MediaFormat.KEY_COLOR_FORMAT,
                MediaCodecInfo.CodecCapabilities.COLOR_FormatYUV420Flexible,
            )
            setInteger(MediaFormat.KEY_BIT_RATE, BITRATE)
            setInteger(MediaFormat.KEY_FRAME_RATE, FPS)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 1)
        }
        val enc = MediaCodec.createEncoderByType("video/avc").apply {
            configure(fmt, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            start()
        }
        encoder = enc

        val ir = ImageReader.newInstance(W, H, PixelFormat.RGBA_8888, 2)
        imageReader = ir
        // Android 13+：createVirtualDisplay 前必须注册 callback（否则 IllegalStateException）。
        projection.registerCallback(object : MediaProjection.Callback() {
            override fun onStop() {
                stop()
            }
        }, handler)
        display = projection.createVirtualDisplay(
            "AeroDeskCapture", W, H, 1,
            DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR, ir.surface, null, handler,
        )

        val rgba = ByteArray(W * H * 4)
        val yuv = ByteArray(W * H * 3 / 2)

        while (running) {
            val image = ir.acquireLatestImage()
            if (image == null) {
                Thread.sleep(16)
                continue
            }
            try {
                val plane = image.planes[0]
                val buffer = plane.buffer
                val rowStride = plane.rowStride
                val pixelStride = plane.pixelStride
                // RGBA → I420
                for (y in 0 until H) {
                    val src = y * rowStride
                    buffer.position(src)
                    buffer.get(rgba, y * W * 4, W * 4)
                }
                rgbaToI420(rgba, yuv, W, H)

                // 送入编码器
                val inIdx = enc.dequeueInputBuffer(10_000)
                if (inIdx >= 0) {
                    val inBuf = enc.getInputBuffer(inIdx)!!
                    inBuf.clear()
                    inBuf.put(yuv)
                    enc.queueInputBuffer(inIdx, 0, yuv.size, ptsUs, 0)
                    ptsUs += 1_000_000L / FPS
                }

                // 取编码输出 → Rust 发送
                drainEncoder(enc, ptr)
            } finally {
                image.close()
            }
        }
        drainEncoder(enc, ptr)
        stop()
    }

    private fun drainEncoder(enc: MediaCodec, ptr: Long) {
        val info = MediaCodec.BufferInfo()
        while (true) {
            val outIdx = enc.dequeueOutputBuffer(info, 0)
            if (outIdx >= 0) {
                val outBuf = enc.getOutputBuffer(outIdx)!!
                val data = ByteArray(info.size)
                outBuf.position(info.offset)
                outBuf.get(data)
                if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) {
                    // SPS/PPS（关键帧前置）
                    csd = data
                } else {
                    val send = if (info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0 && csd != null) {
                        csd!! + data
                    } else data
                    bridge.publisherFeedAnnexB(ptr, send, info.presentationTimeUs)
                }
                enc.releaseOutputBuffer(outIdx, false)
                if (info.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0) break
            } else break
        }
    }

    /** RGBA8888 → I420（YUV420 平面，U/V 分量平面）。 */
    private fun rgbaToI420(rgba: ByteArray, yuv: ByteArray, w: Int, h: Int) {
        var yIdx = 0
        var uIdx = w * h
        var vIdx = w * h + w * h / 4
        for (y in 0 until h) {
            for (x in 0 until w) {
                val i = (y * w + x) * 4
                val r = rgba[i].toInt() and 0xFF
                val g = rgba[i + 1].toInt() and 0xFF
                val b = rgba[i + 2].toInt() and 0xFF
                val yy = ((66 * r + 129 * g + 25 * b + 128) shr 8) + 16
                yuv[yIdx++] = yy.toByte()
                if (y % 2 == 0 && x % 2 == 0) {
                    val u = ((-38 * r - 74 * g + 112 * b + 128) shr 8) + 128
                    val v = ((112 * r - 94 * g - 18 * b + 128) shr 8) + 128
                    yuv[uIdx++] = u.toByte()
                    yuv[vIdx++] = v.toByte()
                }
            }
        }
    }
}
