pub mod generate;
pub mod new;

/// Mutex to serialize tests that change the process-global current directory.
#[cfg(test)]
pub(crate) static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Helper to create a unique temp directory for test isolation.
#[cfg(test)]
pub(crate) fn make_test_dir(label: &str) -> std::path::PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("mahalo_test_{label}_{ts}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
