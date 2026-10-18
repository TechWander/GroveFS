//! Unit coverage for the data plane of `grove import`: registering an
//! EXISTING file as an object must be zero-copy (hard link) when source and
//! store share a filesystem, and the shared inode must stay live through
//! every path of the object interface.

use grovefs::backend::local::LocalBackend;
use grovefs::backend::{Imported, StorageBackend};
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;

#[test]
fn import_at_hardlinks_zero_copy_and_shares_the_inode() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    fs::create_dir_all(&root).unwrap();
    let src_path = dir.path().join("dataset.bin");
    fs::write(&src_path, b"legacy bytes").unwrap();
    let src_ino = fs::metadata(&src_path).unwrap().ino();

    let be = LocalBackend::new(&root).unwrap();
    let got = be.import_at("k1", &src_path).unwrap();
    assert_eq!(got, Imported::Hardlinked, "same-fs import must not copy");

    // One inode behind both paths: size reads match, and a write through the
    // object interface is visible in the source file (the passthrough half
    // of the import promise), and vice versa.
    assert_eq!(be.len("k1").unwrap(), 12);
    be.write("k1", 7, b"BYTES!").unwrap();
    assert_eq!(fs::read_to_string(&src_path).unwrap(), "legacy BYTES!");
    let mut f = fs::OpenOptions::new().append(true).open(&src_path).unwrap();
    f.write_all(b"!!").unwrap();
    drop(f);
    assert_eq!(be.read("k1", 13, 8).unwrap(), b"!!".to_vec());

    // The blocks path is a second link on the SAME inode (nlink == 2).
    let obj = fs::metadata(root.join("blocks").join("k1")).unwrap();
    assert_eq!(obj.nlink(), 2);
    assert_eq!(obj.ino(), src_ino, "both paths name one inode");
}

#[test]
fn gc_remove_drops_only_the_blocks_link_not_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    fs::create_dir_all(&root).unwrap();
    let src_path = dir.path().join("keepme.txt");
    fs::write(&src_path, b"precious").unwrap();

    let be = LocalBackend::new(&root).unwrap();
    be.import_at("k2", &src_path).unwrap();
    // Unlinking the object (what view-level GC does) must leave the user's
    // original dataset path fully intact — importing can never destroy data.
    be.remove("k2").unwrap();
    assert!(!root.join("blocks").join("k2").exists());
    assert_eq!(fs::read_to_string(&src_path).unwrap(), "precious");
}

#[test]
fn default_import_at_streams_a_copy_for_other_backends() {
    // A backend that does not override import_at still ends up with a correct,
    // independent object (exercised through LocalBackend's own copy fallback
    // path is hard to force on one filesystem, so assert the trait default
    // via a minimal newtype wrapper that inherits it).
    struct Plain(LocalBackend);
    impl StorageBackend for Plain {
        fn create(&self, key: &str) -> std::io::Result<()> {
            self.0.create(key)
        }
        fn read(&self, k: &str, o: u64, l: usize) -> std::io::Result<Vec<u8>> {
            self.0.read(k, o, l)
        }
        fn write(&self, k: &str, o: u64, d: &[u8]) -> std::io::Result<usize> {
            self.0.write(k, o, d)
        }
        fn len(&self, k: &str) -> std::io::Result<u64> {
            self.0.len(k)
        }
        fn fsync(&self, k: &str) -> std::io::Result<()> {
            self.0.fsync(k)
        }
        fn truncate(&self, k: &str, n: u64) -> std::io::Result<()> {
            self.0.truncate(k, n)
        }
        fn remove(&self, k: &str) -> std::io::Result<()> {
            self.0.remove(k)
        }
        fn statvfs(&self) -> std::io::Result<grovefs::backend::FsStat> {
            self.0.statvfs()
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    fs::create_dir_all(&root).unwrap();
    let src_path = dir.path().join("file.bin");
    fs::write(&src_path, b"0123456789").unwrap();

    let be = Plain(LocalBackend::new(&root).unwrap());
    // Plain does NOT override import_at... but it delegates nothing, so the
    // trait default runs: a true independent copy.
    let got = be.import_at("k3", &src_path).unwrap();
    assert_eq!(got, Imported::Copied);
    assert_eq!(be.len("k3").unwrap(), 10);
    // Independent: mutating the source no longer shows in the object.
    fs::write(&src_path, b"changed").unwrap();
    assert_eq!(be.read("k3", 0, 20).unwrap(), b"0123456789".to_vec());
}
