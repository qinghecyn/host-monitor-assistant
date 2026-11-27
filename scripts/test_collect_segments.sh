#!/bin/sh
# collect.sh 新段（PSUM / PTOP / CON）本地回归。
# 用 XMM_PROC_ROOT 注入假 /proc，两帧验证 CPU% 差值；不碰真实 /proc、不落盘。
# 跑法：sh scripts/test_collect_segments.sh
set -u

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
collect="$here/../collector/remote/collect.sh"
# 固定用 POSIX 风格 /tmp 路径：mktemp 在 Git Bash 下会给出带反斜杠的 Windows 路径，
# awk -v 的转义处理会把 \U \A 等吃掉（statfile 打不开，CPU% 差值算不出来）。
root="/tmp/xmm-collect-test.$$"
mkdir -p "$root"
cleanup() { rm -rf "$root"; }
trap cleanup EXIT INT TERM

mkdir -p "$root/1" "$root/2" "$root/net"

# /proc/stat：总 jiffies 10000，2 核
cat > "$root/stat" <<'EOF'
cpu  1000 0 1000 8000 0 0 0 0 0 0
cpu0 500 0 500 4000 0 0 0 0 0 0
cpu1 500 0 500 4000 0 0 0 0 0 0
procs_running 1
procs_blocked 0
EOF

# pid 123：名字带空格的普通进程（ticks=800，rss=2048 页 ≈ 8MB）
echo '123 (my proc) S 1 123 123 0 -1 4194304 100 0 0 0 500 300 0 0 20 0 2 0 999 1048576 2048' > "$root/1/stat"
# pid 456：僵尸进程（ticks=30，rss=0）
echo '456 (z0mbie) Z 1 456 456 0 -1 0 0 0 0 0 10 20 0 0 20 0 1 0 999 0 0' > "$root/2/stat"

# /proc/net/tcp：1 LISTEN + 1 ESTAB + 1 TIME_WAIT；tcp6：1 LISTEN；udp：1 个套接字
{
  printf '  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n'
  printf '   0: 0100007F:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12345 1 0000000000000000 100 0 0 10 0\n'
  printf '   1: 0100007F:0016 0100007F:C1A2 01 00000000:00000000 00:00000000 00000000     0        0 12346 1 0000000000000000 100 0 0 10 0\n'
  printf '   2: 0100007F:0016 0100007F:C1A3 06 00000000:00000000 00:00000000 00000000     0        0 12347 1 0000000000000000 100 0 0 10 0\n'
} > "$root/net/tcp"
{
  printf '  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n'
  printf '   0: 00000000000000000000000000000000:0016 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 22345 1 0000000000000000 100 0 0 10 0\n'
} > "$root/net/tcp6"
{
  printf '   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n'
  printf '   12: 00000000:0035 00000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 32345 2 0000000000000000 0 0 0\n'
} > "$root/net/udp"

XMM_NO_DISPATCH=1
XMM_PROC_ROOT="$root"
# shellcheck disable=SC1090
. "$collect"

fail=0
check() {
  # check <描述> <实际> <期望包含>
  case "$2" in
    *"$3"*) echo "ok   $1" ;;
    *) echo "FAIL $1"; echo "     期望包含: $3"; echo "     实际: $2"; fail=1 ;;
  esac
}

emit_once > "$root/out1.txt"
frame1=$(cat "$root/out1.txt")

check "首帧 PSUM 进程数 2 / 运行 0 / 僵尸 1" "$frame1" "PSUM=2,0,1"
check "首帧 PTOP 含 pid 123 与清洗后的名字" "$frame1" "123,,8192,my proc"
check "首帧 PTOP 含僵尸 pid 456" "$frame1" "456,,0,z0mbie"
check "首帧 PTOP 按 rss 降序（123 在 456 前）" "$frame1" "PTOP=123,,8192,my proc;456,,0,z0mbie"
check "首帧 CON ESTAB=1 TIME_WAIT=1 LISTEN=2 UDP=1" "$frame1" "CON=1,1,2,1"

# 第二帧前推进计数器：pid123 ticks +1000（utime 500→1500），/proc/stat 总 jiffies +4000
echo '123 (my proc) S 1 123 123 0 -1 4194304 100 0 0 0 1500 300 0 0 20 0 2 0 999 1048576 2048' > "$root/1/stat"
cat > "$root/stat" <<'EOF'
cpu  2000 0 2000 10000 0 0 0 0 0 0
cpu0 1000 0 1000 5000 0 0 0 0 0 0
cpu1 1000 0 1000 5000 0 0 0 0 0 0
procs_running 1
procs_blocked 0
EOF

emit_once > "$root/out2.txt"
frame2=$(cat "$root/out2.txt")

# pcpu = 1000 * 100 * 2 / 4000 = 50.0
check "次帧 PTOP CPU% 为 50.0（两帧差值）" "$frame2" "123,50.0,8192,my proc"
check "次帧 PTOP 排序改为 CPU% 优先" "$frame2" "PTOP=123,50.0,8192,my proc"
check "次帧 PSUM 保持" "$frame2" "PSUM=2,0,1"

if [ "$fail" = 0 ]; then
  echo "collect.sh 新段回归通过"
else
  echo "collect.sh 新段回归失败"
fi
exit "$fail"
