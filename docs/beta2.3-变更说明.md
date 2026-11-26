# beta2.3 变更说明 —— 多网卡 + 安卓全面屏

> 基线：beta2.2（v0.2.2，运维核心包）。交付日期：2026-11-06。
> 本次是**修 bug 为主**的版本，核心目标只有一个：**多网卡主机要把所有网卡显示出来**。

---

## 一、两个致命根因

### 根因 1：旧前端只渲染 `network[0]`

旧版 `web/js/app.js` 网络卡片取的是：

```js
const net = f.network && f.network[0];
```

`/proc/net/dev` 的第一张网卡通常就是 `eth0`，所以 `eth1`、`br-lan`、`utun`、
`docker0`、`veth*` 等即使采集到了也永远不显示。用户本机是多网卡主机，因此「只看到 eth0」。

此外 `server/store.py` 的历史曲线、`pushCharts()` 的折线图也各自在第一篇 / 最大流量之间
口径不一，多网卡时会出现「大数字显示 br-lan、曲线画 eth0」。

### 根因 2：busybox awk 的 `%d` 32 位溢出（致命）

真实目标 `192.168.1.100`（iStoreOS 24.10.8）上，累计字节超过 `2^31` 后，采集脚本里的
`printf "%d" <累计字节>` 在 busybox awk 里变成负数。修复前实测：

```text
eth0     total=1158872.6 / -2097152.0 KB
eth1     total=-2097152.0 / 1139616.6 KB
br-lan   total=-2097152.0 / -2097152.0 KB
```

后果：累计流量错误、差值速率变 0 或负数、主网卡判断错误 —— 即使前端改了，多网卡依然显示不对。

修复后同一目标实测：

```text
eth0   up      total= 1199856.0 / 8661739.3 KB
eth1   up      total=10889051.2 / 1197685.8 KB
br-lan up      total=10949712.1 / 9134936.0 KB   ← primary
utun   unknown total=10706899.5 / 10134509.0 KB
共 19 个接口，negative totals: []
```

---

## 二、改动清单（逐文件）

| 文件 | 改动 |
|---|---|
| `collector/remote/collect.sh` | `NET` 段增加第 4 字段 `operstate`；输出 `/proc/net/dev` 中除 `lo` 外**全部**接口；所有累计型数值（`CPU`/`MEM`/`DSK`/`NET`/`uptime`/Docker 计数）统一 `%.0f`；`fan_info()` 读 `fan*_label` 前先判可读，避免每秒刷 stderr；用法注释修正为 `sh -s -- --once` |
| `collector/parser.py` | 新增 `pick_primary_network()`；`_parse_net()` 解析 `state`、给每项加 `state`/`primary`、拒绝负计数、每帧恰好一个 `primary=True`；兼容旧 3 字段 NET |
| `server/store.py` | 历史 `net.rx_kbps`/`tx_kbps` 优先用帧内 `primary` 标记，旧帧回退 `pick_primary_network()` |
| `web/js/app.js` | 新增 `fmtTrafficKB()`；折线图改 `pickPrimaryNet()`；网络卡片逐行渲染全部网卡（主网卡置顶打「主」、`down` 标「未连接」置灰删除线、零流量置灰、按活跃/累计/名称排序）；`window.__xmmHandleBack()` 分层返回 |
| `web/js/targets.js` | 新增 `back()`：表单先退回列表，再关抽屉 |
| `web/css/main.css` | `.netlist` 限高 260px（手机 220px）+ 内部滚动；新增 `.net-state`、`.net-down`、`.net-zero` |
| `collector/mock.py` | 模拟数据 3 → 4 张网卡：`eth0`/`eth1`/`br-lan`(主)/`tunl0`(零流量) |
| `collector/ssh_client.py` | 新增模块常量 `ONESHOT_CMD = "sh -s -- --once"`，修掉一次性采集进死循环的遗留 bug |
| `MainActivity.java` | `applyEdgeToEdge()`、`applySystemBarInsets()`（状态栏/刘海/手势条/输入法）、WebView 深色底、`OnBackInvokedCallback` + `onBackPressed` 统一进 `handleBack()`，删除 `onKeyDown` 重复处理 |
| `AndroidManifest.xml` | Activity 加 `windowSoftInputMode="adjustResize"`、`windowLayoutInDisplayCutoutMode="shortEdges"` |
| `app/build.gradle` | `versionCode 8 → 9`（`versionName` 保持 `0.2.3`） |
| `packaging/build_all.bat` | 横幅 beta2.3；新增 `XMM_NO_PAUSE=1` 跳过末尾按键等待；文件保持 GBK + CRLF |
| `tests/test_beta23_regression.py` | **新增 7 例**：`--once` 常量与实际使用、真实 sh 传参、真实 collect.sh 只输出一行、>2^31 累计值、逐卡独立速率、负计数拒绝、旧 3 字段兼容 |
| `scripts/verify_multi_nic.py` | **新增**：可重复运行的多网卡 API 验收脚本（版本/网卡数/唯一 primary/非负累计/独立速率） |
| `build/ui_shot.mjs` | **新增**：Edge headless + CDP 的界面验收与截图（桌面 + 375px 手机） |
| 版本号全套 | `server/api.py`、`server/app.py`、`web/index.html`(2 处)、`run.py`、Tauri `tauri.conf.json`/`Cargo.toml`/`main.rs`、`build_all.bat` → beta2.3 / 0.2.3 |

---

## 三、主网卡口径（前后端统一，别再各算一套）

```
候选 = 所有接口（已排除 lo）
  → 去掉 state == "down" 的（若全 down 则用全集）
  → 优先真实接口：eth / en / wl / br- / lan / wan / bond / pppoe / ap 开头
  → 在候选中取 (rx_total_kb + tx_total_kb) 最大者 → primary=True
虚拟口（tunl0 / utun / docker0 / veth* / ip6tnl0 …）不参选，但仍完整显示在列表里。
```

后端 `parser.pick_primary_network()` 算一次，帧里写 `primary`；
前端大数字 / 折线图、`server/store.py` 历史曲线都读这个标记，
前端保留同口径兜底只为兼容旧后端 JSON。

---

## 四、安卓全面屏与侧滑返回

- **edge-to-edge**：透明状态栏/导航栏，API 30+ `setDecorFitsSystemWindows(false)` +
  浅色系统图标，低版本 `SYSTEM_UI_FLAG_LAYOUT_*` 兜底。
- **insets 让位**：`WindowInsets.Type.systemBars() | displayCutout() | ime()` 给根容器设 padding，
  顶部状态栏/刘海、底部手势条/输入法、横屏左右刘海都不遮挡内容。
- **分层返回**：API 33+ `OnBackInvokedCallback`，低版本 `onBackPressed()`，
  都调 `web.evaluateJavascript("window.__xmmHandleBack()")`：
  表单 → 列表 → 关抽屉 → 返回 `default` 才 `moveTaskToBack(true)` 收后台（监控继续）。
  删除了 `onKeyDown` 里重复的返回处理，避免一次侧滑触发两次。

---

## 五、验证证据（本机实测）

| 项 | 结果 |
|---|---|
| `pytest tests -q` | **61 passed**（基线 54 + 新增 7） |
| `sh -n collect.sh` / `sh -s -- --once` | 通过；输出一行即退出；旧写法 `$1` 为空已复现 |
| `node --check` 前端 6 个 JS / Python `compileall` | 全过 |
| mock（8899） | beta2.3、4 张网卡、`br-lan` 主、`tunl0` 置灰、无负累计 |
| 真实目标（8900 / 192.168.1.100） | **19 个接口全部返回**，`br-lan` 主，累计值全正，9 个有流量，逐卡速率正常 |
| 无头浏览器桌面视图 | 卡片头「br-lan（共 19 个接口 · 9 个有流量）」，19 行全部渲染 |
| 无头浏览器 375px | `clientHeight=220` / `scrollHeight=474`，列表内部滚动、页面不被撑爆 |
| PC 产物 | `dist\主机监控小助手_0.2.3_x64-setup.exe` |
| APK | `dist\主机监控小助手_手机版_0.2.3.apk`，`aapt dump badging` → `versionCode='10' versionName='0.2.3'`；解 `assets/chaquopy/app.imy` 确认内嵌 `web/index.html` 为 beta2.3、`collect.sh` 含 `operstate`、`ssh_client.pyc` 含 `ONESHOT_CMD` |

截图：`docs/shots/beta2.3-多网卡-*.png`（mock + 真实目标 × 桌面 + 手机，共 4 张）。

### 关于 qwen3:8b 辅助审查

按要求让本机 `qwen3:8b` 对 `collect.sh` / `parser.py` / 网络渲染片段做了一轮独立审查
（脚本 `build/qwen_review_beta23.py`，输出 `build/qwen_review_beta23.txt`）。
它给出 6 条「缺陷」，逐条核对后发现**全部与当前代码不符**——把已经修掉的
`%d` 溢出、负计数防御、down 过滤、operstate 缺失兼容又报了一遍。
**未采纳任何一条**，主判断以单元测试 + 真实目标 + 浏览器实测为准。

---

## 六、交付物

```
dist\主机监控小助手_0.2.3_x64-setup.exe        PC（NSIS，装完即用）
dist\主机监控小助手_手机版_0.2.3.apk            安卓（versionCode 10，可直接覆盖旧版）
```

旧 0.2.2 产物先保留在 `dist/`，确认新版可用后再清理。

---

## 七、遗留 / 下一步

- beta2.2 遗留：配置文件热重载未做（服务模式与桌面窗口需二选一运行）。
- `MainActivity.onBackPressed()` 为 deprecated API（编译提示，功能正常）。
- 打包坑（GBK+CRLF 的 .bat、沙箱批量删除保护、Rust PATH、versionCode 递增）已写入
  [`项目交接手册.md`](项目交接手册.md) 第 8 节。
- beta2.4 候选见 [`升级路线图.md`](升级路线图.md) 第四节。


---

## 八、补丁：安卓 17 打开闪退修复（versionCode 10）

用户真机（小米 24122RKC7C / HyperOS / Android 17，API 37）报告「安卓端打开闪退」。
`adb logcat` 抓到：

```text
Caused by: java.lang.NullPointerException: DecorView.getWindowInsetsController() on a null object
  at com.android.internal.policy.PhoneWindow.getInsetsController(PhoneWindow.java:4235)
  at com.xiaozhushou.monitor.MainActivity.applyEdgeToEdge(MainActivity.java:82)
```

根因是 `applyEdgeToEdge()` 在 `setContentView()` 之前调用 `getWindow().getInsetsController()`，
Android 17 的 `PhoneWindow` 在 DecorView 未创建时直接解引用 `mDecor` 抛 NPE（旧版返回 null，
被判空逻辑挡住了）。

修复：
- `MainActivity.java`：`setContentView()` 提前；改用 `getDecorView().getWindowInsetsController()`；
  系统栏适配与返回手势注册整体 `try/catch (Throwable)` 降级；新增 `TAG` 日志。
- 新增 `BackInvokedCompat.java`：隔离 API 33 返回手势类，保证 Android 12/13 不会因类验证失败闪退。
- `app/build.gradle`：`versionCode 9 → 10`（`versionName` 仍 `0.2.3`），保证已装 9 的机器能覆盖升级。
- 新增 `build/adb_diagnose.sh`：一键抓机型 / 版本 / 覆盖安装 / 冷启动 / 崩溃栈。

真机复测：进程存活、栈顶为 `MainActivity`、logcat 无本 App FATAL、`/api/ping` 返回 beta2.3、
首页正常渲染（状态栏 / 手势条不遮挡）、返回键后进程仍在。截图 `docs/shots/android17-fixed.png`。
PC 端不受影响，无需重新打包。
