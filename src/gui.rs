// 界面：三个页签（一键功能 / 强迫症修改 / 原理说明）
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Layout,
    Margin, RichText, Sense, Stroke, Vec2,
};

use crate::actions::{self, KILL_DAMAGE, MAX_LEVEL};
use crate::keepers::{
    FlagKeeper, GameSpeedKeeper, KeeperEvent, ManaKeeper, PickupKeeper, ValueFreezer,
};
use crate::locate::{locate_player_speed_sources, Player};
use crate::memory::{self, ProcessMemory};
use crate::scan::{ValueType, FLAG_CANNOT_TAKE_DAMAGE};

pub const APP_TITLE: &str = "猫咪斗恶龙2 修改器 2.0";

const BG: Color32 = Color32::from_rgb(0xf0, 0xf2, 0xf5);
const CARD_BG: Color32 = Color32::from_rgb(0xf7, 0xf9, 0xfc);
const CARD_BD: Color32 = Color32::from_rgb(0xd5, 0xdd, 0xe8);
const TEXT: Color32 = Color32::from_rgb(0x22, 0x2a, 0x35);
const SUBTEXT: Color32 = Color32::from_rgb(0x6b, 0x74, 0x82);
const BTN_BLUE: Color32 = Color32::from_rgb(0x2d, 0x7f, 0xf9);
const BTN_GREEN: Color32 = Color32::from_rgb(0x22, 0xa4, 0x5d);
const BTN_RED: Color32 = Color32::from_rgb(0xe0, 0x52, 0x52);
const BTN_ORANGE: Color32 = Color32::from_rgb(0xf0, 0xa0, 0x20);
const SWITCH_ON: Color32 = Color32::from_rgb(0x2f, 0xbf, 0x71);
const SWITCH_OFF: Color32 = Color32::from_rgb(0xc4, 0xcb, 0xd4);

const SPEED_OPTS: [&str; 6] = ["不变", "1.5倍", "2倍", "3倍", "5倍", "10倍"];
const SPEED_VALUES: [f32; 6] = [1.0, 1.5, 2.0, 3.0, 5.0, 10.0];

#[derive(Debug)]
enum Msg {
    Status(String),
    Located {
        task_gen: u64,
        res: std::result::Result<Vec<Player>, String>,
    },
    MaxLevel {
        task_gen: u64,
        res: std::result::Result<i32, String>,
    },
    MaxGold {
        task_gen: u64,
        res: std::result::Result<(i32, bool), String>,
    },
    SpeedTask {
        task_gen: u64,
        res: std::result::Result<(), String>,
    },
    Keeper(KeeperEvent),
}

#[derive(Clone, Copy, Debug)]
enum Pending {
    God(bool),
    Kill(bool),
    Speed(f32),
}

pub struct TrainerApp {
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    status: String,
    proc_status: String,
    adv_status: String,
    busy: bool,
    task_gen: u64,
    pm: Option<Arc<ProcessMemory>>,
    pid: Option<u32>,
    players: Arc<Mutex<Vec<Player>>>,
    orig: Vec<Player>,
    freezer: Arc<ValueFreezer>,
    god: bool,
    kill: bool,
    speed_idx: usize,
    mana: bool,
    pickup: bool,
    gspeed_idx: usize,
    gspeed_applied: usize,
    adv_god: Arc<AtomicBool>,
    adv_kill: Arc<AtomicBool>,
    adv_speed: Arc<Mutex<f32>>,
    flag_keeper: Option<FlagKeeper>,
    mana_keeper: Option<ManaKeeper>,
    pickup_keeper: Option<PickupKeeper>,
    speed_keeper: Option<GameSpeedKeeper>,
    pending: Option<Pending>,
    relocating: bool,
    last_rebuild: Instant,
    pickup_total: u64,
    dialog: Option<(String, String)>,
    tab: usize,
    round_done: bool,
}

impl TrainerApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_fonts(&cc.egui_ctx);
        apply_style(&cc.egui_ctx);
        let (tx, rx) = channel();
        let game = memory::find_game_pids();
        let status = if game.is_empty() {
            "未检测到游戏进程，请先启动《猫咪斗恶龙2》".to_string()
        } else {
            "检测到游戏正在运行，可点「连接并识别角色」".to_string()
        };
        Self {
            tx,
            rx,
            status,
            proc_status: "未附加".to_string(),
            adv_status: "未识别".to_string(),
            busy: false,
            task_gen: 0,
            pm: None,
            pid: None,
            players: Arc::new(Mutex::new(Vec::new())),
            orig: Vec::new(),
            freezer: Arc::new(ValueFreezer::new()),
            god: false,
            kill: false,
            speed_idx: 0,
            mana: false,
            pickup: false,
            gspeed_idx: 0,
            gspeed_applied: 0,
            adv_god: Arc::new(AtomicBool::new(false)),
            adv_kill: Arc::new(AtomicBool::new(false)),
            adv_speed: Arc::new(Mutex::new(1.0)),
            flag_keeper: None,
            mana_keeper: None,
            pickup_keeper: None,
            speed_keeper: None,
            pending: None,
            relocating: false,
            last_rebuild: Instant::now() - Duration::from_secs(60),
            pickup_total: 0,
            dialog: None,
            tab: 0,
            round_done: false,
        }
    }

    fn poll_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Status(s) => self.status = s,
                Msg::Located { task_gen, res } => self.on_located(task_gen, res),
                Msg::MaxLevel { task_gen, res } => {
                    if task_gen == self.task_gen {
                        self.busy = false;
                        match res {
                            Ok(level) => {
                                self.status =
                                    format!("一键满级完成：已升到 {} 级（属性已同步成长）", level);
                                self.dialog = Some((
                                    "一键满级".to_string(),
                                    format!(
                                        "已升到 {} 级满级！\n血量、攻击等属性已按满级成长，进度已自动存档。\n切回游戏即可看到效果。",
                                        level
                                    ),
                                ));
                            }
                            Err(e) => {
                                self.status = format!("一键满级失败：{}", truncate(&e, 80));
                                self.dialog =
                                    Some(("一键满级没成功".to_string(), e));
                            }
                        }
                    }
                }
                Msg::MaxGold { task_gen, res } => {
                    if task_gen == self.task_gen {
                        self.busy = false;
                        match res {
                            Ok((gold, changed)) => {
                                if changed {
                                    self.status =
                                        format!("一键满金币完成：当前金币 {}", comma(gold));
                                } else {
                                    self.status = format!(
                                        "当前金币 {} 已经不低于满额，未做改动",
                                        comma(gold)
                                    );
                                }
                            }
                            Err(e) => {
                                self.status = format!("一键满金币失败：{}", truncate(&e, 80));
                                self.dialog =
                                    Some(("一键满金币没成功".to_string(), e));
                            }
                        }
                    }
                }
                Msg::SpeedTask { task_gen, res } => {
                    if task_gen == self.task_gen {
                        self.busy = false;
                        match res {
                            Ok(()) => self.status = "游戏速度已恢复正常".to_string(),
                            Err(e) => {
                                self.status =
                                    format!("恢复游戏速度失败：{}", truncate(&e, 60))
                            }
                        }
                    }
                }
                Msg::Keeper(ev) => self.on_keeper_event(ev),
            }
        }
    }

    fn on_keeper_event(&mut self, ev: KeeperEvent) {
        match ev {
            KeeperEvent::Picked { got, total } => {
                self.pickup_total += total;
                self.status = format!(
                    "全屏拾取中：又捡到 {} 个（本次累计数值 {}）",
                    got, self.pickup_total
                );
            }
            KeeperEvent::Rebuild => self.do_rebuild(),
            KeeperEvent::Error { kind, msg } => match kind {
                "speed" => {
                    if let Some(mut k) = self.speed_keeper.take() {
                        k.stop();
                    }
                    self.gspeed_idx = 0;
                    self.gspeed_applied = 0;
                    self.status = format!("游戏速度已自动关闭：{}", truncate(&msg, 80));
                    self.dialog = Some((
                        "游戏速度".to_string(),
                        format!("游戏速度已自动关闭：\n{}", msg),
                    ));
                }
                "mana" => {
                    if let Some(mut k) = self.mana_keeper.take() {
                        k.stop();
                    }
                    self.mana = false;
                    self.status = format!("最大蓝条已自动关闭：{}", truncate(&msg, 80));
                    self.dialog = Some((
                        "最大蓝条".to_string(),
                        format!("最大蓝条已自动关闭：\n{}", msg),
                    ));
                }
                "pickup" => {
                    if let Some(mut k) = self.pickup_keeper.take() {
                        k.stop();
                    }
                    self.pickup = false;
                    self.status = format!("全屏拾取已自动关闭：{}", truncate(&msg, 80));
                    self.dialog = Some((
                        "全屏拾取".to_string(),
                        format!("全屏拾取已自动关闭：\n{}", msg),
                    ));
                }
                _ => self.status = msg,
            },
        }
    }

    fn on_located(
        &mut self,
        task_gen: u64,
        res: std::result::Result<Vec<Player>, String>,
    ) {
        if task_gen != self.task_gen {
            return;
        }
        self.busy = false;
        match res {
            Ok(players) => {
                if players.is_empty() {
                    self.relocating = false;
                    self.adv_status = "未识别（进入游戏画面后再试）".to_string();
                    return;
                }
                self.players.lock().unwrap().clear();
                self.players.lock().unwrap().extend(players.clone());
                if self.orig.is_empty() {
                    self.orig = players.clone();
                }
                self.adv_status = format!(
                    "已识别：{}",
                    players
                        .iter()
                        .map(|p| p.name.as_str())
                        .collect::<Vec<_>>()
                        .join("、")
                );
                self.start_flag_keeper();
                self.reconcile_flags();
                self.relocating = false;
                self.status = "识别完成，可以使用下面的选项了".to_string();
            }
            Err(e) => {
                self.relocating = false;
                self.adv_status = "未识别（点「连接并识别角色」）".to_string();
                self.status = format!("识别失败：{}", truncate(&e, 80));
                self.dialog = Some(("识别失败".to_string(), e));
            }
        }
    }

    fn reconcile_flags(&mut self) {
        if self.adv_kill.load(Ordering::SeqCst) {
            self.apply_kill(true);
        }
        if self.god {
            self.apply_god(true);
        }
        let mult = *self.adv_speed.lock().unwrap();
        if mult != 1.0 {
            self.apply_speed(mult);
        }
    }

    fn do_rebuild(&mut self) {
        let now = Instant::now();
        if self.relocating || now.duration_since(self.last_rebuild) < Duration::from_secs(10) {
            return;
        }
        self.last_rebuild = now;
        if self.busy || self.pm.is_none() {
            return;
        }
        self.relocating = true;
        if let Some(mut k) = self.flag_keeper.take() {
            k.stop();
        }
        self.status = "检测到场景切换，正在自动重新识别……".to_string();
        self.spawn_locate();
    }

    fn keeper_tx(&self) -> Sender<KeeperEvent> {
        let tx = self.tx.clone();
        let (ktx, krx) = channel();
        std::thread::spawn(move || {
            while let Ok(ev) = krx.recv() {
                if tx.send(Msg::Keeper(ev)).is_err() {
                    break;
                }
            }
        });
        ktx
    }

    fn start_flag_keeper(&mut self) {
        let Some(pm) = self.pm.clone() else { return };
        if self.players.lock().map(|p| p.is_empty()).unwrap_or(true) {
            return;
        }
        if let Some(mut k) = self.flag_keeper.take() {
            k.stop();
        }
        let players = self.players.clone();
        let freezer = self.freezer.clone();
        let adv_god = self.adv_god.clone();
        let adv_kill = self.adv_kill.clone();
        let adv_speed = self.adv_speed.clone();
        let pm2 = pm.clone();
        let freezer2 = freezer.clone();
        let tx = self.keeper_tx();
        self.flag_keeper = Some(FlagKeeper::start(
            pm,
            players,
            freezer,
            adv_god,
            move |ps: &[Player]| {
                // 组件被 Replace 换新：把一次性写入的秒杀/移速补到新地址
                if adv_kill.load(Ordering::SeqCst) {
                    for p in ps {
                        if p.atk != 0 {
                            let _ = pm2.write_i32(p.atk, KILL_DAMAGE);
                        }
                        if p.magic != 0 {
                            let _ = pm2.write_i32(p.magic, KILL_DAMAGE);
                        }
                    }
                }
                let mult = *adv_speed.lock().unwrap_or_else(|e| e.into_inner());
                if mult != 1.0 {
                    for p in ps {
                        if p.addr != 0 {
                            let v = p.value * mult;
                            let _ = pm2.write_f32(p.addr, v);
                            freezer2.set(p.addr, ValueType::Float, v as f64);
                        }
                    }
                }
            },
            tx,
            0.5,
        ));
    }

    fn auto_attach(&mut self, action: &str) -> bool {
        if self.pm.is_some() {
            return true;
        }
        let pids = memory::windowed_game_pids();
        let Some(&pid) = pids.first() else {
            self.dialog = Some((
                "提示".to_string(),
                format!(
                    "未发现游戏进程「{}」。\n请先启动游戏并进入游戏画面，再{}。",
                    memory::GAME_PROCESS_NAME, action
                ),
            ));
            return false;
        };
        match ProcessMemory::new(pid) {
            Ok(pm) => {
                let pm = Arc::new(pm);
                self.freezer.start(pm.clone(), 0.08);
                self.pm = Some(pm);
                self.pid = Some(pid);
                self.proc_status = format!("已附加 PID {}", pid);
                true
            }
            Err(e) => {
                self.dialog = Some(("附加失败".to_string(), e.0));
                false
            }
        }
    }

    fn detach_all(&mut self) {
        self.stop_all_keepers();
        self.freezer.clear();
        self.freezer.stop();
        self.pm = None;
        self.pid = None;
        self.players.lock().unwrap().clear();
        self.orig.clear();
        self.god = false;
        self.kill = false;
        self.speed_idx = 0;
        self.mana = false;
        self.pickup = false;
        self.gspeed_idx = 0;
        self.gspeed_applied = 0;
        self.pickup_total = 0;
        self.adv_god.store(false, Ordering::SeqCst);
        self.adv_kill.store(false, Ordering::SeqCst);
        *self.adv_speed.lock().unwrap() = 1.0;
        self.proc_status = "未附加".to_string();
        self.adv_status = "未识别".to_string();
    }

    fn stop_all_keepers(&mut self) {
        if let Some(mut k) = self.flag_keeper.take() {
            k.stop();
        }
        if let Some(mut k) = self.mana_keeper.take() {
            k.stop();
        }
        if let Some(mut k) = self.pickup_keeper.take() {
            k.stop();
        }
        if let Some(mut k) = self.speed_keeper.take() {
            k.stop();
        }
    }

    fn begin_task(&mut self) -> u64 {
        self.busy = true;
        self.task_gen += 1;
        self.task_gen
    }

    fn spawn_locate(&mut self) {
        let Some(pm) = self.pm.clone() else { return };
        if self.busy {
            return;
        }
        let task_gen = self.begin_task();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let tx2 = tx.clone();
            let mut cb = |m: &str| {
                let _ = tx2.send(Msg::Status(m.to_string()));
            };
            let res = locate_player_speed_sources(&pm, &mut cb).map_err(|e| e.0);
            let _ = tx.send(Msg::Located { task_gen, res });
        });
    }

    fn on_connect(&mut self) {
        if self.busy {
            self.status = "正在处理，请稍候再操作……".to_string();
            return;
        }
        if !self.auto_attach("点此按钮") {
            return;
        }
        if !self.players.lock().map(|p| p.is_empty()).unwrap_or(true) {
            self.restore_all(false);
            // 旧存档/旧场景的原始值已还原并作废，重新识别时按新角色记录
            self.orig.clear();
        }
        self.status = "正在识别角色属性（通常几秒，请稍候）...".to_string();
        self.spawn_locate();
    }

    fn ensure_players(&mut self, pending: Pending) -> bool {
        if !self.players.lock().map(|p| p.is_empty()).unwrap_or(true) {
            return true;
        }
        if self.busy {
            self.status = "正在处理，请稍候再操作……".to_string();
            return false;
        }
        if !self.auto_attach("点此按钮") {
            return false;
        }
        self.pending = Some(pending);
        self.status = "正在识别角色属性（通常几秒，请稍候）...".to_string();
        self.spawn_locate();
        false
    }

    fn apply_god(&mut self, want: bool) {
        let Some(pm) = self.pm.clone() else { return };
        let players = self.players.lock().map(|p| p.clone()).unwrap_or_default();
        for p in &players {
            if p.hp == 0 {
                continue;
            }
            if want {
                // 保险1：写游戏原生"无法受伤"标记
                if p.uf != 0 {
                    let cur = pm.read_i32(p.uf).unwrap_or(0);
                    let _ = pm.write_i32(p.uf, cur | FLAG_CANNOT_TAKE_DAMAGE);
                }
                // 保险2：锁血兜底
                self.freezer
                    .set(p.hp, ValueType::Int32, p.god_target() as f64);
            } else {
                if p.uf != 0 {
                    let cur = pm.read_i32(p.uf).unwrap_or(0);
                    let _ = pm.write_i32(p.uf, cur & !FLAG_CANNOT_TAKE_DAMAGE);
                }
                self.freezer.remove(p.hp);
            }
        }
        if want {
            self.freezer.start(pm, 0.08);
        }
        self.status = if want {
            "无敌已开启：完全不会受伤（切回游戏即生效）".to_string()
        } else {
            "无敌已关闭，恢复正常受伤".to_string()
        };
    }

    fn apply_kill(&mut self, want: bool) {
        let Some(pm) = self.pm.clone() else { return };
        let players = self.players.lock().map(|p| p.clone()).unwrap_or_default();
        let mut lines = Vec::new();
        for p in &players {
            if p.atk == 0 {
                continue;
            }
            let orig = p.atk_val;
            let v = if want { KILL_DAMAGE } else { orig };
            let _ = pm.write_i32(p.atk, v);
            // 远程武器/弓箭的伤害走魔法属性，必须一起改
            if p.magic != 0 {
                let mv = if want { KILL_DAMAGE } else { p.magic_val };
                let _ = pm.write_i32(p.magic, mv);
            }
            lines.push(format!("{} {}→{}", p.name, orig, v));
        }
        self.status = if want {
            "秒杀已开启：近战远程都是一击必杀（切回游戏即生效）".to_string()
        } else {
            "秒杀已关闭，恢复正常伤害".to_string()
        };
        if !lines.is_empty() && want {
            self.status.push_str(&format!("（{}）", lines.join("，")));
        }
    }

    fn apply_speed(&mut self, mult: f32) {
        let Some(pm) = self.pm.clone() else { return };
        let players = self.players.lock().map(|p| p.clone()).unwrap_or_default();
        let mut lines = Vec::new();
        for p in &players {
            if p.addr == 0 {
                continue;
            }
            if (mult - 1.0).abs() < f32::EPSILON {
                self.freezer.remove(p.addr);
                let _ = pm.write_f32(p.addr, p.value);
                lines.push(format!("{}恢复正常", p.name));
            } else {
                let v = p.value * mult;
                let _ = pm.write_f32(p.addr, v);
                self.freezer.set(p.addr, ValueType::Float, v as f64);
                lines.push(format!("{} {}→{}", p.name, p.value, v));
            }
        }
        if (mult - 1.0).abs() >= f32::EPSILON {
            self.freezer.start(pm, 0.08);
        }
        self.status = format!(
            "移动速度已设置：{}（切回游戏即生效）",
            lines.join("，")
        );
    }

    fn on_god_toggle(&mut self, want: bool) {
        if self.busy {
            self.god = !want;
            self.status = "正在处理，请稍候再操作……".to_string();
            return;
        }
        self.god = want;
        self.adv_god.store(want, Ordering::SeqCst);
        if !self.ensure_players(Pending::God(want)) {
            return;
        }
        self.apply_god(want);
    }

    fn on_kill_toggle(&mut self, want: bool) {
        if self.busy {
            self.kill = !want;
            self.status = "正在处理，请稍候再操作……".to_string();
            return;
        }
        self.kill = want;
        self.adv_kill.store(want, Ordering::SeqCst);
        if !self.ensure_players(Pending::Kill(want)) {
            return;
        }
        self.apply_kill(want);
    }

    fn on_speed_change(&mut self, idx: usize) {
        if self.busy {
            self.status = "正在处理，请稍候再操作……".to_string();
            return;
        }
        let mult = SPEED_VALUES[idx];
        *self.adv_speed.lock().unwrap() = mult;
        if !self.ensure_players(Pending::Speed(mult)) {
            return;
        }
        self.apply_speed(mult);
    }

    fn on_game_speed_change(&mut self, idx: usize) {
        if self.gspeed_idx == idx {
            return;
        }
        if self.busy {
            self.gspeed_idx = self.gspeed_applied;
            self.status = "正在处理，请稍候再操作……".to_string();
            return;
        }
        let mult = SPEED_VALUES[idx];
        if (mult - 1.0).abs() < f32::EPSILON {
            if let Some(mut k) = self.speed_keeper.take() {
                k.stop();
            }
            self.gspeed_idx = 0;
            self.gspeed_applied = 0;
            let Some(pm) = self.pm.clone() else {
                self.status = "游戏速度已选「不变」".to_string();
                return;
            };
            let task_gen = self.begin_task();
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let res = actions::set_game_speed(&pm, 1.0).map_err(|e| e.0);
                let _ = tx.send(Msg::SpeedTask { task_gen, res });
            });
            return;
        }
        if !self.auto_attach("设置倍速") {
            self.gspeed_idx = self.gspeed_applied;
            return;
        }
        if let Some(mut k) = self.speed_keeper.take() {
            k.stop();
        }
        let Some(pm) = self.pm.clone() else { return };
        self.speed_keeper = Some(GameSpeedKeeper::start(
            pm,
            mult as f64,
            self.keeper_tx(),
            0.25,
        ));
        self.gspeed_idx = idx;
        self.gspeed_applied = idx;
        self.status = format!(
            "游戏速度已设为 {}：切回游戏即生效（后台自动保持）",
            SPEED_OPTS[idx]
        );
    }

    fn on_mana_toggle(&mut self, want: bool) {
        if self.busy {
            self.mana = !want;
            self.status = "正在处理，请稍候再操作……".to_string();
            return;
        }
        if !want {
            if let Some(mut k) = self.mana_keeper.take() {
                k.stop();
            }
            self.status = "最大蓝条已关闭，恢复正常消耗".to_string();
            return;
        }
        if !self.auto_attach("开启最大蓝条") {
            self.mana = false;
            return;
        }
        if self.mana_keeper.is_some() {
            return;
        }
        let Some(pm) = self.pm.clone() else { return };
        self.mana_keeper = Some(ManaKeeper::start(pm, self.keeper_tx(), 0.2));
        self.status = "最大蓝条已开启：蓝条永远是满的（切回游戏即生效）".to_string();
    }

    fn on_pickup_toggle(&mut self, want: bool) {
        if self.busy {
            self.pickup = !want;
            self.status = "正在处理，请稍候再操作……".to_string();
            return;
        }
        if !want {
            if let Some(mut k) = self.pickup_keeper.take() {
                k.stop();
            }
            let extra = if self.pickup_total > 0 {
                format!("（本次共自动拾取数值 {}）", self.pickup_total)
            } else {
                String::new()
            };
            self.status = format!("全屏拾取已关闭{}", extra);
            return;
        }
        if !self.auto_attach("开启全屏拾取") {
            self.pickup = false;
            return;
        }
        if self.pickup_keeper.is_some() {
            return;
        }
        let Some(pm) = self.pm.clone() else { return };
        self.pickup_total = 0;
        self.pickup_keeper = Some(PickupKeeper::start(pm, self.keeper_tx(), 0.3, 5.0));
        self.status = "全屏拾取已开启：场上的金币经验会自动进账（切回游戏生效）".to_string();
    }

    fn on_max_level(&mut self) {
        if self.busy {
            self.status = "正在处理，请稍候再操作……".to_string();
            return;
        }
        if !self.auto_attach("点此按钮") {
            return;
        }
        let Some(pm) = self.pm.clone() else { return };
        let task_gen = self.begin_task();
        let tx = self.tx.clone();
        self.status = "正在一键满级（稍等几秒，状态栏会显示进度）...".to_string();
        std::thread::spawn(move || {
            let tx2 = tx.clone();
            let mut cb = |lv: i32| {
                let _ = tx2.send(Msg::Status(format!("一键满级进行中：当前 {} 级……", lv)));
            };
            let res = actions::max_level_up(&pm, &mut cb, MAX_LEVEL, 400)
                .map_err(|e| e.0);
            let _ = tx.send(Msg::MaxLevel { task_gen, res });
        });
    }

    fn on_max_gold(&mut self) {
        if self.busy {
            self.status = "正在处理，请稍候再操作……".to_string();
            return;
        }
        if !self.auto_attach("点此按钮") {
            return;
        }
        let Some(pm) = self.pm.clone() else { return };
        let task_gen = self.begin_task();
        let tx = self.tx.clone();
        self.status = "正在设置满金币...".to_string();
        std::thread::spawn(move || {
            let res = actions::set_max_gold(&pm).map_err(|e| e.0);
            let _ = tx.send(Msg::MaxGold { task_gen, res });
        });
    }

    fn restore_all(&mut self, show_status: bool) {
        if self.busy {
            self.status = "正在处理，请稍候再操作……".to_string();
            return;
        }
        if let Some(mut k) = self.flag_keeper.take() {
            k.stop();
        }
        let had_speed = self.speed_keeper.is_some();
        if let Some(mut k) = self.speed_keeper.take() {
            k.stop();
        }
        if let Some(mut k) = self.mana_keeper.take() {
            k.stop();
        }
        if let Some(mut k) = self.pickup_keeper.take() {
            k.stop();
        }
        self.freezer.clear();
        if let Some(pm) = self.pm.clone() {
            for p in &self.orig {
                self.freezer.remove(p.hp);
                self.freezer.remove(p.addr);
                let _ = pm.write_f32(p.addr, p.value);
                if p.atk != 0 {
                    let _ = pm.write_i32(p.atk, p.atk_val);
                }
                if p.magic != 0 {
                    let _ = pm.write_i32(p.magic, p.magic_val);
                }
                if p.uf != 0 {
                    if let Ok(cur) = pm.read_i32(p.uf) {
                        let _ = pm.write_i32(p.uf, cur & !FLAG_CANNOT_TAKE_DAMAGE);
                    }
                }
            }
            if had_speed {
                let pm2 = pm.clone();
                std::thread::spawn(move || {
                    let _ = actions::set_game_speed(&pm2, 1.0);
                });
            }
        }
        self.god = false;
        self.kill = false;
        self.speed_idx = 0;
        self.mana = false;
        self.pickup = false;
        self.gspeed_idx = 0;
        self.gspeed_applied = 0;
        self.adv_god.store(false, Ordering::SeqCst);
        self.adv_kill.store(false, Ordering::SeqCst);
        *self.adv_speed.lock().unwrap() = 1.0;
        if show_status {
            if self.orig.is_empty() {
                self.status = "当前没有需要恢复的修改".to_string();
            } else {
                self.status = "已全部恢复原样".to_string();
            }
        }
    }

    fn on_status_refresh(&mut self) {
        // 1) 作废在跑的旧任务，立刻恢复界面响应
        self.task_gen += 1;
        self.busy = false;
        self.pending = None;
        // 2) 停掉旧自愈线程、清空旧识别结果
        if let Some(mut k) = self.flag_keeper.take() {
            k.stop();
        }
        self.players.lock().unwrap().clear();
        self.orig.clear();
        self.adv_status = "刷新中……".to_string();
        self.status = "正在刷新状态……".to_string();

        // 3) 检查游戏进程与附加是否仍然有效
        let pids = memory::windowed_game_pids();
        let Some(&pid) = pids.first() else {
            self.detach_all();
            self.status =
                "状态已刷新：未发现游戏进程，启动游戏后再点「状态刷新」".to_string();
            self.adv_status = "未识别（未发现游戏进程）".to_string();
            return;
        };
        if self.pm.is_some() && self.pid == Some(pid) {
            self.proc_status = format!("已附加 PID {}", pid);
        } else if !self.reattach(pid) {
            return;
        }

        // 4) 重新识别角色（旧任务结果已被代号机制作废）
        self.status = "状态刷新：正在重新识别角色（通常几秒）……".to_string();
        self.spawn_locate();
    }

    fn reattach(&mut self, pid: u32) -> bool {
        self.stop_all_keepers();
        self.freezer.clear();
        self.freezer.stop();
        self.pm = None;
        match ProcessMemory::new(pid) {
            Ok(pm) => {
                let pm = Arc::new(pm);
                self.pm = Some(pm.clone());
                self.pid = Some(pid);
                self.freezer.start(pm.clone(), 0.08);
                self.proc_status = format!("已附加 PID {}", pid);
                // 不依赖角色识别的三个功能：按开关状态重新拉起
                if self.mana {
                    self.mana_keeper = Some(ManaKeeper::start(pm.clone(), self.keeper_tx(), 0.2));
                }
                if self.pickup {
                    self.pickup_total = 0;
                    self.pickup_keeper =
                        Some(PickupKeeper::start(pm.clone(), self.keeper_tx(), 0.3, 5.0));
                }
                if self.gspeed_idx != 0 {
                    self.speed_keeper = Some(GameSpeedKeeper::start(
                        pm,
                        SPEED_VALUES[self.gspeed_idx] as f64,
                        self.keeper_tx(),
                        0.25,
                    ));
                }
                true
            }
            Err(e) => {
                self.pid = None;
                self.proc_status = "附加失败".to_string();
                self.status = format!("状态刷新失败：{}", truncate(&e.0, 60));
                self.dialog = Some(("附加失败".to_string(), e.0));
                false
            }
        }
    }

    fn apply_pending(&mut self) {
        if let Some(p) = self.pending.take() {
            match p {
                Pending::God(want) => self.apply_god(want),
                Pending::Kill(want) => self.apply_kill(want),
                Pending::Speed(mult) => self.apply_speed(mult),
            }
        }
    }
}

impl eframe::App for TrainerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.round_done {
            memory::round_window(APP_TITLE);
            self.round_done = true;
        }
        self.poll_messages();
        // 识别完成且有待执行操作时补生效
        if !self.busy && self.pending.is_some() {
            if !self.players.lock().map(|p| p.is_empty()).unwrap_or(true) {
                self.apply_pending();
            }
        }
        if self.busy
            || self.flag_keeper.is_some()
            || self.mana_keeper.is_some()
            || self.pickup_keeper.is_some()
            || self.speed_keeper.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(120));
        }

        egui::TopBottomPanel::bottom("status_bar")
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(0xe8, 0xec, 0xf2))
                    .inner_margin(Margin::symmetric(14, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&self.status).size(12.5).color(TEXT));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if flat_button(ui, "状态刷新", BTN_ORANGE, !self.busy) {
                            self.on_status_refresh();
                        }
                    });
                });
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(BG)
                    .inner_margin(Margin::symmetric(18, 12)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("猫咪斗恶龙2 修改器")
                            .size(22.0)
                            .strong()
                            .color(TEXT),
                    );
                    ui.label(RichText::new("2.0 Rust").size(12.0).color(SUBTEXT));
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if tab_button(ui, "一键功能", self.tab == 0) {
                        self.tab = 0;
                    }
                    if tab_button(ui, "强迫症修改", self.tab == 1) {
                        self.tab = 1;
                    }
                    if tab_button(ui, "原理说明", self.tab == 2) {
                        self.tab = 2;
                    }
                });
                ui.add_space(10.0);
                egui::ScrollArea::vertical().show(ui, |ui| {
                    match self.tab {
                        0 => self.page_main(ui),
                        1 => self.page_advanced(ui),
                        _ => page_help(ui),
                    }
                    ui.add_space(16.0);
                });
            });

        if let Some((title, msg)) = self.dialog.clone() {
            let mut close = false;
            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label(msg);
                    ui.add_space(8.0);
                    if ui.button("确定").clicked() {
                        close = true;
                    }
                });
            if close {
                self.dialog = None;
            }
        }
    }
}

impl Drop for TrainerApp {
    fn drop(&mut self) {
        self.stop_all_keepers();
        self.freezer.clear();
        self.freezer.stop();
        if let Some(pm) = self.pm.clone() {
            std::thread::spawn(move || {
                let _ = actions::set_game_speed(&pm, 1.0);
            });
        }
    }
}

// ---------------- 页面 ----------------
impl TrainerApp {
    fn page_main(&mut self, ui: &mut egui::Ui) {
        let mut connect = false;
        card(
            ui,
            "游戏进程",
            "只认带可见窗口的真进程，自动跳过崩溃残留的僵尸进程",
            |ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(&self.proc_status).size(12.0).color(SUBTEXT));
                    ui.add_space(4.0);
                    if flat_button(ui, "连接并识别角色", BTN_BLUE, !self.busy) {
                        connect = true;
                    }
                });
            },
        );
        if connect {
            self.on_connect();
        }

        let mut god = self.god;
        card(
            ui,
            "无敌",
            "写入游戏原生「无法受伤」标记 + 锁血兜底，换场景后自动修复",
            |ui| {
                if pill_switch(ui, &mut god).changed() {
                    // 由下方统一处理
                }
            },
        );
        if god != self.god {
            self.on_god_toggle(god);
        }

        let mut kill = self.kill;
        card(
            ui,
            "秒杀",
            "近战与远程伤害（魔法属性）同时拉满，一击必杀且不溢出",
            |ui| {
                pill_switch(ui, &mut kill);
            },
        );
        if kill != self.kill {
            self.on_kill_toggle(kill);
        }

        let mut idx = self.speed_idx;
        let mut changed = false;
        card(
            ui,
            "移动速度",
            "按原始移速等比放大，后台持续锁住；组件换新时自动补写",
            |ui| {
                changed = segmented(ui, &SPEED_OPTS, &mut idx);
            },
        );
        if changed {
            self.speed_idx = idx;
            self.on_speed_change(idx);
        }

        let mut want_level = false;
        card(
            ui,
            "一键满级",
            "注入经验让游戏自己的升级流程一路升到 999 级（属性/存档同步）",
            |ui| {
                if flat_button(ui, "一键满级", BTN_GREEN, !self.busy) {
                    want_level = true;
                }
            },
        );
        if want_level {
            self.on_max_level();
        }

        let mut want_gold = false;
        card(
            ui,
            "一键满金币",
            "调用游戏自己的 ReplaceGold，金币直接到 20 亿且正常存档",
            |ui| {
                if flat_button(ui, "一键满金币", BTN_GREEN, !self.busy) {
                    want_gold = true;
                }
            },
        );
        if want_gold {
            self.on_max_gold();
        }

        ui.add_space(4.0);
        if flat_button(ui, "全部恢复原样", BTN_RED, !self.busy) {
            self.restore_all(true);
        }
        ui.add_space(4.0);
        ui.label(
            RichText::new(&self.adv_status)
                .size(12.0)
                .color(SUBTEXT),
        );
    }

    fn page_advanced(&mut self, ui: &mut egui::Ui) {
        let mut mana = self.mana;
        card(
            ui,
            "最大蓝条",
            "持续把蓝量顶在属性上限；走游戏 ReplaceMana 触发 HUD 刷新",
            |ui| {
                pill_switch(ui, &mut mana);
            },
        );
        if mana != self.mana {
            self.mana = mana;
            self.on_mana_toggle(mana);
        }

        let mut gidx = self.gspeed_idx;
        let mut changed = false;
        card(
            ui,
            "游戏整体速度",
            "Unity Time.timeScale：移动、动画、战斗全部按倍速缩放",
            |ui| {
                changed = segmented(ui, &SPEED_OPTS, &mut gidx);
            },
        );
        if changed {
            self.on_game_speed_change(gidx);
        }

        let mut pickup = self.pickup;
        card(
            ui,
            "全屏拾取",
            "后台巡逻场上掉落物，掉落即捡；音效与入账全由游戏完成",
            |ui| {
                pill_switch(ui, &mut pickup);
            },
        );
        if pickup != self.pickup {
            self.pickup = pickup;
            self.on_pickup_toggle(pickup);
        }

        ui.add_space(4.0);
        if flat_button(ui, "全部恢复原样", BTN_RED, !self.busy) {
            self.restore_all(true);
        }
        ui.add_space(10.0);
        ui.label(
            RichText::new("提示：这里的开关不依赖角色识别，附加游戏进程后即可使用。")
                .size(12.0)
                .color(SUBTEXT),
        );
    }
}

fn page_help(ui: &mut egui::Ui) {
    let sections: [(&str, &str); 7] = [
        (
            "它是怎么找到角色的",
            "先用 Mono 运行时接口直接拿到游戏上下文：mono_get_root_domain → \
             Assembly-CSharp 镜像 → Contexts.GameEntity → GetEntities → \
             isPlayerControlled 过滤，几秒内就能枚举玩家实体。\
             快速通道失败时才退回全堆扫描：先找移速 FloatStat（base 2.2~25），\
             再反查 FloatStat 引用找到 StatsComponent，最后反查 StatsComponent \
             引用找到实体组件表里的槽位，按固定槽位偏移做类名校验。",
        ),
        (
            "实体槽位校验",
            "实体 = 按类名字母序排列的组件指针表。玩家必须同时满足：\
             StatsComponent（0）、PlayerControlledComponent（-0x160）、\
             HealthComponent（-0x260）、MoveSpeedComponent（-0x1B0），\
             同伴还会多一个 AIPlayerComponent（-0x3D0）。\
             这套校验即 resolve_class_name + _extract_player 的移植口径。",
        ),
        (
            "Mono 类名解析",
            "对象头 vtable → MonoVTable.klass → MonoClass.name。\
             不同 Mono 版本 MonoClass 字段偏移差异很大（name 大致在 0x48~0xA0），\
             所以以 8 字节步长逐偏移试探，取第一个能读出合法 ASCII 标识符的地址。",
        ),
        (
            "远程调用怎么执行",
            "在游戏进程里分配 RWX 页，写入手写 x64 stub：sub rsp,0x28（对齐 + \
             shadow space）→ 逐个 mov 参数寄存器 → call r10 → mov [slot],rax → \
             add rsp,0x28 → ret。用 CreateRemoteThread 执行并等待，再读回槽位。\
             这样不依赖游戏自带的 DLL，也不用注入模块。",
        ),
        (
            "为什么会「开一会儿就失效」",
            "游戏在翻滚/冲刺/倒地/换人时会 Replace 整个 UnitFlagsComponent，\
             一次性写入的免伤标记随旧对象作废。所以后台每 0.5 秒重读槽位链路，\
             对象被换新就跟着改地址并补写标记；组件换新时还会把秒杀/移速写回新地址。",
        ),
        (
            "数值约定细节",
            "float32 与 float64 严格区分；扫描按 8 字节对齐；命中上限 500000；\
             内存按 32MB 分块读取并保留 pattern 长度-1 的重叠，保证跨块命中；\
             int 参数用「地址 == 值」的自引用缓冲，兼容直接传值/传指针/指针数组三种 Mono 约定。",
        ),
        (
            "安全说明",
            "本工具只读写本机单机游戏进程，不联网、不注入 DLL、不改游戏文件。\
             所有修改都走游戏自身的逻辑入口，退出前请点「全部恢复原样」。",
        ),
    ];
    for (title, body) in sections {
        egui::Frame::new()
            .fill(CARD_BG)
            .stroke(Stroke::new(1.0_f32, CARD_BD))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.label(RichText::new(title).size(15.0).strong().color(TEXT));
                ui.add_space(4.0);
                ui.label(RichText::new(body).size(12.5).color(SUBTEXT));
            });
        ui.add_space(8.0);
    }
}

// ---------------- 自绘控件 ----------------
fn pill_switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(52.0, 26.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool(resp.id, *on);
    let track = lerp_color(SWITCH_OFF, SWITCH_ON, t);
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(13), track);
    let r = 11.0;
    let x = egui::lerp((rect.left() + r + 2.0)..=(rect.right() - r - 2.0), t);
    painter.circle_filled(egui::pos2(x, rect.center().y), r, Color32::WHITE);
    resp
}

fn segmented(ui: &mut egui::Ui, values: &[&str], sel: &mut usize) -> bool {
    let font = FontId::proportional(13.0);
    let widths: Vec<f32> = values
        .iter()
        .map(|v| {
            ui.painter()
                .layout_no_wrap((*v).to_string(), font.clone(), TEXT)
                .size()
                .x
                + 22.0
        })
        .collect();
    let total = widths.iter().sum::<f32>() + 8.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(total, 28.0), Sense::click());
    let mut changed = false;
    if resp.clicked() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let mut x = rect.left() + 4.0;
            for (i, w) in widths.iter().enumerate() {
                if pos.x >= x && pos.x <= x + w {
                    if *sel != i {
                        *sel = i;
                        changed = true;
                    }
                    break;
                }
                x += w;
            }
        }
    }
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(8), Color32::from_rgb(0xe8, 0xed, 0xf4));
    let mut x = rect.left() + 4.0;
    for (i, w) in widths.iter().enumerate() {
        let item = egui::Rect::from_min_size(
            egui::pos2(x, rect.top() + 3.0),
            Vec2::new(*w, rect.height() - 6.0),
        );
        if i == *sel {
            painter.rect_filled(item, CornerRadius::same(6), BTN_BLUE);
        }
        painter.text(
            item.center(),
            Align2::CENTER_CENTER,
            values[i],
            font.clone(),
            if i == *sel { Color32::WHITE } else { SUBTEXT },
        );
        x += w;
    }
    changed
}

fn card(
    ui: &mut egui::Ui,
    title: &str,
    desc: &str,
    control: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::new()
        .fill(CARD_BG)
        .stroke(Stroke::new(1.0_f32, CARD_BD))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let avail = ui.available_width();
                ui.vertical(|ui| {
                    ui.set_width((avail - 190.0).max(120.0));
                    ui.label(RichText::new(title).size(15.0).strong().color(TEXT));
                    ui.add_space(2.0);
                    ui.label(RichText::new(desc).size(12.0).color(SUBTEXT));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    control(ui);
                });
            });
        });
    ui.add_space(8.0);
}

fn flat_button(ui: &mut egui::Ui, text: &str, color: Color32, enabled: bool) -> bool {
    let btn = egui::Button::new(RichText::new(text).size(14.0).color(Color32::WHITE))
        .fill(if enabled {
            color
        } else {
            Color32::from_rgb(0xb6, 0xbd, 0xc7)
        })
        .corner_radius(CornerRadius::same(8))
        .min_size(Vec2::new(0.0, 30.0));
    ui.add_enabled(enabled, btn).clicked()
}

fn tab_button(ui: &mut egui::Ui, text: &str, selected: bool) -> bool {
    let btn = egui::Button::new(
        RichText::new(text)
            .size(14.0)
            .color(if selected { Color32::WHITE } else { TEXT }),
    )
    .fill(if selected { BTN_BLUE } else { Color32::WHITE })
    .stroke(Stroke::new(
        1.0_f32,
        if selected { BTN_BLUE } else { CARD_BD },
    ))
    .corner_radius(CornerRadius::same(8))
    .min_size(Vec2::new(96.0, 30.0));
    ui.add(btn).clicked()
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    Color32::from_rgb(
        egui::lerp(a.r() as f32..=b.r() as f32, t) as u8,
        egui::lerp(a.g() as f32..=b.g() as f32, t) as u8,
        egui::lerp(a.b() as f32..=b.b() as f32, t) as u8,
    )
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    for path in [
        "C:/Windows/Fonts/msyh.ttc",
        "C:/Windows/Fonts/msyh.ttf",
        "C:/Windows/Fonts/simhei.ttf",
    ] {
        if let Ok(bytes) = fs::read(path) {
            fonts
                .font_data
                .insert("cjk".to_string(), Arc::new(FontData::from_owned(bytes)));
            fonts
                .families
                .entry(FontFamily::Proportional)
                .or_default()
                .insert(0, "cjk".to_string());
            fonts
                .families
                .entry(FontFamily::Monospace)
                .or_default()
                .push("cjk".to_string());
            break;
        }
    }
    ctx.set_fonts(fonts);
}

fn apply_style(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::light();
    visuals.panel_fill = BG;
    visuals.window_fill = CARD_BG;
    visuals.override_text_color = Some(TEXT);
    visuals.selection.bg_fill = BTN_BLUE;
    visuals.widgets.inactive.bg_fill = Color32::WHITE;
    visuals.widgets.inactive.weak_bg_fill = Color32::WHITE;
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(0xe8, 0xf0, 0xff);
    visuals.widgets.active.bg_fill = Color32::from_rgb(0xda, 0xe7, 0xff);
    ctx.set_visuals(visuals);
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(10.0, 5.0);
    ctx.set_style(style);
}

fn truncate(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

fn comma(n: i32) -> String {
    let neg = n < 0;
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if neg {
        out.insert(0, '-');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_table() {
        assert_eq!(SPEED_OPTS.len(), SPEED_VALUES.len());
        assert_eq!(SPEED_VALUES[0], 1.0);
        assert_eq!(SPEED_VALUES[5], 10.0);
    }

    #[test]
    fn comma_format() {
        assert_eq!(comma(1_234_567), "1,234,567");
        assert_eq!(comma(12), "12");
        assert_eq!(comma(-1000), "-1,000");
    }

    #[test]
    fn truncate_utf8_safe() {
        assert_eq!(truncate("一二三四", 2), "一二…");
        assert_eq!(truncate("abc", 5), "abc");
    }
}

