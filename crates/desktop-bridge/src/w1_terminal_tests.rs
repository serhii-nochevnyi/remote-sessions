use super::*;
use portable_pty::{ChildKiller, ExitStatus};
use std::{cell::Cell, sync::mpsc as thread_channel, thread::JoinHandle};

struct ControlledReader {
    steps: thread_channel::Receiver<std::io::Result<Vec<u8>>>,
    requests: mpsc::UnboundedSender<usize>,
    exit: Option<Arc<ExitState>>,
}

#[derive(Default)]
struct ExitState {
    held: AtomicBool,
    dropped: AtomicBool,
}

impl Drop for ControlledReader {
    fn drop(&mut self) {
        if let Some(exit) = &self.exit {
            while exit.held.load(Ordering::Acquire) {
                std::thread::sleep(CLEANUP_POLL);
            }
            exit.dropped.store(true, Ordering::Release);
        }
    }
}

impl Read for ControlledReader {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if self.requests.send(bytes.len()).is_err() {
            return Ok(0);
        }
        let Ok(step) = self.steps.recv_timeout(Duration::from_secs(5)) else {
            return Ok(0);
        };
        let data = step?;
        bytes[..data.len()].copy_from_slice(&data);
        Ok(data.len())
    }
}

struct ReadControl {
    steps: thread_channel::Sender<std::io::Result<Vec<u8>>>,
    requests: mpsc::UnboundedReceiver<usize>,
    thread: Option<JoinHandle<()>>,
    child: Option<AllowReap>,
    exit: Option<AllowExit>,
}

struct AllowExit(Arc<ExitState>);

impl Drop for AllowExit {
    fn drop(&mut self) {
        self.0.held.store(false, Ordering::Release);
    }
}

impl ReadControl {
    async fn requested(&mut self) -> usize {
        tokio::time::timeout(Duration::from_secs(2), self.requests.recv())
            .await
            .expect("PTY reader did not request another read")
            .expect("PTY reader stopped")
    }

    fn data(&self, bytes: &[u8]) {
        self.steps.send(Ok(bytes.to_vec())).unwrap();
    }

    fn would_block(&self) {
        self.steps.send(Err(ErrorKind::WouldBlock.into())).unwrap();
    }

    fn finish(mut self) {
        self.child.take();
        if let Some(thread) = self.thread.take() {
            drop(self.steps);
            thread.join().unwrap();
        }
    }
}

fn controlled_reader(state: &Arc<AtomicU64>) -> (ReadControl, mpsc::Receiver<PtyEvent>) {
    let (steps, commands) = thread_channel::channel();
    let (requests, waits) = mpsc::unbounded_channel();
    let (sender, events) = mpsc::channel(OUTPUT_SLOTS);
    let reader = state.clone();
    let thread = std::thread::spawn(move || {
        let mut publisher = PtyPublisher::new(sender);
        read_pty(
            Box::new(ControlledReader {
                steps: commands,
                requests,
                exit: None,
            }),
            &reader,
            |event| publisher.publish(event).is_none(),
            |_| {},
        );
    });
    (
        ReadControl {
            steps,
            requests: waits,
            thread: Some(thread),
            child: None,
            exit: None,
        },
        events,
    )
}

async fn read_event(events: &mut mpsc::Receiver<PtyEvent>) -> Option<PtyEvent> {
    tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("PTY reader did not publish an event")
}

async fn expect_bytes(events: &mut mpsc::Receiver<PtyEvent>, expected: &[u8]) {
    let Some(PtyEvent::Bytes(bytes)) = read_event(events).await else {
        panic!("expected original PTY bytes");
    };
    let bytes = std::mem::take(&mut *bytes.lock().unwrap());
    assert_eq!(bytes, expected);
    assert!(bytes.len() <= OUTPUT_CHUNK_SIZE);
}

#[tokio::test]
async fn w1_resize_w1_terminal_reader_barrier_waits_for_complete_output_and_ack() {
    let state = Arc::new(AtomicU64::new(RESUME));
    let (mut control, mut events) = controlled_reader(&state);
    assert_eq!(control.requested().await, OUTPUT_CHUNK_SIZE);
    control.data(b"prefix\xe1\x80");
    expect_bytes(&mut events, b"prefix").await;
    assert_eq!(control.requested().await, OUTPUT_CHUNK_SIZE - 2);

    state.store(DRAIN, Ordering::Release);
    for _ in 0..2 {
        control.would_block();
        assert_eq!(control.requested().await, OUTPUT_CHUNK_SIZE - 2);
        assert!(events.try_recv().is_err());
    }
    control.data(b"\x80\x1b[");
    expect_bytes(&mut events, b"\xe1\x80\x80\x1b[").await;
    assert_eq!(control.requested().await, OUTPUT_CHUNK_SIZE);
    for _ in 0..2 {
        control.would_block();
        assert_eq!(control.requested().await, OUTPUT_CHUNK_SIZE);
        assert_eq!(state.load(Ordering::Acquire), DRAIN);
        assert!(events.try_recv().is_err());
    }
    control.data(b"2H");
    expect_bytes(&mut events, b"2H").await;
    control.requested().await;
    control.would_block();
    assert!(matches!(
        read_event(&mut events).await,
        Some(PtyEvent::Boundary)
    ));
    assert_eq!(state.load(Ordering::Acquire), PAUSE);
    assert!(events.try_recv().is_err());
    assert!(control.requests.try_recv().is_err());
    state.store(RESUME, Ordering::Release);
    control.requested().await;
    control.data(b"post-grid");
    expect_bytes(&mut events, b"post-grid").await;
    control.finish();
    assert!(read_event(&mut events).await.is_none());
}

#[tokio::test]
async fn w1_terminal_reader_matches_vt_transitions_and_cancellation() {
    let cases: &[[&[u8]; 4]] = &[
        [b"\x1b]", b"2;title", b"", b"\x07A"],
        [b"\x1b]2;title", b"\x1b", b"", b"\\A"],
        [b"\x1b]2;cancel", b"\xef\xbb\xbf", b"", b"\x18A"],
        [b"\x1bP", b"1qcancel\x07", b"", b"\x1aA"],
        [b"\x1bP", b"1?ignoredq", b"", b"\xc2\x9cA"],
        [b"\x1bX", b"payload\x07", b"", b"\x1b\\A"],
        [b"\x1b^", b"payload", b"", b"\xc2\x9cA"],
        [b"\x1b_", b"payload", b"", b"\xc3\xa9A"],
        [b"\x1b", b"(", b"", b"BA"],
        [b"\x1b[", b"1 ?>", b"", b"CA"],
        [b"\x1b]2;ignored", b"\xc2", b"\x9b\x07", b"2CA"],
        [b"\x1bP1qignored", b"\xc2", b"\x9d2;new title", b"\x07A"],
        [b"\x1b[", b"\xc0", b"\x802", b"CA"],
        [b"\x1b(", b"\xed\xa0", b"\x80", b"BA"],
        [b"\x1b[", b"\xf5\x80\x80", b"\x80", b"CA"],
        [b"\x1b]", b"\xe1\x80", b"\xc2\x9b", b"2CA"],
    ];
    for &[before, prefix, continuation, suffix] in cases {
        let state = Arc::new(AtomicU64::new(RESUME));
        let (mut control, mut events) = controlled_reader(&state);
        control.requested().await;
        control.data(before);
        expect_bytes(&mut events, before).await;
        control.requested().await;
        state.store(DRAIN, Ordering::Release);
        control.data(prefix);
        if continuation.is_empty() {
            expect_bytes(&mut events, prefix).await;
        }
        control.requested().await;
        control.would_block();
        control.requested().await;
        assert_eq!(state.load(Ordering::Acquire), DRAIN, "prefix {prefix:?}");
        assert!(events.try_recv().is_err());
        if !continuation.is_empty() {
            control.data(continuation);
            expect_bytes(&mut events, &[prefix, continuation].concat()).await;
            control.requested().await;
            control.would_block();
            control.requested().await;
            assert!(events.try_recv().is_err(), "continuation {continuation:?}");
        }
        control.data(suffix);
        expect_bytes(&mut events, suffix).await;
        control.requested().await;
        control.would_block();
        assert!(matches!(
            read_event(&mut events).await,
            Some(PtyEvent::Boundary)
        ));
        state.store(CLOSE, Ordering::Release);
        control.finish();
    }
}

#[tokio::test]
async fn w1_terminal_reader_preserves_fifo_and_chunk_cap_with_three_byte_carry() {
    let state = Arc::new(AtomicU64::new(RESUME));
    let (mut control, mut events) = controlled_reader(&state);
    let mut start = vec![b'A'; OUTPUT_CHUNK_SIZE - 3];
    start.extend(b"\xf0\x90\x80");
    assert_eq!(control.requested().await, OUTPUT_CHUNK_SIZE);
    control.data(&start);
    expect_bytes(&mut events, &start[..OUTPUT_CHUNK_SIZE - 3]).await;
    assert_eq!(control.requested().await, OUTPUT_CHUNK_SIZE - 3);
    let mut end = vec![b'B'; OUTPUT_CHUNK_SIZE - 3];
    end[0] = 0x80;
    state.store(DRAIN, Ordering::Release);
    control.would_block();
    assert_eq!(control.requested().await, OUTPUT_CHUNK_SIZE - 3);
    assert!(events.try_recv().is_err());
    control.data(&end);
    let expected = [&start[OUTPUT_CHUNK_SIZE - 3..], end.as_slice()].concat();
    expect_bytes(&mut events, &expected).await;
    assert_eq!(control.requested().await, OUTPUT_CHUNK_SIZE);
    control.would_block();
    assert!(matches!(
        read_event(&mut events).await,
        Some(PtyEvent::Boundary)
    ));
    state.store(CLOSE, Ordering::Release);
    control.finish();
}

#[tokio::test]
async fn w1_terminal_reader_coalesces_framed_output_without_crossing_the_grid_marker() {
    let state = Arc::new(AtomicU64::new(DRAIN));
    let (mut control, mut events) = controlled_reader(&state);
    control.requested().await;
    control.data(b"first \xe2\x82");
    assert_eq!(control.requested().await, OUTPUT_CHUNK_SIZE - 2);
    control.data(b"\xac\x1b[");
    control.requested().await;
    control.would_block();
    control.requested().await;
    assert_eq!(state.load(Ordering::Acquire), DRAIN);
    assert_eq!(events.len(), 1);
    control.data(b"2H");
    control.requested().await;
    control.would_block();
    until(|| events.len() == 2).await;
    assert_eq!(state.load(Ordering::Acquire), PAUSE);

    state.store(RESUME, Ordering::Release);
    control.requested().await;
    control.data(b"redraw");
    control.requested().await;
    assert_eq!(events.len(), 3);
    expect_bytes(&mut events, b"first \xe2\x82\xac\x1b[2H").await;
    assert!(matches!(events.try_recv(), Ok(PtyEvent::Boundary)));
    expect_bytes(&mut events, b"redraw").await;
    state.store(CLOSE, Ordering::Release);
    control.finish();
}

#[derive(Debug, Default)]
struct ChildState {
    allow_reap: AtomicBool,
    reaped: AtomicBool,
    signals: AtomicU64,
}

struct AllowReap(Arc<ChildState>);

impl Drop for AllowReap {
    fn drop(&mut self) {
        self.0.allow_reap.store(true, Ordering::Release);
    }
}

#[derive(Debug, Clone)]
struct ControlledChild(Arc<ChildState>);

impl ChildKiller for ControlledChild {
    fn kill(&mut self) -> std::io::Result<()> {
        assert!(!self.0.reaped.load(Ordering::Acquire), "signal after reap");
        self.0.signals.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(self.clone())
    }
}

impl Child for ControlledChild {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        Ok(self.0.allow_reap.load(Ordering::Acquire).then(|| {
            self.0.reaped.store(true, Ordering::Release);
            ExitStatus::with_exit_code(0)
        }))
    }

    fn wait(&mut self) -> std::io::Result<ExitStatus> {
        panic!("cleanup must poll its owned child");
    }

    fn process_id(&self) -> Option<u32> {
        Some(42)
    }
}

async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !condition() {
            tokio::time::sleep(CLEANUP_POLL).await;
        }
    })
    .await
    .expect("attachment fixture did not reach expected state");
}

fn controlled_view(
    bridge: &DesktopBridge,
    target: &remote_sessions_host::model::TerminalTarget,
    agent: Uuid,
    revision: u64,
) -> (Arc<View>, ReadControl, Arc<ChildState>) {
    use remote_sessions_host::model::{CommittedGeometry, ResizeIntent};
    let epoch = ResizeEpoch {
        agent,
        owner: Uuid::new_v4(),
    };
    let terminal = TerminalView {
        target: target.clone(),
        view_id: Uuid::new_v4(),
        agent_epoch: epoch.agent,
    };
    let reader_state = Arc::new(AtomicU64::new(RESUME));
    let (sender, receiver) = mpsc::channel(OUTPUT_SLOTS);
    let (steps, commands) = thread_channel::channel();
    let (requests, waits) = mpsc::unbounded_channel();
    let child = Arc::new(ChildState::default());
    let exit = Arc::new(ExitState::default());
    let status = TerminalStatus {
        terminal: terminal.clone(),
        epoch: Some(epoch),
        committed: CommittedGeometry {
            size: TerminalSize {
                cols: 110,
                rows: 32,
            },
            revision,
            intent: (revision > 0).then(|| ResizeIntent {
                id: Uuid::new_v4(),
                epoch,
            }),
        },
        provider: None,
        observation: ObservationState::Unavailable,
    };
    let generation = bridge.generation.load(Ordering::SeqCst);
    let allocation = bridge.reserve(generation).unwrap();
    let view = Arc::new(View {
        terminal,
        directory: bridge.directory.clone().unwrap_or_default(),
        tmux: "/unused/tmux".into(),
        client_pid: 42,
        tty: PathBuf::new(),
        control: Arc::new(tokio::sync::Mutex::new(Control { lease: None })),
        resize: tokio::sync::Mutex::new(()),
        master: Mutex::new(None),
        output: tokio::sync::Mutex::new(Output {
            receiver,
            status,
            refresh_at: Instant::now() + Duration::from_secs(60),
            applied_revision: 0,
            pending: None,
            drain_started: None,
            writable: false,
        }),
        drain_revision: AtomicU64::new(0),
        reader_state,
        views: Arc::downgrade(&bridge.views),
        allocation: Arc::downgrade(&allocation),
        retirement: watch::channel(None).0,
        reading: AtomicBool::new(false),
        handle: tokio::runtime::Handle::current(),
        cleanup_confirmations: AtomicU64::new(0),
    });
    bridge
        .start_attachment(
            generation,
            view.clone(),
            AttachmentChild::new(Box::new(ControlledChild(child.clone())), allocation),
            Box::new(ControlledReader {
                steps: commands,
                requests,
                exit: Some(exit.clone()),
            }),
            sender,
        )
        .unwrap();
    (
        view,
        ReadControl {
            steps,
            requests: waits,
            thread: None,
            child: Some(AllowReap(child.clone())),
            exit: Some(AllowExit(exit)),
        },
        child,
    )
}

#[tokio::test]
async fn w1_terminal_expired_incomplete_drain_isolates_the_view_and_drops_its_tail() {
    use remote_sessions_host::model::TerminalTarget;
    let target = TerminalTarget {
        runtime_id: Uuid::new_v4(),
        generation: Uuid::new_v4(),
        session: "$0".into(),
        window: "@0".into(),
        pane: "%0".into(),
        pid: 123,
        process_start: 456,
    };
    let bridge = DesktopBridge::new(None, "/unused/tmux".into());
    let agent = Uuid::new_v4();
    let (stalled, mut writer, stalled_kills) = controlled_view(&bridge, &target, agent, 1);
    let (peer, mut peer_writer, peer_kills) = controlled_view(&bridge, &target, agent, 0);
    let view_id = stalled.terminal.view_id.to_string();
    let peer_id = peer.terminal.view_id.to_string();
    let start = Instant::now();
    writer.requested().await;
    writer.data(b"before\xe1\x80");
    let chunk = bridge.read_with_clock(&view_id, || start).await.unwrap();
    assert_eq!(chunk.data, b"before");
    assert!(chunk.geometry.is_none());
    assert_eq!(writer.requested().await, OUTPUT_CHUNK_SIZE - 2);
    for _ in 0..2 {
        writer.would_block();
        writer.requested().await;
        assert_eq!(stalled.reader_state.load(Ordering::Acquire), DRAIN);
    }
    peer_writer.requested().await;
    peer_writer.data(b"peer first");
    assert_eq!(bridge.read(&peer_id).await.unwrap().data, b"peer first");
    peer_writer.requested().await;
    let chunk = bridge
        .read_with_clock(&view_id, || {
            start + DRAIN_TIMEOUT - Duration::from_millis(1)
        })
        .await
        .unwrap();
    assert!(chunk.data.is_empty() && chunk.geometry.is_none());
    assert_eq!(stalled.output.lock().await.drain_started, Some(start));
    let failure = bridge
        .read_with_clock(&view_id, || start + DRAIN_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(failure.closure, Some(TerminalClosure::GeometryTimeout));
    assert!(stalled.closed());
    until(|| stalled_kills.signals.load(Ordering::Relaxed) == 1).await;
    assert!(stalled.output.lock().await.pending.is_none());

    let (fresh, mut fresh_writer, _) = controlled_view(&bridge, &target, agent, 0);
    let fresh_id = fresh.terminal.view_id.to_string();
    assert_ne!(fresh_id, view_id);
    assert_eq!(fresh.terminal.target, stalled.terminal.target);
    writer.data(b"\x80old tail");
    writer.finish();
    assert!(
        read_event(&mut stalled.output.lock().await.receiver)
            .await
            .is_none()
    );
    assert!(bridge.read(&view_id).await.is_err());
    fresh_writer.requested().await;
    fresh_writer.data(b"fresh redraw");
    assert_eq!(bridge.read(&fresh_id).await.unwrap().data, b"fresh redraw");
    peer_writer.data(b"peer next");
    assert_eq!(bridge.read(&peer_id).await.unwrap().data, b"peer next");
    assert!(!peer.closed() && !fresh.closed());
    assert_eq!(peer_kills.signals.load(Ordering::Relaxed), 0);
    fresh_writer.finish();
    peer_writer.finish();
}

#[tokio::test]
async fn w1_terminal_watchdog_expires_only_its_generation_without_the_output_lock() {
    let target = remote_sessions_host::model::TerminalTarget {
        runtime_id: Uuid::new_v4(),
        generation: Uuid::new_v4(),
        session: "$0".into(),
        window: "@0".into(),
        pane: "%0".into(),
        pid: 123,
        process_start: 456,
    };
    let bridge = DesktopBridge::new(None, "/unused/tmux".into());
    let (view, mut writer, _) = controlled_view(&bridge, &target, Uuid::new_v4(), 2);
    let id = view.terminal.view_id;
    writer.requested().await;
    writer.data(b"before boundary");
    let chunk = bridge.read(&id.to_string()).await.unwrap();
    assert_eq!(chunk.data, b"before boundary");
    writer.requested().await;

    let key = id.to_string();
    let mut reading = std::pin::pin!(bridge.read(&key));
    std::future::poll_fn(|cx| {
        use std::future::Future;
        assert!(reading.as_mut().poll(cx).is_pending());
        assert!(view.output.try_lock().is_err());
        expire_drain(&bridge.views, id, 1);
        assert!(!view.closed());
        std::task::Poll::Ready(())
    })
    .await;
    writer.data(b"current drain");
    assert_eq!(reading.await.unwrap().data, b"current drain");
    writer.requested().await;

    let output = view.output.lock().await;
    expire_drain(&bridge.views, id, 2);
    assert!(view.closed());
    assert_eq!(
        bridge.read(&key).await.unwrap().closure,
        Some(TerminalClosure::GeometryTimeout)
    );
    drop(output);
    writer.finish();
}

#[tokio::test]
async fn w1_terminal_boundary_received_at_deadline_cannot_reach_geometry() {
    use remote_sessions_host::model::TerminalTarget;
    let target = TerminalTarget {
        runtime_id: Uuid::new_v4(),
        generation: Uuid::new_v4(),
        session: "$0".into(),
        window: "@0".into(),
        pane: "%0".into(),
        pid: 123,
        process_start: 456,
    };
    let bridge = DesktopBridge::new(None, "/unused/tmux".into());
    let (view, mut writer, kills) = controlled_view(&bridge, &target, Uuid::new_v4(), 1);
    let view_id = view.terminal.view_id.to_string();
    let start = Instant::now();
    writer.requested().await;
    writer.data(b"\x1b[");
    assert_eq!(
        bridge
            .read_with_clock(&view_id, || start)
            .await
            .unwrap()
            .data,
        b"\x1b["
    );
    writer.requested().await;
    writer.data(b"H");
    let before_deadline = start + DRAIN_TIMEOUT - Duration::from_millis(100);
    assert_eq!(
        bridge
            .read_with_clock(&view_id, || before_deadline)
            .await
            .unwrap()
            .data,
        b"H"
    );
    writer.requested().await;
    writer.would_block();
    tokio::time::timeout(Duration::from_secs(2), async {
        while view.output.lock().await.receiver.is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(view.output.lock().await.receiver.len(), 1);
    assert_eq!(view.reader_state.load(Ordering::Acquire), PAUSE);

    let clock_reads = Cell::new(0);
    let failure = bridge
        .read_with_clock(&view_id, || {
            let read = clock_reads.get() + 1;
            clock_reads.set(read);
            if read < 3 {
                before_deadline
            } else {
                start + DRAIN_TIMEOUT
            }
        })
        .await
        .unwrap();
    assert_eq!(failure.closure, Some(TerminalClosure::GeometryTimeout));
    assert_eq!(clock_reads.get(), 3);
    assert!(view.closed());
    until(|| kills.signals.load(Ordering::Relaxed) == 1).await;
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if view.output.lock().await.receiver.is_closed() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut output = view.output.lock().await;
    assert!(output.pending.is_none());
    assert_eq!(output.applied_revision, 0);
    assert!(output.receiver.try_recv().is_err());
    writer.finish();
}

use remote_sessions_host::{
    decode_local_fixture, encode_local_observation,
    model::{CommittedGeometry, ResizeIntent, TerminalTarget},
};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

const FULL: TerminalClosure = TerminalClosure::Overflow {
    high_water: OUTPUT_SLOTS,
};

fn target() -> TerminalTarget {
    TerminalTarget {
        runtime_id: Uuid::new_v4(),
        generation: Uuid::new_v4(),
        session: "$0".into(),
        window: "@0".into(),
        pane: "%0".into(),
        pid: 123,
        process_start: 456,
    }
}

async fn saturate(view: &View, reader: &mut ReadControl, marker: bool) {
    for byte in 0..OUTPUT_SLOTS {
        reader.requested().await;
        reader.data(&[byte as u8 + b'A'; OUTPUT_CHUNK_SIZE]);
    }
    reader.requested().await;
    assert_eq!(view.output.lock().await.receiver.len(), OUTPUT_SLOTS);
    if marker {
        assert_eq!(view.reader_state.swap(DRAIN, Ordering::AcqRel), RESUME);
        reader.would_block();
    } else {
        reader.data(b"full");
    }
    view.retired().await;
}

#[tokio::test]
async fn w1_terminal_delayed_reader_keeps_up_while_paused_peer_overflows() {
    const BURST_BYTES: usize = 1745;
    const BURSTS_PER_PULL: usize = 4;
    let bridge = DesktopBridge::new(None, "/unused/tmux".into());
    let target = target();
    let agent = Uuid::new_v4();
    let (healthy, mut owner, _) = controlled_view(&bridge, &target, agent, 0);
    let (paused, mut victim, _) = controlled_view(&bridge, &target, agent, 0);
    let healthy_id = healthy.terminal.view_id.to_string();
    let victim_id = paused.terminal.view_id.to_string();
    owner.requested().await;
    victim.requested().await;
    for pull in 0..=OUTPUT_SLOTS {
        let mut expected = Vec::new();
        for index in 0..BURSTS_PER_PULL {
            let burst = [b'A' + ((pull * BURSTS_PER_PULL + index) % 26) as u8; BURST_BYTES];
            expected.extend_from_slice(&burst);
            owner.data(&burst);
            owner.requested().await;
            owner.would_block();
            owner.requested().await;
            if pull < OUTPUT_SLOTS {
                victim.data(&burst);
                victim.requested().await;
            }
        }
        if pull == OUTPUT_SLOTS {
            assert_eq!(paused.output.lock().await.receiver.len(), OUTPUT_SLOTS);
            victim.data(&[b'Z'; BURST_BYTES]);
            until(|| paused.closed()).await;
            assert_eq!(bridge.read(&victim_id).await.unwrap().closure, Some(FULL));
            assert!(bridge.read(&victim_id).await.is_err());
        }
        assert_eq!(healthy.output.lock().await.receiver.len(), 1);
        let chunk = bridge.read(&healthy_id).await.unwrap();
        assert!(!chunk.closed && chunk.geometry.is_none());
        assert_eq!(chunk.data, expected);
    }
    assert!(!healthy.closed());
    bridge.detach(&healthy_id);
    owner.finish();
    victim.finish();
}

#[tokio::test]
async fn w1_terminal_byte_and_marker_full_evict_before_a_first_pull() {
    let bridge = DesktopBridge::new(None, "/unused/tmux".into());
    for marker in [false, true] {
        let (view, mut reader, child) = controlled_view(&bridge, &target(), Uuid::new_v4(), 0);
        let id = view.terminal.view_id.to_string();
        saturate(&view, &mut reader, marker).await;
        assert!(bridge.view(&id).is_err());
        assert_eq!(bridge.views.lock().unwrap().notices.len(), 1);
        bridge.detach(&id);
        view.retire(Retirement::Eof);
        let closed = bridge.read(&id).await.unwrap();
        assert_eq!(closed.closure, Some(FULL));
        assert!(closed.closed && !closed.writable && closed.data.is_empty());
        assert!(closed.status.is_none() && closed.geometry.is_none());
        assert!(bridge.read(&id).await.is_err());
        until(|| child.signals.load(Ordering::Relaxed) == 1).await;
        reader.finish();
        until(|| allocation_counts(&bridge) == (0, 0)).await;
        assert!(child.reaped.load(Ordering::Acquire));
        assert_eq!(child.signals.load(Ordering::Relaxed), 1);
    }
}

#[tokio::test]
async fn w1_terminal_eof_detach_and_dropped_receiver_are_distinct() {
    let (tx, rx) = mpsc::channel(OUTPUT_SLOTS);
    drop(rx);
    assert!(matches!(
        PtyPublisher::new(tx).publish(PtyRead::Boundary),
        Some(Retirement::ReceiverClosed)
    ));
    let bridge = Arc::new(DesktopBridge::new(None, "/unused/tmux".into()));
    for closure in [TerminalClosure::Detached, TerminalClosure::Eof] {
        let (view, mut reader, _) = controlled_view(&bridge, &target(), Uuid::new_v4(), 0);
        let id = view.terminal.view_id.to_string();
        reader.requested().await;
        let pending = {
            let bridge = bridge.clone();
            let id = id.clone();
            tokio::spawn(async move { bridge.read(&id).await })
        };
        until(|| view.reading.load(Ordering::Acquire)).await;
        if closure == TerminalClosure::Eof {
            reader.data(b"");
        } else {
            bridge.detach(&id);
        }
        let chunk = tokio::time::timeout(Duration::from_millis(100), pending)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(chunk.closure, Some(closure));
        assert!(bridge.read(&id).await.is_err());
        reader.finish();
    }
}

#[tokio::test]
async fn w1_terminal_detach_supersedes_eof_with_a_read_in_flight() {
    let bridge = DesktopBridge::new(None, "/unused/tmux".into());
    let (view, mut reader, _) = controlled_view(&bridge, &target(), Uuid::new_v4(), 0);
    let id = view.terminal.view_id.to_string();
    let mut pending = std::pin::pin!(bridge.read(&id));
    std::future::poll_fn(|cx| {
        use std::future::Future;
        assert!(pending.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    reader.requested().await;
    reader.data(b"discard on detach");
    reader.requested().await;
    reader.data(b"");
    view.retired().await;
    assert!(view.eof_draining());
    bridge.detach(&id);
    let closed = pending.as_mut().await.unwrap();
    assert_eq!(closed.closure, Some(TerminalClosure::Detached));
    assert!(closed.closed && closed.data.is_empty());
    assert_read_only_drain(&closed);
    assert!(bridge.read(&id).await.is_err());
    reader.finish();
    until(|| allocation_counts(&bridge) == (0, 0)).await;
    assert!(view.output.lock().await.receiver.is_empty());
}

#[tokio::test]
async fn w1_terminal_eof_fifo() {
    for parked in [false, true] {
        let fixture = AgentFixture::new();
        let listener = UnixListener::bind(fixture.0.join("agent.sock")).unwrap();
        let bridge = DesktopBridge::new(Some(fixture.0.clone()), "/unused/tmux".into());
        let (view, mut reader, child) = controlled_view(&bridge, &target(), Uuid::new_v4(), 0);
        let id = view.terminal.view_id.to_string();
        let epoch = view.output.lock().await.status.epoch.unwrap();
        let token = Uuid::new_v4();
        let acquire = {
            let view = view.clone();
            tokio::spawn(async move { refresh_control(&view).await })
        };
        let (mut peer, Request::AcquireTerminal { terminal }) = receive(&listener).await else {
            panic!("expected scoped acquisition");
        };
        assert_eq!(terminal, view.terminal);
        respond(&mut peer, Response::TerminalControl { token, epoch }).await;
        acquire.await.unwrap();
        assert!(bridge.read(&id).await.unwrap().writable);

        let mut pending = std::pin::pin!(bridge.read(&id));
        if parked {
            std::future::poll_fn(|cx| {
                use std::future::Future;
                assert!(pending.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
        }
        reader.requested().await;
        for frame in 0..OUTPUT_SLOTS {
            reader.data(&[b'A' + frame as u8; OUTPUT_CHUNK_SIZE]);
            reader.requested().await;
        }
        reader.data(b"\xe1\x80");
        assert_eq!(reader.requested().await, OUTPUT_CHUNK_SIZE - 2);
        child.allow_reap.store(true, Ordering::Release);
        reader.data(b"");
        view.retired().await;
        assert!(view.eof_draining());
        assert_eq!(allocation_counts(&bridge), (0, 1));
        assert!(bridge.status(&id).await.is_err());
        assert!(
            bridge
                .input(&id, b"after EOF".to_vec(), false)
                .await
                .is_err()
        );
        let (
            mut peer,
            Request::Release {
                runtime_id,
                token: released,
            },
        ) = receive(&listener).await
        else {
            panic!("expected exact EOF lease release");
        };
        assert_eq!(
            (runtime_id, released),
            (view.terminal.target.runtime_id, token)
        );
        respond(&mut peer, Response::Accepted {}).await;
        until(|| view.allocation.strong_count() == 1).await;
        assert!(child.reaped.load(Ordering::Acquire));
        assert_eq!(view.cleanup_confirmations.load(Ordering::Acquire), 3);
        assert!(view.master.lock().unwrap().is_none());

        assert_eof_frame(pending.as_mut().await.unwrap(), 0);
        for index in 1..OUTPUT_SLOTS {
            assert_eof_frame(bridge.read(&id).await.unwrap(), index);
        }
        let eof = bridge.read(&id).await.unwrap();
        assert_eq!(eof.closure, Some(TerminalClosure::Eof));
        assert!(eof.closed && eof.data.is_empty());
        assert_read_only_drain(&eof);
        assert!(bridge.read(&id).await.is_err());
        assert_eq!(allocation_counts(&bridge), (0, 0));
        assert!(
            tokio::time::timeout(Duration::from_millis(25), listener.accept())
                .await
                .is_err()
        );
        reader.finish();
    }
}

fn assert_eof_frame(chunk: Chunk, index: usize) {
    assert_eq!(chunk.data, [b'A' + index as u8; OUTPUT_CHUNK_SIZE]);
    assert!(!chunk.closed && chunk.closure.is_none());
    assert_read_only_drain(&chunk);
}

fn assert_read_only_drain(chunk: &Chunk) {
    assert!(!chunk.writable && chunk.status.is_none() && chunk.geometry.is_none());
    assert!(chunk.paste_incarnation.is_none() && chunk.paste_completion.is_none());
    assert!(chunk.paste_admission.is_none());
}

#[tokio::test]
async fn w1_terminal_abandoned_eof_capacity() {
    let bridge = DesktopBridge::new(None, "/unused/tmux".into());
    let target = target();
    let agent = Uuid::new_v4();
    let mut abandoned = VecDeque::new();
    for cycle in 0..(VIEW_LIMIT * 3) {
        let (view, mut reader, child) = controlled_view(&bridge, &target, agent, 0);
        let id = view.terminal.view_id;
        if abandoned.len() == VIEW_LIMIT {
            let evicted: Arc<View> = abandoned.pop_front().unwrap();
            assert!(
                bridge
                    .read(&evicted.terminal.view_id.to_string())
                    .await
                    .is_err()
            );
            assert!(evicted.output.lock().await.receiver.is_empty());
        }
        reader.requested().await;
        reader.data(&[b'Z'; OUTPUT_CHUNK_SIZE]);
        reader.requested().await;
        child.allow_reap.store(true, Ordering::Release);
        reader.data(b"");
        view.retired().await;
        until(|| view.allocation.strong_count() == 1).await;
        assert_eq!(view.cleanup_confirmations.load(Ordering::Acquire), 3);
        assert!(child.reaped.load(Ordering::Acquire));
        assert_eq!(view.output.lock().await.receiver.len(), 1);
        assert_eq!(allocation_counts(&bridge), (0, (cycle + 1).min(VIEW_LIMIT)));
        assert_eq!(
            bridge.views.lock().unwrap().drains.len(),
            (cycle + 1).min(VIEW_LIMIT)
        );
        assert!(bridge.view(&id.to_string()).is_err());
        reader.finish();
        abandoned.push_back(view);
    }

    let detached = abandoned.pop_front().unwrap();
    let id = detached.terminal.view_id.to_string();
    bridge.detach(&id);
    assert!(bridge.read(&id).await.is_err());
    assert!(detached.output.lock().await.receiver.is_empty());

    for view in abandoned {
        let id = view.terminal.view_id;
        {
            let mut views = bridge.views.lock().unwrap();
            let expires = views.drains[&id].expires;
            views.expire_eof(id, expires - Duration::from_nanos(1));
            assert!(views.drains.contains_key(&id));
            views.expire_eof(id, expires);
            assert!(!views.drains.contains_key(&id));
        }
        assert!(bridge.read(&id.to_string()).await.is_err());
        assert!(view.output.lock().await.receiver.is_empty());
    }
    assert_eq!(allocation_counts(&bridge), (0, 0));
}

#[test]
fn w1_terminal_notices_are_capped_pruned_and_scalar() {
    let mut views = Views::default();
    let first = Uuid::new_v4();
    views.remember(first, FULL);
    for _ in 0..VIEW_LIMIT {
        views.remember(Uuid::new_v4(), FULL);
    }
    assert_eq!(views.notices.len(), VIEW_LIMIT);
    assert!(views.notices.iter().all(|notice| notice.view != first));
    assert!(!std::mem::needs_drop::<ClosureNotice>());
    views.prune(Instant::now() + NOTICE_TTL);
    assert!(views.notices.is_empty());
}

struct AgentFixture(PathBuf);

impl AgentFixture {
    fn new() -> Self {
        let root = Path::new("/tmp")
            .canonicalize()
            .unwrap()
            .join(format!("rs-{}", Uuid::new_v4().simple()));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        Self(root)
    }
}

impl Drop for AgentFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn receive(listener: &UnixListener) -> (UnixStream, Request) {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (mut peer, _) = listener.accept().await.unwrap();
        let mut line = String::new();
        BufReader::new(&mut peer)
            .read_line(&mut line)
            .await
            .unwrap();
        (peer, decode_local_fixture(line.as_bytes()).unwrap())
    })
    .await
    .expect("expected a scoped fixture request")
}

async fn respond(peer: &mut UnixStream, reply: Response) {
    let line = encode_local_observation(&reply).unwrap() + "\n";
    peer.write_all(line.as_bytes()).await.unwrap();
}

fn attachment_tmux(fixture: &AgentFixture, generation: Uuid, flags: &str) -> PathBuf {
    let tmux = fixture.0.join("tmux");
    fs::write(
        &tmux,
        format!(
            r#"#!/bin/sh
set -eu
root=$(dirname "$0")
case "$4" in
  attach-session)
    test "$5" = -f && test "$6" = ignore-size && test "$7" = -E
    test "$8" = -t && test "$9" = '$0'
    printf '%s\n%s\n' "$$" "$(tty)" > "$root/client-next"
    mv "$root/client-next" "$root/client"
    if [ -f "$root/flood" ]; then
      while [ ! -f "$root/continue" ]; do /bin/sleep 0.01; done
      exec /bin/dd if=/dev/zero bs=8192 count=32 2>/dev/null
    fi
    printf 'buffered attachment output'
    if [ -f "$root/hold" ]; then
      while [ ! -f "$root/continue" ]; do /bin/sleep 0.01; done
      printf 'output before failed refresh'
      while [ ! -f "$root/finish" ]; do /bin/sleep 0.01; done
      exit 0
    fi
    exec /bin/sleep 30
    ;;
  list-clients)
    test ! -f "$root/client-changed" || exit 1
    {{ read -r pid; read -r tty; }} < "$root/client"
    set -- $(stty size < "$tty")
    printf '%s|%s|$0|@0|%%0|{generation}|{flags}|%s|%s\n' "$pid" "$tty" "$2" "$1"
    ;;
  *) exit 1 ;;
esac
"#,
        ),
    )
    .unwrap();
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    tmux
}

async fn open_fixture(listener: &UnixListener, target: &TerminalTarget) -> TerminalStatus {
    let (
        mut peer,
        Request::OpenTerminal {
            runtime_id,
            view_id,
        },
    ) = receive(listener).await
    else {
        panic!("expected an opening request");
    };
    assert_eq!(runtime_id, target.runtime_id);
    let status = TerminalStatus {
        terminal: TerminalView {
            target: target.clone(),
            view_id,
            agent_epoch: Uuid::new_v4(),
        },
        committed: CommittedGeometry {
            size: TerminalSize {
                cols: 100,
                rows: 30,
            },
            revision: 0,
            intent: None,
        },
        epoch: None,
        provider: None,
        observation: ObservationState::Unavailable,
    };
    respond(
        &mut peer,
        Response::TerminalOpened {
            status: status.clone(),
        },
    )
    .await;
    status
}

async fn attachment_status_peer(listener: &UnixListener, status: &TerminalStatus) -> UnixStream {
    let (peer, Request::TerminalStatus { terminal }) = receive(listener).await else {
        panic!("expected a full terminal scope check");
    };
    assert_eq!(terminal, status.terminal);
    peer
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttachmentProof {
    Verified,
    ReplacementServer,
    WrongClientRole,
    RefreshUnavailable,
    ClientChanged,
    ProcessReused,
    Timeout,
    Cancelled,
    Detached,
}

#[tokio::test]
async fn w1_terminal_exact_attachment() {
    use AttachmentProof::*;
    for proof in [
        Verified,
        ReplacementServer,
        WrongClientRole,
        RefreshUnavailable,
        ClientChanged,
        ProcessReused,
        Timeout,
        Cancelled,
        Detached,
    ] {
        let fixture = AgentFixture::new();
        let target = target();
        let generation = if proof == ReplacementServer {
            Uuid::new_v4()
        } else {
            target.generation
        };
        let flags = if proof == WrongClientRole {
            "read-only"
        } else {
            "ignore-size"
        };
        let tmux = attachment_tmux(&fixture, generation, flags);
        if proof == Verified {
            fs::write(fixture.0.join("hold"), "").unwrap();
        }
        let listener = UnixListener::bind(fixture.0.join("agent.sock")).unwrap();
        let bridge = Arc::new(DesktopBridge::new(Some(fixture.0.clone()), tmux));
        let runtime = target.runtime_id.to_string();
        let attaching = {
            let bridge = bridge.clone();
            tokio::spawn(async move {
                if proof == Verified {
                    bridge.attach_observer(&runtime).await
                } else {
                    bridge.attach(&runtime).await
                }
            })
        };
        let opened = open_fixture(&listener, &target).await;
        let id = opened.terminal.view_id.to_string();
        let client = fixture.0.join("client");
        until(|| client.exists()).await;
        let identity = fs::read_to_string(client).unwrap();
        let pid = identity.lines().next().unwrap();
        assert!(pid.parse::<u32>().unwrap() > 0);

        let mut refreshed = opened.clone();
        let mut proof_peer = match proof {
            ReplacementServer | WrongClientRole => None,
            _ => Some(attachment_status_peer(&listener, &opened).await),
        };
        assert!(!attaching.is_finished(), "{proof:?}");
        assert_eq!(allocation_counts(&bridge), (0, 1));
        assert!(bridge.read(&id).await.is_err());
        assert!(bridge.status(&id).await.is_err());
        assert!(
            bridge
                .input(&id, b"pre-proof".to_vec(), false)
                .await
                .is_err()
        );
        match proof {
            ReplacementServer | WrongClientRole | Timeout => {}
            RefreshUnavailable => {
                respond(
                    proof_peer.as_mut().unwrap(),
                    Response::Error {
                        message: "fixture unavailable".into(),
                    },
                )
                .await;
            }
            Cancelled => attaching.abort(),
            Verified | ClientChanged | ProcessReused | Detached => {
                if proof == Detached {
                    bridge.detach_all();
                }
                if proof == ClientChanged {
                    fs::write(fixture.0.join("client-changed"), "").unwrap();
                }
                if proof == ProcessReused {
                    refreshed.terminal.target.process_start += 1;
                } else {
                    refreshed.epoch = Some(ResizeEpoch {
                        agent: opened.terminal.agent_epoch,
                        owner: Uuid::new_v4(),
                    });
                }
                respond(
                    proof_peer.as_mut().unwrap(),
                    Response::TerminalStatus {
                        status: refreshed.clone(),
                    },
                )
                .await;
            }
        }

        let result = tokio::time::timeout(Duration::from_secs(4), attaching)
            .await
            .unwrap();
        if proof == Verified {
            let attachment = result.unwrap().unwrap();
            assert_eq!(attachment.view_id, id);
            assert_eq!(attachment.status, refreshed);
            assert_eq!(bridge.attachment_pid(&id).unwrap().to_string(), pid);
            let view = bridge.view(&id).unwrap();
            view.output.lock().await.refresh_at = Instant::now() + Duration::from_secs(60);
            assert_eq!(
                read_until_bytes(&bridge, &id, b"buffered attachment output").await,
                b"buffered attachment output"
            );
            fs::write(fixture.0.join("continue"), "").unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                while view.output.lock().await.receiver.is_empty() {
                    tokio::time::sleep(CLEANUP_POLL).await;
                }
            })
            .await
            .unwrap();
            view.output.lock().await.refresh_at = Instant::now();
            let reading = {
                let bridge = bridge.clone();
                let id = id.clone();
                tokio::spawn(async move { bridge.read(&id).await })
            };
            let mut refresh = attachment_status_peer(&listener, &opened).await;
            fs::write(fixture.0.join("finish"), "").unwrap();
            tokio::time::timeout(Duration::from_secs(2), view.retired())
                .await
                .unwrap();
            assert!(view.eof_draining());
            respond(
                &mut refresh,
                Response::Error {
                    message: "later observation failed".into(),
                },
            )
            .await;
            assert!(reading.await.unwrap().is_err());
        } else if proof == Cancelled {
            assert!(result.err().unwrap().is_cancelled());
        } else {
            let error = result.unwrap().err().unwrap();
            let expected = match proof {
                ReplacementServer | WrongClientRole | Timeout => {
                    "Terminal attachment proof timed out"
                }
                RefreshUnavailable => "Розмір термінала недоступний",
                ClientChanged => "Attachment client unavailable",
                ProcessReused => "Terminal attachment identity changed",
                Detached => "View context changed",
                _ => unreachable!(),
            };
            assert_eq!(error.to_string(), expected, "{proof:?}");
        }
        assert!(bridge.read(&id).await.is_err());
        assert!(bridge.status(&id).await.is_err());
        assert!(
            bridge
                .input(&id, b"post-proof".to_vec(), false)
                .await
                .is_err()
        );
        until(|| allocation_counts(&bridge) == (0, 0)).await;
        let observed = std::process::Command::new("/bin/ps")
            .args(["-p", pid, "-o", "pid="])
            .output()
            .unwrap();
        assert!(
            !observed.status.success() && observed.stdout.is_empty(),
            "child {pid} was not reaped for {proof:?}"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(25), listener.accept())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn w1_terminal_parked_read_gets_producer_full_and_late_lease_is_released_once() {
    let fixture = AgentFixture::new();
    let listener = UnixListener::bind(fixture.0.join("agent.sock")).unwrap();
    let bridge = Arc::new(DesktopBridge::new(
        Some(fixture.0.clone()),
        "/unused/tmux".into(),
    ));
    let target = target();
    let agent = Uuid::new_v4();
    let (old, mut reader, _) = controlled_view(&bridge, &target, agent, 0);
    let (fresh, fresh_reader, _) = controlled_view(&bridge, &target, agent, 0);
    let old_id = old.terminal.view_id.to_string();
    old.output.lock().await.refresh_at = Instant::now();
    let acquire_old = {
        let old = old.clone();
        tokio::spawn(async move { refresh_control(&old).await })
    };
    let (mut stale_acquire, Request::AcquireTerminal { terminal }) = receive(&listener).await
    else {
        panic!("expected scoped acquisition");
    };
    assert_eq!(terminal, old.terminal);
    let pending = {
        let bridge = bridge.clone();
        let id = old_id.clone();
        tokio::spawn(async move { bridge.read(&id).await })
    };
    let (_parked_status, Request::TerminalStatus { terminal }) = receive(&listener).await else {
        panic!("expected parked status read");
    };
    assert_eq!(terminal, old.terminal);
    saturate_parked(&old, &mut reader).await;
    assert_eq!(pending.await.unwrap().unwrap().closure, Some(FULL));
    assert!(bridge.read(&old_id).await.is_err());

    let old_token = Uuid::new_v4();
    let successor_token = Uuid::new_v4();
    let owner_epoch = ResizeEpoch {
        agent,
        owner: Uuid::new_v4(),
    };
    let successor_epoch = ResizeEpoch {
        agent,
        owner: Uuid::new_v4(),
    };
    let acquire_fresh = {
        let fresh = fresh.clone();
        tokio::spawn(async move { refresh_control(&fresh).await })
    };
    let (mut successor, Request::AcquireTerminal { terminal }) = receive(&listener).await else {
        panic!("expected successor acquisition");
    };
    assert_eq!(terminal, fresh.terminal);
    respond(
        &mut successor,
        Response::TerminalControl {
            token: successor_token,
            epoch: successor_epoch,
        },
    )
    .await;
    acquire_fresh.await.unwrap();
    respond(
        &mut stale_acquire,
        Response::TerminalControl {
            token: old_token,
            epoch: owner_epoch,
        },
    )
    .await;
    let (mut release, Request::Release { runtime_id, token }) =
        tokio::time::timeout(Duration::from_secs(1), receive(&listener))
            .await
            .unwrap()
    else {
        panic!("expected exact stale release");
    };
    assert_eq!((runtime_id, token), (target.runtime_id, old_token));
    respond(&mut release, Response::Accepted {}).await;
    acquire_old.await.unwrap();
    assert!(old.control.lock().await.lease.is_none());
    assert_eq!(
        fresh.control.lock().await.lease.as_ref().unwrap().token,
        successor_token
    );
    let fresh_input = {
        let bridge = bridge.clone();
        let view = fresh.terminal.view_id.to_string();
        tokio::spawn(async move { bridge.input(&view, vec![b'F'], false).await })
    };
    let (
        mut peer,
        Request::Input {
            runtime_id,
            token,
            sequence,
            data,
            paste,
        },
    ) = receive(&listener).await
    else {
        panic!("expected fresh successor input");
    };
    assert_eq!(
        (runtime_id, token, sequence, data, paste),
        (target.runtime_id, successor_token, 0, vec![b'F'], false)
    );
    respond(&mut peer, Response::Accepted {}).await;
    fresh_input.await.unwrap().unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
    reader.finish();
    fresh_reader.finish();
}

async fn saturate_parked(view: &View, reader: &mut ReadControl) {
    for _ in 0..=OUTPUT_SLOTS {
        reader.requested().await;
        reader.data(&[b'P'; OUTPUT_CHUNK_SIZE]);
    }
    tokio::time::timeout(Duration::from_secs(1), view.retired())
        .await
        .unwrap();
}

#[tokio::test]
async fn w1_terminal_capacity_waits_for_reader_and_child_independently() {
    for reap_first in [false, true] {
        capacity_case(reap_first).await;
    }
}

async fn capacity_case(reap_first: bool) {
    let fixture = AgentFixture::new();
    let tmux = fixture.0.join("tmux");
    let target = target();
    let epoch = ResizeEpoch {
        agent: Uuid::new_v4(),
        owner: Uuid::new_v4(),
    };
    let revision = Arc::new(AtomicU64::new(0));
    let opens = Arc::new(AtomicU64::new(0));
    let (updates, open_count, running) = (revision.clone(), opens.clone(), target.clone());
    let intent = ResizeIntent {
        id: Uuid::new_v4(),
        epoch,
    };
    let listener = UnixListener::bind(fixture.0.join("agent.sock")).unwrap();
    fs::write(
        &tmux,
        format!(
            r#"#!/bin/sh
set -eu
root=$(dirname "$0")
case "$4" in
  attach-session)
    printf '%s\n%s\n' "$$" "$(tty)" > "$root/client-$$"
    printf peer-start
    while [ ! -f "$root/peer-next" ]; do /bin/sleep 0.01; done
    printf peer-next
    while [ ! -f "$root/peer-redraw-$$" ]; do /bin/sleep 0.01; done
    printf peer-redraw
    exec /bin/sleep 30
    ;;
  list-clients)
    for entry in "$root"/client-*; do
      {{ read -r pid; read -r tty; }} < "$entry"
      set -- $(stty size < "$tty")
      printf '%s|%s|$0|@0|%%0|{}|ignore-size|%s|%s\n' "$pid" "$tty" "$2" "$1"
    done
    ;;
  refresh-client)
    for entry in "$root"/client-*; do
      {{ read -r pid; read -r tty; }} < "$entry"
      if [ "$6" = "$tty" ]; then : > "$root/peer-redraw-$pid"; exit 0; fi
    done
    exit 1
    ;;
  *) exit 1 ;;
esac
"#,
            target.generation
        ),
    )
    .unwrap();
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let server = tokio::spawn(async move {
        loop {
            let (mut peer, request) = receive(&listener).await;
            let (opening, terminal) = match request {
                Request::OpenTerminal {
                    runtime_id,
                    view_id,
                } => {
                    assert_eq!(runtime_id, running.runtime_id);
                    open_count.fetch_add(1, Ordering::Relaxed);
                    (
                        true,
                        TerminalView {
                            target: running.clone(),
                            view_id,
                            agent_epoch: epoch.agent,
                        },
                    )
                }
                Request::TerminalStatus { terminal } => (false, terminal),
                _ => panic!("unexpected IPC route"),
            };
            let revision = updates.load(Ordering::Acquire);
            let status = TerminalStatus {
                terminal,
                epoch: Some(epoch),
                committed: CommittedGeometry {
                    size: TerminalSize {
                        cols: if revision == 0 { 100 } else { 110 },
                        rows: 32,
                    },
                    revision,
                    intent: (revision > 0).then_some(intent),
                },
                provider: None,
                observation: ObservationState::Unavailable,
            };
            let reply = if opening {
                Response::TerminalOpened { status }
            } else {
                Response::TerminalStatus { status }
            };
            respond(&mut peer, reply).await;
        }
    });
    let bridge = DesktopBridge::new(Some(fixture.0.clone()), tmux);
    let runtime = target.runtime_id.to_string();
    let peer = bridge.attach_observer(&runtime).await.unwrap();
    assert_eq!(
        read_until_bytes(&bridge, &peer.view_id, b"peer-start").await,
        b"peer-start"
    );
    let (victim, mut writer, child) = controlled_view(&bridge, &target, epoch.agent, 0);
    let victim_id = victim.terminal.view_id.to_string();
    let exit = writer.exit.as_ref().unwrap().0.clone();
    exit.held.store(true, Ordering::Release);
    let fillers: Vec<_> = (0..VIEW_LIMIT - 2)
        .map(|_| controlled_view(&bridge, &target, epoch.agent, 0))
        .collect();
    assert_eq!(allocation_counts(&bridge), (VIEW_LIMIT, VIEW_LIMIT));
    saturate(&victim, &mut writer, reap_first).await;
    until(|| child.signals.load(Ordering::Relaxed) == 1).await;
    assert_eq!(allocation_counts(&bridge), (VIEW_LIMIT - 1, VIEW_LIMIT));
    reject_ninth(&bridge, &runtime).await;
    assert_eq!(opens.load(Ordering::Relaxed), 1);

    if reap_first {
        child.allow_reap.store(true, Ordering::Release);
        until(|| victim.cleanup_confirmations.load(Ordering::Acquire) == 2).await;
        assert!(child.reaped.load(Ordering::Acquire));
        assert!(!exit.dropped.load(Ordering::Acquire));
    } else {
        exit.held.store(false, Ordering::Release);
        until(|| victim.cleanup_confirmations.load(Ordering::Acquire) == 1).await;
        assert!(exit.dropped.load(Ordering::Acquire));
        assert!(!child.reaped.load(Ordering::Acquire));
    }
    assert_eq!(allocation_counts(&bridge), (VIEW_LIMIT - 1, VIEW_LIMIT));
    reject_ninth(&bridge, &runtime).await;
    assert_eq!(opens.load(Ordering::Relaxed), 1);

    fs::write(fixture.0.join("peer-next"), "").unwrap();
    assert_eq!(
        read_until_bytes(&bridge, &peer.view_id, b"peer-next").await,
        b"peer-next"
    );
    revision.store(1, Ordering::Release);
    let event = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let chunk = bridge.read(&peer.view_id).await.unwrap();
            assert!(!chunk.closed);
            if let Some(event) = chunk.geometry {
                break event;
            }
        }
    })
    .await
    .unwrap();
    let ack = GridAcknowledgement {
        renderer_id: Uuid::new_v4(),
        epoch: Some(epoch),
        revision: 1,
        size: event.status.committed.size,
        focused: false,
    };
    let redraw = bridge.acknowledge_grid(&peer.view_id, ack).await.unwrap();
    let live_peer = bridge.view(&peer.view_id).unwrap();
    let output = live_peer.output.lock().await;
    expire_drain(&bridge.views, live_peer.terminal.view_id, ack.revision);
    assert!(!live_peer.closed());
    drop(output);
    assert_eq!(redraw.status.terminal, peer.status.terminal);
    assert_eq!(redraw.bridge_pty.size, ack.size);
    assert_eq!(redraw.client.size, ack.size);
    assert_eq!(
        read_until_bytes(&bridge, &peer.view_id, b"peer-redraw").await,
        b"peer-redraw"
    );

    assert_eq!(allocation_counts(&bridge), (VIEW_LIMIT - 1, VIEW_LIMIT));
    reject_ninth(&bridge, &runtime).await;
    assert_eq!(bridge.read(&victim_id).await.unwrap().closure, Some(FULL));
    assert!(bridge.acknowledge_grid(&victim_id, ack).await.is_err());
    bridge.detach(&victim_id);
    exit.held.store(false, Ordering::Release);
    child.allow_reap.store(true, Ordering::Release);
    until(|| allocation_counts(&bridge) == (VIEW_LIMIT - 1, VIEW_LIMIT - 1)).await;
    assert_eq!(child.signals.load(Ordering::Relaxed), 1);
    let fresh = bridge.attach_observer(&runtime).await.unwrap();
    assert_ne!(fresh.view_id, victim_id);
    assert_eq!(fresh.status.terminal.target, victim.terminal.target);
    reject_ninth(&bridge, &runtime).await;
    assert_eq!(opens.load(Ordering::Relaxed), 2);
    assert_eq!(allocation_counts(&bridge), (VIEW_LIMIT, VIEW_LIMIT));
    writer.finish();
    drop(fillers);
    bridge.detach_all();
    until(|| allocation_counts(&bridge) == (0, 0)).await;
    server.abort();
}

fn allocation_counts(bridge: &DesktopBridge) -> (usize, usize) {
    let state = bridge.views.lock().unwrap();
    (state.live.len(), state.allocated.load(Ordering::Relaxed))
}

async fn reject_ninth(bridge: &DesktopBridge, runtime: &str) {
    let error = bridge
        .attach_observer(runtime)
        .await
        .err()
        .expect("admission exceeded capacity");
    assert_eq!(error.to_string(), "At most 8 local views may be attached");
}

async fn read_until_bytes(bridge: &DesktopBridge, id: &str, expected: &[u8]) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut data = Vec::new();
        while data.len() < expected.len() {
            let chunk = bridge.read(id).await.unwrap();
            assert!(!chunk.closed && chunk.geometry.is_none());
            data.extend(chunk.data);
        }
        data
    })
    .await
    .unwrap()
}

pub(super) struct StartupGate {
    attached: tokio::sync::oneshot::Sender<Uuid>,
    resume: tokio::sync::oneshot::Receiver<()>,
}

impl StartupGate {
    pub(super) async fn park(self, id: Uuid) {
        let _ = self.attached.send(id);
        let _ = self.resume.await;
    }
}

#[tokio::test]
async fn w1_terminal_startup_full_is_retired_before_attach_replies() {
    let fixture = AgentFixture::new();
    let target = target();
    let tmux = attachment_tmux(&fixture, target.generation, "ignore-size");
    fs::write(fixture.0.join("flood"), "").unwrap();
    let listener = UnixListener::bind(fixture.0.join("agent.sock")).unwrap();
    let runtime = target.runtime_id.to_string();
    let bridge = Arc::new(DesktopBridge::new(Some(fixture.0.clone()), tmux));
    let (attached, starting) = tokio::sync::oneshot::channel();
    let (resume, wait_reply) = tokio::sync::oneshot::channel();
    *bridge.startup_gate.lock().unwrap() = Some(StartupGate {
        attached,
        resume: wait_reply,
    });
    let start = {
        let bridge = bridge.clone();
        tokio::spawn(async move { bridge.attach_observer(&runtime).await })
    };
    let status = open_fixture(&listener, &target).await;
    let mut peer = attachment_status_peer(&listener, &status).await;
    let view_id = status.terminal.view_id;
    respond(&mut peer, Response::TerminalStatus { status }).await;
    assert_eq!(starting.await.unwrap(), view_id);
    fs::write(fixture.0.join("continue"), "").unwrap();
    until(|| allocation_counts(&bridge).0 == 0).await;
    assert!(!start.is_finished());
    assert_eq!(bridge.views.lock().unwrap().notices.len(), 1);
    resume.send(()).unwrap();
    let returned = start.await.unwrap().unwrap();
    assert_eq!(returned.view_id, view_id.to_string());
    assert_eq!(
        bridge.read(&returned.view_id).await.unwrap().closure,
        Some(FULL)
    );
    assert!(bridge.read(&returned.view_id).await.is_err());
    until(|| allocation_counts(&bridge) == (0, 0)).await;
}

#[tokio::test]
async fn w1_terminal_racing_eof_and_detach_have_one_owner_and_never_signal_reaped_child() {
    let bridge = DesktopBridge::new(None, "/unused/tmux".into());
    let (view, mut writer, child) = controlled_view(&bridge, &target(), Uuid::new_v4(), 0);
    let id = view.terminal.view_id.to_string();
    writer.requested().await;
    child.allow_reap.store(true, Ordering::Release);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    std::thread::scope(|scope| {
        let detach = barrier.clone();
        let (bridge, id) = (&bridge, &id);
        scope.spawn(move || {
            detach.wait();
            bridge.detach(id);
            bridge.detach(id);
        });
        barrier.wait();
        writer.data(b"");
    });
    until(|| allocation_counts(&bridge) == (0, 0)).await;
    assert_eq!(view.cleanup_confirmations.load(Ordering::Acquire), 3);
    assert_eq!(child.signals.load(Ordering::Relaxed), 0);
    assert!(bridge.read(&id).await.is_err());
    bridge.detach(&id);
    assert_eq!(child.signals.load(Ordering::Relaxed), 0);
    writer.finish();
}
