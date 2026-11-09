#!/usr/bin/env bash
# §6-2 local repro: a striped object with an INTERIOR chunk removed must be
# announced as damaged by `grovefs du` (was: silent under-report 40MiB -> 16MiB).
set -u
cd "$(dirname "$0")/.."
Y=./target/release/grovefs
A=127.0.0.1:15141
B=127.0.0.1:15142
D=64 # data token: exercise the authed path like production does

rm -rf .d62; mkdir -p .d62/home .d62/sA .d62/sB .d62/mnt
export GROVEFS_HOME=$PWD/.d62/home

$Y data-server --listen $A --data-dir $PWD/.d62/sA > .d62/sA.log 2>&1 &
PA=$!
$Y data-server --listen $B --data-dir $PWD/.d62/sB > .d62/sB.log 2>&1 &
PB=$!
sleep 0.8
$Y init --view alice > /dev/null
$Y mount --view alice --mountpoint .d62/mnt --data-servers "$A,$B" --foreground > .d62/mnt.log 2>&1 &
PM=$!
sleep 1.2
cleanup(){ fusermount -u .d62/mnt 2>/dev/null; kill $PM $PA $PB 2>/dev/null; wait 2>/dev/null; }
trap cleanup EXIT

echo "=== write 40MiB (chunks 0,1,2 -> A#0 B#1 A#2) ==="
dd if=/dev/urandom of=.d62/mnt/big.bin bs=1M count=40 2>/dev/null
sync .d62/mnt/big.bin
sleep 0.5

echo "=== baseline du (intact) ==="
$Y du --view alice --home $PWD/.d62/home --data-servers "$A,$B" --bytes --total 2>&1 | head -3

echo "=== kill servers, remove chunk #1 (on B), restart ==="
fusermount -u .d62/mnt; kill $PA $PB; wait 2>/dev/null
ls .d62/sB/blocks/*/* 2>/dev/null | tee .d62/removed.txt
rm -f $(cat .d62/removed.txt)
$Y data-server --listen $A --data-dir $PWD/.d62/sA > .d62/sA2.log 2>&1 &
PA=$!
$Y data-server --listen $B --data-dir $PWD/.d62/sB > .d62/sB2.log 2>&1 &
PB=$!
sleep 0.8

echo "=== du after damage (must announce damaged, not silently report 16MiB) ==="
$Y du --view alice --home $PWD/.d62/home --data-servers "$A,$B" --bytes --total
rc=$?
echo "rc=$rc"

echo "=== also: missing chunk #2 (the tail) stays a normal short read, not damage ==="
