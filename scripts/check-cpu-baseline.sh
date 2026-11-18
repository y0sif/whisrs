#!/usr/bin/env bash
# Fail if a binary uses instructions above the release builds' CPU floor.
#
# ggml builds with -march=native (-mcpu=native on ARM) unless GGML_NATIVE=OFF,
# so an unpinned release inherits the runner's ISA and SIGILLs on every CPU
# without it. The floor and what crosses it, by ELF machine:
#   x86_64, x86-64-v3: AVX-512, seen as %zmm registers, %k0-7 mask registers,
#     the EVEX-only %xmm16-31/%ymm16-31, embedded broadcasts ({1toN}), or
#     EVEX-only mnemonics on low ymm/xmm registers.
#   aarch64, armv8.2-a+dotprod+fp16: SVE/SVE2/SME, seen as z0-z31 or p0-p15
#     operands or the SVE-only scalar ops (cnt/inc/dec[bhwd], addvl, addpl,
#     rdvl); i8mm and bf16, seen as their mnemonics.
#
# Usage: scripts/check-cpu-baseline.sh [binary...]
#        (default: target/release/whisrsd target/release/whisrs)
# OBJDUMP sets the disassembler (default objdump). On an x86_64 host, check an
# aarch64 binary with OBJDUMP=llvm-objdump.
set -euo pipefail

x86_64_pattern='%zmm|%k[0-7]\b|%[xy]mm(1[6-9]|2[0-9]|3[01])\b|\{1to[0-9]+\}|\bvpternlog|\bvpermt2|\bvpro[lr][dq]\b'
aarch64_pattern='\bz([0-9]|[12][0-9]|3[01])\b|\bp([0-9]|1[0-5])[./]|\b(cnt|inc|dec)[bhwd]\b|\b(addvl|addpl|rdvl)\b|\b(smmla|ummla|usmmla|usdot|sudot)\b|\b(bfdot|bfmmla|bfmlal[bt]|bfcvt[a-z0-9]*)\b'
objdump=${OBJDUMP:-objdump}

[ "$#" -gt 0 ] || set -- target/release/whisrsd target/release/whisrs

dis=$(mktemp)
trap 'rm -f "$dis"' EXIT

failed=0
for bin in "$@"; do
  [ -f "$bin" ] || { echo "::error::$bin not found"; exit 1; }
  machine=$(readelf -h "$bin" 2>/dev/null | grep 'Machine:') || true
  case $machine in
    *X86-64*) pattern=$x86_64_pattern isa='AVX-512' cpus='x86_64' ;;
    *AArch64*) pattern=$aarch64_pattern isa='SVE/SME, i8mm or bf16' cpus='ARM' ;;
    *) echo "::error::$bin is not an x86_64 or aarch64 ELF binary"; exit 1 ;;
  esac
  # Keep only instruction lines, without <symbol> annotations, so a path or a
  # symbol name cannot match. Written to a file so an objdump failure fails
  # the check instead of reading as 0.
  "$objdump" -d "$bin" | sed -nE '/^ *[0-9a-f]+:/{s/<[^>]*>//g;p}' > "$dis"
  # grep exits 1 on zero matches; anything above that is a real error.
  rc=0
  count=$(grep -cE "$pattern" "$dis") || rc=$?
  [ "$rc" -le 1 ] || { echo "::error::grep failed on $bin"; exit 1; }
  echo "$isa instructions in $bin: $count"
  if [ "$count" -ne 0 ]; then
    echo "::error::$bin requires $isa and would SIGILL on most $cpus CPUs"
    failed=1
  fi
done
exit "$failed"
