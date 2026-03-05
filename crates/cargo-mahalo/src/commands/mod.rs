pub mod generate;
pub mod new;

/// Mutex to serialize tests that change the process-global current directory.
#[cfg(test)]
pub(crate) static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
