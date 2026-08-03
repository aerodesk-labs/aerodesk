import SwiftUI
import CoreVideo
import CoreImage

/// AeroDesk iOS 观看端壳层（P3.5 里程碑 1）。
/// 当前：展示 SDK 版本/硬解能力 + 信令连接（ad_connect）；渲染在下一里程碑接入。
struct ContentView: View {
    @State private var version = ""
    @State private var hardware = false
    @State private var status = "解码器: 就绪"
    @State private var server = "wss://signal.aerodesk.io"
    @State private var room = "demo"

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
                    .frame(width: 120)
            }

            Button {
                status = "连接中…"
                let s = server
                let r = room
                DispatchQueue.global().async {
                    let result: String = s.withCString { sc in
                        r.withCString { rc in
                            guard let ptr = ad_connect(UnsafeMutablePointer(mutating: sc), UnsafeMutablePointer(mutating: rc)) else { return "返回为空" }
                            defer { ad_free_string(UnsafeMutablePointer(mutating: ptr)) }
                            return String(cString: ptr)
                        }
                    }
                    DispatchQueue.main.async { status = result }
                }
            } label: {
                Text("连接")
                    .frame(maxWidth: .infinity)
            }
            .buttonStyle(.borderedProminent)

            Text(status)
                .font(.footnote)
                .foregroundStyle(.secondary)

            Spacer()
        }
        .padding()
        .onAppear {
            version = String(cString: ad_version())
            hardware = ad_decoder_hardware() != 0
        }
    }
}
