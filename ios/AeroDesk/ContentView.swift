import SwiftUI
import CoreVideo
import CoreImage
import CoreFoundation

/// AeroDesk iOS 观看端壳层。
/// 连接（ad_viewer_create）→ 后台收流解码 → 定时取帧渲染（CVPixelBuffer → CIImage）。
struct ContentView: View {
    @State private var version = ""
    @State private var hardware = false
    @State private var status = "未连接"
    @State private var server = "ws://127.0.0.1:3003"
    @State private var room = "demo"
    @State private var viewer: UnsafeMutableRawPointer?
    @State private var timer: Timer?
    @State private var frameImage: UIImage?

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

            if let img = frameImage {
                Image(uiImage: img)
                    .resizable()
                    .aspectRatio(contentMode: .fit)
                    .frame(maxHeight: 360)
                    .background(Color.black)
                    .clipShape(RoundedRectangle(cornerRadius: 8))
            } else {
                Rectangle()
                    .fill(Color.black.opacity(0.2))
                    .frame(height: 200)
                    .overlay(Text("视频区域（等待媒体流）").foregroundStyle(.secondary))
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
        let ci = CIImage(cvPixelBuffer: cvBuf)
        let ctx = CIContext()
        if let cg = ctx.createCGImage(ci, from: ci.extent) {
            frameImage = UIImage(cgImage: cg)
        }
        Unmanaged<AnyObject>.fromOpaque(buf).release()
    }

    private func stopViewer() {
        timer?.invalidate()
        timer = nil
        if let viewer {
            ad_viewer_destroy(viewer)
        }
        viewer = nil
        frameImage = nil
        status = "已断开"
    }
}
