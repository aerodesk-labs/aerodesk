//! F1 帧面通道：共享内存双槽环形 + 事件换手（docs/IPC_PROTOCOL.md §4 的产品化；
//! 协议形态即 `examples/ipc_frame_bench.rs` 基准验证过的 shm 载体，结论见
//! docs/IPC_FRAME_BENCHMARK.md：p95 附加延迟 1080p 0.75ms / 4K 3.09ms）。
//!
//! SPSC 语义：单一写方（host 引擎解码线程）经 [`FrameRingWriter::create`] 建环，
//! 单一读方（desktop 呈现线程）经 [`FrameRingReader::open_wait`] 打开。双槽允许
//! 「写 B 槽时读 A 槽」的流水重叠；换手经 ready/taken 双事件 + 共享头里的
//! write_seq/read_seq 序号（跨进程原子，x86 对齐 u32 原子操作在共享页上相干）。
//!
//! 失效处理（§4）：写方 `taken` 等待超时即视为 UI 侧卡死——调用方（host）回收
//! 该会话帧面，引擎不被阻塞；读方按自身节奏丢帧追赶（读序号跳读由调用方决定，
//! 本层保序不丢帧）。
//!
//! 平台：Windows 先行（`Local\aerodesk-frame-<name>` + 命名事件）；macOS/Linux
//! 载体在对应平台批次另测（§4 口径：p95 <5ms）。本模块仅 cfg(windows) 提供。

#![cfg(windows)]

use std::io;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::core::{HSTRING, PCWSTR};

use crate::transport::validate_name;

/// 共享头魔数（"AFBF"）与 ABI 版本；读方打开时校验，不符即视为未就绪/不兼容。
const MAGIC: u32 = u32::from_le_bytes(*b"AFBF");
const ABI: u32 = 1;
/// 双槽环形（协议 §4）。
const SLOTS: u32 = 2;
/// 共享头 64B：magic/abi/slot_bytes/slot_count/write_seq/read_seq + 预留。
const HEADER_BYTES: usize = 64;
/// 槽头 16B：w/h/len + 预留。
const SLOT_HDR: usize = 16;

/// 帧元数据（每槽自带，随帧变化）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameMeta {
    pub width: u32,
    pub height: u32,
    /// 本帧载荷字节数（≤ 建环时的 slot_bytes）。
    pub len: u32,
}

/// 共享内存视图与事件句柄的公共持有；Drop 统一回收。
struct Ring {
    map: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    ready: HANDLE,
    taken: HANDLE,
    slot_bytes: usize,
}

// HANDLE/视图指针仅随所有者线程移动（writer/reader 各为单持有者，写读方法
// 都要 &mut self），不存在跨线程并发访问；SPSC 同步由序号原子 + 事件保证。
unsafe impl Send for Ring {}

impl Ring {
    fn total_bytes(slot_bytes: usize) -> usize {
        HEADER_BYTES + SLOTS as usize * (SLOT_HDR + slot_bytes)
    }

    fn header_u32(&self, offset: usize) -> &std::sync::atomic::AtomicU32 {
        // 共享头为定长定偏移布局；aligned u32 原子访问在映射页上跨进程相干。
        unsafe { &*(self.view.Value.cast::<u8>().add(offset).cast()) }
    }

    fn write_seq(&self) -> u32 {
        self.header_u32(16).load(Ordering::Acquire)
    }
    fn read_seq(&self) -> u32 {
        self.header_u32(20).load(Ordering::Acquire)
    }

    fn slot_base(&self, seq: u32) -> *mut u8 {
        let idx = (seq % SLOTS) as usize;
        unsafe {
            self.view
                .Value
                .cast::<u8>()
                .add(HEADER_BYTES + idx * (SLOT_HDR + self.slot_bytes))
        }
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.ready);
            let _ = CloseHandle(self.taken);
            let _ = CloseHandle(self.map);
        }
    }
}

fn mapping_name(name: &str) -> HSTRING {
    HSTRING::from(format!(r"Local\aerodesk-frame-{name}"))
}

fn open_or_create_event(suffix: &str, name: &str) -> io::Result<HANDLE> {
    // 两侧均走 CreateEventW：先建者创建、后建者自动得同一对象句柄
    // （ERROR_ALREADY_EXISTS 无害），省掉 OpenEventW 的权限位编排。
    let wide = HSTRING::from(format!(r"Local\aerodesk-frame-{name}-{suffix}"));
    unsafe { CreateEventW(None, false, false, PCWSTR(wide.as_ptr())) }.map_err(io::Error::other)
}

/// 写方（host 引擎侧）：创建并拥有环形。
pub struct FrameRingWriter {
    ring: Ring,
    next_seq: u32,
}

impl FrameRingWriter {
    /// 建环：`slot_bytes` 为单帧上限（4K RGBA = 33,177,600B；双槽 4K 环约 64MB）。
    pub fn create(name: &str, slot_bytes: usize) -> io::Result<Self> {
        validate_name(name)?;
        if slot_bytes == 0 || slot_bytes > 64 * 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("slot_bytes {slot_bytes} out of (0, 64MB]"),
            ));
        }
        let total = Ring::total_bytes(slot_bytes);
        let mapping = mapping_name(name);
        let map = unsafe {
            CreateFileMappingW(
                HANDLE::default(), // INVALID_HANDLE_VALUE：页面文件后备
                None,
                PAGE_READWRITE,
                (total >> 32) as u32,
                (total & 0xffff_ffff) as u32,
                PCWSTR(mapping.as_ptr()),
            )
        }
        .map_err(io::Error::other)?;
        let view = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, total) };
        if view.Value.is_null() {
            let e = io::Error::last_os_error();
            unsafe {
                let _ = CloseHandle(map);
            };
            return Err(e);
        }
        let ready = open_or_create_event("r", name)?;
        let taken = open_or_create_event("t", name)?;
        let ring = Ring {
            map,
            view,
            ready,
            taken,
            slot_bytes,
        };
        // 头字段先于 magic 发布；映射页初始为零，读方轮询 magic 即见一致头。
        ring.header_u32(8)
            .store(slot_bytes as u32, Ordering::Relaxed);
        ring.header_u32(12).store(SLOTS, Ordering::Relaxed);
        ring.header_u32(16).store(0, Ordering::Relaxed);
        ring.header_u32(20).store(0, Ordering::Relaxed);
        ring.header_u32(4).store(ABI, Ordering::Relaxed);
        ring.header_u32(0).store(MAGIC, Ordering::Release);
        Ok(Self { ring, next_seq: 0 })
    }

    pub fn slot_bytes(&self) -> usize {
        self.ring.slot_bytes
    }

    /// 写一帧（拷入槽位并发布）。环满（读方落后 2 帧）时等 `taken` 至
    /// `timeout`，超时返回 `Err(TimedOut)`——host 侧据此判定 UI 卡死并回收。
    pub fn write_frame(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
        timeout: Duration,
    ) -> io::Result<()> {
        if rgba.len() > self.ring.slot_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "frame {}B exceeds slot {}B",
                    rgba.len(),
                    self.ring.slot_bytes
                ),
            ));
        }
        let deadline = Instant::now() + timeout;
        while self.next_seq.wrapping_sub(self.ring.read_seq()) >= SLOTS {
            let remain = deadline.saturating_duration_since(Instant::now());
            if remain.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "ring full"));
            }
            let ms = remain.as_millis().min(u32::MAX as u128) as u32;
            let r = unsafe { WaitForSingleObject(self.ring.taken, ms) };
            if r == WAIT_TIMEOUT {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "ring full"));
            }
            if r != WAIT_OBJECT_0 {
                return Err(io::Error::last_os_error());
            }
        }
        let base = self.ring.slot_base(self.next_seq);
        unsafe {
            // 先载荷与槽头，再以 Release 发布序号——读方 Acquire 读序号后
            // 见到的载荷必然完整（单写单读，槽位复用受环满等待保护）。
            std::ptr::copy_nonoverlapping(rgba.as_ptr(), base.add(SLOT_HDR), rgba.len());
            (base as *mut u32).write(width);
            (base.add(4) as *mut u32).write(height);
            (base.add(8) as *mut u32).write(rgba.len() as u32);
        }
        self.next_seq = self.next_seq.wrapping_add(1);
        self.ring
            .header_u32(16)
            .store(self.next_seq, Ordering::Release);
        unsafe { SetEvent(self.ring.ready) }.map_err(io::Error::other)?;
        Ok(())
    }
}

/// 读方（desktop 呈现侧）：打开既有环形。
pub struct FrameRingReader {
    ring: Ring,
    next_seq: u32,
}

impl FrameRingReader {
    /// 打开并等待写方就绪（映射存在且头魔数发布），至 `timeout`。
    pub fn open_wait(name: &str, timeout: Duration) -> io::Result<Self> {
        validate_name(name)?;
        let deadline = Instant::now() + timeout;
        let mapping = mapping_name(name);
        loop {
            match Self::try_open(&mapping, name) {
                Ok(r) => return Ok(r),
                Err(e) => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("ring {name} not ready: {e}"),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    fn try_open(mapping: &HSTRING, name: &str) -> io::Result<Self> {
        let map =
            unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, PCWSTR(mapping.as_ptr())) }
                .map_err(io::Error::other)?;
        let probe = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, HEADER_BYTES) };
        if probe.Value.is_null() {
            unsafe {
                let _ = CloseHandle(map);
            };
            return Err(io::Error::last_os_error());
        }
        let read_hdr = |off: usize| -> u32 {
            unsafe { (probe.Value.cast::<u8>().add(off) as *const u32).read_volatile() }
        };
        let (magic, abi, slot_bytes, slots) =
            (read_hdr(0), read_hdr(4), read_hdr(8) as usize, read_hdr(12));
        unsafe {
            let _ = UnmapViewOfFile(probe);
        };
        if magic != MAGIC || abi != ABI {
            unsafe {
                let _ = CloseHandle(map);
            };
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("bad magic/abi {magic:#x}/{abi}"),
            ));
        }
        if slots != SLOTS || slot_bytes == 0 || slot_bytes > 64 * 1024 * 1024 {
            unsafe {
                let _ = CloseHandle(map);
            };
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad ring geometry slots={slots} slot_bytes={slot_bytes}"),
            ));
        }
        let total = Ring::total_bytes(slot_bytes);
        let view = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, total) };
        if view.Value.is_null() {
            unsafe {
                let _ = CloseHandle(map);
            };
            return Err(io::Error::last_os_error());
        }
        let ready = open_or_create_event("r", name)?;
        let taken = open_or_create_event("t", name)?;
        Ok(Self {
            ring: Ring {
                map,
                view,
                ready,
                taken,
                slot_bytes,
            },
            next_seq: 0,
        })
    }

    pub fn slot_bytes(&self) -> usize {
        self.ring.slot_bytes
    }

    /// 读一帧（拷出至 `dst`），无帧时等 `ready` 至 `timeout`。
    /// `dst` 须 ≥ 本帧 len（调用方按 slot_bytes 分配即可恒够）。
    pub fn read_frame(&mut self, dst: &mut [u8], timeout: Duration) -> io::Result<FrameMeta> {
        let deadline = Instant::now() + timeout;
        while self.next_seq == self.ring.write_seq() {
            let remain = deadline.saturating_duration_since(Instant::now());
            if remain.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "no frame"));
            }
            let ms = remain.as_millis().min(u32::MAX as u128) as u32;
            let r = unsafe { WaitForSingleObject(self.ring.ready, ms) };
            if r == WAIT_TIMEOUT {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "no frame"));
            }
            if r != WAIT_OBJECT_0 {
                return Err(io::Error::last_os_error());
            }
        }
        let base = self.ring.slot_base(self.next_seq);
        let (width, height, len) = unsafe {
            (
                (base as *const u32).read(),
                (base.add(4) as *const u32).read(),
                (base.add(8) as *const u32).read() as usize,
            )
        };
        if len > dst.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("dst {}B < frame {len}B", dst.len()),
            ));
        }
        unsafe { std::ptr::copy_nonoverlapping(base.add(SLOT_HDR), dst.as_mut_ptr(), len) };
        self.next_seq = self.next_seq.wrapping_add(1);
        self.ring
            .header_u32(20)
            .store(self.next_seq, Ordering::Release);
        unsafe { SetEvent(self.ring.taken) }.map_err(io::Error::other)?;
        Ok(FrameMeta {
            width,
            height,
            len: len as u32,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering as AOrd};

    fn unique_name(tag: &str) -> String {
        static N: AtomicU32 = AtomicU32::new(0);
        format!("fb{}-{}", N.fetch_add(1, AOrd::Relaxed), tag)
    }

    fn fill(seq: usize, len: usize) -> Vec<u8> {
        (0..len).map(|j| ((seq + j) % 251) as u8).collect()
    }

    #[test]
    fn writer_reader_roundtrip_integrity() {
        let name = unique_name("rr");
        let slot = 1 << 20;
        let mut writer = FrameRingWriter::create(&name, slot).unwrap();
        let mut reader = FrameRingReader::open_wait(&name, Duration::from_secs(5)).unwrap();
        assert_eq!(reader.slot_bytes(), slot);

        const FRAMES: usize = 200;
        let wt = std::thread::spawn(move || {
            for i in 0..FRAMES {
                let len = 64 + (i * 4093) % (slot - 64);
                writer
                    .write_frame(&fill(i, len), 1920, 1080, Duration::from_secs(5))
                    .unwrap();
            }
        });
        let rt = std::thread::spawn(move || {
            let mut dst = vec![0u8; slot];
            for i in 0..FRAMES {
                let meta = reader.read_frame(&mut dst, Duration::from_secs(5)).unwrap();
                let want_len = 64 + (i * 4093) % (slot - 64);
                assert_eq!((meta.width, meta.height), (1920, 1080));
                assert_eq!(meta.len as usize, want_len);
                let want = fill(i, want_len);
                assert_eq!(&dst[..want_len], &want[..], "frame {i} payload mismatch");
            }
            reader
        });
        wt.join().unwrap();
        let reader = rt.join().unwrap();
        assert_eq!(reader.next_seq, FRAMES as u32);
    }

    #[test]
    fn writer_times_out_when_ring_full() {
        let name = unique_name("full");
        let mut writer = FrameRingWriter::create(&name, 4096).unwrap();
        let f = vec![7u8; 4096];
        writer
            .write_frame(&f, 64, 64, Duration::from_secs(1))
            .unwrap();
        writer
            .write_frame(&f, 64, 64, Duration::from_secs(1))
            .unwrap();
        // 双槽已满且无人消费 → 第三帧超时。
        let e = writer
            .write_frame(&f, 64, 64, Duration::from_millis(150))
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        // 超尺寸帧直接拒绝。
        let big = vec![0u8; 4097];
        assert!(
            writer
                .write_frame(&big, 64, 64, Duration::from_millis(1))
                .is_err()
        );
    }

    #[test]
    fn reader_times_out_when_empty() {
        let name = unique_name("empty");
        let _writer = FrameRingWriter::create(&name, 4096).unwrap();
        let mut reader = FrameRingReader::open_wait(&name, Duration::from_secs(5)).unwrap();
        let mut dst = vec![0u8; 4096];
        let e = reader
            .read_frame(&mut dst, Duration::from_millis(150))
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn reader_open_wait_times_out_without_writer() {
        match FrameRingReader::open_wait("fb-nonexist", Duration::from_millis(200)) {
            Ok(_) => panic!("expected timeout when no writer exists"),
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::TimedOut),
        }
    }
}
