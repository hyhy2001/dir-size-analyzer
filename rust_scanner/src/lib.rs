use dashmap::DashSet;
use ignore::{WalkBuilder, WalkState};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const CRITICAL_SKIP_NAMES: &[&str] = &[
    ".snapshot", ".snapshots", ".zfs", "proc", "sys", "dev", ".nfs",
];

#[pyfunction(signature = (path, max_workers=None))]
fn scan_dir_info(path: String, max_workers: Option<usize>) -> PyResult<(u64, u64, u64)> {
    if !std::path::Path::new(&path).exists() {
        return Err(PyRuntimeError::new_err(format!("Path not found: {path}")));
    }

    let root_dev = fs::metadata(&path).ok().map(|m| m.dev());
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let workers = max_workers.unwrap_or((cpus * 2).clamp(4, 32)).max(1);

    let total_bytes = Arc::new(AtomicU64::new(0));
    let skipped_perm = Arc::new(AtomicU64::new(0));
    let skipped_cross_dev = Arc::new(AtomicU64::new(0));

    let seen_inodes: Arc<DashSet<(u64, u64)>> = Arc::new(DashSet::new());
    let seen_dirs: Arc<DashSet<(u64, u64)>> = Arc::new(DashSet::new());

    let tb = total_bytes.clone();
    let sp = skipped_perm.clone();
    let sx = skipped_cross_dev.clone();
    let si = seen_inodes.clone();
    let sd = seen_dirs.clone();

    WalkBuilder::new(&path)
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_exclude(false)
        .git_global(false)
        .threads(workers)
        .build_parallel()
        .run(|| {
            let tb = tb.clone();
            let sp = sp.clone();
            let sx = sx.clone();
            let si = si.clone();
            let sd = sd.clone();
            Box::new(move |entry_res| {
                let entry = match entry_res {
                    Ok(e) => e,
                    Err(_) => {
                        sp.fetch_add(1, Ordering::Relaxed);
                        return WalkState::Continue;
                    }
                };

                let path = entry.path();
                let ft = match entry.file_type() {
                    Some(ft) => ft,
                    None => return WalkState::Continue,
                };

                if ft.is_symlink() {
                    return WalkState::Continue;
                }

                if ft.is_dir() {
                    if let Some(name) = path.file_name() {
                        let n = name.to_string_lossy();
                        if CRITICAL_SKIP_NAMES.contains(&n.as_ref()) {
                            return WalkState::Skip;
                        }
                    }

                    let meta = match entry.metadata() {
                        Ok(m) => m,
                        Err(_) => {
                            sp.fetch_add(1, Ordering::Relaxed);
                            return WalkState::Skip;
                        }
                    };

                    let dkey = (meta.dev(), meta.ino());
                    if !sd.insert(dkey) {
                        return WalkState::Skip;
                    }

                    if let Some(rdev) = root_dev {
                        if meta.dev() != rdev {
                            sx.fetch_add(1, Ordering::Relaxed);
                            return WalkState::Skip;
                        }
                    }

                    return WalkState::Continue;
                }

                if ft.is_file() {
                    let meta = match entry.metadata() {
                        Ok(m) => m,
                        Err(_) => {
                            sp.fetch_add(1, Ordering::Relaxed);
                            return WalkState::Continue;
                        }
                    };

                    if let Some(rdev) = root_dev {
                        if meta.dev() != rdev {
                            return WalkState::Continue;
                        }
                    }
                    if meta.nlink() <= 1 {
                        tb.fetch_add(meta.blocks().saturating_mul(512), Ordering::Relaxed);
                        return WalkState::Continue;
                    }

                    let key = (meta.dev(), meta.ino());
                    if si.insert(key) {
                        tb.fetch_add(meta.blocks().saturating_mul(512), Ordering::Relaxed);
                    }
                }

                WalkState::Continue
            })
        });

    Ok((
        total_bytes.load(Ordering::Relaxed) / 1024,
        skipped_perm.load(Ordering::Relaxed),
        skipped_cross_dev.load(Ordering::Relaxed),
    ))
}

#[pyfunction(signature = (path, max_workers=None))]
fn scan_dir_kb(path: String, max_workers: Option<usize>) -> PyResult<u64> {
    let (kb, _, _) = scan_dir_info(path, max_workers)?;
    Ok(kb)
}

#[pymodule]
fn fast_scanner(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(scan_dir_info, m)?)?;
    m.add_function(wrap_pyfunction!(scan_dir_kb, m)?)?;
    Ok(())
}
