// PAN-471: the decode-window recorder writes `<name>.part`, finalizes it and
// renames it to `<name>`, so a reader of the recordings folder (the corpus
// offload) only ever sees whole `ft8_*.wav` files, and `open` removes the
// `ft8_*.wav.part` files a crash left behind.
use super::*;
use std::sync::atomic::AtomicBool;

fn ts() -> UtcTimestamp {
    // Formats as `ft8_20260921_141320`.
    UtcTimestamp::from_parts(1_790_000_000, 0).unwrap()
}

fn tone(len: usize) -> Vec<f32> {
    (0..len).map(|i| (i as f32 * 0.3).sin() * 0.5).collect()
}

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

fn is_finished_wav(path: &std::path::Path, n: u32) -> bool {
    hound::WavReader::open(path).is_ok_and(|r| r.len() == n)
}

#[test]
fn open_removes_stale_ft8_parts_and_nothing_else() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("ft8_a.wav"), vec![0u8; 1000]).unwrap();
    std::fs::write(dir.join("ft8_20260921_141320.wav.part"), vec![0u8; 500]).unwrap();
    std::fs::write(dir.join("ft8_20260921_141320_20m.wav.part"), vec![0u8; 500]).unwrap();
    std::fs::write(dir.join("other.wav.part"), vec![0u8; 500]).unwrap();
    std::fs::write(dir.join("notes.txt"), b"keep me").unwrap();
    std::fs::create_dir(dir.join("ft8_dir.wav.part")).unwrap();
    std::fs::create_dir(dir.join("raw48")).unwrap();
    std::fs::write(dir.join("raw48").join("raw48_x.wav.part"), vec![0u8; 500]).unwrap();

    let rec = WavRecorder::open(dir.to_path_buf(), u64::MAX).unwrap();

    assert_eq!(
        names(dir),
        [
            "ft8_a.wav",
            "ft8_dir.wav.part",
            "notes.txt",
            "other.wav.part",
            "raw48"
        ]
    );
    assert_eq!(names(&dir.join("raw48")), ["raw48_x.wav.part"]);
    assert_eq!(rec.total_bytes, 1000);
    assert_eq!(WavRecorder::dir_size(dir).unwrap(), (1000, true));
}

#[test]
fn a_written_window_keeps_its_final_name_and_leaves_no_part() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let mut rec = WavRecorder::open(dir.to_path_buf(), u64::MAX).unwrap();

    rec.write_window(&tone(1200), &ts(), 0);
    rec.write_window(&tone(1200), &ts(), 14_074_000);

    assert_eq!(
        names(dir),
        ["ft8_20260921_141320.wav", "ft8_20260921_141320_20m.wav"]
    );
    for name in names(dir) {
        assert!(is_finished_wav(&dir.join(&name), 1200), "{name} unfinished");
    }
    assert_eq!(WavRecorder::dir_size(dir).unwrap(), (rec.total_bytes, true));
}

#[test]
fn a_failed_publish_leaves_no_part_and_counts_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // A non-empty directory under the final name makes the rename fail.
    let blocker = dir.join("ft8_20260921_141320.wav");
    std::fs::create_dir(&blocker).unwrap();
    std::fs::write(blocker.join("keep.txt"), b"keep me").unwrap();
    let mut rec = WavRecorder::open(dir.to_path_buf(), u64::MAX).unwrap();

    rec.write_window(&tone(1200), &ts(), 0);

    assert_eq!(names(dir), ["ft8_20260921_141320.wav"]);
    assert!(blocker.is_dir());
    assert_eq!(names(&blocker), ["keep.txt"]);
    assert_eq!(rec.total_bytes, 0);
}

#[test]
fn a_concurrent_offload_only_ever_moves_finished_windows() {
    let tmp = tempfile::tempdir().unwrap();
    let recordings = tmp.path().join("recordings");
    let nas = tmp.path().join("nas");
    std::fs::create_dir_all(&nas).unwrap();
    let mut rec = WavRecorder::open(recordings.clone(), u64::MAX).unwrap();

    let done = std::sync::Arc::new(AtomicBool::new(false));
    let offload = {
        let done = done.clone();
        let recordings = recordings.clone();
        let nas = nas.clone();
        std::thread::spawn(move || loop {
            // Read `done` before the sweep, so the last sweep runs after the
            // write has returned.
            let finished = done.load(std::sync::atomic::Ordering::SeqCst);
            for entry in std::fs::read_dir(&recordings).unwrap().flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|ext| ext != "wav") {
                    continue;
                }
                if std::fs::copy(&path, nas.join(entry.file_name())).is_ok() {
                    let _ = std::fs::remove_file(&path);
                }
            }
            if finished {
                break;
            }
        })
    };

    // One real-size window: 12.64 s at 12 kHz.
    rec.write_window(&tone(151_680), &ts(), 0);
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    offload.join().unwrap();

    assert_eq!(names(&nas), ["ft8_20260921_141320.wav"]);
    let moved = nas.join("ft8_20260921_141320.wav");
    assert!(
        is_finished_wav(&moved, 151_680),
        "offload moved an unfinished window: {} bytes",
        std::fs::metadata(&moved).map(|m| m.len()).unwrap_or(0)
    );
    assert!(names(&recordings).is_empty(), "{:?}", names(&recordings));
}
