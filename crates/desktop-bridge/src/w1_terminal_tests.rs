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
    controlled_view_with_master(bridge, target, agent, revision, None)
}

fn controlled_view_with_master(
    bridge: &DesktopBridge,
    target: &TerminalTarget,
    agent: Uuid,
    revision: u64,
    master: Option<Box<dyn MasterPty + Send>>,
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
    let allocation = bridge.reserve(generation, None).unwrap();
    let view = Arc::new(View {
        terminal,
        directory: bridge.directory.clone().unwrap_or_default(),
        tmux: bridge.tmux.clone(),
        client_pid: 42,
        tty: master
            .as_ref()
            .and_then(|master| master.tty_name())
            .unwrap_or_default(),
        control: Arc::new(tokio::sync::Mutex::new(Control { lease: None })),
        resize: tokio::sync::Mutex::new(()),
        master: Mutex::new(master),
        output: tokio::sync::Mutex::new(Output {
            receiver,
            status,
            refresh_at: Instant::now() + Duration::from_secs(60),
            applied_revision: 0,
            pending: None,
            renderer_id: None,
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
    model::{CommittedGeometry, ProviderSample, ResizeIntent, TerminalTarget},
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
        let writable = bridge.read(&id).await.unwrap();
        assert!(writable.writable);
        let incarnation = writable.paste_incarnation.unwrap().to_string();

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
                .input(&id, b"after EOF".to_vec(), &incarnation, false)
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

fn refresh_fixture_control(view: &Arc<View>) -> tokio::task::JoinHandle<()> {
    let view = view.clone();
    tokio::spawn(async move { refresh_control(&view).await })
}

async fn acquire_fixture_writer(listener: &UnixListener, view: &View) -> UnixStream {
    let (peer, Request::AcquireTerminal { terminal }) = receive(listener).await else {
        panic!("expected scoped acquisition");
    };
    assert_eq!(terminal, view.terminal);
    peer
}

async fn release_fixture_writer(listener: &UnixListener, view: &View, expected: Uuid) {
    let (mut peer, Request::Release { runtime_id, token }) = receive(listener).await else {
        panic!("expected scoped release");
    };
    assert_eq!(
        (runtime_id, token),
        (view.terminal.target.runtime_id, expected)
    );
    respond(&mut peer, Response::Accepted {}).await;
}

async fn separate_writer_incarnation() -> String {
    let fixture = AgentFixture::new();
    let listener = UnixListener::bind(fixture.0.join("agent.sock")).unwrap();
    let bridge = DesktopBridge::new(Some(fixture.0.clone()), "/unused/tmux".into());
    let (view, reader, _) = controlled_view(&bridge, &target(), Uuid::new_v4(), 0);
    let id = view.terminal.view_id.to_string();
    let token = Uuid::new_v4();
    let epoch = view.output.lock().await.status.epoch.unwrap();
    let acquiring = refresh_fixture_control(&view);
    let mut peer = acquire_fixture_writer(&listener, &view).await;
    respond(&mut peer, Response::TerminalControl { token, epoch }).await;
    acquiring.await.unwrap();
    let writable = bridge.read(&id).await.unwrap();
    assert!(writable.writable);
    bridge.detach(&id);
    release_fixture_writer(&listener, &view, token).await;
    reader.finish();
    until(|| allocation_counts(&bridge) == (0, 0)).await;
    writable.paste_incarnation.unwrap().to_string()
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
      printf 'first diagnostic'
      /bin/dd if=/dev/zero bs={OUTPUT_CHUNK_SIZE} count=1 2>/dev/null
      printf 'last diagnostic'
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
    open_fixture_on_agent(listener, target, Uuid::new_v4()).await
}

async fn open_fixture_on_agent(
    listener: &UnixListener,
    target: &TerminalTarget,
    agent_epoch: Uuid,
) -> TerminalStatus {
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
            agent_epoch,
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

struct WriterCandidateFixture {
    bridge: Arc<DesktopBridge>,
    listener: UnixListener,
    target: TerminalTarget,
    row: Runtime,
    expected: TerminalInventoryBinding,
    epoch: ResizeEpoch,
    acquires: usize,
    inputs: usize,
    resizes: usize,
    _directory: AgentFixture,
}

impl WriterCandidateFixture {
    fn new() -> Self {
        let directory = AgentFixture::new();
        let target = target();
        let tmux = attachment_tmux(&directory, target.generation, "ignore-size");
        let listener = UnixListener::bind(directory.0.join("agent.sock")).unwrap();
        let bridge = Arc::new(DesktopBridge::new(Some(directory.0.clone()), tmux));
        let row = Runtime {
            session_id: Uuid::new_v4(),
            runtime_id: target.runtime_id,
            tmux_name: "fixture".into(),
            pane_id: target.pane.clone(),
            command: "fixture".into(),
            pane_pid: Some(target.pid),
            exited: false,
            width: 100,
            height: 30,
            origin: RuntimeOrigin::Managed,
            workspace_id: Some(Uuid::new_v4()),
            operation_id: Some(Uuid::new_v4()),
        };
        let expected = TerminalInventoryBinding {
            agent_pid: 12,
            session_id: row.session_id,
            operation_id: row.operation_id,
            workspace_id: row.workspace_id,
        };
        Self {
            bridge,
            listener,
            target,
            row,
            expected,
            epoch: ResizeEpoch {
                agent: Uuid::new_v4(),
                owner: Uuid::new_v4(),
            },
            acquires: 0,
            inputs: 0,
            resizes: 0,
            _directory: directory,
        }
    }

    fn restoration(&self, previous: TerminalView) -> AttachmentMode {
        AttachmentMode::Restore(TerminalRestoration {
            previous,
            agent_pid: self.expected.agent_pid,
            session_id: self.expected.session_id,
            operation_id: self.expected.operation_id,
        })
    }

    async fn park_proof(
        &self,
        mode: AttachmentMode,
    ) -> (
        tokio::task::JoinHandle<Result<Attachment>>,
        TerminalStatus,
        UnixStream,
    ) {
        let bridge = self.bridge.clone();
        let runtime = self.target.runtime_id.to_string();
        let attaching = tokio::spawn(async move {
            match mode {
                AttachmentMode::Candidate => bridge.prepare_attachment(&runtime).await,
                AttachmentMode::Restore(restoration) => {
                    bridge.restore_attachment(&runtime, restoration).await
                }
                AttachmentMode::Observer => bridge.attach_observer(&runtime).await,
                AttachmentMode::Ordinary => unreachable!(),
            }
        });
        let mut status =
            open_fixture_on_agent(&self.listener, &self.target, self.epoch.agent).await;
        let peer = attachment_status_peer(&self.listener, &status).await;
        status.epoch = Some(self.epoch);
        (attaching, status, peer)
    }

    async fn reply_inventory(&self, agent_pid: u32, row: Runtime) {
        let (mut health, Request::Health {}) = receive(&self.listener).await else {
            panic!("expected health for candidate inventory");
        };
        respond(
            &mut health,
            Response::Health {
                agent_pid,
                remote_execution_enabled: false,
            },
        )
        .await;
        let (mut list, Request::List {}) = receive(&self.listener).await else {
            panic!("expected candidate inventory list");
        };
        respond(
            &mut list,
            Response::Runtimes {
                runtimes: vec![row],
            },
        )
        .await;
    }

    async fn prepare(&self, mode: AttachmentMode) -> Attachment {
        let needs_inventory = mode.is_candidate();
        let (attaching, status, mut peer) = self.park_proof(mode).await;
        respond(&mut peer, Response::TerminalStatus { status }).await;
        if needs_inventory {
            self.reply_inventory(self.expected.agent_pid, self.row.clone())
                .await;
        }
        attaching.await.unwrap().unwrap()
    }

    fn confirming(&self, terminal: TerminalView) -> tokio::task::JoinHandle<Result<()>> {
        let bridge = self.bridge.clone();
        let expected = self.expected;
        tokio::spawn(async move { bridge.confirm_writer(terminal, expected).await })
    }

    async fn reject(&self, terminal: TerminalView, expected: TerminalInventoryBinding) {
        tokio::select! {
            result = self.bridge.confirm_writer(terminal, expected) => assert!(result.is_err()),
            _ = self.listener.accept() => panic!("ineligible candidate entered host validation"),
        }
    }

    async fn poll(&self, attachment: &Attachment) -> Chunk {
        let view = self.bridge.view(&attachment.view_id).unwrap();
        view.output.lock().await.refresh_at = Instant::now();
        let mut status = attachment.status.clone();
        status.epoch = Some(self.epoch);
        let reply = async {
            let mut peer = attachment_status_peer(&self.listener, &status).await;
            respond(&mut peer, Response::TerminalStatus { status }).await;
        };
        let (chunk, ()) = tokio::join!(self.bridge.read(&attachment.view_id), reply);
        let chunk = chunk.unwrap();
        assert_eq!(chunk.status.as_ref().unwrap().epoch, Some(self.epoch));
        chunk
    }

    async fn readonly(&self, attachment: &Attachment) {
        let sample = self.poll(attachment).await;
        assert!(!sample.writable && sample.paste_incarnation.is_none());
        assert!(
            self.bridge
                .input(
                    &attachment.view_id,
                    vec![65],
                    &Uuid::new_v4().to_string(),
                    false
                )
                .await
                .is_err()
        );
        assert!(
            self.bridge
                .resize(
                    &attachment.view_id,
                    attachment.status.committed.size,
                    self.epoch
                )
                .await
                .is_err()
        );
    }

    async fn acquire(&mut self, view: &View) -> UnixStream {
        let peer = acquire_fixture_writer(&self.listener, view).await;
        self.acquires += 1;
        self.epoch.owner = Uuid::new_v4();
        peer
    }

    async fn resize_as_writer(&mut self, view: &View, token: Uuid) {
        let id = view.terminal.view_id.to_string();
        let size = TerminalSize {
            cols: 101,
            rows: 31,
        };
        let admission = ResizeIntent {
            epoch: self.epoch,
            id: Uuid::new_v4(),
        };
        let reply = async {
            let (
                mut peer,
                Request::ResizeTerminal {
                    terminal,
                    token: actual_token,
                    sequence,
                    size: actual_size,
                },
            ) = receive(&self.listener).await
            else {
                panic!("expected one scoped resize");
            };
            assert_eq!(
                (terminal, actual_token, sequence, actual_size),
                (view.terminal.clone(), token, 0, size)
            );
            respond(
                &mut peer,
                Response::Resize {
                    resize: ResizeResult::Committed {
                        admission,
                        status: TerminalStatus {
                            terminal: view.terminal.clone(),
                            epoch: Some(self.epoch),
                            committed: CommittedGeometry {
                                size,
                                revision: 1,
                                intent: Some(admission),
                            },
                            provider: None,
                            observation: ObservationState::Unavailable,
                        },
                    },
                },
            )
            .await;
        };
        let (result, ()) = tokio::join!(self.bridge.resize(&id, size, self.epoch), reply);
        assert!(
            matches!(result.unwrap(), ResizeResult::Committed { admission: actual, .. } if actual == admission)
        );
        self.resizes += 1;
    }

    async fn no_effects(&self, epoch: ResizeEpoch) {
        assert_no_fixture_request(&self.listener).await;
        assert_eq!((self.acquires, self.inputs, self.resizes), (0, 0, 0));
        assert_eq!(self.epoch, epoch);
    }

    async fn finish(&self) {
        self.bridge.detach_all();
        until(|| allocation_counts(&self.bridge) == (0, 0)).await;
        assert_no_fixture_request(&self.listener).await;
    }
}

#[tokio::test]
async fn w1_terminal_current_candidate_confirms_once_and_sends_fresh_input() {
    for restore in [false, true] {
        let mut fixture = WriterCandidateFixture::new();
        let original = fixture.epoch;
        let mode = if restore {
            let observer = fixture.prepare(AttachmentMode::Observer).await;
            fixture
                .reject(observer.status.terminal.clone(), fixture.expected)
                .await;
            fixture.bridge.detach(&observer.view_id);
            fixture.restoration(observer.status.terminal)
        } else {
            AttachmentMode::Candidate
        };
        let candidate = fixture.prepare(mode).await;
        fixture.readonly(&candidate).await;
        fixture.no_effects(original).await;
        let view = fixture.bridge.view(&candidate.view_id).unwrap();
        let confirmation = fixture.confirming(view.terminal.clone());
        fixture
            .reply_inventory(fixture.expected.agent_pid, fixture.row.clone())
            .await;
        let mut status = attachment_status_peer(&fixture.listener, &candidate.status).await;
        let duplicate = fixture.confirming(view.terminal.clone());
        fixture
            .reply_inventory(fixture.expected.agent_pid, fixture.row.clone())
            .await;
        let mut duplicate_status =
            attachment_status_peer(&fixture.listener, &candidate.status).await;
        fixture.no_effects(original).await;
        respond(
            &mut status,
            Response::TerminalStatus {
                status: candidate.status.clone(),
            },
        )
        .await;
        confirmation.await.unwrap().unwrap();
        respond(
            &mut duplicate_status,
            Response::TerminalStatus {
                status: candidate.status.clone(),
            },
        )
        .await;
        assert!(duplicate.await.unwrap().is_err());

        let mut acquire = fixture.acquire(&view).await;
        assert_ne!(fixture.epoch, original);
        fixture
            .reject(view.terminal.clone(), fixture.expected)
            .await;
        let token = Uuid::new_v4();
        respond(
            &mut acquire,
            Response::TerminalControl {
                token,
                epoch: fixture.epoch,
            },
        )
        .await;
        assert_eq!(
            view.control.lock().await.lease.as_ref().unwrap().token,
            token
        );
        let grant = fixture.poll(&candidate).await;
        assert!(grant.writable);
        send_fixture_input(
            &fixture.listener,
            &fixture.bridge,
            &view,
            token,
            grant.paste_incarnation.unwrap(),
            0,
            b"fresh",
        )
        .await;
        fixture.inputs += 1;
        assert!(
            fixture
                .bridge
                .resize(
                    &candidate.view_id,
                    candidate.status.committed.size,
                    original
                )
                .await
                .is_err()
        );
        fixture.resize_as_writer(&view, token).await;
        assert_eq!(
            (fixture.acquires, fixture.inputs, fixture.resizes),
            (1, 1, 1)
        );
        fixture.bridge.detach(&candidate.view_id);
        release_fixture_writer(&fixture.listener, &view, token).await;
        fixture.finish().await;
    }
}

#[tokio::test]
async fn w1_terminal_restore_proof_canceled_before_reply_preserves_host_epoch() {
    let fixture = WriterCandidateFixture::new();
    let original = fixture.epoch;
    let observer = fixture.prepare(AttachmentMode::Observer).await;
    let (attached, starting) = tokio::sync::oneshot::channel();
    let (resume, wait_reply) = tokio::sync::oneshot::channel();
    *fixture.bridge.startup_gate.lock().unwrap() = Some(ViewGate {
        attached,
        resume: wait_reply,
    });
    let (restoring, status, mut proof) = fixture
        .park_proof(fixture.restoration(observer.status.terminal))
        .await;
    let id = status.terminal.view_id;
    assert!(!restoring.is_finished());
    fixture.bridge.detach(&observer.view_id);
    fixture
        .reject(status.terminal.clone(), fixture.expected)
        .await;
    fixture.no_effects(original).await;
    respond(&mut proof, Response::TerminalStatus { status }).await;
    fixture
        .reply_inventory(fixture.expected.agent_pid, fixture.row.clone())
        .await;
    assert_eq!(starting.await.unwrap(), id);
    assert!(!restoring.is_finished());
    fixture.no_effects(original).await;
    resume.send(()).unwrap();
    let stale = restoring.await.unwrap().unwrap();
    assert_eq!(stale.view_id, id.to_string());
    fixture.readonly(&stale).await;
    fixture.no_effects(original).await;
    fixture.bridge.detach(&stale.view_id);
    fixture
        .reject(stale.status.terminal, fixture.expected)
        .await;
    fixture.finish().await;
    fixture.no_effects(original).await;
}

#[tokio::test]
async fn w1_terminal_confirmation_rejects_inexact_and_superseded_candidates() {
    let fixture = WriterCandidateFixture::new();
    let original = fixture.epoch;
    let candidate = fixture.prepare(AttachmentMode::Candidate).await;
    let terminal = &candidate.status.terminal;
    for wrong in [
        TerminalInventoryBinding {
            agent_pid: fixture.expected.agent_pid + 1,
            ..fixture.expected
        },
        TerminalInventoryBinding {
            session_id: Uuid::new_v4(),
            ..fixture.expected
        },
        TerminalInventoryBinding {
            operation_id: None,
            ..fixture.expected
        },
        TerminalInventoryBinding {
            workspace_id: None,
            ..fixture.expected
        },
    ] {
        fixture.reject(terminal.clone(), wrong).await;
    }
    for wrong in [
        TerminalTarget {
            runtime_id: Uuid::new_v4(),
            ..fixture.target.clone()
        },
        TerminalTarget {
            generation: Uuid::new_v4(),
            ..fixture.target.clone()
        },
        TerminalTarget {
            session: "$1".into(),
            ..fixture.target.clone()
        },
        TerminalTarget {
            window: "@1".into(),
            ..fixture.target.clone()
        },
        TerminalTarget {
            pane: "%1".into(),
            ..fixture.target.clone()
        },
        TerminalTarget {
            pid: fixture.target.pid + 1,
            ..fixture.target.clone()
        },
        TerminalTarget {
            process_start: fixture.target.process_start + 1,
            ..fixture.target.clone()
        },
    ] {
        fixture
            .reject(
                TerminalView {
                    target: wrong,
                    ..terminal.clone()
                },
                fixture.expected,
            )
            .await;
    }
    fixture
        .reject(
            TerminalView {
                agent_epoch: Uuid::new_v4(),
                ..terminal.clone()
            },
            fixture.expected,
        )
        .await;
    fixture
        .reject(
            TerminalView {
                view_id: Uuid::new_v4(),
                ..terminal.clone()
            },
            fixture.expected,
        )
        .await;
    fixture.no_effects(original).await;

    let replacement = fixture.prepare(AttachmentMode::Candidate).await;
    assert!(fixture.bridge.view(&candidate.view_id).is_ok());
    fixture.reject(terminal.clone(), fixture.expected).await;
    fixture.bridge.detach(&replacement.view_id);
    fixture.reject(terminal.clone(), fixture.expected).await;
    fixture
        .reject(replacement.status.terminal, fixture.expected)
        .await;
    fixture.finish().await;
    fixture.no_effects(original).await;
}

#[tokio::test]
async fn w1_terminal_confirmation_rechecks_inventory_and_parked_native_validation() {
    for boundary in [
        "agent",
        "runtime inventory",
        "target",
        "detach",
        "replace",
        "generation",
        "expiry",
    ] {
        let fixture = WriterCandidateFixture::new();
        let original = fixture.epoch;
        let candidate = fixture.prepare(AttachmentMode::Candidate).await;
        let terminal = &candidate.status.terminal;
        let confirmation = fixture.confirming(terminal.clone());
        let mut row = fixture.row.clone();
        if boundary == "runtime inventory" {
            row.exited = true;
        }
        let agent_pid = if boundary == "agent" {
            fixture.expected.agent_pid + 1
        } else {
            fixture.expected.agent_pid
        };
        fixture.reply_inventory(agent_pid, row).await;
        if !matches!(boundary, "agent" | "runtime inventory") {
            let mut peer = attachment_status_peer(&fixture.listener, &candidate.status).await;
            fixture.no_effects(original).await;
            match boundary {
                "target" => {
                    let mut status = candidate.status.clone();
                    status.terminal.target.process_start += 1;
                    respond(&mut peer, Response::TerminalStatus { status }).await;
                }
                "detach" | "generation" => {
                    peer.writable().await.unwrap();
                    if boundary == "detach" {
                        fixture.bridge.detach(&candidate.view_id);
                    } else {
                        fixture.bridge.detach_all();
                    }
                    let reply = Response::TerminalStatus {
                        status: candidate.status.clone(),
                    };
                    let frame = encode_local_observation(&reply).unwrap() + "\n";
                    assert_eq!(peer.try_write(frame.as_bytes()).unwrap(), frame.len());
                }
                "replace" => {
                    let replacement = fixture.prepare(AttachmentMode::Candidate).await;
                    respond(
                        &mut peer,
                        Response::TerminalStatus {
                            status: candidate.status.clone(),
                        },
                    )
                    .await;
                    fixture.bridge.detach(&replacement.view_id);
                }
                "expiry" => {
                    {
                        let mut views = fixture.bridge.views.lock().unwrap();
                        views
                            .writer_candidates
                            .get_mut(&fixture.target.runtime_id)
                            .unwrap()
                            .proof = Some((fixture.expected, Instant::now()));
                    }
                    respond(
                        &mut peer,
                        Response::TerminalStatus {
                            status: candidate.status.clone(),
                        },
                    )
                    .await;
                }
                _ => unreachable!(),
            }
        }
        assert!(confirmation.await.unwrap().is_err(), "{boundary}");
        fixture.no_effects(original).await;
        fixture.finish().await;
    }
}

struct GeometryFixture {
    bridge: DesktopBridge,
    listener: UnixListener,
    view: Arc<View>,
    writer: ReadControl,
    owner: Arc<View>,
    _owner_reader: ReadControl,
    status: TerminalStatus,
    token: Uuid,
    renderer: Uuid,
    start: Instant,
    _slave: Box<dyn portable_pty::SlavePty + Send>,
    directory: AgentFixture,
}

const GRID_A: TerminalSize = TerminalSize {
    cols: 118,
    rows: 38,
};
const GRID_B: TerminalSize = TerminalSize {
    cols: 124,
    rows: 42,
};

impl GeometryFixture {
    async fn new() -> Self {
        let directory = AgentFixture::new();
        let target = target();
        let tmux = directory.0.join("tmux");
        let program = r#"#!/bin/sh
set -eu
root=$(dirname "$0")
test "$3" = "$root/tmux.sock"
tty=$(cat "$root/tty")
case "$4" in
  list-clients)
    test "$6" = '$0'
    set -- $(stty size < "$tty")
    printf '42|%s|$0|@0|%%0|GENERATION|ignore-size|%s|%s\n' "$tty" "$2" "$1"
    ;;
  refresh-client)
    test "$6" = "$tty"
    printf 'redraw\n' >> "$root/redraws"
    ;;
  *) exit 1 ;;
esac
"#;
        fs::write(
            &tmux,
            program.replace("GENERATION", &target.generation.to_string()),
        )
        .unwrap();
        fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
        let listener = UnixListener::bind(directory.0.join("agent.sock")).unwrap();
        let bridge = DesktopBridge::new(Some(directory.0.clone()), tmux);
        let initial = TerminalSize {
            cols: 110,
            rows: 32,
        };
        let pair = native_pty_system().openpty(pty_size(initial)).unwrap();
        let agent = Uuid::new_v4();
        let (view, mut writer, _) =
            controlled_view_with_master(&bridge, &target, agent, 0, Some(pair.master));
        fs::write(directory.0.join("tty"), view.tty.to_str().unwrap()).unwrap();
        writer.requested().await;
        let (owner, owner_reader, _) = controlled_view(&bridge, &target, agent, 0);
        let mut status = view.output.lock().await.status.clone();
        let token = Uuid::new_v4();
        let epoch = ResizeEpoch {
            agent,
            owner: Uuid::new_v4(),
        };
        status.epoch = Some(epoch);
        let grant = async {
            let (mut peer, request) = receive(&listener).await;
            assert!(
                matches!(request, Request::AcquireTerminal { terminal } if terminal == owner.terminal)
            );
            respond(&mut peer, Response::TerminalControl { token, epoch }).await;
        };
        tokio::join!(refresh_control(&owner), grant);
        Self {
            bridge,
            listener,
            view,
            writer,
            owner,
            _owner_reader: owner_reader,
            status,
            token,
            renderer: Uuid::new_v4(),
            start: Instant::now() + Duration::from_secs(60),
            _slave: pair.slave,
            directory,
        }
    }

    async fn commit(&mut self, size: TerminalSize) {
        let owner = self.owner.terminal.view_id.to_string();
        let epoch = self.status.epoch.unwrap();
        let sequence = self.status.committed.revision;
        let admission = ResizeIntent {
            id: Uuid::new_v4(),
            epoch,
        };
        let revision = sequence + 1;
        self.status.committed = CommittedGeometry {
            size,
            revision,
            intent: Some(admission),
        };
        self.status.provider = Some(ProviderSample {
            size,
            sampled_at_us: revision,
        });
        self.status.observation = ObservationState::Observed;
        let mut owner_status = self.status.clone();
        owner_status.terminal = self.owner.terminal.clone();
        let agent = async {
            let (mut peer, request) = receive(&self.listener).await;
            assert!(
                matches!(request, Request::ResizeTerminal { terminal, token, sequence: actual, size: proposed }
                if terminal == self.owner.terminal && token == self.token && actual == sequence && proposed == size)
            );
            respond(
                &mut peer,
                Response::Resize {
                    resize: ResizeResult::Committed {
                        admission,
                        status: owner_status.clone(),
                    },
                },
            )
            .await;
        };
        let (reply, ()) = tokio::join!(self.bridge.resize(&owner, size, epoch), agent);
        assert_eq!(
            reply.unwrap(),
            ResizeResult::Committed {
                admission,
                status: owner_status
            }
        );
    }

    async fn reply_status(&self, status: TerminalStatus) {
        let mut peer = attachment_status_peer(&self.listener, &self.status).await;
        respond(&mut peer, Response::TerminalStatus { status }).await;
    }

    fn key(&self) -> String {
        self.view.terminal.view_id.to_string()
    }

    async fn issue_first(&self) -> GeometryEvent {
        let key = self.key();
        self.view.output.lock().await.refresh_at = self.start;
        let drive_producer = async {
            self.reply_status(self.status.clone()).await;
            until(|| self.view.reader_state.load(Ordering::Acquire) == DRAIN).await;
            self.writer.would_block();
            self.reply_status(self.status.clone()).await;
        };
        let pull_boundary = async {
            loop {
                let chunk = self
                    .bridge
                    .read_with_clock(&key, || self.start)
                    .await
                    .unwrap();
                assert!(chunk.data.is_empty() && !chunk.writable && !chunk.closed);
                if let Some(event) = chunk.geometry {
                    assert!(chunk.status.is_none());
                    return event;
                }
            }
        };
        let (event, ()) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(pull_boundary, drive_producer)
        })
        .await
        .unwrap();
        assert_eq!(event.status, self.status);
        event
    }

    async fn read_successor(&self) -> GeometryEvent {
        let key = self.key();
        let (chunk, ()) = tokio::join!(
            self.bridge.read(&key),
            self.reply_status(self.status.clone())
        );
        let chunk = chunk.unwrap();
        assert!(
            chunk.status.is_none() && chunk.data.is_empty() && !chunk.writable && !chunk.closed
        );
        let event = chunk.geometry.unwrap();
        assert_eq!(event.status, self.status);
        event
    }

    fn acknowledgement(&self, event: &GeometryEvent) -> GridAcknowledgement {
        GridAcknowledgement {
            renderer_id: self.renderer,
            epoch: event.status.epoch,
            revision: event.status.committed.revision,
            size: event.status.committed.size,
            focused: true,
        }
    }

    fn redraws(&self) -> usize {
        fs::read_to_string(self.directory.0.join("redraws")).map_or(0, |log| log.lines().count())
    }

    async fn assert_paused(&mut self, applied: u64) {
        let output = self.view.output.lock().await;
        assert_eq!(output.applied_revision, applied);
        assert_eq!(output.drain_started, Some(self.start));
        assert_eq!(self.view.drain_revision.load(Ordering::Relaxed), 1);
        assert_eq!(self.view.reader_state.load(Ordering::Acquire), PAUSE);
        assert!(output.receiver.is_empty());
        assert!(self.writer.requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn w1_resize_superseding_ack() {
    let mut fixture = GeometryFixture::new().await;
    let key = fixture.key();
    fixture.commit(GRID_A).await;
    let a = fixture.issue_first().await;
    let ack_a = fixture.acknowledgement(&a);
    assert_eq!(a.bridge_pty.size, ack_a.size);
    assert_eq!(a.client.size, ack_a.size);
    fixture.commit(GRID_B).await;
    fixture.writer.data(b"after B");
    fixture.assert_paused(0).await;

    let (reply, ()) = tokio::join!(
        fixture.bridge.acknowledge_grid(&key, ack_a),
        fixture.reply_status(fixture.status.clone()),
    );
    let reply = reply.unwrap();
    assert_eq!(reply.status.committed.revision, ack_a.revision);
    assert_eq!(reply.status.epoch, ack_a.epoch);
    assert_eq!(reply.status, a.status);
    assert_eq!(reply.status.terminal, fixture.view.terminal);
    assert_eq!(reply.bridge_pty.size, ack_a.size);
    assert_eq!(reply.client.size, ack_a.size);
    assert_eq!(fixture.redraws(), 0);
    fixture.assert_paused(1).await;

    let b = fixture.read_successor().await;
    assert_eq!(b.status.terminal, a.status.terminal);
    let ack_b = fixture.acknowledgement(&b);
    assert_eq!(b.bridge_pty.size, ack_b.size);
    assert_eq!(b.client.size, ack_b.size);
    assert_eq!(b.status.provider.unwrap().size, ack_b.size);
    fixture.assert_paused(1).await;
    for wrong in [
        ack_a,
        GridAcknowledgement {
            renderer_id: Uuid::new_v4(),
            ..ack_b
        },
    ] {
        assert!(fixture.bridge.acknowledge_grid(&key, wrong).await.is_err());
        fixture.assert_paused(1).await;
    }

    let (reply, ()) = tokio::join!(
        fixture.bridge.acknowledge_grid(&key, ack_b),
        fixture.reply_status(fixture.status.clone()),
    );
    let reply = reply.unwrap();
    assert_eq!(reply.status, b.status);
    assert_eq!(reply.bridge_pty.size, ack_b.size);
    assert_eq!(reply.client.size, ack_b.size);
    assert_eq!(fixture.redraws(), 1);
    assert_eq!(fixture.view.reader_state.load(Ordering::Acquire), RESUME);
    assert_eq!(fixture.view.output.lock().await.applied_revision, 2);
    assert_eq!(
        read_until_bytes(&fixture.bridge, &key, b"after B").await,
        b"after B",
    );
    drop(fixture);

    let mut fixture = GeometryFixture::new().await;
    let key = fixture.key();
    fixture.commit(GRID_A).await;
    let a = fixture.issue_first().await;
    fixture.commit(GRID_B).await;
    let mut replacement = fixture.status.clone();
    replacement.terminal.target.process_start += 1;
    fixture.writer.data(b"wrong attachment");
    let (reply, ()) = tokio::join!(
        fixture
            .bridge
            .acknowledge_grid(&key, fixture.acknowledgement(&a)),
        fixture.reply_status(replacement),
    );
    assert_eq!(
        reply.unwrap_err().to_string(),
        "Terminal attachment identity changed"
    );
    assert_eq!(fixture.view.reader_state.load(Ordering::Acquire), CLOSE);
    assert!(fixture.bridge.read(&key).await.is_err());
    assert_eq!(fixture.redraws(), 0);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), fixture.writer.requests.recv())
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn w1_resize_w1_terminal_original_drain_deadline() {
    let tick = Duration::from_nanos(1);
    for (elapsed, resumes, superseded) in [
        (DRAIN_TIMEOUT - tick, true, false),
        (DRAIN_TIMEOUT, false, false),
        (DRAIN_TIMEOUT + tick, false, false),
        (DRAIN_TIMEOUT, false, true),
    ] {
        let mut fixture = GeometryFixture::new().await;
        let key = fixture.key();
        fixture.commit(GRID_A).await;
        let mut event = fixture.issue_first().await;
        if superseded {
            fixture.commit(GRID_B).await;
            let (reply, ()) = tokio::join!(
                fixture.bridge.acknowledge_grid_with_clock(
                    &key,
                    fixture.acknowledgement(&event),
                    || fixture.start + tick,
                ),
                fixture.reply_status(fixture.status.clone()),
            );
            assert_eq!(reply.unwrap().status, event.status);
            event = fixture.read_successor().await;
        }
        let applied = event.status.committed.revision - 1;
        fixture.writer.data(b"after redraw");
        fixture.assert_paused(applied).await;

        let (attached, reached) = tokio::sync::oneshot::channel();
        let (complete, resume) = tokio::sync::oneshot::channel();
        *fixture.bridge.grid_completion_gate.lock().unwrap() = Some(ViewGate { attached, resume });
        let now = Cell::new(fixture.start);
        let check_clock = || {
            if now.get() != fixture.start {
                assert!(fixture.bridge.views.try_lock().is_err());
            }
            now.get()
        };
        let drive_redraw = async {
            fixture.reply_status(fixture.status.clone()).await;
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), reached)
                    .await
                    .unwrap()
                    .unwrap(),
                fixture.view.terminal.view_id,
            );
            assert_eq!(fixture.redraws(), 1);
            assert_eq!(fixture.view.reader_state.load(Ordering::Acquire), PAUSE);
            expire_drain(&fixture.bridge.views, fixture.view.terminal.view_id, 2);
            assert!(!fixture.view.closed());
            now.set(fixture.start + elapsed);
            complete.send(()).unwrap();
        };
        let (reply, ()) = tokio::join!(
            fixture.bridge.acknowledge_grid_with_clock(
                &key,
                fixture.acknowledgement(&event),
                check_clock
            ),
            drive_redraw,
        );
        expire_drain(&fixture.bridge.views, fixture.view.terminal.view_id, 1);
        if resumes {
            assert_eq!(reply.unwrap().status, event.status);
            assert_eq!(fixture.view.reader_state.load(Ordering::Acquire), RESUME);
            assert_eq!(
                read_until_bytes(&fixture.bridge, &key, b"after redraw").await,
                b"after redraw",
            );
        } else {
            assert_eq!(reply.unwrap_err().to_string(), DRAIN_TIMEOUT_ERROR);
            assert_eq!(fixture.view.reader_state.load(Ordering::Acquire), CLOSE);
            assert_eq!(fixture.view.output.lock().await.applied_revision, applied);
            let closed = fixture.bridge.read(&key).await.unwrap();
            assert_eq!(closed.closure, Some(TerminalClosure::GeometryTimeout));
            assert!(closed.closed && !closed.writable && closed.data.is_empty());
            assert!(closed.geometry.is_none() && closed.status.is_none());
            assert!(fixture.bridge.read(&key).await.is_err());
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), fixture.writer.requests.recv())
                    .await
                    .unwrap(),
                None
            );
        }
    }
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
    let incarnation = separate_writer_incarnation().await;
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
                .input(&id, b"pre-proof".to_vec(), &incarnation, false)
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
            bridge.detach(&id);
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
                .input(&id, b"post-proof".to_vec(), &incarnation, false)
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

enum EofRefresh {
    Newer,
    Unavailable,
    Changed,
    Replaced,
    Detached,
    Expired,
}

#[tokio::test]
async fn w1_terminal_eof_successful_newer_refresh_preserves_verified_fifo() {
    check_eof_refresh(EofRefresh::Newer).await;
}

#[tokio::test]
async fn w1_terminal_eof_refresh_error_preserves_verified_fifo() {
    check_eof_refresh(EofRefresh::Unavailable).await;
}

#[tokio::test]
async fn w1_terminal_eof_refresh_identity_change_discards_fifo() {
    check_eof_refresh(EofRefresh::Changed).await;
    check_eof_refresh(EofRefresh::Replaced).await;
}

#[tokio::test]
async fn w1_terminal_eof_refresh_detach_discards_fifo() {
    check_eof_refresh(EofRefresh::Detached).await;
}

#[tokio::test]
async fn w1_terminal_eof_refresh_expired_registration_discards_fifo() {
    check_eof_refresh(EofRefresh::Expired).await;
}

async fn check_eof_refresh(outcome: EofRefresh) {
    let fixture = AgentFixture::new();
    fs::write(fixture.0.join("hold"), "").unwrap();
    let target = target();
    let tmux = attachment_tmux(&fixture, target.generation, "ignore-size");
    let listener = UnixListener::bind(fixture.0.join("agent.sock")).unwrap();
    let bridge = DesktopBridge::new(Some(fixture.0.clone()), tmux);
    let runtime = target.runtime_id.to_string();
    let prove = async {
        let mut status = open_fixture(&listener, &target).await;
        status.epoch = Some(ResizeEpoch {
            agent: status.terminal.agent_epoch,
            owner: Uuid::new_v4(),
        });
        let mut peer = attachment_status_peer(&listener, &status).await;
        respond(
            &mut peer,
            Response::TerminalStatus {
                status: status.clone(),
            },
        )
        .await;
        status
    };
    let (attached, mut status) = tokio::join!(bridge.attach_observer(&runtime), prove);
    let attached = attached.unwrap();
    assert_eq!(attached.status, status);
    let id = attached.view_id;
    let view = bridge.view(&id).unwrap();
    let token = Uuid::new_v4();
    let acquiring = refresh_fixture_control(&view);
    let mut peer = acquire_fixture_writer(&listener, &view).await;
    respond(
        &mut peer,
        Response::TerminalControl {
            token,
            epoch: status.epoch.unwrap(),
        },
    )
    .await;
    acquiring.await.unwrap();
    view.output.lock().await.refresh_at = Instant::now() + Duration::from_secs(60);
    assert_eq!(
        read_until_bytes(&bridge, &id, b"buffered attachment output").await,
        b"buffered attachment output"
    );
    let writable = bridge.read(&id).await.unwrap();
    assert!(writable.writable);
    let incarnation = writable.paste_incarnation.unwrap().to_string();

    view.output.lock().await.refresh_at = Instant::now();
    let mut reading = std::pin::pin!(bridge.read(&id));
    let mut refresh = tokio::select! {
        peer = attachment_status_peer(&listener, &status) => peer,
        _ = &mut reading => panic!("read completed before the scoped refresh reply"),
    };
    fs::write(fixture.0.join("continue"), "").unwrap();
    tokio::time::timeout(Duration::from_secs(2), view.retired())
        .await
        .unwrap();
    assert!(view.eof_draining());
    assert_eq!(allocation_counts(&bridge), (0, 1));
    assert_eq!(
        bridge.read(&id).await.err().unwrap().to_string(),
        "Terminal read already in progress"
    );
    assert_eq!(
        bridge
            .input(&id, b"after EOF".to_vec(), &incarnation, false)
            .await
            .unwrap_err()
            .to_string(),
        "View detached"
    );
    release_fixture_writer(&listener, &view, token).await;
    until(|| view.allocation.strong_count() == 1).await;
    assert_eq!(view.cleanup_confirmations.load(Ordering::Acquire), 3);
    assert!(view.master.lock().unwrap().is_none());

    let expires = bridge.views.lock().unwrap().drains[&view.terminal.view_id].expires;
    let revision = status.committed.revision;
    let reply = match outcome {
        EofRefresh::Newer | EofRefresh::Detached | EofRefresh::Expired => {
            status.committed.revision += 1;
            status.committed.size.cols += 1;
            status.committed.intent = Some(remote_sessions_host::model::ResizeIntent {
                id: Uuid::new_v4(),
                epoch: status.epoch.unwrap(),
            });
            Response::TerminalStatus { status }
        }
        EofRefresh::Unavailable => Response::Error {
            message: "later observation failed".into(),
        },
        EofRefresh::Changed => Response::TerminalChanged {},
        EofRefresh::Replaced => {
            status.terminal.target.process_start += 1;
            Response::TerminalStatus { status }
        }
    };
    match outcome {
        EofRefresh::Detached => bridge.detach(&id),
        EofRefresh::Expired => {
            let mut views = bridge.views.lock().unwrap();
            views
                .drains
                .get_mut(&view.terminal.view_id)
                .unwrap()
                .expires = Instant::now();
        }
        _ => {}
    }
    respond(&mut refresh, reply).await;
    let result = reading.await;
    match outcome {
        EofRefresh::Newer | EofRefresh::Unavailable => {
            let mut chunk = result.unwrap();
            assert_eq!(
                bridge.views.lock().unwrap().drains[&view.terminal.view_id].expires,
                expires
            );
            let mut data = Vec::new();
            let mut frames = 0;
            loop {
                assert_read_only_drain(&chunk);
                if chunk.closed {
                    assert_eq!(chunk.closure, Some(TerminalClosure::Eof));
                    assert!(chunk.data.is_empty());
                    break;
                }
                assert!(chunk.closure.is_none() && !chunk.data.is_empty());
                assert!(chunk.data.len() <= OUTPUT_CHUNK_SIZE);
                data.extend(chunk.data);
                frames += 1;
                assert!(frames <= OUTPUT_SLOTS);
                chunk = bridge.read(&id).await.unwrap();
            }
            let expected = [
                b"first diagnostic".as_slice(),
                &[0; OUTPUT_CHUNK_SIZE],
                b"last diagnostic".as_slice(),
            ]
            .concat();
            assert!(frames > 1);
            assert_eq!(data, expected);
        }
        EofRefresh::Changed | EofRefresh::Replaced => {
            assert_eq!(
                result.err().unwrap().to_string(),
                "Terminal attachment identity changed"
            );
        }
        EofRefresh::Detached => {
            let chunk = result.unwrap();
            assert_eq!(chunk.closure, Some(TerminalClosure::Detached));
            assert!(chunk.closed && chunk.data.is_empty());
            assert_read_only_drain(&chunk);
        }
        EofRefresh::Expired => assert!(result.is_err()),
    }
    assert_eq!(
        bridge.read(&id).await.err().unwrap().to_string(),
        "View detached"
    );
    until(|| allocation_counts(&bridge) == (0, 0)).await;
    {
        let output = view.output.lock().await;
        assert!(output.receiver.is_empty());
        assert_eq!(output.status.committed.revision, revision);
        assert!(output.pending.is_none() && output.renderer_id.is_none());
        assert_eq!(view.drain_revision.load(Ordering::Relaxed), 0);
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(25), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn w1_terminal_status_refresh_respects_active_geometry_timeout() {
    let fixture = AgentFixture::new();
    let listener = UnixListener::bind(fixture.0.join("agent.sock")).unwrap();
    let bridge = DesktopBridge::new(Some(fixture.0.clone()), "/unused/tmux".into());
    let (view, mut reader, _) = controlled_view(&bridge, &target(), Uuid::new_v4(), 1);
    let id = view.terminal.view_id.to_string();
    reader.requested().await;
    reader.data(b"before boundary");
    assert_eq!(bridge.read(&id).await.unwrap().data, b"before boundary");
    reader.requested().await;
    assert_eq!(view.reader_state.load(Ordering::Acquire), DRAIN);
    view.output.lock().await.refresh_at = Instant::now();

    let mut reading = std::pin::pin!(bridge.read(&id));
    let (refresh, request) = tokio::select! {
        request = receive(&listener) => request,
        _ = &mut reading => panic!("read finished before the scoped refresh"),
    };
    let Request::TerminalStatus { terminal } = request else {
        panic!("expected scoped status refresh");
    };
    assert_eq!(terminal, view.terminal);
    reader.data(b"unverified across boundary");
    reader.requested().await;
    let chunk = tokio::time::timeout(DRAIN_TIMEOUT + Duration::from_secs(1), reading)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(chunk.closure, Some(TerminalClosure::GeometryTimeout));
    assert!(chunk.closed && chunk.data.is_empty());
    assert_read_only_drain(&chunk);
    assert!(!view.eof_draining());
    drop(refresh);
    assert!(bridge.read(&id).await.is_err());
    reader.finish();
    until(|| allocation_counts(&bridge) == (0, 0)).await;
    assert!(view.output.lock().await.receiver.is_empty());
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
    let incarnation = {
        let control = fresh.control.lock().await;
        let lease = control.lease.as_ref().unwrap();
        assert_eq!(lease.token, successor_token);
        lease.incarnation.to_string()
    };
    let fresh_input = {
        let bridge = bridge.clone();
        let view = fresh.terminal.view_id.to_string();
        tokio::spawn(async move { bridge.input(&view, vec![b'F'], &incarnation, false).await })
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

async fn grant_fixture_writer(
    listener: &UnixListener,
    bridge: &DesktopBridge,
    view: &Arc<View>,
) -> (Uuid, ResizeEpoch, Uuid) {
    let token = Uuid::new_v4();
    let epoch = view.output.lock().await.status.epoch.unwrap();
    let acquiring = refresh_fixture_control(view);
    let mut peer = acquire_fixture_writer(listener, view).await;
    respond(&mut peer, Response::TerminalControl { token, epoch }).await;
    acquiring.await.unwrap();
    let chunk = bridge
        .read(&view.terminal.view_id.to_string())
        .await
        .unwrap();
    assert!(chunk.writable);
    (token, epoch, chunk.paste_incarnation.unwrap())
}

async fn rejected_fixture_input(bridge: &DesktopBridge, view: &View, incarnation: Uuid) -> Uuid {
    let before = sample_clock().unwrap();
    let error = bridge
        .input(
            &view.terminal.view_id.to_string(),
            b"obsolete".to_vec(),
            &incarnation.to_string(),
            false,
        )
        .await
        .unwrap_err();
    let after = sample_clock().unwrap();
    let (rejection, observation) = terminal_rejection(error, None, true);
    let TerminalInputError::Rejected(NativeRejection::StaleInput {
        view_id,
        rejection_id,
        event_time_us,
    }) = rejection
    else {
        panic!("expected pre-dispatch stale input metadata, got {rejection:?}");
    };
    assert_eq!(view_id, view.terminal.view_id);
    assert_ne!(rejection_id, view_id);
    assert_eq!(rejection_id.get_version_num(), 4);
    assert!((before..=after).contains(&event_time_us));
    assert!(observation.is_none());
    rejection_id
}

async fn send_fixture_input(
    listener: &UnixListener,
    bridge: &DesktopBridge,
    view: &View,
    token: Uuid,
    incarnation: Uuid,
    sequence: u64,
    bytes: &[u8],
) {
    let id = view.terminal.view_id.to_string();
    let incarnation = incarnation.to_string();
    let input = bridge.input(&id, bytes.to_vec(), &incarnation, false);
    let accept = async {
        let (
            mut peer,
            Request::Input {
                runtime_id,
                token: actual_token,
                sequence: actual_sequence,
                data,
                paste,
            },
        ) = receive(listener).await
        else {
            panic!("expected one ordinary input");
        };
        assert_eq!(
            (
                runtime_id,
                actual_token,
                actual_sequence,
                data.as_slice(),
                paste
            ),
            (
                view.terminal.target.runtime_id,
                token,
                sequence,
                bytes,
                false
            )
        );
        respond(&mut peer, Response::Accepted {}).await;
    };
    let (result, ()) = tokio::join!(input, accept);
    result.unwrap();
}

async fn assert_no_fixture_request(listener: &UnixListener) {
    assert!(
        tokio::time::timeout(Duration::from_millis(25), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn w1_terminal_missing_grant_rejects_before_dispatch() {
    let fixture = AgentFixture::new();
    let listener = UnixListener::bind(fixture.0.join("agent.sock")).unwrap();
    let bridge = DesktopBridge::new(Some(fixture.0.clone()), "/unused/tmux".into());
    let (view, reader, _) = controlled_view(&bridge, &target(), Uuid::new_v4(), 0);
    let captured = Uuid::new_v4();
    let first = rejected_fixture_input(&bridge, &view, captured).await;
    let second = rejected_fixture_input(&bridge, &view, captured).await;
    assert_ne!(first, second);
    assert!(view.control.lock().await.lease.is_none());
    assert_no_fixture_request(&listener).await;

    let (token, _, current) = grant_fixture_writer(&listener, &bridge, &view).await;
    assert_ne!(current, captured);
    send_fixture_input(&listener, &bridge, &view, token, current, 0, b"fresh").await;
    bridge.detach(&view.terminal.view_id.to_string());
    release_fixture_writer(&listener, &view, token).await;
    reader.finish();
    until(|| allocation_counts(&bridge) == (0, 0)).await;
    assert_no_fixture_request(&listener).await;
}

#[derive(Clone, Copy)]
enum RenewalTiming {
    Live,
    ExpiredBeforeRequest,
    ExpiredWhileWaitingForReply,
}

#[tokio::test]
async fn w1_terminal_expired_input_recovers_through_accepted_renewal() {
    for timing in [
        RenewalTiming::ExpiredBeforeRequest,
        RenewalTiming::ExpiredWhileWaitingForReply,
        RenewalTiming::Live,
    ] {
        check_same_token_renewal(timing, false).await;
    }
}

#[tokio::test]
async fn w1_terminal_same_token_control_reply_fences_expired_incarnation() {
    check_same_token_renewal(RenewalTiming::ExpiredBeforeRequest, true).await;
    check_same_token_renewal(RenewalTiming::Live, true).await;
}

async fn check_same_token_renewal(timing: RenewalTiming, terminal_control_reply: bool) {
    use std::{future::Future, task::Poll};

    let fixture = AgentFixture::new();
    let listener = UnixListener::bind(fixture.0.join("agent.sock")).unwrap();
    let bridge = DesktopBridge::new(Some(fixture.0.clone()), "/unused/tmux".into());
    let (view, reader, _) = controlled_view(&bridge, &target(), Uuid::new_v4(), 0);
    let id = view.terminal.view_id.to_string();
    let (token, epoch, captured) = grant_fixture_writer(&listener, &bridge, &view).await;
    send_fixture_input(&listener, &bridge, &view, token, captured, 0, b"seed").await;
    let original_deadline = {
        let mut control = view.control.lock().await;
        let lease = control.lease.as_mut().unwrap();
        lease.resize_sequence = 7;
        lease.deadline = match timing {
            RenewalTiming::Live => lease.deadline,
            RenewalTiming::ExpiredBeforeRequest => Instant::now(),
            RenewalTiming::ExpiredWhileWaitingForReply => Instant::now() + Duration::from_secs(1),
        };
        let deadline = lease.deadline;
        view.publish_control(&control);
        deadline
    };
    let rejected_before = if matches!(timing, RenewalTiming::ExpiredBeforeRequest) {
        let rejection = rejected_fixture_input(&bridge, &view, captured).await;
        {
            let control = view.control.lock().await;
            let lease = control.lease.as_ref().unwrap();
            assert_eq!(
                (lease.token, lease.epoch, lease.incarnation),
                (token, epoch, captured)
            );
            assert_eq!((lease.input_sequence, lease.resize_sequence), (1, 7));
        }
        assert_no_fixture_request(&listener).await;
        Some(rejection)
    } else {
        None
    };

    let renewing = refresh_fixture_control(&view);
    let (
        mut peer,
        Request::Renew {
            runtime_id,
            token: renewing_token,
        },
    ) = receive(&listener).await
    else {
        panic!("expected renewal of the original lease");
    };
    assert_eq!(
        (runtime_id, renewing_token),
        (view.terminal.target.runtime_id, token)
    );
    match timing {
        RenewalTiming::ExpiredWhileWaitingForReply => {
            assert!(Instant::now() < original_deadline);
            tokio::time::sleep_until(original_deadline.into()).await;
        }
        RenewalTiming::Live => assert!(Instant::now() < original_deadline),
        RenewalTiming::ExpiredBeforeRequest => {}
    }
    let live = matches!(timing, RenewalTiming::Live);
    if !live {
        let expired = bridge.read(&id).await.unwrap();
        assert!(!expired.writable && expired.paste_incarnation.is_none());
    }
    let mut old_input = std::pin::pin!(rejected_fixture_input(&bridge, &view, captured));
    if matches!(timing, RenewalTiming::ExpiredWhileWaitingForReply) {
        std::future::poll_fn(|cx| {
            assert!(old_input.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
    }
    let reply = if terminal_control_reply {
        Response::TerminalControl { token, epoch }
    } else {
        Response::Accepted {}
    };
    respond(&mut peer, reply).await;
    renewing.await.unwrap();

    let published = bridge.read(&id).await.unwrap();
    assert!(published.writable);
    let current = published.paste_incarnation.unwrap();
    assert_eq!(current == captured, live);
    if !live {
        let rejected_again = old_input.await;
        assert_ne!(rejected_before, Some(rejected_again));
        assert_no_fixture_request(&listener).await;
    }
    {
        let control = view.control.lock().await;
        let lease = control.lease.as_ref().unwrap();
        assert_eq!(
            (lease.token, lease.epoch, lease.incarnation),
            (token, epoch, current)
        );
        assert_eq!((lease.input_sequence, lease.resize_sequence), (1, 7));
    }
    send_fixture_input(&listener, &bridge, &view, token, current, 1, b"fresh").await;
    bridge.detach(&id);
    release_fixture_writer(&listener, &view, token).await;
    reader.finish();
    until(|| allocation_counts(&bridge) == (0, 0)).await;
    assert_no_fixture_request(&listener).await;
}

#[tokio::test]
async fn w1_terminal_pending_paste_denies_ordinary_input_and_keeps_its_receipt() {
    let fixture = AgentFixture::new();
    let listener = UnixListener::bind(fixture.0.join("agent.sock")).unwrap();
    let bridge = DesktopBridge::new(Some(fixture.0.clone()), "/unused/tmux".into());
    let (view, reader, _) = controlled_view(&bridge, &target(), Uuid::new_v4(), 0);
    let id = view.terminal.view_id.to_string();
    let (token, epoch, incarnation) = grant_fixture_writer(&listener, &bridge, &view).await;
    let incarnation_id = incarnation.to_string();
    let operation = 1;
    let binding = PasteBinding {
        terminal: view.terminal.clone(),
        token,
        epoch,
        incarnation,
        operation,
        sequence: 0,
    };
    let staging = bridge.paste(
        &id,
        &incarnation_id,
        operation,
        PasteAction::Stage {
            offset: 0,
            data: vec![b'P'],
        },
    );
    let stage_reply = async {
        let (
            mut peer,
            Request::PasteStage {
                binding: actual,
                offset,
                data,
            },
        ) = receive(&listener).await
        else {
            panic!("expected scoped paste stage");
        };
        assert_eq!(actual, binding);
        assert_eq!((offset, data.as_slice()), (0, b"P".as_slice()));
        respond(
            &mut peer,
            Response::Paste {
                paste: PasteResult::Staged {},
            },
        )
        .await;
    };
    let (staged, ()) = tokio::join!(staging, stage_reply);
    assert_eq!(staged, PasteResult::Staged {});

    let receipt = Uuid::new_v4();
    let committing = bridge.paste(
        &id,
        &incarnation_id,
        operation,
        PasteAction::Commit { bytes: 1 },
    );
    let commit_reply = async {
        let (
            mut peer,
            Request::PasteCommit {
                binding: actual,
                bytes,
            },
        ) = receive(&listener).await
        else {
            panic!("expected scoped paste commit");
        };
        assert_eq!((actual, bytes), (binding.clone(), 1));
        respond(
            &mut peer,
            Response::Paste {
                paste: PasteResult::Committed { receipt },
            },
        )
        .await;
    };
    let (committed, ()) = tokio::join!(committing, commit_reply);
    assert_eq!(committed, PasteResult::Committed { receipt });

    let error = bridge
        .input(&id, b"blocked".to_vec(), &incarnation_id, false)
        .await
        .unwrap_err();
    let (rejection, observation) = terminal_rejection(error, None, true);
    assert_eq!(rejection, TerminalInputError::from("Керування втрачено"));
    assert!(observation.is_none());
    refresh_control(&view).await;
    {
        let control = view.control.lock().await;
        let lease = control.lease.as_ref().unwrap();
        let pending = lease.pending_paste.as_ref().unwrap();
        assert_eq!(lease.incarnation, incarnation);
        assert_eq!((lease.input_sequence, lease.resize_sequence), (1, 0));
        assert_eq!(pending.binding, binding);
        assert_eq!(pending.receipt, Some(receipt));
    }
    let pending = bridge.read(&id).await.unwrap();
    assert!(!pending.writable && pending.paste_incarnation.is_none());
    let completion = pending.paste_completion.unwrap();
    assert_eq!(
        (completion.incarnation, completion.operation),
        (incarnation, operation)
    );
    assert_eq!(
        bridge
            .paste(
                &id,
                &incarnation_id,
                operation,
                PasteAction::Ack {
                    receipt: Uuid::new_v4().to_string()
                }
            )
            .await,
        PasteResult::Stale {}
    );
    assert_no_fixture_request(&listener).await;

    let acknowledging = bridge.paste(
        &id,
        &incarnation_id,
        operation,
        PasteAction::Ack {
            receipt: receipt.to_string(),
        },
    );
    let ack_reply = async {
        let (
            mut peer,
            Request::PasteAck {
                binding: actual,
                receipt: actual_receipt,
            },
        ) = receive(&listener).await
        else {
            panic!("expected receipt-bound paste acknowledgement");
        };
        assert_eq!((actual, actual_receipt), (binding, receipt));
        respond(
            &mut peer,
            Response::Paste {
                paste: PasteResult::Acknowledged { writable: true },
            },
        )
        .await;
    };
    let (acknowledged, ()) = tokio::join!(acknowledging, ack_reply);
    assert_eq!(acknowledged, PasteResult::Acknowledged { writable: true });
    let current = bridge.read(&id).await.unwrap();
    assert!(current.writable && current.paste_completion.is_none());
    assert_eq!(current.paste_incarnation, Some(incarnation));
    send_fixture_input(&listener, &bridge, &view, token, incarnation, 1, b"fresh").await;
    bridge.detach(&id);
    release_fixture_writer(&listener, &view, token).await;
    reader.finish();
    until(|| allocation_counts(&bridge) == (0, 0)).await;
    assert_no_fixture_request(&listener).await;
}

#[tokio::test]
async fn w1_terminal_dispatched_input_with_lost_reply_stays_uncertain() {
    let fixture = AgentFixture::new();
    let listener = UnixListener::bind(fixture.0.join("agent.sock")).unwrap();
    let bridge = DesktopBridge::new(Some(fixture.0.clone()), "/unused/tmux".into());
    let (view, reader, _) = controlled_view(&bridge, &target(), Uuid::new_v4(), 0);
    let id = view.terminal.view_id.to_string();
    let (token, _, incarnation) = grant_fixture_writer(&listener, &bridge, &view).await;
    let incarnation = incarnation.to_string();
    let input = bridge.input(&id, b"unconfirmed".to_vec(), &incarnation, false);
    let lose_reply = async {
        let (peer, request) = receive(&listener).await;
        assert!(matches!(
            request,
            Request::Input { runtime_id, token: actual, sequence: 0, data, paste: false }
                if runtime_id == view.terminal.target.runtime_id && actual == token && data == b"unconfirmed"
        ));
        drop(peer);
        release_fixture_writer(&listener, &view, token).await;
    };
    let (result, ()) = tokio::join!(input, lose_reply);
    let (rejection, observation) = terminal_rejection(result.unwrap_err(), Some(true), true);
    assert_eq!(
        rejection,
        TerminalInputError::from("Input outcome unknown; input was not retried")
    );
    assert!(observation.is_none());
    assert!(!bridge.read(&id).await.unwrap().writable);
    assert!(view.control.lock().await.lease.is_none());
    bridge.detach(&id);
    reader.finish();
    until(|| allocation_counts(&bridge) == (0, 0)).await;
    assert_no_fixture_request(&listener).await;
}

#[tokio::test]
async fn w1_terminal_ordinary_input_native_capture() {
    use std::{future::Future, task::Poll};

    let fixture = AgentFixture::new();
    let listener = UnixListener::bind(fixture.0.join("agent.sock")).unwrap();
    let bridge = DesktopBridge::new(Some(fixture.0.clone()), "/unused/tmux".into());
    let (view, reader, _) = controlled_view(&bridge, &target(), Uuid::new_v4(), 0);
    let id = view.terminal.view_id.to_string();
    let first_epoch = view.output.lock().await.status.epoch.unwrap();
    let first_token = Uuid::new_v4();
    let acquiring = refresh_fixture_control(&view);
    let mut peer = acquire_fixture_writer(&listener, &view).await;
    respond(
        &mut peer,
        Response::TerminalControl {
            token: first_token,
            epoch: first_epoch,
        },
    )
    .await;
    acquiring.await.unwrap();
    let first = bridge.read(&id).await.unwrap();
    assert!(first.writable);
    let first_incarnation = first.paste_incarnation.unwrap().to_string();

    let renewing = refresh_fixture_control(&view);
    let (mut peer, Request::Renew { runtime_id, token }) = receive(&listener).await else {
        panic!("expected predecessor renewal");
    };
    assert_eq!(
        (runtime_id, token),
        (view.terminal.target.runtime_id, first_token)
    );
    respond(
        &mut peer,
        Response::Error {
            message: "expired".into(),
        },
    )
    .await;
    release_fixture_writer(&listener, &view, first_token).await;
    renewing.await.unwrap();

    let successor_token = Uuid::new_v4();
    let successor_epoch = ResizeEpoch {
        owner: Uuid::new_v4(),
        ..first_epoch
    };
    view.output.lock().await.status.epoch = Some(successor_epoch);
    let acquiring = refresh_fixture_control(&view);
    let mut peer = acquire_fixture_writer(&listener, &view).await;
    assert!(view.control.try_lock().is_err());
    let mut stale_text =
        std::pin::pin!(bridge.input(&id, b"old text".to_vec(), &first_incarnation, false));
    let mut stale_binary =
        std::pin::pin!(bridge.input(&id, vec![0x1b, 0xff], &first_incarnation, false));
    std::future::poll_fn(|cx| {
        assert!(stale_text.as_mut().poll(cx).is_pending());
        assert!(stale_binary.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    respond(
        &mut peer,
        Response::TerminalControl {
            token: successor_token,
            epoch: successor_epoch,
        },
    )
    .await;
    acquiring.await.unwrap();
    for stale in [stale_text, stale_binary] {
        let rejected = tokio::time::timeout(Duration::from_secs(1), stale)
            .await
            .unwrap();
        assert_eq!(rejected.unwrap_err().to_string(), "Керування змінилося");
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(25), listener.accept())
            .await
            .is_err()
    );
    let current = bridge.read(&id).await.unwrap();
    assert!(current.writable);
    let incarnation = current.paste_incarnation.unwrap().to_string();
    assert_ne!(incarnation, first_incarnation);
    {
        let control = view.control.lock().await;
        let lease = control.lease.as_ref().unwrap();
        assert_eq!(
            (lease.token, lease.epoch, lease.input_sequence),
            (successor_token, successor_epoch, 0)
        );
    }

    for (sequence, expected) in [(0, b"fresh text".as_slice()), (1, &[0x1b, 0x80, 0xff])] {
        let input = bridge.input(&id, expected.to_vec(), &incarnation, false);
        let accept = async {
            let (
                mut peer,
                Request::Input {
                    runtime_id,
                    token,
                    sequence: actual,
                    data,
                    paste,
                },
            ) = receive(&listener).await
            else {
                panic!("expected successor input");
            };
            assert_eq!(
                (runtime_id, token, actual, data.as_slice(), paste),
                (
                    view.terminal.target.runtime_id,
                    successor_token,
                    sequence,
                    expected,
                    false
                )
            );
            respond(&mut peer, Response::Accepted {}).await;
        };
        let (result, ()) = tokio::join!(input, accept);
        result.unwrap();
    }
    bridge.detach(&id);
    release_fixture_writer(&listener, &view, successor_token).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(25), listener.accept())
            .await
            .is_err()
    );
    reader.finish();
    until(|| allocation_counts(&bridge) == (0, 0)).await;
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

pub(super) struct ViewGate {
    attached: tokio::sync::oneshot::Sender<Uuid>,
    resume: tokio::sync::oneshot::Receiver<()>,
}

impl ViewGate {
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
    *bridge.startup_gate.lock().unwrap() = Some(ViewGate {
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
