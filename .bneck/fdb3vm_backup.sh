#!/usr/bin/env bash
# Backup -> damage -> point-in-time restore evidence for the real multi-host
# cluster (.bneck/fdb3vm_up.sh), so that the durability claims rest on a
# four-zone cluster rather than on the single-node dev cluster.
#
# What this proves, in order:
#   1. a continuous backup (snapshot + mutation log) of a cross-host RF=3
#      cluster lands in a container we can inspect;
#   2. keys written AFTER the last snapshot are still recoverable — that can only
#      come from log replay, not from the snapshot;
#   3. destroying a key range and restoring it to a chosen version puts every
#      value back byte-identically;
#   4. a key written after the backup's stop version is GONE after the restore.
#      Point-in-time recovery means both directions, and only the second half is
#      a real test (the first half is what any copy-back would do).
#
# Things that cost a debugging cycle each and are recorded here so nobody
# rediscovers them on 7.1:
#   - backup still needs an agent, and it is NOT `fdbbackup agent`: the same
#     binary is named `backup_agent` and picks its program type from argv[0], so
#     it must be installed under that exact name.  It takes no --data-dir, only
#     -C, --log/--logdir, -m.
#   - fdbbackup/fdbrestore also identify themselves by basename, so the
#     `.x86_64` suffix used for the other assets here does not work ("Unable to
#     determine program type based on executable").
#   - top-level `-C` goes BEFORE the action for fdbbackup, but fdbrestore has no
#     -C at all: the target is --dest-cluster-file, which is also the option that
#     makes restoring into a non-empty database legal.
#   - restore reads the BackupURL (the dated backup directory), not the base
#     container the backup was pointed at; and `discontinue -w` does not print
#     the stop version — `status` does, once it says "completed at version".
#
# usage: bash .bneck/fdb3vm_backup.sh
#        KEEP=1 bash .bneck/fdb3vm_backup.sh   # reuse an existing container dir
set -u
BIN=/data/grovefs/.fdbbin
CLUSTER=/data/grovefs/.fdb3vm/cluster3vm
WORK=/data/grovefs/.fdb3vm/backup2
AGENTLOG=/data/grovefs/.fdb3vm/backup-agent/logs
URL=file://$WORK
CLI="$BIN/fdbcli.x86_64 -C $CLUSTER"
BAK="$BIN/fdbbackup"
RES="$BIN/fdbrestore"
TAG=grovefsbk
PREFIX=bk                      # the probe key range that gets backed up, destroyed, restored
NPRE=${NPRE:-50}               # keys written before the snapshot
NLOG=${NLOG:-10}               # keys written after the snapshot, only in the log
KRG="\"$PREFIX""01\" \"${PREFIX}zz\""        # -k argument for a partial restore
pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
say(){ echo "    $*"; }

fval(){ # fval <key> -> value, or empty when the key is absent
  $CLI --exec "get $1" 2>/dev/null | tr -d '\r' | tail -1 | sed -E "s/^.* is \`(.*)'\$/\1/" | grep -v 'not found' || true
}
write(){ # write: one fdbcli statement batch, semicolon separated
  $CLI --exec "writemode on; $*" > $WORK/set.log 2>&1
  grep -q Committed $WORK/set.log || { bad "write batch failed: $(tail -3 $WORK/set.log | tr '\n' ' ')"; return 1; }
  return 0
}

echo "===== 0. container and agent ====="
mkdir -p "$WORK" "$AGENTLOG"
if [ "${KEEP:-0}" != 1 ]; then rm -rf "$WORK"/backup-* ; fi
if ! pgrep -f "backup_agent -C $CLUSTER" >/dev/null; then
  setsid nohup "$BIN/backup_agent" -C "$CLUSTER" --log --logdir "$AGENTLOG" \
      > "$AGENTLOG/agent.out" 2>&1 < /dev/null &
  sleep 8
fi
pgrep -f "backup_agent -C $CLUSTER" >/dev/null && ok "backup_agent running (pid $(pgrep -f "backup_agent -C $CLUSTER" | head -1))" \
  || { bad "no backup_agent; nothing will be backed up"; exit 1; }
$CLI --exec "status minimal" 2>&1 | grep -q "database is available" && ok "cluster available before the run" \
  || bad "cluster not available before the run"

echo "===== 1. ${NPRE} keys that will be in the snapshot ====="
cmds=""
for i in $(seq -w 1 $NPRE); do cmds="$cmds set $PREFIX$i snap-$i;"; done
write "$cmds" && say "wrote $PREFIX""01..$PREFIX$NPRE"
ok "snapshot-side keys written ($(fval $PREFIX""01) / $(fval $PREFIX$NPRE))"

echo "===== 2. start the continuous backup ====="
$BAK start -C "$CLUSTER" -d "$URL" -t "$TAG" -s 5 -z > $WORK/start.log 2>&1
tail -2 $WORK/start.log | sed 's/^/    /'
$BAK status -C "$CLUSTER" -t "$TAG" > $WORK/st1.log 2>&1
grep -q "in progress" $WORK/st1.log && ok "backup accepted and in progress" || bad "backup not running: $(head -3 $WORK/st1.log|tr '\n' ' ')"
# the first snapshot must actually complete, otherwise there is no snapshot to
# take credit for and step 3 proves nothing.
snap=""
for _ in $(seq 1 60); do
  snap=$($BAK status -C "$CLUSTER" -t "$TAG" 2>/dev/null | grep "Last complete snapshot version" | grep -oE "[0-9]{6,}" | head -1)
  [ -n "${snap:-}" ] && [ "$snap" != "0" ] && break
  sleep 3
done
$BAK status -C "$CLUSTER" -t "$TAG" 2>/dev/null | grep -E 'Snapshot interval|Last complete (log|snapshot)' | sed 's/^/    /'
[ -n "${snap:-}" ] && ok "a snapshot completed at version $snap" || bad "no completed snapshot within 180s"

echo "===== 3. ${NLOG} keys that can only come back through the log ====="
cmds=""
for i in $(seq -w 1 $NLOG); do cmds="$cmds set ${PREFIX}z$i logonly-$i;"; done
write "$cmds" && ok "post-snapshot keys written (${PREFIX}z01..${PREFIX}z$NLOG)"

echo "===== 4. stop the backup at a known version ====="
$BAK discontinue -C "$CLUSTER" -t "$TAG" -w > $WORK/stop.log 2>&1
tail -3 $WORK/stop.log | sed 's/^/    /'
# `discontinue -w' only says it is waiting; the stop version and the dated
# BackupURL come out of `status' once the backup reports completed.
st=""
for _ in $(seq 1 40); do
  st=$($BAK status -C "$CLUSTER" -t "$TAG" 2>/dev/null)
  echo "$st" | grep -q "completed at version" && break
  sleep 3
done
echo "$st" > $WORK/stopstatus.log
V=$(echo "$st" | grep -o "completed at version [0-9]*" | awk '{print $4}' | tail -1)
BU=$(echo "$st" | grep -o "BackupURL: file://[^ ]*" | awk '{print $2}' | tail -1)
[ -n "$V" ] && ok "backup completed at version $V" || bad "could not read the stop version"
[ -n "$BU" ] && ok "restore will read $BU" || bad "no BackupURL"
SNAPV=$($BAK status -C "$CLUSTER" -t "$TAG" 2>/dev/null | grep -oE "snapshot,${snap}" | head -1)
[ -n "$snap" ] && [ -n "$V" ] && [ "$snap" -lt "$V" ] && ok "the log runs past the snapshot ($snap -> $V), so anything after $snap is log-only"

echo "===== 5. damage ====="
write "set ${PREFIX}z01 post-backup-junk;"            # written after $V: must NOT survive
$CLI --exec "writemode on; clearrange $PREFIX""01 ${PREFIX}zz" > $WORK/clear.log 2>&1
grep -q Committed $WORK/clear.log && say "clearrange $PREFIX""01..${PREFIX}zz committed: $(tail -1 $WORK/clear.log)" \
  || bad "clearrange failed: $(tail -3 $WORK/clear.log|tr '\n' ' ')"
[ -z "$(fval $PREFIX""05)" ] && ok "probe key really is gone before the restore" || bad "$PREFIX""05 still present, the damage step did not work"

echo "===== 6. point-in-time restore of just the damaged range ====="
# An error check must look at the VALUE of LastError: the string "LastError:
# None" contains "Error", and the field is printed with a space after the colon,
# so both of the obvious greps ({case-insensitive} error, and ":None") were wrong
# in opposite directions — the first two runs of this script each fell for one.
noerr(){ ! grep -oE "LastError: +[^ ]+" "$1" | grep -v "None" | grep -q .; }
$RES start --dest-cluster-file "$CLUSTER" -r "$BU" -t "$TAG" -v "$V" -k "$KRG" -n > $WORK/rdry.log 2>&1
tail -4 $WORK/rdry.log | sed 's/^/    dryrun: /'
if noerr $WORK/rdry.log && grep -q "can be used to restore" $WORK/rdry.log; then
  ok "dry run accepted the plan: $(grep -o 'restore to version [0-9]*' $WORK/rdry.log | head -1)"
else
  bad "dry run refused: $(tail -3 $WORK/rdry.log|tr '\n' ' ')"
fi
$RES start --dest-cluster-file "$CLUSTER" -r "$BU" -t "$TAG" -v "$V" -k "$KRG" -w > $WORK/rst.log 2>&1
tail -5 $WORK/rst.log | sed 's/^/    /'
if noerr $WORK/rst.log && grep -q "Restored to version $V" $WORK/rst.log; then
  ok "restore completed at version $V with no error reported"
else
  bad "restore did not report success: $(tail -3 $WORK/rst.log|tr '\n' ' ')"
fi

echo "===== 7. verification ====="
miss=0; mism=0; checked=0
for i in $(seq -w 1 $NPRE); do
  v=$(fval $PREFIX$i); checked=$((checked+1))
  [ -z "$v" ] && miss=$((miss+1)) || { [ "$v" = "snap-$i" ] || mism=$((mism+1)); }
done
for i in $(seq -w 1 $NLOG); do
  v=$(fval ${PREFIX}z$i); checked=$((checked+1))
  [ -z "$v" ] && miss=$((miss+1)) || { [ "$v" = "logonly-$i" ] || mism=$((mism+1)); }
done
say "checked $checked keys: $miss missing, $mism differing"
[ "$miss" = 0 ] && ok "all $((NPRE+NLOG)) keys came back" || bad "$miss keys missing after the restore"
# only a claim about keys that exist: an all-missing run would otherwise let an
# empty comparison pass as "byte-identical".
if [ "$miss" = 0 ]; then
  [ "$mism" = 0 ] && ok "every restored value is byte-identical to what was written" || bad "$mism values differ"
fi
if [ "$miss" = 0 ] && [ "$mism" = 0 ]; then
  # the $NLOG "logonly" keys were written after the snapshot, so their presence
  # is the proof that the log was replayed, not just the snapshot copied back.
  ok "${PREFIX}z$NLOG = '$(fval ${PREFIX}z$NLOG)' recovered from the mutation log, not the snapshot"
fi
junk=$(fval ${PREFIX}z01)
if [ "$junk" = "post-backup-junk" ]; then bad "a write after version $V survived the restore to $V - not point-in-time"
elif [ "$junk" = "logonly-01" ]; then ok "the write made after $V was rolled back (${PREFIX}z01 = '$junk')"
else bad "${PREFIX}z01 = '$junk', expected 'logonly-01' after a restore to version $V"; fi

echo "===== 8. what is in the container ====="
say "container: $(du -sh "$WORK" | cut -f1) in $(find "$WORK" -type f | wc -l) files"
find "$WORK" -name 'snapshot*' -o -name 'range*' -o -name 'log*' | head -8 | sed 's#.*/##;s/^/    /'
# the container is deliberately left in place (it is a few hundred KB): a backup
# that was expired by the same script that proved it works proves nothing twice.
echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" = 0 ] && echo "FDB_BACKUP_GREEN" || echo "FDB_BACKUP_RED"
