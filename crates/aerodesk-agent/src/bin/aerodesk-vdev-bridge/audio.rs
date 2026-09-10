//! 音频输出：Opus 解码 PCM → SPSC ring → AudioUnit → vdev-audio 声卡。
//! 精简自 vdev-app/audio.rs（去掉 AVAssetReader 解音轨，只留 PCM→声卡）。

use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

const K_AUDIO_OBJECT_SYSTEM: u32 = 1;
const K_HW_PROP_DEVICES: u32 = 0x64657623; // 'dev#'
const K_OBJ_SCOPE_GLOBAL: u32 = 0x676c6f62; // 'glob'
const K_OBJ_ELEMENT_MAIN: u32 = 0;
const K_DEV_PROP_NAME_CFSTRING: u32 = 0x6c6e616d; // 'lnam'
const K_AUDIO_UNIT_TYPE_OUTPUT: u32 = 0x61756f75; // 'auou'
const K_AUDIO_UNIT_SUBTYPE_HAL: u32 = 0x6168616c; // 'ahal'
const K_AUDIO_UNIT_MANUF_APPLE: u32 = 0x6170706c; // 'appl'
const K_OUTPUT_UNIT_PROP_CURRENT_DEVICE: u32 = 2000;
const K_OUTPUT_UNIT_PROP_ENABLE_IO: u32 = 2003;
const K_AUDIO_UNIT_PROP_STREAM_FORMAT: u32 = 8;
const K_AUDIO_UNIT_PROP_SET_RENDER_CB: u32 = 23;
const K_AUDIO_UNIT_SCOPE_GLOBAL: u32 = 0;
const K_AUDIO_UNIT_SCOPE_INPUT: u32 = 1;
const K_AUDIO_UNIT_SCOPE_OUTPUT: u32 = 2;
const SAMPLE_RATE: f64 = 48_000.0;
const CHANNELS: u32 = 2;

type AudioObjectID = u32;

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioObjectPropertyAddress {
    m_selector: u32,
    m_scope: u32,
    m_element: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioStreamBasicDescription {
    m_sample_rate: f64,
    m_format_id: u32,
    m_format_flags: u32,
    m_bytes_per_packet: u32,
    m_frames_per_packet: u32,
    m_bytes_per_frame: u32,
    m_channels_per_frame: u32,
    m_bits_per_channel: u32,
    m_reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioBuffer {
    m_number_channels: u32,
    m_data_byte_size: u32,
    m_data: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioBufferList {
    m_number_buffers: u32,
    m_buffers: [AudioBuffer; 1],
}

#[repr(C)]
struct AudioTimeStamp {
    m_sample_time: f64,
    m_host_time: u64,
    m_rate_scalar: f64,
    m_word_clock_time: u64,
    m_smpte_time: u64,
    m_flags: u32,
    m_reserved: u32,
}
#[repr(C)]
struct AudioUnitRenderingActionFlags(u32);
#[repr(C)]
struct OpaqueAudioComponent(*mut c_void);
#[repr(C)]
struct OpaqueAudioComponentInstance(*mut c_void);

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyDataSize(
        object: u32,
        address: *const AudioObjectPropertyAddress,
        qsize: u32,
        qdata: *const c_void,
        size: *mut u32,
    ) -> i32;
    fn AudioObjectGetPropertyData(
        object: u32,
        address: *const AudioObjectPropertyAddress,
        qsize: u32,
        qdata: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> i32;
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioComponentFindNext(
        component: *mut OpaqueAudioComponent,
        desc: *const AudioComponentDescription,
    ) -> *mut OpaqueAudioComponent;
    fn AudioComponentInstanceNew(
        component: *mut OpaqueAudioComponent,
        out: *mut *mut OpaqueAudioComponentInstance,
    ) -> i32;
    fn AudioUnitInitialize(unit: *mut OpaqueAudioComponentInstance) -> i32;
    fn AudioUnitUninitialize(unit: *mut OpaqueAudioComponentInstance) -> i32;
    fn AudioComponentInstanceDispose(unit: *mut OpaqueAudioComponentInstance) -> i32;
    fn AudioUnitSetProperty(
        unit: *mut OpaqueAudioComponentInstance,
        id: u32,
        scope: u32,
        element: u32,
        data: *const c_void,
        data_size: u32,
    ) -> i32;
    fn AudioOutputUnitStart(unit: *mut OpaqueAudioComponentInstance) -> i32;
    fn AudioOutputUnitStop(unit: *mut OpaqueAudioComponentInstance) -> i32;
}

#[repr(C)]
struct AudioComponentDescription {
    component_type: u32,
    component_sub_type: u32,
    component_manufacturer: u32,
    component_flags: u32,
    component_flags_mask: u32,
}

// ---- 无锁 SPSC ring（f32 交错立体声）----
const RING_LEN: usize = 65536;
struct SpscRing {
    buf: std::cell::UnsafeCell<Vec<f32>>,
    mask: usize,
    head: AtomicUsize,
    tail: AtomicUsize,
}
unsafe impl Sync for SpscRing {}
static RING: OnceLock<SpscRing> = OnceLock::new();
fn ring() -> &'static SpscRing {
    RING.get_or_init(|| SpscRing {
        buf: std::cell::UnsafeCell::new(vec![0.0f32; RING_LEN]),
        mask: RING_LEN - 1,
        head: AtomicUsize::new(0),
        tail: AtomicUsize::new(0),
    })
}
fn ring_push(data: &[f32]) -> usize {
    let r = ring();
    let head = r.head.load(Ordering::Relaxed);
    let tail = r.tail.load(Ordering::Relaxed);
    let used = head.wrapping_sub(tail) & r.mask;
    let free = (RING_LEN - 1) - used;
    let n = data.len().min(free);
    let buf = unsafe { &mut *r.buf.get() };
    for i in 0..n {
        buf[(head + i) & r.mask] = data[i];
    }
    r.head.store(head.wrapping_add(n), Ordering::Release);
    n
}
fn ring_pop(dst: &mut [f32]) -> usize {
    let r = ring();
    let head = r.head.load(Ordering::Acquire);
    let tail = r.tail.load(Ordering::Relaxed);
    let used = head.wrapping_sub(tail) & r.mask;
    let n = used.min(dst.len());
    let buf = unsafe { &*r.buf.get() };
    for i in 0..n {
        dst[i] = buf[(tail + i) & r.mask];
    }
    r.tail.store(tail.wrapping_add(n), Ordering::Release);
    n
}

// 渲染回调（实时线程）：从 ring 读，不足补静音
extern "C" fn render_cb(
    _ref_con: *mut c_void,
    _flags: *mut AudioUnitRenderingActionFlags,
    _ts: *const AudioTimeStamp,
    _bus: u32,
    frames: u32,
    io_data: *mut AudioBufferList,
) -> i32 {
    if io_data.is_null() {
        return 0;
    }
    let abl = unsafe { &*io_data };
    if abl.m_number_buffers == 0 || abl.m_buffers[0].m_data.is_null() {
        return 0;
    }
    let cap = (abl.m_buffers[0].m_data_byte_size as usize) / 4;
    let samples = (frames as usize * CHANNELS as usize).min(cap);
    let dst =
        unsafe { std::slice::from_raw_parts_mut(abl.m_buffers[0].m_data as *mut f32, samples) };
    let n = ring_pop(dst);
    if n < samples {
        for v in dst[n..].iter_mut() {
            *v = 0.0;
        }
    }
    0
}

fn find_device(keyword: &str) -> Option<AudioObjectID> {
    unsafe {
        let addr = AudioObjectPropertyAddress {
            m_selector: K_HW_PROP_DEVICES,
            m_scope: K_OBJ_SCOPE_GLOBAL,
            m_element: K_OBJ_ELEMENT_MAIN,
        };
        let mut size = 0u32;
        if AudioObjectGetPropertyDataSize(
            K_AUDIO_OBJECT_SYSTEM,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
        ) != 0
        {
            return None;
        }
        let count = size as usize / 4;
        let mut ids = vec![0u32; count];
        if AudioObjectGetPropertyData(
            K_AUDIO_OBJECT_SYSTEM,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            ids.as_mut_ptr() as *mut c_void,
        ) != 0
        {
            return None;
        }
        let na = AudioObjectPropertyAddress {
            m_selector: K_DEV_PROP_NAME_CFSTRING,
            m_scope: K_OBJ_SCOPE_GLOBAL,
            m_element: K_OBJ_ELEMENT_MAIN,
        };
        for id in ids {
            let mut cf: *mut c_void = std::ptr::null_mut();
            let mut nsize = std::mem::size_of::<*mut c_void>() as u32;
            if AudioObjectGetPropertyData(
                id,
                &na,
                0,
                std::ptr::null(),
                &mut nsize,
                &mut cf as *mut *mut c_void as *mut c_void,
            ) == 0
                && !cf.is_null()
            {
                unsafe extern "C" {
                    fn CFStringGetCString(
                        s: *const c_void,
                        buf: *mut std::os::raw::c_char,
                        len: isize,
                        encoding: u32,
                    ) -> bool;
                    fn CFRelease(o: *const c_void);
                }
                let mut b = [0 as std::os::raw::c_char; 256];
                let ok = CFStringGetCString(cf, b.as_mut_ptr(), 256, 0x08000100);
                CFRelease(cf);
                if ok {
                    let s = std::ffi::CStr::from_ptr(b.as_ptr())
                        .to_string_lossy()
                        .to_string();
                    if s.to_lowercase().contains(&keyword.to_lowercase()) {
                        return Some(id);
                    }
                }
            }
        }
    }
    None
}

pub struct AudioSink {
    unit: *mut OpaqueAudioComponentInstance,
    started: bool,
}

impl AudioSink {
    pub fn new() -> Result<Self, String> {
        let Some(dev_id) = find_device("vdev-audio") else {
            return Err("未找到 vdev-audio 声卡".into());
        };
        let desc = AudioComponentDescription {
            component_type: K_AUDIO_UNIT_TYPE_OUTPUT,
            component_sub_type: K_AUDIO_UNIT_SUBTYPE_HAL,
            component_manufacturer: K_AUDIO_UNIT_MANUF_APPLE,
            component_flags: 0,
            component_flags_mask: 0,
        };
        let comp = unsafe { AudioComponentFindNext(std::ptr::null_mut(), &desc) };
        if comp.is_null() {
            return Err("找不到 HALOutput AudioUnit".into());
        }
        let mut unit: *mut OpaqueAudioComponentInstance = std::ptr::null_mut();
        if unsafe { AudioComponentInstanceNew(comp, &mut unit) } != 0 || unit.is_null() {
            return Err("AudioComponentInstanceNew 失败".into());
        }
        // 纯输出单元
        let enable: u32 = 1;
        let disable: u32 = 0;
        unsafe {
            AudioUnitSetProperty(
                unit,
                K_OUTPUT_UNIT_PROP_ENABLE_IO,
                K_AUDIO_UNIT_SCOPE_OUTPUT,
                0,
                &enable as *const u32 as *const c_void,
                4,
            );
            AudioUnitSetProperty(
                unit,
                K_OUTPUT_UNIT_PROP_ENABLE_IO,
                K_AUDIO_UNIT_SCOPE_INPUT,
                1,
                &disable as *const u32 as *const c_void,
                4,
            );
            // 指定输出设备 = vdev-audio
            AudioUnitSetProperty(
                unit,
                K_OUTPUT_UNIT_PROP_CURRENT_DEVICE,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
                &dev_id as *const u32 as *const c_void,
                4,
            );
            // 流格式 48k 2ch float
            let fmt = AudioStreamBasicDescription {
                m_sample_rate: SAMPLE_RATE,
                m_format_id: 0x6c70636d,
                m_format_flags: 0x09,
                m_bytes_per_packet: 8,
                m_frames_per_packet: 1,
                m_bytes_per_frame: 8,
                m_channels_per_frame: CHANNELS,
                m_bits_per_channel: 32,
                m_reserved: 0,
            };
            AudioUnitSetProperty(
                unit,
                K_AUDIO_UNIT_PROP_STREAM_FORMAT,
                K_AUDIO_UNIT_SCOPE_INPUT,
                0,
                &fmt as *const _ as *const c_void,
                std::mem::size_of::<AudioStreamBasicDescription>() as u32,
            );
            if AudioUnitInitialize(unit) != 0 {
                AudioComponentInstanceDispose(unit);
                return Err("AudioUnitInitialize 失败".into());
            }
        }
        // 渲染回调
        #[repr(C)]
        struct AURenderCallbackStruct {
            input_proc: *const c_void,
            input_proc_ref_con: *mut c_void,
        }
        let cb = AURenderCallbackStruct {
            input_proc: render_cb as *const c_void,
            input_proc_ref_con: std::ptr::null_mut(),
        };
        unsafe {
            AudioUnitSetProperty(
                unit,
                K_AUDIO_UNIT_PROP_SET_RENDER_CB,
                K_AUDIO_UNIT_SCOPE_INPUT,
                0,
                &cb as *const _ as *const c_void,
                std::mem::size_of::<AURenderCallbackStruct>() as u32,
            );
            AudioOutputUnitStart(unit);
        }
        println!("音频输出：vdev-audio 声卡（设备 {dev_id}）");
        Ok(Self {
            unit,
            started: true,
        })
    }

    /// 写入单声道 i16 PCM（Opus 解码输出），自动转 f32 立体声交错
    pub fn push_mono_i16(&mut self, pcm: &[i16]) {
        let stereo: Vec<f32> = pcm
            .iter()
            .flat_map(|&s| {
                let v = s as f32 / 32768.0;
                [v, v]
            })
            .collect();
        let mut off = 0;
        while off < stereo.len() {
            let n = ring_push(&stereo[off..]);
            if n == 0 {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            off += n;
        }
    }
}

impl Drop for AudioSink {
    fn drop(&mut self) {
        if self.started {
            unsafe {
                AudioOutputUnitStop(self.unit);
                AudioUnitUninitialize(self.unit);
                AudioComponentInstanceDispose(self.unit);
            }
        }
    }
}
