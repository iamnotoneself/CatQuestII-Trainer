// 后台维持器：数值冻结、免伤标记、最大蓝条、游戏倍速、全屏拾取
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::actions::{alloc_scale_magic, resolve_timescale};
use crate::locate::Player;
use crate::memory::{err, ProcessMemory, Result};
use crate::mono::{Arg, MonoApi};
use crate::scan::{
    is_plausible_ptr, resolve_class_name, scan_exact, ClassReader, ValueType,
    FLAG_CANNOT_TAKE_DAMAGE, SLOT_HEALTH_FROM_STATS, SLOT_PLAYERCONTROLLED_FROM_STATS,
    SLOT_UNITFLAGS_FROM_STATS, STAT_BASE_OFF,
};

pub const MANA_STAT_OFF: usize = 0x18;
pub const MANA_VALUE_OFF: usize = 0x10;
const CACHED_PTR_OFF: usize = 0x10;
const COLLECT_TYPE_OFF: usize = 0x60; // 0=金币 1=经验
const COLLECT_AMOUNT_OFF: usize = 0x70; // 拾取金额

#[derive(Clone, Debug)]
pub enum KeeperEvent {
    Rebuild,
    Picked { got: u64, total: u64 },
    Error { kind: &'static str, msg: String },
}

fn sleep_or_stop(stop: &AtomicBool, dur: Duration) -> bool {
    let end = Instant::now() + dur;
    loop {
        if stop.load(Ordering::SeqCst) {
            return true;
        }
        let remain = end.saturating_duration_since(Instant::now());
        if remain.is_zero() {
            break;
        }
        thread::sleep(remain.min(Duration::from_millis(20)));
    }
    stop.load(Ordering::SeqCst)
}

// ============================================================
// 数值冻结
// ============================================================
/// 把若干地址的值固定为指定值。
/// 普通模式每 80ms 写回一次；高速模式（interval=0）紧密循环写回，
/// 用于对抗游戏里"每帧从属性同步到组件"的值（如同步移速）。
pub struct ValueFreezer {
    items: Mutex<HashMap<usize, (ValueType, Vec<u8>)>>,
    stop: AtomicBool,
    running: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl ValueFreezer {
    pub fn new() -> Self {
        Self {
            items: Mutex::new(HashMap::new()),
            stop: AtomicBool::new(false),
            running: AtomicBool::new(false),
            thread: Mutex::new(None),
        }
    }

    pub fn set(&self, addr: usize, vtype: ValueType, value: f64) {
        let mut items = self.items.lock().unwrap();
        if items.len() >= 20_000 && !items.contains_key(&addr) {
            items.clear();
        }
        items.insert(addr, (vtype, vtype.pack(value)));
    }

    pub fn remove(&self, addr: usize) {
        if let Ok(mut items) = self.items.lock() {
            items.remove(&addr);
        }
    }

    pub fn clear(&self) {
        if let Ok(mut items) = self.items.lock() {
            items.clear();
        }
    }

    pub fn count(&self) -> usize {
        self.items.lock().map(|i| i.len()).unwrap_or(0)
    }

    pub fn start(self: &Arc<Self>, pm: Arc<ProcessMemory>, interval: f64) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        self.stop.store(false, Ordering::SeqCst);
        let this = self.clone();
        let handle = thread::spawn(move || {
            let mut spins = 0u64;
            while !this.stop.load(Ordering::SeqCst) {
                let snapshot: Vec<(usize, Vec<u8>)> = this
                    .items
                    .lock()
                    .map(|i| i.iter().map(|(a, (_, b))| (*a, b.clone())).collect())
                    .unwrap_or_default();
                for (addr, packed) in snapshot {
                    let _ = pm.write(addr, &packed);
                }
                if interval > 0.0 {
                    if sleep_or_stop(&this.stop, Duration::from_secs_f64(interval)) {
                        break;
                    }
                } else {
                    // 高速模式：每 500 轮主动让出一次调度
                    spins += 1;
                    if spins % 500 == 0 {
                        thread::sleep(Duration::from_millis(1));
                    }
                }
            }
            this.running.store(false, Ordering::SeqCst);
        });
        if let Ok(mut t) = self.thread.lock() {
            *t = Some(handle);
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut t) = self.thread.lock() {
            if let Some(h) = t.take() {
                let _ = h.join();
            }
        }
        self.running.store(false, Ordering::SeqCst);
    }
}

impl Default for ValueFreezer {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================
// 免伤标记维持器
// ============================================================
/// 游戏里翻滚/冲刺/倒地/换人都会重建 UnitFlagsComponent，一次性写入的
/// CannotTakeDamage 标记随旧对象一起作废；因此周期校验槽位链路并补写。
pub struct FlagKeeper {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl FlagKeeper {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        pm: Arc<ProcessMemory>,
        players: Arc<Mutex<Vec<Player>>>,
        freezer: Arc<ValueFreezer>,
        god_enabled: Arc<AtomicBool>,
        on_refresh: impl Fn(&[Player]) + Send + 'static,
        events: Sender<KeeperEvent>,
        interval: f64,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = thread::spawn(move || {
            let mut cls = ClassReader::new(&pm);
            loop {
                if stop2.load(Ordering::SeqCst) {
                    break;
                }
                let result = {
                    let mut guard = match players.lock() {
                        Ok(g) => g,
                        Err(_) => break,
                    };
                    flag_tick(&pm, &mut guard, &freezer, &god_enabled, &mut cls)
                };
                match result {
                    Ok(true) => {
                        let snapshot = players.lock().map(|g| g.clone()).unwrap_or_default();
                        on_refresh(&snapshot);
                    }
                    Ok(false) => {}
                    Err(_) => {
                        let _ = events.send(KeeperEvent::Rebuild);
                        return;
                    }
                }
                if sleep_or_stop(&stop2, Duration::from_secs_f64(interval)) {
                    break;
                }
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for FlagKeeper {
    fn drop(&mut self) {
        self.stop();
    }
}

fn flag_tick(
    pm: &ProcessMemory,
    players: &mut [Player],
    freezer: &ValueFreezer,
    god: &AtomicBool,
    cls: &mut ClassReader,
) -> Result<bool> {
    let mut touched = false;
    let god_on = god.load(Ordering::SeqCst);
    for p in players.iter_mut() {
        if p.uf == 0 {
            continue;
        }
        let ent = p.ent;
        // ---- 免伤标记组件：对象可能被 Replace 换新 ----
        let uf_obj = pm.read_u64(ent + SLOT_UNITFLAGS_FROM_STATS)? as usize;
        if cls.cls(uf_obj).as_deref() != Some("UnitFlagsComponent") {
            return err("免伤组件身份校验失败");
        }
        if uf_obj != p.uf.wrapping_sub(0x10) {
            p.uf = uf_obj + 0x10;
            p.uf_val = pm.read_i32(p.uf)?;
            touched = true;
        }
        if god_on {
            let cur = pm.read_i32(p.uf)?;
            if cur & FLAG_CANNOT_TAKE_DAMAGE == 0 {
                pm.write_i32(p.uf, cur | FLAG_CANNOT_TAKE_DAMAGE)?;
            }
        }

        // ---- 血量组件：锁定地址跟上换新 ----
        let hp_obj = pm.read_u64(ent.wrapping_sub(SLOT_HEALTH_FROM_STATS))? as usize;
        if cls.cls(hp_obj).as_deref() != Some("HealthComponent") {
            return err("血量组件身份校验失败");
        }
        let hp_addr = hp_obj + 0x10;
        if hp_addr != p.hp {
            freezer.remove(p.hp); // 摘掉旧地址的锁
            p.hp = hp_addr;
            // 注意：原 Python 实现这里读的是当前血量，而非最大血量，保持原样
            p.hp_max = pm.read_i32(hp_addr)?;
            if god_on {
                let cur_hp = pm.read_i32(hp_addr)?;
                let target = if p.hp_max > 0 && p.hp_max <= 10_000_000 {
                    p.hp_max
                } else {
                    cur_hp
                };
                freezer.set(hp_addr, ValueType::Int32, target as f64);
            }
            touched = true;
        }

        // ---- Stats 组件：攻击/魔法/移速属性源跟上换新 ----
        let sc = pm.read_u64(ent)? as usize;
        if cls.cls(sc).as_deref() != Some("StatsComponent") {
            return err("属性组件身份校验失败");
        }
        for (off, key) in [(0x20usize, 0usize), (0x28, 1), (0x40, 2)] {
            let obj = pm.read_u64(sc + off)?;
            if is_plausible_ptr(obj) {
                let new_addr = obj as usize + STAT_BASE_OFF;
                let slot = match key {
                    0 => &mut p.atk,
                    1 => &mut p.magic,
                    _ => &mut p.addr,
                };
                if new_addr != *slot {
                    *slot = new_addr;
                    touched = true;
                }
            }
        }

        // ---- 实体身份总校验：这个实体表还是"那个玩家"吗 ----
        let pc_obj = pm.read_u64(ent.wrapping_sub(SLOT_PLAYERCONTROLLED_FROM_STATS))? as usize;
        if cls.cls(pc_obj).as_deref() != Some("PlayerControlledComponent") {
            return err("玩家实体已不存在");
        }
    }
    Ok(touched)
}

// ============================================================
// 游戏整体倍速
// ============================================================
pub struct GameSpeedKeeper {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl GameSpeedKeeper {
    pub fn start(
        pm: Arc<ProcessMemory>,
        scale: f64,
        events: Sender<KeeperEvent>,
        interval: f64,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = thread::spawn(move || {
            let result = run_speed_keeper(&pm, scale, &stop2, interval);
            if let Err(e) = result {
                let _ = events.send(KeeperEvent::Error {
                    kind: "speed",
                    msg: e.0,
                });
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for GameSpeedKeeper {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_speed_keeper(
    pm: &Arc<ProcessMemory>,
    scale: f64,
    stop: &AtomicBool,
    interval: f64,
) -> Result<()> {
    let mut api = MonoApi::open(pm.clone())?;
    let m_set = resolve_timescale(&mut api)?;
    let bits = alloc_scale_magic(&mut api, scale)?;
    let mut errs = 0;
    while !stop.load(Ordering::SeqCst) {
        let attempt: Result<()> = (|| {
            api.clear_exc()?;
            let f_invoke = api.f.runtime_invoke as u64;
            let exc = api.exc_buf as u64;
            api.seq(&[(
                f_invoke,
                vec![
                    Arg::Imm(m_set),
                    Arg::Imm(0),
                    Arg::Imm(bits),
                    Arg::Imm(exc),
                ],
            )])?;
            if !api.exc_name().is_empty() {
                return err("游戏内部保持倍速时发生异常");
            }
            Ok(())
        })();
        match attempt {
            Ok(()) => errs = 0,
            Err(_) => {
                errs += 1;
                if errs >= 5 {
                    return err("游戏内部保持倍速时发生异常（游戏可能已退出）");
                }
            }
        }
        if sleep_or_stop(stop, Duration::from_secs_f64(interval)) {
            break;
        }
    }
    Ok(())
}

// ============================================================
// 最大蓝条
// ============================================================
pub struct ManaKeeper {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ManaKeeper {
    pub fn start(
        pm: Arc<ProcessMemory>,
        events: Sender<KeeperEvent>,
        interval: f64,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = thread::spawn(move || {
            let mut worker = ManaWorker::new();
            let result = worker
                .setup(&pm)
                .and_then(|_| worker.run(&stop2, interval));
            if let Err(e) = result {
                let _ = events.send(KeeperEvent::Error {
                    kind: "mana",
                    msg: e.0,
                });
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for ManaKeeper {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Conv {
    Ptr,
    Inline,
}

struct ManaPlayer {
    e: u64,
    intstat: u64,
    mval: u64,
    buf: usize,
}

struct ManaWorker {
    api: Option<MonoApi>,
    game: u64,
    m_ents: u64,
    m_haspc: u64,
    m_stats: u64,
    m_mana: u64,
    m_repl: u64,
    s_val: usize,
    players: Vec<ManaPlayer>,
    conv: Option<Conv>,
    name_cache: HashMap<u64, Option<String>>,
}

impl ManaWorker {
    fn new() -> Self {
        Self {
            api: None,
            game: 0,
            m_ents: 0,
            m_haspc: 0,
            m_stats: 0,
            m_mana: 0,
            m_repl: 0,
            s_val: 0,
            players: Vec::new(),
            conv: None,
            name_cache: HashMap::new(),
        }
    }

    fn pm(&self) -> Arc<ProcessMemory> {
        self.api.as_ref().unwrap().pm.clone()
    }

    fn cls(&mut self, obj: u64) -> Option<String> {
        let pm = self.pm();
        let vtp = pm.read_u64(obj as usize).ok()?;
        if let Some(v) = self.name_cache.get(&vtp) {
            return v.clone();
        }
        let name = resolve_class_name(&pm, vtp as usize);
        self.name_cache.insert(vtp, name.clone());
        name
    }

    fn setup(&mut self, pm: &Arc<ProcessMemory>) -> Result<()> {
        let mut api = MonoApi::open(pm.clone())?;
        let image = api.image;
        let s_ns = api.rc.write_str("")?;
        let s_ctx = api.rc.write_str("Contexts")?;
        let s_ge = api.rc.write_str("GameEntity")?;
        let s_shared = api.rc.write_str("get_sharedInstance")?;
        let s_game = api.rc.write_str("get_game")?;
        let s_ents = api.rc.write_str("GetEntities")?;
        let s_haspc = api.rc.write_str("get_isPlayerControlled")?;
        let s_stats = api.rc.write_str("get_stats")?;
        let s_mana = api.rc.write_str("get_mana")?;
        let s_repl = api.rc.write_str("ReplaceMana")?;
        let s_val = api.rc.write_str("get_Value")?;

        let f_cfn = api.f.class_from_name as u64;
        let r = api.seq(&[
            (
                f_cfn,
                vec![Arg::Imm(image), Arg::Imm(s_ns as u64), Arg::Imm(s_ctx as u64)],
            ),
            (
                f_cfn,
                vec![Arg::Imm(image), Arg::Imm(s_ns as u64), Arg::Imm(s_ge as u64)],
            ),
        ])?;
        let (k_ctx, k_ge) = (r[1], r[2]);
        if k_ctx == 0 || k_ge == 0 {
            return err("未找到游戏的角色逻辑（游戏版本可能不匹配）");
        }
        let m_shared = api.find_method(k_ctx, s_shared, 0)?;
        let (shared, e) = if m_shared != 0 {
            api.invoke_checked(m_shared, 0, 0)?
        } else {
            (0, String::new())
        };
        if !e.is_empty() || shared == 0 || m_shared == 0 {
            return err("读不到游戏上下文（请先进入可操作角色的画面）");
        }
        let k_shared = api.object_class(shared)?;
        let m_game = api.find_method(k_shared, s_game, 0)?;
        let (game, e) = if m_game != 0 {
            api.invoke_checked(m_game, shared, 0)?
        } else {
            (0, String::new())
        };
        if !e.is_empty() || game == 0 || m_game == 0 {
            return err("读不到游戏状态（请先进入可操作角色的画面）");
        }
        let k_game = api.object_class(game)?;
        let m_ents = api.find_method(k_game, s_ents, 0)?;
        let m_haspc = api.find_method(k_ge, s_haspc, 0)?;
        let m_stats = api.find_method(k_ge, s_stats, 0)?;
        let m_mana = api.find_method(k_ge, s_mana, 0)?;
        let m_repl = api.find_method(k_ge, s_repl, 1)?;
        if m_ents == 0 || m_haspc == 0 || m_stats == 0 || m_mana == 0 || m_repl == 0 {
            return err("未找到游戏的蓝量入口（游戏版本可能不匹配）");
        }
        self.game = game;
        self.m_ents = m_ents;
        self.m_haspc = m_haspc;
        self.m_stats = m_stats;
        self.m_mana = m_mana;
        self.m_repl = m_repl;
        self.s_val = s_val;
        self.api = Some(api);
        Ok(())
    }

    fn api(&mut self) -> &mut MonoApi {
        self.api.as_mut().unwrap()
    }

    fn inv(&mut self, m: u64, this: u64, arg: u64) -> Result<(u64, String)> {
        let api = self.api.as_mut().unwrap();
        api.invoke_checked(m, this, arg)
    }

    fn drop_players(&mut self) {
        for p in self.players.drain(..) {
            if let Some(api) = self.api.as_mut() {
                api.rc.free(p.buf);
            }
        }
    }

    /// 重拉角色列表并解析每个玩家的蓝量链路
    fn chain(&mut self) -> Result<usize> {
        self.drop_players();
        let (arr, e) = self.inv(self.m_ents, self.game, 0)?;
        if !e.is_empty() || arr == 0 {
            return err(format!(
                "读不到角色列表{}",
                if e.is_empty() { String::new() } else { format!("：{}", e) }
            ));
        }
        let pm = self.pm();
        let mut data_off = 0usize;
        let mut count = 0usize;
        for off in [0x18usize, 0x20, 0x10, 0x28] {
            let n = match pm.read_u64(arr as usize + off - 8) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if !(1..=200_000).contains(&n) {
                continue;
            }
            let first = match pm.read_u64(arr as usize + off) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if self.cls(first).as_deref() != Some("GameEntity") {
                continue;
            }
            if n >= 2 {
                let second = match pm.read_u64(arr as usize + off + 8) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if self.cls(second).as_deref() != Some("GameEntity") {
                    continue;
                }
            }
            data_off = off;
            count = n as usize;
            break;
        }
        if data_off == 0 {
            return err("角色列表格式变了（游戏版本可能不匹配）");
        }

        let mut found: Vec<u64> = Vec::new();
        let f_invoke = self.api().f.runtime_invoke as u64;
        let exc_buf = self.api().exc_buf as u64;
        let mut i = 0usize;
        while i < count {
            let end = (i + 50).min(count);
            let ents: Vec<u64> = (i..end)
                .map(|j| pm.read_u64(arr as usize + data_off + 8 * j).unwrap_or(0))
                .collect();
            self.api().clear_exc()?;
            let steps: Vec<(u64, Vec<Arg>)> = ents
                .iter()
                .map(|&ep| {
                    (
                        f_invoke,
                        vec![Arg::Imm(self.m_haspc), Arg::Imm(ep), Arg::Imm(0), Arg::Imm(exc_buf)],
                    )
                })
                .collect();
            let r = self.api().seq(&steps)?;
            let e = self.api().exc_name();
            if !e.is_empty() {
                return err(format!("识别角色时游戏内部异常（{}）", e));
            }
            for j in 0..ents.len() {
                if self.api().is_true(r[1 + j]) {
                    found.push(ents[j]);
                }
            }
            if found.len() >= 2 {
                break;
            }
            i += 50;
        }

        for &e_obj in found.iter().take(2) {
            let (sc, e) = self.inv(self.m_stats, e_obj, 0)?;
            if !e.is_empty() || sc == 0 {
                continue;
            }
            let intstat = match pm.read_u64(sc as usize + MANA_STAT_OFF) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if self.cls(intstat).as_deref() != Some("IntStat") {
                continue;
            }
            let k_is = self.api().object_class(intstat)?;
            let s_val = self.s_val;
            let m_val = self.api().find_method(k_is, s_val, 0)?;
            if m_val == 0 {
                continue;
            }
            let buf = self.api().rc.alloc(0x40, false)?;
            pm.write_u64(buf, buf as u64 + 8)?; // 基线 ptr 布局
            self.players.push(ManaPlayer {
                e: e_obj,
                intstat,
                mval: m_val,
                buf,
            });
        }
        Ok(self.players.len())
    }

    /// 发一次 ReplaceMana 再读回当前蓝量
    fn set_read(&mut self, p: &ManaPlayer, _v: i32) -> Result<u32> {
        let (_, e) = self.inv(self.m_repl, p.e, p.buf as u64)?;
        if !e.is_empty() {
            return err(format!("刷新蓝条时游戏内部异常（{}）", e));
        }
        let (mo, e) = self.inv(self.m_mana, p.e, 0)?;
        if !e.is_empty() || mo == 0 {
            return err(format!(
                "读取蓝量失败（{}）",
                if e.is_empty() { "对象为空" } else { &e }
            ));
        }
        Ok(self.pm().read_i32(mo as usize + MANA_VALUE_OFF)? as u32)
    }

    /// 判定 mono_runtime_invoke 的 int 传参约定，判一次终身有效
    fn probe_conv(&mut self, p: &ManaPlayer, v: i32) -> Result<()> {
        let pm = self.pm();
        pm.write_u64(p.buf, p.buf as u64 + 8)?;
        pm.write_i32(p.buf + 8, v)?;
        let got = self.set_read(p, v)?;
        if got == v as u32 {
            self.conv = Some(Conv::Ptr);
            return Ok(());
        }
        if got == ((p.buf as u64 + 8) & 0xFFFF_FFFF) as u32 {
            self.conv = Some(Conv::Inline);
            pm.write_u64(p.buf, v as u32 as u64)?;
            if self.set_read(p, v)? == v as u32 {
                return Ok(());
            }
            self.conv = None;
        }
        err("写入蓝量没生效，请关掉开关再试一次")
    }

    fn tick(&mut self) -> Result<()> {
        if self.players.is_empty() {
            return Ok(());
        }
        let pm = self.pm();
        // 一拍：读各自蓝量上限
        let f_invoke = self.api().f.runtime_invoke as u64;
        let exc_buf = self.api().exc_buf as u64;
        pm.write(exc_buf as usize, &[0u8; 8])?;
        let steps: Vec<(u64, Vec<Arg>)> = self
            .players
            .iter()
            .map(|p| {
                (
                    f_invoke,
                    vec![
                        Arg::Imm(p.mval),
                        Arg::Imm(p.intstat),
                        Arg::Imm(0),
                        Arg::Imm(exc_buf),
                    ],
                )
            })
            .collect();
        let r = self.api().seq(&steps)?;
        let e = self.api().exc_name();
        if !e.is_empty() {
            return err(format!("读取蓝量上限时游戏内部异常（{}）", e));
        }
        let mut targets: Vec<(usize, i32)> = Vec::new();
        for i in 0..self.players.len() {
            let v = self.api().as_i32(r[1 + i]);
            if v > 0 && v < 10_000_000 {
                targets.push((i, v));
            }
        }
        if targets.is_empty() {
            return Ok(()); // 菜单/过场读不到蓝量，等下一拍
        }
        if self.conv.is_none() {
            let p = ManaPlayer {
                e: self.players[targets[0].0].e,
                intstat: self.players[targets[0].0].intstat,
                mval: self.players[targets[0].0].mval,
                buf: self.players[targets[0].0].buf,
            };
            let v = targets[0].1;
            self.probe_conv(&p, v)?;
        }
        // 二拍：按上限写蓝 + 走 ReplaceMana 触发 HUD 刷新
        for (idx, v) in &targets {
            let buf = self.players[*idx].buf;
            if self.conv == Some(Conv::Ptr) {
                pm.write_i32(buf + 8, *v)?;
            } else {
                pm.write_u64(buf, *v as u32 as u64)?;
            }
        }
        let mut steps: Vec<(u64, Vec<Arg>)> = Vec::new();
        for (idx, _) in &targets {
            let p = &self.players[*idx];
            steps.push((
                f_invoke,
                vec![
                    Arg::Imm(self.m_repl),
                    Arg::Imm(p.e),
                    Arg::Imm(p.buf as u64),
                    Arg::Imm(exc_buf),
                ],
            ));
        }
        for (idx, _) in &targets {
            let e_obj = self.players[*idx].e;
            steps.push((
                f_invoke,
                vec![Arg::Imm(self.m_mana), Arg::Imm(e_obj), Arg::Imm(0), Arg::Imm(exc_buf)],
            ));
        }
        pm.write(exc_buf as usize, &[0u8; 8])?;
        let r = self.api().seq(&steps)?;
        let e = self.api().exc_name();
        if !e.is_empty() {
            return err(format!("刷新蓝条时游戏内部异常（{}）", e));
        }
        let n = targets.len();
        for (i, (idx, v)) in targets.iter().enumerate() {
            let mo = r[1 + n + i];
            if mo == 0 || pm.read_i32(mo as usize + MANA_VALUE_OFF)? != *v {
                return err("蓝量写入未生效");
            }
            let _ = idx;
        }
        Ok(())
    }

    fn run(&mut self, stop: &AtomicBool, interval: f64) -> Result<()> {
        let mut errs = 0;
        let mut tick = 0u64;
        while !stop.load(Ordering::SeqCst) {
            let r: Result<()> = (|| {
                if self.players.is_empty() || tick % 8 == 0 {
                    self.chain()?;
                }
                tick += 1;
                if !self.players.is_empty() {
                    self.tick()?;
                }
                Ok(())
            })();
            match r {
                Ok(()) => errs = 0,
                Err(_) => {
                    self.drop_players();
                    errs += 1;
                    if errs >= 5 {
                        return err("蓝条维持失败（游戏可能已退出）");
                    }
                }
            }
            if sleep_or_stop(stop, Duration::from_secs_f64(interval)) {
                break;
            }
        }
        Ok(())
    }
}

// ============================================================
// 全屏拾取
// ============================================================
pub struct PickupKeeper {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl PickupKeeper {
    pub fn start(
        pm: Arc<ProcessMemory>,
        events: Sender<KeeperEvent>,
        interval: f64,
        full_scan: f64,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = thread::spawn(move || {
            if let Err(e) = run_pickup_keeper(&pm, &stop2, &events, interval, full_scan) {
                let _ = events.send(KeeperEvent::Error {
                    kind: "pickup",
                    msg: e.0,
                });
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for PickupKeeper {
    fn drop(&mut self) {
        self.stop();
    }
}

fn decode_collectible(raw: &[u8], vt: u64) -> Option<(i32, i32)> {
    if raw.len() < 0x78 {
        return None;
    }
    let vtp = u64::from_le_bytes(raw[0..8].try_into().unwrap());
    if vtp != vt {
        return None;
    }
    if u64::from_le_bytes(raw[CACHED_PTR_OFF..CACHED_PTR_OFF + 8].try_into().unwrap()) == 0 {
        return None;
    }
    let amt = i32::from_le_bytes(
        raw[COLLECT_AMOUNT_OFF..COLLECT_AMOUNT_OFF + 4]
            .try_into()
            .unwrap(),
    );
    let typ = i32::from_le_bytes(
        raw[COLLECT_TYPE_OFF..COLLECT_TYPE_OFF + 4]
            .try_into()
            .unwrap(),
    );
    if !(0 < amt && amt < 10_000_000) || !(typ == 0 || typ == 1) {
        return None;
    }
    Some((amt, typ))
}

fn decode(raw: &[u8], vt: u64) -> Option<(i32, i32)> {
    decode_collectible(raw, vt)
}

fn full_scan(pm: &ProcessMemory, vt: u64) -> Result<HashMap<usize, (usize, i32, i32)>> {
    let (objs, _) = scan_exact(pm, &vt.to_le_bytes(), None)?;
    let mut out = HashMap::new();
    for o in objs {
        let raw = match pm.read(o, 0x78) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if let Some((amt, typ)) = decode(&raw, vt) {
            out.insert(o, (o, amt, typ));
        }
    }
    Ok(out)
}

fn recheck(
    pm: &ProcessMemory,
    known: &mut HashMap<usize, (usize, i32, i32)>,
    vt: u64,
) -> Vec<(usize, i32, i32)> {
    let mut live = Vec::new();
    for o in known.keys().copied().collect::<Vec<_>>() {
        let raw = match pm.read(o, 0x78) {
            Ok(r) => r,
            Err(_) => {
                known.remove(&o);
                continue;
            }
        };
        match decode(&raw, vt) {
            Some((amt, typ)) => {
                known.insert(o, (o, amt, typ));
                live.push((o, amt, typ));
            }
            None => {
                // 无原生指针=池中休眠，地址保留等复用；vtable 对不上=对象换代，移除
                let vtp = u64::from_le_bytes(raw[0..8].try_into().unwrap());
                if vtp != vt {
                    known.remove(&o);
                }
            }
        }
    }
    live
}

/// 拾取单个；无论成败都清零金额（防止反复重试同一坏对象）
fn pickup_one(api: &mut MonoApi, method: u64, item: (usize, i32, i32)) -> bool {
    let pm = api.pm.clone();
    let exc_buf = api.exc_buf as u64;
    let _ = pm.write(exc_buf as usize, &[0u8; 8]);
    let f_invoke = api.f.runtime_invoke as u64;
    // 远程调用失败也视为异常（对应 Python 的 exc = -1），
    // 不能把清过零的异常槽误判成成功
    let exc = match api.seq(&[(
        f_invoke,
        vec![
            Arg::Imm(method),
            Arg::Imm(item.0 as u64),
            Arg::Imm(0),
            Arg::Imm(exc_buf),
        ],
    )]) {
        Ok(_) => pm.read_u64(exc_buf as usize).unwrap_or(1),
        Err(_) => 1,
    };
    let _ = pm.write_i32(item.0 + COLLECT_AMOUNT_OFF, 0);
    exc == 0
}

fn collect_live(
    api: &mut MonoApi,
    method: u64,
    live: &[(usize, i32, i32)],
    vt: u64,
    stop: &AtomicBool,
) -> (u64, u64) {
    let pm = api.pm.clone();
    let exc_buf = api.exc_buf as u64;
    let f_invoke = api.f.runtime_invoke as u64;
    let mut got = 0u64;
    let mut total = 0u64;
    let mut i = 0usize;
    while i < live.len() && !stop.load(Ordering::SeqCst) {
        let chunk: Vec<(usize, i32, i32)> = live[i..(i + 20).min(live.len())].to_vec();
        i += 20;
        // 调用前逐个重校验（对象可能刚被玩家捡走/回收）
        let valid: Vec<(usize, i32, i32)> = chunk
            .into_iter()
            .filter(|it| {
                pm.read(it.0, 0x78)
                    .ok()
                    .map(|raw| decode(&raw, vt).is_some())
                    .unwrap_or(false)
            })
            .collect();
        if valid.is_empty() {
            continue;
        }
        let _ = pm.write(exc_buf as usize, &[0u8; 8]);
        let mut steps: Vec<(u64, Vec<Arg>)> = Vec::with_capacity(valid.len());
        for it in &valid {
            steps.push((
                f_invoke,
                vec![
                    Arg::Imm(method),
                    Arg::Imm(it.0 as u64),
                    Arg::Imm(0),
                    Arg::Imm(exc_buf),
                ],
            ));
        }
        let ok = api.seq_timeout(&steps, 30000).is_ok();
        let exc = if ok {
            pm.read_u64(exc_buf as usize).unwrap_or(1)
        } else {
            1
        };
        if exc != 0 {
            // 批量出问题 -> 逐个拾取隔离坏对象
            for it in valid {
                if pickup_one(api, method, it) {
                    got += 1;
                    total += it.1 as u64;
                }
            }
            continue;
        }
        for it in &valid {
            let _ = pm.write_i32(it.0 + COLLECT_AMOUNT_OFF, 0);
        }
        got += valid.len() as u64;
        total += valid.iter().map(|it| it.1 as u64).sum::<u64>();
    }
    (got, total)
}

fn run_pickup_keeper(
    pm: &Arc<ProcessMemory>,
    stop: &AtomicBool,
    events: &Sender<KeeperEvent>,
    interval: f64,
    full_scan_secs: f64,
) -> Result<()> {
    let mut api = MonoApi::open(pm.clone())?;
    let image = api.image;
    let s_ns = api.rc.write_str("")?;
    let s_cls = api.rc.write_str("Collectible")?;
    let s_mtd = api.rc.write_str("ExecuteCollection")?;
    let f_cfn = api.f.class_from_name as u64;
    let f_getm = api.f.class_get_method_from_name as u64;
    let f_vtable = api.f.class_vtable as u64;
    let domain = api.domain;
    let r = api.seq(&[(
        f_cfn,
        vec![Arg::Imm(image), Arg::Imm(s_ns as u64), Arg::Imm(s_cls as u64)],
    )])?;
    let klass = r[1];
    let method = if klass != 0 {
        let r2 = api.seq(&[(
            f_getm,
            vec![Arg::Imm(klass), Arg::Imm(s_mtd as u64), Arg::Imm(0)],
        )])?;
        r2[1]
    } else {
        0
    };
    if klass == 0 || method == 0 {
        return err("未找到游戏的拾取逻辑（游戏版本可能不匹配）");
    }
    let r = api.seq(&[(f_vtable, vec![Arg::Imm(domain), Arg::Imm(klass)])])?;
    let vt = r[1];
    if vt == 0 {
        return err("未找到掉落物类型信息");
    }

    let mut known: HashMap<usize, (usize, i32, i32)> = HashMap::new();
    let mut last_full: Option<Instant> = None;
    let mut errs = 0;
    while !stop.load(Ordering::SeqCst) {
        let attempt: Result<()> = (|| {
            let mut live = recheck(pm, &mut known, vt);
            let due = last_full
                .map(|t| t.elapsed().as_secs_f64() >= full_scan_secs)
                .unwrap_or(true);
            if due {
                known.extend(full_scan(pm, vt)?);
                last_full = Some(Instant::now());
                live = recheck(pm, &mut known, vt);
            }
            if !live.is_empty() {
                let (got, total) = collect_live(&mut api, method, &live, vt, stop);
                if got > 0 {
                    let _ = events.send(KeeperEvent::Picked { got, total });
                }
            }
            Ok(())
        })();
        match attempt {
            Ok(()) => errs = 0,
            Err(_) => {
                errs += 1;
                if errs >= 5 {
                    return err("全屏拾取失败（游戏可能已退出）");
                }
            }
        }
        if sleep_or_stop(stop, Duration::from_secs_f64(interval)) {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collectible_layout_constants() {
        assert_eq!(CACHED_PTR_OFF, 0x10);
        assert_eq!(COLLECT_TYPE_OFF, 0x60);
        assert_eq!(COLLECT_AMOUNT_OFF, 0x70);
        assert_eq!(MANA_STAT_OFF, 0x18);
        assert_eq!(MANA_VALUE_OFF, 0x10);
    }

    #[test]
    fn decode_requires_vtable_and_amount() {
        let vt = 0x1234_5678u64;
        let mut raw = vec![0u8; 0x78];
        raw[0..8].copy_from_slice(&vt.to_le_bytes());
        assert!(decode(&raw, vt).is_none()); // 无原生指针
        raw[0x10..0x18].copy_from_slice(&0x1000u64.to_le_bytes());
        raw[0x70..0x74].copy_from_slice(&25i32.to_le_bytes());
        raw[0x60..0x64].copy_from_slice(&1i32.to_le_bytes());
        assert_eq!(decode(&raw, vt), Some((25, 1)));
        assert!(decode(&raw, vt + 8).is_none());
        raw[0x70..0x74].copy_from_slice(&0i32.to_le_bytes());
        assert!(decode(&raw, vt).is_none());
    }
}
