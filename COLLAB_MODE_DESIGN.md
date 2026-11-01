# 协作模式（Collaboration Mode）设计文档

> 目标读者：接手本项目的开发者 / 智能体。
> 所有「代码事实」均已在本仓库核实，标注 `文件:行号`，可直接跳转验证。
> 本文档描述的是**待实现**的设计，不是现状。现状见 `HANDOFF.md` 与 `.workbuddy/memory/`。

---

## 0. 一句话

在 RustDesk 上新增一个与「浏览模式（View Mode）」并列的会话模式：**多名远程租客可见、可移动虚拟光标，但同一时刻只有一方持有键鼠控制权；所有者始终保留物理否决权。**

---

## 1. 为什么需要新造一个模式

上游没有可直接复用的基座，三条路都走不通：

| 现成能力 | 为什么不能用 |
|---|---|
| 默认模式 | 所有连接的键鼠直接注入同一个 OS 输入队列，无仲裁。B 一动鼠标，A 的真光标就被拽走（`connection.rs:3210`） |
| 浏览模式（View Mode） | 是**纯只读**：`disable_keyboard = Yes` 会让 `peer_keyboard_enabled()` 为假，鼠标消息走 `else if show_my_cursor` 分支、`simulate = false`，`handle_mouse_simulation_` 根本不被调用（`connection.rs:3210-3235`） |
| 现有 `virtual_pointer` 补丁 | 在两种模式下都跑不通（详见 §2.3） |

---

## 2. 现状约束（已核实，不要再挖）

### 2.1 键鼠注入的共同闸门

```rust
// connection.rs:2348
fn peer_keyboard_enabled(&self) -> bool { self.keyboard && !self.disable_keyboard }
```

`self.keyboard` 来自被控端权限位 `OPTION_ENABLE_KEYBOARD`（`connection.rs:594`），`disable_keyboard` 默认 `false`（`connection.rs:616`），由**控制端**在浏览模式下置 Yes。

**这个函数名字只提键盘，但它同时是鼠标注入的开关。** 这是最容易误判的一点。

### 2.2 白板（虚拟光标渲染器）的三重耦合

| 环节 | 位置 | 约束 |
|---|---|---|
| 客户端发送 | `client.rs:3406` | 只有 `view_only && show-my-cursor` 才发 `show_my_cursor = Yes` |
| 客户端强制清除 | `client.rs:3300-3304` | 退出浏览模式时把 `show_my_cursor` 强行置回 No |
| UI 强制联动 | `remote_toolbar.dart:2772` | 开启「显示我的光标」会顺带强制打开浏览模式 |
| 服务端注册 | `connection.rs:5148` | 收到 `show_my_cursor = Yes` 才 `register_whiteboard()` |
| 未注册后果 | `whiteboard/client.rs:86` | `update_whiteboard` 静默 `return`，不报错不打日志 |

### 2.3 现有补丁的致命矛盾

`virtual_pointer` 补丁只在 `handle_mouse_simulation_` 里生效：

| 模式 | 白板注册 | `simulate` | 补丁是否执行 |
|---|---|---|---|
| 浏览模式 | ✅ | `false` | **不执行** → 光标可见但点不了 |
| 默认模式 | ❌ | `true` | 执行了但白板未注册 → **盲操作** |

**「虚拟光标可见」与「注入被拦截」在现有代码里互斥。** 协作模式必须同时打通这两条，这是本设计的第一个硬要求。

### 2.4 所有者的输入几乎不可观测

- 唯一通道：`try_start_record_cursor_pos()` 起一个线程，**每 33ms** 调 `crate::get_cursor_pos()` 写入 `LATEST_SYS_CURSOR_POS`（`input_service.rs:661-678`、`349`）
- 全仓库**没有** `SetWindowsHookEx` / `WH_MOUSE_LL` / `WH_KEYBOARD_LL` / RawInput
- 结论：**看不见按键，只能看见光标位移**
- 上游曾尝试用这套机制做"本机优先"，因阻塞式实现 + macOS bug 而整段注释，退化为 `fn active_mouse_() -> bool { true }`（`input_service.rs:1169`）

### 2.5 阻断所有者的唯一手段是全封锁

`platform::block_input(true)` = Win32 `BlockInput()`（`windows.rs:1960`）。键盘鼠标一起锁、需要管理员、UAC 安全桌面失效。**做不到只冻结鼠标、保留键盘。**

---

## 3. 角色与术语

| 术语 | 定义 |
|---|---|
| **所有者 Owner** | 被控端机器的物理使用者（文中的 A）。输入来自硬件，不可拦截 |
| **租客 Tenant** | 远程连接方（文中的 B、C…）。输入来自我们的代码，完全可控 |
| **控制权 Control Grant** | 独占式的键鼠注入许可凭证。同一时刻最多一方持有 |
| **协作模式 Collab Mode** | 本设计新增的会话模式，与浏览模式互斥 |
| **虚拟光标** | 白板浮窗绘制的、代表某个连接意图位置的图形 |

---

## 4. 设计原则

1. **控制权是显式对象。** 不是"谁最后发的输入谁生效"，而是一张有持有者、有超时、可撤销的凭证。
2. **单写者。** 任何时刻最多一方能注入键鼠。杜绝 §1 表第一行的交织问题。
3. **所有权优先且不可谈判。** 所有者不需要"申请"，动一下即夺回。这不是缺陷，是产品语义。
4. **租客之间对称，所有者与租客之间不对称。** 权限维度（剪贴板、文件、音频…）对租客一视同仁；抢占手段必须区别对待，因为物理层不对称。
5. **状态必须可见。** 每次控制权变化都要在屏幕上留下痕迹，不能靠用户猜。

---

## 5. 对外行为规范

### 5.1 控制权状态机

```
                 ┌──────────────────────────────────────┐
                 │                                      │
            ┌────▼────┐   owner_input    ┌──────────┐  │
            │  IDLE   ├─────────────────►│  OWNER   │  │
            │(无人持有)│◄─────────────────┤ (所有者) │  │
            └────┬────┘   idle_timeout   └────┬─────┘  │
       request   │                             │        │
                 │ tenant_click                │revoke  │
            ┌────▼────┐                         │        │
            │ GRANTED │◄────────────────────────┘        │
            │(租客持有)│                                  │
            └────┬────┘                                  │
      press      │      release / timeout                │
            ┌────▼─────┐                                 │
            │ DRAGGING │                                 │
            └────┬─────┘                                 │
                 └───────────────────────────────────────┘
```

| 状态 | 含义 | 可注入键鼠 |
|---|---|---|
| `IDLE` | 无人持有，所有租客仅可移动 | 否 |
| `OWNER` | 所有者在操作 | 否（所有者是物理的，不需要许可） |
| `GRANTED` | 某租客持有 | 是，仅该租客 |
| `DRAGGING` | 某租客按住不放 | 是，且禁止其他租客移动注入 |

> 注：`OWNER` 是**观测态**而非授予态。所有者的物理输入永远有效，软件只能观测到并据此收回租客的凭证。

### 5.2 转移规则

| 触发 | 迁移 | 说明 |
|---|---|---|
| 租客 T 按下鼠标（`IDLE`/`OWNER` 态） | `IDLE`/`OWNER` → `GRANTED(T)` | 显式请求，无需他人同意 |
| 租客 T 按住并移动 | `GRANTED(T)` → `DRAGGING(T)` | 拖拽期间禁止 T 以外的移动注入 |
| 租客 T 松开 | `DRAGGING(T)` → `GRANTED(T)` → `IDLE`（延迟） | 延迟见 §7.1 |
| 持有者空闲超过 `GRANT_TIMEOUT` | `GRANTED(T)` → `IDLE` | 默认 5s，可配 |
| 检测到所有者输入 | 任意 → `OWNER` | 立即撤销租客凭证，见 §6.3 |
| 连接断开 | `GRANTED(T)` → `IDLE` | 释放凭证与白板注册 |
| 所有者在 UI 上点「收回」 | 任意 → `OWNER` | 显式入口，与物理检测并行 |

### 5.3 各状态下的租客能力

| 能力 | 持有者 | 非持有者 |
|---|---|---|
| 移动虚拟光标 | ✅ | ✅ |
| 键鼠注入 | ✅ | ❌ |
| 白板显示光标 | ✅（高亮） | ✅（常规） |
| 剪贴板（若授权） | ✅ | ✅ |
| 文件传输（若授权） | ✅ | ✅ |
| 看见「谁在控制」角标 | ✅ | ✅ |

---

## 6. 权限矩阵

行 = 权限维度，列 = 角色。「授权」指所有者在被控端设置里的开关，对所有租客一致生效。

| 维度 | 所有者 | 租客（持有控制权） | 租客（未持有） |
|---|---|---|---|
| 鼠标移动 | 物理 | 注入 | 仅虚拟光标 |
| 鼠标点击/拖拽 | 物理 | 注入 | 禁止 |
| 键盘 | 物理 | 注入 | 禁止 |
| 剪贴板 | 物理 | 按被控端授权 | 按被控端授权 |
| 文件传输 | 物理 | 按被控端授权 | 按被控端授权 |
| 音频播放 | 物理 | 按被控端授权 | 按被控端授权 |
| 撤销他人控制权 | ✅ 独占 | ❌ | ❌ |
| 修改权限矩阵 | ✅ 独占 | ❌ | ❌ |

**关键点**：剪贴板、文件、音频是**权限**维度，可以对称；键鼠是**互斥资源**，必须单写者。把这两类混在一个"控制权"里会打架。

---

## 7. 技术设计

### 7.1 状态承载：per-connection，不再用全局单例

现有 `src/server/virtual_pointer.rs` 全部是全局单例（`REMOTE_POS` / `BUTTON_HELD` / `SAVED_LOCAL` / `GENERATION`），**3 人以上必然互相踩**。必须改造成：

```rust
struct TenantState {
    conn_id: i32,
    cursor: (i32, i32),        // 最新意图位置
    buttons: i32,              // 位掩码
    last_active: Instant,
    dragging: bool,
    generation: u64,           // 作废过期任务
}

struct CollabState {
    tenants: HashMap<i32, TenantState>,
    holder: Option<i32>,       // None = 无租客持有
    owner_pos: (i32, i32),     // 所有者最后已知位置
    owner_active_at: Instant,
}
```

访问一律 `Mutex<CollabState>`；**禁止跨 `.await` 持锁**（见 `AGENTS.md` Tokio 规则）。

### 7.2 仲裁器

单一入口，所有注入前必须过闸：

```rust
fn admit(&self, conn: i32) -> Option<Grant>;
// None = 拒绝注入（非持有者 / 所有者正在操作）
```

调用点全部在 `input_service.rs`：MOVE / DOWN / UP / WHEEL / `handle_key_`。
**键盘必须一起纳入**——现有补丁完全没有碰键盘，这是必须补的洞。

### 7.3 所有者夺回检测（补齐观测缺口）

两路信号，都在现有的 33ms 轮询线程里顺带采集，不新增线程：

| 信号 | 来源 | 判据 |
|---|---|---|
| 光标位移 | 现有 `get_cursor_pos()`（`input_service.rs:661-678`） | 相对 `owner_pos` 位移 > `OWNER_ACTIVE_DISTANCE`（建议 5px，沿用 `MOUSE_ACTIVE_DISTANCE`） |
| 任意输入 | **新增** `GetLastInputInfo()` | 时间戳前移即视为所有者输入，覆盖鼠标点击与键盘 |

`GetLastInputInfo()` 的价值：**所有者停在原地点击按钮时，光标不移动，位移判据失效；该 API 能捕获。** 全仓库目前未使用（grep 零命中），`winapi 0.3` 已具备。

采样伪码：

```rust
// 在 try_start_record_cursor_pos 的循环体内追加
let last_input = unsafe {
    let mut lii = LASTINPUTINFO { cbSize: size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    GetLastInputInfo(&mut lii);
    lii.dwTime
};
if last_input != last_seen_input {
    arbiter.on_owner_input();   // → 状态迁到 OWNER，撤销租客凭证
    last_seen_input = last_input;
}
```

同时必须**过滤自身注入造成的扰动**：租客注入也会刷新 `GetLastInputInfo`。做法是记录「最近一次注入的时刻与坐标」，若采样点落在注入后 `INJECTION_SETTLE_MS`（建议 50ms）窗口内且坐标与注入目标一致，则不计为所有者输入。

### 7.4 客户端改动（无法回避）

要同时满足「光标可见 + 注入被拦截」，必须让 `show_my_cursor` 脱离 `view_only`：

| 文件 | 改动 |
|---|---|
| `client.rs:3406` | `if view_only && ...` → 协作模式下独立发送 |
| `client.rs:3300-3304` | 退出浏览模式时不再强制清除 `show_my_cursor` |
| `remote_toolbar.dart:2772` | 取消「开光标 → 强制浏览模式」的联动 |
| `keyboard/shortcuts.rs` | 新增 `TOGGLE_COLLAB_MODE`，与 `TOGGLE_VIEW_ONLY`（`shortcuts.rs:74`）并列 |
| 新 toggle 名字 | `"collab-mode"`，与 `"view-only"` 同级，走 `sessionToggleOption` |

**代价必须写进产品说明：控制端也必须使用本魔改版**，官方客户端进入协作模式不生效。

### 7.5 白板改动

复用现有渲染器，只补三处：

| 改动 | 原因 |
|---|---|
| 注册解耦 | `register_whiteboard` 不再依赖 `show_my_cursor`，改为由「进入协作模式」触发；仍保留 `show_my_cursor` 兼容旧路径 |
| 所有者样式 | `Cursor` 结构已有 `argb` + `text`，所有者光标用独立颜色 + 「所有者」标签，与租客视觉区分 |
| 状态角标 | 白板是全屏透明覆盖窗（`whiteboard/windows.rs:36` 用 `get_displays_rect()` + `BorderlessFullscreen`），直接在角上画「当前控制：XXX」，**不必另开窗口** |
| spawn 去重 | `register_whiteboard`（`whiteboard/client.rs:42`）每次调用都 `thread::spawn` 一次 `start_whiteboard_`，靠 IPC 连接成功与否兜底。多人依次加入会重复 spawn，加 `TX_WHITEBOARD.is_some()` 短路即可 |

### 7.6 协议

服务端 → 客户端需要一条「控制权已变更」的推送，客户端据此更新角标与工具栏状态：

- 复用 `message::Union::Misc` 通道（服务端已有 `connection.rs:487` 等处理入口）
- 新增 misc name：`"collab-control"`，value 为 JSON `{"holder": 3, "state": "granted"}`（`holder` 为 conn_id，`0` 表示无人）

---

## 8. 必须处理的坑（每一条都已有实证）

| # | 坑 | 现状证据 | 处理 |
|---|---|---|---|
| 1 | 弹回判等用严格 `!=`，`mouse_move_to` 走 65535 归一化，反算回像素必有 ±1px 误差 → 永不弹回 | `libs/enigo/src/win/win_impl.rs:139`；`virtual_pointer.rs:123` | 改容差比较（≤2px） |
| 2 | `mouse_up` 是裸 `LEFTUP` 不带坐标，DOWN 与 UP 之间若所有者移动光标，抬起落在错误位置 | `win_impl.rs` mouse_down/up | 拖拽期间禁止所有者输入干扰；或用 `SendInput` 批量提交 [移动→down→up→移回] |
| 3 | 相对鼠标模式下白板整体失效 | `input_service.rs:1274` 有 `is_relative_mouse_active` 判断 | 协作模式强制绝对模式，进入前检测并提示 |
| 4 | 白板光标 1/4 节流，虚拟光标必跳帧 | `whiteboard/client.rs:93` 注释 "Send one movement event every 4" | 协作模式下放宽节流或按 `btns` 优先 |
| 5 | 多显示器 / DPI 缩放对齐未验证 | `mouse_move_to` 用 `MOUSEEVENTF_VIRTUALDESK`，坐标是虚拟桌面绝对坐标 | 需实测；白板用 `get_displays_rect()` 与之同源，理论一致 |
| 6 | `MOUSE_TYPE_UP` 白板事件在 `evt.x==0&&evt.y==0` 时回落到 `get_last_input_cursor_pos()`，协作模式下该值是所有者光标位置 | `input_service.rs:1533-1541` | 协作模式下改用租客自身 `cursor` |
| 7 | 每次滚轮都 `thread::spawn` 一个睡 400ms 的线程，连发会堆几十个 | `virtual_pointer.rs:115` | 改单个常驻定时器 + 版本号作废 |
| 8 | 所有者输入检测会误判自身注入 | 见 §7.3 | 注入后 50ms 沉降窗口 |

---

## 9. 分期计划

| 里程碑 | 内容 | 产出 | 验收 |
|---|---|---|---|
| **M1 可编译** | 状态机骨架 + per-conn 状态 + 仲裁器 + 键盘纳入 + 修坑 1/2/7 | 能编译，单租客可用 | A 与 B 同屏，B 移动不碰 A 光标；B 点击落点正确；A 一动即夺回 |
| **M2 可用** | 白板解耦 + 所有者样式 + 状态角标 + 协议推送 + 修坑 3/4/6 | 3 人可见可切换 | 3 人同时在线，角标显示正确持有者，无坐标互踩 |
| **M3 完备** | 权限矩阵 + 快捷键 + 超时配置 + 修坑 5 | 可交付 | 权限开关全部生效；多显示器 DPI 对齐通过 |

每期结束必须有一次真实双机联调，不能只靠 CI 编译通过。

---

## 10. 明确不做

1. **不做驱动层。** 不上虚拟输入设备 / 过滤驱动。理由：需要内核签名与安装权限，成本高一个数量级；MouseMux 卖的就是驱动 + 应用，其 rustdesk fork 单独运行毫无多光标能力（已核实：`src/platform/windows_mousemux.rs` 只是给 `SendInput` 的 `dwExtraInfo` 打标签）。
2. **不追求所有者与租客的物理对称。** 所有者的输入来自硬件，软件层无法拦截。文档中一律不宣称"平权"。
3. **不改白板渲染引擎。** 复用 `softbuffer + tiny_skia`，只加元素不改架构。
4. **不引入新的原生依赖。** `GetLastInputInfo` 已在 `winapi 0.3` 内。

---

## 11. 验收标准

单机双客户端 + 一台被控端的场景下，以下全部成立才算 M3 完成：

1. 所有者正常使用电脑（打字、拖文件、点按钮）时，光标**零感知**——不被拽走、不闪现
2. 租客 B 移动鼠标时，所有者屏幕上出现 B 的虚拟光标，所有者真光标不动
3. 租客 B 点击时，落点与虚拟光标位置**误差 ≤ 2px**，点击后真光标回到原位
4. 租客 B 拖拽时能正常拖动窗口/选区，松开后恢复
5. 所有者一动鼠标或键盘，租客立即失去控制权，角标同步更新
6. 三个租客同时在线，各自光标独立，无坐标互踩
7. 断开任一租客，其余不受影响，白板进程不退出
8. 浏览模式、协作模式、默认模式三者切换无残留状态

---

## 12. 风险

| 风险 | 影响 | 缓解 |
|---|---|---|
| 控制端也需魔改 | 无法与官方客户端互通 | 产品说明明示；保留默认模式兜底 |
| 33ms 轮询延迟 | 所有者夺回体感有 ≤33ms 滞后 | 可接受；必要时降轮询间隔至 16ms（代价是 CPU） |
| 白板是独立进程，跨进程协调 | 状态不同步、崩溃后不恢复 | 加心跳与自动重启；崩溃时降级为「无光标但可操作」 |
| Windows 7 / 低版本 Win10 白板不支持 | 功能不可用 | 沿用上游检测（`connection.rs:5132` `is_win_10_or_greater`），不支持时明确提示 |
| 上游跟进 | 改动面大，未来 rebase 冲突重 | 尽量 additive；核心逻辑集中在新建文件，避免改 `input_service.rs` 过多行 |

---

## 附：关键代码索引

| 位置 | 作用 |
|---|---|
| `connection.rs:2348` | `peer_keyboard_enabled()` 键鼠共同闸门 |
| `connection.rs:3210-3235` | 鼠标消息 `simulate` 取值决定是否注入 |
| `connection.rs:5148` | 白板注册触发点 |
| `input_service.rs:1169` | `active_mouse_()` 恒真，所有者优先被禁用 |
| `input_service.rs:1274` | 光标事件分发（含相对鼠标判断） |
| `input_service.rs:1515` | `handle_mouse_show_cursor_` 白板数据源 |
| `input_service.rs:661-678` | 33ms 光标采样线程 |
| `whiteboard/client.rs:42` | `register_whiteboard` 进程启动 |
| `whiteboard/client.rs:86` | 未注册静默丢弃 |
| `whiteboard/windows.rs:36` | 全屏透明覆盖窗 |
| `windows.rs:1960` | `BlockInput()` 全封锁 |
| `libs/enigo/src/win/win_impl.rs:139` | `mouse_move_to` 归一化注入 |
| `client.rs:3290-3304` | 浏览模式五项副作用 |
| `client.rs:3406` | `show_my_cursor` 依赖 `view_only` |
| `remote_toolbar.dart:2588-2591` | 「显示我的光标」菜单位置 |
| `remote_toolbar.dart:2772` | 开光标强制进浏览模式 |
