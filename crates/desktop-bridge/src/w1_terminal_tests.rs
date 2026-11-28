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
                    self.epoch,
                    Uuid::new_v4(),
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
        let incarnation = view
            .control
            .lock()
            .await
            .lease
            .as_ref()
            .unwrap()
            .incarnation;
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
        let (result, ()) = tokio::join!(
            self.bridge.resize(&id, size, self.epoch, incarnation),
            reply
        );
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
                    original,
                    grant.paste_incarnation.unwrap(),
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

#[tokio::test]
async fn w1_terminal_candidate_drains_eof_during_confirmation() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let fixture = WriterCandidateFixture::new();
        let original = fixture.epoch;
        fs::write(fixture._directory.0.join("hold"), "").unwrap();
        let candidate = fixture.prepare(AttachmentMode::Candidate).await;
        let view = fixture.bridge.view(&candidate.view_id).unwrap();
        assert_eq!(view.terminal, candidate.status.terminal);
        view.output.lock().await.refresh_at = Instant::now() + Duration::from_secs(60);
        assert_eq!(
            read_until_bytes(
                &fixture.bridge,
                &candidate.view_id,
                b"buffered attachment output"
            )
            .await,
            b"buffered attachment output"
        );

        let confirmation = fixture.confirming(candidate.status.terminal.clone());
        assert!(!fixture.row.exited);
        fixture
            .reply_inventory(fixture.expected.agent_pid, fixture.row.clone())
            .await;
        let validation = attachment_status_peer(&fixture.listener, &candidate.status).await;
        assert!(!confirmation.is_finished());
        fs::write(fixture._directory.0.join("continue"), "").unwrap();
        tokio::time::timeout(Duration::from_secs(2), view.retired())
            .await
            .unwrap();
        assert!(view.eof_draining());
        {
            let views = fixture.bridge.views.lock().unwrap();
            let id = view.terminal.view_id;
            assert!(!views.live.contains_key(&id));
            let drain = &views.drains[&id];
            assert!(Arc::ptr_eq(&drain.view, &view));
            let now = Instant::now();
            assert!(drain.expires > now && drain.expires <= now + EOF_TTL);
        }
        assert_eq!(
            confirmation.await.unwrap().unwrap_err().to_string(),
            "Terminal candidate is no longer current"
        );
        drop(validation);

        assert!(
            fixture
                .bridge
                .input(
                    &candidate.view_id,
                    vec![65],
                    &Uuid::new_v4().to_string(),
                    false
                )
                .await
                .is_err()
        );
        assert!(
            fixture
                .bridge
                .resize(
                    &candidate.view_id,
                    candidate.status.committed.size,
                    original,
                    Uuid::new_v4(),
                )
                .await
                .is_err()
        );
        fixture.no_effects(original).await;
        let mut exited = fixture.row.clone();
        exited.exited = true;
        exited.pane_pid = None;
        let (inventory, ()) = tokio::join!(
            fixture.bridge.inventory(),
            fixture.reply_inventory(fixture.expected.agent_pid, exited)
        );
        let inventory = inventory.unwrap();
        assert_eq!(inventory.agent_pid, fixture.expected.agent_pid);
        assert_eq!(inventory.runtimes.len(), 1);
        assert!(inventory.runtimes[0].exited);
        assert_eq!(inventory.runtimes[0].pane_pid, None);
        assert_eq!(inventory.runtimes[0].runtime_id, fixture.target.runtime_id);

        let mut data = Vec::new();
        let mut frames = 0;
        loop {
            let chunk = fixture.bridge.read(&candidate.view_id).await.unwrap();
            assert_read_only_drain(&chunk);
            if chunk.closed {
                assert_eq!(chunk.closure, Some(TerminalClosure::Eof));
                assert!(chunk.data.is_empty());
                break;
            }
            frames += 1;
            assert!(frames <= OUTPUT_SLOTS && !chunk.data.is_empty());
            assert!(chunk.closure.is_none() && chunk.data.len() <= OUTPUT_CHUNK_SIZE);
            data.extend(chunk.data);
        }
        assert!(frames > 1);
        assert_eq!(
            data,
            [
                b"first diagnostic".as_slice(),
                &[0; OUTPUT_CHUNK_SIZE],
                b"last diagnostic".as_slice(),
            ]
            .concat()
        );
        assert!(fixture.bridge.read(&candidate.view_id).await.is_err());
        assert_eq!(allocation_counts(&fixture.bridge), (0, 0));
        fixture.no_effects(original).await;
        fixture.finish().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn w1_terminal_candidate_inventory_mismatch_leaves_queued_readable_bytes() {
    tokio::time::timeout(Duration::from_secs(8), async {
        let fixture = WriterCandidateFixture::new();
        let original = fixture.epoch;
        fs::write(fixture._directory.0.join("hold"), "").unwrap();
        let candidate = fixture.prepare(AttachmentMode::Candidate).await;
        let view = fixture.bridge.view(&candidate.view_id).unwrap();
        let read_clock = view.output.lock().await.refresh_at - Duration::from_millis(1);
        let confirmation = fixture.confirming(candidate.status.terminal.clone());
        let mut conflicting = fixture.row.clone();
        conflicting.operation_id = Some(Uuid::new_v4());
        fixture
            .reply_inventory(fixture.expected.agent_pid, conflicting)
            .await;
        assert_eq!(
            confirmation.await.unwrap().unwrap_err().to_string(),
            "Terminal inventory binding changed"
        );
        {
            let views = fixture.bridge.views.lock().unwrap();
            assert!(Arc::ptr_eq(
                &views.live[&candidate.status.terminal.view_id],
                &view
            ));
            assert!(views.drains.is_empty());
        }
        let expected = b"buffered attachment output";
        let mut data = Vec::new();
        while data.len() < expected.len() {
            let chunk = fixture
                .bridge
                .read_with_clock(&candidate.view_id, || read_clock)
                .await
                .unwrap();
            assert_read_only_drain(&chunk);
            assert!(!chunk.closed && chunk.closure.is_none());
            data.extend(chunk.data);
        }
        assert_eq!(data, expected);
        fixture.no_effects(original).await;
        fixture.bridge.detach(&candidate.view_id);
        fixture.finish().await;
    })
    .await
    .unwrap();
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
        let incarnation = self
            .owner
            .control
            .lock()
            .await
            .lease
            .as_ref()
            .unwrap()
            .incarnation;
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
        let (reply, ()) = tokio::join!(self.bridge.resize(&owner, size, epoch, incarnation), agent);
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

    async fn handoff_to_view(&mut self) -> (Uuid, Uuid) {
        self.bridge.detach(&self.owner.terminal.view_id.to_string());
        release_fixture_writer(&self.listener, &self.owner, self.token).await;
        let epoch = ResizeEpoch {
            owner: Uuid::new_v4(),
            ..self.status.epoch.unwrap()
        };
        let token = Uuid::new_v4();
        let grant = async {
            let mut peer = acquire_fixture_writer(&self.listener, &self.view).await;
            respond(&mut peer, Response::TerminalControl { token, epoch }).await;
        };
        tokio::join!(refresh_control(&self.view), grant);
        self.status.epoch = Some(epoch);
        let incarnation = self
            .view
            .control
            .lock()
            .await
            .lease
            .as_ref()
            .unwrap()
            .incarnation;
        (token, incarnation)
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
async fn w1_resize_settles_original_reply_across_same_view_token_handoff() {
    for grid_first in [false, true] {
        let mut fixture = GeometryFixture::new().await;
        let key = fixture.key();
        fixture
            .bridge
            .detach(&fixture.owner.terminal.view_id.to_string());
        release_fixture_writer(&fixture.listener, &fixture.owner, fixture.token).await;
        let (token, epoch, capture) =
            grant_fixture_writer(&fixture.listener, &fixture.bridge, &fixture.view).await;
        let mut old = std::pin::pin!(fixture.bridge.resize(&key, GRID_A, epoch, capture));
        let (mut peer, request) = tokio::select! {
            result = old.as_mut() => panic!("old resize completed before its reply: {result:?}"),
            request = receive(&fixture.listener) => request,
        };
        assert!(
            matches!(request, Request::ResizeTerminal { terminal, token: sent, sequence: 0, size: GRID_A }
            if terminal == fixture.view.terminal && sent == token)
        );
        let admission = ResizeIntent {
            epoch,
            id: Uuid::new_v4(),
        };
        fixture.status.epoch = Some(epoch);
        fixture.status.committed = CommittedGeometry {
            revision: 1,
            size: GRID_A,
            intent: Some(admission),
        };
        let historical = ResizeResult::Committed {
            admission,
            status: fixture.status.clone(),
        };

        let refreshing = refresh_fixture_control(&fixture.view);
        let (mut renewal, request) = receive(&fixture.listener).await;
        assert!(matches!(request, Request::Renew { runtime_id, token: sent }
            if runtime_id == fixture.view.terminal.target.runtime_id && sent == token));
        respond(
            &mut renewal,
            Response::Error {
                message: "expired original writer".into(),
            },
        )
        .await;
        release_fixture_writer(&fixture.listener, &fixture.view, token).await;
        refreshing.await.unwrap();
        assert!(fixture.view.control.lock().await.lease.is_none());
        let successor = Uuid::new_v4();
        assert_ne!(successor, token);
        let next_epoch = ResizeEpoch {
            owner: Uuid::new_v4(),
            ..epoch
        };
        let refreshing = refresh_fixture_control(&fixture.view);
        let mut acquiring = acquire_fixture_writer(&fixture.listener, &fixture.view).await;
        respond(
            &mut acquiring,
            Response::TerminalControl {
                token: successor,
                epoch: next_epoch,
            },
        )
        .await;
        refreshing.await.unwrap();
        fixture.status.epoch = Some(next_epoch);

        let sample = async || {
            let control = fixture.view.control.lock().await;
            let lease = control.lease.as_ref().unwrap();
            assert_eq!((lease.token, lease.epoch), (successor, next_epoch));
            assert_eq!(
                (
                    lease.input_sequence,
                    lease.resize_sequence,
                    lease.paste_high_water
                ),
                (0, 0, 0)
            );
            assert!(lease.pending_paste.is_none() && lease.staged_paste.is_none());
            assert!(lease.writable(Instant::now()));
            (
                lease.deadline,
                lease.incarnation,
                lease.paste_wire_incarnation,
            )
        };
        let before = sample().await;
        assert_ne!(before.1, capture);
        let event_before = if grid_first {
            Some(fixture.issue_first().await)
        } else {
            None
        };
        let drain = fixture.view.output.lock().await.drain_started;
        assert_eq!(drain, grid_first.then_some(fixture.start));
        assert_eq!(fixture.view.output.lock().await.applied_revision, 0);
        respond(
            &mut peer,
            Response::Resize {
                resize: historical.clone(),
            },
        )
        .await;
        assert_eq!(old.await.unwrap(), historical);
        assert_eq!(sample().await, before);
        assert_eq!(fixture.view.output.lock().await.drain_started, drain);
        assert_eq!(fixture.view.output.lock().await.applied_revision, 0);
        assert_eq!(fixture.redraws(), 0);

        let event = match event_before {
            Some(event) => event,
            None => fixture.issue_first().await,
        };
        assert_eq!(event.status.epoch, Some(next_epoch));
        assert_eq!(event.status.committed.intent, Some(admission));
        assert_eq!(event.status.committed.size, GRID_A);
        let ack = fixture.acknowledgement(&event);
        let (reply, ()) = tokio::join!(
            fixture
                .bridge
                .acknowledge_grid_with_clock(&key, ack, || fixture.start + DRAIN_TIMEOUT / 2),
            fixture.reply_status(fixture.status.clone()),
        );
        assert_eq!(reply.unwrap().status, fixture.status);
        assert_eq!(fixture.view.output.lock().await.applied_revision, 1);
        assert_eq!(fixture.redraws(), 1);
        assert_eq!(sample().await, before);
        fixture.bridge.detach(&key);
        release_fixture_writer(&fixture.listener, &fixture.view, successor).await;
        fixture.writer.finish();
        fixture._owner_reader.finish();
        until(|| allocation_counts(&fixture.bridge) == (0, 0)).await;
    }
}

#[tokio::test]
async fn w1_resize_native_and_host_busy_keep_the_sequence_for_fresh_scope() {
    let fixture = AgentFixture::new();
    let listener = UnixListener::bind(fixture.0.join("agent.sock")).unwrap();
    let bridge = DesktopBridge::new(Some(fixture.0.clone()), "/unused/tmux".into());
    let (view, reader, _) = controlled_view(&bridge, &target(), Uuid::new_v4(), 0);
    let id = view.terminal.view_id.to_string();
    let (token, epoch, incarnation) = grant_fixture_writer(&listener, &bridge, &view).await;
    let mut old = std::pin::pin!(bridge.resize(&id, GRID_A, epoch, incarnation));
    let (mut peer, request) = tokio::select! {
        result = old.as_mut() => panic!("first resize completed before IPC: {result:?}"),
        request = receive(&listener) => request,
    };
    assert!(
        matches!(request, Request::ResizeTerminal { terminal, token: sent, sequence: 0, size: GRID_A }
        if terminal == view.terminal && sent == token)
    );
    assert_eq!(
        bridge
            .resize(&id, GRID_B, epoch, incarnation)
            .await
            .unwrap(),
        ResizeResult::Busy {
            terminal: view.terminal.clone(),
            size: GRID_B
        },
    );
    let refused = ResizeResult::Busy {
        terminal: view.terminal.clone(),
        size: GRID_A,
    };
    respond(
        &mut peer,
        Response::Resize {
            resize: refused.clone(),
        },
    )
    .await;
    assert_eq!(old.await.unwrap(), refused);
    assert_no_fixture_request(&listener).await;

    let admission = ResizeIntent {
        epoch,
        id: Uuid::new_v4(),
    };
    let mut status = view.output.lock().await.status.clone();
    status.committed = CommittedGeometry {
        size: GRID_B,
        revision: 1,
        intent: Some(admission),
    };
    let committed = ResizeResult::Committed { admission, status };
    let reply = async {
        let (mut peer, request) = receive(&listener).await;
        assert!(
            matches!(request, Request::ResizeTerminal { terminal, token: sent, sequence: 0, size: GRID_B }
            if terminal == view.terminal && sent == token)
        );
        respond(
            &mut peer,
            Response::Resize {
                resize: committed.clone(),
            },
        )
        .await;
    };
    let (result, ()) = tokio::join!(bridge.resize(&id, GRID_B, epoch, incarnation), reply);
    assert_eq!(result.unwrap(), committed);
    let control = view.control.lock().await;
    let lease = control.lease.as_ref().unwrap();
    assert_eq!(
        (
            lease.token,
            lease.epoch,
            lease.incarnation,
            lease.resize_sequence
        ),
        (token, epoch, incarnation, 1)
    );
    drop(control);
    assert_no_fixture_request(&listener).await;
    bridge.detach(&id);
    release_fixture_writer(&listener, &view, token).await;
    reader.finish();
    until(|| allocation_counts(&bridge) == (0, 0)).await;
}

#[tokio::test]
async fn w1_resize_cannot_settle_a_retired_source_with_a_live_replacement() {
    let fixture = AgentFixture::new();
    let listener = UnixListener::bind(fixture.0.join("agent.sock")).unwrap();
    let bridge = DesktopBridge::new(Some(fixture.0.clone()), "/unused/tmux".into());
    let agent = Uuid::new_v4();
    let (view, reader, _) = controlled_view(&bridge, &target(), agent, 0);
    let id = view.terminal.view_id.to_string();
    let (token, epoch, capture) = grant_fixture_writer(&listener, &bridge, &view).await;
    let mut status = view.output.lock().await.status.clone();
    let admission = ResizeIntent {
        epoch,
        id: Uuid::new_v4(),
    };
    status.committed = CommittedGeometry {
        size: GRID_A,
        revision: 1,
        intent: Some(admission),
    };
    let mut old = std::pin::pin!(bridge.resize(&id, GRID_A, epoch, capture));
    let (mut peer, request) = tokio::select! {
        result = old.as_mut() => panic!("old resize completed before IPC: {result:?}"),
        request = receive(&listener) => request,
    };
    assert!(
        matches!(request, Request::ResizeTerminal { terminal, token: sent, sequence: 0, size: GRID_A }
        if terminal == view.terminal && sent == token)
    );

    bridge.detach(&id);
    release_fixture_writer(&listener, &view, token).await;
    let replacement = TerminalTarget {
        pane: "%9".into(),
        ..view.terminal.target.clone()
    };
    let (fresh, fresh_reader, _) = controlled_view(&bridge, &replacement, agent, 0);
    let (successor, fresh_epoch, fresh_capture) =
        grant_fixture_writer(&listener, &bridge, &fresh).await;
    let fresh_id = fresh.terminal.view_id.to_string();
    assert_ne!(fresh_id, id);
    assert_ne!(fresh_epoch, epoch);
    let original = fresh.output.lock().await.status.clone();
    respond(
        &mut peer,
        Response::Resize {
            resize: ResizeResult::Committed { admission, status },
        },
    )
    .await;
    assert_eq!(old.await.unwrap_err().to_string(), "View detached");
    assert_eq!(fresh.output.lock().await.status, original);
    let control = fresh.control.lock().await;
    let lease = control.lease.as_ref().unwrap();
    assert_eq!(
        (
            lease.token,
            lease.epoch,
            lease.incarnation,
            lease.resize_sequence
        ),
        (successor, fresh_epoch, fresh_capture, 0)
    );
    drop(control);
    reader.finish();
    bridge.detach(&fresh_id);
    release_fixture_writer(&listener, &fresh, successor).await;
    fresh_reader.finish();
    until(|| allocation_counts(&bridge) == (0, 0)).await;
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
async fn w1_resize_handoff_orders_issued_ack_and_queued_successor_before_writable() {
    for queued in [false, true] {
        let mut fixture = GeometryFixture::new().await;
        let key = fixture.key();
        fixture.commit(GRID_A).await;
        let mut event = fixture.issue_first().await;
        if queued {
            fixture.commit(GRID_B).await;
            let (reply, ()) = tokio::join!(
                fixture
                    .bridge
                    .acknowledge_grid(&key, fixture.acknowledgement(&event)),
                fixture.reply_status(fixture.status.clone()),
            );
            assert_eq!(reply.unwrap().status, event.status);
            fixture.assert_paused(1).await;
        }
        let predecessor = fixture.status.clone();
        let (token, incarnation) = fixture.handoff_to_view().await;
        assert_ne!(fixture.status.epoch, predecessor.epoch);
        assert_eq!(fixture.status.committed, predecessor.committed);
        assert_eq!(fixture.view.output.lock().await.status, event.status);
        assert_eq!(fixture.redraws(), 0);
        fixture.writer.data(b"after handoff");
        if queued {
            event = fixture.read_successor().await;
            assert_eq!(event.status.committed, predecessor.committed);
            assert_eq!(event.status.epoch, fixture.status.epoch);
        }
        let ack = fixture.acknowledgement(&event);
        let wrong_epoch = if queued {
            predecessor.epoch
        } else {
            fixture.status.epoch
        };
        let wrong = GridAcknowledgement {
            epoch: wrong_epoch,
            ..ack
        };
        assert_eq!(
            fixture
                .bridge
                .acknowledge_grid(&key, wrong)
                .await
                .unwrap_err()
                .to_string(),
            "Terminal renderer acknowledgement is stale or mismatched"
        );
        fixture.assert_paused(ack.revision - 1).await;
        assert_eq!(fixture.redraws(), 0);
        let (reply, ()) = tokio::join!(
            fixture.bridge.acknowledge_grid(&key, ack),
            fixture.reply_status(fixture.status.clone()),
        );
        let reply = reply.unwrap();
        assert_eq!(reply.status, event.status);
        assert_eq!(
            (reply.bridge_pty.size, reply.client.size),
            (ack.size, ack.size)
        );
        assert_eq!(fixture.view.reader_state.load(Ordering::Acquire), RESUME);
        assert_eq!(fixture.redraws(), 1);
        if !queued {
            let authority = fixture.bridge.read(&key).await.unwrap();
            assert_eq!(authority.status.as_ref(), Some(&fixture.status));
            assert!(authority.geometry.is_none() && authority.data.is_empty());
            assert!(
                !authority.closed && !authority.writable && authority.paste_incarnation.is_none()
            );
        }
        let data = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let chunk = fixture.bridge.read(&key).await.unwrap();
                assert!(!chunk.closed && chunk.geometry.is_none() && chunk.status.is_none());
                assert!(chunk.writable);
                assert_eq!(chunk.paste_incarnation, Some(incarnation));
                if !chunk.data.is_empty() {
                    break chunk.data;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(data, b"after handoff");
        let output = fixture.view.output.lock().await;
        assert_eq!(output.status, fixture.status);
        assert_eq!(output.applied_revision, ack.revision);
        assert!(output.drain_started.is_none());
        drop(output);
        fixture.bridge.detach(&key);
        release_fixture_writer(&fixture.listener, &fixture.view, token).await;
        fixture.writer.finish();
    }
}

#[tokio::test]
async fn w1_resize_handoff_rejects_replacement_and_inconsistent_equal_commit() {
    for queued in [false, true] {
        for mismatch in ["target", "size", "intent", "intent epoch", "revision"] {
            let mut fixture = GeometryFixture::new().await;
            let key = fixture.key();
            fixture.commit(GRID_A).await;
            let event = fixture.issue_first().await;
            let ack = fixture.acknowledgement(&event);
            if queued {
                fixture.commit(GRID_B).await;
                let (reply, ()) = tokio::join!(
                    fixture.bridge.acknowledge_grid(&key, ack),
                    fixture.reply_status(fixture.status.clone()),
                );
                assert_eq!(reply.unwrap().status, event.status);
            }
            let mut changed = fixture.status.clone();
            changed.epoch.as_mut().unwrap().owner = Uuid::new_v4();
            match mismatch {
                "target" => changed.terminal.target.process_start += 1,
                "size" => {
                    changed.committed.size.cols += 1;
                    changed.provider.as_mut().unwrap().size = changed.committed.size;
                }
                "intent" => changed.committed.intent.as_mut().unwrap().id = Uuid::new_v4(),
                "intent epoch" => {
                    changed.committed.intent.as_mut().unwrap().epoch.owner = Uuid::new_v4()
                }
                "revision" => {
                    changed.committed.revision -= 1;
                    if changed.committed.revision == 0 {
                        changed.committed.intent = None;
                    }
                }
                _ => unreachable!(),
            }
            fixture.writer.data(b"must remain paused");
            fixture.assert_paused(if queued { 1 } else { 0 }).await;
            let reject = async {
                if queued {
                    fixture
                        .bridge
                        .read(&key)
                        .await
                        .err()
                        .expect("queued successor must fail")
                } else {
                    fixture
                        .bridge
                        .acknowledge_grid(&key, ack)
                        .await
                        .unwrap_err()
                }
            };
            let (error, ()) = tokio::join!(reject, fixture.reply_status(changed));
            assert_eq!(
                error.to_string(),
                if mismatch == "target" {
                    "Terminal attachment identity changed"
                } else {
                    "Terminal geometry boundary was superseded"
                }
            );
            assert_eq!(fixture.view.reader_state.load(Ordering::Acquire), CLOSE);
            assert_eq!(fixture.redraws(), 0);
            assert!(fixture.bridge.read(&key).await.is_err());
        }
    }
}

#[tokio::test]
async fn w1_resize_w1_terminal_original_drain_deadline() {
    let tick = Duration::from_nanos(1);
    for (elapsed, resumes, superseded, handoff) in [
        (DRAIN_TIMEOUT - tick, true, false, false),
        (DRAIN_TIMEOUT, false, false, false),
        (DRAIN_TIMEOUT + tick, false, false, false),
        (DRAIN_TIMEOUT, false, true, false),
        (DRAIN_TIMEOUT - tick, true, false, true),
        (DRAIN_TIMEOUT, false, false, true),
        (DRAIN_TIMEOUT - tick, true, true, true),
        (DRAIN_TIMEOUT, false, true, true),
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
        }
        if handoff {
            fixture.status.epoch.as_mut().unwrap().owner = Uuid::new_v4();
        }
        if superseded {
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

#[cfg(debug_assertions)]
const STATUS_HOST_TMUX: &str = r##"#!/bin/sh
set -eu
cd "$(dirname "$3")"
token=$(cat generation)
pid=$(cat pane-pid)
birth_format='#{pid}|#{start_time}|#{@rs_create_generation}|#{session_id}|#{window_id}|#{pane_id}|#{pane_pid}|#{session_name}|#{window_index}|#{pane_index}|#{@rs_create_nonce}|#{pane_dead}|#{pane_tty}|#{@rs_create_initializer}|#{@rs_create_birth}'
sender=0
if test -f paste-tmux; then
  test "$1" = -N && test "$2" = -S && test "$3" = "$PWD/tmux.sock" && test -S "$3"
  sender=1
fi
read_option() {
  if test -f "$1"; then cat "$1"; fi
}
case "$4" in
  display-message)
    if test "$#" = 8 && test "$5" = -p && test "$6" = -t; then
      test "$7" = '%0'
      name=$(cat active)
      device=$(cat tty)
      case "$8" in
        "$birth_format")
          if test -s samples; then name=$(tail -n 1 samples); fi
          nonce=$(cat "$name.nonce")
          printf '52|101|%s|$0|@0|%%0|%s|%s|0|0|%s:$0:@0:%%0:%s|0|%s|%s|%s\n' \
            "$token" "$pid" "$name" "$nonce" "$pid" "$device" \
            "$(read_option "$name.initializer")" "$(read_option "$name.birth")"
          exit 0 ;;
        '#{@rs_create_initializer}|#{@rs_create_birth}')
          printf '%s|%s\n' "$(read_option "$name.initializer")" "$(read_option "$name.birth")"
          exit 0 ;;
        *'#{session_windows}|#{window_panes}|#{window-size}|#{pane_tty}|'*) ;;
        *) exit 1 ;;
      esac
      if test -f samples; then
        sample=$(awk 'END {print NR + 1}' samples)
        name=$(cat "sample-$sample")
        printf '%s\n' "$name" >> samples
        if test -f fail-query; then exit 1; fi
        if test -f missing-device && test "$sample" = 1; then device=/dev/pts/999999999; fi
      fi
      nonce=$(cat "$name.nonce")
      launch=$(read_option launch)
      case "$launch" in ??????????????8*) launch="$launch~$(read_option "$name.birth")" ;; esac
      printf '52|101|%s|$0|@0|%%0|%s|%s|0|0|%s:$0:@0:%%0:%s|0|1|1|manual|%s|100|30|100|30|%s||%s|%s|0|%s\n' \
        "$token" "$pid" "$name" "$nonce" "$pid" "$device" \
        "$(read_option epoch)" "$launch" "$(read_option marker)" "$sender"
    else
      printf '52|101|%s\n' "$token"
      if test "$#" -gt 6 && test -f names; then cat names; fi
    fi ;;
  if-shell)
    case "${7-}" in
      'new-session '*)
        if test "$sender" = 1; then exec ./paste-tmux "$@"; fi
        exec ./spawn-tmux "$@" ;;
      'show-options -sq -F '*)
        test "$sender" = 1
        exec ./paste-tmux "$@" ;;
      *)
        if test "$sender" = 1; then exec ./paste-tmux "$@"; fi
        printf '%s\n' "$*" >> unexpected; exit 1 ;;
    esac ;;
  list-commands|save-buffer)
    test "$sender" = 1
    exec ./paste-tmux "$@" ;;
  show-options)
    test "$#" -eq 14 && test "$5 $6 $7 $8" = "-pq -t %0 @rs_create_initializer"
    test "${9} ${10} ${11} ${12} ${13} ${14}" = "; show-options -pq -t %0 @rs_create_birth"
    name=$(cat active)
    if test -f "$name.initializer"; then printf 'present\n'; fi
    if test -f "$name.birth"; then printf 'present\n'; fi ;;
  show-environment)
    name=${6#=}
    test "$#" = 14 && test "$7" = RS_CREATE_NONCE
    nonce=$(cat "$name.nonce")
    printf 'RS_CREATE_NONCE=%s\n52|101|%s|$0|@0|%%0|%s|%s|0|0|%s:$0:@0:%%0:%s|0|100|30\n' \
      "$nonce" "$token" "$pid" "$name" "$nonce" "$pid" ;;
  list-sessions)
    if test -f names; then cat names; fi ;;
  list-panes)
    while IFS= read -r name; do
      printf '%s\t%%0\t%s\t0\t100\t30\tcodex' "$name" "$pid"
      case "$7" in
        *'#{pid}|#{start_time}|#{?#{m/r:^[0-9a-f-]*$,#{@rs_create_generation}},#{@rs_create_generation},invalid}|#{session_id}|#{window_id}'*)
          printf '\t52|101|%s|$0|@0' "$token" ;;
      esac
      printf '\n'
    done < names ;;
  *) printf '%s\n' "$*" >> unexpected; exit 1 ;;
esac
"##;

#[cfg(debug_assertions)]
const PASTE_HOST_MUTATIONS: &str = r##"#!/usr/bin/env python3
from pathlib import Path
import json
import re
import shlex
import sys
import uuid

def read(name):
    path = Path(name)
    return path.read_text().strip() if path.exists() else ""

def buffer_path(name):
    assert re.fullmatch(r"rs-paste-(?:probe-)?[0-9a-f]{32}", name), name
    return Path(name)

def allows(guard, actual):
    guard = re.sub(r"#\{==:#\{([^}]+)\},([^}]*)\}",
                   lambda match: str(int(actual[match[1]] == match[2])), guard)
    while "#{" in guard:
        reduced = re.sub(r"#\{&&:([01]),([01])\}",
                         lambda match: str(int(match[1] == match[2] == "1")), guard)
        assert reduced != guard, guard
        guard = reduced
    return guard == "1"

def write(name, value):
    Path(name).write_text(value)

def custody_options():
    token = uuid.UUID(read("generation")).hex
    return [f"@rs_paste_custody_52_101_{token}_{i:02}" for i in range(16)]

def custody_path(option):
    assert option in custody_options()
    return Path("custody") / option

def custody_value(value):
    assert len(value) <= 2048 and re.fullmatch(r"[a-zA-Z0-9:/_~-]+", value)
    version, mutation, state, *fields = value.split("~")
    assert version in ("v1", "v2") and str(uuid.UUID(mutation)) == mutation
    assert uuid.UUID(mutation).version == 4
    if state == "free":
        assert version == "v1" and not fields
    elif version == "v2":
        assert state in ("possible", "completed") and len(fields) == 17
        assert fields[0] == "probe" and fields[2:5] == [read("generation"), "52", "101"]
        assert fields[-1] == "rs-paste-probe-" + uuid.UUID(fields[-2]).hex
    else:
        assert state in ("possible", "completed") and len(fields) == 19
        assert fields[2] == "v1" and fields[4:7] == [read("generation"), "52", "101"]
        assert fields[-4] == "l" and re.fullmatch(r"rs-paste-[0-9a-f]{32}", fields[-1])
    return mutation, state, fields

command, *args = sys.argv[4:]
if command == "list-commands":
    assert not args
    print("load-buffer (loadb) [-w] [-b buffer-name] path")
    print("paste-buffer (pasteb) [-dprS] [-b buffer-name] [-t target-pane]")
elif command == "save-buffer":
    flag, buffer, output = args
    assert (flag, output) == ("-b", "-") and buffer.startswith("rs-paste-probe-")
    sys.stdout.buffer.write(buffer_path(buffer).read_bytes())
elif command == "if-shell" and args[0] == "-F" and args[1] != "-t":
    flag, guard, branch = args
    token, pid = read("generation"), read("pane-pid")
    actual = {"pid": "52", "start_time": "101", "@rs_create_generation": token, "command-error": ""}
    if branch.startswith("show-options "):
        actual.update({"after-if-shell": "", "after-show-options": "", "after-display-message": ""})
        if not allows(guard, actual):
            sys.exit()
        nodes = [shlex.split(node) for node in branch.split(" ; ")]
        assert len(nodes) == 17 and nodes[-1] == ["display-message", "-p", "-l", "rs-custody-snapshot"]
        row = "#{option_name} #{?#{m/r:^[a-zA-Z0-9:/_~-]+$,#{option_value}},#{option_value},invalid}"
        for words, option in zip(nodes[:-1], custody_options()):
            assert words == ["show-options", "-sq", "-F", row, option]
            path = custody_path(option)
            if path.exists():
                value = path.read_text()
                print(option, value if re.fullmatch(r"[a-zA-Z0-9:/_~-]+", value) else "invalid")
        print("rs-custody-snapshot")
        sys.exit()
    for hook in ("after-if-shell", "after-new-session", "session-created", "session-window-changed",
                 "window-linked", "window-layout-changed", "after-set-environment", "after-set-option"):
        actual[hook] = ""
    assert allows(guard, actual)
    spawn, mask, initialize, stamp, label = branch.split(" ; ")
    name = re.search(r" -s (rs_[0-9a-f]{32}_[0-9a-f]{32}) ", spawn)[1]
    nonce, ticket = re.search(r" -e RS_CREATE_NONCE=(\S+) -e RS_CREATE_TICKET=(\S+) ", spawn).groups()
    assert str(uuid.UUID(ticket)) == ticket and uuid.UUID(ticket).version == 4
    capsule = json.loads(bytes.fromhex(spawn.rsplit(" __managed-launch ", 1)[1]))
    birth = capsule["birth"]
    assert capsule["nonce"] == nonce and birth["ticket"] == ticket
    assert Path(capsule["service_state"]) == Path.cwd() and Path(birth["tmux"]) == Path.cwd() / "tmux"
    assert birth["server"] == {"pid": 52, "start": 101, "token": token}
    assert name == "rs_" + uuid.UUID(birth["session"]).hex + "_" + uuid.UUID(birth["runtime"]).hex
    prefix = f"v1~52~101~{token}~{birth['operation']}~{birth['runtime']}~{nonce}~{ticket}~"
    assert mask == f"set-environment -r -t ={name} RS_CREATE_TICKET"
    assert initialize == (f"set-option -Fpo -t ={name}:0.0 @rs_create_initializer '{prefix}"
                          "#{session_id}~#{window_id}~#{pane_id}~#{pane_pid}'")
    assert stamp == (f"set-option -Fpo -t ={name}:0.0 @rs_create_nonce '{nonce}:"
                     "#{session_id}:#{window_id}:#{pane_id}:#{pane_pid}'")
    assert label == f"set-option -p -t ={name}:0.0 @rs_command codex"
    assert not Path(name + ".nonce").exists() and not Path(name + ".birth").exists()
    session, window, pane = read("pane-scope").split("~")
    init = prefix + f"{session}~{window}~{pane}~{pid}"
    tty = read("tty")
    device = Path(tty).stat()
    witness = f"{init}~{read('pane-start')}~{tty}~{device.st_dev}~{device.st_ino}~{device.st_rdev}"
    write(name + ".initializer", init)
    write(name + ".nonce", nonce)
    write(name + ".birth", witness)
    write("active", name)
    with Path("names").open("a") as names:
        names.write(name + "\n")
    print(f"{session}|{window}|{pane}|{pid}")
else:
    assert command == "if-shell"
    flag, target, pane, guard, branch, *otherwise = args
    assert (flag, target, pane) == ("-F", "-t", "%0")
    pid, name = read("pane-pid"), read("active")
    nonce = read(name + ".nonce")
    actual = {
        "pid": "52", "start_time": "101", "@rs_create_generation": read("generation"),
        "session_name": name, "session_id": "$0", "window_id": "@0", "pane_id": pane,
        "window_index": "0", "pane_index": "0", "pane_pid": pid, "pane_tty": read("tty"),
        "@rs_create_nonce": f"{nonce}:$0:@0:%0:{pid}", "@rs_diagnostic_create": "", "session_windows": "1",
        "window_panes": "1", "pane_dead": "0", "window-size": "manual",
        "@rs_resize_epoch": read("epoch"), "version": "3.8", "socket_path": sys.argv[3],
        "@rs_paste_launch": read("launch"), "@rs_paste_admission": read("marker"),
        "@rs_create_initializer": read(name + ".initializer"), "@rs_create_birth": read(name + ".birth"),
        "pane_input_off": "0", "command-error": "", "after-if-shell": "",
    }
    for hook in ("load-buffer", "save-buffer", "paste-buffer", "delete-buffer",
                 "send-keys", "set-option", "display-message", "resize-window", "list-buffers"):
        actual["after-" + hook] = ""
    options = re.findall(r"#\{==:#\{(@rs_paste_custody_[^}]+)\},", guard)
    if options:
        option, = options
        path = custody_path(option)
        prior = path.read_text() if path.exists() else None
        actual[option] = "" if prior is None else prior
        if not allows(guard, actual):
            if otherwise:
                print(re.search(r"rs-(?:paste-)?rejected-[0-9a-f-]+", otherwise[0]).group())
            sys.exit()
        nodes = branch.split(" ; ")
        if nodes[-1].startswith("set-option -s"):
            action, flag, key, value = nodes[-1].split()
            assert action == "set-option" and key == option and flag in ("-s", "-so")
            mutation, state, fields = custody_value(value)
            if flag == "-so" and prior is not None:
                sys.exit(1)
            if prior is None:
                assert flag == "-so" and state == "possible"
            else:
                old_mutation, old_state, old_fields = custody_value(prior)
                assert mutation != old_mutation
                assert (old_state, state) in (("free", "possible"), ("possible", "completed"),
                                              ("possible", "free"), ("completed", "free"))
                if state == "completed":
                    assert fields == old_fields
            if state == "possible":
                if fields[0] == "probe":
                    assert fields[1] == name and fields[5:11] == ["0", "0", "0", pid, read("pane-start"), read("tty")]
                    assert fields[-3] == "managed:" + ":".join(read(name + ".initializer").split("~")[4:8:2]) + ":" + read(name + ".initializer").split("~")[7]
                    assert not read("marker") and not read("launch")
                else:
                    assert fields[:2] == [name, read("tty")] and "~".join(fields[2:]) == read("marker")
            if len(nodes) == 2:
                assert state == "free" and nodes[0] == f"delete-buffer -b {old_fields[-1]}"
                buffer_path(old_fields[-1]).unlink()
            else:
                assert len(nodes) == 1
            path.parent.mkdir(exist_ok=True)
            with path.open("x" if flag == "-so" else "w") as out:
                out.write(value)
            sys.exit()
    assert allows(guard, actual), (guard, branch, otherwise)
    for node in branch.split(" ; "):
        action, *words = node.split()
        if action == "set-option":
            option, value = words[-2:]
            assert words[:-2] in (["-p", "-t", pane], ["-p", "-o", "-t", pane], ["-po", "-t", pane])
            path = {"@rs_resize_epoch": "epoch", "@rs_paste_launch": "launch",
                    "@rs_paste_admission": "marker", "@rs_create_birth": name + ".birth"}[option]
            if option == "@rs_create_birth":
                assert re.search(r"#\{==:#\{@rs_create_initializer\},[^}]+\}", guard)
                assert f"#{{==:#{{@rs_create_birth}},{read(path)}}}" in guard
                assert read(path).startswith(read(name + ".initializer") + "~")
                assert value == "'used~" + read(path) + "'"
                value = value[1:-1]
            if "-o" in words or "-po" in words:
                assert not Path(path).exists()
            Path(path).write_text(value)
        elif action == "load-buffer":
            flag, buffer, source = words
            assert (flag, source) == ("-b", "-")
            assert options and prior is not None
            _, state, fields = custody_value(prior)
            assert state == "possible" and fields[-1] == buffer
            buffer_path(buffer).write_bytes(sys.stdin.buffer.read())
        elif action == "delete-buffer":
            flag, buffer = words
            assert flag == "-b"
            buffer_path(buffer).unlink()
        elif action == "list-buffers":
            _, flag, present, filter_flag, condition = shlex.split(node)
            assert (flag, present, filter_flag) == ("-F", "present", "-f") and options
            name = re.fullmatch(r"#\{==:#\{buffer_name\},(rs-paste-(?:probe-)?[0-9a-f]{32})\}", condition).group(1)
            assert custody_value(prior)[2][-1] == name
            if buffer_path(name).exists():
                print("present")
        elif action == "paste-buffer":
            assert words[:5] == ["-p", "-r", "-S", "-d", "-b"] and words[6:] == ["-t", pane]
            buffer = buffer_path(words[5])
            assert read("marker").split("~")[-4] == "s" and read("marker").endswith("~" + buffer.name)
            with Path("pane-bytes").open("ab") as terminal:
                terminal.write(buffer.read_bytes())
            buffer.unlink()
            with Path("paste-effects").open("ab") as effects:
                effects.write(b"1")
        else:
            assert action == "display-message" and words[:-1] == ["-p", "-l", "-t", pane]
            print(words[-1])
"##;

#[cfg(debug_assertions)]
#[derive(Clone, Copy)]
enum RealEofStatus {
    Unavailable,
    FirstSample,
    SecondSample,
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn w1_terminal_eof_standalone_status_failure_preserves_fifo() {
    check_real_eof_status(RealEofStatus::Unavailable, true).await;
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn w1_terminal_eof_real_host_replacement_discards_fifo() {
    for outcome in [RealEofStatus::FirstSample, RealEofStatus::SecondSample] {
        for standalone in [true, false] {
            check_real_eof_status(outcome, standalone).await;
        }
    }
}

#[cfg(debug_assertions)]
pub(super) fn write_catalog_birth_fixture(directory: &Path, tty: &Path, scope: &str) {
    let spawn = directory.join("spawn-tmux");
    fs::write(&spawn, PASTE_HOST_MUTATIONS).unwrap();
    fs::set_permissions(&spawn, fs::Permissions::from_mode(0o700)).unwrap();
    let pid = std::process::id();
    let start = remote_sessions_host::engine::launch_fixture_process_start(pid).unwrap();
    fs::write(directory.join("pane-pid"), pid.to_string()).unwrap();
    fs::write(directory.join("pane-start"), start.to_string()).unwrap();
    fs::write(directory.join("pane-scope"), scope).unwrap();
    fs::write(directory.join("tty"), tty.to_str().unwrap()).unwrap();
}

#[cfg(debug_assertions)]
pub(super) fn seed_historical_terminal(
    directory: &Path,
    runtime: &Runtime,
    scope: (u32, u32, u32),
    disposition: char,
) -> String {
    let (session, window, pane) = scope;
    let name = &runtime.tmux_name;
    for option in ["initializer", "birth"] {
        fs::remove_file(directory.join(format!("{name}.{option}"))).unwrap();
    }
    let read = |name: &str| fs::read_to_string(directory.join(name)).unwrap();
    let generation = read("generation");
    let pid = read("pane-pid");
    let start = read("pane-start");
    let nonce = read(&format!("{name}.nonce"));
    let launch = Uuid::new_v4();
    let marker_nonce = Uuid::new_v4();
    let (epoch, marker_epoch, suffix) = if disposition == 'r' {
        (String::new(), String::new(), format!("r~{marker_nonce}"))
    } else {
        let epoch = format!("{}:{}", Uuid::new_v4(), Uuid::new_v4());
        let suffix = format!(
            "{disposition}~{marker_nonce}~1~rs-paste-{}",
            Uuid::new_v4().simple()
        );
        (epoch.clone(), epoch, suffix)
    };
    let marker = format!(
        "v1~{}~{}~52~101~{session}~{window}~{pane}~{}~{}~{}~{launch}~{marker_epoch}~{suffix}",
        runtime.runtime_id,
        generation.trim_end(),
        pid.trim_end(),
        start.trim_end(),
        nonce.trim_end()
    );
    fs::write(directory.join("epoch"), epoch).unwrap();
    fs::write(directory.join("launch"), launch.to_string()).unwrap();
    fs::write(directory.join("marker"), &marker).unwrap();
    marker
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn w1_legacy_provenance_bridge_restores_finished_held_and_retains_bound_read() {
    use std::io::Write;

    let host = AgentFixture::new();
    let attachment = AgentFixture::new();
    let generation = Uuid::new_v4();
    let provider = native_pty_system()
        .openpty(pty_size(TerminalSize {
            cols: 100,
            rows: 30,
        }))
        .unwrap();
    let tty = provider.master.tty_name().unwrap();
    let (server, workspace_id) = start_catalog_terminal_fixture(&host, generation, &tty).await;
    let original = launch_catalog_terminal(&host, workspace_id).await;
    fs::write(host.0.join("active"), &original.tmux_name).unwrap();
    let marker = seed_historical_terminal(&host.0, &original, (0, 0, 0), 's');

    let tmux = attachment_tmux(&attachment, generation, "ignore-size");
    let listener = UnixListener::bind(attachment.0.join("agent.sock")).unwrap();
    let relay_host = host.0.clone();
    let opens = Arc::new(AtomicU64::new(0));
    let accepted = Arc::new(AtomicU64::new(0));
    let relay_opens = Arc::clone(&opens);
    let relay_accepted = Arc::clone(&accepted);
    let relay = tokio::spawn(async move {
        loop {
            let (mut peer, command) = receive(&listener).await;
            assert!(matches!(
                command,
                Request::OpenTerminal { .. }
                    | Request::TerminalStatus { .. }
                    | Request::List {}
                    | Request::Health {}
            ));
            let response = request(&relay_host, &command).await.unwrap();
            if matches!(command, Request::OpenTerminal { .. }) {
                relay_opens.fetch_add(1, Ordering::Relaxed);
                if matches!(response, Response::TerminalOpened { .. }) {
                    relay_accepted.fetch_add(1, Ordering::Relaxed);
                }
            }
            respond(&mut peer, response).await;
        }
    });
    let runtime = original.runtime_id.to_string();
    let bridge = DesktopBridge::new(Some(attachment.0.clone()), tmux);
    let attached = bridge.attach_observer(&runtime).await.unwrap();
    let restoration = || TerminalRestoration {
        previous: attached.status.terminal.clone(),
        agent_pid: std::process::id(),
        session_id: original.session_id,
        operation_id: original.operation_id,
    };
    let restored = bridge
        .restore_attachment(&runtime, restoration())
        .await
        .unwrap();
    assert_ne!(restored.view_id, attached.view_id);
    assert_eq!(
        restored.status.terminal.target,
        attached.status.terminal.target
    );
    assert!(restored.status.epoch.is_some());
    bridge.detach(&attached.view_id);
    until(|| allocation_counts(&bridge) == (1, 1)).await;
    assert_eq!(
        read_until_bytes(&bridge, &restored.view_id, b"buffered attachment output").await,
        b"buffered attachment output"
    );

    fs::rename(
        host.0.join("catalog/catalog.sqlite3"),
        host.0.join("retained.sqlite3"),
    )
    .unwrap();
    let status = bridge.status(&restored.view_id).await.unwrap();
    assert_eq!(status.terminal, restored.status.terminal);
    assert_eq!(status.observation, ObservationState::Observed);
    let view = bridge.view(&restored.view_id).unwrap();
    let client = fs::read_to_string(attachment.0.join("client")).unwrap();
    let mut client_lines = client.lines();
    assert_eq!(
        client_lines.next().unwrap().parse::<u32>().unwrap(),
        view.client_pid
    );
    assert_eq!(Path::new(client_lines.next().unwrap()), view.tty);
    let mut output = fs::OpenOptions::new().write(true).open(&view.tty).unwrap();
    let expected = b"new PTY bytes during catalog fault";
    output.write_all(expected).unwrap();
    drop(output);
    view.output.lock().await.refresh_at = Instant::now();
    let bytes = tokio::time::timeout(Duration::from_secs(2), async {
        let mut bytes = Vec::new();
        let mut refreshed = false;
        while bytes.len() < expected.len() {
            let chunk = bridge.read(&restored.view_id).await.unwrap();
            assert!(!chunk.closed && !chunk.writable && chunk.geometry.is_none());
            assert!(chunk.paste_incarnation.is_none() && chunk.paste_completion.is_none());
            if let Some(current) = chunk.status {
                assert_eq!(current.terminal, status.terminal);
                assert_eq!(current.observation, ObservationState::Observed);
                refreshed = true;
            }
            bytes.extend(chunk.data);
        }
        assert!(refreshed);
        bytes
    })
    .await
    .unwrap();
    assert_eq!(bytes, expected);
    let refused_observer = bridge
        .attach_observer(&runtime)
        .await
        .err()
        .expect("catalog fault admitted a fresh observer");
    assert_eq!(refused_observer.to_string(), "Attachment not authorized");
    let refused = bridge
        .restore_attachment(
            &runtime,
            TerminalRestoration {
                previous: status.terminal.clone(),
                ..restoration()
            },
        )
        .await
        .err()
        .unwrap();
    assert_eq!(refused.to_string(), "Attachment not authorized");
    assert_eq!(opens.load(Ordering::Relaxed), 4);
    assert_eq!(accepted.load(Ordering::Relaxed), 2);
    assert_eq!(allocation_counts(&bridge), (1, 1));
    assert!(Arc::ptr_eq(&view, &bridge.view(&restored.view_id).unwrap()));
    assert_eq!(
        fs::read_to_string(attachment.0.join("client")).unwrap(),
        client
    );
    assert_eq!(fs::read_to_string(host.0.join("marker")).unwrap(), marker);
    assert!(!host.0.join("unexpected").exists());
    assert!(!host.0.join("pane-bytes").exists());

    bridge.detach(&restored.view_id);
    until(|| allocation_counts(&bridge) == (0, 0)).await;
    relay.abort();
    server.abort();
    assert!(relay.await.unwrap_err().is_cancelled());
    assert!(server.await.unwrap_err().is_cancelled());
}

#[cfg(debug_assertions)]
async fn start_catalog_terminal_fixture(
    host: &AgentFixture,
    generation: Uuid,
    tty: &Path,
) -> (tokio::task::JoinHandle<Result<()>>, Uuid) {
    use remote_sessions_host::serve_catalog_fixture;

    let tmux = host.0.join("tmux");
    fs::write(&tmux, STATUS_HOST_TMUX).unwrap();
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
    let codex = host.0.join("codex");
    fs::write(&codex, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(host.0.join("generation"), generation.to_string()).unwrap();
    write_catalog_birth_fixture(&host.0, tty, "$0~@0~%0");
    let project = host.0.join("project");
    fs::create_dir(&project).unwrap();
    let server = tokio::spawn({
        let directory = host.0.clone();
        async move {
            serve_catalog_fixture(
                directory.clone(),
                tmux,
                directory.join("catalog"),
                directory.as_os_str(),
            )
            .await
        }
    });
    until(|| server.is_finished() || host.0.join("agent.sock").exists()).await;
    assert!(!server.is_finished());
    let workspace = product_request(
        &host.0,
        &ProductRequest::RegisterWorkspace {
            locator: project.to_str().unwrap().into(),
        },
    )
    .await
    .unwrap();
    let ProductResponse::WorkspaceRegistered { workspace } = workspace else {
        panic!("expected a registered fixture workspace");
    };
    (server, workspace.workspace_id)
}

#[cfg(debug_assertions)]
async fn launch_catalog_terminal(host: &AgentFixture, workspace_id: Uuid) -> Runtime {
    use remote_sessions_host::model::{CreateState, PaneLiveness};

    let reply = product_request(
        &host.0,
        &ProductRequest::CreateManaged {
            key: Uuid::new_v4(),
            workspace_id,
            profile: ProviderProfile::Codex,
            reservation: None,
        },
    )
    .await
    .unwrap();
    let ProductResponse::Create {
        state:
            CreateState::Confirmed {
                outcome,
                current: PaneLiveness::Live,
                ..
            },
    } = reply
    else {
        panic!("expected a catalog-corroborated live pane: {reply:?}");
    };
    outcome.runtime
}

#[cfg(debug_assertions)]
async fn check_real_eof_status(outcome: RealEofStatus, standalone: bool) {
    let host = AgentFixture::new();
    let attachment = AgentFixture::new();
    let generation = Uuid::new_v4();
    let initial = TerminalSize {
        cols: 100,
        rows: 30,
    };
    let provider = native_pty_system().openpty(pty_size(initial)).unwrap();
    let tty = provider.master.tty_name().unwrap();
    let (server, workspace_id) = start_catalog_terminal_fixture(&host, generation, &tty).await;
    let original = launch_catalog_terminal(&host, workspace_id).await;
    fs::write(host.0.join("active"), &original.tmux_name).unwrap();
    let runtime_id = original.runtime_id;
    let runtime = runtime_id.to_string();
    fs::write(attachment.0.join("hold"), "").unwrap();
    let tmux = attachment_tmux(&attachment, generation, "ignore-size");
    let listener = UnixListener::bind(attachment.0.join("agent.sock")).unwrap();
    let bridge = DesktopBridge::new(Some(attachment.0.clone()), tmux);
    let prove = async {
        let (mut peer, opening) = receive(&listener).await;
        assert!(
            matches!(opening, Request::OpenTerminal { runtime_id: id, .. } if id == runtime_id)
        );
        let opened = request(&host.0, &opening).await.unwrap();
        let Response::TerminalOpened { status } = &opened else {
            panic!("host did not prove the original terminal: {opened:?}");
        };
        assert_eq!(status.observation, ObservationState::Observed);
        let status = status.clone();
        respond(&mut peer, opened).await;
        let mut peer = attachment_status_peer(&listener, &status).await;
        let reply = request(
            &host.0,
            &Request::TerminalStatus {
                terminal: status.terminal.clone(),
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(&reply, Response::TerminalStatus { status: current }
            if current.terminal == status.terminal && current.observation == ObservationState::Observed)
        );
        respond(&mut peer, reply).await;
        status
    };
    let (attached, status) = tokio::join!(bridge.attach_observer(&runtime), prove);
    let id = attached.unwrap().view_id;
    let view = bridge.view(&id).unwrap();
    assert_eq!(view.terminal, status.terminal);
    view.output.lock().await.refresh_at = Instant::now() + Duration::from_secs(60);
    assert_eq!(
        read_until_bytes(&bridge, &id, b"buffered attachment output").await,
        b"buffered attachment output"
    );
    let birth_path = host.0.join(format!("{}.birth", original.tmux_name));
    let birth = fs::read_to_string(&birth_path).unwrap();
    let start = fs::read_to_string(host.0.join("pane-start")).unwrap();
    let mut fields: Vec<_> = birth.split('~').collect();
    let start_index = fields.len() - 5;
    assert_eq!(fields[start_index], start);
    let other_start = (start.parse::<u64>().unwrap() + 1).to_string();
    fields[start_index] = &other_start;
    fs::write(&birth_path, fields.join("~")).unwrap();
    let restored = DesktopBridge::new(Some(host.0.clone()), "/unused/tmux".into());
    let restoration = || TerminalRestoration {
        previous: status.terminal.clone(),
        agent_pid: std::process::id(),
        session_id: original.session_id,
        operation_id: original.operation_id,
    };
    let refused = restored
        .restore_attachment(&runtime, restoration())
        .await
        .err()
        .expect("restored view adopted unproved birth");
    assert_eq!(refused.to_string(), "Attachment not authorized");
    assert_eq!(allocation_counts(&restored), (0, 0));
    fs::write(&birth_path, birth).unwrap();
    let exact = request(
        &host.0,
        &Request::TerminalStatus {
            terminal: status.terminal.clone(),
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(exact, Response::TerminalStatus { status: exact } if exact.terminal == status.terminal)
    );
    let replacement = launch_catalog_terminal(&host, workspace_id).await;
    assert_eq!(replacement.pane_id, original.pane_id);
    assert_ne!(replacement.runtime_id, original.runtime_id);

    let samples = match outcome {
        RealEofStatus::Unavailable => {
            fs::write(host.0.join("active"), &original.tmux_name).unwrap();
            fs::rename(
                host.0.join("catalog/catalog.sqlite3"),
                host.0.join("retained.sqlite3"),
            )
            .unwrap();
            let exact = request(
                &host.0,
                &Request::TerminalStatus {
                    terminal: status.terminal.clone(),
                },
            )
            .await
            .unwrap();
            assert!(matches!(exact, Response::TerminalStatus { status: exact }
                if exact.terminal == status.terminal && exact.observation == ObservationState::Observed));
            let refused = restored
                .restore_attachment(&runtime, restoration())
                .await
                .err()
                .expect("faulted catalog admitted an unbound view");
            assert_eq!(refused.to_string(), "Attachment not authorized");
            assert_eq!(allocation_counts(&restored), (0, 0));
            fs::write(host.0.join("fail-query"), "").unwrap();
            vec![&original.tmux_name]
        }
        RealEofStatus::FirstSample => vec![&replacement.tmux_name],
        RealEofStatus::SecondSample => {
            fs::write(host.0.join("missing-device"), "").unwrap();
            vec![&original.tmux_name, &replacement.tmux_name]
        }
    };
    for (index, name) in samples.iter().enumerate() {
        fs::write(host.0.join(format!("sample-{}", index + 1)), name).unwrap();
    }
    fs::write(host.0.join("samples"), "").unwrap();
    view.output.lock().await.refresh_at = Instant::now();
    let mut pending = std::pin::pin!(async {
        if standalone {
            bridge.status(&id).await.map(|_| None)
        } else {
            bridge.read(&id).await.map(Some)
        }
    });
    let (mut peer, query) = tokio::select! {
        incoming = receive(&listener) => incoming,
        _ = &mut pending => panic!("terminal status settled before the fixture received it"),
    };
    assert!(matches!(&query, Request::TerminalStatus { terminal } if *terminal == view.terminal));
    let reply = request(&host.0, &query).await.unwrap();
    let expected_error = match outcome {
        RealEofStatus::Unavailable => {
            assert!(matches!(&reply, Response::Error { .. }));
            "Розмір термінала недоступний"
        }
        RealEofStatus::FirstSample | RealEofStatus::SecondSample => {
            assert!(matches!(&reply, Response::TerminalChanged {}));
            "Terminal attachment identity changed"
        }
    };
    let sampled = fs::read_to_string(host.0.join("samples")).unwrap();
    assert!(sampled.lines().eq(samples.into_iter().map(String::as_str)));

    fs::write(attachment.0.join("continue"), "").unwrap();
    tokio::time::timeout(Duration::from_secs(2), view.retired())
        .await
        .unwrap();
    let expires = {
        let views = bridge.views.lock().unwrap();
        let drain = &views.drains[&view.terminal.view_id];
        assert!(view.eof_draining() && Arc::ptr_eq(&drain.view, &view));
        drain.expires
    };
    until(|| view.cleanup_confirmations.load(Ordering::Acquire) == 3).await;
    assert_eq!(allocation_counts(&bridge), (0, 1));
    assert_eq!(
        bridge
            .input(
                &id,
                b"after EOF".to_vec(),
                &Uuid::new_v4().to_string(),
                false
            )
            .await
            .unwrap_err()
            .to_string(),
        "View detached"
    );
    respond(&mut peer, reply).await;
    let result = pending.await;
    assert_eq!(result.err().unwrap().to_string(), expected_error);
    if matches!(outcome, RealEofStatus::Unavailable) {
        assert_eq!(
            bridge.views.lock().unwrap().drains[&view.terminal.view_id].expires,
            expires
        );
        assert_parked_eof_fifo(&bridge, &id, bridge.read(&id).await.unwrap()).await;
    } else {
        assert!(matches!(
            *view.retirement.borrow(),
            Some(Retirement::Failed)
        ));
    }
    assert_eq!(
        bridge.read(&id).await.err().unwrap().to_string(),
        "View detached"
    );
    until(|| allocation_counts(&bridge) == (0, 0)).await;
    assert!(view.output.lock().await.receiver.is_empty());
    assert!(!host.0.join("unexpected").exists());
    assert!(
        tokio::time::timeout(Duration::from_millis(25), listener.accept())
            .await
            .is_err()
    );
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

async fn assert_parked_eof_fifo(bridge: &DesktopBridge, id: &str, mut chunk: Chunk) {
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
        chunk = bridge.read(id).await.unwrap();
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
            assert_eq!(
                bridge.views.lock().unwrap().drains[&view.terminal.view_id].expires,
                expires
            );
            assert_parked_eof_fifo(&bridge, &id, result.unwrap()).await;
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
async fn w1_terminal_resize_admission_and_historical_settlement_across_renewal() {
    use std::{future::Future, task::Poll};

    for already_admitted in [false, true] {
        let fixture = AgentFixture::new();
        let listener = UnixListener::bind(fixture.0.join("agent.sock")).unwrap();
        let bridge = DesktopBridge::new(Some(fixture.0.clone()), "/unused/tmux".into());
        let (view, reader, _) = controlled_view(&bridge, &target(), Uuid::new_v4(), 0);
        let id = view.terminal.view_id.to_string();
        let (token, epoch, captured) = grant_fixture_writer(&listener, &bridge, &view).await;
        let started = Instant::now() + Duration::from_secs(30);
        let old_deadline = started + INPUT_LEASE_TTL / 2;
        {
            let mut control = view.control.lock().await;
            control.lease.as_mut().unwrap().deadline = old_deadline;
            view.publish_control(&control);
        }
        let now = Cell::new(started);
        let mut old = std::pin::pin!(bridge.resize(&id, GRID_A, epoch, captured));
        let pending = if already_admitted {
            Some(tokio::select! {
                result = old.as_mut() => panic!("historical resize finished early: {result:?}"),
                request = receive(&listener) => request,
            })
        } else {
            None
        };
        let drive_renewal = async {
            let (
                mut peer,
                Request::Renew {
                    runtime_id,
                    token: actual,
                },
            ) = receive(&listener).await
            else {
                panic!("expected same-token renewal");
            };
            assert_eq!(
                (runtime_id, actual),
                (view.terminal.target.runtime_id, token)
            );
            assert!(view.control.try_lock().is_err());
            if !already_admitted {
                std::future::poll_fn(|cx| {
                    assert!(old.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                assert!(view.resize.try_lock().is_err());
            }
            now.set(old_deadline + Duration::from_nanos(1));
            respond(&mut peer, Response::TerminalControl { token, epoch }).await;
        };
        tokio::join!(
            refresh_control_with_clock(&view, || now.get()),
            drive_renewal
        );
        if pending.is_none() {
            tokio::select! {
                result = old.as_mut() => assert_eq!(result.unwrap_err().to_string(), "Resize authority changed"),
                _ = receive(&listener) => panic!("old capture reached the host after renewal"),
            }
        }
        let fresh = bridge.read(&id).await.unwrap().paste_incarnation.unwrap();
        assert_ne!(fresh, captured);
        {
            let control = view.control.try_lock().unwrap();
            let lease = control.lease.as_ref().unwrap();
            assert_eq!(
                (lease.token, lease.epoch, lease.resize_sequence),
                (token, epoch, 0)
            );
        }
        let settle = async |(mut peer, request): (UnixStream, Request),
                            sequence: u64,
                            size: TerminalSize| {
            assert!(matches!(request, Request::ResizeTerminal {
                terminal, token: actual, sequence: sent, size: proposed,
            } if terminal == view.terminal && actual == token && sent == sequence && proposed == size));
            let admission = ResizeIntent {
                id: Uuid::new_v4(),
                epoch,
            };
            let mut status = view.output.lock().await.status.clone();
            status.committed = CommittedGeometry {
                size,
                revision: sequence + 1,
                intent: Some(admission),
            };
            let result = ResizeResult::Committed { admission, status };
            respond(
                &mut peer,
                Response::Resize {
                    resize: result.clone(),
                },
            )
            .await;
            result
        };
        if let Some(request) = pending {
            let (result, expected) = tokio::join!(old, settle(request, 0, GRID_A));
            assert_eq!(result.unwrap(), expected);
        }
        assert_no_fixture_request(&listener).await;

        let sequence = if already_admitted { 1 } else { 0 };
        assert_eq!(
            view.control
                .lock()
                .await
                .lease
                .as_ref()
                .unwrap()
                .resize_sequence,
            sequence
        );
        let reply = async { settle(receive(&listener).await, sequence, GRID_B).await };
        let (result, expected) = tokio::join!(bridge.resize(&id, GRID_B, epoch, fresh), reply);
        assert_eq!(result.unwrap(), expected);
        assert_eq!(
            view.control
                .lock()
                .await
                .lease
                .as_ref()
                .unwrap()
                .resize_sequence,
            sequence + 1
        );
        assert_no_fixture_request(&listener).await;
        bridge.detach(&id);
        release_fixture_writer(&listener, &view, token).await;
        reader.finish();
        until(|| allocation_counts(&bridge) == (0, 0)).await;
    }
}

#[tokio::test]
async fn w1_terminal_same_token_control_reply_fences_expired_incarnation() {
    check_same_token_renewal(RenewalTiming::ExpiredBeforeRequest, true).await;
    check_same_token_renewal(RenewalTiming::Live, true).await;
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn w1_terminal_paste_same_token_renewal_accepts_fresh_grant() {
    let host = AgentFixture::new();
    let attachment = AgentFixture::new();
    let generation = Uuid::new_v4();
    let provider = native_pty_system()
        .openpty(pty_size(TerminalSize {
            cols: 100,
            rows: 30,
        }))
        .unwrap();
    let tty = provider.master.tty_name().unwrap();
    let _tmux_socket = UnixListener::bind(host.0.join("tmux.sock")).unwrap();
    let mutations = host.0.join("paste-tmux");
    fs::write(&mutations, PASTE_HOST_MUTATIONS).unwrap();
    fs::set_permissions(&mutations, fs::Permissions::from_mode(0o700)).unwrap();
    let (server, workspace_id) = start_catalog_terminal_fixture(&host, generation, &tty).await;
    let runtime = launch_catalog_terminal(&host, workspace_id).await;
    let birth = host.0.join(format!("{}.birth", runtime.tmux_name));
    assert!(
        fs::read_to_string(host.0.join("marker"))
            .unwrap()
            .contains("~r~")
    );

    let tmux = attachment_tmux(&attachment, generation, "ignore-size");
    let listener = UnixListener::bind(attachment.0.join("agent.sock")).unwrap();
    let bridge = DesktopBridge::new(Some(attachment.0.clone()), tmux);
    let park = async || {
        let (peer, query) = receive(&listener).await;
        let reply = request(&host.0, &query).await.unwrap();
        (peer, query, reply)
    };
    let forward = async || {
        let (mut peer, query, reply) = park().await;
        respond(&mut peer, reply).await;
        query
    };
    let runtime_id = runtime.runtime_id;
    let runtime = runtime_id.to_string();
    let attaching = bridge.attach_observer(&runtime);
    let prove = async {
        assert!(
            matches!(forward().await, Request::OpenTerminal { runtime_id: id, .. } if id == runtime_id)
        );
        assert!(matches!(forward().await, Request::TerminalStatus { .. }));
    };
    let (attached, ()) = tokio::join!(attaching, prove);
    let id = attached.unwrap().view_id;
    let view = bridge.view(&id).unwrap();

    let acquiring = refresh_fixture_control(&view);
    let (mut peer, query, reply) = park().await;
    assert!(matches!(query, Request::AcquireTerminal { terminal } if terminal == view.terminal));
    let Response::TerminalControl { token, epoch } = reply else {
        panic!("expected a real writer grant: {reply:?}");
    };
    respond(&mut peer, reply).await;
    acquiring.await.unwrap();

    let read = async || {
        view.output.lock().await.refresh_at = Instant::now();
        let (chunk, query) = tokio::join!(bridge.read(&id), forward());
        assert!(matches!(query, Request::TerminalStatus { terminal } if terminal == view.terminal));
        chunk.unwrap()
    };
    let paste = async |incarnation: Uuid, operation, action| {
        bridge
            .paste(&id, &incarnation.to_string(), operation, action)
            .await
    };
    let send = async |incarnation, operation, action| {
        tokio::join!(paste(incarnation, operation, action), forward())
    };
    let stage = |data: &[u8]| PasteAction::Stage {
        offset: 0,
        data: data.into(),
    };
    let ack = |receipt: Uuid| PasteAction::Ack {
        receipt: receipt.to_string(),
    };
    let effects = || fs::read(host.0.join("paste-effects")).unwrap_or_default();
    let first = read().await;
    assert!(first.writable);
    assert_eq!(first.paste_admission, None);
    let old_grant = first.paste_incarnation.unwrap();
    let first_draft = "first draft ☃".as_bytes();
    let (staged, request) = send(old_grant, 1, stage(first_draft)).await;
    assert_eq!(staged, PasteResult::Staged {});
    let Request::PasteStage {
        binding: first_binding,
        offset: 0,
        data,
    } = request
    else {
        panic!("expected a real paste stage");
    };
    assert_eq!(data, first_draft);
    assert_eq!(first_binding.terminal, view.terminal);
    assert_eq!((first_binding.token, first_binding.epoch), (token, epoch));
    assert_eq!((first_binding.sequence, first_binding.operation), (0, 1));
    assert_ne!(first_binding.incarnation, old_grant);
    let (committed, request) = send(
        old_grant,
        1,
        PasteAction::Commit {
            bytes: first_draft.len(),
        },
    )
    .await;
    assert!(matches!(request, Request::PasteCommit { binding, bytes }
        if binding == first_binding && bytes == first_draft.len()));
    let PasteResult::Committed {
        receipt: first_receipt,
    } = committed
    else {
        panic!("expected the first host receipt: {committed:?}");
    };
    let (acknowledged, request) = send(old_grant, 1, ack(first_receipt)).await;
    assert_eq!(acknowledged, PasteResult::Acknowledged { writable: true });
    assert!(matches!(request, Request::PasteAck { binding, receipt }
        if binding == first_binding && receipt == first_receipt));
    assert_eq!(effects(), b"1");
    assert_eq!(fs::read(host.0.join("pane-bytes")).unwrap(), first_draft);

    let obsolete = b"old staged draft".as_slice();
    let (staged, request) = send(old_grant, 2, stage(obsolete)).await;
    assert_eq!(staged, PasteResult::Staged {});
    assert!(
        matches!(request, Request::PasteStage { binding, offset: 0, data }
        if binding == (PasteBinding { operation: 2, sequence: 1, ..first_binding.clone() })
            && data == obsolete)
    );
    let old_deadline = {
        let mut control = view.control.lock().await;
        let lease = control.lease.as_mut().unwrap();
        lease.deadline = Instant::now();
        let deadline = lease.deadline;
        view.publish_control(&control);
        deadline
    };
    let native_clock = Cell::new(old_deadline - Duration::from_secs(1));
    let mut renewing = std::pin::pin!(refresh_control_with_clock(&view, || native_clock.get()));
    let (mut peer, query, reply) = tokio::select! {
        parked = park() => parked,
        () = &mut renewing => panic!("renewal settled before the accepted reply was parked"),
    };
    assert!(
        matches!(query, Request::Renew { runtime_id: id, token: current }
        if id == runtime_id && current == token)
    );
    assert!(matches!(reply, Response::Accepted {}));
    native_clock.set(old_deadline);
    let expired = read().await;
    assert!(!expired.writable && expired.paste_incarnation.is_none());
    respond(&mut peer, reply).await;
    renewing.await;
    let renewed = read().await;
    assert!(renewed.writable && renewed.paste_admission.is_none());
    let fresh_grant = renewed.paste_incarnation.unwrap();
    assert_ne!(fresh_grant, old_grant);
    assert_ne!(fresh_grant, first_binding.incarnation);

    let fresh_draft = "fresh draft ї".as_bytes();
    for (capture, operation, action) in [
        (old_grant, 3, stage(fresh_draft)),
        (
            old_grant,
            2,
            PasteAction::Commit {
                bytes: obsolete.len(),
            },
        ),
        (
            fresh_grant,
            2,
            PasteAction::Commit {
                bytes: obsolete.len(),
            },
        ),
        (
            fresh_grant,
            2,
            PasteAction::Stage {
                offset: obsolete.len(),
                data: b"x".into(),
            },
        ),
        (fresh_grant, 2, stage(obsolete)),
        (
            fresh_grant,
            3,
            PasteAction::Commit {
                bytes: fresh_draft.len(),
            },
        ),
    ] {
        assert_eq!(
            paste(capture, operation, action).await,
            PasteResult::Stale {}
        );
    }
    assert_no_fixture_request(&listener).await;
    assert_eq!(effects(), b"1");

    let (staged, request) = send(fresh_grant, 3, stage(fresh_draft)).await;
    assert_eq!(staged, PasteResult::Staged {});
    let expected_binding = PasteBinding {
        operation: 3,
        sequence: 1,
        ..first_binding
    };
    assert!(
        matches!(request, Request::PasteStage { binding, offset: 0, data }
        if binding == expected_binding && data == fresh_draft)
    );
    let mut committing = std::pin::pin!(paste(
        fresh_grant,
        3,
        PasteAction::Commit {
            bytes: fresh_draft.len()
        }
    ));
    let (mut peer, query, reply) = tokio::select! {
        parked = park() => parked,
        result = &mut committing => panic!("fresh Commit settled before the host reply was parked: {result:?}"),
    };
    let Request::PasteCommit {
        binding: fresh_binding,
        bytes,
    } = query
    else {
        panic!("expected the real fresh Commit");
    };
    assert_eq!(
        (fresh_binding.clone(), bytes),
        (expected_binding, fresh_draft.len())
    );
    let Response::Paste {
        paste: PasteResult::Committed {
            receipt: fresh_receipt,
        },
    } = reply
    else {
        panic!("expected the fresh host receipt: {reply:?}");
    };
    assert_ne!(fresh_receipt, first_receipt);
    let pending = tokio::time::timeout(Duration::from_secs(2), read())
        .await
        .unwrap();
    let fresh_gesture = PasteCompletion {
        incarnation: fresh_grant,
        operation: 3,
    };
    assert_eq!(pending.paste_completion, Some(fresh_gesture));
    assert!(!pending.writable && pending.paste_incarnation.is_none());
    assert_eq!(pending.paste_admission, None);
    assert_eq!(effects(), b"11");
    respond(&mut peer, reply).await;
    assert_eq!(
        committing.await,
        PasteResult::Committed {
            receipt: fresh_receipt
        }
    );

    refresh_control(&view).await;
    for (capture, operation, receipt) in [
        (old_grant, 3, fresh_receipt),
        (fresh_binding.incarnation, 3, fresh_receipt),
        (fresh_grant, 2, fresh_receipt),
        (fresh_grant, 3, first_receipt),
    ] {
        assert_eq!(
            paste(capture, operation, ack(receipt)).await,
            PasteResult::Stale {}
        );
    }
    assert_no_fixture_request(&listener).await;
    let pending = read().await;
    assert_eq!(pending.paste_completion, Some(fresh_gesture));
    assert_eq!(pending.paste_admission, None);
    let (acknowledged, request) = send(fresh_grant, 3, ack(fresh_receipt)).await;
    assert_eq!(acknowledged, PasteResult::Acknowledged { writable: true });
    assert!(matches!(request, Request::PasteAck { binding, receipt }
        if binding == fresh_binding && receipt == fresh_receipt));
    assert_eq!(
        paste(fresh_grant, 3, ack(fresh_receipt)).await,
        PasteResult::Stale {}
    );
    assert_no_fixture_request(&listener).await;
    let settled = read().await;
    assert!(settled.writable && settled.paste_completion.is_none());
    assert_eq!(settled.paste_incarnation, Some(fresh_grant));
    assert_eq!(settled.paste_admission, None);
    assert_eq!(effects(), b"11");
    assert_eq!(
        fs::read(host.0.join("pane-bytes")).unwrap(),
        [first_draft, fresh_draft].concat()
    );

    let used = fs::read_to_string(&birth).unwrap();
    assert!(used.starts_with("used~v1~"));
    let mut replaced: Vec<_> = used.split('~').map(str::to_owned).collect();
    replaced[8] = Uuid::new_v4().to_string();
    for invalid in [String::new(), replaced.join("~")] {
        fs::write(&birth, invalid).unwrap();
        for query in [
            Request::AcquireTerminal {
                terminal: view.terminal.clone(),
            },
            Request::Input {
                runtime_id,
                token,
                sequence: fresh_binding.sequence + 1,
                data: vec![b'x'],
                paste: false,
            },
        ] {
            let reply = remote_sessions_host::request(&host.0, &query)
                .await
                .unwrap();
            assert!(
                matches!(reply, Response::ControlRecovering { .. }),
                "{reply:?}"
            );
        }
        assert_eq!(effects(), b"11");
        assert_eq!(
            fs::read(host.0.join("pane-bytes")).unwrap(),
            [first_draft, fresh_draft].concat()
        );
    }
    fs::write(birth, used).unwrap();
    bridge.detach(&id);
    assert!(
        matches!(forward().await, Request::Release { runtime_id: id, token: current }
        if id == runtime_id && current == token)
    );
    until(|| allocation_counts(&bridge) == (0, 0)).await;
    assert!(!host.0.join("unexpected").exists());
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
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
    let wire_incarnation = {
        let control = view.control.lock().await;
        control.lease.as_ref().unwrap().paste_wire_incarnation
    };
    let incarnation_id = incarnation.to_string();
    let operation = 1;
    let binding = PasteBinding {
        terminal: view.terminal.clone(),
        token,
        epoch,
        incarnation: wire_incarnation,
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
            .resize(&id, GRID_A, epoch, incarnation)
            .await
            .unwrap(),
        ResizeResult::Busy {
            terminal: view.terminal.clone(),
            size: GRID_A
        }
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
