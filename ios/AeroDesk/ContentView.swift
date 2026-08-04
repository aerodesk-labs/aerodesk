import SwiftUI
import CoreVideo
import CoreFoundation
import AVFoundation

/// AVSampleBufferDisplayLayer 容器（低延迟渲染路径）。
struct VideoLayerView: UIViewRepresentable {
    let layer: AVSampleBufferDisplayLayer

    func makeUIView(context: Context) -> UIView {
        let v = UIView(frame: .zero)
        v.backgroundColor = .black
        layer.videoGravity = .resizeAspect
        layer.backgroundColor = UIColor.black.cgColor
        v.layer.addSublayer(layer)
        layer.frame = v.bounds
        return v
    }

    func updateUIView(_ uiView: UIView, context: Context) {
        layer.frame = uiView.bounds
    }
}

/// AeroDesk iOS 观看端壳层。
/// 连接（ad_viewer_create）→ 后台收流解码 → 定时取帧渲染（CVPixelBuffer → AVSampleBufferDisplayLayer）。
struct ContentView: View {
    @State private var version = ""
    @State private var hardware = false
    @State private var status = "未连接"
    @State private var server = "ws://127.0.0.1:3003"
    @State private var room = "demo"
    @State private var viewer: UnsafeMutableRawPointer?
    @State private var timer: Timer?
    @State private var hasFrame = false
    @State private var inputSeq: UInt64 = 0
    @State private var displayLayer = AVSampleBufferDisplayLayer()

    var body: some View {
        VStack(spacing: 18) {
            Text("AeroDesk")
                .font(.largeTitle)
                .bold()
            Text("iOS Viewer · SDK \(version)")
                .font(.subheadline)
                .foregroundStyle(.secondary)
            Label(hardware ? "硬件解码可用" : "硬件解码不可用",
                  systemImage: hardware ? "checkmark.circle.fill" : "exclamationmark.triangle.fill")
                .foregroundStyle(hardware ? .green : .orange)

            HStack {
                TextField("服务器", text: $server)
                    .textFieldStyle(.roundedBorder)
                    .autocorrectionDisabled()
                    .textInputAutocapitalization(.never)
                TextField("房间", text: $room)
                    .textFieldStyle(.roundedBorder)
                    .frame(width: 110)
            }

            HStack {
                Button {
                    if viewer == nil {
                        startViewer()
                    }
                } label: {
                    Text("连接")
                        .frame(maxWidth: .infinity)
                }
                .buttonStyle(.borderedProminent)
                .disabled(viewer != nil)

                Button {
                    stopViewer()
                } label: {
                    Text("断开")
                        .frame(maxWidth: .infinity)
                }
                .buttonStyle(.bordered)
                .disabled(viewer == nil)
            }

            Text(status)
                .font(.footnote)
                .foregroundStyle(.secondary)

            GeometryReader { geo in
                ZStack {
                    VideoLayerView(layer: displayLayer)
                        .frame(maxHeight: 360)
                        .background(Color.black)
                        .clipShape(RoundedRectangle(cornerRadius: 8))
                        .contentShape(Rectangle())
                        .focusable()
                        .onKeyPress(.space) {
                            sendInput(["type": "key", "code": "space", "state": "pressed"])
                            return .handled
                        }
                        .gesture(
                            DragGesture(minimumDistance: 0)
                                .onChanged { g in
                                    let x = Double(g.location.x / max(geo.size.width, 1))
                                    let y = Double(g.location.y / max(geo.size.height, 1))
                                    sendInput(["type": "mouse_move", "x": x, "y": y])
                                }
                                .onEnded { g in
                                    let x = Double(g.location.x / max(geo.size.width, 1))
                                    let y = Double(g.location.y / max(geo.size.height, 1))
                                    sendInput(["type": "mouse_button", "button": "left", "state": "released", "x": x, "y": y])
                                }
                        )
                        .onTapGesture {
                            sendInput(["type": "mouse_button", "button": "left", "state": "pressed", "x": 0.5, "y": 0.5])
                        }
                    if !hasFrame {
                        Rectangle()
                            .fill(Color.black.opacity(0.2))
                            .overlay(Text("视频区域（等待媒体流）").foregroundStyle(.secondary))
                    }
                }
                .frame(height: 360)
            }

            Spacer()
        }
        .padding()
        .onAppear {
            version = String(cString: ad_version())
            hardware = ad_decoder_hardware() != 0
        }
        .onDisappear { stopViewer() }
    }

    private func startViewer() {
        status = "连接中…"
        let s = server
        let r = room
        DispatchQueue.global().async {
            let v = s.withCString { sc in
                r.withCString { rc in ad_viewer_create(sc, rc) }
            }
            DispatchQueue.main.async {
                guard let v else {
                    status = "连接失败"
                    return
                }
                viewer = v
                hasFrame = false
                status = "已连接，收流解码中…"
                timer = Timer.scheduledTimer(withTimeInterval: 0.05, repeats: true) { _ in
                    pollFrame()
                }
            }
        }
    }

    private func pollFrame() {
        guard let viewer else { return }
        var out: UnsafeMutableRawPointer?
        let r = ad_viewer_take_frame(viewer, &out)
        guard r == 0, let buf = out else { return }
        let cvBuf = unsafeBitCast(buf, to: CVPixelBuffer.self)
        // AVSampleBufferDisplayLayer 低延迟渲染（CMSampleBuffer 持有 CVPixelBuffer 引用）。
        enqueue(cvBuf)
        // 释放 FFI +1 所有权（CMSampleBuffer 已 retain）。
        Unmanaged<AnyObject>.fromOpaque(buf).release()
        if !hasFrame {
            hasFrame = true
        }
    }

    /// CVPixelBuffer → CMSampleBuffer → AVSampleBufferDisplayLayer 入队。
    private func enqueue(_ pixelBuffer: CVPixelBuffer) {
        var fmt: CMVideoFormatDescription?
        let s1 = CMVideoFormatDescriptionCreateForImageBuffer(
            allocator: kCFAllocatorDefault,
            imageBuffer: pixelBuffer,
            formatDescriptionOut: &fmt
        )
        guard s1 == 0, let fmt else { return }

        var timing = CMSampleTimingInfo(
            duration: CMTime(value: 1, timescale: 30),
            presentationTimeStamp: CMTime(value: CMTimeValue(Date().timeIntervalSince1970 * 1000), timescale: 1000),
            decodeTimeStamp: .invalid
        )
        var sb: CMSampleBuffer?
        let s2 = CMSampleBufferCreateReadyWithImageBuffer(
            allocator: kCFAllocatorDefault,
            imageBuffer: pixelBuffer,
            formatDescription: fmt,
            sampleTiming: &timing,
            sampleBufferOut: &sb
        )
        guard s2 == 0, let sb else { return }
        if var att = CMSampleBufferGetSampleAttachmentsArray(sb, createIfNecessary: true) as? [[CFString: Any]] {
            att[0][kCMSampleAttachmentKey_DisplayImmediately] = true
        }
        displayLayer.enqueue(sb)
        displayLayer.setNeedsDisplay()
    }

    /// 发送输入事件（InputFrame JSON）到 input 数据通道。
    private func sendInput(_ event: [String: Any]) {
        guard let viewer else { return }
        let frame: [String: Any] = [
            "version": 1,
            "seq": inputSeq,
            "timestamp_ms": Int(Date().timeIntervalSince1970 * 1000),
            "event": event,
        ]
        inputSeq += 1
        guard let data = try? JSONSerialization.data(withJSONObject: frame),
              let s = String(data: data, encoding: .utf8)
        else { return }
        s.withCString {
            _ = ad_viewer_send_input(viewer, UnsafeMutablePointer(mutating: $0))
        }
    }

    private func stopViewer() {
        timer?.invalidate()
        timer = nil
        displayLayer.flushAndRemoveImage()
        hasFrame = false
        if let viewer {
            ad_viewer_destroy(viewer)
            self.viewer = nil
        }
        status = "已断开"
    }
}
