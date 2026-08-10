//! ADREC2 → MP4 转换工具（#234）。
//! 用法: aerodesk-rec2mp4 --input <room.adrec> --output <out.mp4>

use std::path::PathBuf;

fn arg(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!("usage: aerodesk-rec2mp4 --input <room.adrec> --output <out.mp4>");
        std::process::exit(0);
    }
    let input = arg(&args, "--input").expect("--input required");
    let output = arg(&args, "--output").expect("--output required");
    match aerodesk_ffmpeg::mux::adrec_to_mp4(
        PathBuf::from(&input).as_path(),
        PathBuf::from(&output).as_path(),
    ) {
        Ok(n) => {
            println!("OK {input} -> {output} ({n} video packets)");
        }
        Err(e) => {
            eprintln!("FAIL: {e}");
            std::process::exit(1);
        }
    }
}
