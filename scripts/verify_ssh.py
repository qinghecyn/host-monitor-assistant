"""真实 SSH 采集链路验证：asyncssh → 目标机 collect.sh → 本地解析。

用法：
  python scripts/verify_ssh.py --host 127.0.0.1 --user qinhe --key D:\\path\\key
  python scripts/verify_ssh.py --host 192.168.1.1 --user root --env MON_PASSWORD
退出码 0 = 验证通过。
"""
import argparse
import asyncio
import os
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from collector.ssh_client import HostCollector  # noqa: E402


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", required=True)
    ap.add_argument("--port", type=int, default=22)
    ap.add_argument("--user", default="root")
    ap.add_argument("--key", default=None, help="私钥路径")
    ap.add_argument("--password-env", default="MON_PASSWORD", help="从该环境变量读 SSH 密码")
    ap.add_argument("--frames", type=int, default=5, help="收集多少帧后停止")
    ap.add_argument("--interval", type=float, default=1.0)
    args = ap.parse_args()

    cfg = {"host": args.host, "port": args.port, "username": args.user}
    if args.key:
        cfg["private_key"] = args.key
    collect_cfg = {"interval_s": args.interval, "history_minutes": 5}

    pw = os.environ.get(args.password_env, "") if not args.key else ""
    coll = HostCollector("verify", cfg, collect_cfg, lambda t: pw or None)

    frames = []
    statuses = []
    done = asyncio.Event()

    async def on_frame(name, frame):
        frames.append(frame)
        if len(frames) >= args.frames:
            done.set()

    async def on_status(name, s):
        statuses.append(s)

    coll.start(on_frame, on_status)
    print("连接 %s@%s:%s ..." % (args.user, args.host, args.port))
    try:
        await asyncio.wait_for(done.wait(), timeout=40)
    except asyncio.TimeoutError:
        print("FAIL: %.0f 秒内未收到 %d 帧数据" % (40, args.frames))
        print("状态记录:", statuses)
        sys.exit(2)
    finally:
        await coll.stop()

    print("状态记录:", statuses)
    ok = True
    for i, f in enumerate(frames):
        cpu = f.get("cpu") or {}
        mem = f.get("memory") or {}
        nets = f.get("network") or []
        print("帧%d: cpu=%s%%(%s核) mem可用=%s%% 磁盘=%d个 网卡=%d个 温度=%d个 os=%s uptime=%ss" % (
            i, cpu.get("percent"), cpu.get("cores"),
            mem.get("available_percent"), len(f.get("disks") or []),
            len(nets), len(f.get("temperatures") or []),
            (f.get("os") or {}).get("pretty_name"), f.get("uptime_s")))
    last = frames[-1]
    if not (last.get("cpu") or {}).get("cores"):
        print("FAIL: 未取得 CPU 核数"); ok = False
    if not (last.get("memory") or {}).get("total_kb"):
        print("FAIL: 未取得内存"); ok = False
    if not (last.get("uptime_s") or 0) > 0:
        print("FAIL: 未取得 uptime"); ok = False
    rates = [f for f in frames[2:] if (f.get("cpu") or {}).get("percent") is not None]
    if not rates:
        print("FAIL: 3 帧后仍未计算出 CPU 速率"); ok = False
    print("RESULT:", "PASS" if ok else "FAIL")
    sys.exit(0 if ok else 3)


if __name__ == "__main__":
    asyncio.run(main())
