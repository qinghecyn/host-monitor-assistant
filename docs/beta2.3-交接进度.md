# beta2.3 多网卡开发 · 交接进度（2026-11-06）

> ✅ **2026-11-06 更新：beta2.3 已完整交付，本文件转为「开发过程 + 验收证据」历史记录。**
> **新会话请优先读 [`项目交接手册.md`](项目交接手册.md)**（项目怎么做 / 打包 / 验收 / 踩坑清单），
> 改动逐文件说明看 [`beta2.3-变更说明.md`](beta2.3-变更说明.md)。
> 双端 0.2.3 产物已在 `dist/`；61 个测试全过；真实目标 19 张网卡实测可见。
> 下文第五节原本是「待办清单」，现已全部完成，保留作为可复现的操作步骤。

---

> **补丁 2026-11-06 11:45**：用户真机（Android 17）报告「打开闪退」，
> 已用 adb 定位为 `PhoneWindow.getInsetsController()` NPE 并修复，
> 重打包为 `versionCode 10`、真机复测通过。详见 [`项目交接手册.md`](项目交接手册.md) 第 4 节
> 与 [`beta2.3-变更说明.md`](beta2.3-变更说明.md) 第八节。本文件其余内容记录的是首轮交付。

## 一、用户这次的核心需求

1. **主机是多网卡的，但旧版只显示一个 `eth0`**；必须显示全部/多张网卡。
2. 版本号升级到 **beta2.3（v0.2.3）**，安卓端、PC 端都要重新打包。
3. 安卓端适配全面屏：左右侧滑返回上一级、状态栏/刘海/手势条/输入法不挡内容。
4. 把“项目怎么做的”写成文件，换窗口/换 agent 后不至于断片。
5. 完成验收、重点排查 bug。用户本机有 `qwen3:8b`，可作辅助，但主工作必须由主 agent 完成。

---

## 二、已经定位到的两个关键根因

### 根因 1：旧前端只渲染 `network[0]`

旧版 `web/js/app.js` 网络卡片取的是：

```js
const net = f.network && f.network[0];
```

而 `/proc/net/dev` 的第一张网卡通常就是 `eth0`，所以 `eth1`、`br-lan`、`utun`、`docker0` 等即使采集到了也永远不显示。

**已修：** 前端现在逐行渲染 `network` 全数组；大数字/折线图只代表“主网卡”，下方列表展示全部接口。

### 根因 2：busybox awk 的 `%d` 32 位溢出（致命）

真实目标 `192.168.1.100`（iStoreOS 24.10.8）上，累计字节超过 `2^31` 后，采集脚本里的：

```sh
printf "%d" <累计字节>
```

在 busybox awk 里会变成负数。实测修复前：

```text
eth0  total=1158872.6 / -2097152.0 KB
eth1  total=-2097152.0 / 1139616.6 KB
br-lan total=-2097152.0 / -2097152.0 KB
```

这会导致累计流量错误、差值速率变 0/负、主网卡判断错误，实打实是“多网卡显示不对”的致命问题。

**已修：** `collect.sh` 里所有累计型数值统一改成 `%.0f`（CPU jiffies、内存、磁盘、网络、uptime、Docker 计数），并增加负计数防御。

修复后同一目标实测：

```text
eth0   up      total= 1161673.7 / 8036882.4 KB
eth1   up      total=10225052.7 / 1143539.6 KB
br-lan up      total=10275285.6 / 8459493.3 KB
utun   unknown total=10040863.4 / 9521601.0 KB
primary: br-lan
negative totals: []
```

### 附带发现的 bug（✅ 已修复并补回归测试）

`collector/ssh_client.py` 的一次性采集模式写的是：

```python
proc = await conn.create_process("sh -s --once")
```

实测 `sh -s --once` 在目标机上 **`$1` 是空字符串**，`--once` 被 sh 当成了自己的选项，所以远端脚本进入无限循环；现在只是因为调用方读一行就关闭进程才“看起来能用”。

**正确写法：**

```python
proc = await conn.create_process("sh -s -- --once")
```

✅ 已在 `collector/ssh_client.py` 修掉（模块常量 `ONESHOT_CMD = "sh -s -- --once"`），
并在 `tests/test_beta23_regression.py` 补了 3 组回归测试（常量、真实 sh 传参、真实脚本只输出一行）。

---

## 三、已经改过的文件（按文件）

### 1. `collector/remote/collect.sh`

- `NET=` 段从：

  ```text
  NET=<网卡>,<rx累计字节>,<tx累计字节>;...
  ```

  改为：

  ```text
  NET=<网卡>,<rx累计字节>,<tx累计字节>,<operstate>;...
  ```

- 仍然排除 `lo`，但输出 `/proc/net/dev` 中的**全部其他接口**。
- `operstate` 从 `/sys/class/net/<iface>/operstate` 读取；读不到留空。
- **全部 `%d` 累计值改成 `%.0f`**，防止 busybox awk 32 位溢出。
- `fan_info()` 读取 `fan*_label` 前先 `[ -r "$lf" ]`，避免每秒往 SSH stderr 刷
  `can't open /sys/class/hwmon/.../fan1_label`。
- 这个文件是“多网卡 + 大流量计数”修复的核心。

### 2. `collector/parser.py`

- 新增 `import re`。
- 新增 `_REAL_IFACE_RE` 和 `pick_primary_network(nets)`：
  - 优先 `state != down`；
  - 再优先 `eth/en/wl/br-/lan/wan/bond/pppoe/ap` 开头的真实接口；
  - 再按 `rx_total_kb + tx_total_kb` 取最大。
- `_parse_net()`：
  - 解析第 4 个字段 `state`；
  - 每项新增 `state`、`primary`；
  - 拒绝负计数（旧脚本溢出/坏数据防御）；
  - 每帧只给一个接口标 `primary=True`。
- 主网卡口径现在由后端统一，`server/store.py` 与前端共用，避免曲线和界面各算一套。

### 3. `server/store.py`

- 引入 `from collector.parser import pick_primary_network`。
- 历史指标 `net.rx_kbps` / `net.tx_kbps` 改为：
  - 优先使用帧里带 `primary=True` 的接口；
  - 旧帧没有 `primary` 时回退 `pick_primary_network()`。
- 不再简单取 `network[0]` 或全数组最大，避免多网卡历史画错接口。

### 4. `web/js/app.js`

- 新增 `fmtTrafficKB()`，用于每个网卡行的 `title` 累计流量提示。
- `pushCharts()` 的折线图改为 `pickPrimaryNet(f.network)`，不再取 `f.network[0]`。
- 网络卡片：
  - 头部显示 `主网卡（共 N 个接口 · M 个有流量）`；
  - 列表渲染**全部网卡**，主网卡置顶打“主”标；
  - 活跃接口优先，再按累计流量、名称排序；
  - `state=down` 显示“未连接”并置灰/删除线；
  - 全零流量接口置灰；
  - 列表有最大高度，内部滚动，避免 19 张网卡把页面撑爆。
- `pickPrimaryNet()`：
  - 优先读后端 `n.primary`；
  - 兜底按 up 状态 + 真实接口前缀 + 累计流量；
  - 兼容旧后端没有 `primary` 字段的 JSON。
- `window.__xmmHandleBack`：
  - 服务器抽屉 → 调用 `manager.back()`（表单先退回列表，再关抽屉）；
  - 设置面板 / 拨测 / 历史 → 逐层关闭；
  - 拨测表单打开时先关表单，不直接关整个抽屉。

### 5. `web/js/targets.js`

- 新增 `back()`：
  - 抽屉没开返回 `false`；
  - 表单开着先 `hideForm()`；
  - 否则 `close()`。
- 返回值新增 `back`，供安卓返回手势调用。

### 6. `web/css/main.css`

- `.netlist` 增加 `max-height: 260px`、纵向滚动、细滚动条。
- 新增 `.net-state`（“未连接”标签）。
- 新增 `.netlist li.net-down`（置灰 + 速率删除线）。
- 手机媒体查询下 `.netlist { max-height: 220px; }`。

### 7. `collector/mock.py`

- 模拟数据从 3 张网卡扩展为 4 张：
  - `eth0`（up，非主）
  - `eth1`（up，非主）
  - `br-lan`（up，**primary=True**）
  - `tunl0`（unknown，0 流量）
- 便于 `--mock` 模式直接验收多网卡 UI。

### 8. `shell/chaquopy-app/app/src/main/java/com/xiaozhushou/monitor/MainActivity.java`

- 新增 `applyEdgeToEdge()`：
  - 透明状态栏/导航栏；
  - API 29+ 关闭系统栏对比度遮罩；
  - API 30+ `setDecorFitsSystemWindows(false)`，状态栏/导航栏用浅色图标；
  - API <30 用 `SYSTEM_UI_FLAG_LAYOUT_*` 兜底。
- `applySystemBarInsets()`：
  - 顶部（状态栏/刘海）、底部（导航栏/手势条/输入法）、左右（横屏刘海）都设置 padding；
  - API 30+ 使用 `WindowInsets.Type.systemBars() | displayCutout() | ime()`；
  - 避免全面屏内容被系统栏盖住。
- WebView：
  - 深色背景 `#0D1117`，防白屏闪；
  - `setUseWideViewPort(true)`、关闭缩放。
- 返回手势：
  - API 33+ 保留 `OnBackInvokedCallback`；
  - API <33 用 `onBackPressed`；
  - **移除了 `onKeyDown` 里重复的返回处理**，避免一次侧滑触发两次（先关面板又立刻退后台）。
- 未改的旧逻辑：返回键最终 `moveTaskToBack(true)`，App 退后台但监控继续。

### 9. `shell/chaquopy-app/app/src/main/AndroidManifest.xml`

- Activity 增加：
  - `android:windowSoftInputMode="adjustResize"`
  - `android:windowLayoutInDisplayCutoutMode="shortEdges"`
- 原有 `android:enableOnBackInvokedCallback="true"` 保留。

### 10. 测试文件

- `tests/test_parser.py` 新增：
  - `test_multi_nic_all_interfaces_and_operstate`
  - `test_multi_nic_rates_for_every_interface`
- `tests/test_store.py` 新增：
  - `test_frame_metrics_prefers_backend_primary_flag`
- 这些测试覆盖：全部接口保留、`state` 解析、down 接口不抢主网卡、隧道口不参选、每张卡独立速率、历史主网卡优先 `primary` 标记。

---

## 四、已经做过的验证（可作为证据）

### 1. 改动前基线

```text
.venv\Scripts\python -m pytest tests -q
51 passed, 1 warning in 34.18s
```

### 2. 改动后局部测试

```text
.venv\Scripts\python -m pytest tests/test_parser.py tests/test_store.py -q
21 passed in 0.13s
```

> ✅ 2026-11-06 已跑全量：**61 passed**（原 54 + 新增 7 例 beta2.3 回归）。

### 3. WSL 脚本语法/真实执行

```text
wsl -d Ubuntu -- sh -n /mnt/d/xiaozhushou_monitor/collector/remote/collect.sh
SYNTAX_OK
NET=eth0,29960,6463,up
```

### 4. 真实目标机（iStoreOS 24.10.8，192.168.1.100）实测

用正确的一次性调用方式：

```text
sh -s -- --once
```

结果：

- 解析出 **19 个接口**（`/proc/net/dev` 去掉 lo 后的全部接口）；
- 主网卡自动选为 `br-lan`（累计流量最大且 up）；
- `eth0/eth1/br-lan/utun/docker0/veth*/pve-ext/dheth0` 等全部在列表中；
- 修复后**无负累计值**；
- 速率正常，例如：
  - `eth0 rx=151.4 kbps tx=4160.6 kbps`
  - `eth1 rx=4302.9 kbps tx=305.4 kbps`
  - `br-lan rx=4283.7 kbps tx=4459.7 kbps`
  - `utun rx=4184.3 kbps tx=4184.7 kbps`
- 说明“只显示 eth0”的根因已从采集、解析、前端三处一起修掉。

### 5. `--once` 参数问题的实测证据

```text
sh -s --once      → $1 为空，脚本进入无限循环
sh -s -- --once   → $1 为 --once，脚本执行一次后退出
```

---

## 五、验收与打包（原待办清单，2026-11-06 已全部完成，以下保留为可复现步骤）

> **完成结果速览**：全量 pytest `61 passed`；`sh -n` 通过；`sh -s -- --once` 修好并补回归；
> `--mock` 4 张网卡 / 真实目标 19 张网卡均通过 API 与无头浏览器验收（含 375px 手机视图滚动）；
> PC `0.2.3` NSIS 安装包 + APK `0.2.3`（versionCode 10）已放入 `dist/` 并验证内嵌资源；
> README / 使用手册 / 开发文档 / 协议文档 / 升级路线图 / AGENTS / beta2.3 变更说明 /
> 项目交接手册全部同步。详细证据见 [`beta2.3-变更说明.md`](beta2.3-变更说明.md) 第五节。

### A. 先跑全量测试

```powershell
cd D:\xiaozhushou_monitor
.\.venv\Scripts\python -m pytest tests -q
```

✅ 实际 **61 passed**（原 51 → 加了 3 例多网卡/store → 54，再加 7 例 `test_beta23_regression.py`）。

### B. 修复 `--once` 的遗留 bug（✅ 已完成）

文件：`collector/ssh_client.py`

```python
# 现在（有 bug）
proc = await conn.create_process("sh -s --once")
# 改成
proc = await conn.create_process("sh -s -- --once")
```

顺手检查 README/docs 里是否写了 `sh -s --once`，统一改成 `sh -s -- --once`。

### C. 源码模式 UI 验收（多网卡）

```powershell
cd D:\xiaozhushou_monitor
.\.venv\Scripts\python run.py --mock --port 8899
```

浏览器打开 `http://127.0.0.1:8899`，确认：

- 网络卡片下方出现 **4 行**：`br-lan（主）`、`eth0`、`eth1`、`tunl0`；
- 主网卡大数字/曲线来自 `br-lan`；
- `tunl0` 置灰；
- 手机窄屏（DevTools 375px）下列表可滚动、不撑爆页面。

> 注意：本机 8787 端口上还跑着**旧安装版**的 `xiaozhushou-server.exe`，不要被它返回的旧数据误导；源码验收请用 `--port 8899`。

### D. 真实目标 API 验收

可以临时用源码服务 + 真实 `config.toml` 开在 8899，登录后看 `/api/snapshot`：

```powershell
.\.venv\Scripts\python run.py --config config.toml --data-dir .\build\tmp-data --port 8899
```

确认快照里 `primary` 目标返回的 `network` 数组长度 > 1，且 `primary=True` 的接口是 `br-lan`，所有累计值为正。

### E. PC 打包（✅ 已完成）

```bat
packaging\build_all.bat
```

注意事项：

- **Rust 工具链不在 PATH**，在 `%USERPROFILE%\.cargo\bin`，打包前需要：
  ```bat
  set PATH=%USERPROFILE%\.cargo\bin;%PATH%
  ```
- `shell/tauri` 当前没有 `node_modules`，`build_all.bat` 用 `npx @tauri-apps/cli@2 build`，需要网络。
- `packaging/xmm_server.spec` 会打包 `web/`、`collector/remote/collect.sh`、`config.example.toml`、`packaging/winsw/xiaozhushou-service.exe`。
- 产物在：
  ```text
  shell\tauri\src-tauri\target\release\bundle\nsis\主机监控小助手_0.2.3_x64-setup.exe
  ```
- 复制到 `dist\主机监控小助手_0.2.3_x64-setup.exe`；`dist` 里旧的 `0.2.2` 先别删，确认新版可用后再清理。
- 打包后建议用 `--mock` 启动冻结后端，确认 `/api/ping`、静态页、版本号、4 张网卡。

### F. 安卓打包（✅ 已完成）

环境已确认：

- JDK：`C:\Program Files\Eclipse Adoptium\jdk-21.0.13.9-hotspot`
- Android SDK：`D:\Android\Sdk`（build-tools 34/35/36 都在）
- buildPython：`D:\xiaozhushou_monitor\build\py313\python.exe`（存在）
- gradle wrapper：`shell\chaquopy-app\gradlew.bat`

命令：

```bat
cd shell\chaquopy-app
set JAVA_HOME=C:\Program Files\Eclipse Adoptium\jdk-21.0.13.9-hotspot
gradlew.bat assembleRelease
```

产物：

```text
shell\chaquopy-app\app\build\outputs\apk\release\app-release.apk
```

复制到：

```text
dist\主机监控小助手_手机版_0.2.3.apk
```

**打包前务必改 `shell/chaquopy-app/app/build.gradle`：**

```gradle
versionCode 10          // beta2.2 是 8，beta2.3 应递增，否则覆盖安装可能不触发升级
versionName '0.2.3'    // 已经是 0.2.3
```

打包后验证：

- `aapt dump badging app-release.apk` 看 `versionName='0.2.3'`、`versionCode='10'`；
- 解包或运行时确认内嵌 `web/index.html` 是 beta2.3；
- 手机/模拟器实测：左右侧滑先关抽屉/表单，再侧滑才退后台；状态栏/刘海/底部手势条不挡内容。

### G. 文档同步（✅ 已完成）

需要更新/新增：

- `README.md`：标题和当前版本改 beta2.3；新增“多网卡”说明；打包产物名改 0.2.3；验证结论更新。
- `使用手册.md`：网络卡片多网卡列表、安卓侧滑返回/全面屏说明。
- `docs/开发文档.md`：当前版本、NET 协议增加 operstate、多网卡主网卡口径、Android edge-to-edge 说明。
- `docs/协议与数据格式.md`：`NET=<网卡>,<rx>,<tx>,<operstate>`；frame JSON 增加 `state` / `primary`。
- 新增 `docs/beta2.3-变更说明.md`：写清“旧版只看 network[0]”和“busybox awk %d 溢出”两个根因。
- `docs/升级路线图.md`：把 beta2.3 标为已交付。
- `AGENTS.md`：当前版本、版本历史、打包产物名、遗留问题同步。

---

## 六、环境与注意事项（避免踩坑）

1. **项目不是 git 仓库**，没有 `git diff`/回滚；改文件前先备份或确认内容。
2. **本机 8787 有旧版在跑**：
   - `C:\Users\<你的用户名>\AppData\Local\主机监控小助手\xiaozhushou-monitor.exe`
   - `...\server-dist\xiaozhushou-server.exe`
   源码调试请用 `--port 8899`，不要直接杀用户正在跑的旧版，除非用户同意。
3. **真实目标**：`config.toml` 里的 `targets.primary = 192.168.1.100`，用户名/密码在文件里，注意不要把密码写进文档/日志/提交。
4. **WSL Ubuntu 是可用的**，可用来跑 `sh -n` 和 collect.sh 冒烟；WSL 只有 `eth0`，多网卡真实验证还是要靠 192.168.1.100。
5. ✅ **ollama qwen3:8b 可用**：本次用它做了一轮辅助 review（`build/qwen_review_beta23.py`），给出的 6 条「缺陷」经逐条核对全是误报，未采纳；主判断始终由主 agent 按单元测试 + 真实目标 + 浏览器实测下结论。
6. ✅ `sh -s --once` 已改为 `ONESHOT_CMD = "sh -s -- --once"` 并补回归测试；一次性测试/文档统一用 `sh -s -- --once`。
7. `collect.sh` 中累计值一定用 `%.0f`，不要改回 `%d`；这是本次最关键的防回绕修复。
8. ✅ 版本号全套、文档、Android versionCode(9)、双端打包产物均已完成同步。
9. ✅ `dist/` 已放入两个 `0.2.3` 产物；旧的 `0.2.2` 产物作为回退保留，不要拿它判断功能。`dist/xiaozhushou-server` 仍是旧产物。
10. ✅ 上述构建产物目录已由本次重新打包刷新（PC PyInstaller + Tauri、安卓 Chaquopy），仍不要手改生成目录。

---

## 七、建议的最终验收清单

- [x] 全量 pytest 通过。
- [x] `sh -n` 通过；真实目标 19 个接口全部解析，累计值无负数。
- [x] `--mock` 页面显示 4 张网卡，主网卡 `br-lan`，`tunl0` 置灰。
- [x] 真实目标页面显示 `eth0/eth1/br-lan/utun/docker0/...`，不再只有 `eth0`。
- [x] 手机窄屏网络列表不撑爆、可滚动。
- [x] 安卓侧滑：抽屉 → 表单 → 再侧滑退后台；状态栏/手势条/输入法不遮挡。
- [x] PC 安装包 `0.2.3` 生成、安装、启动、`--mock`/真实目标出数。
- [x] APK `0.2.3`、`versionCode 10` 生成，可覆盖旧版安装。
- [x] README/使用手册/开发文档/协议文档/beta2.3 变更说明/AGENTS 全部同步。
- [x] `dist/` 放入新产物；确认无误后再处理旧 0.2.2。

---

## 八、当前一句话状态

**beta2.3 已完整交付。** 多网卡（真实目标 19 个接口全部可见）、主网卡口径统一、32 位溢出修复、`--once` 修复、安卓全面屏 / 分层侧滑返回，以及全量测试（61 passed）、文档同步、双端打包、最终验收全部完成。新会话从 [`项目交接手册.md`](项目交接手册.md) 第 0 节开始即可。
