// 进程查找与内存读写（对应 Python 的 ProcessMemory / find_game_pids）
use std::ffi::c_void;
use std::fmt;
use std::mem::size_of;
use std::sync::Arc;

use windows::core::BOOL;
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HWND, LPARAM,
};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
use windows::Win32::System::Diagnostics::Debug::{
    ReadProcessMemory, WriteProcessMemory,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, Process32FirstW, Process32NextW,
    MODULEENTRY32W, PROCESSENTRY32W, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, VirtualQueryEx, MEMORY_BASIC_INFORMATION, MEM_COMMIT,
    MEM_MAPPED, MEM_PRIVATE, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, PAGE_GUARD,
    PAGE_PROTECTION_FLAGS, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION,
    PROCESS_VM_READ, PROCESS_VM_WRITE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
};
use windows::Win32::Foundation::GetLastError;

// ---------- 常量 ----------
pub const CHUNK: usize = 32 * 1024 * 1024; // 每次 ReadProcessMemory 的块大小
pub const MAX_HITS: usize = 500_000; // 扫描命中上限，防止结果爆炸
pub const GAME_PROCESS_NAME: &str = "Cat Quest II.exe";
pub const USER_LIMIT: usize = 0x00007FFF_FFFFFFFF;

const WRITABLE_PROTECTS: [u32; 4] = [0x04, 0x08, 0x40, 0x80];

#[derive(Debug, Clone)]
pub struct TrainerError(pub String);

impl TrainerError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

impl fmt::Display for TrainerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TrainerError {}

pub type Result<T> = std::result::Result<T, TrainerError>;

pub fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(TrainerError::new(msg))
}

pub fn last_error() -> u32 {
    unsafe { GetLastError().0 }
}

/// 对目标进程的内存读写封装
pub struct ProcessMemory {
    pub pid: u32,
    handle: HANDLE,
}

// Win32 句柄本身可跨线程使用；读写调用是线程安全的
unsafe impl Send for ProcessMemory {}
unsafe impl Sync for ProcessMemory {}

impl ProcessMemory {
    pub fn new(pid: u32) -> Result<Self> {
        let access = PROCESS_ACCESS_RIGHTS(
            PROCESS_VM_READ.0
                | PROCESS_VM_WRITE.0
                | PROCESS_VM_OPERATION.0
                | PROCESS_QUERY_INFORMATION.0,
        );
        let handle = unsafe { OpenProcess(access, false, pid) }.map_err(|_| {
            TrainerError::new(format!(
                "无法附加到进程（错误码 {}）。请确认游戏正在运行，\
                 且修改器与游戏的管理员权限一致。",
                last_error()
            ))
        })?;
        Ok(Self { pid, handle })
    }

    pub fn handle(&self) -> HANDLE {
        self.handle
    }

    /// 遍历虚拟内存，返回可写、已提交、非 PAGE_GUARD 的区域。
    /// 跳过 MEM_IMAGE（exe/dll 映像，一般存放代码和常量），
    /// 只扫 MEM_PRIVATE / MEM_MAPPED —— Mono 的托管堆就在这里。
    pub fn writable_regions(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut addr: usize = 0;
        while addr < USER_LIMIT {
            let mut mbi = MEMORY_BASIC_INFORMATION::default();
            let n = unsafe {
                VirtualQueryEx(
                    self.handle,
                    Some(addr as *const c_void),
                    &mut mbi,
                    size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            };
            if n == 0 {
                break;
            }
            let base = mbi.BaseAddress as usize;
            let size = mbi.RegionSize;
            let prot = mbi.Protect.0;
            if mbi.State == MEM_COMMIT
                && WRITABLE_PROTECTS.contains(&prot)
                && (prot & PAGE_GUARD.0) == 0
                && (mbi.Type == MEM_PRIVATE || mbi.Type == MEM_MAPPED)
                && size > 0
            {
                out.push((base, size));
            }
            let next = base.saturating_add(size);
            if next <= addr {
                break; // 防死循环
            }
            addr = next;
        }
        out
    }

    pub fn read(&self, addr: usize, size: usize) -> Result<Vec<u8>> {
        if size == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; size];
        let mut got = 0usize;
        unsafe {
            ReadProcessMemory(
                self.handle,
                addr as *const c_void,
                buf.as_mut_ptr() as *mut c_void,
                size,
                Some(&mut got),
            )
        }
        .map_err(|_| {
            TrainerError::new(format!(
                "ReadProcessMemory 失败 @ 0x{:X}（错误码 {}）",
                addr,
                last_error()
            ))
        })?;
        buf.truncate(got);
        Ok(buf)
    }

    pub fn write(&self, addr: usize, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let mut got = 0usize;
        unsafe {
            WriteProcessMemory(
                self.handle,
                addr as *const c_void,
                data.as_ptr() as *const c_void,
                data.len(),
                Some(&mut got),
            )
        }
        .map_err(|_| {
            TrainerError::new(format!(
                "WriteProcessMemory 失败 @ 0x{:X}（错误码 {}）",
                addr,
                last_error()
            ))
        })?;
        if got != data.len() {
            return err(format!(
                "WriteProcessMemory 写入不完整 @ 0x{:X}（{}/{}）",
                addr,
                got,
                data.len()
            ));
        }
        Ok(())
    }

    pub fn read_u64(&self, addr: usize) -> Result<u64> {
        let b = self.read(addr, 8)?;
        if b.len() < 8 {
            return err(format!("读取 8 字节失败 @ 0x{:X}", addr));
        }
        Ok(u64::from_le_bytes(b[..8].try_into().unwrap()))
    }

    pub fn read_i32(&self, addr: usize) -> Result<i32> {
        let b = self.read(addr, 4)?;
        if b.len() < 4 {
            return err(format!("读取 4 字节失败 @ 0x{:X}", addr));
        }
        Ok(i32::from_le_bytes(b[..4].try_into().unwrap()))
    }

    pub fn read_f32(&self, addr: usize) -> Result<f32> {
        let b = self.read(addr, 4)?;
        if b.len() < 4 {
            return err(format!("读取 4 字节失败 @ 0x{:X}", addr));
        }
        Ok(f32::from_le_bytes(b[..4].try_into().unwrap()))
    }

    pub fn write_i32(&self, addr: usize, v: i32) -> Result<()> {
        self.write(addr, &v.to_le_bytes())
    }

    pub fn write_u64(&self, addr: usize, v: u64) -> Result<()> {
        self.write(addr, &v.to_le_bytes())
    }

    pub fn write_f32(&self, addr: usize, v: f32) -> Result<()> {
        self.write(addr, &v.to_le_bytes())
    }

    /// 在目标进程分配内存；固定地址分配失败返回 None
    pub fn alloc_ex(&self, address: Option<usize>, size: usize, exec: bool) -> Option<usize> {
        let prot = if exec {
            PAGE_EXECUTE_READWRITE
        } else {
            PAGE_READWRITE
        };
        let p = unsafe {
            VirtualAllocEx(
                self.handle,
                address.map(|a| a as *const c_void),
                size,
                MEM_COMMIT | MEM_RESERVE,
                prot,
            )
        };
        if p.is_null() {
            None
        } else {
            Some(p as usize)
        }
    }

    pub fn free_ex(&self, address: usize) {
        unsafe {
            let _ = VirtualFreeEx(self.handle, address as *mut c_void, 0, MEM_RELEASE);
        }
    }
}

impl Drop for ProcessMemory {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// 取目标进程里指定模块的加载基址（Toolhelp32 快照）
pub fn remote_module_base(pid: u32, name: &str) -> Result<Option<usize>> {
    let flags = windows::Win32::System::Diagnostics::ToolHelp::CREATE_TOOLHELP_SNAPSHOT_FLAGS(
        TH32CS_SNAPMODULE.0 | TH32CS_SNAPMODULE32.0,
    );
    let snap = unsafe { CreateToolhelp32Snapshot(flags, pid) }
        .map_err(|_| TrainerError::new(format!("读取游戏模块列表失败（错误码 {}）", last_error())))?;
    let mut entry = MODULEENTRY32W {
        dwSize: size_of::<MODULEENTRY32W>() as u32,
        ..Default::default()
    };
    let mut result = None;
    let mut ok = unsafe { Module32FirstW(snap, &mut entry) }.is_ok();
    while ok {
        let module = utf16_to_string(&entry.szModule);
        if module.eq_ignore_ascii_case(name) {
            result = Some(entry.modBaseAddr as usize);
            break;
        }
        ok = unsafe { Module32NextW(snap, &mut entry) }.is_ok();
    }
    unsafe {
        let _ = CloseHandle(snap);
    }
    Ok(result)
}

fn utf16_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

unsafe extern "system" fn enum_window_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let set = unsafe { &mut *(lparam.0 as *mut Vec<u32>) };
    if unsafe { IsWindowVisible(hwnd) }.as_bool() {
        let mut pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        set.push(pid);
    }
    BOOL(1)
}

/// 枚举所有有可见窗口的进程 PID（用于剔除崩溃残留的无窗口僵尸进程）
pub fn windowed_pids() -> Vec<u32> {
    let mut pids: Vec<u32> = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(enum_window_proc),
            LPARAM(&mut pids as *mut Vec<u32> as isize),
        );
    }
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// 按进程名找游戏 PID。同名进程可能有多个（崩溃后残留的僵尸进程），
/// 有可见窗口的排在前面，保证附加到真正的游戏窗口进程。
pub fn find_game_pids() -> Vec<u32> {
    let mut pids = Vec::new();
    let snap = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        Ok(h) => h,
        Err(_) => return pids,
    };
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut ok = unsafe { Process32FirstW(snap, &mut entry) }.is_ok();
    while ok {
        let name = utf16_to_string(&entry.szExeFile);
        if name.eq_ignore_ascii_case(GAME_PROCESS_NAME) {
            pids.push(entry.th32ProcessID);
        }
        ok = unsafe { Process32NextW(snap, &mut entry) }.is_ok();
    }
    unsafe {
        let _ = CloseHandle(snap);
    }
    let win = windowed_pids();
    // 有窗口的排在前面（Python 用 0/1 作为排序键，排序是稳定的）
    pids.sort_by_key(|p| if win.contains(p) { 0 } else { 1 });
    pids
}

/// 只取带可见窗口的游戏 PID
pub fn windowed_game_pids() -> Vec<u32> {
    let win = windowed_pids();
    find_game_pids()
        .into_iter()
        .filter(|p| win.contains(p))
        .collect()
}

/// Windows 11：把整个窗口（含标题栏）改成圆角
pub fn round_window(title: &str) {
    let my_pid = std::process::id();
    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = unsafe { &mut *(lparam.0 as *mut (u32, &str, Vec<HWND>)) };
        let mut pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        if pid == ctx.0 && unsafe { IsWindowVisible(hwnd) }.as_bool() {
            let len = unsafe {
                windows::Win32::UI::WindowsAndMessaging::GetWindowTextLengthW(hwnd)
            };
            if len > 0 {
                let mut buf = vec![0u16; (len + 1) as usize];
                let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
                if n > 0 && String::from_utf16_lossy(&buf[..n as usize]) == ctx.1 {
                    ctx.2.push(hwnd);
                }
            }
        }
        BOOL(1)
    }
    let mut ctx = (my_pid, title, Vec::<HWND>::new());
    unsafe {
        let _ = EnumWindows(
            Some(cb),
            LPARAM(&mut ctx as *mut (u32, &str, Vec<HWND>) as isize),
        );
    }
    let targets = std::mem::take(&mut ctx.2);
    let pref: i32 = 2; // DWMWCP_ROUND
    for hwnd in targets {
        unsafe {
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWINDOWATTRIBUTE(33),
                &pref as *const i32 as *const c_void,
                size_of::<i32>() as u32,
            );
        }
    }
}

/// 常用：把进程对象包成 Arc 方便后台线程共享
pub type SharedProcess = Arc<ProcessMemory>;

/// 保护属性是否属于可写集合（供单元测试）
pub fn is_writable_protect(prot: u32) -> bool {
    WRITABLE_PROTECTS.contains(&prot) && (prot & PAGE_GUARD.0) == 0
}

/// PAGE_PROTECTION_FLAGS 的辅助构造（部分调用处需要）
pub fn page_flags(v: u32) -> PAGE_PROTECTION_FLAGS {
    PAGE_PROTECTION_FLAGS(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writable_protect_filter() {
        assert!(is_writable_protect(0x04));
        assert!(is_writable_protect(0x40));
        assert!(!is_writable_protect(0x02)); // PAGE_READONLY
        assert!(!is_writable_protect(0x104)); // READWRITE | PAGE_GUARD
    }

    #[test]
    fn utf16_name_parsing() {
        let mut buf = [0u16; 32];
        for (i, c) in "Cat Quest II.exe".encode_utf16().enumerate() {
            buf[i] = c;
        }
        assert_eq!(utf16_to_string(&buf), "Cat Quest II.exe");
    }
}
