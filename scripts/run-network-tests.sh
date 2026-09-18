#!/usr/bin/env bash
# 在受限内存下运行联网测试。
#
# 背景：损坏/被截断的音频文件曾让解码路径申请十几 GB 内存，触发内核 OOM，
# 把整机拖入 zram 交换风暴。本脚本用 systemd 的内存上限给进程加护栏，
# 超限时只杀测试进程，不会拖垮桌面。
#
# 用法：
#   scripts/run-network-tests.sh                       # 全部 #[ignore] 用例
#   scripts/run-network-tests.sh soda_lossless_track_download_is_decrypted
set -euo pipefail

cd "$(dirname "$0")/.."

if command -v systemd-run >/dev/null 2>&1; then
  exec systemd-run --user --scope -p MemoryMax=2G -p MemorySwapMax=1G -p CPUQuota=200% -- \
    cargo test --offline -j 2 -- --ignored --nocapture "$@"
fi

echo "systemd-run 不可用，退化为 ulimit（约 2GB 虚拟内存上限）" >&2
ulimit -v 2000000
exec cargo test --offline -j 2 -- --ignored --nocapture "$@"
