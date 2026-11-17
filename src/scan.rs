// 内存扫描、Mono 类名解析、组件聚类、全堆反查
use std::collections::{HashMap, HashSet};

use crate::memory::{err, ProcessMemory, Result, CHUNK, MAX_HITS};

// 实体槽位布局（实机 dump 确认）：实体 = "按类名字母序排列的组件指针表"
pub const SLOT_HEALTH_FROM_STATS: usize = 0x260;
pub const SLOT_MOVESPEED_FROM_STATS: usize = 0x1B0;
pub const SLOT_PLAYERCONTROLLED_FROM_STATS: usize = 0x160;
pub const SLOT_AIPLAYER_FROM_STATS: usize = 0x3D0;
pub const SLOT_UNITFLAGS_FROM_STATS: usize = 0x50;

// 游戏原生"无法受伤"标记（IL 实锤：DoDamage 会先检查 HasUnitFlag(262144)）
pub const FLAG_CANNOT_TAKE_DAMAGE: i32 = 0x40000;
// Stat<T> 的属性源字段偏移（base=+0x18、min=+0x1C、max=+0x20）
pub const STAT_BASE_OFF: usize = 0x18;

/// 支持扫描的数据类型
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueType {
    Int32,
    Int64,
    Float,
    Double,
}

impl ValueType {
    pub fn size(self) -> usize {
        match self {
            ValueType::Int32 | ValueType::Float => 4,
            ValueType::Int64 | ValueType::Double => 8,
        }
    }

    pub fn pack(self, v: f64) -> Vec<u8> {
        match self {
            ValueType::Int32 => (v as i32).to_le_bytes().to_vec(),
            ValueType::Int64 => (v as i64).to_le_bytes().to_vec(),
            ValueType::Float => (v as f32).to_le_bytes().to_vec(),
            ValueType::Double => v.to_le_bytes().to_vec(),
        }
    }

    pub fn unpack(self, raw: &[u8]) -> Option<f64> {
        Some(match self {
            ValueType::Int32 => i32::from_le_bytes(raw.get(..4)?.try_into().ok()?) as f64,
            ValueType::Int64 => i64::from_le_bytes(raw.get(..8)?.try_into().ok()?) as f64,
            ValueType::Float => f32::from_le_bytes(raw.get(..4)?.try_into().ok()?) as f64,
            ValueType::Double => f64::from_le_bytes(raw.get(..8)?.try_into().ok()?),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefineMode {
    Value,
    Unchanged,
    Changed,
    Increased,
    Decreased,
}

/// 在 haystack 的 [start, end) 范围内查找 needle，返回起始下标。
fn find_from(haystack: &[u8], needle: &[u8], start: usize, end: usize) -> Option<usize> {
    if needle.is_empty() || end == 0 || start >= end {
        return None;
    }
    let end = end.min(haystack.len());
    if end < needle.len() || haystack.len() < needle.len() {
        return None;
    }
    let last = end - needle.len();
    let mut i = start;
    while i <= last {
        if &haystack[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// 在目标进程可写区域中精确搜索字节模式，返回命中的地址列表。
/// 分块读取时留出 pattern 长度-1 的重叠，避免模式跨块时漏检。
pub fn scan_exact(
    pm: &ProcessMemory,
    pattern: &[u8],
    mut on_progress: Option<&mut dyn FnMut(usize)>,
) -> Result<(Vec<usize>, bool)> {
    if pattern.is_empty() {
        return err("搜索值不能为空");
    }
    let plen = pattern.len();
    let mut hits: Vec<usize> = Vec::new();
    let mut truncated = false;
    let mut scanned = 0usize;
    'regions: for (base, size) in pm.writable_regions() {
        let mut off = 0usize;
        while off < size {
            let n = CHUNK.min(size - off);
            let more = off + n < size;
            let want = n + if more { plen - 1 } else { 0 };
            let data = match pm.read(base + off, want) {
                Ok(d) => d,
                Err(_) => {
                    // 个别页读失败就跳过（可能被系统回收）
                    off += n;
                    continue;
                }
            };
            if data.len() < plen {
                off += n;
                continue;
            }
            // 本块只接受"起始于 [0, n)"的命中，避免与下一块重复
            let limit = n.min(data.len() - plen + 1);
            let mut pos = 0usize;
            while pos < limit {
                match find_from(&data, pattern, pos, limit + plen - 1) {
                    Some(i) if i < limit => {
                        hits.push(base + off + i);
                        if hits.len() >= MAX_HITS {
                            truncated = true;
                            break;
                        }
                        pos = i + 1;
                    }
                    _ => break,
                }
            }
            if truncated {
                break 'regions;
            }
            off += n;
            scanned += n;
        }
        if let Some(cb) = on_progress.as_mut() {
            if scanned % (128 * 1024 * 1024) < CHUNK {
                cb(scanned);
            }
        }
    }
    Ok((hits, truncated))
}

/// 再次扫描。history: {地址: 上次读到的原始字节}；相等类比较直接比字节。
pub fn refine_with_history(
    pm: &ProcessMemory,
    history: &HashMap<usize, Vec<u8>>,
    vtype: ValueType,
    mode: RefineMode,
    ref_value: Option<f64>,
) -> HashMap<usize, Vec<u8>> {
    let size = vtype.size();
    let ref_bytes = if mode == RefineMode::Value {
        ref_value.map(|v| vtype.pack(v))
    } else {
        None
    };
    let mut out = HashMap::new();
    for (&a, old) in history {
        let raw = match pm.read(a, size) {
            Ok(r) if r.len() == size => r,
            _ => continue,
        };
        let keep = match mode {
            RefineMode::Value => ref_bytes.as_ref().map(|b| &raw == b).unwrap_or(false),
            RefineMode::Unchanged => &raw == old,
            RefineMode::Changed => &raw != old,
            RefineMode::Increased | RefineMode::Decreased => {
                let (Some(nv), Some(ov)) = (vtype.unpack(&raw), vtype.unpack(old)) else {
                    continue;
                };
                if mode == RefineMode::Increased {
                    nv > ov
                } else {
                    nv < ov
                }
            }
        };
        if keep {
            out.insert(a, raw);
        }
    }
    out
}

/// 范围扫描：返回值在 [vmin, vmax] 内且按类型对齐的地址列表。
pub fn radar_scan(
    pm: &ProcessMemory,
    vtype: ValueType,
    vmin: f64,
    vmax: f64,
    mut on_progress: Option<&mut dyn FnMut(usize)>,
) -> Vec<usize> {
    let esize = vtype.size();
    let mut hits = Vec::new();
    let mut scanned = 0usize;
    for (base, size) in pm.writable_regions() {
        let mut off = 0usize;
        while off < size {
            let n = CHUNK.min(size - off);
            let data = match pm.read(base + off, n) {
                Ok(d) => d,
                Err(_) => {
                    off += n;
                    continue;
                }
            };
            let usable = (data.len() / esize) * esize;
            let mut i = 0usize;
            while i < usable {
                if let Some(v) = vtype.unpack(&data[i..i + esize]) {
                    if v >= vmin && v <= vmax {
                        hits.push(base + off + i);
                    }
                }
                i += esize;
            }
            off += n;
            scanned += n;
            if let Some(cb) = on_progress.as_mut() {
                if scanned % (512 * 1024 * 1024) < CHUNK {
                    cb(scanned);
                }
            }
        }
    }
    hits
}

pub fn is_plausible_ptr(p: u64) -> bool {
    0x1_0000_0000 <= p && p < 0x0000_8000_0000_0000 && p % 8 == 0
}

fn is_ascii_identifier(s: &[u8]) -> bool {
    if s.is_empty() {
        return false;
    }
    let first = s[0] as char;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    s[1..]
        .iter()
        .all(|&b| (b as char).is_ascii_alphanumeric() || b == b'_')
}

/// 从对象的 vtable 指针解析出 Mono 类名。
/// 链路：object.vtable -> MonoVTable.klass -> klass->name (char*)。
/// Mono 不同版本 MonoClass 的字段偏移差异很大（name 大致在 0x48~0xA0），
/// 以 8 字节步长逐偏移试探，取第一个能读出合法标识符的作为类名。
pub fn resolve_class_name(pm: &ProcessMemory, vtable: usize) -> Option<String> {
    let klass = pm.read_u64(vtable).ok()? as usize;
    if klass < 0x10000 || klass % 8 != 0 {
        return None;
    }
    let mut off = 0x08usize;
    while off < 0xB0 {
        let sp = match pm.read_u64(klass + off) {
            Ok(v) => v as usize,
            Err(_) => {
                off += 8;
                continue;
            }
        };
        if sp < 0x10000 {
            off += 8;
            continue;
        }
        let raw = match pm.read(sp, 64) {
            Ok(r) => r,
            Err(_) => {
                off += 8;
                continue;
            }
        };
        let z = raw.iter().position(|&b| b == 0).unwrap_or(64);
        if !(3..=60).contains(&z) {
            off += 8;
            continue;
        }
        let name = &raw[..z];
        if name.iter().any(|&b| b >= 0x80) {
            off += 8;
            continue;
        }
        if is_ascii_identifier(name) {
            return Some(String::from_utf8_lossy(name).into_owned());
        }
        off += 8;
    }
    None
}

/// 带 vtable 缓存的「地址 -> 类名」读取器（对应 Python 的 _mk_cls）
pub struct ClassReader<'a> {
    pm: &'a ProcessMemory,
    cache: HashMap<u64, Option<String>>,
}

impl<'a> ClassReader<'a> {
    pub fn new(pm: &'a ProcessMemory) -> Self {
        Self {
            pm,
            cache: HashMap::new(),
        }
    }

    pub fn cls(&mut self, obj: usize) -> Option<String> {
        if !is_plausible_ptr(obj as u64) {
            return None;
        }
        let vtp = self.pm.read_u64(obj).ok()?;
        if !is_plausible_ptr(vtp) {
            return None;
        }
        if let Some(v) = self.cache.get(&vtp) {
            return v.clone();
        }
        let name = resolve_class_name(self.pm, vtp as usize);
        self.cache.insert(vtp, name.clone());
        name
    }

    pub fn slot_cls(&mut self, slot: usize) -> Option<String> {
        let p = self.pm.read_u64(slot).ok()? as usize;
        self.cls(p)
    }
}

#[derive(Clone, Debug)]
pub struct ComponentGroup {
    pub vtable: u64,
    pub addrs: Vec<usize>,
    pub value: Option<f64>,
    pub name: String,
}

/// 按对象头 vtable 指针对候选地址聚类，返回组件组列表。
/// 过滤：8 字节对齐、vtable 形如堆指针、实例数范围、地址集中、
/// 能沿 vtable -> MonoClass -> name 解析出合法类名。
pub fn cluster_components(
    pm: &ProcessMemory,
    candidates: &[usize],
    vtype: ValueType,
    min_group: usize,
    max_group: usize,
    max_candidates: usize,
) -> Result<Vec<ComponentGroup>> {
    if candidates.len() > max_candidates {
        return err(format!(
            "候选过多（{} 个），请缩小数值范围",
            candidates.len()
        ));
    }
    let mut cands: Vec<usize> = candidates
        .iter()
        .copied()
        .filter(|&a| a >= 0x10 && a % 8 == 0)
        .collect();
    cands.sort_unstable();

    let mut groups: HashMap<u64, Vec<usize>> = HashMap::new();
    let page_size = 0x1000usize;
    let total = cands.len();
    let mut i = 0usize;
    while i < total {
        let page = cands[i] & !(page_size - 1);
        let mut j = i;
        while j < total && (cands[j] & !(page_size - 1)) == page {
            j += 1;
        }
        let page_cands: Vec<usize> = cands[i..j]
            .iter()
            .copied()
            .filter(|&a| a - 0x10 >= page)
            .collect();
        if !page_cands.is_empty() {
            if let Ok(buf) = pm.read(page, page_size) {
                for a in page_cands {
                    let off = a - 0x10 - page;
                    if let Some(slice) = buf.get(off..off + 8) {
                        let vt = u64::from_le_bytes(slice.try_into().unwrap());
                        if is_plausible_ptr(vt) {
                            groups.entry(vt).or_default().push(a);
                        }
                    }
                }
            }
        }
        // 跨页的零散候选单独处理
        for &a in &cands[i..j] {
            if a - 0x10 < page {
                if let Ok(raw) = pm.read(a - 0x10, 8) {
                    let vt = u64::from_le_bytes(raw[..8].try_into().unwrap());
                    if is_plausible_ptr(vt) {
                        groups.entry(vt).or_default().push(a);
                    }
                }
            }
        }
        i = j;
    }

    let mut out = Vec::new();
    for (vt, addrs) in groups {
        if !(min_group..=max_group).contains(&addrs.len()) {
            continue;
        }
        let lo = *addrs.iter().min().unwrap();
        let hi = *addrs.iter().max().unwrap();
        if hi - lo > 16 * 1024 * 1024 {
            continue; // 分配太散，不是同一 size-class
        }
        let Some(name) = resolve_class_name(pm, vt as usize) else {
            continue; // 解析不出类名 = 不是 Mono 托管对象
        };
        let value = pm.read(addrs[0], vtype.size()).ok().and_then(|r| vtype.unpack(&r));
        out.push(ComponentGroup {
            vtable: vt,
            addrs,
            value,
            name,
        });
    }
    out.sort_by(|a, b| b.addrs.len().cmp(&a.addrs.len()));
    Ok(out)
}

/// 把一组地址批量写成同一个值，返回成功写入的数量
pub fn write_group(pm: &ProcessMemory, addrs: &[usize], vtype: ValueType, value: f64) -> usize {
    let packed = vtype.pack(value);
    let mut ok = 0;
    for &a in addrs {
        if pm.write(a, &packed).is_ok() {
            ok += 1;
        }
    }
    ok
}

/// 全堆反查：返回所有存放了 targets 中任一对象地址的 8 字节槽位地址列表。
pub fn reverse_scan_refs(
    pm: &ProcessMemory,
    targets: &[u64],
    mut on_progress: Option<&mut dyn FnMut(&str)>,
    label: &str,
) -> Vec<usize> {
    if targets.is_empty() {
        return Vec::new();
    }
    let mut tarr: Vec<u64> = targets.to_vec();
    tarr.sort_unstable();
    tarr.dedup();
    let mut refs = Vec::new();
    let mut scanned = 0usize;
    let mut last_report = 0usize;
    for (base, size) in pm.writable_regions() {
        let mut off = 0usize;
        while off < size {
            let n = CHUNK.min(size - off);
            let data = match pm.read(base + off, n) {
                Ok(d) => d,
                Err(_) => {
                    off += n;
                    scanned += n;
                    continue;
                }
            };
            let usable = (data.len() / 8) * 8;
            let mut i = 0usize;
            while i < usable {
                let v = u64::from_le_bytes(data[i..i + 8].try_into().unwrap());
                if tarr.binary_search(&v).is_ok() {
                    refs.push(base + off + i);
                }
                i += 8;
            }
            off += n;
            scanned += n;
        }
        if let Some(cb) = on_progress.as_mut() {
            if scanned - last_report >= 512 * 1024 * 1024 {
                last_report = scanned;
                cb(&format!(
                    "{}（已扫描 {:.1} GB）……",
                    label,
                    scanned as f64 / (1024.0 * 1024.0 * 1024.0)
                ));
            }
        }
    }
    refs
}

/// 过滤掉明显重复/无效的引用地址（供单元测试）
pub fn dedup_addresses(addrs: &[usize]) -> HashSet<usize> {
    addrs.iter().copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_float32_matches_python_struct() {
        // struct.pack('<f', 2.3) == b'\x33\x33\x13\x40'
        assert_eq!(ValueType::Float.pack(2.3), vec![0x33, 0x33, 0x13, 0x40]);
        assert_eq!(ValueType::Float.size(), 4);
        assert_eq!(ValueType::Double.size(), 8);
        assert_eq!(ValueType::Int64.size(), 8);
    }

    #[test]
    fn unpack_roundtrip() {
        let b = ValueType::Int32.pack(-12345.0);
        assert_eq!(ValueType::Int32.unpack(&b), Some(-12345.0));
        let b = ValueType::Float.pack(2.3);
        assert!((ValueType::Float.unpack(&b).unwrap() - 2.3).abs() < 1e-6);
    }

    #[test]
    fn pattern_search_bounds() {
        let data = b"abcabcabc";
        assert_eq!(find_from(data, b"abc", 0, 9), Some(0));
        assert_eq!(find_from(data, b"abc", 1, 9), Some(3));
        assert_eq!(find_from(data, b"abc", 7, 9), None);
        assert_eq!(find_from(data, b"zz", 0, 9), None);
    }

    #[test]
    fn identifier_filter() {
        assert!(is_ascii_identifier(b"HealthComponent"));
        assert!(is_ascii_identifier(b"_IntStat2"));
        assert!(!is_ascii_identifier(b"2Health"));
        assert!(!is_ascii_identifier(b"Health-Component"));
        assert!(!is_ascii_identifier(b""));
    }

    #[test]
    fn plausible_pointer_rules() {
        assert!(is_plausible_ptr(0x1_0000_0000));
        assert!(!is_plausible_ptr(0x1000));
        assert!(!is_plausible_ptr(0x1_0000_0004));
        assert!(!is_plausible_ptr(0x0000_8000_0000_0000));
    }
}
