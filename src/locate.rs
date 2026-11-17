// 玩家实体识别：快速通道（Mono 内部接口）与全堆扫描两条路线
use std::collections::HashSet;
use std::sync::Arc;

use crate::memory::{err, ProcessMemory, Result};
use crate::mono::{Arg, MonoApi};
use crate::scan::{
    is_plausible_ptr, radar_scan, resolve_class_name, reverse_scan_refs, ClassReader,
    FLAG_CANNOT_TAKE_DAMAGE, SLOT_AIPLAYER_FROM_STATS, SLOT_HEALTH_FROM_STATS,
    SLOT_MOVESPEED_FROM_STATS, SLOT_PLAYERCONTROLLED_FROM_STATS, SLOT_UNITFLAGS_FROM_STATS,
    STAT_BASE_OFF,
};

/// 一个玩家的全部修改点
#[derive(Clone, Debug, Default)]
pub struct Player {
    pub name: String,
    pub addr: usize,   // 移速属性源地址
    pub value: f32,    // 原始移速
    pub comp: usize,   // MoveSpeedComponent 的值地址（游戏同步目标）
    pub ent: usize,    // 实体上存放 StatsComponent 指针的槽位地址
    pub hp: usize,     // HealthComponent 当前血量地址
    pub hp_cur: i32,
    pub hp_max: i32,
    pub atk: usize,    // 近战攻击属性源
    pub atk_val: i32,
    pub magic: usize,  // 魔法属性源（远程伤害来源）
    pub magic_val: i32,
    pub uf: usize,     // 原生免伤标记地址
    pub uf_val: i32,
}

impl Player {
    pub fn god_target(&self) -> i32 {
        if self.hp_max > 0 && self.hp_max <= 10_000_000 {
            self.hp_max
        } else {
            self.hp_cur
        }
    }
}

/// ent = 实体上「存放 StatsComponent 指针」的槽位地址；按固定槽偏移做
/// 身份校验并提取一个玩家的全部修改点，校验不过或重复返回 None。
/// （快速通道与全堆扫描共用本函数，保证两条路产出的字段完全一致）
pub fn extract_player(
    pm: &ProcessMemory,
    ent: usize,
    seen: &mut HashSet<usize>,
    cls: &mut ClassReader,
) -> Option<Player> {
    if cls.slot_cls(ent).as_deref() != Some("StatsComponent") {
        return None;
    }
    if cls
        .slot_cls(ent.wrapping_sub(SLOT_PLAYERCONTROLLED_FROM_STATS))
        .as_deref()
        != Some("PlayerControlledComponent")
    {
        return None;
    }
    if cls
        .slot_cls(ent.wrapping_sub(SLOT_HEALTH_FROM_STATS))
        .as_deref()
        != Some("HealthComponent")
    {
        return None;
    }
    if cls
        .slot_cls(ent.wrapping_sub(SLOT_MOVESPEED_FROM_STATS))
        .as_deref()
        != Some("MoveSpeedComponent")
    {
        return None;
    }

    let sc = pm.read_u64(ent).ok()? as usize;
    let fs = pm.read_u64(sc + 0x40).ok()? as usize; // moveSpeed 字段
    let addr = fs.wrapping_add(STAT_BASE_OFF); // 移速属性源
    if seen.contains(&addr) {
        return None;
    }
    let value = pm.read_f32(addr).ok()?;
    // 血量(hp+0x10)/伤害(+0x20)/魔法(+0x28，远程伤害来源) 三个 IntStat 的属性源
    let hp_obj = pm.read_u64(sc + 0x10).ok()? as usize;
    let atk_obj = pm.read_u64(sc + 0x20).ok()? as usize;
    let magic_obj = pm.read_u64(sc + 0x28).ok()? as usize;
    let hp_src = if cls.cls(hp_obj).as_deref() == Some("IntStat") {
        hp_obj + STAT_BASE_OFF
    } else {
        0
    };
    let atk_src = if cls.cls(atk_obj).as_deref() == Some("IntStat") {
        atk_obj + STAT_BASE_OFF
    } else {
        0
    };
    let magic_src = if cls.cls(magic_obj).as_deref() == Some("IntStat") {
        magic_obj + STAT_BASE_OFF
    } else {
        0
    };
    let hp_max = if hp_src != 0 {
        pm.read_i32(hp_src).ok()?
    } else {
        0
    };
    let atk_val = if atk_src != 0 {
        pm.read_i32(atk_src).ok()?
    } else {
        0
    };
    let magic_val = if magic_src != 0 {
        pm.read_i32(magic_src).ok()?
    } else {
        0
    };
    // 当前血量：Health 槽存对象指针，值在对象 +0x10
    let hp_obj2 = pm
        .read_u64(ent.wrapping_sub(SLOT_HEALTH_FROM_STATS))
        .ok()? as usize;
    let hp_cur = pm.read_i32(hp_obj2 + 0x10).ok()?;
    let comp_obj = pm
        .read_u64(ent.wrapping_sub(SLOT_MOVESPEED_FROM_STATS))
        .ok()? as usize;
    let comp = comp_obj + 0x10; // 游戏同步目标
    // 原生免伤标记：UnitFlagsComponent.value（+0x10）
    let uf_obj = pm
        .read_u64(ent.wrapping_add(SLOT_UNITFLAGS_FROM_STATS))
        .ok()? as usize;
    let uf_addr = if cls.cls(uf_obj).as_deref() == Some("UnitFlagsComponent") {
        uf_obj + 0x10
    } else {
        0
    };
    let uf_val = if uf_addr != 0 {
        pm.read_i32(uf_addr).ok()?
    } else {
        0
    };

    seen.insert(addr);
    let is_ai = cls
        .slot_cls(ent.wrapping_sub(SLOT_AIPLAYER_FROM_STATS))
        .as_deref()
        == Some("AIPlayerComponent");
    Some(Player {
        name: if is_ai { "同伴(AI)" } else { "主角" }.to_string(),
        addr,
        value,
        comp,
        ent,
        hp: hp_obj2 + 0x10,
        hp_cur,
        hp_max,
        atk: atk_src,
        atk_val,
        magic: magic_src,
        magic_val,
        uf: uf_addr,
        uf_val,
    })
}

/// 在实体上找出「存放组件指针 sc 的槽位地址」。
/// Entitas 的组件表是实体的一个 _components 数组字段（不在实体对象内），
/// 所以先在实体本体直接找，找不到再逐字段跟一层进数组里找。
pub fn find_component_slot(pm: &ProcessMemory, ent_obj: usize, sc: usize) -> Vec<usize> {
    let needle = sc.to_le_bytes();
    let mut hits = Vec::new();
    let mut off = 0usize;
    while off < 0xA00 {
        match pm.read(ent_obj + off, 8) {
            Ok(raw) if raw == needle => hits.push(ent_obj + off),
            Ok(_) => {}
            Err(_) => break,
        }
        off += 8;
    }
    if !hits.is_empty() {
        return hits;
    }

    let mut off = 0usize;
    while off < 0x100 {
        let p = match pm.read_u64(ent_obj + off) {
            Ok(v) => v as usize,
            Err(_) => break,
        };
        if is_plausible_ptr(p as u64) {
            for aoff in [0x18usize, 0x20, 0x10, 0x28] {
                let n = match pm.read_u64(p + aoff - 8) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if !(1..10000).contains(&n) {
                    continue;
                }
                let blob = match pm.read(p + aoff, 8 * n as usize) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let mut idx = 0usize;
                while idx + 8 <= blob.len() {
                    let k = match blob[idx..].windows(8).position(|w| w == needle) {
                        Some(k) => idx + k,
                        None => break,
                    };
                    if k % 8 == 0 {
                        hits.push(p + aoff + k);
                    }
                    idx = k + 8;
                }
            }
        }
        if !hits.is_empty() {
            break;
        }
        off += 8;
    }
    hits
}

/// 快速识别：直连游戏自身的 Contexts/GameEntity 接口枚举玩家实体，
/// 再在实体对象的组件槽里定位属性组件 —— 完全跳过全堆扫描，正常 1~3 秒。
pub fn locate_via_mono(
    pm: &Arc<ProcessMemory>,
    on_progress: &mut dyn FnMut(&str),
) -> Result<Vec<Player>> {
    on_progress("通过游戏内部接口识别角色（约1秒）……");
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
        return err("未找到角色逻辑");
    }
    let m_shared = api.find_method(k_ctx, s_shared, 0)?;
    let (shared, e) = api.invoke_checked(m_shared, 0, 0)?;
    if !e.is_empty() || shared == 0 {
        return err("读不到游戏上下文");
    }
    let k_shared = api.object_class(shared)?;
    let m_game = api.find_method(k_shared, s_game, 0)?;
    let (game, e) = api.invoke_checked(m_game, shared, 0)?;
    if !e.is_empty() || game == 0 {
        return err("读不到游戏状态");
    }
    let k_game = api.object_class(game)?;
    let m_ents = api.find_method(k_game, s_ents, 0)?;
    let m_haspc = api.find_method(k_ge, s_haspc, 0)?;
    let m_stats = api.find_method(k_ge, s_stats, 0)?;
    if m_ents == 0 || m_haspc == 0 || m_stats == 0 {
        return err("未找到角色接口");
    }

    // ---- 枚举全部实体并批量过滤出玩家 ----
    let (arr, e) = api.invoke_checked(m_ents, game, 0)?;
    if !e.is_empty() || arr == 0 {
        return err("读不到实体列表");
    }
    let mut cls = ClassReader::new(pm);
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
        if cls.cls(first as usize).as_deref() != Some("GameEntity") {
            continue;
        }
        if n >= 2 {
            let second = match pm.read_u64(arr as usize + off + 8) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if cls.cls(second as usize).as_deref() != Some("GameEntity") {
                continue;
            }
        }
        data_off = off;
        count = n as usize;
        break;
    }
    if data_off == 0 {
        return err("实体列表格式变了");
    }

    let mut players: Vec<u64> = Vec::new();
    let f_invoke = api.f.runtime_invoke as u64;
    let exc_buf = api.exc_buf as u64;
    let mut i = 0usize;
    while i < count {
        let end = (i + 50).min(count);
        let ents: Vec<u64> = (i..end)
            .map(|j| pm.read_u64(arr as usize + data_off + 8 * j).unwrap_or(0))
            .collect();
        api.clear_exc()?;
        let steps: Vec<(u64, Vec<Arg>)> = ents
            .iter()
            .map(|&ep| {
                (
                    f_invoke,
                    vec![
                        Arg::Imm(m_haspc),
                        Arg::Imm(ep),
                        Arg::Imm(0),
                        Arg::Imm(exc_buf),
                    ],
                )
            })
            .collect();
        let r2 = api.seq(&steps)?;
        for j in 0..ents.len() {
            if api.is_true(r2[1 + j]) {
                players.push(ents[j]);
            }
        }
        if players.len() >= 2 {
            break;
        }
        i += 50;
    }
    if players.is_empty() {
        return err("当前画面没有可操作角色，请进入游戏世界后再连接");
    }

    // ---- 每个玩家：get_stats -> 在实体对象里定位 Stats 槽 -> 提取 ----
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for ep in players {
        let (sc, e) = api.invoke_checked(m_stats, ep, 0)?;
        if !e.is_empty() || sc == 0 {
            continue;
        }
        for ent in find_component_slot(pm, ep as usize, sc as usize) {
            if let Some(p) = extract_player(pm, ent, &mut seen, &mut cls) {
                out.push(p);
                break;
            }
        }
    }
    if out.is_empty() {
        return err("没解析出玩家属性（可能游戏版本不匹配）");
    }
    Ok(out)
}

/// 定位玩家（主角/同伴）的三个"属性源"地址 —— 血量/伤害/移速的 Stat.baseValue。
/// 快速通道失败时自动退回全堆扫描。
pub fn locate_player_speed_sources(
    pm: &Arc<ProcessMemory>,
    on_progress: &mut dyn FnMut(&str),
) -> Result<Vec<Player>> {
    // 快速通道：直连游戏实体接口，秒级完成
    if let Ok(players) = locate_via_mono(pm, on_progress) {
        return Ok(players);
    }
    on_progress("快速识别未成功，改用全堆扫描（约30秒）……");

    let mut cls = ClassReader::new(pm);

    // 1. 玩家移速属性源 FloatStat（base ≈ 2.3，base 字段在 +0x18）
    // 范围放宽到 2.2~25：覆盖界面倍速档（最高10倍≈23）
    on_progress("扫描玩家移速……");
    let cands = radar_scan(pm, crate::scan::ValueType::Float, 2.2, 25.0, None);
    let mut fs_objs: Vec<usize> = cands
        .iter()
        .filter(|&&c| c >= 0x18 && c % 8 == 0)
        .filter(|&&c| cls.cls(c - 0x18).as_deref() == Some("FloatStat"))
        .map(|&c| c - 0x18)
        .collect();
    fs_objs.sort_unstable();
    fs_objs.dedup();
    if fs_objs.is_empty() {
        return err("未找到玩家移速数据，请先进入游戏世界（可控制角色移动的画面）再试");
    }

    // 2. 反查 FloatStat 引用 -> StatsComponent（moveSpeed 字段在 +0x40）
    on_progress("定位角色属性（全堆反查，约 20~60 秒）……");
    let fs_set: HashSet<u64> = fs_objs.iter().map(|&a| a as u64).collect();
    let fs_targets: Vec<u64> = fs_objs.iter().map(|&a| a as u64).collect();
    let mut stats_objs: Vec<usize> = Vec::new();
    let mut stats_set: HashSet<usize> = HashSet::new();
    for r in reverse_scan_refs(
        pm,
        &fs_targets,
        Some(&mut |m| on_progress(m)),
        "定位角色属性",
    ) {
        let s = r.wrapping_sub(0x40);
        if stats_set.contains(&s) || cls.cls(s).as_deref() != Some("StatsComponent") {
            continue;
        }
        if let Ok(v) = pm.read_u64(s + 0x40) {
            if fs_set.contains(&v) {
                stats_set.insert(s);
                stats_objs.push(s);
            }
        }
    }
    if stats_objs.is_empty() {
        return err("未找到玩家属性组件，请先进入游戏世界再试");
    }

    // 3. 反查 StatsComponent 引用 -> 实体组件表槽位，按固定槽偏移校验玩家标签
    on_progress("识别玩家实体（再次全堆反查，约 20~60 秒）……");
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let stats_targets: Vec<u64> = stats_objs.iter().map(|&a| a as u64).collect();
    for ent in reverse_scan_refs(
        pm,
        &stats_targets,
        Some(&mut |m| on_progress(m)),
        "识别玩家实体",
    ) {
        if let Some(p) = extract_player(pm, ent, &mut seen, &mut cls) {
            out.push(p);
        }
    }
    if out.is_empty() {
        return err("未找到玩家实体，请先进入游戏世界再试");
    }
    Ok(out)
}

/// 解析异常槽里的类名（供需要异常提示的模块复用）
pub fn exception_class_name(pm: &ProcessMemory, exc: u64) -> String {
    if exc == 0 {
        return String::new();
    }
    match pm
        .read_u64(exc as usize)
        .ok()
        .and_then(|vt| resolve_class_name(pm, vt as usize))
    {
        Some(n) => n,
        None => "未知异常".to_string(),
    }
}

/// 判断一个值是否能作为玩家字段（用于单元测试/防御）
pub fn is_valid_player(p: &Player) -> bool {
    p.addr != 0 && p.hp != 0 && p.ent != 0 && p.value.is_finite()
}

pub fn flag_constant() -> i32 {
    FLAG_CANNOT_TAKE_DAMAGE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_offsets_match_python() {
        assert_eq!(SLOT_HEALTH_FROM_STATS, 0x260);
        assert_eq!(SLOT_MOVESPEED_FROM_STATS, 0x1B0);
        assert_eq!(SLOT_PLAYERCONTROLLED_FROM_STATS, 0x160);
        assert_eq!(SLOT_AIPLAYER_FROM_STATS, 0x3D0);
        assert_eq!(SLOT_UNITFLAGS_FROM_STATS, 0x50);
        assert_eq!(FLAG_CANNOT_TAKE_DAMAGE, 0x40000);
        assert_eq!(STAT_BASE_OFF, 0x18);
    }

    #[test]
    fn god_target_falls_back_to_current_hp() {
        let mut p = Player::default();
        p.hp_max = 100;
        p.hp_cur = 50;
        assert_eq!(p.god_target(), 100);
        p.hp_max = 0;
        assert_eq!(p.god_target(), 50);
        p.hp_max = 20_000_000;
        assert_eq!(p.god_target(), 50);
    }
}
