# beta2.1 变更说明

> 基线：beta2.0（Docker 负载 + 桌面/手机双端）。
> 本次升级的目标：**监控 Linux 风扇转速——OpenWrt 和常规 Linux 都能看，依旧纯 SSH 只读。**

---

## 一、这次到底改了什么

### 1. 风扇卡片（核心）

温度卡片下方新增 **风扇** 卡片：

- 每个风扇一行：名称 + 实时转速（RPM）；大字显示转速最高的一把
- `rpm = 0` 是**有效读数**（空置插针 / DC 调速停转），显示为灰色"停转"，不当成故障
- 没有风扇探头的机器显示"无风扇"，并注明"未检测到风扇转速探头（被动散热或未加载 hwmon 驱动）"

### 2. 采集实现（OpenWrt / 常规 Linux 通用）

- 数据源：`/sys/class/hwmon/hwmon*/fan*_input` —— 内核 hwmon 接口，
  **不依赖 lm-sensors 的 `sensors` 命令**，busybox ash 原生可用（OpenWrt 友好）
- 命名：优先 `fanN_label`（驱动提供的名字，如 `CPU Fan`），缺省用 `芯片名:fanN`
  （如 `it8620:fan1`），与 `sensors -u` 的命名习惯一致
- 没有风扇的机器完全不输出 FAN 段（整段静默省略），其余采集零影响
- 行协议新增：`FAN=<名称>,<RPM>;...`（段顺序：TMP 之后、OS 之前）

### 3. 数据链路

```
collect.sh   FAN=it8620:fan1,1033;it8620:fan2,0;...
    ↓ sh -s 长连接（复用原通道）
parser.py    frame.fans = [{name, rpm}, ...]
    ↓
server/app.py  metrics 广播带 fans（字段很小，历史缓冲照常保留）
    ↓
web/js/app.js  renderAll() 风扇分支：数量、最高转速、逐行列表、停转标注
```

安全边界没变：只读 `/sys/class/hwmon`，**不会调节 PWM/风扇转速**。

---

## 二、代码层面

```
修改
  collector/remote/collect.sh   新增 fan_info()：hwmon fan*_input 采集 + FAN 段
  collector/parser.py           frame 默认 fans=[]；新增 _parse_fans
  collector/mock.py             模拟数据带 2 把风扇（含转速摆动）
  web/index.html                新增风扇卡片骨架；版本徽标 → beta2.1
  web/js/app.js                 风扇渲染分支 + 卡片显隐
  server/app.py / server/api.py / run.py   版本 → beta2.1
  shell/tauri                   0.2.1；窗口标题/托盘 → beta2.1
  packaging/build_all.bat       横幅 → beta2.1
新增
  docs/beta2.1-变更说明.md       本文件
  tests/test_parser.py          风扇 3 个用例
```

---

## 三、测试与验证

- `pytest tests`：**36/36 通过**（新增风扇解析 3 例：正常+停转、缺段+坏条目容错、负值钳 0）
- 采集脚本 `sh -n` 语法检查通过（WSL busybox 兼容）
- **真实 OpenWrt（iStoreOS 24.10）实测**：目标机带 IT8620 Super I/O 芯片、5 个风扇插针，
  采集输出 `FAN=it8620:fan1,1033;fan2..5,0`，界面显示 5 行（fan1≈1035 RPM，
  其余"停转"），Docker 卡片不受影响
- mock 模式验证快照/历史链路

---

## 四、手机端补丁与新版 logo（beta2.1 随附）

手机 APK 同步升到 **0.2.1（versionCode 3）**，内嵌代码跟随到 beta2.1（手机也能看风扇卡片），
并修复用户实测反馈的两个 bug、更换启动图标：

| 问题 | 原因 | 修复 |
|---|---|---|
| 顶栏/表单和系统状态栏叠在一起 | targetSdk 35 在 Android 15+ 强制 edge-to-edge，WebView 画到了状态栏底下 | MainActivity 按系统栏/刘海 inset 给根容器加顶部内边距（API 30+ 用 `WindowInsets.Type`，老系统自然为 0 不受影响）；状态栏区域底色改为与主题一致的 `#0d1117` |
| "私钥路径"占位符是 Windows 路径 `C:/Users/you/.ssh/...` | 同一套网页两端共用，占位符写死了桌面习惯 | `targets.js` 按 UA 区分平台：安卓显示"手机版建议用密码登录"，表单提示文案同步调整 |
| 旧 logo（卫星天线 emoji 图标） | 换用用户提供的 `logo.png`（机器人+服务器栈徽标） | `build/gen_app_icons.py` 生成全套图标：自适应图标（前景徽标 62% 安全区 + 纯黑背景，圆形裁剪不切内容）+ 传统 density PNG；背景色改 `#000000` |

模拟器复测截图见 `docs/shots/手机版-风扇卡片.png`、`手机版-私钥提示修复.png`：
顶栏与状态栏完全分离、私钥提示正确、新图标在应用抽屉正常显示、风扇卡片实采 1032 RPM。

### 手机端实测补充：WebView 缓存旧页面（versionCode 4）

用户真机升级后反馈私钥提示仍是旧文案，而状态栏修复已生效——排查结论：**原生代码生效但
WebView 沿用了旧 JS 的启发式缓存**（此前静态资源响应不带 Cache-Control，升级换包后
WebView 不会主动重新校验）。修复：后端对网页静态资源统一加 `Cache-Control: no-cache`
（`server/app.py` 中间件，API/WS 不受影响），每次升级后强制按 etag 重新校验，文件没变走 304。
桌面浏览器同样受益。已验证：源码与冻结 exe 响应头均带 `no-cache`，36/36 测试通过；
手机端 versionCode 升至 4。

### 手机端实测补充：全面屏侧滑返回（versionCode 5）

用户反馈全面屏手势的左右侧滑无法"返回上一级"——此前返回事件被壳统一处理成"收到后台"。
修复：MainActivity 的返回（侧滑/三键）先调用页面暴露的 `window.__xmmHandleBack()`——
服务器面板、设置面板开着时先关掉（等于返回上一级），关无可关（返回 default）才收到后台；
面板内的删除确认、编辑表单随面板一起关闭。前端逻辑已实测：面板开→closed+面板隐藏、
无面板→default、设置面板→closed+隐藏。手机端 versionCode 升至 5。
