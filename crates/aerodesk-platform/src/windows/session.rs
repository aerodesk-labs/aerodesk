//! #470 M3：WTS 会话查询 + 用户会话进程拉起。
//! 仅 SYSTEM 服务进程可拉起用户会话进程（`WTSQueryUserToken` 需 SE_TCB）；
//! 非服务上下文调用返回显式错误（detect-and-return，禁 skip 凑绿）。

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{
    WTS_CONNECTSTATE_CLASS, WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW, WTSActive, WTSConnected,
    WTSDisconnected, WTSDown, WTSEnumerateSessionsW, WTSFreeMemory, WTSIdle, WTSQueryUserToken,
};
use windows::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::core::{PCWSTR, PWSTR};

/// 单条 WTS 会话信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: u32,
    /// WTS_ACTIVE：已登录且连接到输入设备。
    pub active: bool,
    /// 存在已登录用户（Active=在用 / Connected=锁屏 / Disconnected=断开保留，
    /// 如 fast user switching、RDP 断开）——会话内进程仍在运行。
    /// 区别于 `active`：登录界面阶段所有会话均无 logged-in 用户。
    pub logged_in: bool,
}

/// 状态枚举值归一（便于单测映射，不直接依赖生成类型的比较行为）。
fn state_is_active(state: WTS_CONNECTSTATE_CLASS) -> bool {
    state == WTSActive
}

/// 是否有已登录用户（WTSActive/WTSConnected/WTSDisconnected 三态都意味着
/// 用户已登录、会话内进程存活;仅锁屏≠断开登录)。
fn state_is_logged_in(state: WTS_CONNECTSTATE_CLASS) -> bool {
    state == WTSActive || state == WTSConnected || state == WTSDisconnected
}

/// 枚举本机全部 WTS 会话。
pub fn enumerate() -> Result<Vec<SessionInfo>, String> {
    unsafe {
        let mut ptr: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
        let mut count: u32 = 0;
        WTSEnumerateSessionsW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut ptr, &mut count)
            .map_err(|e| format!("WTSEnumerateSessionsW: {e}"))?;
        let mut out = Vec::with_capacity(count as usize);
        if !ptr.is_null() {
            for i in 0..count as usize {
                let s = *ptr.add(i);
                out.push(SessionInfo {
                    id: s.SessionId,
                    active: state_is_active(s.State),
                    logged_in: state_is_logged_in(s.State),
                });
            }
            WTSFreeMemory(ptr.cast());
        }
        Ok(out)
    }
}

/// 活动（已登录且在用）会话 id;无则 `None`。
pub fn active_session() -> Option<u32> {
    enumerate()
        .ok()?
        .into_iter()
        .find(|s| s.active)
        .map(|s| s.id)
}

/// 任一已登录用户会话 id（含锁屏 Connected / 断开 Disconnected——会话内
/// 进程仍存活，desktop 自带 presence 在线）。登录界面阶段（无人登录过/
/// 已注销）返回 `None`。#470 让位状态机的初始判据必须用它而非
/// `active_session`：锁屏不是"无会话"，否则服务与会话内 desktop 双 presence。
pub fn logged_in_session() -> Option<u32> {
    enumerate()
        .ok()?
        .into_iter()
        .find(|s| s.logged_in && s.id != 0)
        .map(|s| s.id)
}

/// 在指定用户会话内以该用户身份拉起 GUI 进程（SYSTEM 服务专用，M3）。
///
/// `exe`：目标程序绝对路径；desktop 固定 `winsta0\default`（用户桌面站），
/// 环境块按目标用户展开（`CREATE_UNICODE_ENVIRONMENT`）。
pub fn spawn_in_session(exe: &str, session_id: u32) -> Result<(), String> {
    unsafe {
        let mut token = HANDLE::default();
        WTSQueryUserToken(session_id, &mut token).map_err(|e| {
            format!(
                "WTSQueryUserToken 失败（需 SYSTEM 服务 SE_TCB 上下文，session={session_id}）：{e}"
            )
        })?;
        let result = spawn_with_token(exe, token);
        let _ = CloseHandle(token);
        result
    }
}

fn spawn_with_token(exe: &str, token: HANDLE) -> Result<(), String> {
    unsafe {
        let mut env: *mut core::ffi::c_void = std::ptr::null_mut();
        let env_ok = CreateEnvironmentBlock(&mut env, token, false).is_ok();
        let mut desktop: Vec<u16> = "winsta0\\default"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let exe_w: Vec<u16> = exe.encode_utf16().chain(std::iter::once(0)).collect();
        let mut si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        let spawned = CreateProcessAsUserW(
            token,
            PCWSTR(exe_w.as_ptr()),
            PWSTR::null(),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT,
            if env_ok { Some(env.cast_const()) } else { None },
            None,
            &raw mut si,
            &raw mut pi,
        );
        if env_ok {
            let _ = DestroyEnvironmentBlock(env);
        }
        spawned.map_err(|e| format!("CreateProcessAsUserW({exe}): {e}"))?;
        let _ = CloseHandle(pi.hProcess);
        let _ = CloseHandle(pi.hThread);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 让位判据:Active/Connected/Disconnected 均算"有已登录会话";
    /// 登录界面阶段(Idle/Down 等)不算。锁屏误判会导致双 presence。
    #[test]
    fn logged_in_state_mapping() {
        assert!(state_is_logged_in(WTSActive));
        assert!(
            state_is_logged_in(WTSConnected),
            "锁屏=Connected,须算已登录"
        );
        assert!(state_is_logged_in(WTSDisconnected));
        assert!(!state_is_logged_in(WTSIdle));
        assert!(!state_is_logged_in(WTSDown));
    }

    /// 任意上下文可枚举（登录界面/服务态/用户态均返回列表，至少含 session 0）。
    #[test]
    fn enumerate_returns_sessions() {
        let sessions = enumerate().expect("WTSEnumerateSessionsW 应可用");
        assert!(sessions.iter().any(|s| s.id == 0), "应包含服务会话 0");
    }

    /// 非 SYSTEM 上下文调用 spawn：detect-and-return 显式报错而非 panic/skip。
    /// SYSTEM 服务内运行本测试也会通过（session 0xF…BADA 无此会话，仍失败）。
    #[test]
    fn spawn_without_tcb_returns_friendly_error() {
        let err = spawn_in_session(r"C:\nonexistent\aerodesk-desktop.exe", 0xFFFF_BADA)
            .expect_err("无 SE_TCB 上下文应失败");
        assert!(err.contains("SE_TCB"), "错误应含上下文提示：{err}");
    }
}
