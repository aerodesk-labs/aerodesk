# SFU 录制与 MP4/WebM 转换（#234/#236）

## 录制（ADREC2）

`RECORD_DIR=<dir>` 开启：SFU 把每个房间收到的媒体载荷落盘（自动或 `RECORD_ON_DEMAND=1`
按需，内部 API `/record/start|stop|status`，#160）。

### 格式（ADREC2）
```text
magic "ADREC2\n"
每包: [u8 kind(0=video,1=audio)][u8 codec][u8 flags(bit0=keyframe)][u8 rsv]
      [u64 wall_us][u64 rtp_ts][u32 len][payload bytes]
```
codec id：0=none 1=H264 2=H265 3=VP8 4=VP9 5=AV1 6=Opus 7=PCMU。
- `rtp_ts` 为 RTP 时间戳（视频 90kHz），`keyframe` 标记关键帧——供精确转封装。
- 文件：`{RECORD_DIR}/{room}.adrec`；元数据：`{room}.meta.json`；审计：`audit.log`。
- 轮转（#180）：`{room}.adrec.{N}`。

## 转 MP4/WebM（H.264/H.265/VP9/AV1）

```sh
cargo build -p aerodesk-codec --bin aerodesk-rec2mp4
aerodesk-rec2mp4 --input <room.adrec> --output <out.mp4|out.webm>
```

- **H.264 → MP4**：Annex-B 提取 SPS/PPS → AVCC extradata；载荷 Annex-B → AVCC（length-prefixed），90kHz pts（#234）。
- **H.265 → MP4**：Annex-B 提取 VPS/SPS/PPS（type 32/33/34）→ hvcC extradata；载荷 Annex-B → AVCC（#236）。
- **VP9 → WebM**：载荷即完整帧位流，无 extradata，1ms 时间基（#236）。
- **AV1 → WebM**：载荷为带 leb128 size 的 OBU 流；从 sequence header OBU 构造 av1c extradata；1ms 时间基（#236）。
- 统一：解码首关键帧探测宽高；不重编码；`read_adrec` 容忍录制尾部截断（崩溃/强杀场景）。

## 端到端验证

```sh
scripts/record-mp4-e2e.sh              # 四格式：h264/h265/vp9/av1
scripts/record-mp4-e2e.sh h265 av1     # 指定格式
# 断言：ADREC2 落盘 → rec2mp4（自动选 mp4/webm）→ ffprobe codec/时长 → ffmpeg 解码 0 错误
```

本机结果（macOS M4，各 codec 720p30 ~6s）：h264 182 帧/6.03s、h265 189/6.27s、
vp9 192/6.37s、av1 167/6.37s，全部 ffmpeg 解码 0 错误（连跑 PASS）。
