# SFU 录制与 MP4 转换（#234）

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

## 转 MP4（H.264）

```sh
cargo build -p aerodesk-ffmpeg --bin aerodesk-rec2mp4
aerodesk-rec2mp4 --input <room.adrec> --output <out.mp4>
```

- 从 Annex-B 流提取 SPS/PPS → AVCC extradata；解码首关键帧探测宽高；
  载荷 Annex-B → AVCC（length-prefixed）以 90kHz pts mux（不重编码）。
- H.265/VP9/AV1 容器化：M2（RTP 载荷格式/参数集处理不同，见 #234）。

## 端到端验证

```sh
scripts/record-mp4-e2e.sh
# 断言：ADREC2 落盘 → rec2mp4 → ffprobe(h264/时长>0) → ffmpeg 解码 0 错误
```

本机结果（macOS M4，vt H.264 720p30 10s）：206 视频包 / 6.83s / ffmpeg 解码 0 错误（2/2）。
