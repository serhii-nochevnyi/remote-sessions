#[cfg(any(target_os = "ios", test))]
mod recorder {
    use std::sync::Mutex;

    const CASE_IDS: &[&str] = &[
        "english-return",
        "ukrainian-return",
        "composition-commit",
        "composition-replacement",
        "composition-cancel",
        "paste-short",
        "paste-multiline",
        "touch-selection-copy",
        "touch-scroll",
        "keyboard-open-cursor",
        "rotation",
        "app-switch",
        "lock-foreground",
    ];
    const EVENT_BYTES: usize = 1024;
    const APPEND_EVENTS: usize = 16;
    const APPEND_BYTES: usize = 4096;
    const ATTEMPT_EVENTS: usize = 256;
    const ATTEMPT_BYTES: usize = 16384;

    #[derive(Clone, Debug, PartialEq, Eq)]
    #[cfg_attr(target_os = "ios", derive(serde::Serialize))]
    #[cfg_attr(target_os = "ios", serde(rename_all = "camelCase"))]
    struct Snapshot {
        case_id: &'static str,
        generation: u32,
        next_sequence: u32,
        bytes: Vec<u8>,
        event_lengths: Vec<usize>,
        rejected: u32,
        invalid: bool,
    }

    #[derive(Default)]
    struct Recorder {
        generation: u32,
        active: Option<Snapshot>,
    }

    #[cfg(target_os = "ios")]
    type RecorderState<'a> = tauri::State<'a, Mutex<Recorder>>;

    #[cfg(all(test, not(target_os = "ios")))]
    type RecorderState<'a> = &'a Mutex<Recorder>;

    impl Recorder {
        fn begin(&mut self, case_id: &str) -> Result<u32, &'static str> {
            let case_id = CASE_IDS
                .iter()
                .copied()
                .find(|allowed| *allowed == case_id)
                .ok_or("unknown fixture case")?;
            let generation = self
                .generation
                .checked_add(1)
                .ok_or("generation exhausted")?;
            self.active = Some(Snapshot {
                case_id,
                generation,
                next_sequence: 1,
                bytes: Vec::new(),
                event_lengths: Vec::new(),
                rejected: 0,
                invalid: false,
            });
            self.generation = generation;
            Ok(generation)
        }

        fn append(
            &mut self,
            generation: u32,
            sequence: u32,
            events: Vec<Vec<u8>>,
        ) -> Result<(), &'static str> {
            let active = self.active.as_mut().ok_or("no active attempt")?;
            if active.generation != generation {
                return Err("stale generation");
            }
            if let Err(error) = active.validate_append(sequence, &events) {
                active.rejected = active.rejected.saturating_add(1);
                active.invalid = true;
                return Err(error);
            }
            for event in events {
                active.event_lengths.push(event.len());
                active.bytes.extend(event);
            }
            active.next_sequence += 1;
            Ok(())
        }

        fn read(&self, generation: u32) -> Result<Snapshot, &'static str> {
            let active = self.active.as_ref().ok_or("no active attempt")?;
            if active.generation != generation {
                return Err("stale generation");
            }
            Ok(active.clone())
        }
    }

    impl Snapshot {
        fn validate_append(&self, sequence: u32, events: &[Vec<u8>]) -> Result<(), &'static str> {
            if sequence != self.next_sequence {
                return Err("invalid append sequence");
            }
            if events.is_empty() || events.len() > APPEND_EVENTS {
                return Err("invalid append event count");
            }
            if events
                .iter()
                .any(|event| !(1..=EVENT_BYTES).contains(&event.len()))
            {
                return Err("invalid event byte count");
            }
            let bytes = events.iter().map(Vec::len).sum::<usize>();
            if bytes > APPEND_BYTES {
                return Err("append byte limit exceeded");
            }
            if self.event_lengths.len() + events.len() > ATTEMPT_EVENTS
                || self.bytes.len() + bytes > ATTEMPT_BYTES
            {
                return Err("attempt capacity exceeded");
            }
            Ok(())
        }
    }

    #[cfg_attr(target_os = "ios", tauri::command)]
    fn w2_begin_attempt(state: RecorderState<'_>, case_id: String) -> Result<u32, &'static str> {
        state
            .lock()
            .map_err(|_| "recorder unavailable")?
            .begin(&case_id)
    }

    #[cfg_attr(target_os = "ios", tauri::command)]
    fn w2_append_input(
        state: RecorderState<'_>,
        generation: u32,
        sequence: u32,
        events: Vec<Vec<u8>>,
    ) -> Result<(), &'static str> {
        state
            .lock()
            .map_err(|_| "recorder unavailable")?
            .append(generation, sequence, events)
    }

    #[cfg_attr(target_os = "ios", tauri::command)]
    fn w2_read_attempt(
        state: RecorderState<'_>,
        generation: u32,
    ) -> Result<Snapshot, &'static str> {
        state
            .lock()
            .map_err(|_| "recorder unavailable")?
            .read(generation)
    }

    #[cfg(target_os = "ios")]
    #[cfg_attr(mobile, tauri::mobile_entry_point)]
    pub fn run() {
        tauri::Builder::default()
            .manage(Mutex::new(Recorder::default()))
            .invoke_handler(tauri::generate_handler![
                w2_begin_attempt,
                w2_append_input,
                w2_read_attempt,
            ])
            .run(tauri::generate_context!())
            .expect("failed to run the offline terminal fixture");
    }

    #[cfg(test)]
    #[cfg(not(target_os = "ios"))]
    mod tests {
        use super::*;
        use std::io::{self, BufRead, Write};

        const SYNTHETIC_CASE: &str = "ukrainian-return";
        const SYNTHETIC_BYTES: [u8; 3] = [0xd1, 0x97, 0x0d];

        #[derive(Debug, PartialEq, Eq)]
        enum Request {
            Begin(String),
            Append(u32, u32, Vec<Vec<u8>>),
            Read(u32),
        }

        fn parse_request(line: &str) -> Request {
            let parts: Vec<_> = line.split('\t').collect();
            match parts.as_slice() {
                ["w2_begin_attempt", case_id] => Request::Begin((*case_id).to_owned()),
                ["w2_append_input", generation, sequence, batch] => Request::Append(
                    generation.parse().unwrap(),
                    sequence.parse().unwrap(),
                    batch
                        .split(';')
                        .map(|event| {
                            event
                                .split(',')
                                .map(|byte| byte.parse::<u8>().unwrap())
                                .collect()
                        })
                        .collect(),
                ),
                ["w2_read_attempt", generation] => Request::Read(generation.parse().unwrap()),
                _ => panic!("invalid tracer request: {line:?}"),
            }
        }

        fn assert_snapshot(
            state: RecorderState<'_>,
            generation: u32,
            bytes: &[u8],
            event_lengths: &[usize],
        ) -> Snapshot {
            let snapshot = w2_read_attempt(state, generation)
                .expect("read wrapper must return the active snapshot");
            assert_eq!(
                snapshot,
                Snapshot {
                    case_id: SYNTHETIC_CASE,
                    generation,
                    next_sequence: event_lengths.len() as u32 + 1,
                    bytes: bytes.into(),
                    event_lengths: event_lengths.into(),
                    rejected: 0,
                    invalid: false,
                },
                "read wrapper must return the exact snapshot"
            );
            snapshot
        }

        fn snapshot_json(snapshot: &Snapshot) -> String {
            format!(
                "{{\"caseId\":{:?},\"generation\":{},\"nextSequence\":{},\"bytes\":{:?},\"eventLengths\":{:?},\"rejected\":{},\"invalid\":{}}}",
                snapshot.case_id,
                snapshot.generation,
                snapshot.next_sequence,
                snapshot.bytes,
                snapshot.event_lengths,
                snapshot.rejected,
                snapshot.invalid,
            )
        }

        fn serve_requests(state: RecorderState<'_>) {
            let mut stdout = io::stdout().lock();
            for line in io::stdin().lock().lines() {
                let response = match parse_request(&line.unwrap()) {
                    Request::Begin(case_id) => {
                        w2_begin_attempt(state, case_id).unwrap().to_string()
                    }
                    Request::Append(generation, sequence, events) => {
                        w2_append_input(state, generation, sequence, events).unwrap();
                        String::from("null")
                    }
                    Request::Read(generation) => {
                        snapshot_json(&w2_read_attempt(state, generation).unwrap())
                    }
                };
                writeln!(stdout, "W2\t{response}").unwrap();
                stdout.flush().unwrap();
            }
        }

        #[test]
        fn w2_recorder_manifest_vectors() {
            let built_in = concat!(
                "english-return\t\t6f6b0d\n",
                "ukrainian-return\t\td1970d\n",
                "composition-commit\t\tc3a9\n",
                "composition-replacement\t\tc3a8\n",
                "composition-cancel\t\t\n",
                "paste-short\t\td087d0b6\n",
                "paste-multiline\t\td1970dc3a90de7958c\n",
                "touch-selection-copy\t\t\n",
                "touch-scroll\t\t\n",
                "keyboard-open-cursor\t\t\n",
                "rotation\t\t\n",
                "app-switch\t61\t62\n",
                "lock-foreground\t63\t64",
            );
            let supplied = std::env::var("W2_MANIFEST_VECTORS").ok();
            let vectors: Vec<_> = supplied
                .as_deref()
                .unwrap_or(built_in)
                .lines()
                .map(|line| {
                    let fields: Vec<_> = line.split('\t').collect();
                    assert_eq!(fields.len(), 3, "case ID and two hex segments");
                    (fields[0], [fields[1], fields[2]])
                })
                .collect();
            let mut ids: Vec<_> = vectors.iter().map(|(id, _)| *id).collect();
            ids.sort_unstable();
            let mut allowed = CASE_IDS.to_vec();
            allowed.sort_unstable();
            assert_eq!(ids, allowed, "exact native case allowlist");

            let state = &Mutex::new(Recorder::default());
            let mut previous = 0;
            for (case_id, segments) in vectors {
                for hex in segments {
                    assert!(hex.len().is_multiple_of(2));
                    let bytes: Vec<_> = (0..hex.len())
                        .step_by(2)
                        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
                        .collect();
                    assert_eq!(w2_begin_attempt(state, case_id.into()), Ok(previous + 1));
                    let generation = previous + 1;
                    previous = generation;
                    let empty = w2_read_attempt(state, generation).unwrap();
                    assert_eq!(empty.case_id, case_id);
                    assert!(empty.bytes.is_empty());
                    assert!(empty.event_lengths.is_empty());

                    let mut sequence = 1;
                    let mut lengths = Vec::new();
                    if !bytes.is_empty() {
                        assert_eq!(
                            w2_append_input(state, generation, sequence, vec![bytes.clone()]),
                            Ok(())
                        );
                        sequence += 1;
                        lengths.push(bytes.len());
                    }
                    assert_eq!(
                        w2_read_attempt(state, generation),
                        Ok(Snapshot {
                            bytes: bytes.clone(),
                            next_sequence: sequence,
                            event_lengths: lengths,
                            ..empty
                        })
                    );

                    let mut retained = bytes.clone();
                    while retained.len() < ATTEMPT_BYTES {
                        let filler = vec![0x5a; EVENT_BYTES.min(ATTEMPT_BYTES - retained.len())];
                        assert_eq!(
                            w2_append_input(state, generation, sequence, vec![filler.clone()]),
                            Ok(())
                        );
                        retained.extend(filler);
                        sequence += 1;
                    }
                    let full = w2_read_attempt(state, generation).unwrap();
                    assert_eq!(full.bytes, retained);
                    assert_eq!(
                        w2_append_input(state, generation, sequence, vec![vec![0x5a]]),
                        Err("attempt capacity exceeded")
                    );
                    assert_eq!(
                        w2_read_attempt(state, generation),
                        Ok(Snapshot {
                            rejected: 1,
                            invalid: true,
                            ..full
                        })
                    );
                }
            }
            assert_eq!(
                w2_begin_attempt(state, "unknown".into()),
                Err("unknown fixture case")
            );
        }

        #[test]
        fn w2_recorder_tracer() {
            let state = &Mutex::new(Recorder::default());
            let mode = std::env::var("W2_TRACER_CASE").ok();
            if mode.as_deref() == Some("requests") {
                serve_requests(state);
                return;
            }
            let events: &[&[u8]] = match mode.as_deref() {
                None | Some("combined") => &[&SYNTHETIC_BYTES],
                Some("split") => &[&SYNTHETIC_BYTES[..2], &SYNTHETIC_BYTES[2..]],
                Some(other) => panic!("unexpected tracer mode: {other}"),
            };
            let mut expected = vec![Request::Begin(SYNTHETIC_CASE.into())];
            expected.extend(
                events.iter().enumerate().map(|(index, event)| {
                    Request::Append(1, index as u32 + 1, vec![event.to_vec()])
                }),
            );
            expected.push(Request::Read(1));
            let lengths: Vec<_> = events.iter().map(|event| event.len()).collect();

            let mut stdin = io::stdin().lock().lines();
            let mut stdout = io::stdout().lock();
            for expected in expected {
                let request = if mode.is_some() {
                    let received =
                        parse_request(&stdin.next().expect("missing tracer request").unwrap());
                    assert_eq!(received, expected, "actual ordered IPC request");
                    received
                } else {
                    expected
                };
                let response = match request {
                    Request::Begin(case_id) => {
                        assert_eq!(
                            w2_begin_attempt(state, case_id),
                            Ok(1),
                            "begin wrapper must start generation 1"
                        );
                        assert_snapshot(state, 1, &[], &[]);
                        String::from("1")
                    }
                    Request::Append(generation, sequence, events) => {
                        assert_eq!(
                            w2_append_input(state, generation, sequence, events),
                            Ok(()),
                            "append wrapper must accept the event"
                        );
                        let count = sequence as usize;
                        let accepted = lengths[..count].iter().sum::<usize>();
                        let snapshot = w2_read_attempt(state, generation)
                            .expect("read wrapper must return the active snapshot");
                        assert_eq!(
                            snapshot.bytes,
                            SYNTHETIC_BYTES[..accepted],
                            "append wrapper must commit the event"
                        );
                        assert_snapshot(
                            state,
                            generation,
                            &SYNTHETIC_BYTES[..accepted],
                            &lengths[..count],
                        );
                        String::from("null")
                    }
                    Request::Read(generation) => snapshot_json(&assert_snapshot(
                        state,
                        generation,
                        &SYNTHETIC_BYTES,
                        &lengths,
                    )),
                };
                if mode.is_some() {
                    writeln!(stdout, "W2\t{response}").unwrap();
                    stdout.flush().unwrap();
                }
            }
            if mode.is_some() {
                assert!(stdin.next().is_none(), "unexpected extra tracer request");
            }

            assert_eq!(w2_begin_attempt(state, SYNTHETIC_CASE.into()), Ok(2));
            assert_eq!(
                w2_append_input(state, 1, 1, vec![vec![1]]),
                Err("stale generation")
            );
            assert_eq!(w2_read_attempt(state, 1), Err("stale generation"));
            assert_snapshot(state, 2, &[], &[]);
            assert_eq!(
                w2_append_input(state, 2, 1, vec![SYNTHETIC_BYTES.to_vec()]),
                Ok(())
            );
            assert_snapshot(state, 2, &SYNTHETIC_BYTES, &[3]);
        }

        #[test]
        fn stale_duplicate_future_and_invalid_requests_preserve_accepted_events() {
            let state = &Mutex::new(Recorder::default());
            assert_eq!(w2_read_attempt(state, 1), Err("no active attempt"));
            assert_eq!(
                w2_append_input(state, 1, 1, vec![vec![1]]),
                Err("no active attempt")
            );
            assert_eq!(w2_begin_attempt(state, SYNTHETIC_CASE.into()), Ok(1));
            assert_eq!(
                w2_append_input(state, 1, 1, vec![SYNTHETIC_BYTES.to_vec()]),
                Ok(())
            );
            let accepted = assert_snapshot(state, 1, &SYNTHETIC_BYTES, &[3]);
            assert_eq!(
                w2_begin_attempt(state, "unknown".into()),
                Err("unknown fixture case")
            );
            assert_eq!(
                w2_append_input(state, 0, 2, vec![vec![1]]),
                Err("stale generation")
            );
            assert_eq!(w2_read_attempt(state, 1), Ok(accepted.clone()));

            let cases = [
                (0, vec![vec![1]], "invalid append sequence"),
                (1, vec![vec![1]], "invalid append sequence"),
                (3, vec![vec![1]], "invalid append sequence"),
                (2, vec![], "invalid append event count"),
                (2, vec![vec![1], vec![]], "invalid event byte count"),
                (2, vec![vec![1], vec![0; 1025]], "invalid event byte count"),
                (2, vec![vec![1]; 17], "invalid append event count"),
                (
                    2,
                    [vec![vec![0; 1024]; 4], vec![vec![0]]].concat(),
                    "append byte limit exceeded",
                ),
            ];
            for (index, (sequence, events, error)) in cases.into_iter().enumerate() {
                assert_eq!(w2_append_input(state, 1, sequence, events), Err(error));
                assert_eq!(
                    w2_read_attempt(state, 1),
                    Ok(Snapshot {
                        rejected: index as u32 + 1,
                        invalid: true,
                        ..accepted.clone()
                    })
                );
            }

            assert_eq!(w2_begin_attempt(state, SYNTHETIC_CASE.into()), Ok(2));
            assert_snapshot(state, 2, &[], &[]);
        }

        #[test]
        fn recorder_accepts_exact_limits_and_rejects_overflow_atomically() {
            for (events, appends, expected_bytes, expected_events) in [
                (vec![vec![5; 1024]; 4], 4, 16384, 16),
                (vec![vec![5]; 16], 16, 256, 256),
            ] {
                let state = &Mutex::new(Recorder::default());
                assert_eq!(w2_begin_attempt(state, SYNTHETIC_CASE.into()), Ok(1));
                for sequence in 1..=appends {
                    assert_eq!(w2_append_input(state, 1, sequence, events.clone()), Ok(()));
                }
                let full = w2_read_attempt(state, 1).unwrap();
                assert_eq!(full.bytes, vec![5; expected_bytes]);
                assert_eq!(full.event_lengths, vec![events[0].len(); expected_events]);
                assert_eq!(full.next_sequence, appends + 1);
                assert_eq!(full.rejected, 0);
                assert!(!full.invalid);
                assert_eq!(
                    w2_append_input(state, 1, appends + 1, vec![vec![5]]),
                    Err("attempt capacity exceeded")
                );
                assert_eq!(
                    w2_read_attempt(state, 1),
                    Ok(Snapshot {
                        rejected: 1,
                        invalid: true,
                        ..full
                    })
                );
            }
        }

        #[test]
        fn attempt_overflow_preserves_room_for_a_valid_event() {
            let state = &Mutex::new(Recorder::default());
            assert_eq!(w2_begin_attempt(state, SYNTHETIC_CASE.into()), Ok(1));
            let full_batch = vec![vec![5; 1024]; 4];
            for sequence in 1..=3 {
                assert_eq!(
                    w2_append_input(state, 1, sequence, full_batch.clone()),
                    Ok(())
                );
            }
            let mut final_batch = full_batch;
            final_batch[0].pop();
            assert_eq!(w2_append_input(state, 1, 4, final_batch), Ok(()));
            let before = w2_read_attempt(state, 1).unwrap();
            assert_eq!(before.bytes.len(), 16383);
            assert_eq!(
                w2_append_input(state, 1, 5, vec![vec![6], vec![7]]),
                Err("attempt capacity exceeded")
            );
            assert_eq!(
                w2_read_attempt(state, 1),
                Ok(Snapshot {
                    rejected: 1,
                    invalid: true,
                    ..before
                })
            );
            assert_eq!(w2_append_input(state, 1, 5, vec![vec![8]]), Ok(()));
            let full = w2_read_attempt(state, 1).unwrap();
            assert_eq!(full.bytes.len(), 16384);
            assert_eq!(full.bytes.last(), Some(&8));
            assert_eq!(full.next_sequence, 6);
        }

        #[test]
        fn begin_does_not_wrap_generation() {
            let state = &Mutex::new(Recorder {
                generation: u32::MAX - 1,
                active: None,
            });
            assert_eq!(w2_begin_attempt(state, SYNTHETIC_CASE.into()), Ok(u32::MAX));
            let active = w2_read_attempt(state, u32::MAX).unwrap();
            assert_eq!(
                w2_begin_attempt(state, SYNTHETIC_CASE.into()),
                Err("generation exhausted")
            );
            assert_eq!(w2_read_attempt(state, u32::MAX), Ok(active));
        }
    }
}

#[cfg(target_os = "ios")]
pub use recorder::run;
