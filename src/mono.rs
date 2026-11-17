// Mono 运行时：模块 RVA、远程线程调用（x64 手写机器码 stub）、Mono API 封装
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::Arc;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{CreateRemoteThread, WaitForSingleObject};

use crate::memory::{
    err, last_error, remote_module_base, ProcessMemory, Result, TrainerError,
};
use crate::scan::resolve_class_name;

pub const MONO_DLL: &str = "mono-2.0-bdwgc.dll";

/// mono-2.0-bdwgc.dll 导出 RVA（已用 pefile 对照导出表逐个核对过）
pub const MONO_RVAS: [(&str, usize); 11] = [
    ("mono_thread_attach", 0x1EE030),
    ("mono_thread_detach", 0x1EE440),
    ("mono_get_root_domain", 0xA4C30),
    ("mono_image_loaded", 0x141A50),
    ("mono_class_from_name", 0xBD050),
    ("mono_class_get_method_from_name", 0xC10C0),
    ("mono_class_vtable", 0x1BC270),
    ("mono_runtime_invoke", 0x1BEBD0),
    ("mono_object_get_class", 0x1C9AD0),
    ("mono_class_get_name", 0xBFD40),
    ("mono_class_get_parent", 0xBFBF0),
];

#[derive(Clone, Copy, Debug)]
pub struct MonoFunctions {
    pub thread_attach: usize,
    pub thread_detach: usize,
    pub get_root_domain: usize,
    pub image_loaded: usize,
    pub class_from_name: usize,
    pub class_get_method_from_name: usize,
    pub class_vtable: usize,
    pub runtime_invoke: usize,
    pub object_get_class: usize,
    pub class_get_name: usize,
    pub class_get_parent: usize,
}

impl MonoFunctions {
    pub fn from_base(base: usize) -> Self {
        let get = |k: &str| {
            let (_, rva) = MONO_RVAS.iter().find(|(n, _)| *n == k).unwrap();
            base + *rva
        };
        Self {
            thread_attach: get("mono_thread_attach"),
            thread_detach: get("mono_thread_detach"),
            get_root_domain: get("mono_get_root_domain"),
            image_loaded: get("mono_image_loaded"),
            class_from_name: get("mono_class_from_name"),
            class_get_method_from_name: get("mono_class_get_method_from_name"),
            class_vtable: get("mono_class_vtable"),
            runtime_invoke: get("mono_runtime_invoke"),
            object_get_class: get("mono_object_get_class"),
            class_get_name: get("mono_class_get_name"),
            class_get_parent: get("mono_class_get_parent"),
        }
    }
}

/// 远程调用参数：立即数 / 读远程地址的值 / 上一步返回值槽位
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Arg {
    Imm(u64),
    Read(u64),
    Slot(usize),
}

// 远程调用的寄存器编码（x64：参数走 rcx/rdx/r8/r9，调用目标用 r10）
const REG_MOV: [&[u8]; 5] = [
    &[0x48, 0xB9],
    &[0x48, 0xBA],
    &[0x49, 0xB8],
    &[0x49, 0xB9],
    &[0x49, 0xBA],
];
const REG_CALL: [&[u8]; 5] = [
    &[0xFF, 0xD1],
    &[0xFF, 0xD2],
    &[0x41, 0xFF, 0xD0],
    &[0x41, 0xFF, 0xD1],
    &[0x41, 0xFF, 0xD2],
];
const MOV_REG_FROM_RAX: [&[u8]; 4] = [
    &[0x48, 0x89, 0xC1],
    &[0x48, 0x89, 0xC2],
    &[0x49, 0x89, 0xC0],
    &[0x49, 0x89, 0xC1],
];

/// 生成远程调用 stub。
/// x64 约定：入口 rsp≡8(mod16)，每次 call 前 sub rsp,0x28 保证对齐与
/// shadow space；每步返回值立即写槽，跨 call 不依赖易失寄存器。
pub fn build_stub(calls: &[(u64, Vec<Arg>)], slots: u64) -> Vec<u8> {
    let mut asm = Vec::new();
    asm.extend_from_slice(&[0x48, 0x83, 0xEC, 0x28]); // sub rsp,0x28
    for (i, (func, args)) in calls.iter().enumerate() {
        for (k, arg) in args.iter().take(4).enumerate() {
            match arg {
                Arg::Imm(v) => {
                    asm.extend_from_slice(REG_MOV[k]);
                    asm.extend_from_slice(&v.to_le_bytes());
                }
                Arg::Read(src) => {
                    asm.extend_from_slice(&[0x48, 0xA1]); // mov rax,[imm64]
                    asm.extend_from_slice(&src.to_le_bytes());
                    asm.extend_from_slice(MOV_REG_FROM_RAX[k]);
                }
                Arg::Slot(slot) => {
                    let src = slots + 8 * (*slot as u64);
                    asm.extend_from_slice(&[0x48, 0xA1]);
                    asm.extend_from_slice(&src.to_le_bytes());
                    asm.extend_from_slice(MOV_REG_FROM_RAX[k]);
                }
            }
        }
        asm.extend_from_slice(REG_MOV[4]);
        asm.extend_from_slice(&func.to_le_bytes());
        asm.extend_from_slice(REG_CALL[4]);
        asm.extend_from_slice(&[0x48, 0xA3]); // mov [imm64],rax
        asm.extend_from_slice(&(slots + 8 * i as u64).to_le_bytes());
    }
    asm.extend_from_slice(&[0x48, 0x83, 0xC4, 0x28]); // add rsp,0x28
    asm.push(0xC3); // ret
    asm
}

/// 在游戏进程里按序执行若干 Mono API 调用。
pub struct RemoteCall {
    pub pm: Arc<ProcessMemory>,
    mems: Vec<usize>,
}

impl RemoteCall {
    pub fn new(pm: Arc<ProcessMemory>) -> Self {
        Self {
            pm,
            mems: Vec::new(),
        }
    }

    pub fn alloc(&mut self, size: usize, exec_page: bool) -> Result<usize> {
        let p = self.pm.alloc_ex(None, size, exec_page).ok_or_else(|| {
            TrainerError::new(format!("分配游戏内存失败（错误码 {}）", last_error()))
        })?;
        self.mems.push(p);
        Ok(p)
    }

    pub fn register(&mut self, p: usize) {
        self.mems.push(p);
    }

    pub fn free(&mut self, p: usize) {
        self.pm.free_ex(p);
        self.mems.retain(|&m| m != p);
    }

    pub fn free_all(&mut self) {
        for p in self.mems.drain(..).collect::<Vec<_>>() {
            self.pm.free_ex(p);
        }
    }

    pub fn write_str(&mut self, s: &str) -> Result<usize> {
        let mut data = s.as_bytes().to_vec();
        data.push(0);
        let size = data.len().max(8);
        let addr = self.alloc(size, false)?;
        // 缓冲区尾部可能不足 8 字节，补齐后再写，避免误伤相邻分配
        let mut padded = data;
        padded.resize(size, 0);
        self.pm.write(addr, &padded)?;
        Ok(addr)
    }

    /// calls: [(函数地址, [arg0..arg3]), ...] -> 每步 rax
    pub fn run_sequence(
        &mut self,
        calls: &[(u64, Vec<Arg>)],
        timeout_ms: u32,
    ) -> Result<Vec<u64>> {
        let n = calls.len();
        let slots = self.alloc(8 * n + 64, false)?;
        let asm = build_stub(calls, slots as u64);
        let code = self.alloc(asm.len(), true)?;
        self.pm.write(code, &asm)?;
        self.pm.write(slots, &vec![0u8; 8 * n])?;

        let mut handle = None;
        for attempt in 0..5 {
            let start: unsafe extern "system" fn(*mut c_void) -> u32 =
                unsafe { std::mem::transmute(code as *const c_void) };
            match unsafe {
                CreateRemoteThread(
                    self.pm.handle(),
                    None,
                    0,
                    Some(start),
                    None,
                    0,
                    None,
                )
            } {
                Ok(h) => {
                    handle = Some(h);
                    break;
                }
                Err(_) => {
                    let e = last_error();
                    if e == 5 && attempt < 4 {
                        // 进程繁忙时偶发拒绝，稍候重试
                        std::thread::sleep(Duration::from_millis(200));
                        continue;
                    }
                    return err(format!("在游戏内执行代码失败（错误码 {}）", e));
                }
            }
        }
        let h = handle.ok_or_else(|| TrainerError::new("在游戏内执行代码失败"))?;
        let wait = unsafe { WaitForSingleObject(h, timeout_ms) };
        unsafe {
            let _ = CloseHandle(h);
        }
        if wait == WAIT_TIMEOUT {
            return err("游戏内执行超时");
        }
        let raw = self.pm.read(slots, 8 * n)?;
        if raw.len() < 8 * n {
            return err("读取远程调用结果失败");
        }
        let results = (0..n)
            .map(|i| u64::from_le_bytes(raw[8 * i..8 * i + 8].try_into().unwrap()))
            .collect();
        // 线程已结束，临时代码/槽位即刻回收，防止长驻巡逻把游戏内存写爆
        self.free(code);
        self.free(slots);
        Ok(results)
    }
}

impl Drop for RemoteCall {
    fn drop(&mut self) {
        self.free_all();
    }
}

/// 一条已经搭好的 Mono 会话（域/镜像/异常槽 + 常用 API）
pub struct MonoApi {
    pub pm: Arc<ProcessMemory>,
    pub rc: RemoteCall,
    pub f: MonoFunctions,
    pub domain: u64,
    pub image: u64,
    pub exc_buf: usize,
}

impl MonoApi {
    /// 打开会话：取 mono 模块基址，attach 线程并加载 Assembly-CSharp。
    pub fn open(pm: Arc<ProcessMemory>) -> Result<Self> {
        let base = remote_module_base(pm.pid, MONO_DLL)?
            .ok_or_else(|| TrainerError::new("取不到游戏脚本运行时（游戏可能已退出）"))?;
        let f = MonoFunctions::from_base(base);
        let mut rc = RemoteCall::new(pm.clone());
        let s_asm = rc.write_str("Assembly-CSharp")?;
        let exc_buf = rc.alloc(8, false)?;
        pm.write(exc_buf, &[0u8; 8])?;
        let r = rc.run_sequence(
            &[
                (f.get_root_domain as u64, vec![]),
                (f.thread_attach as u64, vec![Arg::Slot(0)]),
                (f.image_loaded as u64, vec![Arg::Imm(s_asm as u64)]),
                (f.thread_detach as u64, vec![Arg::Slot(1)]),
            ],
            15000,
        )?;
        let domain = r[0];
        let image = r[2];
        if domain == 0 || image == 0 {
            return err("游戏脚本尚未加载完成，请进入游戏画面后再使用");
        }
        Ok(Self {
            pm,
            rc,
            f,
            domain,
            image,
            exc_buf,
        })
    }

    /// attach -> steps -> detach；返回各步（含 attach）返回值
    pub fn seq(&mut self, steps: &[(u64, Vec<Arg>)]) -> Result<Vec<u64>> {
        self.seq_timeout(steps, 20000)
    }

    pub fn seq_timeout(
        &mut self,
        steps: &[(u64, Vec<Arg>)],
        timeout_ms: u32,
    ) -> Result<Vec<u64>> {
        let mut calls = Vec::with_capacity(steps.len() + 2);
        calls.push((self.f.thread_attach as u64, vec![Arg::Imm(self.domain)]));
        calls.extend_from_slice(steps);
        calls.push((self.f.thread_detach as u64, vec![Arg::Slot(0)]));
        self.rc.run_sequence(&calls, timeout_ms)
    }

    pub fn clear_exc(&mut self) -> Result<()> {
        self.pm.write(self.exc_buf, &[0u8; 8])
    }

    /// 读异常槽：正常返回空串，异常返回异常类名并清槽
    pub fn exc_name(&mut self) -> String {
        let exc = match self.pm.read_u64(self.exc_buf) {
            Ok(v) => v,
            Err(_) => return String::new(),
        };
        if exc == 0 {
            return String::new();
        }
        let _ = self.clear_exc();
        let Ok(vt) = self.pm.read_u64(exc as usize) else {
            return "未知异常".to_string();
        };
        resolve_class_name(&self.pm, vt as usize).unwrap_or_else(|| "未知异常".to_string())
    }

    /// 调 mono_runtime_invoke(m, this, arg, exc)，返回返回值，不检查异常
    pub fn invoke(&mut self, m: u64, this: u64, arg: u64) -> Result<u64> {
        self.clear_exc()?;
        let f_invoke = self.f.runtime_invoke as u64;
        let exc = self.exc_buf as u64;
        let r = self.seq(&[(
            f_invoke,
            vec![Arg::Imm(m), Arg::Imm(this), Arg::Imm(arg), Arg::Imm(exc)],
        )])?;
        Ok(r[1])
    }

    /// 调 mono_runtime_invoke 并检查异常
    pub fn invoke_checked(&mut self, m: u64, this: u64, arg: u64) -> Result<(u64, String)> {
        let ret = self.invoke(m, this, arg)?;
        Ok((ret, self.exc_name()))
    }

    /// 本类找不到就沿父类链找（GetEntities 定义在 Entitas 泛型基类上）
    pub fn find_method(&mut self, mut k: u64, name: usize, argc: i32) -> Result<u64> {
        let f_get = self.f.class_get_method_from_name as u64;
        let f_parent = self.f.class_get_parent as u64;
        for _ in 0..6 {
            if k == 0 {
                return Ok(0);
            }
            let r = self.seq(&[(
                f_get,
                vec![Arg::Imm(k), Arg::Imm(name as u64), Arg::Imm(argc as u64)],
            )])?;
            if r[1] != 0 {
                return Ok(r[1]);
            }
            let r = self.seq(&[(f_parent, vec![Arg::Imm(k)])])?;
            k = r[1];
        }
        Ok(0)
    }

    pub fn object_class(&mut self, obj: u64) -> Result<u64> {
        let f = self.f.object_get_class as u64;
        let r = self.seq(&[(f, vec![Arg::Imm(obj)])])?;
        Ok(r[1])
    }

    /// bool 返回可能是装箱对象（真值在 +0x10）或裸 0/1，自动区分
    pub fn is_true(&self, v: u64) -> bool {
        if v >= 0x1_0000_0000 && v % 8 == 0 {
            self.pm.read(v as usize + 0x10, 1).map(|b| b[0] == 1).unwrap_or(false)
        } else {
            (v & 0xFF) == 1
        }
    }

    /// int 返回同样可能是装箱 Int32（值在 +0x10）或裸值
    pub fn as_i32(&self, ret: u64) -> i32 {
        if ret >= 0x1_0000_0000 && ret % 8 == 0 {
            self.pm.read_i32(ret as usize + 0x10).unwrap_or(0)
        } else {
            ret as u32 as i32
        }
    }
}

impl Drop for MonoApi {
    fn drop(&mut self) {
        self.rc.free_all();
    }
}

pub fn read_u64(pm: &ProcessMemory, addr: usize) -> Result<u64> {
    pm.read_u64(addr)
}

pub fn read_i32(pm: &ProcessMemory, addr: usize) -> Result<i32> {
    pm.read_i32(addr)
}

pub fn pointer_size() -> usize {
    size_of::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_matches_python_layout() {
        let calls = vec![(0x1000u64, vec![Arg::Imm(1)])];
        let bytes = build_stub(&calls, 0x2000);
        let mut expected: Vec<u8> = vec![0x48, 0x83, 0xEC, 0x28];
        expected.extend_from_slice(&[0x48, 0xB9]);
        expected.extend_from_slice(&1u64.to_le_bytes());
        expected.extend_from_slice(&[0x49, 0xBA]);
        expected.extend_from_slice(&0x1000u64.to_le_bytes());
        expected.extend_from_slice(&[0x41, 0xFF, 0xD2]);
        expected.extend_from_slice(&[0x48, 0xA3]);
        expected.extend_from_slice(&0x2000u64.to_le_bytes());
        expected.extend_from_slice(&[0x48, 0x83, 0xC4, 0x28, 0xC3]);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn stub_slot_argument() {
        let calls = vec![(0x1000u64, vec![Arg::Slot(2)])];
        let bytes = build_stub(&calls, 0x3000);
        // mov rax,[slots+16] -> 48 A1 <addr>
        let idx = bytes
            .windows(2)
            .position(|w| w == [0x48, 0xA1])
            .unwrap();
        let src = u64::from_le_bytes(bytes[idx + 2..idx + 10].try_into().unwrap());
        assert_eq!(src, 0x3000 + 16);
        assert_eq!(&bytes[idx + 10..idx + 13], &[0x48, 0x89, 0xC1]);
    }

    #[test]
    fn mono_rva_table_complete() {
        assert_eq!(MONO_RVAS.len(), 11);
        let f = MonoFunctions::from_base(0x1000_0000);
        assert_eq!(f.thread_attach, 0x1000_0000 + 0x1EE030);
        assert_eq!(f.class_get_parent, 0x1000_0000 + 0xBFBF0);
    }
}
