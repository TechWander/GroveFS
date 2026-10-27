#!/usr/bin/env bash
# Error-path & errno correctness for grovefs, asserted via *exact errno numbers*
# using python3 (locale-independent, unlike grepping localized tool messages).
#   RW mount : ENOTDIR / EISDIR / EEXIST / ENOENT / EINVAL / dir-hardlink -> EPERM
#   RO mount : EROFS on create/write/mkdir/unlink/rename/chmod/truncate/link/
#              symlink, while read/list/stat keep working.
set -u
cd /data/xy/grovefs
export PKG_CONFIG=/data/xy/grovefs/.build-bin/pkg-config
Y=./target/release/grovefs
rm -rf .xtest; mkdir -p .xtest/home .xtest/mnt .xtest/mnt_ro
export GROVEFS_HOME=$PWD/.xtest/home

cat > .xtest/man <<'EOF'
mkdir docs
touch docs/f.txt
touch victim.txt
EOF
$Y init --view erruser --manifest .xtest/man >/dev/null

pass=0; fail=0
ok(){ echo "  OK: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
chk(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }

# probe <python-snippet>: print the OSError.errno it raises, or NONE if it
# succeeds, or PYERR:<..> for a non-OSError (i.e. a harness bug, never a match).
probe(){ python3 -c '
import sys, os
try:
    exec(compile(sys.argv[1], "<probe>", "exec"), {"os": os})
    print("NONE")
except OSError as e:
    print(e.errno)
except Exception as e:
    print("PYERR:%r" % (e,))
' "$1"; }

# Linux errno constants
E_PERM=1; E_NOENT=2; E_EXIST=17; E_NOTDIR=20; E_ISDIR=21; E_INVAL=22; E_ROFS=30
export E_PERM E_NOENT E_EXIST E_NOTDIR E_ISDIR E_INVAL E_ROFS

MPRO=""
cleanup(){ fusermount -u .xtest/mnt 2>/dev/null; fusermount -u .xtest/mnt_ro 2>/dev/null; kill $MP ${MPRO:-} 2>/dev/null; }
trap cleanup EXIT

# ---------- RW mount ----------
$Y mount --view erruser --mountpoint .xtest/mnt --foreground > .xtest/mnt.log 2>&1 &
MP=$!
sleep 1.2
M=.xtest/mnt
echo "===== RW errno correctness ====="
chk "$(probe "open('$M/docs/f.txt','w').write('x')")" "NONE" "sanity: normal write on RW works"
chk "$(probe "os.mkdir('$M/docs/f.txt/sub')")" "$E_NOTDIR" "mkdir under a regular file => ENOTDIR"
chk "$(probe "os.rmdir('$M/docs/f.txt')")"      "$E_NOTDIR" "rmdir on a regular file => ENOTDIR"
chk "$(probe "os.open('$M/docs', os.O_WRONLY)")" "$E_ISDIR" "open(dir, O_WRONLY) => EISDIR"
chk "$(probe "os.open('$M/victim.txt', os.O_CREAT|os.O_EXCL|os.O_WRONLY)")" "$E_EXIST" "O_CREAT|O_EXCL on existing => EEXIST"
chk "$(probe "os.mkdir('$M/docs')")"            "$E_EXIST" "mkdir existing dir => EEXIST"
chk "$(probe "os.open('$M/nope', os.O_RDONLY)")" "$E_NOENT" "open missing file => ENOENT"
chk "$(probe "os.unlink('$M/nope')")"           "$E_NOENT" "unlink missing file => ENOENT"
chk "$(probe "os.readlink('$M/victim.txt')")"   "$E_INVAL" "readlink on non-symlink => EINVAL"
chk "$(probe "os.rename('$M/nope', '$M/other')")" "$E_NOENT" "rename missing src => ENOENT"
chk "$(probe "os.link('$M/docs', '$M/docs_hl')")" "$E_PERM" "hardlink of a directory => EPERM"

# ---------- RO mount (unmount RW first so the per-user DB isn't opened twice) ----------
fusermount -u $M 2>/dev/null; kill $MP 2>/dev/null; wait $MP 2>/dev/null
$Y mount --view erruser --mountpoint .xtest/mnt_ro --ro --foreground > .xtest/mnt_ro.log 2>&1 &
MPRO=$!
sleep 1.2
R=.xtest/mnt_ro
echo "===== RO mount: EROFS on mutations, reads still allowed ====="
chk "$(probe "len(os.listdir('$R'))")"             "NONE" "listdir allowed on RO mount"
chk "$(probe "os.stat('$R/victim.txt')")"          "NONE" "stat allowed on RO mount"
chk "$(probe "open('$R/docs/f.txt').read()")"      "NONE" "read file allowed on RO mount"
chk "$(probe "os.open('$R/new.txt', os.O_CREAT|os.O_WRONLY)")" "$E_ROFS" "create on RO => EROFS"
chk "$(probe "open('$R/victim.txt','w')")"         "$E_ROFS" "open(w) on RO => EROFS"
chk "$(probe "os.mkdir('$R/newdir')")"             "$E_ROFS" "mkdir on RO => EROFS"
chk "$(probe "os.unlink('$R/victim.txt')")"        "$E_ROFS" "unlink on RO => EROFS"
chk "$(probe "os.rename('$R/victim.txt','$R/ren.txt')")" "$E_ROFS" "rename on RO => EROFS"
chk "$(probe "os.chmod('$R/victim.txt', 0o600)")"  "$E_ROFS" "chmod on RO => EROFS"
chk "$(probe "os.truncate('$R/docs/f.txt', 0)")"   "$E_ROFS" "truncate on RO => EROFS"
chk "$(probe "os.link('$R/victim.txt','$R/hl')")"  "$E_ROFS" "hardlink create on RO => EROFS"
chk "$(probe "os.symlink('victim.txt','$R/sl')")"  "$E_ROFS" "symlink create on RO => EROFS"

echo
echo "==================== SUMMARY: pass=$pass fail=$fail ===================="
[ "$fail" -eq 0 ] && echo "ALL ERROR-PATH CHECKS PASSED" || echo "SOME ERROR-PATH CHECKS FAILED"
if [ "$fail" -eq 0 ]; then exit 0; else exit 1; fi
