# -*- coding: utf-8 -*-
"""beta2.3 多网卡验收脚本（可重复运行，不依赖 pytest）。

用法：
    # 先验模拟数据（4 张网卡）
    .venv\\Scripts\\python scripts\\verify_multi_nic.py --port 8899 --expect-min 4
    # 再验真实目标（要求至少 2 张，实际通常 10+）
    .venv\\Scripts\\python scripts\\verify_multi_nic.py --port 8900 --expect-min 2 --wait 60

检查项：
  1. /api/ping 版本 = beta2.3；
  2. 每个目标的 network 数组长度 >= expect-min（旧版只会是 1 张 eth0）；
  3. 每帧恰好一个 primary，且 primary 不是 down 状态；
  4. 所有累计流量非负（旧 collect.sh 的 busybox awk %d 会溢出成负数）；
  5. 每张网卡都有独立的 rx/tx 速率字段。

登录凭据从 --config 指定的 toml 读取（默认 config.toml），不会打印到终端。
"""
import argparse
import json
import time
import tomllib
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
REAL_IFACE_PREFIXES = ("eth", "en", "wl", "br-", "lan", "wan", "bond", "pppoe", "ap")


def http(base, path, body=None, token=None):
    url = base + path
    data = json.dumps(body).encode() if body is not None else None
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = "Bearer " + token
    req = urllib.request.Request(url, data=data, headers=headers)
    with urllib.request.urlopen(req, timeout=8) as r:
        return json.loads(r.read().decode("utf-8"))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8899)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--config", default=str(ROOT / "config.toml"))
    ap.add_argument("--expect-min", type=int, default=2, help="每个目标至少要有几张网卡")
    ap.add_argument("--wait", type=int, default=30, help="等待首帧数据的秒数")
    ap.add_argument("--expect-version", default="beta2.3")
    args = ap.parse_args()

    base = "http://%s:%d" % (args.host, args.port)
    ok = True

    ping = http(base, "/api/ping")
    version = ping.get("version")
    print("[版本] /api/ping -> %s (期望 %s)" % (version, args.expect_version))
    ok &= version == args.expect_version

    cfg_path = Path(args.config)
    auth = {}
    if cfg_path.exists():
        with open(cfg_path, "rb") as f:
            auth = tomllib.load(f).get("auth", {})
    token = http(base, "/api/login", {
        "username": auth.get("username", "admin"),
        "password": auth.get("password", "admin123"),
    }).get("token")
    if not token:
        print("[失败] 登录没拿到 token")
        return 1

    snap = None
    deadline = time.time() + args.wait
    while time.time() < deadline:
        snap = http(base, "/api/snapshot", token=token)
        if any((d.get("latest") or {}).get("network") for d in (snap.get("data") or {}).values()):
            break
        time.sleep(1)
    else:
        print("[失败] %d 秒内没有任何目标产出 network 数据" % args.wait)
        return 1

    print("[快照] version=%s mock=%s targets=%s" %
          (snap.get("version"), snap.get("mock"), snap.get("targets")))
    for name, d in (snap.get("data") or {}).items():
        nets = ((d.get("latest") or {}).get("network")) or []
        primaries = [n.get("iface") for n in nets if n.get("primary")]
        negatives = [n.get("iface") for n in nets
                     if (n.get("rx_total_kb") or 0) < 0 or (n.get("tx_total_kb") or 0) < 0]
        missing_rate = [n.get("iface") for n in nets
                        if "rx_kbps" not in n or "tx_kbps" not in n]
        print("\n[目标 %s] status=%s 网卡数=%d 主网卡=%s" %
              (name, d.get("status"), len(nets), primaries))
        for n in nets:
            print("   %-18s state=%-9s primary=%-5s rx=%-12s tx=%-12s total=%s / %s KB" % (
                n.get("iface"), n.get("state"), n.get("primary"),
                n.get("rx_kbps"), n.get("tx_kbps"),
                n.get("rx_total_kb"), n.get("tx_total_kb")))
        checks = [
            ("网卡数 >= %d" % args.expect_min, len(nets) >= args.expect_min),
            ("恰好一个 primary", len(primaries) == 1),
            ("无负累计值", not negatives),
            ("每张卡都有独立速率字段", not missing_rate),
        ]
        for label, passed in checks:
            print("   %s %s" % ("PASS" if passed else "FAIL", label))
            ok &= passed
        if primaries:
            p = next(n for n in nets if n.get("primary"))
            is_real = str(p.get("iface") or "").lower().startswith(REAL_IFACE_PREFIXES)
            not_down = str(p.get("state") or "").lower() != "down"
            print("   %s 主网卡是真实接口(%s)且非 down(%s)" %
                  ("PASS" if is_real and not_down else "FAIL", p.get("iface"), p.get("state")))
            ok &= is_real and not_down

    print("\n结论：%s" % ("多网卡验收通过" if ok else "多网卡验收失败"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
