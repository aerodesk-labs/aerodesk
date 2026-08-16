# aerodesk-softenc 与 aerodesk-ffmpeg 差异与合并评估

## 1. 两个 crate 的职责

### aerodesk-softenc
- 定位：**轻量软编解码回退路径**
- 依赖：
  - `openh264`：全平台软解/软编
  - `x264`：非 Windows 软编（Windows 因系统库问题不启用）
- 能力：
  - H.264 AnnexB 编码 / 解码
  - OpenH264 软编
  - BGRA/RGBA/RGB 色彩转换工具
- 平台：
  - 桌面三端可用（Windows 仅 OpenH264 编码 + 解码）
  - 移动端当前不直接使用（项目选择 MediaCodec / VideoToolbox，而非 FFmpeg 不支持移动端）

### aerodesk-ffmpeg
- 定位：**FFmpeg 多格式编解码 / 音频 / 容器**
- 依赖：
  - `ffmpeg-next = "8"`
- 能力：
  - H.264 / HEVC / AV1 / VP9 编解码
  - 硬件编码/解码（macOS VideoToolbox、Windows D3D11VA/DXVA2）
  - 音频编解码
  - MP4/容器封装
- 平台：
  - 桌面/CLI 使用
  - 移动端当前不直接使用（项目选择系统 native codec，而非 FFmpeg 无法编译移动端）

## 2. 关键差异

| 维度 | aerodesk-softenc | aerodesk-ffmpeg |
| --- | --- | --- |
| 依赖重量 | 轻（OpenH264 + 可选 x264） | 重（FFmpeg 全家桶） |
| Codec 覆盖 | H.264 为主 | H.264/HEVC/AV1/VP9 |
| 音频 | 无 | 有 |
| 容器/录制 | 无 | 有 MP4 mux |
| 硬件加速 | 无 | 有 |
| 移动端 | 当前未使用 | 当前未使用 |
| Windows x264 | 禁用 | 支持 FFmpeg x264 软编 |
| 构建风险 | 低 | 高（ffmpeg-sys-next / pkg-config / 预编译库） |

## 3. 为什么不能简单合并

1. **依赖策略不同**
   - `aerodesk-softenc` 设计为无 FFmpeg 也能跑，适合轻量回退。
   - `aerodesk-ffmpeg` 需要完整 FFmpeg，Windows/macOS/Linux 构建与打包差异大。

2. **平台裁剪不同**
   - 移动端构建不希望拉入 FFmpeg；如果合并，`aerodesk-platform` 或端侧会
     被迫携带 FFmpeg 依赖，增加移动端交叉编译风险。

3. **错误面不同**
   - FFmpeg 构建失败是 CI/打包常见问题；softenc 只有 x264/OpenH264。
   - 分开便于定位问题、独立升级。

4. **使用场景不同**
   - `softenc` 是桌面端 H.264 回退。
   - `ffmpeg` 是多格式、音频、录制、硬件加速的完整编解码层。

## 4. 结论

**从当前依赖关系看，可以合并。** FFmpeg 已经是 desktop/CLI 的硬依赖，softenc 的 H.264 回退路径与 FFmpeg 的 libx264/libopenh264 重叠；继续保留两个 crate 属于历史边界，不是技术必需。

- `aerodesk-softenc`：轻量 H.264 软编解码回退
- `aerodesk-ffmpeg`：重型多格式/音频/录制层

可选优化不是“合并”，而是：
- 在 `aerodesk-core` 或 `aerodesk-platform` 里定义统一 codec trait/facade
- 让调用方根据能力选择 `softenc` 或 `ffmpeg`
- 保持两个 crate 独立构建，避免互相拖累

## 5. 建议

- 短期：可保留现状，但不应把 softenc 当作长期独立层。
- 中期：优先把 softenc 的 H.264 回退逻辑并入 `aerodesk-ffmpeg`，删除 vendored `x264` crate；若仍有纯类型/转换需求，再抽一个
  `aerodesk-codec-common`，但只放纯 Rust 类型与转换，不引入 FFmpeg/x264。