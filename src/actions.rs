// 一次性动作：一键满级、一键满金币、游戏整体倍速
use std::sync::Arc;
use std::time::Duration;

use crate::locate::exception_class_name;
use crate::memory::{err, ProcessMemory, Result, TrainerError};
use crate::mono::{Arg, MonoApi, RemoteCall};

pub const MAX_LEVEL: i32 = 999;
pub const KILL_DAMAGE: i32 = 1_000_000;

/// "地址 == 值"的自引用参数候选（都正好落在页内偏移 0x400，且是正整数）。
/// 0x77359400 = 2000000000 恰好等于期望传入的经验值。
pub const MAGIC_CANDIDATES: [u64; 6] = [
    0x77359400,
    0x60000400,
    0x50000400,
    0x40000400,
    0x30000400,
    0x20000400,
];

fn check_exc(api: &mut MonoApi, context: &str) -> Result<()> {
    let e = api.exc_name();
    if e.is_empty() {
        Ok(())
    } else {
        err(format!("{}（{}）", context, e))
    }
}

/// 解析 Time 类与 set_timeScale 方法（每次现解析，不复用旧地址）。
pub fn resolve_timescale(api: &mut MonoApi) -> Result<u64> {
    let s_asm = api.rc.write_str("UnityEngine.CoreModule")?;
    let s_ns = api.rc.write_str("UnityEngine")?;
    let s_cls = api.rc.write_str("Time")?;
    let s_set = api.rc.write_str("set_timeScale")?;
    let f_root = api.f.get_root_domain as u64;
    let f_attach = api.f.thread_attach as u64;
    let f_image = api.f.image_loaded as u64;
    let f_detach = api.f.thread_detach as u64;
    let r = api.rc.run_sequence(
        &[
            (f_root, vec![]),
            (f_attach, vec![Arg::Slot(0)]),
            (f_image, vec![Arg::Imm(s_asm as u64)]),
            (f_detach, vec![Arg::Slot(1)]),
        ],
        15000,
    )?;
    let domain = r[0];
    let image = r[2];
    if domain == 0 || image == 0 {
        return err("游戏脚本尚未加载完成，请进入游戏画面后再设置倍速");
    }
    let f_cfn = api.f.class_from_name as u64;
    let r = api.seq(&[(
        f_cfn,
        vec![Arg::Imm(image), Arg::Imm(s_ns as u64), Arg::Imm(s_cls as u64)],
    )])?;
    let klass = r[1];
    if klass == 0 {
        return err("未找到游戏的时间逻辑（游戏版本可能不匹配）");
    }
    let f_getm = api.f.class_get_method_from_name as u64;
    let r = api.seq(&[(
        f_getm,
        vec![Arg::Imm(klass), Arg::Imm(s_set as u64), Arg::Imm(1)],
    )])?;
    let m_set = r[1];
    if m_set == 0 {
        return err("未找到游戏的倍速设置入口（游戏版本可能不匹配）");
    }
    // 后续调用都使用 timeScale 方法，这里把 domain 换成同一域
    api.domain = domain;
    Ok(m_set)
}

/// 分配"地址 == float 位模式"的参数缓冲，返回位模式（同时作参数地址）。
pub fn alloc_scale_magic(api: &mut MonoApi, scale: f64) -> Result<u64> {
    let bits = (scale as f32).to_bits() as u64;
    if bits & 0xFFF != 0 {
        return err("该倍数无法设置（请选界面上提供的档位）");
    }
    let p = api
        .pm
        .alloc_ex(Some(bits as usize), 0x1000, false)
        .ok_or_else(|| TrainerError::new("倍速参数缓冲被占用，请稍后再试"))?;
    if p as u64 != bits {
        api.pm.free_ex(p);
        return err("倍速参数缓冲被占用，请稍后再试");
    }
    api.rc.register(p);
    api.pm.write_u64(bits as usize, bits)?;
    Ok(bits)
}

/// 一次性把 Time.timeScale 设为指定倍数（1.0 = 恢复正常速度）。
pub fn set_game_speed(pm: &Arc<ProcessMemory>, scale: f64) -> Result<()> {
    let mut api = MonoApi::open(pm.clone())?;
    let domain = api.domain;
    let m_set = resolve_timescale(&mut api)?;
    let bits = alloc_scale_magic(&mut api, scale)?;
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
        return err("游戏内部设置倍速时发生异常");
    }
    api.domain = domain;
    Ok(())
}

/// 分配"槽位地址 == 槽位值"的 int 参数缓冲，返回槽位地址（失败返回 0）。
/// mono_runtime_invoke 对值类型参数的传法在不同 Mono 实现里有三种：
/// 直接放值 / 放指向值的指针 / 指针数组。把地址和值做成同一个数，
/// 三种解释读到的整数完全相同 —— 无论游戏用哪种约定都不会传错参。
pub fn alloc_selfref_int(pm: &ProcessMemory, rc: &mut RemoteCall, value: u64) -> u64 {
    let page = value & !0xFFF;
    let Some(p) = pm.alloc_ex(Some(page as usize), 0x1000, false) else {
        return 0;
    };
    if p as u64 & !0xFFF != page {
        return 0; // 该地址被占用，换候选
    }
    if pm.write_u64(value as usize, value).is_err() {
        pm.free_ex(p);
        return 0;
    }
    rc.register(p);
    value
}

struct LevelState {
    level: i32,
    exp: Option<i32>,
    xp_obj: u64,
}

/// 一键满级：远程调用 ReplaceExperience 注入经验，让游戏自己的
/// LevelUpSystem 每帧消费经验、一路升到 cap 级。
pub fn max_level_up(
    pm: &Arc<ProcessMemory>,
    on_progress: &mut dyn FnMut(i32),
    cap: i32,
    max_rounds: usize,
) -> Result<i32> {
    let mut api = MonoApi::open(pm.clone())?;
    let image = api.image;
    let s_ns = api.rc.write_str("")?;
    let s_ctx = api.rc.write_str("Contexts")?;
    let s_gsc = api.rc.write_str("GameStateContext")?;
    let s_shared = api.rc.write_str("get_sharedInstance")?;
    let s_gstate = api.rc.write_str("get_gameState")?;
    let s_rexp = api.rc.write_str("ReplaceExperience")?;
    let s_level = api.rc.write_str("get_level")?;
    let s_xp = api.rc.write_str("get_experience")?;

    let f_cfn = api.f.class_from_name as u64;
    let r = api.seq(&[
        (
            f_cfn,
            vec![Arg::Imm(image), Arg::Imm(s_ns as u64), Arg::Imm(s_ctx as u64)],
        ),
        (
            f_cfn,
            vec![Arg::Imm(image), Arg::Imm(s_ns as u64), Arg::Imm(s_gsc as u64)],
        ),
    ])?;
    let (klass_ctx, klass_gsc) = (r[1], r[2]);
    if klass_ctx == 0 || klass_gsc == 0 {
        return err("未找到游戏的等级逻辑（游戏版本可能不匹配）");
    }
    let f_getm = api.f.class_get_method_from_name as u64;
    let r = api.seq(&[
        (
            f_getm,
            vec![Arg::Imm(klass_ctx), Arg::Imm(s_shared as u64), Arg::Imm(0)],
        ),
        (
            f_getm,
            vec![Arg::Imm(klass_ctx), Arg::Imm(s_gstate as u64), Arg::Imm(0)],
        ),
        (
            f_getm,
            vec![Arg::Imm(klass_gsc), Arg::Imm(s_rexp as u64), Arg::Imm(1)],
        ),
        (
            f_getm,
            vec![Arg::Imm(klass_gsc), Arg::Imm(s_level as u64), Arg::Imm(0)],
        ),
        (
            f_getm,
            vec![Arg::Imm(klass_gsc), Arg::Imm(s_xp as u64), Arg::Imm(0)],
        ),
    ])?;
    let (m_shared, m_gstate, m_rexp, m_level, m_xp) = (r[1], r[2], r[3], r[4], r[5]);
    if m_shared == 0 || m_gstate == 0 || m_rexp == 0 || m_level == 0 || m_xp == 0 {
        return err("未找到游戏的升级入口（游戏版本可能不匹配）");
    }

    let mut magic = 0u64;
    for cand in MAGIC_CANDIDATES {
        magic = alloc_selfref_int(pm, &mut api.rc, cand);
        if magic != 0 {
            break;
        }
    }
    if magic == 0 {
        return err("无法在游戏内分配参数缓冲区，请稍后再试");
    }

    // 取 gameState 上下文；取不到 = 不在可玩画面
    api.clear_exc()?;
    let f_invoke = api.f.runtime_invoke as u64;
    let exc = api.exc_buf as u64;
    let r = api.seq(&[
        (
            f_invoke,
            vec![
                Arg::Imm(m_shared),
                Arg::Imm(0),
                Arg::Imm(0),
                Arg::Imm(exc),
            ],
        ),
        (
            f_invoke,
            vec![
                Arg::Imm(m_gstate),
                Arg::Slot(1),
                Arg::Imm(0),
                Arg::Imm(exc),
            ],
        ),
    ])?;
    check_exc(&mut api, "游戏内部读取游戏状态时发生异常")?;
    let ctx = r[2];
    if ctx == 0 {
        return err("当前读不到游戏状态，请进入游戏画面（可操作角色）后再试");
    }

    let read_state = |api: &mut MonoApi, ctx: u64| -> Result<LevelState> {
        api.clear_exc()?;
        let f_invoke = api.f.runtime_invoke as u64;
        let exc = api.exc_buf as u64;
        let r = api.seq(&[
            (
                f_invoke,
                vec![
                    Arg::Imm(m_level),
                    Arg::Imm(ctx),
                    Arg::Imm(0),
                    Arg::Imm(exc),
                ],
            ),
            (
                f_invoke,
                vec![Arg::Imm(m_xp), Arg::Imm(ctx), Arg::Imm(0), Arg::Imm(exc)],
            ),
        ])?;
        check_exc(api, "游戏内部读取等级时发生异常")?;
        let (lvl_obj, xp_obj) = (r[1], r[2]);
        if lvl_obj == 0 {
            return err("读不到等级数据，请确认已进入游戏画面后再试");
        }
        let level = api.pm.read_i32(lvl_obj as usize + 0x10)?;
        if !(0..=100_000).contains(&level) {
            return err("等级数据异常，请重试");
        }
        let exp = if xp_obj != 0 {
            api.pm.read_i32(xp_obj as usize + 0x10).ok()
        } else {
            None
        };
        Ok(LevelState {
            level,
            exp,
            xp_obj,
        })
    };

    let state = read_state(&mut api, ctx)?;
    let mut level = state.level;
    on_progress(level);

    let mut stall = 0usize;
    let mut rounds = 0usize;
    while level < cap && rounds < max_rounds {
        rounds += 1;
        api.clear_exc()?;
        let f_invoke = api.f.runtime_invoke as u64;
        let exc = api.exc_buf as u64;
        api.seq(&[(
            f_invoke,
            vec![
                Arg::Imm(m_rexp),
                Arg::Imm(ctx),
                Arg::Imm(magic),
                Arg::Imm(exc),
            ],
        )])?;
        check_exc(&mut api, "游戏内部执行升级时发生异常")?;
        std::thread::sleep(Duration::from_millis(60)); // 留出几个逻辑帧消费经验
        let new_level = read_state(&mut api, ctx)?.level;
        on_progress(new_level);
        if new_level > level {
            stall = 0;
        } else {
            stall += 1;
            if stall >= 40 {
                return err(format!(
                    "升级没有推进（当前 {} 级）：请确认游戏画面正常（不在暂停或主菜单）后重新点一次",
                    level
                ));
            }
        }
        level = new_level;
    }
    if level < cap {
        return err(format!("尝试多轮仍未升满（当前 {} 级），请稍后再试", level));
    }

    // 收尾：清掉注入剩余的经验，避免经验条残留大数字
    if let Ok(st) = read_state(&mut api, ctx) {
        if st.xp_obj != 0 && st.exp.unwrap_or(0) != 0 {
            let _ = pm.write_i32(st.xp_obj as usize + 0x10, 0);
        }
    }
    Ok(level)
}

/// 一键满金币：远程调用游戏自己的 GameStateContext.ReplaceGold。
pub fn set_max_gold(pm: &Arc<ProcessMemory>) -> Result<(i32, bool)> {
    let mut api = MonoApi::open(pm.clone())?;
    let image = api.image;
    let s_ns = api.rc.write_str("")?;
    let s_ctx = api.rc.write_str("Contexts")?;
    let s_gsc = api.rc.write_str("GameStateContext")?;
    let s_shared = api.rc.write_str("get_sharedInstance")?;
    let s_gstate = api.rc.write_str("get_gameState")?;
    let s_regold = api.rc.write_str("ReplaceGold")?;
    let s_gold = api.rc.write_str("get_gold")?;

    let f_cfn = api.f.class_from_name as u64;
    let r = api.seq(&[
        (
            f_cfn,
            vec![Arg::Imm(image), Arg::Imm(s_ns as u64), Arg::Imm(s_ctx as u64)],
        ),
        (
            f_cfn,
            vec![Arg::Imm(image), Arg::Imm(s_ns as u64), Arg::Imm(s_gsc as u64)],
        ),
    ])?;
    let (klass_ctx, klass_gsc) = (r[1], r[2]);
    if klass_ctx == 0 || klass_gsc == 0 {
        return err("未找到游戏的金币逻辑（游戏版本可能不匹配）");
    }
    let f_getm = api.f.class_get_method_from_name as u64;
    let r = api.seq(&[
        (
            f_getm,
            vec![Arg::Imm(klass_ctx), Arg::Imm(s_shared as u64), Arg::Imm(0)],
        ),
        (
            f_getm,
            vec![Arg::Imm(klass_ctx), Arg::Imm(s_gstate as u64), Arg::Imm(0)],
        ),
        (
            f_getm,
            vec![Arg::Imm(klass_gsc), Arg::Imm(s_regold as u64), Arg::Imm(1)],
        ),
        (
            f_getm,
            vec![Arg::Imm(klass_gsc), Arg::Imm(s_gold as u64), Arg::Imm(0)],
        ),
    ])?;
    let (m_shared, m_gstate, m_regold, m_gold) = (r[1], r[2], r[3], r[4]);
    if m_shared == 0 || m_gstate == 0 || m_regold == 0 || m_gold == 0 {
        return err("未找到游戏的金币入口（游戏版本可能不匹配）");
    }

    // 金币参数用"地址==值"自引用缓冲（约定差异全兼容，不会传错参）
    let mut magic = 0u64;
    let mut target = 0i32;
    for cand in MAGIC_CANDIDATES {
        magic = alloc_selfref_int(pm, &mut api.rc, cand);
        if magic != 0 {
            target = cand as i32;
            break;
        }
    }
    if magic == 0 {
        return err("无法在游戏内分配参数缓冲区，请稍后再试");
    }

    let invoke_checked = |api: &mut MonoApi, m: u64, this: u64, arg: u64| -> Result<u64> {
        let ret = api.invoke(m, this, arg)?;
        let e = api.exc_name();
        if !e.is_empty() {
            return err(format!("游戏内部设置金币时发生异常（{}）", e));
        }
        Ok(ret)
    };

    // 取 gameState 上下文（取不到 = 没进可玩画面）
    let shared = invoke_checked(&mut api, m_shared, 0, 0)?;
    let ctx = invoke_checked(&mut api, m_gstate, shared, 0)?;
    if ctx == 0 {
        return err("当前读不到游戏状态，请进入游戏画面（可操作角色）后再试");
    }

    let read_gold = |api: &mut MonoApi, ctx: u64| -> Result<i32> {
        let obj = api.invoke(m_gold, ctx, 0)?;
        let e = api.exc_name();
        if !e.is_empty() {
            return err(format!("游戏内部读取金币时发生异常（{}）", e));
        }
        if obj == 0 {
            return err("读不到金币数据，请确认已进入游戏画面后再试");
        }
        api.pm.read_i32(obj as usize + 0x10)
    };

    let cur = read_gold(&mut api, ctx)?;
    if cur >= target {
        return Ok((cur, false)); // 金币已经不比目标少，不往回改
    }
    let _ = invoke_checked(&mut api, m_regold, ctx, magic)?;
    let got = read_gold(&mut api, ctx)?;
    if got != target {
        return err(format!("金币设置未生效（当前 {}），请稍后再试", got));
    }
    Ok((got, true))
}

/// 异常类名（保留 Python 的口径：失败返回"未知异常"）
pub fn exc_display(pm: &ProcessMemory, exc: u64) -> String {
    let name = exception_class_name(pm, exc);
    if name.is_empty() {
        String::new()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_values_pack_as_int32() {
        assert_eq!(MAGIC_CANDIDATES[0], 2_000_000_000);
        assert!(MAGIC_CANDIDATES.iter().all(|v| *v % 0x400 == 0));
        assert_eq!(KILL_DAMAGE, 1_000_000);
        assert_eq!(MAX_LEVEL, 999);
    }

    #[test]
    fn selfref_address_alignment() {
        for v in MAGIC_CANDIDATES {
            assert_eq!(v & 0xFFF, 0x400);
            assert_eq!(v & !0xFFF, v - 0x400);
        }
    }
}
