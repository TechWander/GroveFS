#!/bin/bash
# Ephemeral mounts to validate fork(content_ro) + lens(read-only) semantics on the live cluster; auto-cleans.
set -uo pipefail
G=/usr/local/bin/grovefs
DATA=gfs1:50100,gfs2:50100; META=127.0.0.1:50053
F=/home/hanjun/m_fork; L=/home/hanjun/m_lens
mkdir -p $F $L
echo "=== caps echo from fork mount (expect WRITEBACK=false, ATOMIC_O_TRUNC=true) ==="
$G mount --view team-fork -m $F --data-servers $DATA --meta-server $META --token "$GROVEFS_TOKEN" --foreground > /tmp/f.log 2>&1 &
FP=$!; $G mount --view nomore -m $L --data-servers $DATA --meta-server $META --token "$GROVEFS_TOKEN" --foreground > /tmp/l.log 2>&1 &
LP=$!
sleep 3
grep -h "caps granted" /tmp/f.log /tmp/l.log
echo "=== FORK: existing content read OK, content write/truncate => EROFS, mkdir OK ==="
ls $F | head -3
echo "read alice file: $(head -c 14 $F/README.txt 2>&1)"
if echo "nope" >> $F/README.txt 2>/tmp/e1; then echo "  append: UNEXPECTED OK"; else echo "  append refused: $(cat /tmp/e1)"; fi
if : > $F/README.txt 2>/tmp/e2; then echo "  '>' truncate: UNEXPECTED OK"; else echo "  truncate refused: $(cat /tmp/e2)"; fi
if mkdir $F/newdir 2>/tmp/e3 && echo x > $F/newdir/own.txt 2>/tmp/e3b; then echo "  fork new content (mkdir+write fresh name): OK ($(cat $F/newdir/own.txt))"; else echo "  fork-write err: $(cat /tmp/e3 /tmp/e3b 2>/dev/null)"; fi
echo "=== LENS(derived): read-only, hidden rules applied ==="
echo "  has datasets dir? $(ls $L | grep -c datasets) (expect 0, hidden)"
echo "  has README.txt? $(ls $L | grep -c README.txt) (expect 0, hidden)"
if echo x > $L/anything 2>/tmp/el; then echo "  lens write: UNEXPECTED OK"; else echo "  lens write refused (read-only): $(cat /tmp/el)"; fi
fusermount -u $F 2>/dev/null; fusermount -u $L 2>/dev/null; kill $FP $LP 2>/dev/null; wait 2>/dev/null
rmdir $F $L 2>/dev/null
echo "=== cleaned scratch mounts ==="
