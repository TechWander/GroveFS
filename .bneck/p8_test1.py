#!/usr/bin/env python3
# Spec P8 leaves one judgement un-implemented in test_fdb.sh §12:
#   "id/ino 保留原值（… 既有 pitfall：`.grove-stats` 页缓存会让 A/B 测量失真 →
#    迁移后验证须先失效缓存）"
# 12a compared the hard-link PAIR against itself and 12b diffed the dumps, so the
# mount-side numbers were never pinned to the SOURCE's exported `ino`, and nothing
# re-read a number after the attr/dentry TTL had expired. A cached answer can only
# ever make a re-issued inode look unchanged, so both gaps are closed here:
#   * 12a: `stat -c %i` through the mount must equal the dump's "ino" for that name
#     (the dump is written by an offline CLI call no mount cache can influence, so
#     it is the authority);
#   * 12c: after the post-import create, wait past the <=2s TTL and re-read the same
#     two numbers from the same mount.
F = '/data/grovefs/grovefs/.build-bin/test_fdb.sh'

A_ANCHOR = 'chk "$(T stat -c %a $MF/docs/a.md)" "640" "mode travelled"'
A_BLOCK = r'''
# The inode numbers are pinned to the DUMP's own "ino" values, not merely to each
# other. That is the difference between "the hard link stayed one inode" (which a
# re-issued pair would also satisfy) and "the client keeps the inode it already
# cached handles for" — Spec P8's reason for preserving id/ino at all. The dump is
# written by an offline CLI call, so no mount's attribute cache can influence it:
# it is the authority for what the source numbers were.
AINO=$(grep '"name":"a.md"' $S/m_src.jsonl | grep -o '"ino":[0-9]*' | cut -d: -f2)
NINO=$(grep '"name":"notes.txt"' $S/m_src.jsonl | grep -o '"ino":[0-9]*' | cut -d: -f2)
[ -n "$AINO" ] && [ -n "$NINO" ] && ok "the source dump carries an ino for both names ($AINO/$NINO)" \
    || bad "could not read inos out of the source dump — the assertion below would be vacuous"
chk "$(T stat -c %i $MF/docs/a.md)" "$AINO" "the mount answers the SOURCE's inode for docs/a.md, not a re-issued one"
chk "$(T stat -c %i $MF/notes.txt)" "$NINO" "the mount answers the SOURCE's inode for notes.txt"'''

B_ANCHOR = 'sync "$MF" 2>/dev/null; sleep 1.5'
B_BLOCK = r'''
# Cache discipline (Spec P8 names the pitfall directly: a cached page of
# `.grove-stats` / a cached attribute can make an A/B measurement read as
# unchanged). This mount answers from the cluster and caches attrs for <=2s, so the
# SAME two numbers are re-read after waiting past that bound. An inode that only
# LOOKED preserved because the client still held the old answer would move here;
# the SQLite side of the round trip is mounted --no-cache for the same reason.
sleep 1.5
chk "$(T stat -c %i $MF/docs/a.md)" "$AINO" "docs/a.md still answers the source inode after the attr TTL expired"
chk "$(T stat -c %i $MF/notes.txt)" "$NINO" "notes.txt likewise — the number is the cluster's, not a cache hit"'''


def main():
    lines = open(F, encoding='utf-8').read().split('\n')
    assert 'the SOURCE\'s inode for docs/a.md' not in '\n'.join(lines), 'already applied'
    for anchor, block in ((A_ANCHOR, A_BLOCK), (B_ANCHOR, B_BLOCK)):
        idx = [i for i, l in enumerate(lines) if l == anchor]
        assert len(idx) == 1, 'anchor %r hit %d' % (anchor[:48], len(idx))
        lines[idx[0] + 1:idx[0] + 1] = block.split('\n')
    open(F, 'w', encoding='utf-8').write('\n'.join(lines))
    print('test_fdb.sh: §12 inode-vs-dump and post-TTL re-read assertions added')


main()
