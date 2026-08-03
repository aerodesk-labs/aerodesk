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

/// 观看端连接（阻塞；后台线程调用）。返回 malloc 字符串，用 ad_free_string 释放。
const char *ad_connect(const char *server, const char *room);

/// 释放 ad_connect 返回的字符串。
void ad_free_string(char *s);

/// 创建观看会话（连接 + 后台收流解码）。失败返回 NULL。
void *ad_viewer_create(const char *server, const char *room);
/// 销毁观看会话。
void ad_viewer_destroy(void *viewer);
/// 取最新解码帧：0=有新帧（*out 为 +1 CVPixelBufferRef，调用方 CVBufferRelease），1=暂无。
int ad_viewer_take_frame(void *viewer, void **out);

/// 发送输入事件（JSON InputFrame）到 input 数据通道。返回 0=入队，<0=错误。
int ad_viewer_send_input(void *viewer, const char *json);



#ifdef __cplusplus
}
#endif

#endif /* AD_AERODESK_H */
