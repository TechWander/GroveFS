set -u
cd /data/grovefs/grovefs
M=.s12test/mnt
echo "--- 5-1 chmod 444 then same-uid write:"
echo hello > $M/a.txt
chmod 444 $M/a.txt
ls -l $M/a.txt
sh -c "echo more >> $M/a.txt"
echo "append rc=$?"
cat $M/a.txt
echo "--- cross-uid read as nobody:"
sudo -n -u nobody cat $M/a.txt 2>&1 | head -2
echo "--- 5-2 sparse hole:"
truncate -s 1M $M/sparse.bin
stat -c "size=%s blocks=%b" $M/sparse.bin
echo "allocated=$(( $(stat -c %b $M/sparse.bin) * 512 ))"
