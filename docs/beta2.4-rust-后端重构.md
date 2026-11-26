# Rust 后端重构说明（客户端资源占用优化）

> 目标：降低桌面客户端的内存与磁盘占用。做法是把 Python 后端（FastAPI + asyncssh，
> 经 PyInstaller 冻结成 124MB 的 sidecar）整体移植为 Rust，并与 Tauri 壳合并为同一进程。
>
> **对外契约不变**：HTTP API、WebSocket 消息、JSON 帧结构、`config.toml` 格式与
> `web/` 前端全部零改动；`collector/remote/collect.sh` 仍是唯一远端采集脚本。

---

## 一、为什么要改

实测（Windows 11，真实 SSH 目标 192.168.1.100，同一台机器）：

| 项目 | 旧版（beta2.3，Python sidecar） | 新版（Rust release） |
|---|---|---|
| 后端进程工作集 | 43.6 – 74.6 MB（随负载波动） | **25.4 MB** |
| 后端进程私有内存 | **58.1 MB** | **8.6 MB** |
| 后端可执行文件 | 43 MB 目录（含 `_internal` 共 **124 MB**） | **7.0 MB 单文件** |
| 进程数 | 2 个（壳 + Python 子进程） | **1 个**（壳内跑 axum，无子进程） |
| 安装包 | 23.6 MiB | **3.09 MiB** |
| 启动 | 壳轮询 Python 后端就绪（1–3 s） | 同进程启动，通常 < 1 s |

**但要说实话：桌面应用整体内存只从 ~93 MB 降到 ~90 MB。**
因为 Tauri 的 WebView2 控制器和渲染进程才是大头（子进程合计 ~166 MB，与系统共享），
Rust 后端省下的几十 MB 被它盖过去了。这轮重构真正拿到的是：

- 磁盘：安装包 23.6 MiB → **3.09 MiB**，安装后从 ~130 MB → ~20 MB（不再有 124 MB 的 Python 运行时）
- 进程：2 个 → 1 个，没有子进程要管，退出/崩溃语义更简单
- 后端本身：私有内存 58.1 MB → 8.6 MB（独立跑时），CPU 也更省
- 启动：省掉解释器初始化与 sidecar 就绪轮询

要再把整体内存压下去，只能去掉 WebView2 换原生 UI（egui/iced），
代价是 `web/` 不再三端复用、手机版也要重做——那是另一个量级的工程。

Python 后端的固定成本（解释器 + FastAPI/uvicorn/asyncssh/cryptography）是主因；
Rust 版把这块彻底去掉，剩下的内存主要是 SSH 会话缓冲与 SQLite 页缓存。
私有内存差 6.7 倍，这是最实在的一项——它决定系统实际要为这个进程付出多少物理内存。

**界面渲染（WebView2）没有变**，它的内存取决于系统共享的 WebView2 进程，
Rust 重构不改变这一块；如果哪天连这块也要省，只能换原生 UI，代价是三端不再共用 `web/`。

---

## 二、代码结构

新增 `rs/xmm/`（一个 crate，lib + 一个 bin）：

```
rs/xmm/
  Cargo.toml
  src/
    lib.rs        版本号、时间/取整工具
    parser.rs     行协议 → 结构化帧（含多网卡 primary 评选、Docker 段合并）
    ssh.rs        russh 长连接、指数退避重连、看门狗、主机指纹、oneshot 模式、测试连接
    config.rs     config.toml 读写（原子写 + .bak + 顺序/注释保持）
    state.rs      应用状态：采集器生命周期、目标 CRUD、快照、告警边沿、事件分发
    api.rs        axum 路由 + WebSocket + 静态资源（与 Python api.py 一一对应）
    auth.rs       内存 token（24h）
    alerts.rs     温度 / 内存 / 根分区告警判定
    notify.rs     Server酱 / Telegram / SMTP / WebHook 推送（去重、静默、重试、心跳）
    probes.rs     TCP / HTTP / ping 拨测（防抖状态机）
    store.rs      SQLite 历史（WAL、批量事务、降采样查询、过期清理）
    history.rs    5 分钟内存环形缓冲
    mock.rs       模拟采集器
    service.rs    Windows 服务（WinSW 包装 xmm-server.exe）
    bin/xmm-server.rs  独立服务模式 / 浏览器版入口（对应 run.py）
  tests/
    test_parser.rs  18 例（移植 test_parser.py + test_beta23_regression.py 关键断言）
    test_units.rs   11 例（告警、历史落盘指标、配置读写、SQLite 查询）
```

桌面壳 `shell/tauri/src-tauri/src/main.rs` 改为：

1. 选空闲端口 + 生成一次性令牌，写入环境变量 `XMM_DESKTOP_TOKEN`；
2. 起一个专用线程跑 tokio runtime，在同一进程里跑 `xmm::api::router`（含采集器、拨测、历史落盘）；
3. 轮询 `/api/ping` 就绪后创建窗口并注入令牌，自动登录；
4. 托盘、单实例锁、关窗收托盘、`/api/shutdown` 退出逻辑保持原样。

> 注意：`/api/shutdown` 现在结束的是整个应用进程（因为后端就在应用进程里），
> 效果与旧版"后端退出 → 壳跟着退出"一致。

---

## 三、依赖选型

| 能力 | Python 版 | Rust 版 |
|---|---|---|
| HTTP / WS | FastAPI + uvicorn | axum 0.8 + tokio |
| SSH | asyncssh | russh 0.54（`ring` 后端，关掉默认的 aws-lc-rs） |
| 主机指纹 | 自管 known_hosts | `russh::keys::known_hosts`（同格式，autoadd / strict 语义一致） |
| SQLite | sqlite3 | rusqlite（bundled） |
| 配置 | tomllib + 手写 dumper | toml + 手写 dumper（保持段顺序与注释） |
| 推送 HTTP | urllib | reqwest（native-tls，Windows 走 SChannel） |
| 邮件 | smtplib | lettre（tokio1-native-tls） |
| 密钥环 | keyring | keyring（Windows 凭据管理器） |

> 特意没有用 rustls/aws-lc：BoringSSL 的 C 源码在 Windows 上编译要二十分钟以上，
> 而本项目的 TLS 只用于推送/拨测，系统原生 TLS 完全够用。

---

## 三·五、升级兼容：旧 known_hosts 里的 RSA 指纹

实测踩到的坑：旧版用 asyncssh，协商到的是服务器的 **ssh-rsa** 主机密钥并写进 `known_hosts`；
russh 默认优先协商 **ssh-ed25519**，于是同一台机器会被误判成"指纹不匹配"，升级后连不上。

修法（`ssh.rs`）：连接前先读 `known_hosts` 里这台主机记录过的密钥类型，
把对应算法插到 `Preferred.key` 最前面，让服务器用它已备案的那把主机密钥；
校验逻辑本身没放松——真正的密钥变更（同算法不同公钥）仍然拒绝。
已用真实场景验证：拿旧版写下的 RSA-only `known_hosts` 连接 192.168.1.100，成功出数（19 网卡）。

## 四、行为差异（有意为之）

1. **拨测历史指标名修正**：Python 版 `store.add_point` 的调用参数错位（把 up 当成 metric），
   实际落盘成 `metric=1/0`。Rust 版按文档口径写成 `probe.<name>.up` 与 `probe.<name>.ms`。
2. **Windows 服务指向独立 server**：桌面 exe 是 GUI 程序，不能当服务跑；
   `service.rs` 现在优先使用同目录的 `xmm-server.exe`（安装包会带上）。
3. **取整**：Python `round()` 是银行家舍入，Rust 用四舍五入。极端边界值可能差 0.1，
   前端展示无感；所有测试断言的常规值不受影响。
4. **`--mock` 需要有目标**：与 Python 一致（mock 采集器按 `[targets.*]` 逐个创建）；
   空配置时界面无数据，先加一台服务器即可。

---

### 关于 WinSW（Windows 服务）

`packaging/winsw/xiaozhushou-service.exe` 是一个 18 MB 的 .NET 自包含二进制。
为了不让安装包为"可选功能"翻倍，**它不再打进桌面安装包**：

- 需要服务模式时：把 `xiaozhushou-service.exe` 放到 `xmm-server.exe` 同目录，
  再 `POST /api/service {"action":"install"}`（接口会写 xml 并注册服务）；
- 没放时 `/api/service` 返回 `available:false`，其余功能不受影响。

## 五、验证记录（2026-11-09）

- `cargo test`：**29/29 通过**（parser 18 + 单元 11）
- `pytest tests`：**61/61 通过**（Python 代码未改动，仍可作为参照实现）
- 真实 SSH 目标 **192.168.1.100（iStoreOS 24.10.8）**：
  - 19 张网卡全部解析，主网卡 `br-lan`（唯一 `primary`），累计值全为正
  - Docker `ok`，2 running / 3 total，容器 CPU / 内存 / IO 正常合并
  - 风扇 4 路（`it8620:fan1` 1051 rpm，其余 0）、温度 3 路、磁盘、负载、uptime 全部出数
  - 首次连接自动写入 `known_hosts`（`192.168.1.100 ssh-ed25519 ...`）
  - 拨测 `istoreos443` → up（~2 ms）
- HTTP 冒烟：ping / login（错误密码 401）/ snapshot / targets CRUD / config PUT / history /
  probes CRUD / service / 静态页 全部符合预期
- WebSocket 冒烟：hello（含完整快照）→ 每秒 metrics → ping/pong；错误 token 以 4001 关闭
- **升级兼容**：用旧版（asyncssh）留下的 RSA-only `known_hosts` 连接真实目标，成功；
  旧版与新版的 `config.toml` 可直接互读（Rust 写出的文件保持段顺序与注释）
- **界面**：`build/ui_shot.mjs` 无头浏览器验收通过——桌面视图 19 张网卡、
  手机 375px 内部滚动不撑爆，版本徽标 beta2.3
- **桌面壳**：release 构建启动后同进程后端监听 127.0.0.1 随机端口，
  `/api/ping` 正常、`/api/login/desktop` 令牌校验生效、窗口标题与托盘正常

产物体积（release）：

| 产物 | 体积 |
|---|---|
| `主机监控小助手_0.2.3_x64-setup.exe` | **3.09 MiB**（旧版 23.6 MiB） |
| `xiaozhushou-monitor.exe`（桌面主程序） | 8.19 MiB |
| `xmm-server.exe`（独立后端） | 7.0 MiB |

---

## 六、构建

```bat
set PATH=%USERPROFILE%\.cargo\bin;%PATH%
packaging\build_all.bat
```

> Windows 上偶发 cargo/rustc jobserver 死锁（rustc 进程 CPU 长时间零增长），
> `build_all.bat` 里已设 `CARGO_BUILD_JOBS=1` 串行构建规避；手工构建遇到卡住时同样加上。

三步：`cargo build --release --bin xmm-server` → 把 `web/`、`xmm-server.exe`、
WinSW 包装器放进 `shell/tauri/src-tauri/resources/` → `npx @tauri-apps/cli@2 build`。
不再需要 `.venv-build`、PyInstaller 与 Python 运行时。

独立运行（浏览器版 / 服务模式）：

```bat
rs\xmm\target\release\xmm-server.exe --config config.toml --port 8787 --data-dir .\data
rs\xmm\target\release\xmm-server.exe --mock --port 8899     :: 模拟数据
```

---

## 七、还没做的

- **手机版（Chaquopy APK）仍是 Python 后端**。安卓端要复用 Rust 核心需要 Tauri Mobile 或
  JNI 集成，属于下一步工作；本次只优化桌面客户端。
- Python 后端源码（`server/` `collector/`）暂时保留，作为参照实现与手机版依赖；
  待手机端迁移完成后再考虑下线。
