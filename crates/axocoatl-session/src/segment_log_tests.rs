use super::*;

const SPEC: SegmentSpec = SegmentSpec {
    name: "test-log",
    kind: "test",
    segment_bytes: 64 * 1024,
    segment_records: 4,
    record_bytes: 1024,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Item {
    n: u64,
    text: String,
}

fn item(n: u64) -> Item {
    Item {
        n,
        text: format!("item {n}"),
    }
}

fn meta() -> serde_json::Value {
    serde_json::json!({"owner": "session-a"})
}

fn open(
    dir: &tempfile::TempDir,
    create: bool,
) -> Result<(SegmentLog, Vec<(u64, Item)>), SegmentError> {
    let mut seen = vec![];
    let log = SegmentLog::open(
        SecureDir::open(dir.path()).unwrap(),
        SPEC,
        meta(),
        create,
        |sequence, record: Item| {
            seen.push((sequence, record));
            Ok::<(), SegmentError>(())
        },
    )?;
    Ok((log, seen))
}

fn append(log: &mut SegmentLog, record: &Item) {
    let line = log.encode_record(record).unwrap();
    log.append_line(&line).unwrap();
    if log.should_seal() {
        log.seal().unwrap();
    }
}

fn sealed_path(dir: &tempfile::TempDir, index: u64) -> std::path::PathBuf {
    dir.path().join(SEGMENTS_DIR).join(SPEC.sealed_name(index))
}

#[test]
fn records_rotate_into_sealed_segments_and_read_back_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let (mut log, seen) = open(&dir, true).unwrap();
    assert!(seen.is_empty());
    for n in 1..=10 {
        assert_eq!(log.next_sequence(), n);
        append(&mut log, &item(n));
    }
    // Four records per segment: two sealed, two records active.
    assert_eq!(log.sealed().len(), 2);
    assert_eq!(log.active_records(), 2);
    assert_eq!(log.sealed()[1].first_sequence, 5);
    let second: Vec<Item> = log.read_sealed(&log.sealed()[1].clone()).unwrap();
    assert_eq!(second, (5..=8).map(item).collect::<Vec<_>>());
    assert_eq!(log.segment_of(6).unwrap().index, 1);
    assert!(log.segment_of(9).is_none());
    drop(log);

    let (log, seen) = open(&dir, false).unwrap();
    assert_eq!(
        seen,
        (1..=10).map(|n| (n, item(n))).collect::<Vec<_>>(),
        "every record is read back once, in order"
    );
    assert_eq!(log.next_sequence(), 11);
    assert_eq!(log.recovery(), SegmentRecovery::default());
}

#[test]
fn a_sealed_segment_cannot_change_disappear_or_move() {
    let dir = tempfile::tempdir().unwrap();
    let (mut log, _) = open(&dir, true).unwrap();
    for n in 1..=9 {
        append(&mut log, &item(n));
    }
    drop(log);
    let first = sealed_path(&dir, 0);
    let original = std::fs::read(&first).unwrap();

    // One changed byte breaks the seal's digest.
    let changed = String::from_utf8(original.clone())
        .unwrap()
        .replace("item 2", "item 7");
    std::fs::write(&first, changed).unwrap();
    assert!(matches!(
        open(&dir, false),
        Err(SegmentError::Invalid("sealed segment digest mismatch"))
    ));

    // A segment resealed with a matching digest still breaks the chain.
    let (body, _) = split_seal(&original).unwrap();
    let forged_body = String::from_utf8(body.to_vec())
        .unwrap()
        .replace("item 2", "item 7")
        .into_bytes();
    let mut forged = forged_body.clone();
    forged.extend(
        encode_line(&LineRef::<()>::Seal(&SegmentSeal {
            records: 4,
            digest: sha256(&forged_body),
        }))
        .unwrap(),
    );
    std::fs::write(&first, &forged).unwrap();
    assert!(matches!(
        open(&dir, false),
        Err(SegmentError::Invalid("segment chain is broken"))
    ));
    std::fs::write(&first, &original).unwrap();
    assert!(open(&dir, false).is_ok());

    // A removed segment leaves a gap; swapping two breaks the chain.
    std::fs::rename(&first, dir.path().join("aside")).unwrap();
    assert!(matches!(
        open(&dir, false),
        Err(SegmentError::Invalid("sealed segments are not contiguous"))
    ));
    let second = sealed_path(&dir, 1);
    std::fs::rename(&second, &first).unwrap();
    std::fs::rename(dir.path().join("aside"), &second).unwrap();
    assert!(open(&dir, false).is_err());
}

#[test]
fn a_torn_last_line_is_removed_and_writing_continues() {
    let dir = tempfile::tempdir().unwrap();
    let (mut log, _) = open(&dir, true).unwrap();
    for n in 1..=3 {
        append(&mut log, &item(n));
    }
    drop(log);
    let active = dir.path().join(SPEC.active_name());
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&active)
        .unwrap();
    file.write_all(br#"{"record":{"n":4,"te"#).unwrap();
    drop(file);

    let (mut log, seen) = open(&dir, false).unwrap();
    assert_eq!(seen.len(), 3);
    assert_eq!(log.recovery().torn_bytes, 20);
    assert_eq!(log.next_sequence(), 4);
    append(&mut log, &item(4));
    append(&mut log, &item(5));
    drop(log);
    let (_, seen) = open(&dir, false).unwrap();
    assert_eq!(seen, (1..=5).map(|n| (n, item(n))).collect::<Vec<_>>());

    // A damaged line that was finished is not a torn write: refuse it.
    let mut bytes = std::fs::read(&active).unwrap();
    let at = bytes.len() - 3;
    bytes[at] = b'#';
    std::fs::write(&active, bytes).unwrap();
    assert!(matches!(open(&dir, false), Err(SegmentError::Json(_))));
}

#[test]
fn an_interrupted_seal_is_completed_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let (mut log, _) = open(&dir, true).unwrap();
    for n in 1..=3 {
        append(&mut log, &item(n));
    }
    let line = log.encode_record(&item(4)).unwrap();
    log.append_line(&line).unwrap();
    assert!(log.should_seal());
    let active = dir.path().join(SPEC.active_name());
    let before = std::fs::read(&active).unwrap();
    log.seal().unwrap();
    drop(log);
    // The crash came after the sealed file was published and before the new
    // active segment replaced the old one.
    std::fs::write(&active, &before).unwrap();

    let (mut log, seen) = open(&dir, false).unwrap();
    assert!(log.recovery().completed_seal);
    assert_eq!(seen, (1..=4).map(|n| (n, item(n))).collect::<Vec<_>>());
    assert_eq!(log.sealed().len(), 1);
    assert_eq!(log.active_records(), 0);
    append(&mut log, &item(5));
    drop(log);
    let (log, seen) = open(&dir, false).unwrap();
    assert_eq!(seen.len(), 5);
    assert!(!log.recovery().completed_seal);

    // An active segment that differs from the seal is refused.
    let mut forged = before.clone();
    forged.extend(log.encode_record(&item(99)).unwrap());
    drop(log);
    std::fs::write(&active, forged).unwrap();
    assert!(open(&dir, false).is_err());
}

#[test]
fn a_missing_log_is_created_only_when_asked_and_identity_must_match() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        open(&dir, false),
        Err(SegmentError::Invalid("active segment is missing"))
    ));
    let (mut log, _) = open(&dir, true).unwrap();
    append(&mut log, &item(1));
    drop(log);
    let other = SegmentLog::open(
        SecureDir::open(dir.path()).unwrap(),
        SPEC,
        serde_json::json!({"owner": "session-b"}),
        false,
        |_, _: Item| Ok::<(), SegmentError>(()),
    );
    assert!(matches!(
        other,
        Err(SegmentError::Invalid(
            "segment belongs to another store or schema"
        ))
    ));
    let (log, _) = open(&dir, false).unwrap();
    let huge = Item {
        n: 2,
        text: "x".repeat(2048),
    };
    assert!(matches!(
        log.encode_record(&huge),
        Err(SegmentError::RecordTooLarge)
    ));
}

#[test]
fn a_failed_append_requires_reopening_before_more_writes() {
    let dir = tempfile::tempdir().unwrap();
    let (mut log, _) = open(&dir, true).unwrap();
    append(&mut log, &item(1));
    let line = log.encode_record(&item(2)).unwrap();
    std::fs::remove_dir_all(dir.path()).unwrap();
    assert!(log.append_line(&line).is_err());
    assert!(matches!(
        log.append_line(&line),
        Err(SegmentError::RecoveryRequired)
    ));
    assert!(matches!(log.seal(), Err(SegmentError::RecoveryRequired)));
}

fn read_all(dir: &tempfile::TempDir) -> Result<Vec<(u64, Item)>, SegmentError> {
    let mut seen = vec![];
    SegmentLog::read(
        SecureDir::open(dir.path()).unwrap(),
        SPEC,
        meta(),
        || {},
        |sequence, record: Item| {
            seen.push((sequence, record));
            Ok::<(), SegmentError>(())
        },
    )?;
    Ok(seen)
}

#[test]
fn a_reader_sees_every_acknowledged_record_and_never_writes() {
    let dir = tempfile::tempdir().unwrap();
    let (mut log, _) = open(&dir, true).unwrap();
    for n in 1..=6 {
        append(&mut log, &item(n));
    }
    // An unfinished line from a writer still appending is not read.
    let active = dir.path().join(SPEC.active_name());
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&active)
        .unwrap();
    file.write_all(br#"{"record":{"n":7"#).unwrap();
    drop(file);
    let before = std::fs::read(&active).unwrap();
    assert_eq!(
        read_all(&dir).unwrap(),
        (1..=6).map(|n| (n, item(n))).collect::<Vec<_>>()
    );
    assert_eq!(
        std::fs::read(&active).unwrap(),
        before,
        "a reader never writes"
    );
    drop(log);

    // A seal the writer has published but not yet followed is read from the
    // sealed file, once.
    let (mut log, _) = open(&dir, false).unwrap();
    append(&mut log, &item(7));
    let line = log.encode_record(&item(8)).unwrap();
    log.append_line(&line).unwrap();
    let unsealed = std::fs::read(&active).unwrap();
    log.seal().unwrap();
    std::fs::write(&active, &unsealed).unwrap();
    assert_eq!(read_all(&dir).unwrap().len(), 8);
    let mut reader = SegmentLog::read(
        SecureDir::open(dir.path()).unwrap(),
        SPEC,
        meta(),
        || {},
        |_, _: Item| Ok::<(), SegmentError>(()),
    )
    .unwrap();
    assert!(reader.append_line(&line).is_err());
}

#[test]
fn a_key_filter_never_misses_a_key_it_holds() {
    let keys: Vec<String> = (0..1000).map(|n| format!("invocation-{n}")).collect();
    let filter = KeyFilter::new(keys.iter().map(String::as_str));
    assert!(keys.iter().all(|key| filter.may_contain(key)));
    let false_positives = (0..10_000)
        .filter(|n| filter.may_contain(&format!("other-{n}")))
        .count();
    assert!(false_positives < 300, "{false_positives}");
    assert!(filter.bytes() <= 1000 * 10 / 8 + 8);
    let empty = KeyFilter::new(std::iter::empty());
    assert!(!empty.may_contain("anything"));
}

#[test]
fn the_segment_cache_holds_a_fixed_number_of_segments() {
    let dir = tempfile::tempdir().unwrap();
    let (mut log, _) = open(&dir, true).unwrap();
    for n in 1..=16 {
        append(&mut log, &item(n));
    }
    let cache = SegmentCache::<Item>::new(2);
    let sealed = log.sealed().to_vec();
    assert_eq!(sealed.len(), 4);
    for segment in &sealed {
        let records = cache.get(&log, segment).unwrap();
        assert_eq!(records[0].n, segment.first_sequence);
    }
    assert_eq!(cache.segments.lock().unwrap().len(), 2);
    // A cached segment comes back without reading it again; an evicted one
    // is read from disk.
    std::fs::remove_file(sealed_path(&dir, 3)).unwrap();
    std::fs::remove_file(sealed_path(&dir, 0)).unwrap();
    assert_eq!(cache.get(&log, &sealed[3]).unwrap().len(), 4);
    assert!(cache.get(&log, &sealed[0]).is_err());
}

#[test]
fn removing_a_log_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let handle = SecureDir::open(dir.path()).unwrap();
    assert!(!SegmentLog::exists(&handle, &SPEC).unwrap());
    let (mut log, _) = open(&dir, true).unwrap();
    for n in 1..=5 {
        append(&mut log, &item(n));
    }
    drop(log);
    assert!(SegmentLog::exists(&handle, &SPEC).unwrap());
    SegmentLog::remove(&handle, &SPEC).unwrap();
    assert!(!SegmentLog::exists(&handle, &SPEC).unwrap());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
