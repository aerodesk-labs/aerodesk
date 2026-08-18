//! 帧面选型基准（#516 / ADR-0009 B2，Windows 先行）。
//!
//! 三种载体 × 帧尺寸矩阵，生产者/消费者两线程 ping-pong，测
//! 「写入 → 消费侧帧完整可用」的单向附加延迟（p50/p95/p99）与有效带宽：
//!
//! - `memcpy`：进程内拷贝基线（自旋唤醒），拷贝下限；
//! - `shm`：F1 候选——CreateFileMapping 共享内存 + 事件通知，消费侧拷出
//!   （对应 desktop 从映射视图拷入 Slint 图像的真实路径）；
//! - `pipe`：命名管道字节流（u32 LE 长度前缀），兼作 F2 代理——
//!   编码尺寸载荷（256KB/1MB）即「编码流直连」的传输成本。
//!
//! 运行：`cargo run -p aerodesk-ipc --example ipc_frame_bench --release`
//! 方法学说明与结果归档：docs/IPC_FRAME_BENCHMARK.md。

#[cfg(windows)]
fn main() {
    bench::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("ipc_frame_bench: Windows-only（命名管道/共享内存载体）；其它平台 B4 阶段再补。");
}

#[cfg(windows)]
mod bench {
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::os::windows::io::{FromRawHandle, RawHandle};
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{
        CloseHandle, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
        PAGE_READWRITE, UnmapViewOfFile,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
    use windows::core::{HRESULT, HSTRING, PCWSTR};

    const WARMUP: usize = 50;
    const ITERS: usize = 1000;
    const P1080: usize = 1920 * 1080 * 4; // 8,294,400
    const P4K: usize = 3840 * 2160 * 4; // 33,177,600
    const ENC_256K: usize = 262144; // F2 代理：高码率 P 帧量级
    const ENC_1M: usize = 1_048_576; // F2 代理：IDR 帧量级

    /// 单帧采样：`wake` = 发布→消费侧感知（事件返回/首字节到达）；
    /// `total` = 发布→帧完整拷入消费侧缓冲（≈ UI 可呈现）。
    #[derive(Clone, Copy)]
    struct Sample {
        wake: Duration,
        total: Duration,
    }

    pub fn run() {
        println!(
            "carrier,payload_bytes,wake_p50_ms,wake_p95_ms,wake_p99_ms,total_p50_ms,total_p95_ms,total_p99_ms,eff_MBps"
        );
        let matrix: &[(&str, usize)] = &[
            ("memcpy", P1080),
            ("memcpy", P4K),
            ("shm", P1080),
            ("shm", P4K),
            ("pipe", P1080),
            ("pipe", P4K),
            ("pipe", ENC_256K),
            ("pipe", ENC_1M),
        ];
        for &(carrier, payload) in matrix {
            let samples = match carrier {
                "memcpy" => bench_memcpy(payload),
                "shm" => bench_shm(payload),
                "pipe" => bench_pipe(payload),
                other => unreachable!("未知载体 {other}"),
            };
            report(carrier, payload, &samples);
        }
    }

    fn report(carrier: &str, payload: usize, samples: &[Sample]) {
        let pct = |sel: fn(&Sample) -> Duration, p: f64| -> f64 {
            let mut v: Vec<u128> = samples.iter().map(|s| sel(s).as_nanos()).collect();
            v.sort_unstable();
            let idx = ((v.len() as f64) * p / 100.0).ceil() as usize;
            v[idx.saturating_sub(1).min(v.len() - 1)] as f64 / 1e6
        };
        let p50 = pct(|s| s.total, 50.0);
        let eff_mbps = payload as f64 / (p50 / 1e3) / 1e6; // 单帧 ping-pong 有效带宽
        println!(
            "{carrier},{payload},{:.3},{:.3},{:.3},{p50:.3},{:.3},{:.3},{eff_mbps:.0}",
            pct(|s| s.wake, 50.0),
            pct(|s| s.wake, 95.0),
            pct(|s| s.wake, 99.0),
            pct(|s| s.total, 95.0),
            pct(|s| s.total, 99.0),
        );
    }

    /// 生产者写入源（确定性图案，避免全零页优化干扰）。
    fn source_buf(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    // ---------- 基线：进程内 memcpy + 自旋换手 ----------

    /// 双缓冲换手协议：0=空（生产者可写）、1=满（消费者可读）。
    /// 指针经原子状态 Acquire/Release 严格互斥传递，无并发访问。
    struct SendBuf(*mut u8);
    unsafe impl Send for SendBuf {}
    unsafe impl Sync for SendBuf {}

    fn bench_memcpy(payload: usize) -> Vec<Sample> {
        let src = source_buf(payload);
        let mut dst_storage = vec![0u8; payload];
        let dst = Arc::new(SendBuf(dst_storage.as_mut_ptr()));
        let state = Arc::new(AtomicU8::new(0));
        let t0_slot = Arc::new(Mutex::new(None::<Instant>));

        let (p_state, p_dst, p_t0) = (state.clone(), dst.clone(), t0_slot.clone());
        let producer = thread::spawn(move || {
            for _ in 0..(WARMUP + ITERS) {
                while p_state.load(Ordering::Acquire) != 0 {
                    std::hint::spin_loop();
                }
                let t0 = Instant::now();
                unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), p_dst.0, payload) };
                *p_t0.lock().unwrap() = Some(t0);
                p_state.store(1, Ordering::Release);
            }
        });

        let consumer = thread::spawn(move || {
            let mut out = vec![0u8; payload];
            let mut samples = Vec::with_capacity(ITERS);
            for i in 0..(WARMUP + ITERS) {
                while state.load(Ordering::Acquire) != 1 {
                    std::hint::spin_loop();
                }
                let t0 = t0_slot.lock().unwrap().take().unwrap();
                let wake = t0.elapsed();
                unsafe { std::ptr::copy_nonoverlapping(dst.0, out.as_mut_ptr(), payload) };
                let total = t0.elapsed();
                state.store(0, Ordering::Release);
                if i >= WARMUP {
                    samples.push(Sample { wake, total });
                }
            }
            samples
        });

        producer.join().unwrap();
        consumer.join().unwrap()
    }

    // ---------- F1：共享内存 + 事件 ----------

    struct ShmPair {
        view: MEMORY_MAPPED_VIEW_ADDRESS,
        ready: HANDLE, // 生产者 → 消费者：一帧已写入
        taken: HANDLE, // 消费者 → 生产者：缓冲已取走
        map: HANDLE,
    }
    // 视图指针与句柄在 ready/taken 事件严格交替下互斥访问，无并发读写。
    unsafe impl Send for ShmPair {}
    unsafe impl Sync for ShmPair {}
    impl Drop for ShmPair {
        fn drop(&mut self) {
            unsafe {
                let _ = UnmapViewOfFile(self.view);
                let _ = CloseHandle(self.ready);
                let _ = CloseHandle(self.taken);
                let _ = CloseHandle(self.map);
            }
        }
    }

    fn bench_shm(payload: usize) -> Vec<Sample> {
        let src = source_buf(payload);
        let name = HSTRING::from(format!("Local\\aerodesk-ipc-bench-{}", std::process::id()));
        let pair = unsafe {
            let map = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                (payload >> 32) as u32,
                (payload & 0xffff_ffff) as u32,
                PCWSTR(name.as_ptr()),
            )
            .expect("CreateFileMappingW");
            let view = MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, payload);
            assert!(!view.Value.is_null(), "MapViewOfFile failed");
            let ready = CreateEventW(None, false, false, PCWSTR::null()).expect("event ready");
            let taken = CreateEventW(None, false, false, PCWSTR::null()).expect("event taken");
            // 初始「已取走」，生产者第一帧不必等待。
            let _ = SetEvent(taken);
            ShmPair {
                view,
                ready,
                taken,
                map,
            }
        };
        let pair = Arc::new(pair);
        let t0_slot = Arc::new(Mutex::new(None::<Instant>));

        let (p_pair, p_t0) = (pair.clone(), t0_slot.clone());
        let producer = thread::spawn(move || {
            for _ in 0..(WARMUP + ITERS) {
                unsafe {
                    let r = WaitForSingleObject(p_pair.taken, 10_000);
                    assert_eq!(r, WAIT_OBJECT_0, "taken wait");
                }
                let t0 = Instant::now();
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        src.as_ptr(),
                        p_pair.view.Value as *mut u8,
                        payload,
                    );
                    *p_t0.lock().unwrap() = Some(t0);
                    SetEvent(p_pair.ready).expect("set ready");
                }
            }
        });

        let consumer = thread::spawn(move || {
            let mut out = vec![0u8; payload];
            let mut samples = Vec::with_capacity(ITERS);
            for i in 0..(WARMUP + ITERS) {
                unsafe {
                    let r = WaitForSingleObject(pair.ready, 10_000);
                    assert_eq!(r, WAIT_OBJECT_0, "ready wait");
                }
                let t0 = t0_slot.lock().unwrap().take().unwrap();
                let wake = t0.elapsed();
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        pair.view.Value as *const u8,
                        out.as_mut_ptr(),
                        payload,
                    );
                    SetEvent(pair.taken).expect("set taken");
                }
                let total = t0.elapsed();
                if i >= WARMUP {
                    samples.push(Sample { wake, total });
                }
            }
            samples
        });

        producer.join().unwrap();
        consumer.join().unwrap()
    }

    // ---------- 命名管道字节流 ----------

    fn bench_pipe(payload: usize) -> Vec<Sample> {
        let src = source_buf(payload);
        let pipe_name = format!(r"\\.\pipe\aerodesk-ipc-bench-{}", std::process::id());
        let wide = HSTRING::from(pipe_name.clone());

        let server: File = unsafe {
            let h = CreateNamedPipeW(
                PCWSTR(wide.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                1 << 20,
                1 << 20,
                0,
                None,
            );
            assert!(h != INVALID_HANDLE_VALUE, "CreateNamedPipeW failed");
            File::from_raw_handle(h.0 as RawHandle)
        };

        // 客户端先打开（实例存在即成功），服务端 ConnectNamedPipe 完成握手。
        let client: File = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&pipe_name)
            .expect("open pipe client");
        // 客户端抢在调用前连入时返回 ERROR_PIPE_CONNECTED，属成功路径（MSDN）。
        unsafe { ConnectNamedPipe(HANDLE(server.as_raw_handle() as _), None) }.unwrap_or_else(
            |e| {
                assert_eq!(
                    e.code(),
                    HRESULT::from_win32(ERROR_PIPE_CONNECTED.0),
                    "ConnectNamedPipe: {e}"
                );
            },
        );

        let t0_slot = Arc::new(Mutex::new(None::<Instant>));
        let p_t0 = t0_slot.clone();
        let mut producer_file = server; // 生产者 → 客户端方向
        let producer = thread::spawn(move || {
            for _ in 0..(WARMUP + ITERS) {
                // 等消费侧取走上一帧的时间戳再发下一帧——保持 ping-pong 语义，
                // 否则小载荷下生产者会越过槽位连续覆写（丢样本）。
                while p_t0.lock().unwrap().is_some() {
                    std::hint::spin_loop();
                }
                let t0 = Instant::now();
                *p_t0.lock().unwrap() = Some(t0);
                producer_file
                    .write_all(&(payload as u32).to_le_bytes())
                    .unwrap();
                producer_file.write_all(&src).unwrap();
            }
        });

        let consumer = thread::spawn(move || {
            let mut client = client;
            let mut buf = vec![0u8; payload];
            let mut samples = Vec::with_capacity(ITERS);
            for i in 0..(WARMUP + ITERS) {
                let mut len_buf = [0u8; 4];
                client.read_exact(&mut len_buf).unwrap();
                let t0 = t0_slot.lock().unwrap().take().unwrap();
                let wake = t0.elapsed();
                let len = u32::from_le_bytes(len_buf) as usize;
                assert_eq!(len, payload, "帧长不一致");
                client.read_exact(&mut buf[..len]).unwrap();
                let total = t0.elapsed();
                if i >= WARMUP {
                    samples.push(Sample { wake, total });
                }
            }
            samples
        });

        producer.join().unwrap();
        consumer.join().unwrap()
    }

    // File::as_raw_handle 需要 trait 引入。
    use std::os::windows::io::AsRawHandle as _;
}
