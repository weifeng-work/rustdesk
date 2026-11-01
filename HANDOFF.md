# HANDOFF — RustDesk 协作模式（魔改）

> 接手的智能体：先读这份，再读 `COLLAB_MODE_DESIGN.md`（设计定稿）和
> `.workbuddy/memory/2026-11-01.md`（踩过的坑与已核实事实）。

## 一句话

给 RustDesk 增加一个与「浏览模式」并列的**协作模式**：多名远程租客可见、可移动虚拟光标，
同一时刻只有一方持有键鼠控制权，所有者（被控端物理使用者）一动即夺回。

## 当前进度

**M1 代码已写完，未编译验证。** 本机不做编译（会把用户的机器跑死），走 GitHub Actions。

```
 M src/server.rs                 # +4   注册 collab 模块
 M src/server/input_service.rs   # 四处接入 + 33ms 轮询埋点
 M src/server/connection.rs      # +12  键盘闸门 + 断连清理
 A src/server/collab.rs          # 新增，控制权状态机
 D src/server/virtual_pointer.rs # 旧方案，已删除
```

## 模块边界

| 文件 | 职责 |
|---|---|
| `src/server/collab.rs` | 全部协作模式状态与裁决。外部只通过 `on_move` / `admit_press` / `press_target` / `on_release` / `admit_wheel` / `wheel_target` / `after_wheel` / `keyboard_allowed` / `poll` / `detach` 交互 |
| `src/server/input_service.rs` | 鼠标四个分支接入；`try_start_record_cursor_pos` 的 33ms 循环里调 `collab::poll` |
| `src/server/connection.rs` | 键盘在 `peer_keyboard_enabled()` 之后再过一道 `collab_keyboard_allowed()`；断连时 `detach` |

开关：环境变量 `RUSTDESK_COLLAB=1`，**默认关闭**，别改成默认开。

## 行为（M1 已实现）

- 移动：只更新该连接的虚拟光标坐标，真实光标不动
- 按下：取得控制权 → 真实光标瞬移到租客坐标 → 注入 down
- 拖拽：`cursor_held` 为真期间持续注入
- 抬起：`cursor_held=false`，150ms 后归还到所有者原位置；键盘所有权再保留 5s
- 滚轮：先瞬移再滚，结束 150ms 后归还
- 键盘：只有当前持有者能注入
- 所有者夺回：`GetLastInputInfo` 时间戳变化 + 光标位移 >5px，33ms 内生效

## 立刻该做的

1. 推到自己的 fork，跑 GitHub Actions 拿到第一次**真实编译**结果。M1 只做过语法级 parse，
   没有任何类型检查——`collab.rs` 里大概率有借用/类型问题要改几轮。
2. 编译绿了之后，按 `COLLAB_MODE_DESIGN.md` §11 的 8 条验收标准做双机联调。
3. 白板解耦仍在 M2：**当前版本租客看不到自己的虚拟光标**（白板注册仍绑死浏览模式），
   所以 M1 的验收只能验「所有者光标不被抢」「点击落点正确」「所有者夺回」，验不了"看得见"。

## 不要再挖的东西（已查清）

- `libs/vnc` 与 `src/server/input.rs` **不存在**，输入处理在 `src/server/input_service.rs`
- 代码**不用** `SetCursorPos`；`mouse_move_to` 走 `SendInput + MOUSEEVENTF_MOVE|ABSOLUTE|VIRTUALDESK`
  （`libs/enigo/src/win/win_impl.rs:139`）
- `mouse_down/up` 是裸 `LEFTDOWN/LEFTUP` 不带坐标 → 点击落在当前光标位置
- **不要自己写透明浮窗**。`src/whiteboard/` 已有 tao 透明置顶鼠标穿透窗 + 多光标渲染
- **`peer_keyboard_enabled()` 名字骗人**：它同时是鼠标注入的总开关（`connection.rs:3210`）。
  浏览模式禁键盘 → 鼠标也被拦 → `simulate=false` → `handle_mouse_simulation_` 根本不执行
- 不要用「浏览模式 + 显示我的光标」当基座：那种组合下注入被完全禁用，点不了

## 编译环境（本机，已就绪）

| 依赖 | 状态 |
|---|---|
| rustc / cargo 1.96.0 MSVC | ✅ 开箱可用，**不需要**设 RUSTUP_HOME |
| VS BuildTools 2022 + SDK 10.0.26100 | ✅ |
| LLVM / libclang 23.1.4 | ✅ `C:\tools\llvm`，`LIBCLANG_PATH` 已写入用户环境变量 |
| `libs/hbb_common` submodule | ❌ **仍是空目录**，必须先 `git submodule update --init`，否则 cargo 连 workspace 都加载不了 |
| vcpkg + libvpx/libyuv/opus/aom/ffmpeg | ❌ 未安装 |
| Flutter SDK | ❌ 仅打完整 UI 时需要 |

## 硬约束

- 被控端必须能开「显示我的光标」才有虚拟光标可看；M1 版本该开关仍受浏览模式限制，见设计文档 §7.4
- 拖拽期间不能禁用注入，否则拖不动窗口/选区（已由 `cursor_held` 处理）
- 相对鼠标模式（`MOUSE_TYPE_MOVE_RELATIVE`）下白板整体失效，协作模式需强制绝对模式
- 不要在 `handle_mouse_simulation_` 里 `return`，会漏掉末尾的 `to_release` 键释放

## 参考实现

`https://github.com/MouseMux/rustdesk`（**AGPL-3.0**，非 GPL），分支 `mousemux-v2.2-flutter-complete`。
其做法：Win32 消息 IPC 向 MouseMux 应用申请连接槽，换回 `mouse_id`/`keyboard_id`，
再用 `SendInput` 的 `dwExtraInfo` 打标签。**多光标能力本体在闭源 MouseMux 应用+驱动里**，
单独跑那个 fork 没有任何多光标能力。只可参照思路，不可指望复用。
