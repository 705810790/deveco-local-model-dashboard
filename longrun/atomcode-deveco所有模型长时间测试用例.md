# 轻量级串口调试助手（SSCOM 克隆版）开发文档

> 本文档收录了从「技术可行性分析」到「一次性交付任务书」的全部内容，可作为项目立项、需求归档与 AI 协作开发的统一依据。
> 当前版本：V1.0 ｜ 整理日期：2026-09-11
> 每完成一个阶段更新一次文档

---

## 开发进度记录

| 阶段 | 状态 | 完成日期 | 产出物 | 备注 |
| --- | --- | --- | --- | --- |
| 文档整理与任务书编写 | ✅ 已完成 | 2026-09-11 | 本文档 | 含技术选型、功能拆解、验收清单 |
| 代码实现（serial_tool.py） | ✅ 已完成 | 2026-09-13 | `serial_tool.py` | 单文件交付，py_compile 语法检查通过 |
| 验收自测 | ⏳ 待执行 | — | — | 需 `pip install pyserial` 后运行 `python serial_tool.py` 逐条验证 |
| PyInstaller 打包 | ⏳ 待执行 | — | — | `pyinstaller -F -w serial_tool.py` |

### 实现说明与自行决策清单

1. **架构**：`SerialManager` 类封装串口全部操作，不持有任何 UI 引用；`App(tk.Tk)` 继承主窗口，通过 `queue.Queue` 解耦线程通信，主线程 `after(50ms)` 轮询队列刷新 UI。
2. **线程安全**：接收线程仅向队列投递 `bytes` 或事件元组，严禁直接操作控件；线程退出用 `threading.Event` + `join(timeout=1)` 确保无残留。
3. **掉线保护**：接收线程捕获 `SerialException/OSError` 后投递 `("DISCONNECTED",)`，主线程弹警告并执行与手动关闭完全相同的复原流程。
4. **分包算法**：在接收线程内维护 `bytearray` 缓冲区与 `time.monotonic()` 时间戳，空闲超分包超时则整包入队，缓冲达 4096 字节强制入队；超时值通过 property 实时生效。
5. **行数上限**：接收区超 5000 行时一次性删除最旧 1000 行，防止大数据量卡顿。
6. **配置持久化**：退出时（`WM_DELETE_WINDOW`）自动写 `config.json`，启动时恢复；JSON 读写 try/except 容错，损坏时忽略用默认值。路径用 `sys.frozen` 兼容 PyInstaller 打包。
7. **防重入**：打开/关闭按钮加 `_busy` 忙标志，防止重复快速点击导致状态错乱。
8. **UI 状态矩阵**：串口打开期间禁用端口/刷新/波特率/数据位/停止位/校验位/流控，关闭后恢复。
9. **状态栏临时消息**：错误/提示临时显示 3 秒后自动恢复为连接状态文本。
10. **打开按钮配色**：成功后文字变"关闭串口"并尝试浅红背景（ttk 样式不存在时回退 `config(bg=...)`）。
11. **发送区快捷键**：`Ctrl+Return` 触发发送、`Ctrl+L` 清除窗口，在发送区和接收区均绑定。
12. **HEX 工具函数**：`hex_str_to_bytes` / `bytes_to_hex_str` 全程序复用，无重复实现。
13. **文件发送**：`after` 驱动分块发送，每块 1024 字节、间隔 50ms，状态栏显示进度，`[停止]` 可中断。
14. **接收落盘**：勾选后持有文件句柄，每包原始 `bytes` 追加写入并 `flush()`，取消/退出时关闭句柄。
15. **编码回退**：文本模式解码失败时加 `[HEX回退]` 前缀显示十六进制，不丢数据。
16. **定时发送**：间隔非法（非整数或 <10ms）弹提示并自动取消；未打开串口时勾选则提示并不勾选。
17. **发送内容为空**：决策为提示"发送内容为空"且不发送，不弹错误对话框（用状态栏临时提示）。
18. **窗口置顶**：勾选 `-topmost True`，取消恢复，配置持久化含此项。

---

## 目录

- [1. 项目背景与可行性结论](#1-项目背景与可行性结论)
- [2. 技术栈选型](#2-技术栈选型)
- [3. 功能模块拆解与实现思路](#3-功能模块拆解与实现思路)
- [4. 极简方案对比（轻量优先）](#4-极简方案对比轻量优先)
- [5. Tkinter 最小可行示例代码](#5-tkinter-最小可行示例代码)
- [6. 打包体积说明](#6-打包体积说明)
- [7. 一次性交付任务书（可直接投喂 AI）](#7-一次性交付任务书可直接投喂-ai)
- [8. 使用建议与后续规划](#8-使用建议与后续规划)

---

## 1. 项目背景与可行性结论

目标：用 Python 复刻一款类似 **SSCOM** 的串口/网络数据调试工具，核心功能包括串口通信、HEX/ASCII 显示、定时发送、文件发送等。

**结论：完全可行。**

- SSCOM 的核心能力（串口读写、数据显示、定时任务、文件 IO）在 Python 生态中均有成熟且轻量的库支持。
- 在"不下载大型组件和资源"的硬约束下，采用 **Python 标准库 `tkinter` + 第三方库 `pyserial`** 即可完整实现，打包后体积可控制在 **5–12 MB**。

---

## 2. 技术栈选型

| 能力 | 推荐方案 | 说明 |
| --- | --- | --- |
| GUI 界面 | **tkinter / ttk**（标准库） | 零额外下载，体积极小；备选 PyQt（功能强但体积大，本项目不采用） |
| 串口通信 | **pyserial** | Python 串口事实标准，稳定轻量 |
| 网络通信（可选） | **socket**（标准库） | 实现 TCP/UDP，二期可扩展 |
| 多线程 | **threading** + `queue.Queue` | 接收线程与 UI 解耦，避免界面卡死 |
| 配置持久化 | **json**（标准库） | 保存/恢复控件状态 |
| 打包 | **PyInstaller** | `pyinstaller -F -w serial_tool.py` |

> 硬性原则：**除 `pyserial` 外不引入任何第三方依赖**；不引用任何外部图片/字体/声音资源。

---

## 3. 功能模块拆解与实现思路

### 3.1 界面布局（UI）

- **顶部菜单栏**：通讯端口、串口设置等。
- **中间显示区**：只读文本框，显示接收数据，支持自动滚动到底部。
- **底部控制区**：
  - 端口选择下拉框 + 刷新按钮（扫描可用 COM 口）。
  - 波特率等参数下拉框。
  - 打开/关闭串口按钮（点击切换状态）。
  - 选项复选框：HEX 显示、HEX 发送、加时间戳、定时发送。
  - 发送输入框 + 发送按钮。
- **状态栏**：显示收发字节统计（S/R）与当前连接状态。

### 3.2 串口逻辑（Core Logic）

1. **扫描端口**：`serial.tools.list_ports.comports()` 填充下拉框。
2. **连接**：实例化 `serial.Serial`，传入选定参数。
3. **接收数据（关键）**：子线程循环读取 → 通过队列/信号投递给主线程 → 主线程更新 UI。**严禁子线程直接操作 UI 控件。**
4. **发送数据**：取输入内容，若勾选 HEX 发送则转为 `bytes` 后写出。

### 3.3 数据处理（HEX / ASCII）

- **HEX 显示**：`bytes` → 两位大写、空格分隔的十六进制字符串。
- **HEX 发送**：用户输入去空格后用 `bytes.fromhex()` 转换。
- **文本模式**：按所选编码 `decode`，失败回退 HEX 显示。

### 3.4 高级功能

- **定时发送**：`after` 循环按间隔重复发送。
- **时间戳与分包**：基于 `time.monotonic()` 的空闲超时判定分包边界。
- **保存数据 / 接收落盘 / 发送文件**：标准文件 IO，分块发送防溢出。

---

## 4. 极简方案对比（轻量优先）

| 方案 | 额外依赖 | 打包体积 | 界面观感 | 推荐度 |
| --- | --- | --- | --- | --- |
| **Tkinter（标准库）** | 仅 `pyserial` | 5–12 MB | 复古朴素 | ⭐⭐⭐⭐⭐ 首选 |
| CustomTkinter | `customtkinter` + `pyserial` | 10–15 MB | 现代扁平 | ⭐⭐⭐ 备选 |
| Web 技术 + Flask/FastAPI | `flask` + `pyserial`(+`pywebview`) | 10–20 MB | 可高度美化 | ⭐⭐ 复杂度高 |
| PyQt5/6 | `PyQt5` + `pyserial` | 40–80 MB | 原生精致 | ⭐ 违背轻量目标 |

**最终决策：Tkinter + pyserial。** 满足"不过多下载大组件和资源"的核心诉求，性价比最高。

---

## 5. Tkinter 最小可行示例代码

> 以下为早期验证用的精简骨架，正式交付以第 7 节任务书产出的完整代码为准。

```python
import tkinter as tk
from tkinter import ttk, scrolledtext
import serial
import serial.tools.list_ports
import threading
import time

class MiniSSCOM:
    def __init__(self, root):
        self.root = root
        self.root.title("Mini SSCOM (轻量版)")
        self.root.geometry("600x400")

        self.ser = None
        self.running = False

        # 1. 顶部控制栏
        ctrl_frame = ttk.Frame(root)
        ctrl_frame.pack(fill=tk.X, padx=5, pady=5)

        ttk.Label(ctrl_frame, text="端口:").pack(side=tk.LEFT)
        self.port_var = tk.StringVar()
        self.port_combo = ttk.Combobox(ctrl_frame, textvariable=self.port_var, width=10)
        self.port_combo.pack(side=tk.LEFT, padx=2)

        ttk.Button(ctrl_frame, text="刷新", command=self.refresh_ports).pack(side=tk.LEFT, padx=2)

        ttk.Label(ctrl_frame, text="波特率:").pack(side=tk.LEFT, padx=(10, 0))
        self.baud_var = tk.StringVar(value="115200")
        ttk.Combobox(ctrl_frame, textvariable=self.baud_var, values=["9600", "115200"], width=8).pack(side=tk.LEFT, padx=2)

        self.open_btn = ttk.Button(ctrl_frame, text="打开串口", command=self.toggle_serial)
        self.open_btn.pack(side=tk.LEFT, padx=10)

        # 2. 中间显示区
        self.display = scrolledtext.ScrolledText(root, state='disabled', height=15)
        self.display.pack(fill=tk.BOTH, expand=True, padx=5, pady=5)

        # 3. 底部发送区
        send_frame = ttk.Frame(root)
        send_frame.pack(fill=tk.X, padx=5, pady=5)

        self.send_entry = ttk.Entry(send_frame)
        self.send_entry.pack(side=tk.LEFT, fill=tk.X, expand=True, padx=2)

        self.hex_send_var = tk.BooleanVar()
        ttk.Checkbutton(send_frame, text="HEX发送", variable=self.hex_send_var).pack(side=tk.LEFT, padx=5)

        ttk.Button(send_frame, text="发送", command=self.send_data).pack(side=tk.LEFT, padx=5)

        self.refresh_ports()

    def refresh_ports(self):
        ports = serial.tools.list_ports.comports()
        self.port_combo['values'] = [p.device for p in ports]
        if ports:
            self.port_combo.current(0)

    def toggle_serial(self):
        if not self.running:
            try:
                self.ser = serial.Serial(self.port_var.get(), int(self.baud_var.get()), timeout=0.1)
                self.running = True
                self.open_btn.config(text="关闭串口")
                threading.Thread(target=self.read_serial, daemon=True).start()
                self.log(f"已打开 {self.port_var.get()}")
            except Exception as e:
                self.log(f"错误: {e}")
        else:
            self.running = False
            if self.ser:
                self.ser.close()
            self.open_btn.config(text="打开串口")
            self.log("串口已关闭")

    def read_serial(self):
        while self.running:
            if self.ser and self.ser.in_waiting:
                data = self.ser.read(self.ser.in_waiting)
                self.root.after(0, self.update_display, data)
            time.sleep(0.05)

    def update_display(self, data):
        self.display.config(state='normal')
        try:
            text = data.decode('utf-8')
            self.display.insert(tk.END, text)
        except Exception:
            hex_str = ' '.join([f'{b:02X}' for b in data])
            self.display.insert(tk.END, f"[HEX] {hex_str}\n")
        self.display.see(tk.END)
        self.display.config(state='disabled')

    def send_data(self):
        if self.ser and self.running:
            text = self.send_entry.get()
            if self.hex_send_var.get():
                try:
                    data = bytes.fromhex(text.replace(" ", ""))
                    self.ser.write(data)
                except Exception:
                    self.log("Hex格式错误")
            else:
                self.ser.write(text.encode('utf-8'))

    def log(self, msg):
        self.display.config(state='normal')
        self.display.insert(tk.END, f">> {msg}\n")
        self.display.config(state='disabled')

if __name__ == "__main__":
    try:
        import serial  # noqa: F401
    except ImportError:
        print("请先运行: pip install pyserial")
        raise SystemExit(1)

    root = tk.Tk()
    app = MiniSSCOM(root)
    root.mainloop()
```

---

## 6. 打包体积说明

无论采用哪种方案，最终用 `PyInstaller` 打包为 `.exe` 时的体积量级：

| 方案 | 打包后体积 | 原因 |
| --- | --- | --- |
| PyQt5/6 | 40–80 MB | 包含 Qt 完整 C++ 运行时 |
| Tkinter | 5–12 MB | 仅依赖 Python 自带 tcl/tk |
| Python 解释器基底 | 3–5 MB | 任何方案都含精简解释器，无法再压缩 |

**避坑指南**：

- 不要装 Anaconda（过大），用 Python 官网原版安装包。
- 不要用 PyCharm（过大），用 VS Code 或 Notepad++ 即可。
- 不用 PyQt，除非必须追求界面质感。

---

## 7. 一次性交付任务书（可直接投喂 AI）

> 以下任务书已自包含全部需求、边界与验收标准，**整段复制**投喂给执行方 AI 即可一次性产出完整可运行代码。
> 复制范围：从「===== 任务书开始 =====」到「===== 任务书结束 =====」。

### ===== 任务书开始 =====

#### 《轻量级串口调试助手（SSCOM 克隆版）》一次性开发任务书

##### 0. 给你的总指令（执行方 AI 必读）

你是一名资深 Python 桌面应用工程师。请严格按照本任务书，**一次性交付完整、可直接运行的全部代码**，不允许分多次交付、不允许用 `...` 或"其余代码类似"省略任何部分。若任务书存在歧义或遗漏，请自行做出合理决策，并在代码注释中说明你的决策，**不要反问用户**。交付顺序：先输出完整代码，再附一份不超过 20 条的《实现说明与自行决策清单》。

##### 1. 技术栈与硬性约束

1. 语言：Python 3.8+，文件编码 UTF-8。
2. GUI：仅允许使用标准库 `tkinter` / `ttk` / `tkinter.scrolledtext` / `tkinter.filedialog` / `tkinter.messagebox`。**禁止** PyQt、PySide、Kivy、CustomTkinter 等任何第三方 GUI 库。
3. 串口：仅允许第三方库 `pyserial`（`import serial`、`serial.tools.list_ports`）。除此之外**不允许任何第三方依赖**。
4. 禁止引用任何外部图片、图标、字体、声音资源；界面美观度靠布局、间距、配色（ttk 主题 + 少量 `configure(bg=...)`）实现。
5. 交付物：单个文件 `serial_tool.py`。
6. 目标系统：Windows 10/11。
7. 文件头部 docstring 必须包含：功能简介、依赖安装命令 `pip install pyserial`、打包命令 `pyinstaller -F -w serial_tool.py`。

##### 2. 程序架构要求

1. 采用类组织代码，至少包含：
   - `SerialManager`：封装串口打开/关闭/接收线程/发送，不持有任何 UI 控件引用。
   - `App(tk.Tk)`：主窗口与全部 UI 逻辑。
2. 线程模型（强制）：
   - 接收在独立守护线程中运行；线程**严禁直接调用任何 tkinter 控件方法**。
   - 线程与 UI 之间通过 `queue.Queue` 传递数据（元素类型：`bytes` 数据包，或 `("ERROR", msg)`、`("DISCONNECTED",)` 等事件元组）。
   - 主线程用 `self.after(50, self._poll_queue)` 轮询队列并刷新 UI。
   - 线程退出使用 `threading.Event`，关闭串口时 `event.set()` 后 `join(timeout=1)`，确保无线程残留。
3. 配置持久化：程序退出时（绑定 `WM_DELETE_WINDOW`）将全部控件状态写入脚本同目录 `config.json`；启动时若存在则恢复。JSON 读写需 try/except 容错，损坏时忽略并用默认值。

##### 3. UI 布局规范（自上而下共 6 区）

窗口标题：`轻量串口调试助手 V1.0`；初始大小 `960x680`，可拉伸；接收区与发送区随窗口拉伸（用 `grid` 的 `weight` 或 `pack(fill=BOTH, expand=True)`）。

- **区1 接收显示区**：`ScrolledText`，只读（写入时临时切 state），字体 `Consolas 10`（不存在时回退 `Courier New`），白底黑字。
- **区2 操作按钮行**（一行横排）：`[清除窗口] [保存接收数据] [接收数据到文件☐] [发送文件] [停止] [清空发送区] [保存参数] ☐窗口置顶`
- **区3 串口配置行**（一行横排）：`端口:[Combobox][刷新] 波特率:[Combobox] 数据位:[Combobox] 停止位:[Combobox] 校验位:[Combobox] 流控:[Combobox] [打开串口]`
  - 打开成功后按钮文字变 `关闭串口` 且背景变浅红；关闭后恢复。
- **区4 选项行**：`☐HEX显示 ☐加时间戳和分包显示 分包超时:[20]ms ☐HEX发送 ☐定时发送 [1000]ms/次 ☐加回车换行 编码:[Combobox] ☐自动滚动`
- **区5 发送区**：多行 `Text`（高约 5 行）+ 右侧竖排一个大号 `[发 送]` 按钮；支持快捷键 `Ctrl+Return` 触发发送、`Ctrl+L` 清除窗口。
- **区6 状态栏**（窗口最底）：左侧 `S:0 R:0`（双击清零）；右侧状态文本，如 `COM3 已打开 115200bps,8,1,None,None`；错误/提示临时显示 3 秒后恢复。

**控件默认值与取值表**：

- 波特率候选：1200, 2400, 4800, 9600, 19200, 38400, 57600, 115200, 230400, 460800, 921600, 1500000；默认 115200。
- 数据位：5, 6, 7, 8（默认 8）；停止位：1, 1.5, 2（默认 1）；校验位：None, Even, Odd, Mark, Space（默认 None）；流控：None, RTS/CTS, XON/XOFF（默认 None，对应 `rtscts`、`xonxoff`）。
- 编码候选：UTF-8, GBK, GB2312, ASCII, Latin-1；默认 UTF-8。
- 分包超时默认 20 ms；定时发送默认 1000 ms。
- 所有复选框默认不勾选，除"自动滚动"默认勾选。

**UI 状态矩阵（强制）**：串口打开期间禁用端口下拉、刷新、波特率、数据位、停止位、校验位、流控；其余控件保持可用。关闭后恢复。

##### 4. 功能细则（编号即验收编号）

- **F-01 端口扫描**：启动与点击 `[刷新]` 时调用 `serial.tools.list_ports.comports()`，下拉显示 `COMx`；无端口时清空并提示"未发现可用串口"；刷新不影响已打开连接。
- **F-02 打开/关闭串口**：按参数 `serial.Serial(port, baud, bytesize, parity, stopbits, timeout=0.05)` 打开（流控映射 rtscts/xonxoff）；成功→启动接收线程、更新按钮与状态栏；失败（`SerialException`/`PermissionError`/`OSError`/`ValueError`）→中文 `messagebox.showerror`，按钮与控件状态完全复原，不得残留"假打开"。关闭→先停线程再 `close()`。
- **F-03 接收显示**：未勾 HEX 显示时按编码解码，失败回退 HEX 并加前缀 `[HEX回退]`；勾选时显示两位大写空格分隔（如 `01 0A FF`）；勾自动滚动才滚到底；行数上限 5000 行，超出一次性删最旧 1000 行。
- **F-04 时间戳与分包**：勾选时每包单独成行 `[HH:MM:SS.mmm] RX→ <数据>`；分包算法在接收线程内：维护 buffer 与最后收数据时间戳，空闲超过"分包超时 ms"则整包入队清空，buffer 达 4096 字节强制入队；超时输入实时生效。未勾选时收到即显示（流式）。
- **F-05 发送**：未勾 HEX 发送按编码 encode 后发送，编码失败弹中文错误；勾 HEX 发送时忽略空白、大小写，须偶数个合法 hex 字符否则报错且不发送；勾加回车换行则末尾追加 `\r\n`；按字节数累加 S 计数；未打开时点发送提示"串口未打开"。
- **F-06 定时发送**：勾选后以间隔 ms 用 `after` 循环重复 F-05（每次实时读发送区）；取消立即停止；间隔非法（非整数或 <10）弹提示并自动取消；勾选时未打开串口则提示并不勾选。
- **F-07 停止按钮**：立即停止定时发送（并取消其勾选）与文件发送任务；对普通收发无影响。
- **F-08 清除类**：`[清除窗口]` 仅清接收区（不清计数）；`[清空发送区]` 仅清发送区。
- **F-09 计数与状态栏**：S/R 按字节数累计（HEX 模式也按字节）；双击清零；右侧状态随开/关/参数实时更新。
- **F-10 保存接收数据**：弹保存对话框，将接收区全文以 UTF-8（带 BOM）写盘；取消则静默返回。
- **F-11 接收数据到文件**：勾选时选定文件后每收到一包原始数据即追加写入（持有句柄、写失败 try/except 并告警）；取消立即关闭句柄；退出时仍勾选则自动关闭。
- **F-12 发送文件**：选文件后用 `after` 分块发送，每块 1024 字节、间隔 50 ms，状态栏显示 `发送文件: xx/xx 字节`；完成或按 `[停止]` 结束；打开失败弹中文错误。
- **F-13 保存参数**：立即写 `config.json` 并提示"参数已保存"；正常退出时同样自动保存。
- **F-14 掉线保护**：接收线程捕获 `SerialException/OSError` → 投 `("DISCONNECTED",)` → 主线程弹警告并执行与手动关闭完全相同的复原流程。
- **F-15 窗口置顶**：勾选 `-topmost True`，取消恢复。

##### 5. 必备工具函数（签名与行为固定）

```python
def hex_str_to_bytes(s: str) -> bytes   # 规则同 F-05；失败抛 ValueError(中文消息)
def bytes_to_hex_str(b: bytes) -> str   # 两位大写、空格分隔
```

全程序 HEX 解析/格式化必须复用这两个函数，禁止各处重复实现。

##### 6. 异常与边界总清单（每条都必须有对应处理）

1. 无任何串口启动/刷新；2. 打开被占用端口；3. 波特率等参数非法；4. 运行中拔除 USB；5. HEX 输入含非法字符/奇数位/为空；6. 发送区为空时点发送（决策：提示"发送内容为空"且不发送）；7. 定时间隔非法；8. 分包超时输入非法（回退 20 并提示）；9. 保存/打开文件时用户取消；10. 文件读写 IO 错误；11. config.json 损坏或缺失；12. 接收区大数据量冲击（行数上限已规定）；13. 重复快速点击打开/关闭按钮（加忙标志防重入）。

##### 7. 代码风格要求

1. 全中文注释；每个类与公开方法写 docstring。
2. 不使用全局变量传状态；魔法数字定义为类常量或模块常量。
3. 所有 `after` 循环任务保存 id，关闭/停止时 `after_cancel`。
4. 主入口 `if __name__ == '__main__':`。

##### 8. 验收自测清单（交付前逐条验证，并在《实现说明》中逐条回答"已满足/如何实现"）

- [ ] 无串口环境启动不崩溃，刷新有提示。
- [ ] 打开失败后按钮、下拉状态 100% 复原。
- [ ] 打开后参数控件禁用，关闭后恢复。
- [ ] 文本发送中文正常；HEX 发送 `31 32` 实际发出 0x31 0x32。
- [ ] HEX 非法输入有弹窗且不发送。
- [ ] 勾选时间戳后每包带毫秒级时间戳；改分包超时立即影响分包粒度。
- [ ] 定时发送按间隔循环、取消即停、`[停止]` 可停。
- [ ] 运行中模拟拔线触发掉线复原流程。
- [ ] S/R 计数为字节数且双击可清零。
- [ ] 连续接收 1 万行不卡（行数上限生效）。
- [ ] 退出重开后控件状态与上次一致。
- [ ] 文件发送可被 `[停止]` 中断且状态栏有进度。
- [ ] `pyinstaller -F -w` 打包后无控制台、可正常运行（config 路径需用 `sys.executable` 所在目录或 `os.path.dirname(os.path.abspath(sys.argv[0]))` 兼容处理）。

##### 9. 交付格式

1. 一个完整代码块：`serial_tool.py` 全文。
2. 代码块之后：《实现说明与自行决策清单》（≤20 条）+《验收清单逐条回答》。
3. 除以上内容外不要输出其他闲聊。

### ===== 任务书结束 =====

---

## 8. 使用建议与后续规划

### 8.1 投喂与执行

1. 将第 7 节任务书整段粘贴给执行方 AI，任务书已自包含，对方无需参考 SSCOM 截图。
2. 若一次输出被截断，回复："继续，从截断处输出剩余完整代码，不要重复已输出部分"。
3. 拿到代码后先 `pip install pyserial`，再 `python serial_tool.py` 跑验收清单第 1、2、5、6 条（无需真实硬件，可用虚拟串口软件或 loopback 测试），通过后再上真机。

### 8.2 运行与打包

```bash
# 安装唯一依赖
pip install pyserial

# 运行
python serial_tool.py

# 打包为单文件无控制台 exe
pyinstaller -F -w serial_tool.py
```

### 8.3 后续规划（二期）

- 网络调试：TCP Server/Client、UDP 收发。
- 多字符串发送面板（多条预设指令一键发送）。
- 波形/图表实时显示接收数据。

> 如需二期，可在本文档基础上续写《二期任务书》，保持同一套架构约束与验收格式。

---

*本文档结束。*