import io, re, sys
p = '/data/grovefs/grovefs/.build-bin/test_fdb.sh'
s = io.open(p, encoding='utf-8').read()

anchor = 'MF=$S/mnt_m2\n'
assert s.count(anchor) == 1, 'anchor'
add = anchor + '''# The property the round trip used to break: `('i',1)` must point at the ROOT
# KEY `(ROOT_PARENT,"")`, not at the dump's portable name "/". A `'i` pointer
# that names a key which does not exist makes `getattr` of the mount root answer
# ENOENT for the whole tree, so this is asserted before the listing — and with
# the exact number, not "the path is there".
chk "$(T stat -c %i $MF 2>&1)" "1" "the imported subspace answers getattr on its own root inode"
'''
s = s.replace(anchor, add, 1)

# ls_wait's reason for existing is now measured, not guessed: keep the helper but
# say what it waits for.
old = """# ls_wait: list a mountpoint only once the path itself answers. `findmnt` can
# report a mount a moment before this shell can stat its directory, and reading
# that window as "an empty tree" would make the assertion about a race instead of
# about metadata. The VALUE is still asserted verbatim — only the moment of
# asking moves."""
new = """# ls_wait: list a mountpoint only once the DAEMON answers a stat of it. `findmnt`
# reports the mount as attached a moment before the first getattr can be replied
# to, and reading that window as "an empty tree" would make the assertion about a
# race instead of about metadata. The VALUE is still asserted verbatim — only the
# moment of asking moves. (Measured on a healthy remote mount: attached at 65ms,
# first getattr answered at 69ms; the loop exists for the window, not for seconds.)"""
assert s.count(old) == 1, 'ls_wait comment'
s = s.replace(old, new, 1)

io.open(p, 'w', encoding='utf-8').write(s)
print('patched')
