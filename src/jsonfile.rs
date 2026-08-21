//! Loading and replacing the JSON file a store is persisted to.
//!
//! Two stores outlive the process — pushed content and battery history — and
//! both want the same two properties, so they are written once here.
//!
//! **A load never stops the boot.** A missing file is an empty store, and an
//! unreadable or corrupt one is a warning and an empty store. Refusing to start
//! because a cache of last-seen values is damaged would leave the panel showing
//! nothing at all, which is strictly worse than starting over.
//!
//! **A write is atomic.** A store is rewritten whole, from a process that may be
//! stopped between any two syscalls, so a reader must never see a half-written
//! file — hence temp file plus rename rather than truncate plus write.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Loads `path`, falling back to `T::default()` for anything unusable.
///
/// `what` names the store in the warning, because the operator's next move is to
/// look at that file.
pub fn read<T: DeserializeOwned + Default>(path: &Path, what: &str) -> T {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return T::default(),
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "{what} is unreadable; starting empty"
            );
            return T::default();
        }
    };

    match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "{what} is corrupt; starting empty"
            );
            T::default()
        }
    }
}

/// Replaces `path` with `value` as pretty JSON, atomically.
///
/// The temporary file is created in the *same* directory as the target: a temp
/// file elsewhere would make the rename a cross-device copy and lose the
/// atomicity this exists for.
pub fn write<T: Serialize>(path: &Path, value: &T, what: &str) -> Result<()> {
    let json = serde_json::to_vec_pretty(value).with_context(|| format!("serialising {what}"))?;

    let tmp = temp_path(path);
    std::fs::write(&tmp, &json)
        .with_context(|| format!("writing {what} temp file {}", tmp.display()))?;

    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("replacing {what} {}", path.display()));
    }
    Ok(())
}

/// A per-call temporary path beside the target file, unique so that two
/// concurrent writes cannot share a scratch file.
fn temp_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);

    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let stem = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "store.json".to_owned());
    dir.join(format!(".{stem}.tmp-{}-{nonce}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// A directory of its own per test, removed on drop.
    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "paneld-jsonfile-{}-{name}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn file(&self) -> PathBuf {
            self.0.join("store.json")
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn stored(pairs: &[(&str, u32)]) -> BTreeMap<String, u32> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), *value))
            .collect()
    }

    #[test]
    fn a_missing_file_loads_as_the_default_without_being_created() {
        let dir = Dir::new("missing");
        let loaded: BTreeMap<String, u32> = read(&dir.file(), "test store");
        assert!(loaded.is_empty());
        assert!(!dir.file().exists(), "a load does not create the file");
    }

    #[test]
    fn a_written_store_reads_back_identically() {
        let dir = Dir::new("roundtrip");
        let value = stored(&[("a", 1), ("b", 2)]);

        write(&dir.file(), &value, "test store").unwrap();

        assert_eq!(
            read::<BTreeMap<String, u32>>(&dir.file(), "test store"),
            value
        );
    }

    #[test]
    fn a_write_replaces_the_file_and_leaves_no_temporary_behind() {
        let dir = Dir::new("temp");
        write(&dir.file(), &stored(&[("a", 1)]), "test store").unwrap();
        write(&dir.file(), &stored(&[("b", 2)]), "test store").unwrap();

        let entries: Vec<String> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, ["store.json"], "a temp file was left behind");
        assert_eq!(
            read::<BTreeMap<String, u32>>(&dir.file(), "test store"),
            stored(&[("b", 2)]),
            "a write replaces the file rather than merging into it"
        );
    }

    #[test]
    fn a_corrupt_file_loads_as_the_default_rather_than_failing() {
        let dir = Dir::new("corrupt");
        std::fs::write(dir.file(), "{ this is not json").unwrap();

        let loaded: BTreeMap<String, u32> = read(&dir.file(), "test store");
        assert!(loaded.is_empty(), "a damaged store must not stop the boot");
    }

    #[test]
    fn a_file_holding_the_wrong_shape_loads_as_the_default() {
        let dir = Dir::new("shape");
        std::fs::write(dir.file(), r#"["not","a","map"]"#).unwrap();

        assert!(read::<BTreeMap<String, u32>>(&dir.file(), "test store").is_empty());
    }
}
