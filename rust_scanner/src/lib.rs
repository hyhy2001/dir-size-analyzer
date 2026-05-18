use dashmap::DashSet;
use ignore::{WalkBuilder, WalkState};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

const CRITICAL_SKIP_NAMES: &[&str] = &[
    ".snapshot", ".snapshots", ".zfs", "proc", "sys", "dev", ".nfs",
];

fn default_workers() -> usize {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    (cpus * 2).clamp(4, 32)
}

fn scan_dir_info_impl(path: &str, workers: usize) -> PyResult<(u64, u64, u64)> {
    if !std::path::Path::new(path).exists() {
        return Err(PyRuntimeError::new_err(format!("Path not found: {path}")));
    }

    let root_dev = fs::metadata(path).ok().map(|m| m.dev());

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

    WalkBuilder::new(path)
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

fn is_parent_or_same(parent: &Path, child: &Path) -> bool {
    child.starts_with(parent)
}

#[derive(Clone)]
struct GroupJob {
    root: PathBuf,
    indexes: Vec<usize>,
    weight: u64,
    threads: usize,
}

fn estimate_path_weight(path: &Path, group_size: usize) -> u64 {
    const MAX_SAMPLE_ENTRIES: usize = 4096;
    let mut weight = (group_size as u64).saturating_mul(1_000_000);

    if let Ok(meta) = fs::metadata(path) {
        weight = weight.saturating_add(meta.blocks().saturating_mul(512));
    }

    let read_dir = match fs::read_dir(path) {
        Ok(iter) => iter,
        Err(_) => return weight,
    };

    let mut sampled = 0usize;
    let mut dir_count = 0u64;
    let mut file_count = 0u64;
    for entry_res in read_dir {
        if sampled >= MAX_SAMPLE_ENTRIES {
            break;
        }
        sampled += 1;

        let entry = match entry_res {
            Ok(entry) => entry,
            Err(_) => continue,
        };

        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        if ft.is_dir() {
            dir_count = dir_count.saturating_add(1);
        } else if ft.is_file() {
            file_count = file_count.saturating_add(1);
        }
    }

    let fanout_score = dir_count
        .saturating_mul(200_000)
        .saturating_add(file_count.saturating_mul(20_000));

    weight.saturating_add(fanout_score)
}

fn scan_group_paths_impl(
    root: &Path,
    targets: &[PathBuf],
    workers: usize,
) -> PyResult<Vec<(u64, u64, u64)>> {
    let root_dev = fs::metadata(root)
        .map_err(|e| PyRuntimeError::new_err(format!("metadata error {root:?}: {e}")))?
        .dev();

    let counters: Arc<Vec<AtomicU64>> =
        Arc::new((0..targets.len()).map(|_| AtomicU64::new(0)).collect());
    let skipped_perm: Arc<Vec<AtomicU64>> =
        Arc::new((0..targets.len()).map(|_| AtomicU64::new(0)).collect());
    let skipped_cross_dev: Arc<Vec<AtomicU64>> =
        Arc::new((0..targets.len()).map(|_| AtomicU64::new(0)).collect());
    let seen_inodes_per_target: Arc<Vec<DashSet<(u64, u64)>>> =
        Arc::new((0..targets.len()).map(|_| DashSet::new()).collect());
    let seen_dirs: Arc<DashSet<(u64, u64)>> = Arc::new(DashSet::new());
    let targets_arc = Arc::new(targets.to_vec());

    WalkBuilder::new(root)
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_exclude(false)
        .git_global(false)
        .threads(workers.max(1))
        .build_parallel()
        .run(|| {
            let counters = counters.clone();
            let skipped_perm = skipped_perm.clone();
            let skipped_cross_dev = skipped_cross_dev.clone();
            let seen_inodes_per_target = seen_inodes_per_target.clone();
            let seen_dirs = seen_dirs.clone();
            let targets = targets_arc.clone();

            Box::new(move |entry_res| {
                let entry = match entry_res {
                    Ok(e) => e,
                    Err(_) => {
                        for skipped in skipped_perm.iter() {
                            skipped.fetch_add(1, Ordering::Relaxed);
                        }
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

                let mut matched_indexes = Vec::new();
                for (idx, target) in targets.iter().enumerate() {
                    if path.starts_with(target) {
                        matched_indexes.push(idx);
                    }
                }

                if matched_indexes.is_empty() {
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
                            for idx in &matched_indexes {
                                skipped_perm[*idx].fetch_add(1, Ordering::Relaxed);
                            }
                            return WalkState::Skip;
                        }
                    };

                    let dkey = (meta.dev(), meta.ino());
                    if !seen_dirs.insert(dkey) {
                        return WalkState::Skip;
                    }

                    if meta.dev() != root_dev {
                        for idx in &matched_indexes {
                            skipped_cross_dev[*idx].fetch_add(1, Ordering::Relaxed);
                        }
                        return WalkState::Skip;
                    }

                    return WalkState::Continue;
                }

                if ft.is_file() {
                    let meta = match entry.metadata() {
                        Ok(m) => m,
                        Err(_) => {
                            for idx in &matched_indexes {
                                skipped_perm[*idx].fetch_add(1, Ordering::Relaxed);
                            }
                            return WalkState::Continue;
                        }
                    };

                    if meta.dev() != root_dev {
                        return WalkState::Continue;
                    }

                    let bytes = meta.blocks().saturating_mul(512);
                    if meta.nlink() <= 1 {
                        for idx in &matched_indexes {
                            counters[*idx].fetch_add(bytes, Ordering::Relaxed);
                        }
                    } else {
                        let key = (meta.dev(), meta.ino());
                        for idx in &matched_indexes {
                            if seen_inodes_per_target[*idx].insert(key) {
                                counters[*idx].fetch_add(bytes, Ordering::Relaxed);
                            }
                        }
                    }
                }

                WalkState::Continue
            })
        });

    Ok((0..targets.len())
        .map(|idx| {
            (
                counters[idx].load(Ordering::Relaxed) / 1024,
                skipped_perm[idx].load(Ordering::Relaxed),
                skipped_cross_dev[idx].load(Ordering::Relaxed),
            )
        })
        .collect())
}

#[pyfunction(signature = (path, max_workers=None))]
fn scan_dir_info(path: String, max_workers: Option<usize>) -> PyResult<(u64, u64, u64)> {
    let workers = max_workers.unwrap_or(default_workers()).max(1);
    scan_dir_info_impl(&path, workers)
}

#[pyfunction(signature = (path, max_workers=None))]
fn scan_dir_kb(path: String, max_workers: Option<usize>) -> PyResult<u64> {
    let (kb, _, _) = scan_dir_info(path, max_workers)?;
    Ok(kb)
}

#[pyfunction(signature = (paths, max_workers=None, on_result=None))]
fn scan_multi_dir_info(
    py: Python,
    paths: Vec<String>,
    max_workers: Option<usize>,
    on_result: Option<PyObject>,
) -> PyResult<Vec<(String, u64, u64, u64)>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }

    let mut input_paths: Vec<PathBuf> = Vec::with_capacity(paths.len());
    let mut devices: Vec<u64> = Vec::with_capacity(paths.len());
    for path in &paths {
        let pb = PathBuf::from(path);
        if !pb.exists() {
            return Err(PyRuntimeError::new_err(format!("Path not found: {path}")));
        }
        let canonical = fs::canonicalize(&pb)
            .map_err(|e| PyRuntimeError::new_err(format!("canonicalize error {path}: {e}")))?;
        let dev = fs::metadata(&canonical)
            .map_err(|e| PyRuntimeError::new_err(format!("metadata error {path}: {e}")))?
            .dev();
        input_paths.push(canonical);
        devices.push(dev);
    }

    let mut indexes: Vec<usize> = (0..input_paths.len()).collect();
    indexes.sort_by_key(|&idx| input_paths[idx].components().count());

    let mut groups: Vec<(PathBuf, Vec<usize>)> = Vec::new();
    for idx in indexes {
        let mut attached = false;
        for (root, members) in &mut groups {
            let root_idx = members[0];
            if devices[root_idx] == devices[idx] && is_parent_or_same(root, &input_paths[idx]) {
                members.push(idx);
                attached = true;
                break;
            }
        }
        if !attached {
            groups.push((input_paths[idx].clone(), vec![idx]));
        }
    }

    let total_workers = max_workers.unwrap_or(default_workers()).max(1);
    let worker_count = total_workers.min(groups.len()).max(1);

    let mut jobs = groups
        .into_iter()
        .map(|(root, indexes)| GroupJob {
            weight: estimate_path_weight(&root, indexes.len()),
            root,
            indexes,
            threads: 1,
        })
        .collect::<Vec<_>>();

    jobs.sort_by(|a, b| b.weight.cmp(&a.weight));

    let thread_budget = total_workers.saturating_sub(jobs.len());
    if thread_budget > 0 {
        let total_weight = jobs.iter().map(|job| job.weight.max(1)).sum::<u64>().max(1);
        let mut extra_alloc = vec![0usize; jobs.len()];
        let mut assigned = 0usize;

        for (idx, job) in jobs.iter().enumerate() {
            let proportional = (thread_budget as u128)
                .saturating_mul(job.weight.max(1) as u128)
                / (total_weight as u128);
            let add = usize::try_from(proportional).unwrap_or(0);
            extra_alloc[idx] = add;
            assigned = assigned.saturating_add(add);
        }

        let mut remain = thread_budget.saturating_sub(assigned);
        for idx in 0..jobs.len() {
            if remain == 0 {
                break;
            }
            extra_alloc[idx] = extra_alloc[idx].saturating_add(1);
            remain -= 1;
        }

        for (idx, job) in jobs.iter_mut().enumerate() {
            job.threads = job.threads.saturating_add(extra_alloc[idx]).max(1);
        }
    }

    let queue: Arc<Mutex<VecDeque<GroupJob>>> = Arc::new(Mutex::new(
        jobs.into_iter().collect::<VecDeque<_>>(),
    ));
    let results: Arc<Mutex<Vec<Option<(String, u64, u64, u64)>>>> =
        Arc::new(Mutex::new(vec![None; paths.len()]));
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let input_paths = Arc::new(input_paths);
    let original_paths = Arc::new(paths);
    let callback = on_result.map(|cb| Arc::new(cb.into_py(py)));

    let output = py.allow_threads(move || -> PyResult<Vec<(String, u64, u64, u64)>> {
        let mut handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let queue = queue.clone();
            let results = results.clone();
            let errors = errors.clone();
            let input_paths = input_paths.clone();
            let original_paths = original_paths.clone();
            let callback = callback.clone();

            let handle = thread::spawn(move || loop {
                let next_job = match queue.lock() {
                    Ok(mut guard) => guard.pop_front(),
                    Err(_) => {
                        if let Ok(mut err) = errors.lock() {
                            err.push("queue lock poisoned".to_string());
                        }
                        return;
                    }
                };

                let Some(job) = next_job else {
                    return;
                };

                let targets = job
                    .indexes
                    .iter()
                    .map(|&idx| input_paths[idx].clone())
                    .collect::<Vec<_>>();

                match scan_group_paths_impl(&job.root, &targets, job.threads) {
                    Ok(group_results) => {
                        if group_results.len() != job.indexes.len() {
                            if let Ok(mut err) = errors.lock() {
                                err.push("scan result length mismatch".to_string());
                            }
                            return;
                        }

                        if let Ok(mut output) = results.lock() {
                            for (local_pos, global_idx) in job.indexes.iter().enumerate() {
                                let (kb, skipped_perm, skipped_cross_dev) = group_results[local_pos];
                                let path = original_paths[*global_idx].clone();
                                output[*global_idx] = Some((
                                    path.clone(),
                                    kb,
                                    skipped_perm,
                                    skipped_cross_dev,
                                ));

                                if let Some(cb) = &callback {
                                    let cb = cb.clone();
                                    let callback_result = Python::with_gil(|py| {
                                        cb.call1(
                                            py,
                                            (
                                                *global_idx,
                                                path,
                                                kb,
                                                skipped_perm,
                                                skipped_cross_dev,
                                            ),
                                        )
                                    });
                                    if let Err(err_obj) = callback_result {
                                        if let Ok(mut err) = errors.lock() {
                                            err.push(err_obj.to_string());
                                        }
                                        return;
                                    }
                                }
                            }
                        } else if let Ok(mut err) = errors.lock() {
                            err.push("result lock poisoned".to_string());
                            return;
                        }
                    }
                    Err(error) => {
                        if let Ok(mut err) = errors.lock() {
                            err.push(error.to_string());
                        }
                    }
                }
            });

            handles.push(handle);
        }

        for handle in handles {
            if handle.join().is_err() {
                if let Ok(mut err) = errors.lock() {
                    err.push("worker thread panicked".to_string());
                }
            }
        }

        if let Ok(err) = errors.lock() {
            if !err.is_empty() {
                return Err(PyRuntimeError::new_err(err.join("; ")));
            }
        }

        let output = results
            .lock()
            .map_err(|_| PyRuntimeError::new_err("result lock poisoned"))?
            .iter()
            .enumerate()
            .map(|(index, item)| {
                item.clone().ok_or_else(|| {
                    PyRuntimeError::new_err(format!("Missing scan result at index {index}"))
                })
            })
            .collect::<PyResult<Vec<_>>>()?;

        Ok(output)
    })?;

    Ok(output)
}

#[pymodule]
fn fast_scanner(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(scan_dir_info, m)?)?;
    m.add_function(wrap_pyfunction!(scan_dir_kb, m)?)?;
    m.add_function(wrap_pyfunction!(scan_multi_dir_info, m)?)?;
    Ok(())
}
