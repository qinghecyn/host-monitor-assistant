# 贡献指南

感谢关注！提 Issue / PR 前请过一眼这几条：

## 开发约束（重要）

1. `collector/remote/collect.sh` 被 `rs/xmm/src/ssh.rs` 用 `include_str!` 编译期内嵌，**不能删**；
   改采集逻辑优先改它（保持 POSIX sh + BusyBox 兼容，别引入 bash 特性）。
2. `state.rs` 的 `std::sync::Mutex/RwLock` 不可重入：加锁顺序固定 cfg → collectors → histories/alerts，
   守卫用完即出块，**禁止嵌套加锁**。
3. axum handler / dispatcher 里**锁守卫不能跨 await**（否则 future 非 Send）。
4. russh 保持 `ring` 后端，别打开默认 `aws-lc-rs`（Windows 编译极慢）。
5. 改动后必须 `cargo test --manifest-path rs/xmm/Cargo.toml` 全绿再提 PR。
6. 改 `web/` 后同步复制到 `shell/tauri/src-tauri/resources/web/`。
7. 版本号变更需同步：`rs/xmm/src/lib.rs VERSION`、`web/index.html` 徽标、`web/js/app.js` 兜底、
   `shell/tauri/src-tauri/{tauri.conf.json,Cargo.toml,src/main.rs}`、两个 `Cargo.toml`/`Cargo.lock`、
   `config.example.toml`、README、使用手册。

## 提交

- 一个 PR 只做一件事；中文或英文描述都可以。
- 新增解析字段请带上 `rs/xmm/tests/` 回归用例（坏帧 / 缺段 / 计数器回绕都要覆盖）。
- 不要提交任何运行数据：`config.toml`、`known_hosts`、`history.db`、日志（.gitignore 已挡）。

## 安全

发现安全问题请不要开公开 Issue，直接联系维护者。
