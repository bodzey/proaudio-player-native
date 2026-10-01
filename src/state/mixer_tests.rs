use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;

static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "proaudio-mixer-test-{}-{}",
            std::process::id(),
            TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[tokio::test(start_paused = true)]
async fn continuous_control_updates_cannot_delay_persistence_indefinitely() {
    let (changes, mut receiver) = watch::channel(0);
    let settled = tokio::spawn(async move { coalesce_mixer_changes(&mut receiver).await });
    tokio::task::yield_now().await;
    for generation in 1..=20 {
        tokio::time::advance(Duration::from_millis(100)).await;
        changes.send_replace(generation);
        tokio::task::yield_now().await;
    }
    assert!(settled.is_finished());
    settled.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_short_control_burst_waits_for_the_last_update_to_settle() {
    let (changes, mut receiver) = watch::channel(0);
    let settled = tokio::spawn(async move { coalesce_mixer_changes(&mut receiver).await });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(500)).await;
    changes.send_replace(1);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(749)).await;
    tokio::task::yield_now().await;
    assert!(!settled.is_finished());
    tokio::time::advance(Duration::from_millis(1)).await;
    settled.await.unwrap();
}

#[tokio::test]
async fn queued_flush_saves_latest_values_and_unchanged_flush_avoids_flash_write() {
    let directory = TestDirectory::new();
    let path = directory.0.join("mixer.json");
    let runtime = MixerStateRuntime::new(&path);
    runtime.set_music_percent(20.0);
    let persisted = runtime.persisted.clone();
    let (locked, ready) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let holder = tokio::task::spawn_blocking(move || {
        let _guard = persisted.lock().unwrap();
        locked.send(()).unwrap();
        held.recv().unwrap();
    });
    ready.await.unwrap();
    let queued_runtime = runtime.clone();
    let queued = tokio::spawn(async move { queued_runtime.flush().await });
    tokio::task::yield_now().await;
    runtime.set_music_percent(80.0);
    runtime.set_master_muted(true);
    release.send(()).unwrap();
    holder.await.unwrap();
    queued.await.unwrap().unwrap();
    let saved = MixerStateStore::new(&path).load();
    assert_eq!(saved.music_percent, Some(80.0));
    assert_eq!(saved.master_muted, Some(true));
    let inode = fs::metadata(&path).unwrap().ino();
    runtime.flush().await.unwrap();
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
}

async fn wait_for_saved_percent(path: &Path, expected: f64) {
    tokio::time::timeout(Duration::from_secs(4), async {
        while MixerStateStore::new(path).load().music_percent != Some(expected) {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("background writer did not persist the pending change");
}

#[tokio::test]
async fn writer_persists_changes_that_precede_its_subscription() {
    let directory = TestDirectory::new();
    let path = directory.0.join("mixer.json");
    let runtime = MixerStateRuntime::new(&path);
    runtime.set_music_percent(42.0);
    let writer = runtime.start_writer();
    wait_for_saved_percent(&path, 42.0).await;
    writer.abort();
    let _ = writer.await;
}

#[tokio::test]
async fn failed_background_write_recovers_without_another_control_update() {
    let directory = TestDirectory::new();
    let parent = directory.0.join("unavailable");
    fs::write(&parent, b"not a directory").unwrap();
    let path = parent.join("mixer.json");
    let runtime = MixerStateRuntime::new(&path);
    runtime.set_music_percent(65.0);
    assert!(runtime.flush().await.is_err());
    assert_eq!(runtime.persisted.lock().unwrap().music_percent, None);
    let writer = runtime.start_writer();
    sleep(Duration::from_secs(2)).await;
    fs::remove_file(&parent).unwrap();
    wait_for_saved_percent(&path, 65.0).await;
    writer.abort();
    let _ = writer.await;
}
