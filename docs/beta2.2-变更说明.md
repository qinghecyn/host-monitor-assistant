# beta2.2 变更说明 —— 运维核心包

> 基线：beta2.1（风扇/温度/Docker + 桌面/手机双端）。
> 本次升级目标（按 [`升级路线图.md`](升级路线图.md) 全量实施）：
> **告警推送、SQLite 历史落盘、Windows 服务自监控、服务拨测**——补齐"满足运维要求"的三块硬缺口，
> 并把产品从"看资源的"升级成"看业务的"。

---

## 一、四大新模块

### ① 告警推送（设置 → 告警推送）

温度/内存/磁盘/风扇阈值告警、拨测事件可推送到外部通道，出事能"叫醒人"：

| 通道 | 说明 |
|---|---|
| Server酱 | 微信接收（推荐），填 SendKey 即可 |
| Telegram Bot | Token + Chat ID |
| SMTP 邮件 | 465(SSL)/587(STARTTLS) 自动适配，支持多收件人 |
| WebHook | POST JSON `{title, body, ts}`——钉钉/企微/自建系统都能接 |

配套机制：

- **去重收敛**：同一告警（主机+类型+对象）未恢复前不重复推；恢复后进入冷却期（默认 10 分钟，可配）
- **恢复通知**：告警解除推送"✓ 已恢复（持续 N 分钟）"——和告警本身同等重要
- **静默时段**：如 02:00-06:00，期间告警只记录不发送（支持跨夜）；**心跳不受静默限制**
- **心跳自监控**：可配每 N 分钟推一条"[心跳] 运行中"（默认关，防把 Server酱免费额度刷爆），
  推送中断 = 监控自己出问题了
- 发送失败重试 3 次（2s/4s/8s 退避），绝不阻塞采集主循环；零新依赖（urllib/smtplib 标准库）

设置面板每个通道都有**"测试"按钮**：先保存再发测试消息，当场验证凭据。

### ② SQLite 历史落盘 + 趋势查询页

- 数据落 `<数据目录>/history.db`（WAL 模式），默认保留 **90 天**（7~365 可配），每小时自动清理
- 写入攒 5 秒批量进一个事务——对手机闪存友好，对采集主循环零影响
- 每帧落盘指标：CPU、内存、最高温度、风扇、上下行（自动挑主网卡，口径与界面一致）、
  根分区、Docker 容器数、每条拨测的存活/延迟
- 界面新增 **历史** 页签：任选主机 / 指标 / 时间范围（1 时~90 天），后端按范围降采样
  （桶数封顶 800），90 天范围也不会卡

### ③ Windows 服务 + 开机自启

- 设置面板新增 **Windows 服务** 区：一键"安装并启动"/"卸载"，开机自启、崩溃 10 秒自动拉起
  （WinSW 包装器随包分发）
- 服务固定监听 `127.0.0.1:8787`，数据目录与桌面版共用
- **服务模式与桌面窗口模式请二选一运行**（两者共用配置文件）；用浏览器访问
  `http://127.0.0.1:8787`（默认 admin/admin123）即可看大屏

### ④ 服务拨测（业务可用性）

- 监控机**直接发起**探测（不走 SSH，全新独立管道）：TCP 端口 / HTTP 网页 / ping
- **防抖**：连续失败 N 次（默认 2）才算宕，恢复一次成功即"恢复"——避免抖动误报
- HTTP 口径：**任何 HTTP 应答都算活**（哪怕 501/500，服务在应答就是活的）；可选"网页需包含关键词"
- 宿主兼容：http 的 host 可直接填完整 URL（支持 https）
- 事件接入 ① 的推送管线：宕机告警 / 恢复通知；结果落 SQLite，趋势页可查
- 界面新增 **拨测** 页签：状态列表（正常/失败 + 延迟）+ 增删改

---

## 二、代码层面

```
新增
  server/store.py         SQLite 落盘：批量写、降采样查询、保留清理
  server/notify.py        推送通道 + 收敛/恢复/静默/心跳
  server/probes.py        拨测执行器（TCP/HTTP/ICMP）+ 防抖状态机
  server/service.py       WinSW 服务安装/卸载/状态
  tests/test_store.py     落盘/查询/降采样/清理（3 例）
  tests/test_notify.py    收敛/恢复/冷却/静默/通道发现（6 例）
  tests/test_probes.py    TCP/HTTP/防抖（4 例）
  packaging/winsw/        WinSW-x64.exe（重命名为 xiaozhushou-service.exe 随包分发）
修改
  server/alerts 事件化    State._alert_edges：新增→告警推送、消失→恢复推送
  server/app.py           on_frame 挂钩落盘；lifespan 启停 store/拨测/心跳；快照带 probes/notify
  server/api.py           /api/history、/api/probes CRUD、/api/notify/test、/api/service；
                          配置白名单扩 notify/history/probes（凭据只写不读）
  server/config.py        DEFAULTS + dump_config 支持 [notify]/[history]/[probes.*]
  web/                    历史/拨测抽屉、设置推送区与服务区、charts.js 静态曲线工厂
  packaging/xmm_server.spec  去掉 sqlite3 排除；新模块 hiddenimports；WinSW datas
  版本号全套              beta2.2（0.2.2）
```

数据流：

```
采集帧 ──▶ store.add_frame ──▶ 队列 ──5s 批量事务──▶ history.db
   └─▶ alerts.evaluate ──边沿──▶ notify.alert/recovered ──▶ 四通道（收敛/静默/重试）
拨测任务 ──▶ 状态机（防抖）──▶ 事件 ──▶ 同上推送管线 + store.add_point
```

---

## 三、验证结论（全部实测）

- `pytest tests`：**50/50 通过**（新增 13 例；修复中发现并修正 2 个真 bug：
  降采样桶数溢出、urllib 对非 2xx 抛异常导致 HTTP 拨测误判宕）
- **WebHook 全链路**（本地接收器）：告警→收到 → 去重（同 zone 只 1 条，双传感器各自独立告警正确）
  → 阈值调回→收到恢复通知 → 拨测死端口→收到宕机告警
- **真实 iStoreOS**：拨测 `tcp 192.168.1.100:443` = up · 2ms；历史落盘 CPU/风扇正常积累；
  趋势页/拨测页/推送设置界面浏览器截图确认
- SQLite 查询：10 分钟窗口 33 点、24 小时窗口正确降采样
