use super::*;

fn fixture() -> (tempfile::TempDir, Cache) {
    let root = tempfile::tempdir().unwrap();
    let cache = Cache::open(&root.path().join("cache"), &Settings::default()).unwrap();
    (root, cache)
}
fn id(n: usize) -> String {
    format!("{n:064x}")
}
fn put(cache: &Cache, n: usize, bytes: &[u8]) {
    let lookup = cache.lookup(&id(n)).unwrap();
    assert!(cache.insert(&id(n), bytes, &lookup.ticket).unwrap());
}
fn changed(settings: &Settings, revision: usize, clear: bool, enabled: bool) -> Settings {
    Settings {
        revision: id(revision),
        enabled,
        clear_epoch: if clear {
            id(revision)
        } else {
            settings.clear_epoch.clone()
        },
        ..settings.clone()
    }
}

#[test]
fn defaults_and_large_custom_limits_do_not_reserve_disk_space() {
    let (root, cache) = fixture();
    assert_eq!(cache.stats().settings.limit_bytes, 5 * GB);
    assert_eq!(cache.stats().retained_bytes, 0);
    assert!(Settings {
        limit_bytes: GB - 1,
        ..Default::default()
    }
    .validate()
    .is_err());
    let settings = Settings {
        revision: id(1),
        limit_bytes: 1000 * GB,
        ..Default::default()
    };
    cache.apply(&settings).unwrap();
    assert_eq!(fs::read_dir(root.path().join("cache")).unwrap().count(), 1); // epoch only
    assert_eq!(cache.stats().retained_bytes, 0);
}

#[test]
fn low_disk_reclaims_retained_audio_and_skips_writes_if_space_stays_low() {
    let (_root, cache) = fixture();
    put(&cache, 1, &[1; 100]);
    put(&cache, 2, &[2; 100]);
    let ticket = cache.lookup(&id(3)).unwrap().ticket;
    let mut readings = [FREE_RESERVE, FREE_RESERVE + 140].into_iter();
    assert!(cache
        .insert_with_space(&id(3), &[3; 100], &ticket, || Ok(readings.next().unwrap()))
        .unwrap());
    assert!(cache.lookup(&id(1)).unwrap().bytes.is_none());
    assert!(cache.lookup(&id(2)).unwrap().bytes.is_some());
    assert!(cache.lookup(&id(3)).unwrap().bytes.is_some());
    assert!(!cache
        .insert_with_space(&id(4), &[4; 100], &ticket, || Ok(0))
        .unwrap());
    assert_eq!(cache.stats().retained_bytes, 0);
    assert!(cache.lookup(&id(4)).unwrap().bytes.is_none());
    assert!(cache
        .insert_with_space(&id(4), &[4; 100], &ticket, || Ok(FREE_RESERVE + 140))
        .unwrap());
}

#[test]
fn persistence_checksum_and_content_key_separation() {
    let (root, cache) = fixture();
    put(&cache, 1, b"encoded interval");
    assert_eq!(
        cache.lookup(&id(1)).unwrap().bytes.unwrap(),
        b"encoded interval"
    );
    assert!(cache.lookup(&id(2)).unwrap().bytes.is_none());
    drop(cache);
    let cache = Cache::open(&root.path().join("cache"), &Settings::default()).unwrap();
    assert_eq!(
        cache.lookup(&id(1)).unwrap().bytes.unwrap(),
        b"encoded interval"
    );
    // Moving a valid packet under another key cannot manufacture a hit.
    fs::copy(cache.path(&id(1)), cache.path(&id(2))).unwrap();
    drop(cache);
    let cache = Cache::open(&root.path().join("cache"), &Settings::default()).unwrap();
    assert!(cache.lookup(&id(2)).unwrap().bytes.is_none());
    let mut file = OpenOptions::new()
        .write(true)
        .open(cache.path(&id(1)))
        .unwrap();
    file.write_all(b"damaged!").unwrap();
    assert!(cache.lookup(&id(1)).unwrap().bytes.is_none());
    assert_eq!(cache.stats().retained_bytes, 0);
    put(&cache, 1, b"regenerated");
    assert_eq!(cache.lookup(&id(1)).unwrap().bytes.unwrap(), b"regenerated");
}

#[test]
fn clear_disable_and_reenable_never_admit_old_encoder_tickets() {
    let (_root, cache) = fixture();
    put(&cache, 1, b"active copy is independent");
    let active = cache.lookup(&id(1)).unwrap();
    let clearing = changed(&cache.stats().settings, 10, true, true);
    cache.apply(&clearing).unwrap();
    assert_eq!(cache.stats().retained_bytes, 0);
    assert!(!cache
        .insert(&id(2), b"late encode", &active.ticket)
        .unwrap());
    assert_eq!(active.bytes.unwrap(), b"active copy is independent");
    put(&cache, 2, b"new listening");
    let pending = cache.lookup(&id(3)).unwrap().ticket;
    let disabled = changed(&clearing, 11, true, false);
    cache.apply(&disabled).unwrap();
    assert_eq!(cache.stats().retained_bytes, 0);
    assert!(!cache.insert(&id(3), b"late", &pending).unwrap());
    let enabled = changed(&disabled, 12, false, true);
    cache.apply(&enabled).unwrap();
    assert!(!cache.insert(&id(3), b"still late", &pending).unwrap());
    put(&cache, 3, b"fresh");
}

#[test]
fn interrupted_clear_and_partial_write_recover_on_restart() {
    let (root, cache) = fixture();
    put(&cache, 1, b"old");
    let settings = changed(&cache.stats().settings, 10, true, true);
    // Settings were durably saved, but the process died before applying them.
    fs::write(
        root.path().join("cache/.write-interrupted"),
        b"partial packet",
    )
    .unwrap();
    drop(cache);
    let cache = Cache::open(&root.path().join("cache"), &settings).unwrap();
    assert_eq!(cache.stats().entries, 0);
    assert!(!root.path().join("cache/.write-interrupted").exists());
    put(&cache, 2, b"after recovery");
    drop(cache);
    let cache = Cache::open(&root.path().join("cache"), &settings).unwrap();
    assert_eq!(
        cache.lookup(&id(2)).unwrap().bytes.unwrap(),
        b"after recovery"
    );
}

#[test]
fn eviction_keeps_recently_used_entries_and_survives_restart() {
    let (root, cache) = fixture();
    put(&cache, 1, b"one");
    put(&cache, 2, b"two");
    cache.lookup(&id(1)).unwrap();
    // Small-budget unit exercise uses the same eviction path without allocating GB.
    cache
        .evict_to(&mut cache.state.lock().unwrap(), (HEADER + 3) as u64)
        .unwrap();
    assert!(cache.lookup(&id(2)).unwrap().bytes.is_none());
    assert_eq!(cache.lookup(&id(1)).unwrap().bytes.unwrap(), b"one");
    drop(cache);
    let cache = Cache::open(&root.path().join("cache"), &Settings::default()).unwrap();
    assert_eq!(cache.stats().entries, 1);
}

#[test]
fn reducing_the_real_limit_evicts_to_the_saved_budget() {
    let (root, cache) = fixture();
    drop(cache);
    for n in 0..300 {
        // Sparse files exercise real accounting/eviction without writing 1.2 GB.
        let file = File::create(root.path().join(format!("cache/{}.segment", id(n)))).unwrap();
        file.set_len(4 * 1024 * 1024).unwrap();
    }
    let cache = Cache::open(&root.path().join("cache"), &Settings::default()).unwrap();
    assert!(cache.stats().retained_bytes > GB);
    cache
        .apply(&Settings {
            limit_bytes: GB,
            revision: id(1000),
            ..Default::default()
        })
        .unwrap();
    assert!(cache.stats().retained_bytes <= GB);
    assert!(cache.stats().entries < 300);
}

#[cfg(unix)]
#[test]
fn malformed_files_and_symlinks_cannot_escape_the_cache() {
    use std::os::unix::fs::symlink;
    let (root, cache) = fixture();
    let outside = root.path().join("outside");
    fs::write(&outside, b"keep me").unwrap();
    put(&cache, 1, b"replace");
    fs::remove_file(cache.path(&id(1))).unwrap();
    symlink(&outside, cache.path(&id(1))).unwrap();
    assert!(cache.lookup(&id(1)).unwrap().bytes.is_none());
    assert_eq!(fs::read(&outside).unwrap(), b"keep me");
    assert!(cache.lookup("../outside").is_err());
    fs::write(cache.path(&id(2)), b"").unwrap();
    fs::write(cache.path(&id(3)), MAGIC).unwrap();
    drop(cache);
    let cache = Cache::open(&root.path().join("cache"), &Settings::default()).unwrap();
    assert_eq!(cache.stats().entries, 0);
    assert!(!cache.path(&id(2)).exists());
    assert!(!cache.path(&id(3)).exists());
    let linked = root.path().join("linked");
    symlink(root.path().join("cache"), &linked).unwrap();
    assert!(Cache::open(&linked, &Settings::default()).is_err());
}

#[test]
fn failed_clear_bypasses_retention_until_it_can_be_retried() {
    let (_root, cache) = fixture();
    put(&cache, 1, b"saved");
    let pending = cache.lookup(&id(2)).unwrap().ticket;
    fs::remove_file(cache.path(&id(1))).unwrap();
    fs::create_dir(cache.path(&id(1))).unwrap(); // causes a real unlink error
    let settings = changed(&cache.stats().settings, 10, true, false);
    assert!(cache.apply(&settings).is_err());
    assert!(cache.stats().problem);
    assert!(!cache.insert(&id(2), b"late", &pending).unwrap());
    fs::remove_dir(cache.path(&id(1))).unwrap();
    cache.apply(&settings).unwrap();
    assert!(!cache.stats().problem);
    assert_eq!(cache.stats().retained_bytes, 0);
}
