# dipswitch

个人本机控制台。基于 **GPUI Kit 0.8** 的原生桌面应用（非 Electron），左侧固定导航 + 右侧模块内容区。名字取自**拨码开关（DIP switch）**：一排开关集中在一个面板上，拨过去能拨回来——这就是本应用对每台机器做的事。

目前有两个模块：

- **Fonts** — 已安装字体的浏览、face 级预览、对比与一键复制；
- **Environment** — 用户级环境变量的读取/编辑，以及自定义分组的「一键启用 / 停用还原」。

所有数据都存在本地 SQLite 库里：无账户、无云同步、无遥测、无后台进程。以 Windows 为主要目标平台，其他平台按只读/进程级降级。

![dipswitch 的 Fonts 模块：face 级预览、字号滑杆与对比入口](shots/dipswitch-fonts.png)

## 功能

### 外壳

- **左侧导航**：`Fonts` / `Environment` 两个目的地，选中态是行自身的填充高亮（不是左侧色条）。导航可折叠成图标宽度，折叠状态与上次停留的模块都会落库，重启后回到原样。
- **键盘路径**：`Alt+1` Fonts、`Alt+2` Environment、`Ctrl/Cmd+B` 折叠导航。
- **标题栏**：自绘（隐藏系统标题栏），只放窗口装饰、当前模块名与全局 Dark 开关；模块自己的命令留在模块工具栏里。
- **状态栏**：由外壳统一绘制，左侧是当前模块的计数，右侧是当前模块的键盘提示。
- **模块常驻**：切走的模块视图实体不销毁，切回来是即时的，不重新扫描、不丢状态。

### Fonts 模块

- **按 family 浏览**：扫描系统字体，按 family 聚合，显示 face 数量、格式与等宽标记；字母序排列。字体名统一用界面字体渲染（图标字体没有拉丁字形，用自身字形渲染会变成一排豆腐块），`Aa` 缩略图与预览画布仍然用字体本身。
- **搜索**：按 family 名过滤，大小写不敏感；搜索框支持清空。
- **face 级预览**：选择 Bold / Italic / Light 等具体 face，预览、复制和元信息都针对当前 face；默认优先 Regular。
- **Copy as**：一键复制 family 名，或从下拉菜单复制 `Family + style`、PostScript 名、CSS 规则。
- **收藏 / 最近 / 筛选**：星标收藏 family；最近打开自动记录；支持 All / Favorites / Recent / Mono 四种筛选。
- **预览画布**：示例文本可在 Latin / 中文 / Code / Numbers 间切换；画布底色支持 Match theme / Light / Dark。
- **字号滑杆**：32 / 48 / 72 / 96 为刻度点，拖动连续预览，松手吸附到最近的点。
- **字体对比**：固定 2–4 个 family+face，同示例、同字号、同底色并排比较。
- **模块内快捷键**：`Ctrl/Cmd+F` 聚焦搜索，`Enter` 进入列表，`↑/↓` 浏览，`Ctrl/Cmd+R` 重新扫描，`Ctrl/Cmd+C` 复制。

### Environment 模块

- **读取用户级环境变量**：Windows 下读 `HKCU\Environment`，保留 `REG_SZ` / `REG_EXPAND_SZ` / `REG_MULTI_SZ` 三种类型；`REG_MULTI_SZ` 只读展示（分隔符本身是数据，直接编辑会被改坏）。表头是 Variable / Value / Type / Source / Actions。
- **行交互**：鼠标悬停整行高亮；双击行（key / value / type / source 区域）直接打开编辑弹窗——被分组接管的 key 打开的是分组里那一条。操作列是行的兄弟元素，在按钮上双击不会误触发行编辑。
- **增删改**：`Add variable` 新建；行菜单里 `Edit variable` 改值、`Delete from user environment` 删除（带确认对话框，说明后果）。写完同步当前进程环境并广播 `WM_SETTINGCHANGE`，新启动的进程即可看到。
- **过滤与复制**：按 key 或 value 过滤；每行可复制值。
- **自定义分组**：`New group` 建组（名称 + 备注），组内可增删改变量；行上的开关一键启用/停用整组。
- **同名 key 的覆盖规则**：启用的分组按**启用先后**排优先级，**最后启用的那一层覆盖**同名 key。分组详情里会标出「有几个变量被更晚启用的分组覆盖」。
- **停用后恢复**：某个 key 第一次被分组接管时，把它当时的原值（含「原本不存在」这一状态）记入基线。停用分组后重新解析——还有别的分组接管就生效下一层，没有分组接管就恢复基线；基线是「原本不存在」就把这个变量删掉。
- **不骗人**：被启用分组接管的 key，在系统视图里禁止直接编辑/删除，行菜单会把你引去编辑分组里的那一条（否则你写的值会被分组立刻覆盖，界面却显示已保存）。
- **变更流水**：每次启用/停用/删除都会记一条 `env_event`，分组详情底部显示最近几条，可单独清空。
- **非 Windows 平台**：写入落点退化为「当前进程环境」，界面上的 scope 文案会如实说明，不会假装已经持久化。

## 图标

全部界面图标来自**同一个家族**（Lucide，24×24 网格、2px 描边、圆头端点），所以风格天然统一；组件自带的窗口按钮也是 Lucide，不需要额外对齐。

- **语义命名，不按形状命名**：`src/icon.rs` 的 `AppIcon` 是应用内图标的唯一出口，变体叫 `Delete`、`Edit`、`NewGroup`、`Recent`，不叫 `Trash`、`Pencil`。图形在哪、为什么选它，集中写在 `AppIcon::icon_name()` 一个 `match` 里，改图形不影响调用点。
- **不受组件层 106 个的限制**：`gpui_component::IconName` 只是 `gpui_kit_assets::IconName`（1,830 个）的兼容子集，而组件 API 收的是 `impl Into<Icon>`、`Icon` 对任何实现 `IconNamed` 的枚举有 blanket `From`——所以全量目录都能用。代价是共享枚举给出的是**路径**，默认包只嵌了那 106 个的字节，因此本应用用到的子集之外图形由 `icon_assets!` 点名嵌入（当前 10 个，约 +19 KiB）。
- **漏登记会立刻失败**：`icon::tests::every_app_icon_resolves_through_the_registered_source` 逐个断言 `AppIcon` 的图形能从注册后的资产源取到 SVG 字节——用了没登记的图形不会编译报错，只会在界面上画出一片空白，这条测试就是拦它的。
- **品牌标**：一枚字母 `d`，碗部是一格打到 ON 的开关。任务栏/文件图标用全彩版，标题栏用单色描边版（原因见「实现要点」）。
- 语义修正记录：`Compare` 由「仪表盘」改为两栏并排、`Recent` 由「顺时针箭头」改为时钟（原来与「重新扫描」撞形）、`Edit` 由「查找替换」改为铅笔、`Delete` 由「退格键」改为垃圾桶、`NewGroup` 由「文件夹」改为新建文件夹、`Environment` 由终端方块改为 `Variable`、`Monospace` 由「大小写」改为花括号；警告（`TriangleAlert`）与错误（`CircleAlert`）从此分形分色。

## 运行

```bash
cargo run
```

要求 Rust 1.85+（edition 2024）。Windows 首次构建需要 Windows SDK（SQLite 走 `bundled`，由 `cc` 现场编译 amalgamation）；Linux 需要 X11/Wayland 开发库。

`Cargo.toml` 里给 `dev` profile 的依赖单独开了 `opt-level = 3`（本 crate 用 O1）。GPUI 的布局与绘制在未优化构建下会有明显的交互滞后，这个设置让 `cargo run` 也保持跟手；代价是首次构建更久。

依赖基线：

```text
gpui-kit 0.8.0
├─ gpui-pre 0.3.9        ← GPUI 本体（Zed gpui 快照）
├─ gpui-component 0.8.0  ← 控件库（Sidebar / Dialog / Table / Switch …）
└─ gpui-kit-assets       ← 默认图标（应用层只能用组件层导出的那一份 IconName）
fontdb 0.24              ← 系统字体枚举
rusqlite 0.40 (bundled)  ← 本地配置库
winreg 0.56              ← HKCU\Environment 读写（仅 Windows）
windows-sys 0.60         ← WM_SETTINGCHANGE 广播（仅 Windows）
serde / serde_json / dirs

# 仅 Windows 的 build-dependency
embed-resource 3.0.12    ← 编译 .rc 进 exe（图标 + 版本信息）；gpui-pre 本来就依赖它，
                           所以依赖树里没有任何新增 crate
```

## 结构

```text
build.rs           Windows 下生成 .rc 并编进 exe 资源：图标 + 版本信息（与 Zed 同款做法）
assets/
├─ brand.svg       应用图标源：全彩带底板，喂 .ico
├─ brand-mark.svg  应用内品牌标：同一枚 d，按 Lucide 描边语言重画（见「图标」）
└─ app.ico         由 tools/make-icon.py 从 brand.svg 生成的 7 档图标，作为产物提交进仓库
src/
├─ lib.rs            库目标：让视图与测试共用同一份真实实现
├─ main.rs           二进制入口：初始化、注册资产源、开库、绑键、建窗口
├─ shell.rs          窗口外壳：标题栏 + 左侧导航 + 模块路由 + 状态栏
├─ icon.rs           全局图标出口：AppIcon 语义 → Lucide 图形 + SVG 注册 + 品牌标
├─ fonts_view.rs     Fonts 模块（FontsModule + FontListDelegate + 对比视图）
├─ environment_view.rs  Environment 模块（变量表 / 分组主从 / 弹窗表单）
├─ env_model.rs      分层解析：谁覆盖谁、基线何时记录、停用恢复什么（纯逻辑）
├─ env_service.rs    把解析结果落到环境与数据库（能力缝 EnvBackend）
├─ system_env.rs     平台适配：HKCU\Environment 读写 + 广播 / 进程环境回退
├─ store.rs          SQLite 配置库：schema、偏好、分组、基线、流水
├─ settings.rs       Settings Global + 旧 JSON 的一次性导入
├─ fonts.rs          fontdb 扫描与 family/face 模型（不依赖 GPUI）
└─ size_slider.rs    带预设刻度与松手吸附的字号滑杆
examples/canvas_probe.rs 自绘画布探针：节点用 absolute div、连线用 canvas + PathBuilder
tools/cap-window.ps1     用 PrintWindow 抓当前窗口，供 UI 走查
tools/make-icon.py       从 brand.svg 生成 app.ico（Chrome 无头按真实像素光栅化）
tools/probe-exe-icon.ps1 用 ExtractIconExW 验证图标确实进了 exe 资源
```

状态所有权：

- `Dipswitch`（外壳）只拥有「当前模块」「导航是否折叠」和两个模块实体；模块自己拥有数据与交互状态。
- `EnvironmentModule` 持有的是**快照**（系统变量、分组、组内变量、来源诊断），每次操作后整体重载，视图不维护第二份真相。
- `env_model::plan` 是覆盖/恢复规则的唯一实现；`env_service` 只负责喂数据和落盘；视图不参与判断。
- `FontListDelegate` 仍是字体列表的唯一数据源；`Settings` Global 存长期偏好，退出时统一落库。

## 实现要点

**分层解析。** `build_layers()` 把启用的分组按 `enabled_seq` 升序排成层，`plan()` 对每个被接管的 key 取最高层为生效值，并对「有基线但已无分组接管」的 key 生成恢复步骤。启用＝`enabled_seq = max+1`，停用＝清空——所以「最后启用者覆盖」和「退回下一层」是同一套规则的两个方向，不需要额外记录每组的快照。写入失败的 key **不会**释放基线，下次操作仍会重试。

**基线保真。** 基线表存 `value`（可为 NULL）+ `kind`，因此 `%VAR%` 型值和「本来没有这个变量」都能原样还原。`REG_MULTI_SZ` 的内部表示用 `\0` 连接各项，展示时才用 `; ` 分隔，写回按项重新编码。

**能力缝。** `EnvBackend`（read / write / remove）让「启用 → 覆盖 → 停用 → 恢复」这条主链路可以用内存假后端做端到端测试，不碰真实注册表；真实后端 `SystemEnvBackend` 只在生产路径上出现。

**标题栏里的控件要自己拦下鼠标按下。** Windows 把自绘标题栏的整条内容声明成 `HTCAPTION` 拖拽区：鼠标按下会交给系统窗口移动的模态循环，控件只收到按下、收不到抬起，`on_click` 永远不触发。所以标题栏里的抽屉按钮和主题开关都在自己的按下事件里 `stop_propagation()`（`shell::intercept_title_bar_press`），平台层就不会把这次按下交出去；其余空白区域仍然交给系统拖动。

**图标只用一个家族，语义名与图形名分开。** 见上面的「图标」。

**`Assets::load` 未命中返回 `Err`，不是 `Ok(None)`。** 所以组合资产源时自定义源必须在前、默认包兜底；反过来写并用 `?` 串联，会让默认图标全部加载失败（实测表现为运行期 panic）。

**`Icon` 是单色着色，多色 SVG 会被压成剪影。** `Icon::into_svg` 给 `svg()` 设了 `text_color`，GPUI 用 SVG 的 alpha 做蒙版再上色。因此应用内的品牌标必须是描边图形（`assets/brand-mark.svg`），带底板的全彩版（`assets/brand.svg`）只能用于 exe 图标——实测把全彩版放进标题栏会渲染成一个实心方块。

**Windows 的窗口图标只能走资源编译。** `WindowOptions.icon` 标注 X11 only，GPUI 在 Windows 下也不暴露 HWND，所以运行期 `WM_SETICON` 这条路不通。做法照搬 Zed 的 `crates/windows_resources`：`build.rs` 在 `OUT_DIR` 里**生成** `.rc`（`#pragma code_page(65001)` + `1 ICON` + `VERSIONINFO`），交给 `embed-resource` 编译。它自带 `KitsRoot10` → `vswhom` → `PATH` 的 `rc.exe` 发现链，支持 GNU 工具链走 `windres`，并认 `RC` / `RC_<target>` 覆盖；顺带把版本信息也写进文件属性。两点与 Zed 有意不同：**不写 manifest 行**——`gpui-pre` 自己的 build.rs 已经嵌了 `1 RT_MANIFEST`（实测产物含 `dpiAware`），再来一份同 ID 同类型就是自相冲突；**失败降级成 `cargo:warning`** 而不是 `unwrap()`——图标是观感项，缺 SDK 时也该构建出可用的 exe。

**资源生成不属于构建。** `build.rs` 只**消费**已提交的 `app.ico`，不生成它——生成是维护者的显式步骤（`python tools/make-icon.py`）。把生成塞进构建等于让构建依赖 Python 加一个 SVG 光栅器，与「离线构建」的验收项冲突。代价是一条纪律：**改了 `assets/brand.svg` 必须重跑该脚本**，否则 exe 里还是旧图标而构建不会报错。该脚本自身也不写死机器专属路径：光栅器按 `DIPSWITCH_RASTERIZER` → `rsvg-convert` → PATH 上的 Chrome/Edge → 平台标准安装位置查找（它刻意不尝试 `convert`——Windows 上那是 FAT 转 NTFS 的磁盘工具，不是 ImageMagick）。

**交互路径上的落盘只提交一次。** `Settings::save` 把十几条偏好收进一个事务（`DipswitchStore::set_settings`），库本身跑 WAL + `synchronous = NORMAL`。默认 journal 下逐条提交在 Windows 上要 ~50 ms，整组一个事务后是 ~3 ms——否则每次切模块、点收藏都在这上面卡一下。

**持久化。** 配置库在 `%LOCALAPPDATA%\dipswitch\dipswitch.db`（Linux `~/.config/…`，macOS `~/Library/Application Support/…`）。schema：

```text
app_setting(key PK, value)                 偏好，一个 key 一行（appearance.* / fonts.* / window.* / dipswitch.*）
env_group(id PK, name UNIQUE, note, is_enabled, enabled_seq, created_at)
env_variable(id PK, group_id FK→env_group ON DELETE CASCADE, key, value, kind, UNIQUE(group_id, key))
env_baseline(key PK, value NULL, kind, captured_at)      接管前的原值，恢复依据
env_event(id PK, at, detail)                             启用/停用/覆盖/恢复流水
```

`PRAGMA user_version` 记 schema 版本，`foreign_keys = ON`。两次改名都带数据搬迁：旧 `font-manager/settings.json` 与旧 `workbench/workbench.db` 在第一次启动时各自导入一次，原文件保留不动，`app_setting` 里记下来源。环境变量分组的基线表是「停用后能否还原」的唯一依据，所以导入逻辑与 `Settings::load` 分开——测试不会因为机器上恰好有旧文件而拿到意外结果。

## 路线图

按「零风险高频 → 只读系统观察 → 提权写入」的顺序推进：

| 阶段 | 内容 |
|---|---|
| 下一步 | 模块规格表（新增模块只改一处）、schema 编号迁移、备份/还原点服务、只读命令执行 seam；然后上 **Toolbox**（JSON / 编码 / 哈希 / JWT / 时间戳） |
| 之后 | **Ports & Processes**（端口 → 进程 → 定位，结束进程需二次确认） |
| 更后 | **Hosts Manager**（分组切换 + 提权写入）、**SSH 配置**（只读解析 `~/.ssh/config`） |

明确不做：系统级管理（`HKLM`、服务、驱动、计划任务）、跨设备同步、云账户、插件市场、脚本沙箱、自动更新与代码签名。

## 验证

```text
cargo fmt --check              通过
cargo check --all-targets      通过
cargo clippy --all-targets     0 warning
cargo test                     47 passed + 1 ignored（真实注册表探针）
cargo build --release          通过
```

自动化覆盖（`cargo test`）：

- **覆盖与恢复**：最后启用的分组赢；停用最高层退回下一层而不是直接跳回原值；停用最后一层恢复原值；分组新建的 key 在停用后被删除；删除已启用的分组会先还原它接管的东西；连续两次 `reconcile` 幂等（第二次不动任何值）。
- **基线**：首次接管记录原值（含「原本不存在」）；写入失败时保留基线以便重试；恢复完成后释放基线。
- **护栏**：被启用分组接管的 key 拒绝系统层直接编辑/删除，未接管的放行。
- **存储层**：分组同名唯一、组内 key 唯一、跨组同名允许、级联删除、`enabled_seq` 递增与清空、值类型按注册表名存取、流水倒序与清空。
- **偏好**：`Settings` 全字段往返 SQLite；空库回默认值；旧 JSON 能导入、坏 JSON 不炸且不落库。
- **图标注册完整性**：每个 `AppIcon` 的图形都能从注册后的资产源取到真实 SVG 字节；`AppIcon::ALL` 覆盖全部变体，且没有两个语义图标意外共用同一图形。
- **平台适配**：`REG_SZ` / `REG_EXPAND_SZ` / `REG_MULTI_SZ` 的字节级编解码往返（含 `%`、空格、分号），不支持的类型不会被当字符串读出。
- **真实注册表探针**（`cargo test -- --ignored`）：写一个 `DIPSWITCH_SELFTEST_PROBE` 探针 key → 读回校验类型与值 → 确认当前进程可见 → 删除并确认清走。默认不跑，因为它真的会动 `HKCU\Environment` 并广播一次 `WM_SETTINGCHANGE`。

手工验证（Windows，PostMessage 模拟真实点击 + PrintWindow 抓图）：

- 启动落在 Fonts 模块，标题栏、导航、状态栏、字体列表与预览均正常；
- 导航切到 Environment → 读到本机 `HKCU\Environment` 变量，scope 文案、表格列、Source/Actions 均正确；
- Groups 页签的空状态说明、`New group` 弹窗的字段与按钮渲染正确；
- **重启回到原模块**：停在 Environment 后退出并重启，直接落在 Environment（`dipswitch.active_module` 落库生效）；
- 暗色开关、导航折叠、字体 face 切换、对比、收藏筛选均已验证。

**未验证**：本机键盘注入被系统拒绝（`SendInput`、`WM_CHAR`、`WM_KEYDOWN` 均无法把字符送进 GPUI 窗口），因此**弹窗表单提交、分组创建与「启用/停用」按钮的真实注册表往返没有走 UI 实测**——这条链路只由 `env_service` 的内存后端测试与注册表探针分别覆盖。字体扫描、复制通知、以及 `Ctrl/Cmd+C` 等键盘路径同样只做了代码级检查。

## 已知限制

- 环境变量只管**用户级**（`HKCU\Environment`）；系统级 `HKLM` 需要管理员权限，本期不做。
- `REG_MULTI_SZ` 只读；`REG_DWORD` 等类型不在本模块管理范围内（列出来只会诱导用户改坏）。
- 分组启用状态是持久的：应用关闭后分组仍然生效，直到用户停用或删掉分组。没有「退出时自动全部还原」这种隐藏行为。
- 字体模块不做 CJK/字符覆盖率检测、不安装/卸载字体；对比集合不持久化，最多 4 个。
- 侧栏宽度不持久化（导航折叠状态持久化，字体模块的分栏宽度不持久化）。
- UIA 里列表项的可访问名称为空（`ListItem::accessibility_label` 已设置，但外层 `ListState` 包装节点未取到名字），属于组件层限制。

## License

MIT
