# Tauri 桌面壳（beta0.1）

和旧版最大的区别：**旧版只是个套壳浏览器，要用户自己先开后端；
现在壳进程负责把后端拉起来，用户只管点图标。**

## 它做了什么

```
用户双击图标
   ↓
① 抢单实例锁（127.0.0.1:47823）—— 已经在跑就把老窗口叫出来，自己退出
② 数据目录 = %APPDATA%\com.xiaozhushou.monitor\（配置 / known_hosts / 日志）
③ 挑一个空闲端口 + 生成一次性令牌
④ 隐藏窗口方式启动 resources\server-dist\xiaozhushou-server.exe
     --host 127.0.0.1 --port <port> --data-dir <上面的目录>
     环境变量 XMM_DESKTOP_TOKEN=<令牌>
⑤ 轮询 http://127.0.0.1:<port>/api/ping，就绪后建窗口指向它
     （窗口通过 initialization_script 拿到令牌 → 前端自动登录，用户看不到登录页）
⑥ 托盘常驻：左键显示窗口，右键菜单"显示主窗口 / 退出（停止监控）"
⑦ 关窗口 = 收进托盘（监控继续跑）；真退出用界面"退出"或托盘菜单
⑧ 后端退出（用户点了退出）→ 600ms 内壳进程跟着退出；壳退出 → 一定杀干净子进程
```

后端起不来时窗口不会白屏，而是显示一页带原因和日志路径的错误页。

## 前置条件

1. Rust 工具链（`rustup`，Windows 上 GNU 目标即可：`rustup default stable-x86_64-pc-windows-gnu`）
2. Node（只为跑 `npx @tauri-apps/cli`）
3. WebView2（Win10/11 自带；Win7/8 需另装）
4. 先产出后端：`packaging\build_all.bat` 会顺手做好

## 构建

```bash
# 推荐：项目根目录一键
packaging\build_all.bat

# 或者手动
cd shell/tauri
npx @tauri-apps/cli@2 build
```

产物：

```
src-tauri/target/release/xiaozhushou-monitor.exe                       （裸 exe，需同目录有 server-dist）
src-tauri/target/release/bundle/nsis/主机监控小助手_0.1.0_x64-setup.exe （安装包，约 16MB）
```

## 说明

- `tauri.conf.json` 里 `bundle.resources` 把 `resources/server-dist/` 整个打进安装包，
  安装后位于 `<安装目录>\server-dist\`，运行时用 `resource_dir()` 找它。
- 开发模式（`cargo run`）找不到安装目录时，会往上层目录找 `dist/xiaozhushou-server/`。
- 窗口不加载 `tauri://` 协议下的静态文件，而是直接加载本地 HTTP 服务
  （`WebviewUrl::External`），所以前端和浏览器版完全是同一份代码。
- 采集层永远在后端进程里，桌面壳不含任何 SSH 逻辑，也不碰密码。
