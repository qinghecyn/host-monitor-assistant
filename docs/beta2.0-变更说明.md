# beta2.0 变更说明

> 基线：beta0.1（服务器可视化 + 桌面安装包）。
> 本次升级的目标：**加入 Docker 负载查看——不装任何东西到被监控机，
> 还是那条 SSH 只读通道，把每个容器的状态和资源占用搬进界面**。

---

## 一、这次到底改了什么

### 1. Docker 负载卡片（核心）

主界面在磁盘卡片下面新增 **Docker 负载** 卡片，每个容器一行：

| 列 | 内容 |
|---|---|
| 容器 | 容器名 + 镜像名（如 `nginx:1.27`） |
| 状态 | 彩色徽标：绿"运行中" / 灰红"已退出（含原因）" / 黄"暂停·重启中" |
| CPU | `docker stats` 口径的百分比（多核机器可 >100%） |
| 内存 | 占比条 + `已用 / 上限（占比）`，≥60% 变黄、≥80% 变红 |
| 网络 IO / 块 IO | 容器生命周期的累计值（docker 原生格式） |

右上角汇总"**运行 3 · 共 5 个容器**"；已停止的容器排在后面并显示变暗。

### 2. 三种 Docker 状态都不含糊

| 界面显示 | 含义 |
|---|---|
| "这台机器上没有安装 Docker" | 目标机没有 `docker` 命令（行协议 `DKS=none`） |
| "Docker 守护进程没有在运行" | 命令在但 daemon 没起（`DKS=down`），提示去启动 docker 服务 |
| "Docker 正常，还没有创建任何容器" | daemon 正常、容器数为 0 |
| "此主机未上报 Docker 信息" | 后端还是旧版（beta0.1 前端兼容场景） |

### 3. 采集策略：快与省

- `docker stats --no-stream` 采样一次要 1~2 秒（容器多更久），**不能每秒都跑**。
  采集脚本里按时间缓存：容器列表每 **5s**、资源占用每 **30s** 刷新一次，
  其余帧直接复用缓存值（`collector/remote/collect.sh`）。
- 刷新那一帧会稍慢，但 CPU/网速速率按**帧内时间戳**差值折算，数据不会失真。
- `timeout 15` 兜底：daemon 卡死时最多等 15 秒，之后继续用旧缓存，采集循环不挂。
- 容器名/镜像里的 `;`、`|` 被清洗成空格；`docker ps` 末列（状态详情）里的逗号
  由解析端"末字段吞掉"，不会导致字段错位。容器 ID 截到 12 位作为 ps↔stats 合并键。
- **没装 docker 的机器**：完全不输出 DK* 段，其余采集项零影响。

### 4. 数据链路

```
collect.sh   DKS=ok,3,4|DKP=<id>,<名称>,<镜像>,<state>,<详情>;...|DKST=<id>,<cpu%>,...;...
    ↓ sh -s 长连接（复用原通道，不加第二条 SSH）
parser.py    frame.docker = {daemon, running, total, containers:[{...合并 stats}]}
    ↓
server/app.py  metrics 广播带 docker；历史缓冲剔除 docker（体积大且不画曲线）
    ↓
web/js/app.js  renderDocker()：按 daemon 状态分流；运行中优先排序；CPU/内存/IO 渲染
```

安全边界没变：docker 查询全部是只读命令（`docker ps -a` / `docker stats --no-stream`），
**不能也不会**去启动/停止/重启容器；凭据依旧不出后端。

---

## 二、代码层面

```
修改
  collector/remote/collect.sh   新增 docker_tick/docker_emit：时间缓存刷新 + DKS/DKP/DKST 三段
  collector/parser.py           新增 _parse_docker_state/_parse_docker_ps/_parse_docker_stats/_merge_docker
  collector/mock.py             模拟数据带 3 运行 + 1 已停止容器（无 SSH 也能演示）
  server/app.py                 on_frame 历史剔除 docker；快照 version → beta2.0
  server/api.py                 VERSION → beta2.0
  web/index.html                新增 Docker 卡片骨架；版本徽标 → beta2.0
  web/js/app.js                 renderDocker() + 卡片显隐控制
  web/css/main.css              Docker 表格样式 + 手机竖屏布局
  shell/tauri/src-tauri         版本 0.2.0；窗口标题/托盘 → beta2.0
  packaging/build_all.bat       横幅 → beta2.0
新增
  docs/beta2.0-变更说明.md       本文件
  tests/test_parser.py          Docker 段 5 个用例（见下）
```

---

## 三、测试与验证

- `tests/test_parser.py` 新增：
  - `test_docker_full_frame`：DKS/DKP/DKST 合并、stats 字段解析（% 去除、内存已用/上限拆分）
  - `test_docker_none_and_down`：未安装 / 守护进程未启动 / 旧脚本（无 DK 段 → docker=null）
  - `test_docker_corrupted_segment`：DK 段损坏不影响整机帧
- 采集脚本用假 docker 在 WSL 里做了 5 组行为测试：有容器 / 未安装 / daemon 挂 /
  特殊字符 / 缓存刷新频率（3 帧 stats 只跑 1 次）。
- 真机验证：目标 iStoreOS（装有 Docker）全链路出数，见 README「验证结论」。

---

## 四、手机版（beta2.0 附带交付）

本次同时交付**独立安卓 App**：`dist\主机监控小助手_手机版_0.2.0.apk`。

### 为什么不是"APK 连电脑后端"

旧版安卓壳（Capacitor）只是个浏览器壳，必须电脑开机才能看。用户要求"后端也打包进去、不要分开"，
而桌面版打进去的后端是 PyInstaller 产物——**PyInstaller 无法产出安卓程序**，安卓也不允许 APK
以桌面那种方式拉起子进程。因此手机版采用 **Chaquopy**：把**原来那套 Python 后端
（server/ + collector/ + web/）原封不动打进 APK**，在手机上跑真的 Python 3.13。

### 实现要点

```
shell/chaquopy-app/            新增的独立 Gradle 工程（未改动任何现有源码文件）
  app/build.gradle             chaquopy 17.2 + AGP 8.7；server/ collector/ web/ 以
                               srcDir + include 白名单只读引用原项目
  app/src/main/python/
    android_main.py            新增的启动入口（唯一的新 Python 代码）：
                               127.0.0.1:8787 起 uvicorn；设 XMM_DESKTOP_TOKEN；
                               给 index.html 注入 __XMM_BOOTSTRAP__（与 Tauri 壳同机制）
  MainActivity.java            WebView + 等 /api/ping 就绪再加载；返回键=后台
  monitor.keystore             沿用旧版签名（同 applicationId，可直接覆盖升级）
```

依赖处理：Chaquopy 轮子仓库没有 pydantic-core（Rust），所以锁 `fastapi<0.116 +
pydantic v1（纯 Python）`；cryptography / cffi / bcrypt 用 Chaquopy 官方安卓轮子；
asyncssh 解析到 2.23.1（兼容 cryptography 42）。

### 验证结论（模拟器全链路实测）

- APK 内容核验：Python 3.13 运行时、后端源码、collect.sh、web 页面、全部 pip 依赖在包内
- Windows 先用 APK 同款依赖版本预演 android_main 流程：注入/免登录/加目标/真实 SSH 全通
- Android 模拟器（API 35 arm64 转 x86_64）：安装 → 启动 → 免登录进主界面 →
  通过界面 API 添加 iStoreOS → **手机直连真实 SSH**，CPU/内存/磁盘/温度全部出数，
  Docker 卡片显示 3 个容器（2 运行 1 退出），截图见 `docs/shots/手机版-*.png`

### 已知边界

- 监控只在 App 存活期间运行（未做前台服务常驻；被系统杀掉后重开 App 自动恢复）
- 手机版后端只监听 127.0.0.1（App 内 WebView 访问），不向局域网暴露
- 配置存在手机私有目录，与电脑版各自独立
