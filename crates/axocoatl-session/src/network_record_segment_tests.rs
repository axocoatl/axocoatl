//! The record in segments: rotation, reads across segments, recovery, the
//! move from Axocoatl 1.2.0's single file, and growth past 1.2.0's caps.
use super::tests::{
    file_path, limit_event, open, open_event, setup, single_file_record, web_event,
};
use super::*;
use crate::execution_store::SessionExecutionStore;

/// Five events per segment, so a few events reach several segments.
const SMALL: SegmentLimits = SegmentLimits {
    bytes: 64 * 1024,
    events: 5,
};

fn try_open_small(store: &SessionExecutionStore) -> Result<NetworkRecord, NetworkRecordError> {
    NetworkRecord::open_with(
        store
            .component_namespace(ExecutionComponent::NetworkRecord)
            .unwrap(),
        SMALL,
    )
}

fn open_small(store: &SessionExecutionStore) -> NetworkRecord {
    try_open_small(store).unwrap()
}

fn directory(store: &SessionExecutionStore) -> std::path::PathBuf {
    file_path(store).parent().unwrap().to_path_buf()
}

fn sealed_path(store: &SessionExecutionStore, index: u64) -> std::path::PathBuf {
    directory(store)
        .join(SEGMENTS_DIR)
        .join(segments::sealed_name(index))
}

fn seqs(lines: &[NetworkLine]) -> Vec<u64> {
    lines.iter().map(|line| line.seq).collect()
}

/// Every sequence number, read `page` lines at a time through the writer.
fn page_all(record: &NetworkRecord, page: usize) -> Vec<u64> {
    let mut all = Vec::new();
    let mut after = None;
    loop {
        let lines = record.read_after(after, page).unwrap();
        if lines.is_empty() {
            return all;
        }
        after = lines.last().map(|line| line.seq);
        all.extend(seqs(&lines));
    }
}

fn line_bytes(seq: u64, event: NetworkEvent) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&NetworkLine {
        v: NETWORK_RECORD_VERSION,
        seq,
        ts_ms: seq,
        event,
    })
    .unwrap();
    bytes.push(b'\n');
    bytes
}

fn policy_event(revision: u64) -> NetworkEvent {
    NetworkEvent::Policy {
        scope: EgressScope::Session,
        revision,
        digest: "ab".repeat(32),
        source: PolicySource::Config,
        rules: vec!["registry.npmjs.org:443 (preset npm)".into()],
        change: None,
        actor: None,
    }
}

fn sidecar_event(generation: u32) -> NetworkEvent {
    NetworkEvent::Sidecar {
        state: SidecarState::Ready,
        generation,
        container: None,
        detail: None,
    }
}

#[test]
fn a_new_record_starts_with_its_head_and_an_empty_active_segment() {
    let (_root, _ownership, store) = setup();
    let record = open(&store);
    assert_eq!(record.stats(), RecordStats::default());
    assert_eq!(record.sealed_segments(), 0);
    drop(record);
    assert_eq!(
        std::fs::read(directory(&store).join(NETWORK_RECORD_FILE)).unwrap(),
        segments::head()
    );
    let active = std::fs::read_to_string(file_path(&store)).unwrap();
    assert_eq!(active.lines().count(), 1, "{active}");
    assert!(active.starts_with("{\"header\":{"), "{active}");
    assert!(!directory(&store).join(SEGMENTS_DIR).exists());
    let (lines, stats) = NetworkRecord::read_existing(&store, None, 10)
        .unwrap()
        .unwrap();
    assert!(lines.is_empty());
    assert_eq!(stats, RecordStats::default());
}

#[test]
fn full_segments_are_sealed_and_read_by_sequence_across_them() {
    let (_root, _ownership, store) = setup();
    let mut record = open_small(&store);
    for id in 1..=23 {
        assert_eq!(record.append(id, open_event(id, "a.example")).unwrap(), id);
    }
    // Five events per segment, sealed before the append that would pass it.
    assert_eq!(record.sealed_segments(), 4);
    let all: Vec<u64> = (1..=23).collect();
    assert_eq!(seqs(&record.read_after(None, 1000).unwrap()), all);
    assert_eq!(seqs(&record.read_after(Some(3), 4).unwrap()), [4, 5, 6, 7]);
    assert_eq!(seqs(&record.read_after(Some(10), 1).unwrap()), [11]);
    assert_eq!(
        seqs(&record.read_after(Some(19), 10).unwrap()),
        [20, 21, 22, 23]
    );
    assert!(record.read_after(Some(23), 10).unwrap().is_empty());
    for page in [1, 2, 3, 5, 7, 1000] {
        assert_eq!(page_all(&record, page), all, "{page}");
    }
    let stats = record.stats();
    assert_eq!(
        (
            stats.events,
            stats.last_seq,
            stats.gaps,
            stats.max_generation
        ),
        (23, 23, 0, 1)
    );
    // A reader without the writer reads the same pages and counts.
    for after in [
        None,
        Some(0),
        Some(4),
        Some(5),
        Some(12),
        Some(22),
        Some(23),
    ] {
        for limit in [1, 3, 100] {
            let (lines, read_stats) = NetworkRecord::read_existing(&store, after, limit)
                .unwrap()
                .unwrap();
            assert_eq!(
                lines,
                record.read_after(after, limit).unwrap(),
                "{after:?} {limit}"
            );
            assert_eq!(read_stats, stats);
        }
    }
    record.sync().unwrap();
    drop(record);

    // On disk: the head, the active segment, and four sealed segments that
    // each end in a seal counting their events.
    assert_eq!(
        std::fs::read(directory(&store).join(NETWORK_RECORD_FILE)).unwrap(),
        segments::head()
    );
    for index in 0..4u64 {
        let text = std::fs::read_to_string(sealed_path(&store, index)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 7, "header, five events and the seal");
        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["header"]["index"], index);
        assert_eq!(header["header"]["first_seq"], index * 5 + 1);
        assert_eq!(header["header"]["before"]["events"], index * 5);
        let seal: serde_json::Value = serde_json::from_str(lines[6]).unwrap();
        assert_eq!(seal["seal"]["records"], 5);
    }
    let reopened = open_small(&store);
    assert_eq!(reopened.stats(), stats);
    assert_eq!(page_all(&reopened, 4), all);
}

#[test]
fn the_record_keeps_bytes_past_the_old_byte_cap() {
    // Axocoatl 1.2.0 refused events once its file held 32 MiB.
    const OLD_CAP: u64 = 32 * 1024 * 1024;
    let (_root, _ownership, store) = setup();
    let mut record = open(&store);
    let large = |n: u64| NetworkEvent::Limit {
        what: LimitKind::UnrecordedRefusals,
        detail: format!("{n} {}", "x".repeat(MAX_LINE_BYTES - 200)),
    };
    let mut seq = 0;
    while record.stats().bytes <= OLD_CAP + MAX_LINE_BYTES as u64 {
        seq = record.append(seq, large(seq)).unwrap();
    }
    let stats = record.stats();
    assert!(stats.bytes > OLD_CAP, "{stats:?}");
    assert!(record.sealed_segments() >= 7, "{record:?}");
    let tail = record.read_after(Some(seq - 2), 10).unwrap();
    assert_eq!(seqs(&tail), [seq - 1, seq]);
    assert_eq!(tail[1].event, large(seq - 1));
    assert_eq!(record.append(0, limit_event()).unwrap(), seq + 1);
}

#[test]
fn a_torn_tail_after_sealed_segments_is_cut_and_recorded() {
    let (_root, _ownership, store) = setup();
    let mut record = open_small(&store);
    for id in 1..=12 {
        record.append(id, open_event(id, "a.example")).unwrap();
    }
    drop(record);
    let good = std::fs::read(file_path(&store)).unwrap();
    let mut torn = good.clone();
    torn.extend_from_slice(b"{\"v\":1,\"seq\":13,\"ts");
    std::fs::write(file_path(&store), &torn).unwrap();
    // A reader skips it and changes nothing.
    let (lines, _) = NetworkRecord::read_existing(&store, None, 100)
        .unwrap()
        .unwrap();
    assert_eq!(seqs(&lines), (1..=12).collect::<Vec<_>>());
    assert_eq!(std::fs::read(file_path(&store)).unwrap(), torn);
    let record = open_small(&store);
    assert_eq!(record.sealed_segments(), 2);
    let lines = record.read_after(None, 100).unwrap();
    assert_eq!(seqs(&lines), (1..=13).collect::<Vec<_>>());
    assert_eq!(
        lines[12].event,
        NetworkEvent::Sidecar {
            state: SidecarState::Recovered,
            generation: 0,
            container: None,
            detail: Some("torn 19 bytes".into()),
        }
    );
    assert!(std::fs::read(file_path(&store)).unwrap().starts_with(&good));
}

#[test]
fn an_interrupted_seal_is_completed_and_read_once_meanwhile() {
    let (_root, _ownership, store) = setup();
    let mut record = open_small(&store);
    for id in 1..=5 {
        record.append(id, open_event(id, "a.example")).unwrap();
    }
    record.sync().unwrap();
    let full = std::fs::read(file_path(&store)).unwrap();
    // The sixth append seals events 1-5 first.
    record.append(6, open_event(6, "a.example")).unwrap();
    assert_eq!(record.sealed_segments(), 1);
    drop(record);
    // The system stopped after the sealed copy was published and before the
    // new active segment replaced the old one, and lost the old one's last
    // write, which was never synced, leaving part of it.
    let last_line = full[..full.len() - 1]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .unwrap()
        + 1;
    let mut stale = full[..last_line].to_vec();
    stale.extend_from_slice(b"{\"v\":1,\"se");
    std::fs::write(file_path(&store), &stale).unwrap();

    // Meanwhile a reader takes the events from the sealed copy, once each,
    // and writes nothing.
    let (lines, stats) = NetworkRecord::read_existing(&store, None, 100)
        .unwrap()
        .unwrap();
    assert_eq!(seqs(&lines), [1, 2, 3, 4, 5]);
    assert_eq!((stats.events, stats.last_seq), (5, 5));
    let opens = NetworkRecord::read_existing_matching(&store, "open", |_| true, 100)
        .unwrap()
        .unwrap();
    assert_eq!(seqs(&opens), [1, 2, 3, 4, 5]);
    assert_eq!(std::fs::read(file_path(&store)).unwrap(), stale);

    // Opening completes the seal: the event whose write was lost is in the
    // sealed copy, and the sixth, written after the crash point, never was.
    let mut record = open_small(&store);
    assert_eq!(record.sealed_segments(), 1);
    assert_eq!(page_all(&record, 2), [1, 2, 3, 4, 5]);
    assert_eq!(record.append(7, open_event(7, "a.example")).unwrap(), 6);
    drop(record);

    // An active segment that is not the start of its sealed copy is refused.
    let sealed = std::fs::read(sealed_path(&store, 0)).unwrap();
    let header_end = sealed.iter().position(|byte| *byte == b'\n').unwrap() + 1;
    let mut forged = sealed[..header_end].to_vec();
    forged.extend(line_bytes(1, limit_event()));
    std::fs::write(file_path(&store), &forged).unwrap();
    assert!(try_open_small(&store).is_err());
}

#[test]
fn a_sealed_segment_cannot_change_disappear_or_move() {
    let (_root, _ownership, store) = setup();
    let mut record = open_small(&store);
    for id in 1..=23 {
        record.append(id, open_event(id, "a.example")).unwrap();
    }
    drop(record);
    let first = sealed_path(&store, 0);
    let second = sealed_path(&store, 1);
    let original = std::fs::read(&first).unwrap();
    let text = String::from_utf8(original.clone()).unwrap();

    // One changed byte breaks the seal's digest, for a writer and a reader.
    std::fs::write(&first, text.replacen("g1:2", "g1:9", 1)).unwrap();
    assert!(try_open_small(&store).is_err());
    assert!(NetworkRecord::read_existing(&store, None, 100).is_err());

    // A segment resealed with a matching digest still breaks the chain.
    let lines: Vec<&str> = text.lines().collect();
    let body = format!("{}\n", lines[..6].join("\n")).replacen("g1:2", "g1:9", 1);
    let (resealed, _) = segments::sealed_file(body.as_bytes(), 5).unwrap();
    std::fs::write(&first, &resealed).unwrap();
    let error = try_open_small(&store).unwrap_err().to_string();
    assert!(error.contains("chain is broken"), "{error}");

    // A seal that counts other events is refused too.
    let (miscounted, _) =
        segments::sealed_file(format!("{}\n", lines[..6].join("\n")).as_bytes(), 4).unwrap();
    std::fs::write(&first, &miscounted).unwrap();
    assert!(try_open_small(&store).is_err());
    std::fs::write(&first, &original).unwrap();
    drop(open_small(&store));

    // A removed segment leaves a gap; swapping two breaks the chain.
    let aside = directory(&store).join("aside");
    std::fs::rename(&first, &aside).unwrap();
    let error = try_open_small(&store).unwrap_err().to_string();
    assert!(error.contains("not contiguous"), "{error}");
    std::fs::rename(&second, &first).unwrap();
    std::fs::rename(&aside, &second).unwrap();
    assert!(try_open_small(&store).is_err());
    std::fs::rename(&second, &aside).unwrap();
    std::fs::rename(&first, &second).unwrap();
    std::fs::rename(&aside, &first).unwrap();
    assert_eq!(page_all(&open_small(&store), 50).len(), 23);
}

#[test]
fn a_single_file_record_is_moved_into_segments() {
    let (_root, _ownership, store) = setup();
    // Twelve events with a gap after the fifth and an interrupted append,
    // as Axocoatl 1.2.0 left them.
    let mut bytes = Vec::new();
    for seq in (1..=5).chain(7..=13) {
        bytes.extend(line_bytes(seq, open_event(seq, "a.example")));
    }
    bytes.extend(line_bytes(14, web_event("act-14")));
    let good = bytes.len();
    bytes.extend_from_slice(b"{\"v\":1,\"seq\":15,\"ts");
    single_file_record(&store, &bytes);

    // Until a writer opens it, it reads as before.
    let (lines, stats) = NetworkRecord::read_existing(&store, Some(4), 3)
        .unwrap()
        .unwrap();
    assert_eq!(seqs(&lines), [5, 7, 8]);
    assert_eq!(
        (stats.events, stats.bytes, stats.gaps, stats.last_seq),
        (13, good as u64, 1, 14)
    );
    let web = NetworkRecord::read_existing_matching(&store, "web", |_| true, 10)
        .unwrap()
        .unwrap();
    assert_eq!(seqs(&web), [14]);

    let record = open_small(&store);
    let expected: Vec<u64> = (1..=5).chain(7..=15).collect();
    let lines = record.read_after(None, 100).unwrap();
    assert_eq!(seqs(&lines), expected);
    assert_eq!(lines[12].event, web_event("act-14"));
    assert_eq!(
        lines[13].event,
        NetworkEvent::Sidecar {
            state: SidecarState::Recovered,
            generation: 0,
            container: None,
            detail: Some("torn 19 bytes".into()),
        }
    );
    let stats = record.stats();
    assert_eq!((stats.events, stats.gaps, stats.last_seq), (14, 1, 15));
    // Every migrated event is sealed, five to a segment.
    assert_eq!(record.sealed_segments(), 3);
    drop(record);
    assert_eq!(
        std::fs::read(directory(&store).join(NETWORK_RECORD_FILE)).unwrap(),
        segments::head()
    );
    // A reader of the segments agrees; reopening moves nothing again.
    let (read, read_stats) = NetworkRecord::read_existing(&store, None, 100)
        .unwrap()
        .unwrap();
    assert_eq!(seqs(&read), expected);
    assert_eq!(read_stats, stats);
    let mut record = open_small(&store);
    assert_eq!(record.stats(), stats);
    assert_eq!(record.append(16, limit_event()).unwrap(), 16);
}

#[test]
fn axocoatl_1_2_0_refuses_the_head_instead_of_reading_part_of_the_record() {
    // 1.2.0 parsed the primary file line by line as the whole record, as
    // `load` and `matching_lines` still do for an unmigrated record. The
    // head's first line is not a network line and not the last line, so its
    // writer and its reader report damage rather than cutting the line as a
    // torn tail, and its search for `web` events reports the same.
    let head = segments::head();
    assert!(matches!(
        load(&head),
        Err(NetworkRecordError::Damaged { line: 1, .. })
    ));
    assert!(matches!(
        matching_lines(&head, "web", |_| true, 10),
        Err(NetworkRecordError::Damaged { .. })
    ));
    assert!(segments::is_head(&head).unwrap());
    // A head this version does not know is refused, not read as one file.
    let newer = String::from_utf8(head)
        .unwrap()
        .replacen("\"schema\":1", "\"schema\":2", 1);
    assert!(segments::is_head(newer.as_bytes()).is_err());
}

#[test]
fn an_interrupted_move_into_segments_is_done_again() {
    let (_root, _ownership, store) = setup();
    let mut bytes = Vec::new();
    for seq in 1..=3 {
        bytes.extend(line_bytes(seq, open_event(seq, "a.example")));
    }
    single_file_record(&store, &bytes);
    // A move that stopped before the head replaced the single file leaves
    // segments behind, and then Axocoatl 1.2.0 appended to the single file.
    let segment_dir = directory(&store).join(SEGMENTS_DIR);
    std::fs::create_dir(&segment_dir).unwrap();
    std::fs::write(segment_dir.join(segments::sealed_name(0)), b"partial").unwrap();
    std::fs::write(file_path(&store), b"{\"header\":").unwrap();
    let mut primary = std::fs::OpenOptions::new()
        .append(true)
        .open(directory(&store).join(NETWORK_RECORD_FILE))
        .unwrap();
    primary
        .write_all(&line_bytes(4, open_event(4, "a.example")))
        .unwrap();
    drop(primary);
    let record = open_small(&store);
    assert_eq!(page_all(&record, 10), [1, 2, 3, 4]);
    assert_eq!(record.sealed_segments(), 1);
    drop(record);
    assert_eq!(
        std::fs::read(directory(&store).join(NETWORK_RECORD_FILE)).unwrap(),
        segments::head()
    );
}

#[test]
fn lines_of_some_kinds_are_read_one_segment_at_a_time() {
    let (_root, _ownership, store) = setup();
    let mut record = open_small(&store);
    let mut expected = Vec::new();
    for id in 1..=40u64 {
        let event = if id % 7 == 0 {
            expected.push(id);
            policy_event(id)
        } else if id % 11 == 0 {
            expected.push(id);
            sidecar_event(id as u32)
        } else {
            open_event(id, "a.example")
        };
        record.append(id, event).unwrap();
    }
    for max in [1, 2, 1000] {
        let mut found = Vec::new();
        let mut after = None;
        let mut calls = 0;
        loop {
            calls += 1;
            let page = record
                .read_kinds_after(after, &["policy", "sidecar"], max)
                .unwrap();
            assert!(page.lines.len() <= max);
            assert!(page
                .lines
                .iter()
                .all(|line| matches!(line.event.kind(), "policy" | "sidecar")));
            found.extend(seqs(&page.lines));
            if page.done {
                assert_eq!(page.next_after, Some(40));
                break;
            }
            assert!(page.next_after > after, "{page:?}");
            after = page.next_after;
        }
        assert_eq!(found, expected, "{max}");
        // At most one segment per call: eight segments of five events.
        assert!(calls >= 8, "{max}: {calls}");
    }
    // The highest generation named: sidecar 33, above the open events' g1.
    assert_eq!(record.stats().max_generation, 33);
    drop(record);
    assert_eq!(open_small(&store).stats().max_generation, 33);
}

#[test]
fn the_newest_lines_of_a_kind_are_found_across_segments() {
    let (_root, _ownership, store) = setup();
    let mut record = open_small(&store);
    for id in 1..=30u64 {
        let event = if id % 4 == 0 {
            web_event(&format!("act-{id}"))
        } else {
            open_event(id, "a.example")
        };
        record.append(id, event).unwrap();
    }
    let activations = |lines: Vec<NetworkLine>| -> Vec<String> {
        lines
            .iter()
            .map(|line| match &line.event {
                NetworkEvent::Web { activation_id, .. } => activation_id.clone(),
                _ => unreachable!(),
            })
            .collect()
    };
    let read = |keep: &dyn Fn(&NetworkEvent) -> bool, max: usize| {
        NetworkRecord::read_existing_matching(&store, "web", keep, max)
            .unwrap()
            .unwrap()
    };
    assert_eq!(
        activations(read(&|_| true, 3)),
        ["act-20", "act-24", "act-28"]
    );
    assert_eq!(activations(read(&|_| true, 100)).len(), 7);
    let old = |event: &NetworkEvent| matches!(event, NetworkEvent::Web { activation_id, .. } if activation_id == "act-4");
    assert_eq!(activations(read(&old, 1)), ["act-4"]);
}

#[test]
fn a_last_line_of_another_version_is_refused_not_cut() {
    let (_root, _ownership, store) = setup();
    let mut record = open_small(&store);
    record.append(1, limit_event()).unwrap();
    drop(record);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(file_path(&store))
        .unwrap();
    file.write_all(
        br#"{"v":2,"seq":2,"ts_ms":1,"event":{"kind":"limit","what":"record_full","detail":"x"}}
"#,
    )
    .unwrap();
    drop(file);
    let before = std::fs::read(file_path(&store)).unwrap();
    assert!(matches!(
        try_open_small(&store),
        Err(NetworkRecordError::Damaged { line: 3, .. })
    ));
    assert_eq!(std::fs::read(file_path(&store)).unwrap(), before);
}
