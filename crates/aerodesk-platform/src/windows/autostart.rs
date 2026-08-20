//! Windows 被控端开机自启（#3）：`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`。
//!
//! 远程桌面被控端应能随用户登录自动启动；用 HKCU（当前用户）无需管理员权限，
//! 原生 Win32 `RegOpenKeyExW/RegSetValueExW/RegDeleteValueW/RegQueryValueExW`，
//! 无 PowerShell/reg.exe 子进程。

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "AeroDesk";
const HKEY_CURRENT_USER: isize = -2_147_483_647isize; // HKEY_CURRENT_USER
const KEY_QUERY_VALUE: u32 = 0x0001;
const KEY_SET_VALUE: u32 = 0x0002;
const REG_SZ: u32 = 1;
const ERROR_SUCCESS: i32 = 0;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_MORE_DATA: i32 = 234;

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegOpenKeyExW(
        key: isize,
        subkey: *const u16,
        options: u32,
        access: u32,
        out: *mut isize,
    ) -> i32;
    fn RegSetValueExW(
        key: isize,
        name: *const u16,
        reserved: u32,
        typ: u32,
        data: *const u8,
        size: u32,
    ) -> i32;
    fn RegDeleteValueW(key: isize, name: *const u16) -> i32;
    fn RegQueryValueExW(
        key: isize,
        name: *const u16,
        reserved: *mut u32,
        typ: *mut u32,
        data: *mut u8,
        size: *mut u32,
    ) -> i32;
    fn RegCloseKey(key: isize) -> i32;
}

fn to_utf16(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

/// 组装 Run 值命令行：`"<exe>" --role publisher --encoder screen --signal <signal> --room <room>`。
pub fn autostart_command(exe: &str, signal: &str, room: &str) -> String {
    format!("\"{exe}\" --role publisher --encoder screen --signal {signal} --room {room}")
}

fn open_run(write: bool) -> Result<isize, String> {
    let key = to_utf16(RUN_KEY);
    let mut hkey: isize = 0;
    // SAFETY: hkey 指向栈上句柄；RegOpenKeyExW 成功时写入，失败时保持 0。
    let ret = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            0,
            if write {
                KEY_SET_VALUE
            } else {
                KEY_QUERY_VALUE
            },
            &mut hkey,
        )
    };
    if ret != ERROR_SUCCESS {
        return Err(format!("RegOpenKeyExW(Run): {ret}"));
    }
    Ok(hkey)
}

/// 注册开机自启（覆盖同名值）。
pub fn install(command_line: &str) -> Result<(), String> {
    let hkey = open_run(true)?;
    let name = to_utf16(VALUE_NAME);
    let value = to_utf16(command_line);
    // SAFETY: value 为 NUL 结尾 UTF-16，RegSetValueExW 按字节拷贝（含 NUL）。
    let ret = unsafe {
        RegSetValueExW(
            hkey,
            name.as_ptr(),
            0,
            REG_SZ,
            value.as_ptr() as *const u8,
            (value.len() * 2) as u32,
        )
    };
    // SAFETY: 句柄由 open_run 打开。
    unsafe { RegCloseKey(hkey) };
    if ret != ERROR_SUCCESS {
        return Err(format!("RegSetValueExW: {ret}"));
    }
    Ok(())
}

/// 移除开机自启；返回是否曾存在。
pub fn remove() -> Result<bool, String> {
    let hkey = open_run(true)?;
    let name = to_utf16(VALUE_NAME);
    // SAFETY: 句柄有效；RegDeleteValueW 对不存在的值返回 FILE_NOT_FOUND。
    let ret = unsafe { RegDeleteValueW(hkey, name.as_ptr()) };
    unsafe { RegCloseKey(hkey) };
    match ret {
        ERROR_SUCCESS => Ok(true),
        ERROR_FILE_NOT_FOUND => Ok(false),
        r => Err(format!("RegDeleteValueW: {r}")),
    }
}

/// 查询当前自启命令行；未安装返回 None。
pub fn installed() -> Result<Option<String>, String> {
    let hkey = open_run(false)?;
    let name = to_utf16(VALUE_NAME);
    // 先查大小（含 NUL）。
    let mut size: u32 = 0;
    // SAFETY: 句柄有效；size=0 首次调用会返回 MORE_DATA 并给出所需大小（含 NUL）。
    let mut ret = unsafe {
        RegQueryValueExW(
            hkey,
            name.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if ret == ERROR_FILE_NOT_FOUND {
        unsafe { RegCloseKey(hkey) };
        return Ok(None);
    }
    if ret != ERROR_SUCCESS && ret != ERROR_MORE_DATA {
        unsafe { RegCloseKey(hkey) };
        return Err(format!("RegQueryValueExW(size): {ret}"));
    }
    if size == 0 {
        unsafe { RegCloseKey(hkey) };
        return Ok(None);
    }
    let mut buf = vec![0u8; size as usize];
    let mut typ: u32 = 0;
    // SAFETY: buf 容量按 size 分配；REG_SZ 返回 UTF-16（含 NUL）字节。
    ret = unsafe {
        RegQueryValueExW(
            hkey,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut typ,
            buf.as_mut_ptr(),
            &mut size,
        )
    };
    unsafe { RegCloseKey(hkey) };
    if ret != ERROR_SUCCESS {
        return Err(format!("RegQueryValueExW: {ret}"));
    }
    // 按 UTF-16 解码（去掉尾部 NUL）。
    let u16len = (size as usize / 2).saturating_sub(1);
    let mut units = Vec::with_capacity(u16len);
    for i in 0..u16len {
        units.push(u16::from_le_bytes([buf[i * 2], buf[i * 2 + 1]]));
    }
    Ok(Some(String::from_utf16_lossy(&units)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_command_quotes_exe() {
        let cmd = autostart_command(
            r"C:\Program Files\AeroDesk\aerodesk-agent.exe",
            "ws://127.0.0.1:3003/ws",
            "demo",
        );
        assert!(cmd.starts_with(
            r#""C:\Program Files\AeroDesk\aerodesk-agent.exe" --role publisher --encoder screen"#
        ));
        assert!(cmd.contains("--signal ws://127.0.0.1:3003/ws"));
        assert!(cmd.contains("--room demo"));
    }

    #[test]
    fn utf16_roundtrip() {
        let v = to_utf16("AeroDesk 你好 🚀");
        assert_eq!(v.last(), Some(&0));
        let units = &v[..v.len() - 1];
        assert_eq!(String::from_utf16_lossy(units), "AeroDesk 你好 🚀");
    }
}
