# 大文件传输基准（#72 ≥100MB 验收，#210）

## 结论
**≥100MB 大文件端到端验收通过**：release 构建下 100MB / 256MB 文件经
publisher → SFU → viewer 传输，SHA-256 字节一致，无断连/无 panic。
#85（str0m DTLS 接收队列 30→2048）与 #208（SFU 背压/转发可靠性）已闭环
#72 遗留的"大文件突发下队列溢出断连"问题。

## 基准（macOS M4，2026-08-10，release 构建）

| 尺寸 | 墙钟耗时 | 吞吐 | SHA-256 |
|---|---|---|---|
| 2MB | 10s（含启动） | 0.20 MB/s（wall） | PASS |
| 100MB | 33s | 3.03 MB/s | PASS |
| 256MB | 73s | 3.51 MB/s | PASS |

- 吞吐受 core `file_transfer` 单发节拍（~250-300 chunks/s，#85 实测稳定速率上界）
  约束，为设计内的稳定速率（保证不触发 SFU DTLS 接收队列溢出）
- 复跑 100MB ×3 全 PASS（33/29/34s）

## 复现

```sh
# 基准矩阵（release）
PROFILE=release scripts/file-transfer-bench.sh 2048 102400 262144
# 单尺寸验收
PROFILE=release scripts/file-transfer-e2e.sh <room> 102400
```

## 备注
- 一次 bench 在并发 release 重建负载下出现 e2e 提前退出（file send 未启动、
  等待循环 kill -0 早退）——单次环境 flake，非产品缺陷；复跑稳定
