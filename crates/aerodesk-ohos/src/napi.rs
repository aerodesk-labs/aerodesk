#![cfg(target_env = "ohos")]

//! 手写 NAPI 桥（OpenHarmony ArkTS ↔ aerodesk-core）。
//!
//! 不依赖 `napi-rs`，避免引入额外 C 构建路径；仅声明 Node-API 的 C ABI，
//! 由 OHOS 运行时在加载 `.so` 时解析。模块入口为
//! `napi_register_module_v1`（等价于 `NAPI_MODULE` 宏展开）。
//!
//! 暴露给 ArkTS 壳层的函数（与 docs/HARMONYOS.md 接口一致）：
//! `connectViewer` / `takeFrame` / `disconnect` / `startPublish` / `injectInput`。

use std::collections::HashMap;
use std::ffi::{CString, c_char, c_void};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{LazyLock, Mutex};

use aerodesk_platform::ohos::publisher::PublisherSession;
use aerodesk_platform::ohos::viewer::ViewerSession;

// ---- Node-API C ABI 类型 ----

type NapiEnv = *mut c_void;
type NapiValue = *mut c_void;
type NapiCallbackInfo = *mut c_void;
type NapiStatus = i32;
type NapiCallback = Option<unsafe extern "C" fn(NapiEnv, NapiCallbackInfo) -> NapiValue>;

const NAPI_OK: NapiStatus = 0;
const NAPI_UINT8_ARRAY: i32 = 1;

unsafe extern "C" {
    fn napi_get_undefined(env: NapiEnv, result: *mut NapiValue) -> NapiStatus;
    fn napi_get_boolean(env: NapiEnv, value: bool, result: *mut NapiValue) -> NapiStatus;
    fn napi_create_int32(env: NapiEnv, value: i32, result: *mut NapiValue) -> NapiStatus;
    fn napi_get_value_int32(env: NapiEnv, value: NapiValue, result: *mut i32) -> NapiStatus;
    fn napi_get_value_string_utf8(
        env: NapiEnv,
        value: NapiValue,
        buf: *mut c_char,
        bufsize: usize,
        result: *mut usize,
    ) -> NapiStatus;
    fn napi_create_arraybuffer(
        env: NapiEnv,
        byte_length: usize,
        data: *mut *mut c_void,
        result: *mut NapiValue,
    ) -> NapiStatus;
    fn napi_create_typedarray(
        env: NapiEnv,
        ty: i32,
        length: usize,
        arraybuffer: NapiValue,
        byte_offset: usize,
        result: *mut NapiValue,
    ) -> NapiStatus;
    fn napi_create_function(
        env: NapiEnv,
        utf8name: *const c_char,
        length: usize,
        cb: NapiCallback,
        data: *mut c_void,
        result: *mut NapiValue,
    ) -> NapiStatus;
    fn napi_set_named_property(
        env: NapiEnv,
        object: NapiValue,
        utf8name: *const c_char,
        value: NapiValue,
    ) -> NapiStatus;
    fn napi_get_cb_info(
        env: NapiEnv,
        cbinfo: NapiCallbackInfo,
        argc: *mut usize,
        argv: *mut NapiValue,
        this_arg: *mut NapiValue,
        data: *mut *mut c_void,
    ) -> NapiStatus;
}

// ---- 会话表（session id → 具体会话） ----

static NEXT_SESSION_ID: AtomicI32 = AtomicI32::new(1);
static VIEWERS: LazyLock<Mutex<HashMap<i32, ViewerSession>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PUBLISHERS: LazyLock<Mutex<HashMap<i32, PublisherSession>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn alloc_session_id() -> i32 {
    NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
}

// ---- NAPI 参数读取 / 值构造 ----

/// 读取回调参数并存入 `args`，返回实际参数个数（失败返回 0）。
unsafe fn read_args(env: NapiEnv, info: NapiCallbackInfo, args: &mut [NapiValue]) -> usize {
    if env.is_null() || info.is_null() {
        return 0;
    }
    let mut argc = args.len();
    let status = unsafe {
        napi_get_cb_info(
            env,
            info,
            &mut argc,
            args.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if status != NAPI_OK {
        return 0;
    }
    argc.min(args.len())
}

unsafe fn arg_string(env: NapiEnv, info: NapiCallbackInfo, idx: usize) -> Option<String> {
    let mut args = [std::ptr::null_mut(); 4];
    let argc = unsafe { read_args(env, info, &mut args) };
    if idx >= argc || args[idx].is_null() {
        return None;
    }
    let value = args[idx];
    // 先问长度，再分配并读取。
    let mut len = 0usize;
    let status =
        unsafe { napi_get_value_string_utf8(env, value, std::ptr::null_mut(), 0, &mut len) };
    if status != NAPI_OK {
        return None;
    }
    let mut buf = vec![0u8; len + 1];
    let mut written = 0usize;
    let status = unsafe {
        napi_get_value_string_utf8(env, value, buf.as_mut_ptr().cast(), buf.len(), &mut written)
    };
    if status != NAPI_OK {
        return None;
    }
    String::from_utf8(buf[..written].to_vec()).ok()
}

unsafe fn arg_i32(env: NapiEnv, info: NapiCallbackInfo, idx: usize) -> Option<i32> {
    let mut args = [std::ptr::null_mut(); 4];
    let argc = unsafe { read_args(env, info, &mut args) };
    if idx >= argc || args[idx].is_null() {
        return None;
    }
    let mut out = 0i32;
    let status = unsafe { napi_get_value_int32(env, args[idx], &mut out) };
    (status == NAPI_OK).then_some(out)
}

unsafe fn make_int32(env: NapiEnv, value: i32) -> NapiValue {
    let mut out = std::ptr::null_mut();
    let status = unsafe { napi_create_int32(env, value, &mut out) };
    if status != NAPI_OK {
        return std::ptr::null_mut();
    }
    out
}

unsafe fn make_bool(env: NapiEnv, value: bool) -> NapiValue {
    let mut out = std::ptr::null_mut();
    let status = unsafe { napi_get_boolean(env, value, &mut out) };
    if status != NAPI_OK {
        return std::ptr::null_mut();
    }
    out
}

unsafe fn make_undefined(env: NapiEnv) -> NapiValue {
    let mut out = std::ptr::null_mut();
    let status = unsafe { napi_get_undefined(env, &mut out) };
    if status != NAPI_OK {
        return std::ptr::null_mut();
    }
    out
}

/// 从 Rust 字节切片构造 `Uint8Array`（无数据时返回空数组）。
unsafe fn make_uint8_array(env: NapiEnv, bytes: &[u8]) -> NapiValue {
    let mut ab = std::ptr::null_mut();
    let mut data = std::ptr::null_mut();
    let status = unsafe { napi_create_arraybuffer(env, bytes.len(), &mut data, &mut ab) };
    if status != NAPI_OK {
        return std::ptr::null_mut();
    }
    if !bytes.is_empty() {
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast(), bytes.len()) };
    }
    let mut out = std::ptr::null_mut();
    let status =
        unsafe { napi_create_typedarray(env, NAPI_UINT8_ARRAY, bytes.len(), ab, 0, &mut out) };
    if status != NAPI_OK {
        return ab;
    }
    out
}

// ---- 注册辅助 ----

unsafe fn define_function(
    env: NapiEnv,
    exports: NapiValue,
    name: &str,
    cb: NapiCallback,
) -> NapiStatus {
    let Ok(cname) = CString::new(name) else {
        return -1;
    };
    let mut value = std::ptr::null_mut();
    let status = unsafe {
        napi_create_function(
            env,
            cname.as_ptr(),
            name.len(),
            cb,
            std::ptr::null_mut(),
            &mut value,
        )
    };
    if status != NAPI_OK {
        return status;
    }
    unsafe { napi_set_named_property(env, exports, cname.as_ptr(), value) }
}

// ---- NAPI 导出实现 ----

/// `connectViewer(server: string, room: string, token?: string): number`
unsafe extern "C" fn connect_viewer(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    let server = unsafe { arg_string(env, info, 0) }.unwrap_or_default();
    let room = unsafe { arg_string(env, info, 1) }.unwrap_or_default();
    let token = unsafe { arg_string(env, info, 2) };
    match ViewerSession::connect(&server, &room, token.as_deref()) {
        Ok(session) => {
            let id = alloc_session_id();
            VIEWERS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, session);
            unsafe { make_int32(env, id) }
        }
        Err(err) => {
            // 模拟器/CI 自测诊断：连接失败原因打到 stderr，便于 hilog 排查。
            eprintln!("ohos connectViewer error: {err}");
            unsafe { make_int32(env, 0) }
        }
    }
}

/// `takeFrame(session: number): Uint8Array`
unsafe extern "C" fn take_frame(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    let Some(id) = (unsafe { arg_i32(env, info, 0) }) else {
        return unsafe { make_uint8_array(env, &[]) };
    };
    let frame = VIEWERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .and_then(ViewerSession::take_frame)
        .unwrap_or_default();
    unsafe { make_uint8_array(env, &frame) }
}

/// `disconnect(session: number): void`
unsafe extern "C" fn disconnect(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    if let Some(id) = unsafe { arg_i32(env, info, 0) } {
        VIEWERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        PUBLISHERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }
    unsafe { make_undefined(env) }
}

/// `startPublish(server: string, room: string, token?: string): number`
unsafe extern "C" fn start_publish(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    let server = unsafe { arg_string(env, info, 0) }.unwrap_or_default();
    let room = unsafe { arg_string(env, info, 1) }.unwrap_or_default();
    let token = unsafe { arg_string(env, info, 2) };
    match PublisherSession::connect(&server, &room, token.as_deref()) {
        Ok(session) => {
            let id = alloc_session_id();
            PUBLISHERS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, session);
            unsafe { make_int32(env, id) }
        }
        Err(err) => {
            eprintln!("ohos startPublish error: {err}");
            unsafe { make_int32(env, 0) }
        }
    }
}

/// `injectInput(json: string): boolean`
///
/// 占位实现：真机路径为 `OH_Input` 注入，需要系统权限
/// (`INTERACTIVE_CONTROL` / `INTERCEPT_INPUT_EVENT`，企业签名通道)。
/// 当前无 NDK/真机，先返回 false，保证 ArkTS 接口可用。
unsafe extern "C" fn inject_input(env: NapiEnv, info: NapiCallbackInfo) -> NapiValue {
    let json = unsafe { arg_string(env, info, 0) }.unwrap_or_default();
    if json.is_empty() {
        return unsafe { make_bool(env, false) };
    }
    // TODO(P5): 解析 InputFrame JSON → OH_Input_* 注入（权限评估后）。
    eprintln!("ohos injectInput not implemented (P5): {json}");
    unsafe { make_bool(env, false) }
}

/// NAPI 模块入口：ArkTS 侧 `import libAerodeskOhos from 'libaerodesk_ohos.so'`。
///
/// # Safety
/// OHOS NAPI 运行时调用本函数时保证 `env` 与 `exports` 非空；
/// 其余输入为 C ABI 原始指针，由本函数内部做空指针检查。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_register_module_v1(env: NapiEnv, exports: NapiValue) -> NapiValue {
    if env.is_null() || exports.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        let _ = define_function(env, exports, "connectViewer", Some(connect_viewer));
        let _ = define_function(env, exports, "takeFrame", Some(take_frame));
        let _ = define_function(env, exports, "disconnect", Some(disconnect));
        let _ = define_function(env, exports, "startPublish", Some(start_publish));
        let _ = define_function(env, exports, "injectInput", Some(inject_input));
    }
    exports
}
