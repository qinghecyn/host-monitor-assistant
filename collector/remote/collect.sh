#!/bin/sh
# 主机监控采集脚本 —— 在目标机上以只读方式运行（OpenWrt busybox ash / Debian dash 均兼容）。
# 只读取 /proc、/sys、执行 df -P -k 和 docker 只读查询，不做任何写操作。
# beta2.0 起附带 Docker 负载段（DKS/DKP/DKST）：没装 docker 的机器上完全不输出。
#
# 用法：
#   sh -s           常驻模式：每 INTERVAL 秒输出一行（INTERVAL 由调用方在 stdin 首行注入）
#   sh -s -- --once 单次模式：输出一行后退出（SSH 断流回退方案）
#                    ↑ 必须有 `--`，否则 --once 会被 sh 当成自己的选项，$1 为空，脚本会进入常驻循环
#
# 行协议（段之间用 | 分隔，条目用 ; 分隔，字段用 , 分隔；任一段采集失败则整段省略）：
#   T=<unix秒>
#   U=<uptime 整秒>
#   L=<load1>,<load5>,<load15>
#   CPU=t:usr,nic,sys,idle,iow,irq,sirq,steal;0:...;1:...   （累计 jiffies，速率由本地差值计算）
#   MEM=<总KB>,<可用KB>,<Buffers>,<Cached+SReclaimable>
#   DSK=<挂载点>,<总KB>,<可用KB>,<累计读KB>,<累计写KB>,<块设备名>;...  （beta1.3：读写取
#          /proc/diskstats，速率由解析端按帧差值算；第 6 字段为 df 的设备列，如 /dev/sda1，
#          旧版解析器只读前 5 字段，仍兼容；伪文件系统留空，前端回退显示挂载点）
#   NET=<网卡>,<rx累计字节>,<tx累计字节>,<operstate>;...   （beta2.3：全部网卡，排除 lo）
#   TMP=<类型>,<毫摄氏度>;...
#   FAN=<名称>,<RPM>;...                                  （beta2.1 风扇段，无驱动时省略）
#   GPU=<名称>,<驱动>,<占用%>,<显存已用MiB>,<显存总MiB>,<温度℃>,<功耗W>;...  （beta2.4 GPU 段，
#          NVIDIA 走 nvidia-smi，AMD/Intel 走 sysfs；数值缺失留空字段，无显卡则整段省略）
#   OS=<PRETTY_NAME 全文>
#   DKS=none | down | ok,<运行数>,<容器总数>              （beta2.0 Docker 段）
#   DKP=<id>,<名称>,<镜像>,<state>,<状态详情>;...          （docker ps -a，缓存刷新）
#   DKST=<id>,<cpu%>,<已用内存>,<内存上限>,<内存%>,<网络IO>,<块IO>;...   （docker stats，仅运行中的）

INTERVAL=${INTERVAL:-1}

# ---------- Docker（beta2.0：可选段，没装 docker 就完全不输出） ----------
# docker stats --no-stream 本身要采样 1~2 秒，容器多时更久，不能每秒都跑；
# 这里按时间缓存：容器列表每 DK_PS_EVERY 秒、资源占用每 DK_STATS_EVERY 秒刷新一次，
# 其余循环直接复用缓存值。刷新耗时会让那一帧变慢，速率差值用的是帧内时间戳，不受影响。
DK_PS_EVERY=5
DK_STATS_EVERY=30
DK_AVAIL=""      # 空=还没探测 0=没装 docker 1=有
DK_DAEMON=0      # 1=守护进程正常 0=没起来
DK_PS_T=-999999
DK_ST_T=-999999
DK_PS=""
DK_ST=""

dk_fields() {
  # 清洗条目/段分隔符（; | → 空格）；字段内的逗号不清洗，解析端让"最后一个字段"吞掉。
  # 第 1 列容器 ID 截到 12 位，与 docker stats 的输出对齐（合并键）。
  sed 's/[;|]/ /g' | awk -F, 'BEGIN{OFS=","} NF{$1=substr($1,1,12); print}'
}

docker_ps_refresh() {
  out=$(docker ps -a --format '{{.ID}},{{.Names}},{{.Image}},{{.State}},{{.Status}}' 2>/dev/null \
        | dk_fields | tr '\n' ';')
  out=${out%;}
  if [ -n "$out" ]; then
    DK_PS="$out"
    DK_DAEMON=1
  elif docker ps >/dev/null 2>&1; then
    DK_PS=""
    DK_DAEMON=1      # 守护进程正常，只是没有容器
  else
    DK_PS=""
    DK_DAEMON=0      # docker 命令在但守护进程没起来
  fi
  DK_PS_T=$(date +%s)
}

docker_stats_refresh() {
  fmt='{{.ID}},{{.CPUPerc}},{{.MemUsage}},{{.MemPerc}},{{.NetIO}},{{.BlockIO}}'
  if command -v timeout >/dev/null 2>&1; then
    out=$(timeout 15 docker stats --no-stream --format "$fmt" 2>/dev/null \
          | dk_fields | tr '\n' ';')
  else
    out=$(docker stats --no-stream --format "$fmt" 2>/dev/null \
          | dk_fields | tr '\n' ';')
  fi
  DK_ST=${out%;}
  DK_ST_T=$(date +%s)
}

docker_tick() {
  [ "$DK_AVAIL" = "0" ] && return
  if [ -z "$DK_AVAIL" ]; then
    if command -v docker >/dev/null 2>&1; then
      DK_AVAIL=1
    else
      DK_AVAIL=0
      return
    fi
  fi
  now=$(date +%s)
  [ $((now - DK_PS_T)) -ge "$DK_PS_EVERY" ] && docker_ps_refresh
  [ $((now - DK_ST_T)) -ge "$DK_STATS_EVERY" ] && docker_stats_refresh
}

docker_emit() {
  if [ "$DK_AVAIL" = "0" ]; then
    printf 'DKS=none'
    return
  fi
  if [ "$DK_DAEMON" = "1" ]; then
    cnt=$(printf '%s' "$DK_PS" | tr ';' '\n' | awk -F, 'NF{n++; if ($4=="running") r++} END{printf "%.0f,%.0f", r+0, n+0}')
    seg="DKS=ok,$cnt"
  else
    seg="DKS=down"
  fi
  [ -n "$DK_PS" ] && seg="$seg|DKP=$DK_PS"
  [ -n "$DK_ST" ] && seg="$seg|DKST=$DK_ST"
  printf '%s' "$seg"
}

os_info() {
  if [ -r /etc/os-release ]; then
    awk -F= '$1=="PRETTY_NAME"{gsub(/"/,"",$2);print $2}' /etc/os-release 2>/dev/null
  fi
}

cpu_info() {
  awk '
    # 注意：busybox awk 的 %d 是 32 位有符号，8 核机器跑几十天后累计 jiffies
    # 就会超过 2^31 变成负数。累计型数值统一用 %.0f 输出，避免计数回绕。
    $1=="cpu" {
      printf "t:%.0f,%.0f,%.0f,%.0f,%.0f,%.0f,%.0f,%.0f;", $2,$3,$4,$5,$6,$7,$8,$9
      next
    }
    $1 ~ /^cpu[0-9]+$/ {
      printf "%s:%.0f,%.0f,%.0f,%.0f,%.0f,%.0f,%.0f,%.0f;", substr($1,4), $2,$3,$4,$5,$6,$7,$8,$9
    }
  ' /proc/stat 2>/dev/null
}

mem_info() {
  awk -F: '
    $1=="MemTotal"     {t=$2}
    $1=="MemFree"      {f=$2}
    $1=="MemAvailable" {a=$2}
    $1=="Buffers"      {b=$2}
    $1=="Cached"       {c=$2}
    $1=="SReclaimable" {s=$2}
    END {
      if (a=="") a=f+b+c+s
      printf "%.0f,%.0f,%.0f,%.0f", t+0, a+0, b+0, c+s
    }
  ' /proc/meminfo 2>/dev/null
}

load_info() {
  awk '{printf "%s,%s,%s", $1, $2, $3}' /proc/loadavg 2>/dev/null
}

uptime_info() {
  awk '{printf "%.0f", $1}' /proc/uptime 2>/dev/null
}

disk_info() {
  # df -P 不带文件系统类型列，按设备名/挂载点排除伪文件系统；
  # 唯一例外：根分区 / 即使是 overlay（OpenWrt）也保留，供"根分区可用<1G"告警使用。
  # beta1.3：附加上 /proc/diskstats 里该设备的累计读写 KB（扇区×512/1024），
  # 前端展示的 MiB/s / GiB/s 由解析端按两次采样的差值计算。
  # 第 6 字段输出 df 的设备列（/dev/sdxN 风格），前端可切换"设备名/挂载点"显示；
  # overlay/tmpfs 等伪文件系统没有独立块设备，该字段留空。
  df -P -k 2>/dev/null | awk -v stats=/proc/diskstats '
    BEGIN {
      while ((getline line < stats) > 0) {
        split(line, f, " ")
        d=f[3]
        if (d ~ /^(loop|ram|dm-|md[0-9])/) continue
        rd[d]=f[6]+0; wr[d]=f[10]+0
      }
      close(stats)
    }
    NR>1 {
      dev=$1; total=$2+0; avail=$4+0; mnt=$6
      if (total<=0) next
      gsub(/,/, "_", mnt)
      skip=0
      if (dev=="tmpfs"||dev=="devtmpfs"||dev=="udev"||dev=="none"||dev=="overlay"||dev=="overlayfs"||dev=="rootfs"||dev=="ramfs"||dev=="shm"||dev=="proc"||dev=="sysfs"||dev=="devpts"||dev=="squashfs"||dev=="cgroup"||dev=="cgroup2"||dev=="efivarfs"||dev=="debugfs"||dev=="tracefs"||dev=="configfs"||dev=="securityfs"||dev=="pstore"||dev=="bpf"||dev=="hugetlbfs"||dev=="mqueue"||dev=="nsfs") skip=1
      if (dev ~ /^overlay:/) skip=1
      if (dev ~ /^\/dev\/loop/) skip=1
      if (dev ~ /^\/dev\/mapper\/docker/) skip=1
      if (skip && mnt!="/") next
      # 设备名作为第 6 字段输出：伪文件系统（含 overlay:xxx、loop、docker mapper）留空。
      devout=dev
      if (dev ~ /^(overlay|tmpfs|devtmpfs|udev|none|rootfs|ramfs|shm|proc|sysfs|devpts|squashfs|cgroup|cgroup2|efivarfs|debugfs|tracefs|configfs|securityfs|pstore|bpf|hugetlbfs|mqueue|nsfs)/) devout=""
      if (dev ~ /^\/dev\/loop/ || dev ~ /^\/dev\/mapper\/docker/) devout=""
      gsub(/[,;]/, "_", devout)
      key=dev
      sub(/^.*\//, "", key)
      r=0; w=0
      if (key in rd) { r=int(rd[key]/2); w=int(wr[key]/2) }
      n++
      printf "%s%s,%.0f,%.0f,%.0f,%.0f,%s", (n>1?";":""), mnt, total, avail, r, w, devout
    }
  '
}

net_info() {
  # beta2.3：输出 /proc/net/dev 里的全部接口（排除 lo），并附带 operstate。
  # 旧版前端只取第一篇（通常是 eth0），多网卡主机的 eth1/br-lan/utun 等全被丢掉。
  # operstate 读 /sys/class/net/<iface>/operstate；读不到就留空（老内核/容器里可能没有）。
  awk '
    NR>2 {
      line=$0
      sub(/^[ \t]+/, "", line)
      i=index(line, ":")
      if (i<=0) next
      iface=substr(line, 1, i-1)
      if (iface=="lo" || iface=="") next
      n=split(substr(line, i+1), f, " ")
      if (n<9) next
      state=""
      sf="/sys/class/net/" iface "/operstate"
      if ((getline st < sf) > 0) {
        gsub(/[^a-z]/, "", st)
        state=st
      }
      close(sf)
      cnt++
      # 累计字节很容易超过 2^31（eth0/eth1 跑几天就到 GB 级），必须用 %.0f。
      printf "%s%s,%.0f,%.0f,%s", (cnt>1?";":""), iface, f[1]+0, f[9]+0, state
    }
  ' /proc/net/dev 2>/dev/null
}

thermal_info() {
  sep=""
  for z in /sys/class/thermal/thermal_zone*; do
    [ -r "$z/type" ] || continue
    [ -r "$z/temp" ] || continue
    ty=$(tr -d ',;\t ' < "$z/type" 2>/dev/null)
    te=$(tr -d '\t ' < "$z/temp" 2>/dev/null)
    [ -n "$ty" ] || continue
    case "${te#-}" in ''|*[!0-9]*) continue ;; esac
    printf '%s%s,%s' "$sep" "$ty" "$te"
    sep=";"
  done
}

fan_info() {
  # 风扇转速（RPM，beta2.1）：/sys/class/hwmon/hwmon*/fan*_input。
  # 没装 hwmon 驱动（很多路由器是被动散热）就没有这些文件，整段静默省略。
  # 名字优先用 fan*_label，缺省用「芯片名:fan序号」；协议分隔符一律清洗掉。
  sep=""
  for h in /sys/class/hwmon/hwmon*; do
    [ -d "$h" ] || continue
    chip=$(tr -d ',;|\t ' < "$h/name" 2>/dev/null)
    for f in "$h"/fan*_input; do
      [ -r "$f" ] || continue
      rpm=$(tr -d '\t ' < "$f" 2>/dev/null)
      case "${rpm#-}" in ''|*[!0-9]*) continue ;; esac
      fid=$(basename "$f" | sed 's/_input$//')
      label=""
      lf="$h/${fid}_label"
      # 很多 hwmon 没有 fan*_label；先判断可读，避免每秒往 SSH stderr 刷 "can't open"。
      [ -r "$lf" ] && label=$(tr ',;|\t ' '     ' < "$lf" 2>/dev/null)
      [ -n "$label" ] || label="${chip:-hwmon}:$fid"
      printf '%s%s,%s' "$sep" "$label" "$rpm"
      sep=";"
    done
  done
}

# ---------- GPU（beta2.4：可选段，NVIDIA / AMD / Intel 通吃，没有显卡就完全不输出） ----------
# NVIDIA（专有驱动）：nvidia-smi 一步拿到名称/占用/显存/温度/功耗；
# AMD(amdgpu) / Intel(i915,xe) / nouveau 走 sysfs：
#   device/gpu_busy_percent   —— i915 4.14+ / xe / amdgpu 都有（0-100）
#   device/mem_info_vram_*    —— 仅 amdgpu（字节）
#   device/hwmon/hwmon*/temp1_input —— 摄氏毫度；power1_average / power1_input —— 微瓦
# 名称优先 product_name（新内核 amdgpu 有），缺省用「厂商 (cardN)」；
# 全部数值经过数字校验，某块卡一个有效读数都没有就不上报（过滤 virtio 等虚拟卡）。
gpu_num() {
  case "${1#-}" in ''|*[!0-9.]*) return 1 ;; esac
  printf '%s' "$1"
}

gpu_hwmon_field() {
  # $1 = 卡的 device 目录；$2 = 1 取温度（毫度）/ 2 取功耗（微瓦）；读不到输出空
  want=$2
  for hw in "$1"/hwmon/hwmon*; do
    [ -d "$hw" ] || continue
    if [ "$want" = "1" ] && [ -r "$hw/temp1_input" ]; then
      read -r v < "$hw/temp1_input" 2>/dev/null
      [ -n "$v" ] && { printf '%s' "$v"; return; }
    fi
    if [ "$want" = "2" ]; then
      for pf in power1_average power1_input; do
        [ -r "$hw/$pf" ] || continue
        read -r v < "$hw/$pf" 2>/dev/null
        [ -n "$v" ] && { printf '%s' "$v"; return; }
      done
    fi
  done
}

nvidia_gpu_info() {
  command -v nvidia-smi >/dev/null 2>&1 || return 0
  nvidia-smi --query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw \
    --format=csv,noheader,nounits 2>/dev/null | awk -F, '
    function clean(s) {
      gsub(/[ \t]/, "", s)
      gsub(/\[?N\/A\]?/, "", s)
      gsub(/\[Not.*/, "", s)
      return s
    }
    BEGIN { sep = "" }
    {
      nm = $1
      gsub(/^[ \t]+|[ \t]+$/, "", nm)
      gsub(/[,;|]/, "_", nm)
      if (nm == "") next
      printf "%s%s,nvidia,%s,%s,%s,%s,%s", sep, nm, clean($2), clean($3), clean($4), clean($5), clean($6)
      sep = ";"
    }'
}

sysfs_gpu_info() {
  sep=""
  for card in /sys/class/drm/card[0-9]*; do
    [ -d "$card" ] || continue
    b=${card##*/}
    case "$b" in card[0-9]|card[0-9][0-9]) ;; *) continue ;; esac
    drv=$(basename "$(readlink "$card/device/driver" 2>/dev/null)" 2>/dev/null)
    case "$drv" in amdgpu|i915|xe|nouveau|radeon) ;; *) continue ;; esac

    busy=""; mu=""; mt=""
    if [ -r "$card/device/gpu_busy_percent" ]; then
      read -r busy < "$card/device/gpu_busy_percent" 2>/dev/null
    fi
    if [ -r "$card/device/mem_info_vram_used" ]; then
      read -r mu < "$card/device/mem_info_vram_used" 2>/dev/null
    fi
    if [ -r "$card/device/mem_info_vram_total" ]; then
      read -r mt < "$card/device/mem_info_vram_total" 2>/dev/null
    fi
    te=$(gpu_hwmon_field "$card/device" 1)
    pw=$(gpu_hwmon_field "$card/device" 2)

    busy=$(gpu_num "$busy" 2>/dev/null) || busy=""
    mu=$(gpu_num "$mu" 2>/dev/null) || mu=""
    mt=$(gpu_num "$mt" 2>/dev/null) || mt=""
    te=$(gpu_num "$te" 2>/dev/null) || te=""
    pw=$(gpu_num "$pw" 2>/dev/null) || pw=""
    # 一块卡至少要有一个有效读数才上报
    [ -n "$busy$mu$te$pw" ] || continue

    # 单位换算：显存 B→MiB，温度 毫度→℃，功耗 µW→W；缺的项输出 "-"
    set -- $(awk -v u="$mu" -v t="$mt" -v te="$te" -v pw="$pw" 'BEGIN{
      if (u != "" && t != "" && t+0 > 0) printf "%.0f %.0f ", u/1048576, t/1048576; else printf "- - "
      if (te != "") printf "%.1f ", te/1000; else printf "- "
      if (pw != "") printf "%.1f", pw/1000000
    }')
    mu=$1; mt=$2; te=$3; pw=$4
    [ "$mu" = "-" ] && mu=""
    [ "$mt" = "-" ] && mt=""
    [ "$te" = "-" ] && te=""
    [ "$pw" = "-" ] && pw=""

    nm=""
    if [ -r "$card/device/product_name" ]; then
      # 只替换协议分隔符，保留名称里的空格（如 "AMD Radeon RX 7600"）
      nm=$(sed -e 's/[;,|]/_/g' -e 's/^[ \t]*//' -e 's/[ \t]*$//' "$card/device/product_name" 2>/dev/null)
    fi
    case "$drv" in
      amdgpu)        [ -n "$nm" ] || nm="AMDGPU" ;;
      radeon)        [ -n "$nm" ] || nm="AMD radeon" ;;
      i915|xe)       nm="Intel ${drv}" ;;
      nouveau)       [ -n "$nm" ] || nm="NVIDIA nouveau" ;;
    esac
    [ -n "$nm" ] || nm="$drv"
    printf '%s%s,%s,%s,%s,%s,%s,%s' "$sep" "$nm ($b)" "$drv" "$busy" "$mu" "$mt" "$te" "$pw"
    sep=";"
  done
}

gpu_info() {
  out=""
  sep=""
  r=$(nvidia_gpu_info)
  if [ -n "$r" ]; then
    out="$r"
    sep=";"
  fi
  r=$(sysfs_gpu_info)
  if [ -n "$r" ]; then
    out="$out$sep$r"
  fi
  printf '%s' "$out"
}

emit_once() {
  docker_tick
  out="T=$(date +%s)"
  v=$(uptime_info);  [ -n "$v" ] && out="$out|U=$v"
  v=$(load_info);    [ -n "$v" ] && out="$out|L=$v"
  v=$(cpu_info);     [ -n "$v" ] && out="$out|CPU=$v"
  v=$(mem_info);     [ -n "$v" ] && out="$out|MEM=$v"
  v=$(disk_info);    [ -n "$v" ] && out="$out|DSK=$v"
  v=$(net_info);     [ -n "$v" ] && out="$out|NET=$v"
  v=$(thermal_info); [ -n "$v" ] && out="$out|TMP=$v"
  v=$(fan_info);     [ -n "$v" ] && out="$out|FAN=$v"
  v=$(gpu_info);     [ -n "$v" ] && out="$out|GPU=$v"
  v=$(os_info);      [ -n "$v" ] && out="$out|OS=$v"
  v=$(docker_emit);  [ -n "$v" ] && out="$out|$v"
  printf '%s\n' "$out"
}

trap 'exit 0' INT TERM HUP

case "$1" in
  --once)
    emit_once
    ;;
  *)
    while :; do
      emit_once
      sleep "$INTERVAL"
    done
    ;;
esac
