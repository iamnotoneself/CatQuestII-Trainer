# -*- coding: utf-8 -*-
"""
猫咪斗恶龙2（Cat Quest II）学习用修改器 - 图形界面
仅供个人单机学习研究使用。

启动方式：双击 启动修改器.bat（或 python catquest_trainer.py）
"""

import ctypes
import queue
import struct
import threading
import time
import tkinter as tk
from tkinter import font as tkfont
from tkinter import messagebox, ttk

import trainer_core as tc

KILL_DAMAGE = 1000000    # 秒杀：写入的攻击力（任何攻击一击必杀，且不会溢出）

CARD_BG = '#f7f9fc'      # 功能卡片背景色
CARD_BD = '#d5dde8'      # 功能卡片边框色
BTN_BLUE = '#2d7ff9'     # 主操作按钮（连接/应用修改）
BTN_GREEN = '#22a45d'    # 满级按钮
BTN_RED = '#e05252'      # 恢复原样按钮
BTN_ORANGE = '#f0a020'   # 状态刷新按钮（卡住时的急救入口）


class PillSwitch(tk.Canvas):
    """自绘圆角滑块开关：绿色=开、灰色=关，点击后滑块带动画滑动。
    外部改 variable（例如「全部恢复原样」复位）时外观会自动同步。"""

    W, H = 52, 26

    def __init__(self, master, variable, command=None):
        super().__init__(master, width=self.W, height=self.H,
                         highlightthickness=0, bd=0,
                         bg=master.cget('bg'))
        self._var = variable
        self._cmd = command
        self._state = bool(variable.get())
        self._pos = 1.0 if self._state else 0.0   # 滑块位置 0=关 1=开
        self._anim = None
        self._clicking = False
        fillc = '#2fbf71' if self._state else '#c4cbd4'
        r = self.H / 2
        self._track = [
            self.create_oval(0, 0, self.H, self.H, fill=fillc, width=0),
            self.create_oval(self.W - self.H, 0, self.W, self.H, fill=fillc, width=0),
            self.create_rectangle(r, 0, self.W - r, self.H, fill=fillc, width=0),
        ]
        self._knob = self.create_oval(0, 0, 1, 1, fill='#ffffff', width=0)
        self._redraw()
        self.pack(pady=2)                 # 没有 pack 的话开关根本不会显示
        variable.trace_add('write', self._on_var)
        self.bind('<Button-1>', self._click)
        self.bind('<Enter>', lambda e: self.config(cursor='hand2'))
        self.bind('<Leave>', lambda e: self.config(cursor=''))

    def _click(self, _event=None):
        want = not self._state
        self._set_visual(want)
        self._clicking = True
        try:
            self._var.set(want)
        finally:
            self._clicking = False
        if self._cmd:
            self._cmd()

    def _on_var(self, *_args):
        # 关键：动画进行中外部改值也必须同步！否则"正忙时拨开关被程序拨回"
        # 只改了变量、外观仍停在开，就会出现"开关亮着但功能没生效"的假象。
        if self._clicking:
            return          # 自己点击引发的赋值，外观已经是对的
        v = bool(self._var.get())
        if v != self._state:
            self._set_visual(v)

    def _set_visual(self, on, animate=True):
        self._state = on
        col = '#2fbf71' if on else '#c4cbd4'
        for i in self._track:
            self.itemconfig(i, fill=col)
        target = 1.0 if on else 0.0
        if self._anim:
            self.after_cancel(self._anim)
            self._anim = None
        if not animate:
            self._pos = target
            self._redraw()
            return
        start, steps = self._pos, 8

        def step(i=1):
            self._pos = start + (target - start) * (i / steps)
            self._redraw()
            if i < steps:
                self._anim = self.after(14, step, i + 1)
            else:
                self._anim = None
        step()

    def _redraw(self):
        d = self.H - 6
        cx = 3 + d / 2 + self._pos * (self.W - 6 - d)
        cy = self.H / 2
        self.coords(self._knob, cx - d / 2, cy - d / 2,
                    cx + d / 2, cy + d / 2)

def _rr_points(x1, y1, x2, y2, r):
    """圆角矩形的多边形顶点（配合 create_polygon(smooth=True) 画出圆角）"""
    return [
        x1 + r, y1, x1 + r, y1, x2 - r, y1, x2 - r, y1,
        x2, y1, x2, y1 + r, x2, y1 + r, x2, y2 - r,
        x2, y2 - r, x2, y2, x2 - r, y2, x2 - r, y2,
        x1 + r, y2, x1 + r, y2, x1, y2, x1, y2 - r,
        x1, y2 - r, x1, y1 + r, x1, y1 + r, x1, y1,
    ]


class Segmented(tk.Canvas):
    """倍数选择器：所有档位平铺成一排直接展示，选中框带滑动动画移动到当前档。
    外部改 variable（例如「全部恢复原样」复位、忙时回拨档位）时外观自动同步。"""

    H = 26

    def __init__(self, master, variable, values, command=None, sel_bg='#2d7ff9',
                 track_bg='#e8ecf3', fg='#667',
                 font=('Microsoft YaHei UI', 9)):
        super().__init__(master, width=10, height=self.H, bg=master.cget('bg'),
                         highlightthickness=0, bd=0, cursor='hand2')
        self._var = variable
        self._values = list(values)
        self._cmd = command
        self._fg = fg
        self._font = tkfont.Font(font=font)
        self._padx = 8
        self._segw = [self._font.measure(v) + 2 * self._padx for v in values]
        self._x0 = []                      # 每个档位（含内边距）的左边界
        acc = 2.0
        for w in self._segw:
            self._x0.append(acc)
            acc += w
        self._total = acc + 2
        self.config(width=int(self._total))
        self._track = self.create_polygon(
            _rr_points(0, 0, self._total, self.H, self.H / 2.0),
            smooth=True, fill=track_bg, width=0)
        self._pill = self.create_polygon([0] * 24, smooth=True,
                                         fill=sel_bg, width=0)
        self._texts = []
        for i, v in enumerate(values):
            self._texts.append(self.create_text(
                self._x0[i] + self._segw[i] / 2.0, self.H / 2.0, text=v,
                font=font, fill=fg))
        self._pos = 0.0                    # 选中框当前左边缘（动画用）
        self._sel_i = 0
        self._anim = None
        self._external = False             # 防止自己的点击赋值与回读打架
        self._to(self._index(), instant=True)
        self._var.trace_add('write', self._on_var)
        self.bind('<Button-1>', self._click)
        self.pack(pady=2)

    def _index(self):
        try:
            return self._values.index(self._var.get())
        except ValueError:
            return 0

    def _on_var(self, *_a):
        if self._external:
            return
        self._to(self._index())

    def _click(self, e):
        i = 0
        for k, x in enumerate(self._x0):
            if e.x >= x:
                i = k
            else:
                break
        if self._values[i] == self._var.get():
            self._to(i)
            return
        self._external = True
        try:
            self._var.set(self._values[i])
        finally:
            self._external = False
        self._to(i)
        if self._cmd:
            self._cmd()

    def _to(self, i, instant=False):
        target = self._x0[i] - 2
        if self._anim:
            self.after_cancel(self._anim)
            self._anim = None
        self._sel_i = i
        if instant:
            self._pos = float(target)
            self._redraw()
            self._recolor()
            return
        start, steps = self._pos, 10

        def step(k=1):
            self._pos = start + (target - start) * (k / steps)
            self._redraw()
            if k < steps:
                self._anim = self.after(14, step, k + 1)
            else:
                self._anim = None
                self._recolor()
        step()

    def _redraw(self):
        x = self._pos
        w = self._segw[self._sel_i]
        self.coords(self._pill, *_rr_points(x, 2, x + w, self.H - 2,
                                            (self.H - 4) / 2.0))

    def _recolor(self):
        for i, t in enumerate(self._texts):
            self.itemconfig(t, fill='white' if i == self._sel_i else self._fg)


class Card(tk.Canvas):
    """圆角卡片容器：Canvas 画圆角边框，控件放在内嵌 Frame 里。
    fill_mode='x' 按内容撑高（功能卡片）；'both' 撑满剩余空间（帮助文本）"""

    def __init__(self, master, top=0, fill_mode='x', padx=12, pady=7,
                 radius=12, bg=CARD_BG, border=CARD_BD):
        outer = ''
        try:
            outer = ttk.Style().lookup('TFrame', 'background') or ''
        except Exception:                          # noqa: BLE001
            pass
        super().__init__(master, bg=outer or '#f0f0f0', highlightthickness=0,
                         bd=0)
        self._padx, self._pady, self._r = padx, pady, radius
        self._fill = fill_mode
        self.body = tk.Frame(self, bg=bg)
        self._shape = self.create_polygon([0] * 24, smooth=True, fill=bg,
                                          outline=border, width=1)
        self._win = self.create_window(padx, pady, anchor='nw',
                                       window=self.body)
        self.bind('<Configure>', self._layout)
        self.pack(fill='both' if fill_mode == 'both' else 'x',
                  expand=(fill_mode == 'both'), pady=(top, 8))

    def _layout(self, _e=None):
        need = self.body.winfo_reqheight() + 2 * self._pady
        mapped = self.winfo_ismapped()
        # 按内容撑高；未显示的页签也要先把请求高度定死，
        # 否则窗口尺寸会按 Canvas 的默认高度算（整个窗口被撑爆）
        if (self._fill == 'x' or not mapped) and abs(self.winfo_height() - need) > 1:
            self.configure(height=need)
            return
        w, h = self.winfo_width(), self.winfo_height()
        if w < 12 or h < 12:
            return
        inner_w = max(w - 2 * self._padx, 1)
        kw = {'width': inner_w}
        if self._fill == 'both':
            kw['height'] = max(h - 2 * self._pady, 1)
        self.itemconfig(self._win, **kw)
        self.coords(self._win, self._padx, self._pady)
        r = min(self._r, w / 2.0 - 1, h / 2.0 - 1)
        self.coords(self._shape, *_rr_points(1.0, 1.0, w - 2.0, h - 2.0, r))


def _shade(hexc, delta):
    """把 #rrggbb 颜色整体调亮/调暗 delta（用于按钮悬停效果）"""
    r, g, b = (int(hexc[i:i + 2], 16) for i in (1, 3, 5))
    clamp = lambda v: max(0, min(255, v))          # noqa: E731
    return '#%02x%02x%02x' % (clamp(r + delta), clamp(g + delta),
                              clamp(b + delta))


def _widget_bg(w):
    """取控件背景色（兼容 tk 控件与 ttk 控件）"""
    try:
        v = w.cget('background')
        if v:
            return v
    except Exception:                              # noqa: BLE001
        pass
    try:
        return ttk.Style().lookup(w.winfo_class(), 'background') or '#f0f0f0'
    except Exception:                              # noqa: BLE001
        return '#f0f0f0'


class FlatBtn(tk.Canvas):
    """圆角扁平按钮：外观是圆角矩形，用法与 tk.Button 兼容
    （可直接 pack，支持 config(state='disabled'/'normal')）。"""

    def __init__(self, master, text, bg, command, bold=True, padx=14, pady=4):
        super().__init__(master, bg=_widget_bg(master), highlightthickness=0,
                         bd=0, cursor='hand2')
        self._bg = bg
        self._dis_bg = '#b6bdc7'
        self._command = command
        self._state = 'normal'
        self._font = tkfont.Font(font=('Microsoft YaHei UI', 10,
                                       'bold' if bold else 'normal'))
        w = self._font.measure(text) + 2 * padx
        h = self._font.metrics('linespace') + 2 * pady
        super().configure(width=w, height=h)
        self._shape = self.create_polygon(
            _rr_points(0.0, 0.0, w, h, min(8.0, h / 2.0)),
            smooth=True, fill=bg, width=0)
        self.create_text(w / 2.0, h / 2.0, text=text, font=self._font,
                         fill='white')
        self.bind('<Button-1>', self._on_click)
        self.bind('<Enter>', self._on_enter)
        self.bind('<Leave>', self._on_leave)

    def configure(self, cnf=None, **kw):           # noqa: A003
        if isinstance(cnf, dict):
            kw = {**cnf, **kw}
        st = kw.pop('state', None)
        if st is not None:
            self._set_state(st)
        if kw:
            return super().configure(**kw)

    config = configure

    def _set_state(self, st):
        self._state = st
        if st == 'disabled':
            self.config(cursor='')
            self.itemconfig(self._shape, fill=self._dis_bg)
        else:
            self.config(cursor='hand2')
            self.itemconfig(self._shape, fill=self._bg)

    def _on_enter(self, _e):
        if self._state == 'normal':
            self.itemconfig(self._shape, fill=_shade(self._bg, -22))

    def _on_leave(self, _e):
        if self._state == 'normal':
            self.itemconfig(self._shape, fill=self._bg)

    def _on_click(self, _e):
        if self._state == 'normal' and self._command:
            self._command()


class TrainerApp:
    def __init__(self, root):
        self.root = root
        self.root.title('猫咪斗恶龙2 学习用修改器（仅供研究参考）')
        self.root.geometry('760x760')
        self.root.minsize(680, 640)

        self._busy = False
        self.pm = None                      # ProcessMemory
        self.attached_pid = None
        self.status = tk.StringVar(value='就绪')
        self.proc_status = tk.StringVar(value='未附加')
        self.adv_status = tk.StringVar(value='未识别（点「连接并识别角色」）')
        self._q = queue.Queue()             # 工作线程 -> 主线程的结果队列

        self.freezer = tc.ValueFreezer()
        self.adv_players = None         # 一键功能：定位到的玩家属性信息
        self.adv_orig = None            # 首次定位时的原始值（用于恢复）
        self.flag_keeper = None         # 无敌自愈维持线程（对抗组件被游戏重建）
        self.pickup_keeper = None       # 全屏拾取巡逻线程
        self._pickup_total = 0          # 本次拾取累计数值（仅用于状态提示）
        self.adv_god = False            # 无敌开关状态（后台自愈线程读取）
        self.adv_kill = False           # 秒杀开关状态
        self.adv_speed = 1.0            # 移速倍数状态
        self.speed_keeper = None        # 游戏整体倍速保持线程
        self._gspeed = '不变'           # 游戏速度当前档位（下拉框回拨用）
        self.mana_keeper = None         # 最大蓝条保持线程
        self._relocating = False        # 防止换场景自动重连重复触发
        self._last_rebuild = 0.0        # 自动重连节流时间戳
        self._run_gen = 0               # 任务代号：状态刷新后旧任务的结果一律作废
        self._cur_btn = None            # 当前被任务禁用的按钮（复位时恢复可点）

        self._build_ui()
        self.root.after(80, self._poll_queue)
        self.root.after(50, self._round_window)

    def _round_window(self):
        """Windows 11：把整个窗口（含标题栏）改成圆角"""
        try:
            u32 = ctypes.windll.user32
            dwm = ctypes.windll.dwmapi
            val = ctypes.c_int(2)              # DWMWCP_ROUND
            seen = set()
            for h in (self.root.winfo_id(),
                      u32.GetParent(self.root.winfo_id())):
                if not h or h in seen:
                    continue
                seen.add(h)
                try:
                    dwm.DwmSetWindowAttribute(int(h), 33, ctypes.byref(val), 4)
                except Exception:              # noqa: BLE001 老系统不支持
                    pass
        except Exception:                      # noqa: BLE001
            pass

    # ---------------- UI 搭建 ----------------
    def _build_ui(self):
        nb = ttk.Notebook(self.root)
        nb.pack(fill='both', expand=True, padx=6, pady=6)
        self.nb = nb

        self._build_adv_tab(nb)
        self._build_ocd_tab(nb)
        self._build_help_tab(nb)

        bar = ttk.Frame(self.root)
        bar.pack(fill='x', side='bottom', padx=6, pady=(0, 6))
        ttk.Label(bar, textvariable=self.status, anchor='w').pack(
            side='left', fill='x', expand=True)
        self._flat_btn(bar, ' 状态刷新 ', BTN_ORANGE, self.on_status_refresh,
                       padx=10, pady=2).pack(side='right')

        self.root.protocol('WM_DELETE_WINDOW', self.on_close)

    def _feature_card(self, parent, title, desc, top=0):
        """一张圆角功能卡片：左侧标题+说明，右侧放开关/按钮，返回右侧容器"""
        card = Card(parent, top=top)
        left = tk.Frame(card.body, bg=CARD_BG)
        left.pack(side='left', fill='x', expand=True)
        tk.Label(left, text=title, font=('Microsoft YaHei UI', 10, 'bold'),
                 bg=CARD_BG, anchor='w').pack(anchor='w')
        tk.Label(left, text=desc, font=('Microsoft YaHei UI', 9), fg='#666',
                 bg=CARD_BG, anchor='w', wraplength=380,
                 justify='left').pack(anchor='w')
        right = tk.Frame(card.body, bg=CARD_BG)
        right.pack(side='right', padx=(10, 0))
        card._layout()              # 控件已就位，立刻按内容定高
        return right

    @staticmethod
    def _flat_btn(master, text, bg, command, bold=True, padx=14, pady=4):
        """圆角扁平彩色按钮（比默认 ttk 按钮更醒目）"""
        return FlatBtn(master, text, bg, command, bold=bold, padx=padx,
                       pady=pady)

    def _build_adv_tab(self, nb):
        f = ttk.Frame(nb, padding=10)
        nb.add(f, text=' 一键功能 ')

        # 第一行：附加 + 识别角色
        r1 = ttk.Frame(f)
        r1.pack(fill='x')
        ttk.Label(r1, text='进程：').pack(side='left')
        ttk.Label(r1, textvariable=self.proc_status, foreground='#06c').pack(side='left')
        self._flat_btn(r1, ' 连接并识别角色 ', BTN_BLUE,
                       self.on_adv_locate).pack(side='left', padx=10)
        tk.Label(r1, textvariable=self.adv_status, fg='#06c',
                 font=('Microsoft YaHei UI', 9)).pack(side='left', padx=4)

        # 无敌
        right = self._feature_card(
            f, '无敌', '怪打不到你，完全不会受伤；关掉开关立即恢复正常', top=14)
        self.var_god = tk.BooleanVar(value=False)
        PillSwitch(right, self.var_god, command=self.on_adv_god)

        # 秒杀
        right = self._feature_card(
            f, '秒杀', '近战和远程弓箭都是一击必杀；关掉开关恢复正常伤害')
        self.var_kill = tk.BooleanVar(value=False)
        PillSwitch(right, self.var_kill, command=self.on_adv_kill)

        # 移动速度
        right = self._feature_card(
            f, '移动速度', '跑速倍数，选「不变」立即恢复正常速度')
        self.var_speed = tk.StringVar(value='不变')
        Segmented(right, self.var_speed,
                  ['不变', '1.5倍', '2倍', '3倍', '5倍', '10倍'],
                  command=self.on_adv_speed, sel_bg=BTN_BLUE)

        # 一键满级
        right = self._feature_card(
            f, '一键满级', '升到%d级，血量攻击同步成长并自动存档\n'
            '（不用识别角色）' % tc.MAX_LEVEL)
        self._flat_btn(right, ' 一键满级 ', BTN_GREEN,
                       self.on_adv_maxlevel).pack()

        # 一键满金币
        right = self._feature_card(
            f, '一键满金币', '金币一次性拉满（走游戏自己的金币系统，\n'
                             '界面立刻刷新，不用识别角色）')
        self._flat_btn(right, ' 一键满金币 ', BTN_GREEN,
                       self.on_max_gold).pack()

        # 一键恢复
        r4 = ttk.Frame(f)
        r4.pack(fill='x', pady=(4, 0))
        self._flat_btn(r4, ' 全部恢复原样 ', BTN_RED,
                       self.on_adv_reset).pack(side='left')
        tk.Label(r4, text='一键还原无敌/秒杀/移速/满蓝/拾取/游戏速度（等级、金币属存档进度，不回退）',
                 fg='#777', font=('Microsoft YaHei UI', 9)).pack(
            side='left', padx=10)

        ttk.Label(f, foreground='#555', wraplength=700, justify='left', text=(
            '用法：先点「连接并识别角色」（通常几秒，自动找到主角和同伴），\n'
            '之后拨开关、选倍数即可，改完切回游戏马上生效。\n'
            '「一键满级」「一键满金币」不用识别角色；「最大蓝条」「全屏拾取」\n'
            '「游戏速度」都在旁边「强迫症修改」页，游戏开着直接用。\n'
            '界面卡在「处理中」点不动时（比如新建存档后）：点右下角「状态刷新」。'
        )).pack(fill='x', pady=(10, 0))

    def _build_ocd_tab(self, nb):
        f = ttk.Frame(nb, padding=10)
        nb.add(f, text=' 强迫症修改 ')

        # 最大蓝条
        right = self._feature_card(
            f, '最大蓝条', '蓝条永远保持满值，放技能也不掉蓝；\n关掉开关后恢复正常消耗',
            top=6)
        self.var_mana = tk.BooleanVar(value=False)
        PillSwitch(right, self.var_mana, command=self.on_mana_toggle)

        # 游戏速度（整体倍速，无需识别角色）
        right = self._feature_card(
            f, '游戏速度', '整个游戏一起加速（移动/动画/战斗都变快），\n'
                           '选「不变」立即恢复正常')
        self.var_gspeed = tk.StringVar(value='不变')
        Segmented(right, self.var_gspeed,
                  ['不变', '1.5倍', '2倍', '3倍', '5倍', '10倍'],
                  command=self.on_game_speed, sel_bg=BTN_BLUE)

        # 全屏拾取
        right = self._feature_card(
            f, '全屏拾取', '打怪掉的金币、经验自动进账，不用跑过去捡')
        self.var_pickup = tk.BooleanVar(value=False)
        PillSwitch(right, self.var_pickup, command=self.on_adv_pickup)

        # 一键恢复
        r = ttk.Frame(f)
        r.pack(fill='x', pady=(4, 0))
        self._flat_btn(r, ' 全部恢复原样 ', BTN_RED,
                       self.on_adv_reset).pack(side='left')
        tk.Label(r, text='一键还原满蓝/游戏速度/拾取/无敌/秒杀/移速（等级、金币属存档进度，不回退）',
                 fg='#777', font=('Microsoft YaHei UI', 9)).pack(
            side='left', padx=10)

        ttk.Label(f, foreground='#555', wraplength=700, justify='left', text=(
            '用法：这三项都不用识别角色，游戏开着直接用；改完切回游戏马上生效。'
        )).pack(fill='x', pady=(10, 0))

    def _build_help_tab(self, nb):
        f = ttk.Frame(nb, padding=10)
        nb.add(f, text=' 原理说明 ')
        card = Card(f, fill_mode='both', padx=8, pady=8)
        txt = tk.Text(card.body, wrap='word', font=('Microsoft YaHei UI', 10),
                      bg=CARD_BG, fg='#333', relief='flat', bd=0,
                      highlightthickness=0)
        txt.pack(fill='both', expand=True)
        card._layout()                  # 未显示的页签也要按内容定高
        txt.insert('1.0', (
            '【游戏文件分析结论】\n'
            '· 引擎：Unity + Mono（MonoBleedingEdge），游戏逻辑在 Assembly-CSharp.dll\n'
            '· 反编译可见：金币 GoldComponent.value、血量 HealthComponent.value、'
            '等级 LevelComponent.value 都是 int（Int32）\n\n'

            '【一、一键功能页（无敌/秒杀/移速/满级/金币）】\n'
            '· 游戏逻辑是 ECS 架构：血量 HealthComponent、移速、攻击等各是独立的小对象，'
            'Mono 托管对象头部固定是 vtable 指针（同类实例完全相同），字段从 +0x10 开始\n'
            '· 识别链路：HealthComponent vtable -> 全堆反查实体引用 -> 按 PlayerControlled '
            '标签确认主角/同伴 -> Stats 槽(+0x260) 拿到 血量/伤害/移速 三个属性源\n'
            '· 无敌 = 打开游戏原生"无法受伤"标记（伤害判定直接跳过）+ 满血锁定双保险\n'
            '· 秒杀 = 近战用攻击力、远程弓箭用魔法值，两个属性都拉到 100 万，'
            '近战远程都一击必杀；取消勾选即还原\n'
            '· 移速倍数 = 直接改属性源 Stat.baseValue，游戏自己把新值同步进组件\n'
            '· 一键满级 = 注入经验，让游戏自己的升级流程跑满（上限999级），\n'
            '  属性成长/血蓝刷新/存档/成就全走原生逻辑，不是光改等级数字\n'
            '· 一键满金币 = 调用游戏自己的"设置金币"入口（ReplaceGold），'
            '数值、金币界面刷新、自动存档全走原生逻辑，不是硬写内存\n\n'

            '【二、强迫症修改页（最大蓝条/游戏速度/全屏拾取）】\n'
            '· 最大蓝条 = 蓝量上限在属性 mana、当前值在 ManaComponent，HUD 蓝条'
            '靠游戏自己的 ReplaceMana 事件刷新 —— 后台每 0.2 秒把当前蓝量顶到'
            '上限并走刷新入口，\n'
            '  蓝条立刻满、放技能不掉蓝；关掉开关后蓝量停在满值、按上限正常消耗。'
            '换场景/复活组件被重建会自动重挂，识别不到角色时静默等待\n'
            '· 全屏拾取 = 在游戏进程里直接调用它自己的"捡起"代码，掉一个捡一个\n'
            '· 游戏速度 = 设置引擎的全局时间缩放（Unity timeScale），整个游戏'
            '（移动/动画/战斗）一起加速，后台每 0.25 秒自动保持一次；\n'
            '  游戏暂停或加载可能把速度改回去，所以倍速开着时会持续维持，'
            '选「不变」立即恢复正常\n\n'

            '【三、为什么移速要用"属性源"改】\n'
            '· 游戏每帧执行 UpdateMoveSpeedSystem：把属性 StatsComponent.moveSpeed（FloatStat）'
            '的值同步写进 MoveSpeedComponent —— 直接改组件会被下一帧覆盖\n'
            '· 属性源按钮走的定位链：HealthComponent vtable -> 全堆反查实体引用 -> '
            '按 PlayerControlledComponent 标签识别玩家实体 -> Stats 槽(+0x260) -> '
            'moveSpeed FloatStat(+0x40) -> baseValue(+0x18)\n'
            '· 改在源上后，覆盖动作由游戏自己完成，写入的就是正确值，彻底无竞态\n\n'

            '【免责】仅供个人单机学习研究，请勿用于联机作弊或商业传播。'
        ))
        txt.config(state='disabled')

    # ---------------- 异步执行辅助 ----------------
    def run_async(self, btn, fn, on_ok, busy_text='处理中...', on_err=None):
        """在后台线程执行 fn，结果经队列回到主线程。
        tkinter 不允许从子线程直接操作界面，所以统一走 队列 + after 轮询。
        on_err 可选：传入时错误交给它处理（不再弹窗），否则默认弹窗。"""
        if self._busy:
            return
        self._busy = True
        gen = self._run_gen                 # 记下任务代号，状态刷新后据此作废旧结果
        self._cur_btn = btn
        if btn is not None:
            btn.config(state='disabled')
        self.status.set(busy_text)

        def work():
            try:
                res = fn()
                self._q.put(('ok', btn, res, on_ok, None, gen))
            except Exception as e:                     # noqa: BLE001
                self._q.put(('err', btn, None, on_err, e, gen))

        threading.Thread(target=work, daemon=True).start()

    def _poll_queue(self):
        try:
            while True:
                try:
                    item = self._q.get_nowait()
                except queue.Empty:
                    break
                if item[0] == 'call':
                    # 后台线程请求在主线程执行一段代码（tkinter 只能在主线程操作）
                    _, fn, gen = item
                    if gen != self._run_gen:
                        continue            # 「状态刷新」之前的旧线程回调，作废
                    try:
                        fn()
                    except Exception as e:                 # noqa: BLE001
                        self.status.set('出错：' + str(e)[:100])
                    continue
                kind, btn, res, on_ok, err, gen = item
                if gen != self._run_gen:
                    # 已作废的旧任务：只把按钮还回来，不碰状态、不弹错误
                    if btn is not None:
                        try:
                            btn.config(state='normal')
                        except Exception:                 # noqa: BLE001
                            pass
                    continue
                self._done(btn, res, on_ok, err)
        except Exception:                                 # noqa: BLE001
            # 任何意外都绝不能中断轮询，否则后台结果永远回不来，
            # 界面会永久停在「处理中」——这正是新建存档后卡死的根源
            pass
        try:
            self.root.after(80, self._poll_queue)
        except Exception:                                 # noqa: BLE001 窗口已关闭
            pass

    def _post(self, fn):
        """线程安全地把 fn 投递到主线程执行（带任务代号，刷新后旧回调作废）"""
        self._q.put(('call', fn, self._run_gen))

    def _done(self, btn, res, on_ok, err):
        self._busy = False
        if btn is not None:
            try:
                btn.config(state='normal')
            except Exception:                             # noqa: BLE001
                pass
        if btn is self._cur_btn:
            self._cur_btn = None
        if err is not None:
            if on_ok is not None:
                # 调用方传了自定义错误处理（on_err），交给它，不弹窗
                try:
                    on_ok(err)
                except Exception as e:                     # noqa: BLE001
                    messagebox.showerror('出错', str(e))
                return
            msg = str(err)
            self.status.set('出错：' + msg[:100])
            messagebox.showerror('出错', msg)
            return
        try:
            on_ok(res)
        except Exception as e:                         # noqa: BLE001
            self.status.set('出错：' + str(e)[:100])
            messagebox.showerror('出错', str(e))

    # ---------------- 一键功能页 ----------------
    def _need_pm(self, hint='请先点「连接并识别角色」'):
        if not self.pm:
            messagebox.showinfo('提示', hint)
            return False
        return True

    def _auto_attach(self, action='点此按钮'):
        """未附加时自动附加游戏进程（只认带窗口的真进程，避免附加到
        退出残留的僵尸进程）。返回 True 表示已附加、可以继续操作。"""
        if self.pm:
            return True
        win = tc._windowed_pids()
        pids = [p for p in tc.find_game_pids() if p in win]
        if not pids:
            messagebox.showinfo('提示',
                                '未发现游戏进程「%s」。\n请先启动游戏并进入游戏画面，再%s。'
                                % (tc.GAME_PROCESS_NAME, action))
            return False
        try:
            self.pm = tc.ProcessMemory(pids[0])
        except tc.TrainerError as e:
            messagebox.showerror('附加失败', str(e))
            return False
        self.attached_pid = pids[0]
        self.freezer.start(self.pm)
        self.proc_status.set('已附加 PID %d' % pids[0])
        return True

    def _with_adv_players(self, fn):
        """确保已识别玩家属性后，在主线程执行 fn(players)。
        首次使用自动发起识别（通常几秒），识别完成后接着执行刚才选的操作。"""
        if not self._need_pm('请先点「连接并识别角色」'):
            return
        if self.adv_players:
            fn(self.adv_players)
            return
        if self._busy:
            self.status.set('正在处理，请稍候再操作……')
            return

        def job():
            return tc.locate_player_speed_sources(
                self.pm,
                on_progress=lambda m: self.root.after(0, self.status.set, m))

        def ok(players):
            self.adv_players = players
            if self.adv_orig is None:
                self.adv_orig = players        # 保留首次定位的原始值，用于恢复
            self.adv_status.set('已识别：%s' % '、'.join(p['name'] for p in players))
            self._start_flag_keeper()
            fn(players)
            self._reconcile_flags(players)

        self.run_async(None, job, ok, '正在识别角色属性（通常几秒，请稍候）...')

    def _reconcile_flags(self, players):
        """识别完成后对账：把"界面还开着、但之前没写进内存"的开关统一补生效。
        （没连上游戏时就拨过开关、或上次识别失败留下的意图，在这里兑现）"""
        try:
            if self.adv_kill:
                self._apply_kill(True, players)
            if self.adv_god:
                self._apply_god(True, players)
            if self.adv_speed != 1.0:
                self._apply_speed(self.adv_speed, players)
        except Exception as e:                              # noqa: BLE001
            self.status.set('部分开关补生效失败：%s' % str(e)[:60])

    # ---- 无敌自愈：组件被游戏 Replace/销毁后自动跟上 ----
    def _start_flag_keeper(self):
        """启动（或重启）无敌自愈维持线程"""
        if not (self.pm and self.adv_players):
            return
        if self.flag_keeper:
            self.flag_keeper.stop()
        self.flag_keeper = tc.FlagKeeper(
            self.pm, self.adv_players, self.freezer,
            god_enabled=lambda: self.adv_god,
            on_refresh=self._keeper_refresh,
            on_rebuild=self._keeper_rebuild,
            on_error=lambda e: None)    # 瞬时读失败静默忽略（下个周期自愈）
        self.flag_keeper.start()

    def _keeper_refresh(self, players):
        """组件对象被 Replace 换新（翻滚/冲刺等动画会重建免伤组件）：
        后台线程已就地更新了地址，这里在主线程把一次性写入的值补到新地址。"""
        if not self.pm:
            return
        try:
            for p in players:
                if self.adv_kill and p['atk']:
                    self.pm.write(p['atk'], struct.pack('<i', KILL_DAMAGE))
                if self.adv_kill and p['magic']:
                    self.pm.write(p['magic'], struct.pack('<i', KILL_DAMAGE))
                if self.adv_speed != 1.0 and p['addr']:
                    self.pm.write(p['addr'], struct.pack('<f', p['value'] * self.adv_speed))
                    self.freezer.set(p['addr'], 'Float', p['value'] * self.adv_speed)
        except tc.TrainerError:
            pass

    def _keeper_rebuild(self):
        """实体整体销毁重建（换场景/复活）：全量重新识别并重开功能。
        用节流防重复触发；重定位通常几秒，期间界面给出提示。
        本回调在自愈线程里被调用，tkinter 操作必须投递回主线程。"""
        self._post(self._do_rebuild)

    def _do_rebuild(self):
        now = time.time()
        if self._relocating or now - self._last_rebuild < 10:
            return
        if self._busy or not self.pm:
            # 正在跑别的任务/已断开：清掉标记，下个周期再触发
            self._last_rebuild = now
            return
        self._relocating = True
        self._last_rebuild = now
        # 停掉正在报错的旧自愈线程（重新识别完会重建）
        if self.flag_keeper:
            self.flag_keeper.stop()
            self.flag_keeper = None
        god, kill, spd = self.adv_god, self.adv_kill, self.adv_speed

        def job():
            return tc.locate_player_speed_sources(self.pm)

        def ok(players):
            self._relocating = False
            if not players:
                return
            self.adv_players = players
            self.adv_orig = players
            self._start_flag_keeper()
            # 按开关状态重开功能
            if god:
                self.var_god.set(True)
                self._apply_god(True, players)
            if kill:
                self.var_kill.set(True)
                self._apply_kill(True, players)
            if spd != 1.0:
                self._apply_speed(spd, players)
            self.status.set('检测到场景切换，已自动重新连接并恢复功能')

        def err(e):
            self._relocating = False
            self.adv_players = None
            self.adv_status.set('场景切换后需重新识别（点「连接并识别角色」）')
            self.status.set('自动重连失败：%s' % str(e)[:60])

        self.run_async(None, job, ok, '检测到场景切换，正在自动重新识别……', on_err=err)

    def on_adv_locate(self):
        """手动重新识别（换了存档/重进世界后角色对象会变，需重新识别）。
        未附加时自动附加游戏，用户只需点这一个按钮。"""
        if not self._auto_attach('点此按钮'):
            return
        if self.adv_players:
            # 重新识别前先恢复原样，避免把"已修改的值"当成原始值
            self._adv_restore()
            self.adv_players = None
            self.adv_orig = None
        self._with_adv_players(
            lambda _ps: self.status.set('识别完成，可以使用下面的选项了'))

    # ---- 状态刷新：界面卡在「处理中」时的急救入口 ----
    def on_status_refresh(self):
        """强制中断卡住的任务、恢复按钮响应，然后重新附加游戏并识别角色。
        新建存档/换场景后如果长时间停在「处理中」点不动，点这个一键复位。"""
        # 1) 作废在跑/排队的旧任务，立刻恢复界面响应
        self._run_gen += 1
        self._busy = False
        btn = self._cur_btn
        self._cur_btn = None
        if btn is not None:
            try:
                btn.config(state='normal')
            except Exception:                          # noqa: BLE001
                pass
        # 2) 停掉旧的自愈线程、清空旧识别结果
        try:
            if self.flag_keeper:
                self.flag_keeper.stop()
        except Exception:                              # noqa: BLE001
            pass
        self.flag_keeper = None
        self.adv_players = None
        self.adv_orig = None
        self.adv_status.set('刷新中……')
        self.status.set('正在刷新状态……')

        # 3) 检查游戏进程与附加是否仍然有效
        win = tc._windowed_pids()
        pids = [p for p in tc.find_game_pids() if p in win]
        if not pids:
            self._detach_all()
            self.proc_status.set('未附加')
            self.adv_status.set('未识别（未发现游戏进程）')
            self.status.set('状态已刷新：未发现游戏进程，启动游戏后再点「状态刷新」')
            return
        if self.pm and self.attached_pid == pids[0] and getattr(self.pm, 'handle', None):
            self.proc_status.set('已附加 PID %d' % pids[0])
        elif not self._reattach(pids[0]):
            return                      # 附加失败，提示已在内部处理

        # 4) 重新识别角色（异步；旧任务结果已被代号机制作废，不会串台）
        def job():
            return tc.locate_player_speed_sources(
                self.pm,
                on_progress=lambda m: self.root.after(0, self.status.set, m))

        def ok(players):
            if not players:
                self.adv_status.set('未识别（进入游戏画面后再刷新一次）')
                self.status.set('状态刷新完成，但没找到角色，请进入可操作角色的画面后再点一次')
                return
            self.adv_players = players
            self.adv_orig = players
            self.adv_status.set('已识别：%s' % '、'.join(p['name'] for p in players))
            self._start_flag_keeper()
            self._reconcile_flags(players)
            self.status.set('状态刷新完成：已重新连接并识别角色')

        def err(e):
            self.adv_status.set('未识别（点「连接并识别角色」）')
            self.status.set('刷新后重新识别失败：%s' % str(e)[:70])

        self.run_async(None, job, ok,
                       '状态刷新：正在重新识别角色（通常几秒）……', on_err=err)

    def _reattach(self, pid):
        """重新附加游戏：停掉引用旧连接的线程，附加成功后按界面开关状态拉起。"""
        self._stop_all_keepers()
        try:
            self.freezer.clear()       # 旧存档的地址作废，防止往废地址空写
            self.freezer.stop()
        except Exception:                              # noqa: BLE001
            pass
        if self.pm:
            try:
                self.pm.close()
            except Exception:                          # noqa: BLE001
                pass
            self.pm = None
        try:
            self.pm = tc.ProcessMemory(pid)
        except tc.TrainerError as e:
            self.attached_pid = None
            self.proc_status.set('附加失败')
            self.status.set('状态刷新失败：附加游戏进程出错 %s' % str(e)[:60])
            messagebox.showerror('附加失败', str(e))
            return False
        self.attached_pid = pid
        self.freezer.start(self.pm)
        self.proc_status.set('已附加 PID %d' % pid)
        # 不依赖角色识别的三个功能：按开关状态重新拉起
        if self.var_mana.get():
            keeper = tc.ManaKeeper(
                self.pm, on_error=lambda e: self._post(lambda: self._on_mana_err(e)))
            self.mana_keeper = keeper
            keeper.start()
        if self.var_pickup.get():
            self._pickup_total = 0
            keeper = tc.PickupKeeper(
                self.pm, on_picked=self._on_picked,
                on_error=lambda e: self._post(lambda: self._on_pickup_err(e)))
            self.pickup_keeper = keeper
            keeper.start()
        if self._gspeed != '不变':
            keeper = tc.GameSpeedKeeper(
                self.pm, self._adv_mult(self._gspeed),
                on_error=lambda e: self._post(lambda: self._on_speed_err(e)))
            self.speed_keeper = keeper
            keeper.start()
        return True

    def _stop_all_keepers(self):
        for attr in ('flag_keeper', 'mana_keeper', 'pickup_keeper', 'speed_keeper'):
            keeper = getattr(self, attr)
            if keeper:
                try:
                    keeper.stop()
                except Exception:                      # noqa: BLE001
                    pass
                setattr(self, attr, None)

    def _detach_all(self):
        """游戏进程已不存在：停掉所有后台线程、断开连接、界面全部复位。"""
        self._stop_all_keepers()
        try:
            self.freezer.clear()
            self.freezer.stop()
        except Exception:                              # noqa: BLE001
            pass
        if self.pm:
            try:
                self.pm.close()
            except Exception:                          # noqa: BLE001
                pass
            self.pm = None
        self.attached_pid = None
        self.adv_god = False
        self.adv_kill = False
        self.adv_speed = 1.0
        self._gspeed = '不变'
        self._pickup_total = 0
        self.var_god.set(False)
        self.var_kill.set(False)
        self.var_speed.set('不变')
        self.var_mana.set(False)
        self.var_pickup.set(False)
        self.var_gspeed.set('不变')

    def _adv_restore(self):
        """把内存里的修改全部还原，并把界面上的选项复位（不弹窗）"""
        if self.flag_keeper:
            self.flag_keeper.stop()
            self.flag_keeper = None
        self.adv_god = False
        self.adv_kill = False
        self.adv_speed = 1.0
        orig = self.adv_orig or []
        if self.pm:
            for p in orig:
                self.freezer.remove(p['hp'])
                self.freezer.remove(p['addr'])
                try:
                    self.pm.write(p['addr'], struct.pack('<f', p['value']))
                    if p['atk']:
                        self.pm.write(p['atk'], struct.pack('<i', p['atk_val']))
                    if p['magic']:
                        self.pm.write(p['magic'], struct.pack('<i', p['magic_val']))
                    if p['uf']:
                        cur = struct.unpack('<i', self.pm.read(p['uf'], 4))[0]
                        self.pm.write(p['uf'], struct.pack('<i',
                                      cur & ~tc.FLAG_CANNOT_TAKE_DAMAGE))
                except tc.TrainerError:
                    pass
        if self.mana_keeper:
            self.mana_keeper.stop()                  # 满蓝保持线程
            self.mana_keeper = None
        if self.pickup_keeper:
            self.pickup_keeper.stop()
            self.pickup_keeper = None
        if self.speed_keeper:
            # 先停保持线程，再恢复原速，避免和线程并发远程调用
            self.speed_keeper.stop()
            self.speed_keeper = None
            if self.pm:
                try:
                    tc.set_game_speed(self.pm, 1.0)
                except Exception:                          # noqa: BLE001
                    pass
        self._gspeed = '不变'
        self.var_god.set(False)
        self.var_speed.set('不变')
        self.var_kill.set(False)
        self.var_pickup.set(False)
        self.var_gspeed.set('不变')
        self.var_mana.set(False)

    def on_adv_reset(self):
        if self._busy:
            self.status.set('正在处理，请稍候再操作……')
            return
        if not self.adv_orig:
            self._adv_restore()
            self.status.set('当前没有需要恢复的修改')
            return
        self._adv_restore()
        self.status.set('已全部恢复原样')

    # ---- 三个开关的实际生效逻辑（复用：手动切换 + 换场景自动恢复）----
    def _apply_god(self, want, players):
        """want=True 开启；False 关闭。后台线程会自动维持（自愈）。"""
        for p in players:
            if not p['hp']:
                continue
            if want:
                # 保险1：写游戏原生"无法受伤"标记（伤害判定直接跳过，真·免伤）
                if p['uf']:
                    # 读当前标记再 OR，避免用定位时的旧快照冲掉游戏后来设置的其他标记
                    cur = struct.unpack('<i', self.pm.read(p['uf'], 4))[0]
                    self.pm.write(p['uf'], struct.pack('<i',
                                  cur | tc.FLAG_CANNOT_TAKE_DAMAGE))
                # 保险2：锁血兜底（个别非伤害掉血路径也覆盖）
                target = p['hp_max'] if 0 < p['hp_max'] <= 10_000_000 else p['hp_cur']
                self.freezer.set(p['hp'], 'Int32', target)
            else:
                if p['uf']:
                    # 只摘掉免伤位，不动其他标记（死亡/剧情标记等保持游戏自己管）
                    cur = struct.unpack('<i', self.pm.read(p['uf'], 4))[0]
                    self.pm.write(p['uf'], struct.pack('<i',
                                  cur & ~tc.FLAG_CANNOT_TAKE_DAMAGE))
                self.freezer.remove(p['hp'])
        if want:
            self.freezer.start(self.pm)
        self.status.set('无敌已开启：完全不会受伤（切回游戏即生效）'
                        if want else '无敌已关闭，恢复正常受伤')

    def on_adv_god(self):
        if self._busy:
            self.var_god.set(not self.var_god.get())
            self.status.set('正在处理，请稍候再操作……')
            return
        want = self.var_god.get()
        self.adv_god = want
        self._with_adv_players(lambda ps: self._apply_god(want, ps))

    @staticmethod
    def _adv_mult(text):
        return 1.0 if text == '不变' else float(text.replace('倍', ''))

    def _apply_speed(self, mult, players):
        lines = []
        for p in players:
            addr, orig = p['addr'], p['value']
            if mult == 1.0:
                self.freezer.remove(addr)
                self.pm.write(addr, struct.pack('<f', orig))
                lines.append('%s恢复正常' % p['name'])
            else:
                v = orig * mult
                self.pm.write(addr, struct.pack('<f', v))
                self.freezer.set(addr, 'Float', v)
                lines.append('%s %g→%g' % (p['name'], orig, v))
        if mult != 1.0:
            self.freezer.start(self.pm)
        self.status.set('移动速度已设置：%s（切回游戏即生效）' % '，'.join(lines))

    def on_adv_speed(self):
        if self._busy:
            self.status.set('正在处理，请稍候再操作……')
            return
        mult = self._adv_mult(self.var_speed.get())
        self.adv_speed = mult
        self._with_adv_players(lambda ps: self._apply_speed(mult, ps))

    # ---- 游戏整体倍速（Time.timeScale）：不需要识别角色，附加即可用 ----
    def on_game_speed(self):
        if self._busy:
            self.var_gspeed.set(self._gspeed)   # 忙时把下拉框拨回原档
            self.status.set('正在处理，请稍候再操作……')
            return
        sel = self.var_gspeed.get()
        mult = self._adv_mult(sel)
        if mult == 1.0:
            if self.speed_keeper:
                self.speed_keeper.stop()        # 只停线程，恢复原速单独做
                self.speed_keeper = None
            self._gspeed = '不变'
            pm = self.pm
            if pm:
                def job():
                    tc.set_game_speed(pm, 1.0)
                self.run_async(
                    None, job,
                    lambda _r: self.status.set('游戏速度已恢复正常'),
                    '正在恢复正常速度...',
                    on_err=lambda e: self.status.set(
                        '恢复游戏速度失败：%s' % str(e)[:60]))
            else:
                self.status.set('游戏速度已选「不变」')
            return
        if not self._auto_attach('设置倍速'):
            self.var_gspeed.set(self._gspeed)
            return
        if self.speed_keeper:
            self.speed_keeper.stop()            # 换档：先停旧保持线程
            self.speed_keeper = None
        keeper = tc.GameSpeedKeeper(
            self.pm, mult,
            on_error=lambda e: self._post(lambda: self._on_speed_err(e)))
        self.speed_keeper = keeper
        keeper.start()
        self._gspeed = sel
        self.status.set('游戏速度已设为 %s：切回游戏即生效（后台自动保持）' % sel)

    def _on_speed_err(self, e):
        """倍速保持线程致命错误：自动关闭并提示"""
        if self.speed_keeper:
            self.speed_keeper.stop()
            self.speed_keeper = None
        self._gspeed = '不变'
        self.var_gspeed.set('不变')
        msg = str(e)
        self.status.set('游戏速度已自动关闭：%s' % msg[:80])
        messagebox.showwarning('游戏速度', '游戏速度已自动关闭：\n%s' % msg)

    # ---- 最大蓝条：把蓝量持续顶在上限（不依赖角色识别，附加即可用） ----
    def on_mana_toggle(self):
        if self._busy:
            self.var_mana.set(not self.var_mana.get())   # 忙时把开关拨回去
            self.status.set('正在处理，请稍候再操作……')
            return
        if not self.var_mana.get():
            if self.mana_keeper:
                self.mana_keeper.stop()              # 蓝量停在满值，无需回写
                self.mana_keeper = None
            self.status.set('最大蓝条已关闭，恢复正常消耗')
            return
        if not self._auto_attach('开启最大蓝条'):
            self.var_mana.set(False)
            return
        if self.mana_keeper:
            return
        keeper = tc.ManaKeeper(
            self.pm,
            on_error=lambda e: self._post(lambda: self._on_mana_err(e)))
        self.mana_keeper = keeper
        keeper.start()
        self.status.set('最大蓝条已开启：蓝条永远是满的（切回游戏即生效）')

    def _on_mana_err(self, e):
        """满蓝保持线程致命错误：自动关闭并提示"""
        if self.mana_keeper:
            self.mana_keeper.stop()
            self.mana_keeper = None
        self.var_mana.set(False)
        msg = str(e)
        self.status.set('最大蓝条已自动关闭：%s' % msg[:80])
        messagebox.showwarning('最大蓝条', '最大蓝条已自动关闭：\n%s' % msg)

    def _apply_kill(self, want, players):
        lines = []
        for p in players:
            if not p['atk']:
                continue
            orig = p['atk_val']
            v = KILL_DAMAGE if want else orig
            self.pm.write(p['atk'], struct.pack('<i', v))
            # 远程武器/弓箭的伤害走魔法属性，必须一起改才有远程一击必杀
            if p['magic']:
                morig = p['magic_val']
                self.pm.write(p['magic'], struct.pack('<i',
                              KILL_DAMAGE if want else morig))
            lines.append('%s %d→%d' % (p['name'], orig, v))
        self.status.set('秒杀已开启：近战远程都是一击必杀（切回游戏即生效）'
                        if want else '秒杀已关闭，恢复正常伤害')

    def on_adv_kill(self):
        if self._busy:
            self.var_kill.set(not self.var_kill.get())
            self.status.set('正在处理，请稍候再操作……')
            return
        want = self.var_kill.get()
        self.adv_kill = want
        self._with_adv_players(lambda ps: self._apply_kill(want, ps))

    # ---- 全屏拾取：后台巡逻，掉一个捡一个（不依赖角色识别，附加即可用） ----
    def on_adv_pickup(self):
        if self._busy:
            self.var_pickup.set(not self.var_pickup.get())
            self.status.set('正在处理，请稍候再操作……')
            return
        if not self.var_pickup.get():
            if self.pickup_keeper:
                self.pickup_keeper.stop()
                self.pickup_keeper = None
            self.status.set('全屏拾取已关闭'
                            + ('（本次共自动拾取数值 %d）' % self._pickup_total
                               if self._pickup_total else ''))
            return
        if not self.pm:
            # 未附加时自动附加（拾取只需进程权限，不用识别角色）
            pids = tc.find_game_pids()
            if not pids:
                self.var_pickup.set(False)
                messagebox.showinfo('提示',
                                    '未发现游戏进程「%s」。\n请先启动游戏并进入游戏画面，再开启。'
                                    % tc.GAME_PROCESS_NAME)
                return
            try:
                self.pm = tc.ProcessMemory(pids[0])
            except tc.TrainerError as e:
                self.var_pickup.set(False)
                messagebox.showerror('附加失败', str(e))
                return
            self.attached_pid = pids[0]
            self.freezer.start(self.pm)
            self.proc_status.set('已附加 PID %d' % pids[0])
        if self.pickup_keeper:
            return
        self._pickup_total = 0
        keeper = tc.PickupKeeper(
            self.pm,
            on_picked=self._on_picked,
            on_error=lambda e: self._post(lambda: self._on_pickup_err(e)))
        self.pickup_keeper = keeper
        keeper.start()
        self.status.set('全屏拾取已开启：场上的金币经验会自动进账（切回游戏生效）')

    def _on_picked(self, got, total):
        """拾取线程回调（后台线程），投递回主线程更新状态栏"""
        self._pickup_total += total
        self._post(lambda: self.status.set(
            '全屏拾取中：又捡到 %d 个（本次累计数值 %d）' % (got, self._pickup_total)))

    def _on_pickup_err(self, e):
        """拾取线程致命错误：自动关闭开关并提示"""
        if self.pickup_keeper:
            self.pickup_keeper.stop()
            self.pickup_keeper = None
        self.var_pickup.set(False)
        msg = str(e)
        # 业务错误直接展示；意外异常截断显示
        self.status.set('全屏拾取已自动关闭：%s' % msg[:80])
        messagebox.showwarning('全屏拾取', '全屏拾取已自动关闭：\n%s' % msg)

    # ---- 一键满级：注入经验让游戏自己的升级流程跑满（一次性动作，无需识别角色） ----
    def on_adv_maxlevel(self):
        if self._busy:
            self.status.set('正在处理，请稍候再操作……')
            return
        if not self._auto_attach('点此按钮'):
            return
        pm = self.pm

        def prog(level):
            self._post(lambda lv=level: self.status.set(
                '一键满级进行中：当前 %d 级……' % lv))

        def job():
            return tc.max_level_up(pm, on_progress=prog)

        def ok(level):
            self.status.set('一键满级完成：已升到 %d 级（属性已同步成长）' % level)
            messagebox.showinfo(
                '一键满级',
                '已升到 %d 级满级！\n血量、攻击等属性已按满级成长，进度已自动存档。\n'
                '切回游戏即可看到效果。' % level)

        def on_err(e):
            # 失败原因必须弹窗，藏在底部状态栏太容易漏看
            self.status.set('一键满级失败：%s' % str(e)[:80])
            messagebox.showerror('一键满级没成功', str(e))

        self.run_async(None, job, ok, '正在一键满级（稍等几秒，状态栏会显示进度）...',
                       on_err=on_err)

    # ---- 一键满金币：调用游戏自己的金币系统（一次性动作，无需识别角色） ----
    def on_max_gold(self):
        if self._busy:
            self.status.set('正在处理，请稍候再操作……')
            return
        if not self._auto_attach('点此按钮'):
            return
        pm = self.pm

        def job():
            return tc.set_max_gold(pm)

        def ok(res):
            gold, changed = res
            if changed:
                self.status.set('一键满金币完成：当前金币 %s' % format(gold, ','))
            else:
                self.status.set('当前金币 %s 已经不低于满额，未做改动'
                                % format(gold, ','))

        def on_err(e):
            # 失败原因必须弹窗，藏在底部状态栏太容易漏看
            self.status.set('一键满金币失败：%s' % str(e)[:80])
            messagebox.showerror('一键满金币没成功', str(e))

        self.run_async(None, job, ok, '正在设置满金币...', on_err=on_err)

    def on_close(self):
        try:
            if self.flag_keeper:
                self.flag_keeper.stop()
        except Exception:                              # noqa: BLE001
            pass
        try:
            if self.mana_keeper:
                self.mana_keeper.stop()
        except Exception:                              # noqa: BLE001
            pass
        try:
            if self.pickup_keeper:
                self.pickup_keeper.stop()
        except Exception:                              # noqa: BLE001
            pass
        try:
            if self.speed_keeper:
                self.speed_keeper.stop()
                if self.pm and self._gspeed != '不变':
                    tc.set_game_speed(self.pm, 1.0)    # 退出前把游戏速度还原
        except Exception:                              # noqa: BLE001
            pass
        try:
            self.freezer.clear()
            self.freezer.stop()
        except Exception:                              # noqa: BLE001
            pass
        if self.pm:
            try:
                self.pm.close()
            except Exception:                          # noqa: BLE001
                pass
        self.root.destroy()


def main():
    root = tk.Tk()
    try:
        ttk.Style().theme_use('vista')
    except Exception:                                  # noqa: BLE001
        pass
    app = TrainerApp(root)
    # 启动时顺手探测游戏进程
    if tc.is_game_running():
        app.status.set('检测到游戏正在运行，可点「连接并识别角色」')
    root.mainloop()


if __name__ == '__main__':
    main()
