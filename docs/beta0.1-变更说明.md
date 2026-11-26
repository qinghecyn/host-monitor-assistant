# beta0.1 变更说明

> 基线：第一阶段交付版（Web 版 + 只读 SSH 采集）。
> 本次升级的目标：**把 config.toml 里的 `[targets.*]` 变成可视化界面，多台服务器随意增删改，
> 打包成装完就能用的桌面程序（依赖环境一起打包）。**

---

## 一、这次到底改了什么

### 1. 服务器管理可视化（核心）

以前加一台机器要：记事本打开 `config.toml` → 抄一段 `[targets.xxx]` → 存盘 → 关黑窗口 → 重新启动。
现在：界面右上角 **服务器** → **＋ 添加服务器** → 填地址/账号/密码 → **测试连接** → **保存**，
**立刻生效，不用重启**。

| 能力 | 说明 |
|---|---|
| 多台服务器 | 数量不限，一台一段，互不影响；某台掉线只影响它自己 |
| 新增 | 名称自动校验（只能是字母/数字/下划线/中划线，避免把 TOML 写坏） |
| 编辑 | 只改显示名/备注时**不重连**；改了地址/端口/密码才会重连该目标 |
| 删除 | 二次确认，删完立刻停掉那条 SSH 长连接 |
| 测试连接 | 用和采集完全一样的参数试连一次，返回耗时、系统名、核数；顺手记录主机指纹 |
| 自动落盘 | 每次改动都原子写回 `config.toml`（先写临时文件再替换，并留 `.bak`） |
| 密码不回传 | 接口只告诉前端"有没有密码"，密码永远不回到浏览器 |

### 2. 后台运行 + 退出按钮

- 桌面版启动后**没有黑窗口**：后端以隐藏子进程方式跑，托盘常驻。
- 点窗口右上角 × = **收进托盘**，监控继续跑（这就是"后台运行"）。
- 真要退出：界面右上角 **退出** 按钮，或托盘右键 → **退出（停止监控）**。
  退出时会把后端进程一起杀干净，不留孤儿进程。
- 浏览器版（双击 `启动监控.bat`）的 **退出** 按钮会停掉服务进程。

### 3. 装完即用（Tauri + 自带运行环境）

- 后端用 PyInstaller 冻结成 `xiaozhushou-server.exe`（onedir），**Python 解释器 + asyncssh +
  FastAPI + uvicorn 全部内置**，目标机器不需要装 Python，也不需要装任何依赖。
- Tauri 壳负责：挑空闲端口 → 拉起后端 → 等 `/api/ping` 就绪 → 开窗口指向本地服务。
- 首次运行自动在 `%APPDATA%\com.xiaozhushou.monitor\` 生成 `config.toml`、`known_hosts`、`server.log`。
- 桌面版用壳进程注入的一次性令牌自动登录，**用户看不到登录页**；后端只监听 `127.0.0.1`。

### 4. 顺手修掉的老问题

| 问题 | 处理 |
|---|---|
| 切服务器时 Chart.js 报 "Canvas is already in use"，图表不再刷新 | 切换前 `chart.destroy()`，图表实例不再泄漏 |
| 路由器上主网卡显示成 `tunl0`/`ip6tnl0`（隧道口，永远 0 kbps） | 优先选 `eth/en/wl/br-/lan/wan` 且累计流量最大的那张网卡 |
| 网卡列表把卡片撑到 6 行高 | 只显示主网卡 + "共 N 个接口"，完整列表放 tooltip |
| 磁盘列表混进 `efivarfs` 这种总量为 0 的伪挂载 | 总量 < 1MB 的挂载不显示 |
| 端口被占用时只抛堆栈 | 明确提示"端口 x 被占用"，并以退出码 2 结束 |
| 配置写一半断电会毁文件 | 原子写 + `.bak` 备份 |

---

## 二、代码层面

```
新增
  paths.py                    冻结/源码两种运行方式的路径解析
  sidecar_main.py             PyInstaller 打包入口
  collector/connection.py     SSH 连接参数构造（采集器与"测试连接"共用）
  server/probe.py             测试连接（含友好错误翻译 + 记录指纹）
  web/js/targets.js           服务器管理抽屉
  web/js/ui.js                轻提示 / 转义
  packaging/xmm_server.spec   PyInstaller 配置
  packaging/build_all.bat     一键构建（后端 → 资源 → 安装包）
  tests/test_targets.py       多目标 CRUD / 校验 / 落盘 / 桌面登录（11 例）

重写
  server/config.py            多目标读写、原子写、名称校验、公开字段过滤
  server/app.py               State 持有目标全生命周期（增删改即时生效）
  server/api.py               /api/targets CRUD、/api/targets/test、/api/shutdown、
                              /api/login/desktop、/api/ping
  run.py                      --data-dir、日志落文件、端口占用友好提示
  web/index.html / app.js     服务器管理、退出、桌面免登录、空状态引导
  shell/tauri/src-tauri/      启动后端、托盘、单实例窗口、退出联动
```

### 新增接口

| 方法 | 路径 | 作用 |
|---|---|---|
| GET | `/api/ping` | 就绪探测（无需鉴权，桌面壳启动时轮询） |
| GET | `/api/targets` | 服务器列表（含状态，不含密码） |
| POST | `/api/targets` | 新增 `{name, host, port, username, password?, private_key?, label?, note?}` |
| PUT | `/api/targets/{name}` | 修改（密码留空 = 保持原密码） |
| DELETE | `/api/targets/{name}` | 删除 |
| POST | `/api/targets/test` | 试连（可只传 `name` 复用已存密码） |
| POST | `/api/login/desktop` | 桌面版免登录换 token（需壳进程注入的 `XMM_DESKTOP_TOKEN`） |
| POST | `/api/shutdown` | 停止服务（桌面版会让窗口一起退出） |

---

## 三、怎么构建

前置：Python 3.10+、Node（只为拿 Tauri CLI）、Rust 工具链。

```bat
:: 一键：装打包依赖 → 冻结后端 → 拷进 Tauri 资源 → 出 NSIS 安装包
packaging\build_all.bat
```

分步：

```bash
python -m venv .venv-build
.venv-build\Scripts\pip install -r requirements.txt pyinstaller

# 1) 冻结后端（自带 Python 运行时）
.venv-build\Scripts\pyinstaller --noconfirm ^
  --distpath build\dist-sidecar --workpath build\pyinstaller packaging\xmm_server.spec

# 2) 拷进 Tauri 资源目录
xcopy /E /I /Y build\dist-sidecar\xiaozhushou-server shell\tauri\src-tauri\resources\server-dist

# 3) 出安装包
cd shell\tauri
npx @tauri-apps/cli@2 build
```

产物：

```
shell/tauri/src-tauri/target/release/bundle/nsis/主机监控小助手_0.1.0_x64-setup.exe
```

---

## 四、验证记录（本机实测）

| 项目 | 结果 |
|---|---|
| pytest | **28 passed**（原 17 例 + 新增 11 例多目标/桌面登录/校验） |
| 冻结后端独立运行 | `/api/ping` 200、静态页 200、桌面令牌登录成功、新增目标落盘 |
| 真实 SSH 全链路（冻结 exe） | 目标 `192.168.1.100`（iStoreOS 24.10.6，8 核）：连接 55ms，CPU/内存/温度/磁盘/网络全部取到 |
| 浏览器 UI（Playwright） | 登录、多目标切换、添加/测试/保存/删除、设置面板、手机竖屏，**0 控制台报错** |
| 安装包 | NSIS 安装后双击即用，无需 Python 环境 |
