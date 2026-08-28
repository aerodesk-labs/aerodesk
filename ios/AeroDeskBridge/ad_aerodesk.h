#ifndef AD_AERODESK_H
#define AD_AERODESK_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/// SDK 版本字符串（静态存储，nul 结尾）。
const char *ad_version(void);

/// 创建 H.264 解码器（VideoToolbox 硬解）。
void *ad_decoder_create(void);

/// 释放解码器。
void ad_decoder_free(void *decoder);

/// 是否支持硬件解码。
int ad_decoder_hardware(void);

/// 解码一帧 AnnexB H.264。
/// 返回 0=有输出帧（*out 为 +1 CVPixelBufferRef，调用方负责 CVBufferRelease），
///      1=无输出（等关键帧），<0=错误。
int ad_decoder_decode(void *decoder, const uint8_t *data, size_t len,
                      int64_t pts, void **out);

/// 创建观看会话（连接 + 后台收流解码）。失败返回 NULL。
void *ad_viewer_create(const char *server, const char *room);
/// 销毁观看会话。
void ad_viewer_destroy(void *viewer);
/// 取最新解码帧：0=有新帧（*out 为 +1 CVPixelBufferRef，调用方 CVBufferRelease），1=暂无。
int ad_viewer_take_frame(void *viewer, void **out);

/// 发送输入事件（JSON InputFrame）到 input 数据通道。返回 0=入队，<0=错误。
int ad_viewer_send_input(void *viewer, const char *json);

/// 取解码后的 PCM i16 音频样本（8kHz 单声道）。返回拷贝样本数（0=暂无，<0=错误）。
int ad_viewer_take_audio(void *viewer, int16_t *dst, size_t max);

/// 切换画面源：show=1 摄像头 / 0 屏幕（take_frame 按此返回对应轨）。返回 0=成功。
int ad_viewer_set_show_camera(void *viewer, int show);

/// 是否已收到摄像头轨。1=有，0=无。
int ad_viewer_camera_available(void *viewer);

#ifdef __cplusplus
}
#endif

/// 启动 Slint UI（阻塞运行事件循环；由 Swift 生命周期宿主在主线程调用）。
void ad_slint_run(void);

#endif /* AD_AERODESK_H */
