# -*- coding: utf-8 -*-
"""
猫咪斗恶龙2（Cat Quest II）学习用修改器 - 核心模块
仅供学习研究使用，请勿用于破坏他人游戏体验。

主要功能的实现原理：
1) 内存修改（类似 Cheat Engine 的精确值扫描）
   - 用 OpenProcess / ReadProcessMemory / WriteProcessMemory 读写游戏进程
   - 遍历 VirtualQueryEx 得到的可写已提交内存区（Mono 垃圾回收堆属于
     MEM_PRIVATE 区域），用 bytes.find 做字节模式匹配，速度接近 C
   - "再次扫描" 在上次命中地址上比对数值变化，逐步缩小范围
   - 锁定 = 后台线程周期性把值写回同一地址
"""

import ctypes
import ctypes.wintypes as wintypes
import struct
import subprocess

kernel32 = ctypes.WinDLL('kernel32', use_last_error=True)

# ---------- 常量 ----------
PROCESS_VM_READ = 0x0010
PROCESS_VM_WRITE = 0x0020
PROCESS_VM_OPERATION = 0x0008
PROCESS_QUERY_INFORMATION = 0x0400

MEM_COMMIT = 0x1000
MEM_PRIVATE = 0x20000
MEM_MAPPED = 0x40000
PAGE_GUARD = 0x100
# 可读写保护属性：READWRITE / WRITECOPY / EXECUTE_READWRITE / EXECUTE_WRITECOPY
WRITABLE_PROTECTS = {0x04, 0x08, 0x40, 0x80}

CREATE_NO_WINDOW = 0x08000000

# 支持扫描的数据类型：名称 -> (struct 格式符, 元素字节数)
VALUE_TYPES = {
    'Int32': ('<i', 4),
    'Int64': ('<q', 8),
    'Float': ('<f', 4),
    'Double': ('<d', 8),
}

CHUNK = 32 * 1024 * 1024       # 每次 ReadProcessMemory 的块大小
MAX_HITS = 500_000             # 扫描命中上限，防止结果爆炸

GAME_PROCESS_NAME = 'Cat Quest II.exe'


class TrainerError(Exception):
    """业务错误，直接把 message 展示给用户即可"""


# ============================================================
# 进程内存读写
# ============================================================
class MEMORY_BASIC_INFORMATION(ctypes.Structure):
    # x64 布局；20~23 字节在旧结构里是 DWORD 对齐填充，新结构是 PartitionId，
    # 两种 Windows 版本下各字段偏移完全一致
    _fields_ = [
        ('BaseAddress', ctypes.c_void_p),
        ('AllocationBase', ctypes.c_void_p),
        ('AllocationProtect', wintypes.DWORD),
        ('Alignment', wintypes.DWORD),
        ('RegionSize', ctypes.c_size_t),
        ('State', wintypes.DWORD),
        ('Protect', wintypes.DWORD),
        ('Type', wintypes.DWORD),
    ]


class ProcessMemory:
    """对目标进程的内存读写封装"""

    def __init__(self, pid):
        self.pid = pid
        self.handle = kernel32.OpenProcess(
            PROCESS_VM_READ | PROCESS_VM_WRITE | PROCESS_VM_OPERATION | PROCESS_QUERY_INFORMATION,
            False, pid)
        if not self.handle:
            raise TrainerError('无法附加到进程（错误码 %d）。请确认游戏正在运行，'
                               '且修改器与游戏的管理员权限一致。' % ctypes.get_last_error())

    def close(self):
        if self.handle:
            kernel32.CloseHandle(self.handle)
            self.handle = None

    def __enter__(self):
        return self

    def __exit__(self, *a):
        self.close()

    def writable_regions(self):
        """遍历虚拟内存，返回可写、已提交、非 PAGE_GUARD 的区域。
        跳过 MEM_IMAGE（exe/dll 映像，一般存放代码和常量），
        只扫 MEM_PRIVATE / MEM_MAPPED —— Mono 的托管堆就在这里。"""
        mbi = MEMORY_BASIC_INFORMATION()
        addr = 0
        user_limit = 0x00007FFFFFFFFFFF
        while addr < user_limit:
            n = kernel32.VirtualQueryEx(self.handle, ctypes.c_void_p(addr),
                                        ctypes.byref(mbi), ctypes.sizeof(mbi))
            if not n:
                break
            base = mbi.BaseAddress or 0
            size = mbi.RegionSize
            if (mbi.State == MEM_COMMIT
                    and mbi.Protect in WRITABLE_PROTECTS
                    and not (mbi.Protect & PAGE_GUARD)
                    and mbi.Type in (MEM_PRIVATE, MEM_MAPPED)
                    and size > 0):
                yield base, size
            nxt = base + size
            if nxt <= addr:      # 防死循环
                break
            addr = nxt

    def read(self, addr, size):
        buf = ctypes.create_string_buffer(size)
        got = ctypes.c_size_t(0)
        ok = kernel32.ReadProcessMemory(self.handle, ctypes.c_void_p(addr),
                                        buf, size, ctypes.byref(got))
        if not ok:
            raise TrainerError('ReadProcessMemory 失败 @ 0x%X（错误码 %d）'
                               % (addr, ctypes.get_last_error()))
        return buf.raw[:got.value]

    def write(self, addr, data):
        got = ctypes.c_size_t(0)
        buf = ctypes.create_string_buffer(data, len(data))
        ok = kernel32.WriteProcessMemory(self.handle, ctypes.c_void_p(addr),
                                         buf, len(data), ctypes.byref(got))
        if not ok or got.value != len(data):
            raise TrainerError('WriteProcessMemory 失败 @ 0x%X（错误码 %d）'
                               % (addr, ctypes.get_last_error()))
        return got.value

    def read_value(self, addr, vtype):
        fmt, size = VALUE_TYPES[vtype]
        return struct.unpack(fmt, self.read(addr, size))[0]

    def write_value(self, addr, vtype, value):
        fmt, _ = VALUE_TYPES[vtype]
        self.write(addr, struct.pack(fmt, value))


# ============================================================
# 内存扫描
# ============================================================
def scan_exact(pm, pattern, on_progress=None):
    """在目标进程可写区域中精确搜索字节模式，返回命中的地址列表。
    用 bytes.find（底层是 C 实现的 memchr/memmem）做匹配，速度远快于逐字节循环。
    分块读取时留出 pattern 长度-1 的重叠，避免模式跨块时漏检。"""
    if not pattern:
        raise TrainerError('搜索值不能为空')
    plen = len(pattern)
    hits = []
    truncated = False
    scanned = 0
    for base, size in pm.writable_regions():
        off = 0
        while off < size:
            n = min(CHUNK, size - off)
            more = off + n < size
            want = n + (plen - 1 if more else 0)
            try:
                data = pm.read(base + off, want)
            except TrainerError:
                off += n          # 个别页读失败就跳过（可能被系统回收）
                continue
            # 本块只接受"起始于 [0, n)"的命中，避免与下一块重复
            limit = min(n, len(data) - plen + 1)
            pos = 0
            while pos < limit:
                i = data.find(pattern, pos, limit + plen - 1)
                if i < 0 or i >= limit:
                    break
                hits.append(base + off + i)
                if len(hits) >= MAX_HITS:
                    truncated = True
                    break
                pos = i + 1
            if truncated:
                break
            off += n
            scanned += n
        if truncated:
            break
        if on_progress and scanned % (128 * 1024 * 1024) < CHUNK:
            on_progress(scanned)
    return hits, truncated


def refine_with_history(pm, history, vtype, mode, ref_value=None):
    """再次扫描。
    history: {地址: 上次读到的原始字节}
    返回:    {地址: 本次读到的原始字节}（只保留符合条件的）
    相等类比较直接比字节——Float 类型回读是 float32，
    和用户输入的 float64 字面量直接比较会有精度误差，比字节最准确。"""
    fmt, size = VALUE_TYPES[vtype]
    ref_bytes = struct.pack(fmt, ref_value) if (mode == 'value' and ref_value is not None) else None
    out = {}
    for a, old in history.items():
        try:
            raw = pm.read(a, size)
        except TrainerError:
            continue
        keep = True
        if mode == 'value':
            keep = (ref_bytes is not None and raw == ref_bytes)
        elif mode == 'unchanged':
            keep = (raw == old)
        elif mode == 'changed':
            keep = (raw != old)
        elif mode in ('increased', 'decreased'):
            try:
                nv = struct.unpack(fmt, raw)[0]
                ov = struct.unpack(fmt, old)[0]
            except struct.error:
                continue
            keep = (nv > ov) if mode == 'increased' else (nv < ov)
        if keep:
            out[a] = raw
    return out


# ============================================================
# 锁定（冻结）：后台线程周期写回
# ============================================================
import threading
import time


class ValueFreezer:
    """把若干地址的值固定为指定值。
    普通模式每 80ms 写回一次；高速模式（interval=0）紧密循环写回，
    用于对抗游戏里"每帧从属性同步到组件"的值（如同步移速），
    写入周期需远小于 一帧内 同步点到读取点 的间隔才能生效。"""

    def __init__(self):
        self._items = {}          # addr -> (vtype, packed_bytes)
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._thread = None
        self._interval = 0.08

    def set(self, addr, vtype, value):
        fmt, _ = VALUE_TYPES[vtype]
        with self._lock:
            self._items[addr] = (vtype, struct.pack(fmt, value))

    def remove(self, addr):
        with self._lock:
            self._items.pop(addr, None)

    def clear(self):
        with self._lock:
            self._items.clear()

    def addrs(self):
        with self._lock:
            return set(self._items)

    def start(self, pm, interval=0.08):
        self._interval = interval
        if self._thread and self._thread.is_alive():
            return
        self._stop.clear()

        def loop():
            spins = 0
            while not self._stop.is_set():
                with self._lock:
                    items = list(self._items.items())
                for addr, (vtype, packed) in items:
                    try:
                        pm.write(addr, packed)
                    except Exception:
                        pass
                if self._interval > 0:
                    self._stop.wait(self._interval)
                else:
                    # 高速模式：不让出运行时会 100% 占满一个核，
                    # 每 500 轮主动让出一次调度（约 25ms 一次）
                    spins += 1
                    if spins % 500 == 0:
                        time.sleep(0.001)

        self._thread = threading.Thread(target=loop, daemon=True)
        self._thread.start()

    def stop(self):
        self._stop.set()
        if self._thread:
            self._thread.join(timeout=1)
            self._thread = None


class FlagKeeper:
    """免伤自愈维持器（解决"无敌开一会儿又失效"）。

    IL 实锤：游戏里翻滚/冲刺/倒地/起身/换人等一堆动画状态机都会调用
    ReplaceUnitFlags —— 直接【重建整个 UnitFlagsComponent 对象】，一次性写入的
    CannotTakeDamage 标记随旧对象一起作废；SceneController.OnEnable 还会在进场景时
    主动清除标记。实体表槽位本身不变（变的只是槽里的指针值）。

    因此本线程每隔 interval 秒做一次轻量校验（几次读 + 一两次写，开销可忽略）：
      1. 槽位链路重读当前组件对象 —— 对象被 Replace 换新时就地换新地址，
         无需慢速全量重定位；顺带刷新血量锁定/攻击魔法属性源地址；
      2. 标记被游戏清掉 → 立即写回（只 OR 进自己的位，不冲掉其他标记）；
      3. 血量对象被换新 → 把锁定目标同步到新地址；
      4. 指针读不出或明显非法（实体整体销毁/换场景）→ 触发 on_rebuild，
         由上层全量重新识别并重开功能。
    组件被换新时回调 on_refresh，让上层把秒杀/移速等一次性写入的值补写到新地址。"""

    def __init__(self, pm, players, freezer, god_enabled=None, on_refresh=None,
                 on_rebuild=None, on_error=None, interval=0.5):
        self.pm = pm
        self.players = players            # locate 返回的列表（含 ent/uf/hp 等，原地更新）
        self.freezer = freezer
        self.god_enabled = god_enabled or (lambda: True)
        self.on_refresh = on_refresh      # 组件换新回调：fn(players)
        self.on_rebuild = on_rebuild      # 实体销毁重建回调：fn()
        self.on_error = on_error          # 异常回调：fn(exc)
        self.interval = interval
        self._vt_cache = {}               # vtable -> 类名（避免重复解析）
        self._stop = threading.Event()
        self._thread = None

    def start(self):
        if self._thread and self._thread.is_alive():
            return
        self._stop.clear()
        self._thread = threading.Thread(target=self._loop, daemon=True)
        self._thread.start()

    def stop(self):
        self._stop.set()
        # 若从自己线程内调用（on_rebuild 回调里），不能 join 否则死锁
        if self._thread and self._thread is not threading.current_thread():
            self._thread.join(timeout=1)
            self._thread = None

    @staticmethod
    def _plausible(p):
        return 0x100000000 <= p < 0x0000800000000000 and p % 8 == 0

    def _cls(self, obj):
        """读对象 vtable 并解析类名（带缓存：正常情况下每 tick 只多几次读）"""
        if not self._plausible(obj):
            return None
        vtp = struct.unpack('<Q', self.pm.read(obj, 8))[0]
        if vtp not in self._vt_cache:
            self._vt_cache[vtp] = resolve_class_name(self.pm, vtp)
        return self._vt_cache[vtp]

    def _loop(self):
        while not self._stop.is_set():
            try:
                self._tick()
            except Exception as e:                    # noqa: BLE001
                # 单次校验失败不杀线程（可能只是瞬时读失败），交给上层记录
                if self.on_error:
                    try:
                        self.on_error(e)
                    except Exception:                 # noqa: BLE001
                        pass
            if self._stop.wait(self.interval):
                break

    def _tick(self):
        touched = False
        god = self.god_enabled()
        for p in self.players:
            if not p.get('uf'):
                continue
            ent = p['ent']
            try:
                # ---- 免伤标记组件：对象可能被 Replace 换新 ----
                uf_obj = struct.unpack('<Q', self.pm.read(ent + SLOT_UNITFLAGS_FROM_STATS, 8))[0]
                if self._cls(uf_obj) != 'UnitFlagsComponent':
                    raise TrainerError('免伤组件身份校验失败')
                if uf_obj != p['uf'] - 0x10:
                    p['uf'] = uf_obj + 0x10
                    p['uf_val'] = struct.unpack('<i', self.pm.read(p['uf'], 4))[0]
                    touched = True
                if god:
                    cur = struct.unpack('<i', self.pm.read(p['uf'], 4))[0]
                    if not (cur & FLAG_CANNOT_TAKE_DAMAGE):
                        self.pm.write(p['uf'], struct.pack('<i', cur | FLAG_CANNOT_TAKE_DAMAGE))

                # ---- 血量组件：锁定地址跟上换新 ----
                hp_obj = struct.unpack('<Q', self.pm.read(ent - SLOT_HEALTH_FROM_STATS, 8))[0]
                if self._cls(hp_obj) != 'HealthComponent':
                    raise TrainerError('血量组件身份校验失败')
                hp_addr = hp_obj + 0x10
                if hp_addr != p['hp']:
                    self.freezer.remove(p['hp'])          # 摘掉旧地址的锁
                    p['hp'] = hp_addr
                    p['hp_max'] = struct.unpack('<i', self.pm.read(hp_addr, 4))[0]
                    if god:
                        cur_hp = struct.unpack('<i', self.pm.read(hp_addr, 4))[0]
                        target = p['hp_max'] if 0 < p['hp_max'] <= 10_000_000 else cur_hp
                        self.freezer.set(hp_addr, 'Int32', target)
                    touched = True

                # ---- Stats 组件：攻击/魔法/移速属性源跟上换新 ----
                sc = struct.unpack('<Q', self.pm.read(ent, 8))[0]
                if self._cls(sc) != 'StatsComponent':
                    raise TrainerError('属性组件身份校验失败')
                for off, key in ((0x20, 'atk'), (0x28, 'magic'), (0x40, 'addr')):
                    obj = struct.unpack('<Q', self.pm.read(sc + off, 8))[0]
                    if self._plausible(obj) and obj + STAT_BASE_OFF != p[key]:
                        p[key] = obj + STAT_BASE_OFF
                        touched = True

                # ---- 实体身份总校验：这个实体表还是"那个玩家"吗 ----
                pc_obj = struct.unpack('<Q',
                                       self.pm.read(ent - SLOT_PLAYERCONTROLLED_FROM_STATS, 8))[0]
                if self._cls(pc_obj) != 'PlayerControlledComponent':
                    raise TrainerError('玩家实体已不存在')
            except TrainerError:
                # 槽位非法/身份对不上 → 实体整体已被销毁重建（换场景），需要全量重新识别
                if self.on_rebuild:
                    self.on_rebuild()
                return
        if touched and self.on_refresh:
            self.on_refresh(self.players)


# ============================================================
# 进程查找
# ============================================================
def _windowed_pids():
    """枚举所有有可见主窗口的进程 PID（用于剔除崩溃残留的无窗口僵尸进程）"""
    user32 = ctypes.WinDLL('user32', use_last_error=True)
    result = set()
    EnumProc = ctypes.WINFUNCTYPE(ctypes.c_bool, wintypes.HWND, wintypes.LPARAM)

    def cb(hwnd, _l):
        if user32.IsWindowVisible(hwnd):
            pid = wintypes.DWORD()
            user32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
            result.add(pid.value)
        return True
    user32.EnumWindows(EnumProc(cb), 0)
    return result


def find_game_pids():
    """按进程名找游戏 PID（用 tasklist，简单可靠）。
    同名进程可能有多个（如崩溃后残留的无窗口僵尸进程），
    有可见窗口的排在前面，保证附加到真正的游戏窗口进程。"""
    out = subprocess.run(
        ['tasklist', '/FI', 'IMAGENAME eq %s' % GAME_PROCESS_NAME, '/FO', 'CSV', '/NH'],
        capture_output=True, creationflags=CREATE_NO_WINDOW)
    text = out.stdout.decode('gbk', errors='replace')
    pids = []
    for line in text.splitlines():
        parts = [p.strip('"') for p in line.split('","')]
        if len(parts) >= 2 and parts[0].lower() == GAME_PROCESS_NAME.lower():
            try:
                pids.append(int(parts[1]))
            except ValueError:
                pass
    win = _windowed_pids()
    pids.sort(key=lambda p: 0 if p in win else 1)
    return pids


def is_game_running():
    return bool(find_game_pids())


# ============================================================
# 组件雷达：范围扫描 + Mono vtable 聚类
#
# 游戏是 Unity + Mono 的 ECS 架构，玩家的移速 / 攻击范围 / 血量等
# 分别存在独立的小组件对象里（如 MoveSpeedComponent.value: float）。
# Mono 托管对象的内存布局（x64）：
#     +0x00  vtable 指针   （同类的所有实例完全相同，可当"类指纹"）
#     +0x08  sync 指针
#     +0x10  第一个字段    （单字段组件的 value 就在这里）
# 因此：先按值范围扫出候选 -> 读每个候选 -0x10 处的 vtable ->
# 按 vtable 聚类 -> 一小组地址 = 同一个组件类的全部实例。
# ============================================================
def radar_scan(pm, vtype, vmin, vmax, on_progress=None):
    """范围扫描：返回值在 [vmin, vmax] 内且 4 字节对齐的地址列表。
    用 numpy 向量化比较，比逐字节 Python 循环快两个数量级。"""
    import numpy as np
    np_dtype = {'Int32': '<i4', 'Int64': '<i8', 'Float': '<f4', 'Double': '<f8'}[vtype]
    _, esize = VALUE_TYPES[vtype]
    hits = []
    scanned = 0
    for base, size in pm.writable_regions():
        off = 0
        while off < size:
            n = min(CHUNK, size - off)
            try:
                data = pm.read(base + off, n)
            except TrainerError:
                off += n
                continue
            usable = (len(data) // esize) * esize
            if usable:
                arr = np.frombuffer(data[:usable], dtype=np_dtype)
                # NaN 与范围比较天然为 False，无需额外处理
                idx = np.nonzero((arr >= vmin) & (arr <= vmax))[0]
                hits.extend(base + off + int(i) * esize for i in idx)
            off += n
            scanned += n
            if on_progress and scanned % (512 * 1024 * 1024) < CHUNK:
                on_progress(scanned)
    return hits


def resolve_class_name(pm, vtable):
    """从对象的 vtable 指针解析出 Mono 类名。
    链路：object.vtable -> MonoVTable.klass (MonoClass*) -> klass->name (char*)。
    Mono 不同版本 MonoClass 的字段偏移差异很大（name 大致在 0x48~0xA0），
    以 8 字节步长逐偏移试探，取第一个能读出合法标识符的作为类名；失败返回 None。"""
    try:
        klass = struct.unpack('<Q', pm.read(vtable, 8))[0]
    except TrainerError:
        return None
    if klass < 0x10000 or klass % 8:
        return None
    for off in range(0x08, 0xB0, 8):
        try:
            sp = struct.unpack('<Q', pm.read(klass + off, 8))[0]
        except TrainerError:
            continue
        if sp < 0x10000:                # char* 无对齐要求，只挡明显非法值
            continue
        try:
            raw = pm.read(sp, 64)
        except TrainerError:
            continue
        z = raw.find(b'\x00')
        if not (3 <= z <= 60):
            continue
        name = raw[:z]
        try:
            text = name.decode('ascii')
        except UnicodeDecodeError:
            continue
        if text.isidentifier():         # 类名必是合法标识符（字母开头，可含数字下划线）
            return text
    return None


def cluster_components(pm, candidates, vtype, min_group=2, max_group=200,
                       max_candidates=20_000_000):
    """按对象头 vtable 指针对候选地址聚类，返回组件组列表。
    每组 = {vtable, addrs, value, name}，按实例数降序排列。
    过滤条件（把随机数据碰巧聚成的假组剔掉）：
      1. 候选值地址 8 字节对齐（Mono 对象 8 对齐，字段在 +0x10）
      2. vtable 形如堆指针：>= 4GB 且在用户态范围内、8 对齐
         —— 在逐页读取阶段就过滤，绝大多数噪声（float 位模式等）在此被丢弃
      3. 组内实例数在 [min_group, max_group]（组件类实例数有限）
      4. 组内实例地址集中（散布 <= 16MB，同 size-class 的分配区相邻）
      5. 能沿 vtable -> MonoClass -> name 解析出合法类名（决定性过滤）"""
    if len(candidates) > max_candidates:
        raise TrainerError('候选过多（%d 个），请缩小数值范围' % len(candidates))

    def _plausible_ptr(p):
        return 0x100000000 <= p < 0x0000800000000000 and p % 8 == 0

    cands = sorted(a for a in candidates if a >= 0x10 and a % 8 == 0)
    groups = {}
    PAGE = 0x1000
    i = 0
    total = len(cands)
    while i < total:
        page = cands[i] & ~(PAGE - 1)
        j = i
        page_cands = []
        while j < total and (cands[j] & ~(PAGE - 1)) == page:
            if cands[j] - 0x10 >= page:      # 对象头与值同页才按页批量读
                page_cands.append(cands[j])
            j += 1
        if page_cands:
            try:
                buf = pm.read(page, PAGE)
            except TrainerError:
                buf = None
            if buf is not None:
                for a in page_cands:
                    vt = struct.unpack_from('<Q', buf, a - 0x10 - page)[0]
                    if _plausible_ptr(vt):
                        groups.setdefault(vt, []).append(a)
        # 跨页的零散候选单独处理
        for a in cands[i:j]:
            if a - 0x10 < page:
                try:
                    raw = pm.read(a - 0x10, 8)
                except TrainerError:
                    continue
                vt = struct.unpack('<Q', raw)[0]
                if _plausible_ptr(vt):
                    groups.setdefault(vt, []).append(a)
        i = j

    out = []
    for vt, addrs in groups.items():
        if not (min_group <= len(addrs) <= max_group):
            continue
        if max(addrs) - min(addrs) > 16 * 1024 * 1024:
            continue                       # 分配太散，不是同一 size-class
        name = resolve_class_name(pm, vt)
        if not name:
            continue                       # 解析不出类名 = 不是 Mono 托管对象
        out.append({'vtable': vt, 'addrs': addrs, 'value': None, 'name': name})
    out.sort(key=lambda g: -len(g['addrs']))
    for g in out:
        try:
            g['value'] = pm.read_value(g['addrs'][0], vtype)
        except TrainerError:
            g['value'] = None
    return out


def write_group(pm, addrs, vtype, value):
    """把一组地址批量写成同一个值，返回成功写入的数量"""
    packed = struct.pack(VALUE_TYPES[vtype][0], value)
    ok = 0
    for a in addrs:
        try:
            pm.write(a, packed)
            ok += 1
        except TrainerError:
            pass
    return ok


# 实体槽位布局（实机 dump 确认）：实体 = "按类名字母序排列的组件指针表"，
# 下面是以 Stats 槽为基准的其他组件槽偏移
SLOT_HEALTH_FROM_STATS = 0x260
SLOT_MOVESPEED_FROM_STATS = 0x1B0
SLOT_PLAYERCONTROLLED_FROM_STATS = 0x160
SLOT_AIPLAYER_FROM_STATS = 0x3D0
SLOT_UNITFLAGS_FROM_STATS = 0x50
# 游戏原生"无法受伤"标记（UnitFlags 的 CannotTakeDamage 位，IL 实锤：
# DoDamage / DoDamageToTarget 会先检查 HasUnitFlag(262144)，命中则直接跳过伤害）
FLAG_CANNOT_TAKE_DAMAGE = 0x40000
# Stat<T>（IntStat/FloatStat 的泛型基类）的属性源字段偏移
# （实机确认：base=+0x18、min=+0x1C、max=+0x20，Int/Float 布局一致；
#  改 base 后游戏每帧自动同步进各组件）
STAT_BASE_OFF = 0x18


def _reverse_scan_refs(pm, targets, on_progress=None, label='全堆反查引用'):
    """全堆反查：返回所有存放了 targets 中任一对象地址的 8 字节槽位地址列表。
    用 numpy searchsorted 向量化匹配，比逐字节比对快两个数量级。"""
    import numpy as np
    if not targets:
        return []
    tarr = np.array(sorted(set(targets)), dtype=np.uint64)
    refs = []
    scanned = 0
    last_report = 0
    for base, size in pm.writable_regions():
        off = 0
        while off < size:
            n = min(CHUNK, size - off)
            try:
                data = pm.read(base + off, n)
            except TrainerError:
                off += n
                scanned += n
                continue
            usable = (len(data) // 8) * 8
            if usable:
                arr = np.frombuffer(data[:usable], dtype='<u8')
                idx = np.searchsorted(tarr, arr)
                idx[idx == len(tarr)] = 0
                for pos in np.nonzero(arr == tarr[idx])[0]:
                    refs.append(base + off + int(pos) * 8)
            off += n
            scanned += n
        if on_progress and scanned - last_report >= 512 * 1024 * 1024:
            last_report = scanned
            on_progress('%s（已扫描 %.1f GB）……' % (label, scanned / (1024.0 ** 3)))
    return refs


def _mk_cls(pm):
    """生成「地址→类名」取类函数（带 vtable 缓存）与槽位取类函数。
    全堆扫描和快速通道两条识别路线共用，保证校验口径一致。"""
    _vt_cache = {}

    def _plausible(p):
        return 0x100000000 <= p < 0x0000800000000000 and p % 8 == 0

    def _cls(p):
        if not _plausible(p):
            return None
        try:
            vtp = struct.unpack('<Q', pm.read(p, 8))[0]
        except TrainerError:
            return None
        if not _plausible(vtp):
            return None
        if vtp not in _vt_cache:
            _vt_cache[vtp] = resolve_class_name(pm, vtp)
        return _vt_cache[vtp]

    def _slot_cls(slot):
        try:
            return _cls(struct.unpack('<Q', pm.read(slot, 8))[0])
        except TrainerError:
            return None

    return _cls, _slot_cls


def _extract_player(pm, ent, seen, _cls, _slot_cls):
    """ent = 实体上「存放 StatsComponent 指针」的槽位地址；按固定槽偏移做
    身份校验并提取一个玩家的全部修改点，校验不过或重复返回 None。
    （快速通道与全堆扫描共用本函数，保证两条路产出的字段完全一致）"""
    if _slot_cls(ent) != 'StatsComponent':
        return None
    if _slot_cls(ent - SLOT_PLAYERCONTROLLED_FROM_STATS) != 'PlayerControlledComponent':
        return None
    if _slot_cls(ent - SLOT_HEALTH_FROM_STATS) != 'HealthComponent':
        return None
    if _slot_cls(ent - SLOT_MOVESPEED_FROM_STATS) != 'MoveSpeedComponent':
        return None
    try:
        sc = struct.unpack('<Q', pm.read(ent, 8))[0]
        fs = struct.unpack('<Q', pm.read(sc + 0x40, 8))[0]   # moveSpeed 字段
        addr = fs + STAT_BASE_OFF                            # 移速属性源
        if addr in seen:
            return None
        value = struct.unpack('<f', pm.read(addr, 4))[0]
        # 血量(hp+0x10)/伤害(+0x20)/魔法(+0x28，远程伤害来源) 三个 IntStat 的属性源
        hp_obj = struct.unpack('<Q', pm.read(sc + 0x10, 8))[0]
        atk_obj = struct.unpack('<Q', pm.read(sc + 0x20, 8))[0]
        magic_obj = struct.unpack('<Q', pm.read(sc + 0x28, 8))[0]
        hp_src = hp_obj + STAT_BASE_OFF if _cls(hp_obj) == 'IntStat' else 0
        atk_src = atk_obj + STAT_BASE_OFF if _cls(atk_obj) == 'IntStat' else 0
        magic_src = magic_obj + STAT_BASE_OFF if _cls(magic_obj) == 'IntStat' else 0
        hp_max = struct.unpack('<i', pm.read(hp_src, 4))[0] if hp_src else 0
        atk_val = struct.unpack('<i', pm.read(atk_src, 4))[0] if atk_src else 0
        magic_val = struct.unpack('<i', pm.read(magic_src, 4))[0] if magic_src else 0
        # 当前血量：Health 槽存对象指针，值在对象 +0x10
        hp_obj2 = struct.unpack('<Q', pm.read(ent - SLOT_HEALTH_FROM_STATS, 8))[0]
        hp_cur = struct.unpack('<i', pm.read(hp_obj2 + 0x10, 4))[0]
        comp_obj = struct.unpack('<Q', pm.read(ent - SLOT_MOVESPEED_FROM_STATS, 8))[0]
        comp = comp_obj + 0x10                                 # 游戏同步目标
        # 原生免伤标记：UnitFlagsComponent.value（+0x10）
        uf_obj = struct.unpack('<Q', pm.read(ent + SLOT_UNITFLAGS_FROM_STATS, 8))[0]
        uf_addr = uf_obj + 0x10 if _cls(uf_obj) == 'UnitFlagsComponent' else 0
        uf_val = struct.unpack('<i', pm.read(uf_addr, 4))[0] if uf_addr else 0
    except TrainerError:
        return None
    seen.add(addr)
    is_ai = (_slot_cls(ent - SLOT_AIPLAYER_FROM_STATS) == 'AIPlayerComponent')
    name = '同伴(AI)' if is_ai else '主角'
    return {'name': name, 'addr': addr, 'value': value, 'comp': comp, 'ent': ent,
            'hp': hp_obj2 + 0x10, 'hp_cur': hp_cur, 'hp_max': hp_max,
            'atk': atk_src, 'atk_val': atk_val,
            'magic': magic_src, 'magic_val': magic_val,
            'uf': uf_addr, 'uf_val': uf_val}


def _find_component_slot(pm, ent_obj, sc):
    """在实体上找出「存放组件指针 sc 的槽位地址」。
    Entitas 的组件表是实体的一个 _components 数组字段（不在实体对象内），
    所以先在实体本体直接找，找不到再逐字段跟一层进数组里找。
    返回命中槽位列表（交给 _extract_player 逐一做身份校验）。"""
    needle = struct.pack('<Q', sc)

    def plausible(v):
        return 0x100000000 <= v < 0x0000800000000000 and v % 8 == 0

    hits = []
    try:
        for off in range(0, 0xA00, 8):
            if pm.read(ent_obj + off, 8) == needle:
                hits.append(ent_obj + off)
    except TrainerError:
        pass
    if hits:
        return hits
    for off in range(0, 0x100, 8):
        try:
            p = struct.unpack('<Q', pm.read(ent_obj + off, 8))[0]
        except TrainerError:
            break
        if not plausible(p):
            continue
        for aoff in (0x18, 0x20, 0x10, 0x28):
            try:
                n = struct.unpack('<Q', pm.read(p + aoff - 8, 8))[0]
                if not (0 < n < 10000):
                    continue
                blob = pm.read(p + aoff, 8 * n)
            except TrainerError:
                continue
            idx = 0
            while True:
                k = blob.find(needle, idx)
                if k < 0:
                    break
                if k % 8 == 0:
                    hits.append(p + aoff + k)
                idx = k + 8
        if hits:
            break
    return hits


def _locate_via_mono(pm, on_progress=None):
    """快速识别：直连游戏自身的 Contexts/GameEntity 接口枚举玩家实体，
    再在实体对象的组件槽里定位属性组件 —— 完全跳过全堆扫描，正常 1~3 秒。
    与「最大蓝条」同一套已实测安全的远程调用链路（无参/单 int 参数）。
    任何一步失败都抛异常，由 locate_player_speed_sources 自动退回全堆扫描。"""
    if on_progress:
        on_progress('通过游戏内部接口识别角色（约1秒）……')
    base = remote_module_base(pm.pid, MONO_DLL)
    if not base:
        raise TrainerError('取不到脚本运行时')
    f = {k: base + v for k, v in MONO_RVAS.items()}
    rc = RemoteCall(pm)
    try:
        s_asm = rc.write_str('Assembly-CSharp')
        s_ns = rc.write_str('')
        s_ctx = rc.write_str('Contexts')
        s_ge = rc.write_str('GameEntity')
        s_shared = rc.write_str('get_sharedInstance')
        s_game = rc.write_str('get_game')
        s_ents = rc.write_str('GetEntities')
        s_haspc = rc.write_str('get_isPlayerControlled')
        s_stats = rc.write_str('get_stats')
        exc_buf = rc.alloc(8)
        pm.write(exc_buf, b'\x00' * 8)
        _, r = rc.run_sequence([
            (f['mono_get_root_domain'], []),
            (f['mono_thread_attach'], [('slot', 0)]),
            (f['mono_image_loaded'], [s_asm]),
            (f['mono_thread_detach'], [('slot', 1)]),
        ])
        domain, image = r[0], r[2]
        if not domain or not image:
            raise TrainerError('脚本尚未加载')

        def seq(steps):
            _, rr = rc.run_sequence(
                [(f['mono_thread_attach'], [domain])] + steps +
                [(f['mono_thread_detach'], [('slot', 0)])], timeout_ms=20000)
            return rr

        def exc_name():
            e = struct.unpack('<Q', pm.read(exc_buf, 8))[0]
            if not e:
                return ''
            pm.write(exc_buf, b'\x00' * 8)
            try:
                return resolve_class_name(pm, struct.unpack('<Q', pm.read(e, 8))[0]) \
                    or '未知异常'
            except Exception:                          # noqa: BLE001
                return '未知异常'

        def inv(m, this=0, arg=0):
            pm.write(exc_buf, b'\x00' * 8)
            rr = seq([(f['mono_runtime_invoke'], [m, this, arg, exc_buf])])
            return rr[1], exc_name()

        def find_method(k, name_s, argc):
            """本类找不到就沿父类链找（GetEntities 定义在 Entitas 泛型基类上）"""
            for _ in range(6):
                if not k:
                    return 0
                rr = seq([(f['mono_class_get_method_from_name'], [k, name_s, argc])])
                if rr[1]:
                    return rr[1]
                rr = seq([(f['mono_class_get_parent'], [k])])
                k = rr[1]
            return 0

        def is_true(v):
            """bool 返回可能是装箱对象（真值在 +0x10）或裸 0/1，自动区分"""
            if v >= 0x100000000 and v % 8 == 0:
                return pm.read(v + 0x10, 1)[0] == 1
            return (v & 0xFF) == 1

        def u64(addr):
            return struct.unpack('<Q', pm.read(addr, 8))[0]

        rr = seq([(f['mono_class_from_name'], [image, s_ns, s_ctx]),
                  (f['mono_class_from_name'], [image, s_ns, s_ge])])
        k_ctx, k_ge = rr[1], rr[2]
        if not k_ctx or not k_ge:
            raise TrainerError('未找到角色逻辑')
        m_shared = find_method(k_ctx, s_shared, 0)
        shared, e = inv(m_shared)
        if e or not shared:
            raise TrainerError('读不到游戏上下文')
        k_shared = seq([(f['mono_object_get_class'], [shared])])[1]
        m_game = find_method(k_shared, s_game, 0)
        game, e = inv(m_game, shared)
        if e or not game:
            raise TrainerError('读不到游戏状态')
        k_game = seq([(f['mono_object_get_class'], [game])])[1]
        m_ents = find_method(k_game, s_ents, 0)
        m_haspc = find_method(k_ge, s_haspc, 0)
        m_stats = find_method(k_ge, s_stats, 0)
        if not all((m_ents, m_haspc, m_stats)):
            raise TrainerError('未找到角色接口')

        # ---- 枚举全部实体并批量过滤出玩家 ----
        arr, e = inv(m_ents, game)
        if e or not arr:
            raise TrainerError('读不到实体列表')
        _cls, _slot_cls = _mk_cls(pm)
        data_off, count = 0, 0
        for off in (0x18, 0x20, 0x10, 0x28):
            try:
                n = u64(arr + off - 8)
                if not (1 <= n <= 200000):
                    continue
                if _cls(u64(arr + off)) != 'GameEntity':
                    continue
                if n >= 2 and _cls(u64(arr + off + 8)) != 'GameEntity':
                    continue
                data_off, count = off, int(n)
                break
            except Exception:                          # noqa: BLE001
                continue
        if not data_off:
            raise TrainerError('实体列表格式变了')
        players = []
        for i in range(0, count, 50):
            ents = [u64(arr + data_off + 8 * j)
                    for j in range(i, min(i + 50, count))]
            pm.write(exc_buf, b'\x00' * 8)
            r2 = seq([(f['mono_runtime_invoke'], [m_haspc, ep, 0, exc_buf])
                      for ep in ents])
            players.extend(ents[j] for j in range(len(ents)) if is_true(r2[1 + j]))
            if len(players) >= 2:
                break
        if not players:
            raise TrainerError('当前画面没有可操作角色，请进入游戏世界后再连接')

        # ---- 每个玩家：get_stats -> 在实体对象里定位 Stats 槽 -> 提取 ----
        out, seen = [], set()
        for E in players:
            sc, e = inv(m_stats, E)
            if e or not sc:
                continue
            # 组件表在实体的 _components 数组里，先定位存放 stats 指针的槽位
            # （可能不止一处命中，逐个做槽位身份校验）
            for ent in _find_component_slot(pm, E, sc):
                p = _extract_player(pm, ent, seen, _cls, _slot_cls)
                if p:
                    out.append(p)
                    break
        if not out:
            raise TrainerError('没解析出玩家属性（可能游戏版本不匹配）')
        return out
    finally:
        try:
            rc.free_all()
        except Exception:                              # noqa: BLE001
            pass


def locate_player_speed_sources(pm, on_progress=None):
    """定位玩家（主角/同伴）的三个"属性源"地址 —— 血量/伤害/移速的 Stat.baseValue。
    改这里后游戏自己把值同步进各组件（如 UpdateMoveSpeedSystem 每帧同步移速），
    读写都发生在游戏内部，不存在被每帧覆盖的竞态。

    链路（实机验证）：玩家移速 FloatStat(≈2.3) → StatsComponent(+0x40 字段)
         → 全堆反查 → 实体组件表槽位 → 按槽位偏移读 PlayerControlled 标签
         确认玩家实体 → 同表取 Health/MoveSpeed 槽与 health/attack 两个 IntStat
    返回 [{'name': '主角'/'同伴(AI)',
           'hp': HealthComponent 当前血量地址, 'hp_cur': 当前血量, 'hp_max': 血量上限,
           'atk': 近战攻击属性源地址, 'atk_val': 原始值,
           'magic': 魔法属性源地址（远程伤害来源）, 'magic_val': 原始值,
           'uf': 原生免伤标记地址, 'uf_val': 原始标记值,
           'addr': 移速属性源地址, 'value': 原始移速,
           'comp': MoveSpeedComponent 的值地址（游戏同步目标，用于验证）}]
    """

    # 快速通道：直连游戏实体接口，秒级完成；任何异常自动退回下面的全堆扫描
    try:
        return _locate_via_mono(pm, on_progress)
    except Exception:                                  # noqa: BLE001
        if on_progress:
            on_progress('快速识别未成功，改用全堆扫描（约30秒）……')

    _cls, _slot_cls = _mk_cls(pm)

    # 1. 玩家移速属性源 FloatStat（base ≈ 2.3，base 字段在 +0x18）
    # 范围放宽到 2.2~25：覆盖界面倍速档（最高10倍≈23），
    # 即使先改过速也能重新识别；命中靠后续 FloatStat/实体表结构校验兜底，不会误判
    if on_progress:
        on_progress('扫描玩家移速……')
    cands = radar_scan(pm, 'Float', 2.2, 25.0)
    fs_objs = sorted({c - 0x18 for c in cands
                      if c >= 0x18 and c % 8 == 0 and _cls(c - 0x18) == 'FloatStat'})
    if not fs_objs:
        raise TrainerError('未找到玩家移速数据，请先进入游戏世界（可控制角色移动的画面）再试')

    # 2. 反查 FloatStat 引用 → StatsComponent（moveSpeed 字段在 +0x40）
    if on_progress:
        on_progress('定位角色属性（全堆反查，约 20~60 秒）……')
    fs_set = set(fs_objs)
    stats_objs = []
    for r in _reverse_scan_refs(pm, fs_objs, on_progress, '定位角色属性'):
        s = r - 0x40
        if s in stats_objs or _cls(s) != 'StatsComponent':
            continue
        try:
            if struct.unpack('<Q', pm.read(s + 0x40, 8))[0] in fs_set:
                stats_objs.append(s)
        except TrainerError:
            continue
    if not stats_objs:
        raise TrainerError('未找到玩家属性组件，请先进入游戏世界再试')

    # 3. 反查 StatsComponent 引用 → 实体组件表槽位，按固定槽偏移校验玩家标签
    if on_progress:
        on_progress('识别玩家实体（再次全堆反查，约 20~60 秒）……')
    out, seen = [], set()
    for ent in _reverse_scan_refs(pm, stats_objs, on_progress, '识别玩家实体'):
        p = _extract_player(pm, ent, seen, _cls, _slot_cls)
        if p:
            out.append(p)
    if not out:
        raise TrainerError('未找到玩家实体，请先进入游戏世界再试')
    return out


# ============================================================
# 全屏拾取（远程调用游戏内置的 Collectible.ExecuteCollection）
# ============================================================
# 原理：游戏"捡金币"是一段现成代码 Collectible.ExecuteCollection，正常靠角色
# 碰到金币碰撞体才触发；修改器在游戏进程内创建远程线程，按序调用 Mono API，
# 直接执行这段代码 —— 跳过"跑过去碰"。与探针版 probe_pickup.py 同一套链路，
# 实测 160+ 个掉落物批量入账无异常、游戏不崩。
#
# mono-2.0-bdwgc.dll 导出 RVA（已用 pefile 对照导出表逐个核对过，
# 换游戏版本必须重新核对首字节是否为合法函数序言，否则一调就崩）
MONO_DLL = 'mono-2.0-bdwgc.dll'
MONO_RVAS = {
    'mono_thread_attach': 0x1EE030,
    'mono_thread_detach': 0x1EE440,
    'mono_get_root_domain': 0xA4C30,
    'mono_image_loaded': 0x141A50,
    'mono_class_from_name': 0xBD050,
    'mono_class_get_method_from_name': 0xC10C0,
    'mono_class_vtable': 0x1BC270,
    'mono_runtime_invoke': 0x1BEBD0,
    'mono_object_get_class': 0x1C9AD0,
    'mono_class_get_name': 0xBFD40,
    'mono_class_get_parent': 0xBFBF0,   # 沿父类链找泛型基类上的方法（已核对导出表）
}

# Collectible 实例布局（mono_class_get_fields 实机枚举，与游戏版本绑定）
CACHED_PTR_OFF = 0x10       # Mono 对象 +0x10 = Unity 原生对象指针（0=已销毁）
COLLECT_TYPE_OFF = 0x60     # 0=金币 1=经验
COLLECT_AMOUNT_OFF = 0x70   # 拾取金额

MEM_RESERVE = 0x2000
MEM_RELEASE = 0x8000
PAGE_EXECUTE_READWRITE = 0x40

kernel32.VirtualAllocEx.restype = ctypes.c_void_p
kernel32.VirtualAllocEx.argtypes = [ctypes.c_void_p, ctypes.c_void_p,
                                    ctypes.c_size_t, ctypes.c_uint32,
                                    ctypes.c_uint32]
kernel32.VirtualFreeEx.restype = ctypes.c_bool
kernel32.CreateRemoteThread.restype = ctypes.c_void_p
kernel32.CreateRemoteThread.argtypes = [ctypes.c_void_p, ctypes.c_void_p,
                                        ctypes.c_size_t, ctypes.c_void_p,
                                        ctypes.c_void_p, ctypes.c_uint32,
                                        ctypes.c_void_p]
kernel32.WaitForSingleObject.restype = ctypes.c_uint32
kernel32.WaitForSingleObject.argtypes = [ctypes.c_void_p, ctypes.c_uint32]


class MODULEENTRY32W(ctypes.Structure):
    _fields_ = [
        ('dwSize', ctypes.c_uint32),
        ('th32ModuleID', ctypes.c_uint32),
        ('th32ProcessID', ctypes.c_uint32),
        ('GlblcntUsage', ctypes.c_uint32),
        ('ProccntUsage', ctypes.c_uint32),
        ('modBaseAddr', ctypes.c_void_p),
        ('modBaseSize', ctypes.c_uint32),
        ('hModule', ctypes.c_void_p),
        ('szModule', ctypes.c_wchar * 256),
        ('szExePath', ctypes.c_wchar * 260),
    ]


def remote_module_base(pid, name):
    """取目标进程里指定模块的加载基址（Toolhelp32 快照）"""
    k32 = kernel32
    k32.CreateToolhelp32Snapshot.restype = ctypes.c_void_p
    h = k32.CreateToolhelp32Snapshot(0x8 | 0x10, pid)   # SNAPMODULE|SNAPMODULE32
    if not h or h == 0xFFFFFFFF:
        raise TrainerError('读取游戏模块列表失败（错误码 %d）' % ctypes.get_last_error())
    try:
        e = MODULEENTRY32W()
        e.dwSize = ctypes.sizeof(e)
        ok = k32.Module32FirstW(h, ctypes.byref(e))
        while ok:
            if e.szModule.lower() == name.lower():
                return e.modBaseAddr
            ok = k32.Module32NextW(h, ctypes.byref(e))
    finally:
        k32.CloseHandle(h)
    return None


# 远程调用的寄存器编码（x64：参数走 rcx/rdx/r8/r9）
_REG_MOV = {0: b'\x48\xB9', 1: b'\x48\xBA', 2: b'\x49\xB8', 3: b'\x49\xB9',
            4: b'\x49\xBA'}
_REG_CALL = {0: b'\xFF\xD1', 1: b'\xFF\xD2', 2: b'\x41\xFF\xD0',
             3: b'\x41\xFF\xD1', 4: b'\x41\xFF\xD2'}


class RemoteCall:
    """在游戏进程里按序执行若干 Mono API 调用。

    做法：VirtualAllocEx 分配可执行内存 -> 生成一小段 x64 机器码
    （绝对地址内嵌）-> CreateRemoteThread 执行 -> 结果写在远程槽位读回。
    x64 约定：入口 rsp≡8(mod16)，每次 call 前 sub rsp,0x28 保证对齐与
    shadow space；每步返回值立即写槽，跨 call 不依赖易失寄存器。"""

    def __init__(self, pm):
        self.pm = pm
        self._mems = []

    def alloc(self, size, exec_page=False):
        prot = PAGE_EXECUTE_READWRITE if exec_page else 0x04
        p = kernel32.VirtualAllocEx(self.pm.handle, None, size,
                                    MEM_COMMIT | MEM_RESERVE, prot)
        if not p:
            raise TrainerError('分配游戏内存失败（错误码 %d）' % ctypes.get_last_error())
        self._mems.append(p)
        return p

    def free(self, p):
        kernel32.VirtualFreeEx(self.pm.handle, p, 0, MEM_RELEASE)
        if p in self._mems:
            self._mems.remove(p)

    def free_all(self):
        for p in list(self._mems):
            try:
                kernel32.VirtualFreeEx(self.pm.handle, p, 0, MEM_RELEASE)
            except Exception:                            # noqa: BLE001
                pass
        self._mems = []

    def write_str(self, s):
        data = s.encode('utf-8') + b'\x00'
        addr = self.alloc(max(len(data), 8))
        self.pm.write(addr, data)
        return addr

    def _emit_call(self, asm, func, args, slots):
        """mov rcx/rdx/r8/r9 <- 参数；call func；调用者再存槽。
        参数形式：int=立即数 / ('q',addr)=读远程内存 / ('slot',i)=上一步返回值"""
        for i, a in enumerate(args[:4]):
            if isinstance(a, int):
                asm += _REG_MOV[i] + struct.pack('<Q', a)
            else:
                src = a[1] if a[0] == 'q' else slots + 8 * a[1]
                asm += b'\x48\xA1' + struct.pack('<Q', src)    # mov rax,[imm64]
                asm += (b'\x48\x89\xC1', b'\x48\x89\xC2',
                        b'\x49\x89\xC0', b'\x49\x89\xC1')[i]
        if isinstance(func, int):
            asm += _REG_MOV[4] + struct.pack('<Q', func) + _REG_CALL[4]
        else:
            asm += b'\x48\xA1' + struct.pack('<Q', func[1]) + _REG_CALL[4]
        return asm

    def run_sequence(self, calls, timeout_ms=15000):
        """calls: [(函数地址, [arg0..arg3]), ...] -> (槽位地址, [每步 rax])"""
        n = len(calls)
        slots = self.alloc(8 * n + 64)
        asm = b'\x48\x83\xEC\x28'                            # sub rsp,0x28
        for i, (func, args) in enumerate(calls):
            asm = self._emit_call(asm, func, args, slots)
            asm += b'\x48\xA3' + struct.pack('<Q', slots + 8 * i)
        asm += b'\x48\x83\xC4\x28' + b'\xC3'                 # add rsp,0x28; ret

        code = self.alloc(len(asm), exec_page=True)
        self.pm.write(code, asm)
        self.pm.write(slots, b'\x00' * (8 * n))

        h = None
        for attempt in range(5):
            h = kernel32.CreateRemoteThread(self.pm.handle, None, 0,
                                            code, None, 0, None)
            if h:
                break
            err = ctypes.get_last_error()
            if err == 5 and attempt < 4:      # 进程繁忙时偶发拒绝，稍候重试
                time.sleep(0.2)
                continue
            raise TrainerError('在游戏内执行代码失败（错误码 %d）' % err)
        try:
            wr = kernel32.WaitForSingleObject(h, timeout_ms)
            if wr == 0x102:
                raise TimeoutError('游戏内执行超时')
        finally:
            kernel32.CloseHandle(h)
        raw = self.pm.read(slots, 8 * n)
        # 线程已结束，临时代码/槽位即刻回收，防止长驻巡逻把游戏内存写爆
        try:
            self.free(code)
            self.free(slots)
        except Exception:                                  # noqa: BLE001
            pass
        return slots, [struct.unpack_from('<Q', raw, 8 * i)[0] for i in range(n)]


class PickupKeeper:
    """全屏拾取巡逻线程。

    与探针版一致的三段策略：
      1. 启动时重新解析整条调用链（域/镜像/类/方法/vtable —— 地址每局都会变，
         绝不复用上一局的旧地址）；
      2. 全堆扫一次记住场上掉落物位置（含对象池备用对象）；
      3. 之后每 interval 秒只复查已知位置（毫秒级），每 full_scan 秒全盘扫
         一次兜底对象池扩容 —— 掉落即捡，体感等于"击杀瞬间入账"。
    捡取走游戏自己的 ExecuteCollection：音效/动画/入账全由游戏完成。
    调用前逐个重校验对象身份（vtable + 原生指针 + 金额>0），
    防止对象已被回收时误调用；个别坏对象会被就地隔离（金额清零），不再干扰。"""

    def __init__(self, pm, on_picked=None, on_error=None,
                 interval=0.3, full_scan=5.0):
        self.pm = pm
        self.on_picked = on_picked      # fn(got, total) 每轮有收获时回调
        self.on_error = on_error        # fn(exc) 致命错误（线程退出前回调）
        self.interval = interval
        self.full_scan = full_scan
        self._f = {}                    # Mono API 地址表
        self._stop = threading.Event()
        self._thread = None

    def start(self):
        if self._thread and self._thread.is_alive():
            return
        self._stop.clear()
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def stop(self):
        self._stop.set()
        # 若在自身线程内被调用会死锁，必须判当前线程
        if self._thread and self._thread is not threading.current_thread():
            self._thread.join(timeout=1)
            self._thread = None

    # ---------- 调用链解析 ----------
    def _resolve(self, rc):
        f = self._f
        s_asm = rc.write_str('Assembly-CSharp')
        s_ns = rc.write_str('')
        s_cls = rc.write_str('Collectible')
        s_mtd = rc.write_str('ExecuteCollection')
        _, r = rc.run_sequence([
            (f['mono_get_root_domain'], []),
            (f['mono_thread_attach'], [('slot', 0)]),
            (f['mono_image_loaded'], [s_asm]),
            (f['mono_thread_detach'], [('slot', 1)]),
        ])
        domain, image = r[0], r[2]
        if not domain or not image:
            raise TrainerError('游戏脚本尚未加载完成，请进入游戏画面后再开启')
        _, r = rc.run_sequence([
            (f['mono_thread_attach'], [domain]),
            (f['mono_class_from_name'], [image, s_ns, s_cls]),
            (f['mono_class_get_method_from_name'], [('slot', 1), s_mtd, 0]),
            (f['mono_thread_detach'], [('slot', 0)]),
        ])
        klass, method = r[1], r[2]
        if not klass or not method:
            raise TrainerError('未找到游戏的拾取逻辑（游戏版本可能不匹配）')
        _, r = rc.run_sequence([
            (f['mono_thread_attach'], [domain]),
            (f['mono_class_vtable'], [domain, klass]),
            (f['mono_thread_detach'], [('slot', 0)]),
        ])
        vt = r[1]
        if not vt:
            raise TrainerError('未找到掉落物类型信息')
        return domain, method, vt

    # ---------- 扫描与复查 ----------
    def _decode(self, raw, vt):
        """对象头 -> (obj地址, 金额, 类型)；不是有效活体返回 None"""
        if struct.unpack_from('<Q', raw, 0)[0] != vt:
            return None
        if not struct.unpack_from('<Q', raw, CACHED_PTR_OFF)[0]:
            return None
        amt = struct.unpack_from('<i', raw, COLLECT_AMOUNT_OFF)[0]
        typ = struct.unpack_from('<i', raw, COLLECT_TYPE_OFF)[0]
        if not (0 < amt < 10 ** 7) or typ not in (0, 1):
            return None
        return amt, typ

    def _full_scan(self, vt):
        objs, _ = scan_exact(self.pm, struct.pack('<Q', vt))
        out = {}
        for o in objs:
            try:
                raw = self.pm.read(o, 0x78)
            except TrainerError:
                continue
            d = self._decode(raw, vt)
            if d:
                out[o] = (o, d[0], d[1])
        return out

    def _recheck(self, known, vt):
        """只重读已知对象头部（几百次小读，毫秒级）；返回当前活体列表"""
        live = []
        for o in list(known):
            try:
                raw = self.pm.read(o, 0x78)
            except TrainerError:
                known.pop(o, None)       # 对象已销毁
                continue
            d = self._decode(raw, vt)
            if d is None:
                # 无原生指针=池中休眠，地址保留等复用；
                # vtable 对不上=对象换代，移除（全盘扫会再收进来）
                if struct.unpack_from('<Q', raw, 0)[0] != vt:
                    known.pop(o, None)
                continue
            item = (o, d[0], d[1])
            known[o] = item
            live.append(item)
        return live

    # ---------- 拾取 ----------
    def _one(self, rc, domain, method, exc_buf, item):
        """拾取单个；无论成败都清零金额（防止反复重试同一坏对象）"""
        pm = self.pm
        pm.write(exc_buf, b'\x00' * 8)
        try:
            rc.run_sequence([
                (self._f['mono_thread_attach'], [domain]),
                (self._f['mono_runtime_invoke'], [method, item[0], 0, exc_buf]),
                (self._f['mono_thread_detach'], [('slot', 0)]),
            ], timeout_ms=20000)
            exc = struct.unpack('<Q', pm.read(exc_buf, 8))[0]
        except (TrainerError, OSError, TimeoutError):
            exc = -1
        try:
            pm.write(item[0] + COLLECT_AMOUNT_OFF, struct.pack('<i', 0))
        except TrainerError:
            pass
        return exc == 0

    def _collect(self, rc, domain, method, exc_buf, live, vt):
        pm = self.pm
        f = self._f
        got = total = 0
        i = 0
        while i < len(live) and not self._stop.is_set():
            chunk = live[i:i + 20]
            i += 20
            # 调用前逐个重校验（对象可能刚被玩家捡走/回收）
            valid = []
            for item in chunk:
                try:
                    raw = pm.read(item[0], 0x78)
                except TrainerError:
                    continue
                if self._decode(raw, vt):
                    valid.append(item)
            if not valid:
                continue
            pm.write(exc_buf, b'\x00' * 8)
            calls = [(f['mono_thread_attach'], [domain])]
            calls += [(f['mono_runtime_invoke'], [method, it[0], 0, exc_buf])
                      for it in valid]
            calls += [(f['mono_thread_detach'], [('slot', 0)])]
            try:
                rc.run_sequence(calls, timeout_ms=30000)
                exc = struct.unpack('<Q', pm.read(exc_buf, 8))[0]
            except (TrainerError, OSError, TimeoutError):
                exc = -1
            if exc:                    # 批量出问题 -> 逐个拾取隔离坏对象
                for it in valid:
                    if self._one(rc, domain, method, exc_buf, it):
                        got += 1
                        total += it[1]
                continue
            for it in valid:           # 成功：清零金额，防下轮重复入账
                try:
                    pm.write(it[0] + COLLECT_AMOUNT_OFF, struct.pack('<i', 0))
                except TrainerError:
                    pass
            got += len(valid)
            total += sum(it[1] for it in valid)
        return got, total

    # ---------- 主循环 ----------
    def _run(self):
        rc = None
        errs = 0
        try:
            base = remote_module_base(self.pm.pid, MONO_DLL)
            if not base:
                raise TrainerError('取不到游戏脚本运行时（游戏可能已退出）')
            self._f = {k: base + v for k, v in MONO_RVAS.items()}
            rc = RemoteCall(self.pm)
            domain, method, vt = self._resolve(rc)
            exc_buf = rc.alloc(8)
            known = {}
            last_full = 0.0
            while not self._stop.is_set():
                try:
                    live = self._recheck(known, vt)
                    now = time.time()
                    if now - last_full >= self.full_scan:
                        known.update(self._full_scan(vt))
                        last_full = now
                        live = self._recheck(known, vt)
                    if live:
                        got, total = self._collect(rc, domain, method,
                                                   exc_buf, live, vt)
                        if got and self.on_picked:
                            try:
                                self.on_picked(got, total)
                            except Exception:              # noqa: BLE001
                                pass
                    errs = 0
                except Exception:                          # noqa: BLE001
                    # 瞬时读失败不杀线程；连续 5 轮失败（游戏退出）才上报
                    errs += 1
                    if errs >= 5:
                        raise
                self._stop.wait(self.interval)
        except Exception as e:                             # noqa: BLE001
            if self.on_error:
                try:
                    self.on_error(e)
                except Exception:                          # noqa: BLE001
                    pass
        finally:
            if rc:
                try:
                    rc.free_all()
                except Exception:                          # noqa: BLE001
                    pass


# ============================================================
# 游戏整体倍速（UnityEngine.Time.timeScale）
# ============================================================
# Unity 的 Time.timeScale 是全局时间缩放：移动/动画/打怪全按它缩放。
# 它在 UnityEngine.CoreModule 的 Time 类里（set_timeScale 静态1参float，
# IL 是 extern 但 mono_runtime_invoke 可直接调）。游戏暂停/加载时可能把它
# 改回 1 或 0，所以用线程周期保持；float 传参沿用"地址==位模式"自引用：
# 2.0 的 IEEE754 位模式 0x40000000 恰好页对齐，在该地址写入该位模式，
# 直接传值/传指针/传指针数组三种约定读出来全是 2.0，不会踩野指针。

def _resolve_timescale(rc, f):
    """解析 Time 类与 set_timeScale 方法（每次现解析，不复用旧地址）。
    返回 (domain, m_set)。"""
    s_asm = rc.write_str('UnityEngine.CoreModule')
    s_ns = rc.write_str('UnityEngine')
    s_cls = rc.write_str('Time')
    s_set = rc.write_str('set_timeScale')
    _, r = rc.run_sequence([
        (f['mono_get_root_domain'], []),
        (f['mono_thread_attach'], [('slot', 0)]),
        (f['mono_image_loaded'], [s_asm]),
        (f['mono_thread_detach'], [('slot', 1)]),
    ])
    domain, image = r[0], r[2]
    if not domain or not image:
        raise TrainerError('游戏脚本尚未加载完成，请进入游戏画面后再设置倍速')
    _, r = rc.run_sequence([
        (f['mono_thread_attach'], [domain]),
        (f['mono_class_from_name'], [image, s_ns, s_cls]),
        (f['mono_thread_detach'], [('slot', 0)]),
    ])
    klass = r[1]
    if not klass:
        raise TrainerError('未找到游戏的时间逻辑（游戏版本可能不匹配）')
    _, r = rc.run_sequence([
        (f['mono_thread_attach'], [domain]),
        (f['mono_class_get_method_from_name'], [klass, s_set, 1]),
        (f['mono_thread_detach'], [('slot', 0)]),
    ])
    m_set = r[1]
    if not m_set:
        raise TrainerError('未找到游戏的倍速设置入口（游戏版本可能不匹配）')
    return domain, m_set


def _alloc_scale_magic(rc, pm, scale):
    """分配"地址==float位模式"的参数缓冲，返回位模式（同时作参数地址）。"""
    bits = struct.unpack('<I', struct.pack('<f', float(scale)))[0]
    if bits & 0xFFF:
        raise TrainerError('该倍数无法设置（请选界面上提供的档位）')
    p = kernel32.VirtualAllocEx(pm.handle, ctypes.c_void_p(bits), 0x1000,
                                MEM_COMMIT | MEM_RESERVE, 0x04)
    if not p or p != bits:
        raise TrainerError('倍速参数缓冲被占用，请稍后再试')
    rc._mems.append(p)
    pm.write(bits, struct.pack('<Q', bits))
    return bits


def set_game_speed(pm, scale):
    """一次性把 Time.timeScale 设为指定倍数（1.0 = 恢复正常速度）。"""
    base = remote_module_base(pm.pid, MONO_DLL)
    if not base:
        raise TrainerError('取不到游戏脚本运行时（游戏可能已退出）')
    f = {k: base + v for k, v in MONO_RVAS.items()}
    rc = RemoteCall(pm)
    try:
        domain, m_set = _resolve_timescale(rc, f)
        bits = _alloc_scale_magic(rc, pm, scale)
        exc_buf = rc.alloc(8)
        pm.write(exc_buf, b'\x00' * 8)
        rc.run_sequence([
            (f['mono_thread_attach'], [domain]),
            (f['mono_runtime_invoke'], [m_set, 0, bits, exc_buf]),
            (f['mono_thread_detach'], [('slot', 0)]),
        ], timeout_ms=20000)
        exc = struct.unpack('<Q', pm.read(exc_buf, 8))[0]
        if exc:
            raise TrainerError('游戏内部设置倍速时发生异常')
    finally:
        rc.free_all()


class GameSpeedKeeper:
    """游戏整体倍速维持线程：每 interval 秒把 Time.timeScale 重写为目标倍数
    （游戏暂停/加载可能改回去，这里持续保持）。stop() 只停线程，
    恢复 1.0 由界面层调 set_game_speed 完成，避免线程并发操作同一进程句柄。"""

    def __init__(self, pm, scale, on_error=None, interval=0.25):
        self.pm = pm
        self.scale = float(scale)
        self.on_error = on_error
        self.interval = interval
        self._f = {}
        self._stop = threading.Event()
        self._thread = None

    def start(self):
        if self._thread and self._thread.is_alive():
            return
        self._stop.clear()
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def stop(self):
        self._stop.set()
        if self._thread and self._thread is not threading.current_thread():
            self._thread.join(timeout=2)
            self._thread = None

    def is_running(self):
        return bool(self._thread and self._thread.is_alive())

    def _run(self):
        rc = None
        errs = 0
        try:
            base = remote_module_base(self.pm.pid, MONO_DLL)
            if not base:
                raise TrainerError('取不到游戏脚本运行时（游戏可能已退出）')
            self._f = {k: base + v for k, v in MONO_RVAS.items()}
            rc = RemoteCall(self.pm)
            domain, m_set = _resolve_timescale(rc, self._f)
            bits = _alloc_scale_magic(rc, self.pm, self.scale)
            exc_buf = rc.alloc(8)
            while not self._stop.is_set():
                try:
                    self.pm.write(exc_buf, b'\x00' * 8)
                    rc.run_sequence([
                        (self._f['mono_thread_attach'], [domain]),
                        (self._f['mono_runtime_invoke'],
                         [m_set, 0, bits, exc_buf]),
                        (self._f['mono_thread_detach'], [('slot', 0)]),
                    ], timeout_ms=20000)
                    exc = struct.unpack('<Q', self.pm.read(exc_buf, 8))[0]
                    if exc:
                        raise TrainerError('游戏内部保持倍速时发生异常')
                    errs = 0
                except Exception:                          # noqa: BLE001
                    errs += 1
                    if errs >= 5:                          # 连续失败=游戏退了
                        raise
                self._stop.wait(self.interval)
        except Exception as e:                             # noqa: BLE001
            if self.on_error:
                try:
                    self.on_error(e)
                except Exception:                          # noqa: BLE001
                    pass
        finally:
            if rc:
                try:
                    rc.free_all()
                except Exception:                          # noqa: BLE001
                    pass


class ManaKeeper:
    """最大蓝条保持线程：持续把主角/同伴的当前蓝量重写成属性上限，
    蓝条永远是满的、放技能也不掉蓝。stop() 只停线程（蓝量停在满值，
    关掉后游戏按上限正常消耗即可，无需回写）。

    必须调游戏自身的 GameEntity.ReplaceMana(int) 触发 HUD 刷新，只改
    内存的话蓝条监听器不触发、蓝条纹丝不动。角色识别用 GetEntities()
    加逐个 isPlayerControlled 过滤，全部是无参/单 int 参数的安全约定。"""

    MANA_STAT_OFF = 0x18          # StatsComponent 字段序 mana（IntStat*）所在
    MANA_VALUE_OFF = 0x10         # ManaComponent.value（int32，对象 +0x10）

    def __init__(self, pm, on_error=None, interval=0.2):
        self.pm = pm
        self.on_error = on_error
        self.interval = interval
        self._f = {}
        self._rc = None
        self._domain = 0
        self._exc_buf = 0
        self._players = []
        self._conv = None          # int 传参约定（ptr/inline），首拍判定后固化
        self._name_cache = {}
        self._stop = threading.Event()
        self._thread = None

    def start(self):
        if self._thread and self._thread.is_alive():
            return
        self._stop.clear()
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def stop(self):
        self._stop.set()
        if self._thread and self._thread is not threading.current_thread():
            self._thread.join(timeout=2)
            self._thread = None

    def is_running(self):
        return bool(self._thread and self._thread.is_alive())

    # ---------------- 远程调用小工具 ----------------
    def _seq(self, steps):
        """attach -> steps -> detach，返回各步返回值（r[0]=attach 的返回）"""
        _, r = self._rc.run_sequence(
            [(self._f['mono_thread_attach'], [self._domain])] + steps +
            [(self._f['mono_thread_detach'], [('slot', 0)])], timeout_ms=20000)
        return r

    def _exc(self):
        """读异常槽：正常返回 ''，异常返回异常类名并清槽"""
        exc = struct.unpack('<Q', self.pm.read(self._exc_buf, 8))[0]
        if not exc:
            return ''
        self.pm.write(self._exc_buf, b'\x00' * 8)
        try:
            evt = struct.unpack('<Q', self.pm.read(exc, 8))[0]
            return resolve_class_name(self.pm, evt) or '未知异常'
        except Exception:                              # noqa: BLE001
            return '未知异常'

    def _inv(self, m, this=0, arg=0):
        self.pm.write(self._exc_buf, b'\x00' * 8)
        r = self._seq([(self._f['mono_runtime_invoke'],
                        [m, this, arg, self._exc_buf])])
        return r[1], self._exc()

    def _find_method(self, k, name_s, argc):
        """本类找不到就沿父类链找（GetEntities 定义在 Entitas 泛型基类上）"""
        for _ in range(6):
            if not k:
                return 0
            r = self._seq([(self._f['mono_class_get_method_from_name'],
                            [k, name_s, argc])])
            if r[1]:
                return r[1]
            r = self._seq([(self._f['mono_class_get_parent'], [k])])
            k = r[1]
        return 0

    def _cls(self, obj):
        """对象 -> 类名（先取对象头 vtable 再解析，带缓存）"""
        vtp = struct.unpack('<Q', self.pm.read(obj, 8))[0]
        if vtp not in self._name_cache:
            self._name_cache[vtp] = resolve_class_name(self.pm, vtp)
        return self._name_cache[vtp]

    def _u64(self, addr):
        return struct.unpack('<Q', self.pm.read(addr, 8))[0]

    def _i32(self, addr):
        return struct.unpack('<i', self.pm.read(addr, 4))[0]

    def _bool(self, ret):
        """mono_runtime_invoke 的 bool 返回是装箱对象（真值在对象 +0x10），
        也可能是裸 0/1 —— 按值大小自动区分（托管堆指针必 >= 4GB）。"""
        if ret >= 0x100000000 and ret % 8 == 0:
            return self.pm.read(ret + 0x10, 1)[0] == 1
        return (ret & 0xFF) == 1

    def _int(self, ret):
        """int 返回值同样可能是装箱 Int32（值在 +0x10）或裸值，自动区分"""
        if ret >= 0x100000000 and ret % 8 == 0:
            return self._i32(ret + 0x10)
        return struct.unpack('<i', struct.pack('<I', ret & 0xFFFFFFFF))[0]

    # ---------------- 解析与链路 ----------------
    def _setup(self):
        """一次性解析域/镜像/类/方法（地址每局都会变，绝不复用旧值）"""
        base = remote_module_base(self.pm.pid, MONO_DLL)
        if not base:
            raise TrainerError('取不到游戏脚本运行时（游戏可能已退出）')
        self._f = {k: base + v for k, v in MONO_RVAS.items()}
        rc = RemoteCall(self.pm)
        self._rc = rc
        s_asm = rc.write_str('Assembly-CSharp')
        s_ns = rc.write_str('')
        s_ctx = rc.write_str('Contexts')
        s_ge = rc.write_str('GameEntity')
        s_shared = rc.write_str('get_sharedInstance')
        s_game = rc.write_str('get_game')
        s_ents = rc.write_str('GetEntities')
        s_haspc = rc.write_str('get_isPlayerControlled')
        s_stats = rc.write_str('get_stats')
        s_mana = rc.write_str('get_mana')
        s_repl = rc.write_str('ReplaceMana')
        s_val = rc.write_str('get_Value')
        self._s_val = s_val
        self._exc_buf = rc.alloc(8)
        self.pm.write(self._exc_buf, b'\x00' * 8)
        _, r = rc.run_sequence([
            (self._f['mono_get_root_domain'], []),
            (self._f['mono_thread_attach'], [('slot', 0)]),
            (self._f['mono_image_loaded'], [s_asm]),
            (self._f['mono_thread_detach'], [('slot', 1)]),
        ])
        self._domain, image = r[0], r[2]
        if not self._domain or not image:
            raise TrainerError('游戏脚本尚未加载完成，请进入游戏画面后再使用')
        r = self._seq([(self._f['mono_class_from_name'], [image, s_ns, s_ctx]),
                       (self._f['mono_class_from_name'], [image, s_ns, s_ge])])
        k_ctx, k_ge = r[1], r[2]
        if not k_ctx or not k_ge:
            raise TrainerError('未找到游戏的角色逻辑（游戏版本可能不匹配）')
        m_shared = self._find_method(k_ctx, s_shared, 0)
        if not m_shared:
            raise TrainerError('未找到游戏的角色逻辑（游戏版本可能不匹配）')
        shared, e = self._inv(m_shared)
        if e or not shared:
            raise TrainerError('读不到游戏上下文（请先进入可操作角色的画面）')
        k_shared = self._seq([(self._f['mono_object_get_class'], [shared])])[1]
        m_game = self._find_method(k_shared, s_game, 0)
        if not m_game:
            raise TrainerError('未找到游戏的角色逻辑（游戏版本可能不匹配）')
        game, e = self._inv(m_game, shared)
        if e or not game:
            raise TrainerError('读不到游戏状态（请先进入可操作角色的画面）')
        k_game = self._seq([(self._f['mono_object_get_class'], [game])])[1]
        m_ents = self._find_method(k_game, s_ents, 0)
        m_haspc = self._find_method(k_ge, s_haspc, 0)
        m_stats = self._find_method(k_ge, s_stats, 0)
        m_mana = self._find_method(k_ge, s_mana, 0)
        m_repl = self._find_method(k_ge, s_repl, 1)
        if not all((m_ents, m_haspc, m_stats, m_mana, m_repl)):
            raise TrainerError('未找到游戏的蓝量入口（游戏版本可能不匹配）')
        self._game = game
        self._m_ents, self._m_haspc = m_ents, m_haspc
        self._m_stats, self._m_mana, self._m_repl = m_stats, m_mana, m_repl

    def _drop_players(self):
        """回收旧玩家链路的参数缓冲"""
        for p in self._players:
            try:
                self._rc.free(p['buf'])
            except Exception:                          # noqa: BLE001
                pass
        self._players = []

    def _chain(self):
        """重拉角色列表并解析每个玩家的蓝量链路（实体/组件换场景会被
        游戏重建，所以每条链路都现场重取）。返回玩家数，0=画面里没有
        可操作角色（菜单/过场），属正常等待不算错。"""
        self._drop_players()
        arr, e = self._inv(self._m_ents, self._game)
        if e or not arr:
            raise TrainerError('读不到角色列表%s' % (('：' + e) if e else ''))
        # Mono 数组：数据区偏移与长度位置做小范围组合验证（前两元素必须是 GameEntity）
        data_off, count = 0, 0
        for off in (0x18, 0x20, 0x10, 0x28):
            try:
                n = self._u64(arr + off - 8)
                if not (1 <= n <= 200000):
                    continue
                if self._cls(self._u64(arr + off)) != 'GameEntity':
                    continue
                if n >= 2 and self._cls(self._u64(arr + off + 8)) != 'GameEntity':
                    continue
                data_off, count = off, int(n)
                break
            except Exception:                          # noqa: BLE001
                continue
        if not data_off:
            raise TrainerError('角色列表格式变了（游戏版本可能不匹配）')
        # 批量过滤出玩家（每批 50 个调用只跑一次远程序）
        found = []
        for i in range(0, count, 50):
            ents = [self._u64(arr + data_off + 8 * j)
                    for j in range(i, min(i + 50, count))]
            self.pm.write(self._exc_buf, b'\x00' * 8)
            r = self._seq([(self._f['mono_runtime_invoke'],
                            [self._m_haspc, ep, 0, self._exc_buf])
                           for ep in ents])
            e = self._exc()
            if e:
                raise TrainerError('识别角色时游戏内部异常（%s）' % e)
            found.extend(ents[j] for j in range(len(ents))
                         if self._bool(r[1 + j]))
            if len(found) >= 2:                        # 主角+同伴就两个，够用即止
                break
        for E in found[:2]:
            sc, e = self._inv(self._m_stats, E)
            if e or not sc:
                continue
            try:
                intstat = self._u64(sc + self.MANA_STAT_OFF)
                if self._cls(intstat) != 'IntStat':
                    continue
            except Exception:                          # noqa: BLE001
                continue
            k_is = self._seq([(self._f['mono_object_get_class'], [intstat])])[1]
            m_val = self._find_method(k_is, self._s_val, 0)
            if not m_val:
                continue
            buf = self._rc.alloc(0x40)
            self.pm.write(buf, struct.pack('<Q', buf + 8))   # 基线 ptr 布局
            self._players.append({'e': E, 'intstat': intstat,
                                  'mval': m_val, 'buf': buf})
        return len(self._players)

    # ---------------- 写入与约定判定 ----------------
    def _set_read(self, p, V):
        """发一次 ReplaceMana 再读回当前蓝量"""
        _, e = self._inv(self._m_repl, p['e'], p['buf'])
        if e:
            raise TrainerError('刷新蓝条时游戏内部异常（%s）' % e)
        mo, e = self._inv(self._m_mana, p['e'])
        if e or not mo:
            raise TrainerError('读取蓝量失败（%s）' % (e or '对象为空'))
        return self._i32(mo + self.MANA_VALUE_OFF) & 0xFFFFFFFF

    def _probe_conv(self, p, V):
        """判定 mono_runtime_invoke 的 int 传参约定，判一次终身有效：
          ptr    —— 参数是指向值的指针：buf[0]=buf+8、值放 buf+8（Mono 标准）
          inline —— 参数槽直接装值：读到的正是 buf[0] 的低 32 位
        两种约定下先发的都是 ptr 布局：ptr 语义解引用的是 buf+8（合法内存），
        直传语义只是把地址位当整数读走 —— 都不解引用野地址。只有按
        “读到的正好等于地址位”确认是直传后，才补发裸值布局。"""
        buf = p['buf']
        self.pm.write(buf, struct.pack('<Q', buf + 8))
        self.pm.write(buf + 8, struct.pack('<i', V))
        got = self._set_read(p, V)
        if got == V:
            self._conv = 'ptr'
            return
        if got == ((buf + 8) & 0xFFFFFFFF):      # 直传约定读走了指针位
            self._conv = 'inline'
            self.pm.write(buf, struct.pack('<Q', V))
            if self._set_read(p, V) == V:
                return
            self._conv = None
        raise TrainerError('写入蓝量没生效，请关掉开关再试一次')

    def _tick(self):
        players = self._players
        n = len(players)
        # 一拍：读各自蓝量上限
        self.pm.write(self._exc_buf, b'\x00' * 8)
        r = self._seq([(self._f['mono_runtime_invoke'],
                        [p['mval'], p['intstat'], 0, self._exc_buf])
                       for p in players])
        e = self._exc()
        if e:
            raise TrainerError('读取蓝量上限时游戏内部异常（%s）' % e)
        targets = [(players[i], self._int(r[1 + i])) for i in range(n)]
        targets = [(p, V) for p, V in targets if 0 < V < 10_000_000]
        if not targets:
            return                                 # 菜单/过场读不到蓝量，等下一拍
        if self._conv is None:                     # 首拍判定传参约定
            self._probe_conv(*targets[0])
        # 二拍：按上限写蓝 + 走 ReplaceMana 触发 HUD 刷新
        for p, V in targets:
            if self._conv == 'ptr':
                self.pm.write(p['buf'] + 8, struct.pack('<i', V))
            else:
                self.pm.write(p['buf'], struct.pack('<Q', V))
        steps = []
        for p, V in targets:
            steps.append((self._f['mono_runtime_invoke'],
                          [self._m_repl, p['e'], p['buf'], self._exc_buf]))
        for p, V in targets:
            steps.append((self._f['mono_runtime_invoke'],
                          [self._m_mana, p['e'], 0, self._exc_buf]))
        self.pm.write(self._exc_buf, b'\x00' * 8)
        r = self._seq(steps)
        e = self._exc()
        if e:
            raise TrainerError('刷新蓝条时游戏内部异常（%s）' % e)
        # 回读确认真的写进去了（没写进去=链路失效，抛出去重挂）
        for i, (p, V) in enumerate(targets):
            mo = r[1 + len(targets) + i]
            if not mo or self._i32(mo + self.MANA_VALUE_OFF) != V:
                raise TrainerError('蓝量写入未生效')

    def _run(self):
        errs = 0
        tick = 0
        try:
            self._setup()
            while not self._stop.is_set():
                try:
                    if not self._players or tick % 8 == 0:
                        self._chain()               # 每 8 拍顺带刷新属性链路
                    tick += 1
                    if self._players:
                        self._tick()
                    errs = 0
                except Exception:                    # noqa: BLE001
                    self._drop_players()             # 链路失效，下一拍整体重挂
                    errs += 1
                    if errs >= 5:                    # 连续失败=游戏退了
                        raise
                self._stop.wait(self.interval)
        except Exception as e:                       # noqa: BLE001
            if self.on_error:
                try:
                    self.on_error(e)
                except Exception:                    # noqa: BLE001
                    pass
        finally:
            if self._rc:
                try:
                    self._rc.free_all()
                except Exception:                    # noqa: BLE001
                    pass


# ============================================================
# 一键满级（远程调用游戏自身的"加经验 -> 自动升级"流程）
# ============================================================
# 反编译实锤（Assembly-CSharp.dll）：
#   · 等级存在 gameState 唯一实体的 LevelComponent.value（int，对象 +0x10），
#     主角和同伴共享同一等级；上限 999（LevelUpSystem 内 clamp）。
#   · 升级必须走 LevelUpSystem：它按经验差连升多级，并做属性成长、
#     血蓝刷新、存档、成就 —— 只改等级数字的话血量攻击不会涨，
#     所以这里注入经验，让游戏自己的流程把等级和属性一起做满。
#   · 加经验入口 GameStateContext.ReplaceExperience(int)，每个逻辑帧
#     LevelUpSystem 都会消费经验执行升级。
MAX_LEVEL = 999

# "地址 == 值"的自引用参数候选（都正好落在页内偏移 0x400，且是正整数）。
# 0x77359400 = 2000000000 恰好等于期望传入的经验证。
_MAGIC_CANDIDATES = (0x77359400, 0x60000400, 0x50000400,
                     0x40000400, 0x30000400, 0x20000400)


def _alloc_selfref_int(pm, rc, value):
    """分配"槽位地址 == 槽位值"的 int 参数缓冲，返回槽位地址（失败返回 0）。

    mono_runtime_invoke 对值类型参数的传法在不同 Mono 实现里有三种：
    直接放值 / 放指向值的指针 / 指针数组。把地址和值做成同一个数，
    三种解释读到的整数完全相同 —— 无论游戏用哪种约定都不会传错参，
    也杜绝了"按错约定解引用非法指针"导致的游戏崩溃。"""
    page = value & ~0xFFF
    p = kernel32.VirtualAllocEx(pm.handle, ctypes.c_void_p(page), 0x1000,
                                MEM_COMMIT | MEM_RESERVE, 0x04)
    if not p or (p & ~0xFFF) != page:
        return 0                                   # 该地址被占用，换候选
    try:
        pm.write(value, struct.pack('<Q', value))  # [0x77359400] = 0x77359400
    except TrainerError:
        kernel32.VirtualFreeEx(pm.handle, p, 0, MEM_RELEASE)
        return 0
    rc._mems.append(p)                             # 结束时随 free_all 释放
    return value


def max_level_up(pm, on_progress=None, cap=MAX_LEVEL, max_rounds=400):
    """一键满级：远程调用 ReplaceExperience 注入经验，让游戏自己的
    LevelUpSystem 每帧消费经验、一路升到 cap 级（属性/血蓝/存档/成就
    全走原生逻辑）。返回最终等级；on_progress(level) 每轮回调一次。"""
    base = remote_module_base(pm.pid, MONO_DLL)
    if not base:
        raise TrainerError('取不到游戏脚本运行时（游戏可能已退出）')
    f = {k: base + v for k, v in MONO_RVAS.items()}
    rc = RemoteCall(pm)
    try:
        # ---- 解析类与方法（每次现解析，绝不复用上一局地址） ----
        s_asm = rc.write_str('Assembly-CSharp')
        s_ns = rc.write_str('')
        s_ctx = rc.write_str('Contexts')
        s_gsc = rc.write_str('GameStateContext')
        s_shared = rc.write_str('get_sharedInstance')
        s_gstate = rc.write_str('get_gameState')
        s_rexp = rc.write_str('ReplaceExperience')
        s_level = rc.write_str('get_level')
        s_xp = rc.write_str('get_experience')
        _, r = rc.run_sequence([
            (f['mono_get_root_domain'], []),
            (f['mono_thread_attach'], [('slot', 0)]),
            (f['mono_image_loaded'], [s_asm]),
            (f['mono_thread_detach'], [('slot', 1)]),
        ])
        domain, image = r[0], r[2]
        if not domain or not image:
            raise TrainerError('游戏脚本尚未加载完成，请进入游戏画面后再使用')
        _, r = rc.run_sequence([
            (f['mono_thread_attach'], [domain]),
            (f['mono_class_from_name'], [image, s_ns, s_ctx]),
            (f['mono_class_from_name'], [image, s_ns, s_gsc]),
            (f['mono_thread_detach'], [('slot', 0)]),
        ])
        klass_ctx, klass_gsc = r[1], r[2]
        if not klass_ctx or not klass_gsc:
            raise TrainerError('未找到游戏的等级逻辑（游戏版本可能不匹配）')
        _, r = rc.run_sequence([
            (f['mono_thread_attach'], [domain]),
            (f['mono_class_get_method_from_name'], [klass_ctx, s_shared, 0]),
            (f['mono_class_get_method_from_name'], [klass_ctx, s_gstate, 0]),
            (f['mono_class_get_method_from_name'], [klass_gsc, s_rexp, 1]),
            (f['mono_class_get_method_from_name'], [klass_gsc, s_level, 0]),
            (f['mono_class_get_method_from_name'], [klass_gsc, s_xp, 0]),
            (f['mono_thread_detach'], [('slot', 0)]),
        ])
        m_shared, m_gstate, m_rexp = r[1], r[2], r[3]
        m_level, m_xp = r[4], r[5]
        if not all((m_shared, m_gstate, m_rexp, m_level, m_xp)):
            raise TrainerError('未找到游戏的升级入口（游戏版本可能不匹配）')

        magic = 0
        for cand in _MAGIC_CANDIDATES:
            magic = _alloc_selfref_int(pm, rc, cand)
            if magic:
                break
        if not magic:
            raise TrainerError('无法在游戏内分配参数缓冲区，请稍后再试')
        exc_buf = rc.alloc(8)

        def _check_exc():
            exc = struct.unpack('<Q', pm.read(exc_buf, 8))[0]
            if not exc:
                return
            name = None
            try:
                evt = struct.unpack('<Q', pm.read(exc, 8))[0]
                name = resolve_class_name(pm, evt)
            except TrainerError:
                pass
            raise TrainerError('游戏内部执行升级时发生异常（%s）'
                               % (name or '未知异常'))

        def _context():
            """取 gameState 上下文；取不到 = 不在可玩画面"""
            pm.write(exc_buf, b'\x00' * 8)
            _, r = rc.run_sequence([
                (f['mono_thread_attach'], [domain]),
                (f['mono_runtime_invoke'], [m_shared, 0, 0, exc_buf]),
                (f['mono_runtime_invoke'], [m_gstate, ('slot', 1), 0, exc_buf]),
                (f['mono_thread_detach'], [('slot', 0)]),
            ], timeout_ms=20000)
            _check_exc()
            if not r[2]:
                raise TrainerError('当前读不到游戏状态，请进入游戏画面（可操作角色）后再试')
            return r[2]

        def _inject(ctx):
            pm.write(exc_buf, b'\x00' * 8)
            rc.run_sequence([
                (f['mono_thread_attach'], [domain]),
                (f['mono_runtime_invoke'], [m_rexp, ctx, magic, exc_buf]),
                (f['mono_thread_detach'], [('slot', 0)]),
            ], timeout_ms=20000)
            _check_exc()

        def _read_state(ctx):
            pm.write(exc_buf, b'\x00' * 8)
            _, r = rc.run_sequence([
                (f['mono_thread_attach'], [domain]),
                (f['mono_runtime_invoke'], [m_level, ctx, 0, exc_buf]),
                (f['mono_runtime_invoke'], [m_xp, ctx, 0, exc_buf]),
                (f['mono_thread_detach'], [('slot', 0)]),
            ], timeout_ms=20000)
            _check_exc()
            lvl_obj, xp_obj = r[1], r[2]
            if not lvl_obj:
                raise TrainerError('读不到等级数据，请确认已进入游戏画面后再试')
            level = struct.unpack('<i', pm.read(lvl_obj + 0x10, 4))[0]
            if not (0 <= level <= 100000):
                raise TrainerError('等级数据异常，请重试')
            exp = None
            if xp_obj:
                try:
                    exp = struct.unpack('<i', pm.read(xp_obj + 0x10, 4))[0]
                except TrainerError:
                    exp = None
            return level, exp, xp_obj

        # ---- 主循环：注入 -> 等游戏逻辑帧消费 -> 读等级 ----
        ctx = _context()
        level, _, _ = _read_state(ctx)
        if on_progress:
            on_progress(level)
        injected = False
        stall = 0
        rounds = 0
        while level < cap and rounds < max_rounds:
            rounds += 1
            _inject(ctx)
            injected = True
            time.sleep(0.06)            # 留出几个逻辑帧让 LevelUpSystem 消费经验
            new_level, _, _ = _read_state(ctx)
            if on_progress:
                on_progress(new_level)
            if new_level > level:
                stall = 0
            else:
                stall += 1
                if stall >= 40:         # 连续十几秒毫无推进 = 游戏在暂停/菜单
                    raise TrainerError(
                        '升级没有推进（当前 %d 级）：请确认游戏画面正常'
                        '（不在暂停或主菜单）后重新点一次' % level)
            level = new_level
        if level < cap:
            raise TrainerError('尝试多轮仍未升满（当前 %d 级），请稍后再试' % level)

        # ---- 收尾：清掉注入剩余的经验，避免经验条残留大数字 ----
        # 无论循环走没走（例如点按钮时已是满级、或上一次中断残留），都要清
        try:
            _, exp, xp_obj = _read_state(ctx)
            if xp_obj and exp:
                pm.write(xp_obj + 0x10, struct.pack('<i', 0))
        except TrainerError:
            pass
        return level
    finally:
        rc.free_all()


def set_max_gold(pm):
    """一键满金币：远程调用游戏自己的 GameStateContext.ReplaceGold 设置金币。
    金币数值、界面计数器刷新、后续自动存档全走原生逻辑（等同捡到金币的效果）。
    返回 (当前金币, 是否发生了修改)。"""
    base = remote_module_base(pm.pid, MONO_DLL)
    if not base:
        raise TrainerError('取不到游戏脚本运行时（游戏可能已退出）')
    f = {k: base + v for k, v in MONO_RVAS.items()}
    rc = RemoteCall(pm)
    try:
        # ---- 解析类与方法（每次现解析，绝不复用上一局地址） ----
        s_asm = rc.write_str('Assembly-CSharp')
        s_ns = rc.write_str('')
        s_ctx = rc.write_str('Contexts')
        s_gsc = rc.write_str('GameStateContext')
        s_shared = rc.write_str('get_sharedInstance')
        s_gstate = rc.write_str('get_gameState')
        s_regold = rc.write_str('ReplaceGold')
        s_gold = rc.write_str('get_gold')
        _, r = rc.run_sequence([
            (f['mono_get_root_domain'], []),
            (f['mono_thread_attach'], [('slot', 0)]),
            (f['mono_image_loaded'], [s_asm]),
            (f['mono_thread_detach'], [('slot', 1)]),
        ])
        domain, image = r[0], r[2]
        if not domain or not image:
            raise TrainerError('游戏脚本尚未加载完成，请进入游戏画面后再使用')
        _, r = rc.run_sequence([
            (f['mono_thread_attach'], [domain]),
            (f['mono_class_from_name'], [image, s_ns, s_ctx]),
            (f['mono_class_from_name'], [image, s_ns, s_gsc]),
            (f['mono_thread_detach'], [('slot', 0)]),
        ])
        klass_ctx, klass_gsc = r[1], r[2]
        if not klass_ctx or not klass_gsc:
            raise TrainerError('未找到游戏的金币逻辑（游戏版本可能不匹配）')
        _, r = rc.run_sequence([
            (f['mono_thread_attach'], [domain]),
            (f['mono_class_get_method_from_name'], [klass_ctx, s_shared, 0]),
            (f['mono_class_get_method_from_name'], [klass_ctx, s_gstate, 0]),
            (f['mono_class_get_method_from_name'], [klass_gsc, s_regold, 1]),
            (f['mono_class_get_method_from_name'], [klass_gsc, s_gold, 0]),
            (f['mono_thread_detach'], [('slot', 0)]),
        ])
        m_shared, m_gstate, m_regold, m_gold = r[1], r[2], r[3], r[4]
        if not all((m_shared, m_gstate, m_regold, m_gold)):
            raise TrainerError('未找到游戏的金币入口（游戏版本可能不匹配）')

        # 金币参数用"地址==值"自引用缓冲（约定差异全兼容，不会传错参）
        magic = 0
        target = 0
        for cand in _MAGIC_CANDIDATES:
            magic = _alloc_selfref_int(pm, rc, cand)
            if magic:
                target = cand
                break
        if not magic:
            raise TrainerError('无法在游戏内分配参数缓冲区，请稍后再试')
        exc_buf = rc.alloc(8)

        def _check_exc():
            exc = struct.unpack('<Q', pm.read(exc_buf, 8))[0]
            if not exc:
                return
            name = None
            try:
                evt = struct.unpack('<Q', pm.read(exc, 8))[0]
                name = resolve_class_name(pm, evt)
            except TrainerError:
                pass
            raise TrainerError('游戏内部设置金币时发生异常（%s）'
                               % (name or '未知异常'))

        def _invoke(method, this, arg):
            pm.write(exc_buf, b'\x00' * 8)
            _, r = rc.run_sequence([
                (f['mono_thread_attach'], [domain]),
                (f['mono_runtime_invoke'], [method, this, arg, exc_buf]),
                (f['mono_thread_detach'], [('slot', 0)]),
            ], timeout_ms=20000)
            _check_exc()
            return r[1]

        def _read_gold(ctx):
            obj = _invoke(m_gold, ctx, 0)
            if not obj:
                raise TrainerError('读不到金币数据，请确认已进入游戏画面后再试')
            return struct.unpack('<i', pm.read(obj + 0x10, 4))[0]

        # ---- 取 gameState 上下文（取不到 = 没进可玩画面） ----
        ctx = _invoke(m_gstate, _invoke(m_shared, 0, 0), 0)
        if not ctx:
            raise TrainerError('当前读不到游戏状态，请进入游戏画面（可操作角色）后再试')

        cur = _read_gold(ctx)
        if cur >= target:
            return cur, False            # 金币已经不比目标少，不往回改

        _invoke(m_regold, ctx, magic)    # 走游戏自己的金币系统
        got = _read_gold(ctx)
        if got != target:
            raise TrainerError('金币设置未生效（当前 %d），请稍后再试' % got)
        return got, True
    finally:
        rc.free_all()
