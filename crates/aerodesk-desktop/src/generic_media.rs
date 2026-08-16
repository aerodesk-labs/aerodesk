//! 非 macOS 桌面端（Windows/Linux）主控端：真实媒体观看。
//!
//! #277：解码/渲染统一走 core `Decoder`/`Renderer` trait（泛型管线
//! `generic_viewer::run_viewer_generic`），本模块只负责组装解码器 +
//! SlintRenderer。Linux 优先 VAAPI 硬解（无 /dev/dri 时回退 OpenH264 软解），
//! Windows 优先 DXVA2 硬解（无 GPU 时回退 OpenH264 软解）。

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// 观看端解码器：VAAPI 硬解（Linux，设备可用时）→ OpenH264 软解回退。
///
/// 枚举封装避免 `Box<dyn Decoder>`（trait 无 Box 兜底实现）；
/// `Decoder::configure` 按实际 codec 重建（H.264/HEVC/VP9/AV1）。
enum ViewerDecoder {
    Soft(aerodesk_codec::softenc::decode::SoftDecoder),
    #[cfg(target_os = "linux")]
    Vaapi(aerodesk_platform::linux::vaapi::VaapiDecoder),
    // #3 Windows 观看端硬解（DXVA2）；设备创建失败时回退 OpenH264 软解
    //（见 mk_viewer_decoder）。
    #[cfg(target_os = "windows")]
    Dxva2(aerodesk_platform::windows::decode::Dxva2Decoder),
}

impl aerodesk_core::platform::Decoder for ViewerDecoder {
    type Error = String;

    fn configure(
        &mut self,
        codec: aerodesk_core::media_pipeline::Codec,
        width: u32,
        height: u32,
    ) -> Result<(), Self::Error> {
        match self {
            Self::Soft(d) => aerodesk_core::platform::Decoder::configure(d, codec, width, height),
            #[cfg(target_os = "linux")]
            Self::Vaapi(d) => aerodesk_core::platform::Decoder::configure(d, codec, width, height),
            #[cfg(target_os = "windows")]
            Self::Dxva2(d) => aerodesk_core::platform::Decoder::configure(d, codec, width, height),
        }
    }

    fn decode(
        &mut self,
        unit: &aerodesk_core::media_pipeline::EncodedUnit,
    ) -> Result<Option<aerodesk_core::platform::VideoFrame>, Self::Error> {
        match self {
            Self::Soft(d) => aerodesk_core::platform::Decoder::decode(d, unit),
            #[cfg(target_os = "linux")]
            Self::Vaapi(d) => aerodesk_core::platform::Decoder::decode(d, unit),
            #[cfg(target_os = "windows")]
            Self::Dxva2(d) => aerodesk_core::platform::Decoder::decode(d, unit),
        }
    }
}

fn mk_viewer_decoder() -> Result<ViewerDecoder, String> {
    #[cfg(target_os = "linux")]
    {
        use aerodesk_core::media_pipeline::Codec;
        match aerodesk_platform::linux::vaapi::VaapiDecoder::new(Codec::H264) {
            Ok(d) => {
                tracing::info!("linux viewer: VAAPI 硬解启用");
                return Ok(ViewerDecoder::Vaapi(d));
            }
            Err(e) => {
                tracing::warn!("linux viewer: VAAPI 不可用（{e}），回退 OpenH264 软解");
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        match aerodesk_platform::windows::decode::Dxva2Decoder::new() {
            Ok(d) => {
                tracing::info!("windows viewer: DXVA2 硬解启用");
                return Ok(ViewerDecoder::Dxva2(d));
            }
            Err(e) => {
                tracing::warn!("windows viewer: 硬件解码不可用（{e}），回退 OpenH264 软解");
            }
        }
    }
    Ok(ViewerDecoder::Soft(
        aerodesk_codec::softenc::decode::SoftDecoder::new()?,
    ))
}

/// 解码器状态栏显示名（平台相关）。
fn decoder_label() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "VAAPI 硬解/OpenH264 软解"
    }
    #[cfg(target_os = "windows")]
    {
        "DXVA2 硬解/OpenH264 软解"
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        "OpenH264 软解"
    }
}

/// 启动非 macOS 主控端观看：连接 → 收流 → 组装 → 解码（VAAPI/软解）→ Slint 渲染。
pub fn run_generic_viewer(
    server: String,
    room: String,
    token: Option<String>,
    ui_weak: slint::Weak<crate::AppWindow>,
    session_idx: usize,
    input_rx: std::sync::mpsc::Receiver<String>,
    cmd_rx: std::sync::mpsc::Receiver<aerodesk_protocol::cmd::CmdRequest>,
    file_cmd_rx: std::sync::mpsc::Receiver<crate::FileCmd>,
    chat_cmd_rx: std::sync::mpsc::Receiver<crate::ChatCmd>,
    stop: Arc<AtomicBool>,
    view_only: Arc<AtomicBool>,
) {
    let ui2 = ui_weak.clone();
    crate::generic_viewer::run_viewer_generic(
        server,
        room,
        token,
        ui_weak,
        session_idx,
        input_rx,
        cmd_rx,
        file_cmd_rx,
        chat_cmd_rx,
        stop,
        view_only,
        decoder_label(),
        mk_viewer_decoder,
        move || crate::SlintRenderer::new(ui2.clone(), session_idx),
    );
}
