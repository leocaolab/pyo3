#!/usr/bin/env bash
# 编出被测矩阵:{对照, 本分支} × {默认, abi3-py310} × {无子模块, 有子模块}
#
# 构建维度和行为维度一样重要。这套基准原本只覆盖「默认 ABI + 顶层模块」,
# 于是 abi3 下编不过、slot 发不出、占位符泄漏到子模块这三个 bug,
# 全是由第一个真实消费者(polars)免费抓到的,不是这里抓到的。
set -e
cd "$(dirname "$0")/probe"

BASE="${BASE:-/tmp/pyo3-base}"     # 本分支的父提交,由 git worktree 准备
FORK="${FORK:-../..}"
[ -d "$BASE" ] || { echo "缺 $BASE —— 先 git worktree add $BASE dfdbc46"; exit 1; }

case "$(uname)" in
  Darwin) EXT=dylib; export RUSTFLAGS="-C link-arg=-undefined -C link-arg=dynamic_lookup" ;;
  *)      EXT=so ;;
esac

one () {                            # one <输出目录> <pyo3路径> [额外 feature...]
  local out="$1" path="$2"; shift 2
  local feats=""; [ $# -gt 0 ] && feats="--features $(IFS=,; echo "$*")"
  cargo build --release $feats --config "patch.crates-io.pyo3.path=\"$path\"" >/dev/null 2>&1 \
    || { echo "  ✗ $out 编译失败"; cargo build --release $feats \
           --config "patch.crates-io.pyo3.path=\"$path\"" 2>&1 | grep -E '^error' -A6 | head -12; return 1; }
  rm -rf "../$out" && mkdir -p "../$out"
  cp "target/release/libabi3t.$EXT" "../$out/abi3t.so"
  echo "  ✓ $out"
}

one so_base       "$BASE"
one so_fork       "$FORK"
one so_base_abi3  "$BASE" abi3
one so_fork_abi3  "$FORK" abi3
one sm_base       "$BASE" submodule
one sm_fork       "$FORK" submodule
one sm_base_abi3  "$BASE" submodule abi3
one sm_fork_abi3  "$FORK" submodule abi3
