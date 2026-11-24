use super::*;
use crate::ownership::Ownership;
use nix::libc;
use std::{
    ffi::CStr,
    os::{
        fd::{FromRawFd, OwnedFd},
        unix::fs::PermissionsExt,
    },
};

const ORIGINAL: TerminalSize = TerminalSize {
    cols: 100,
    rows: 30,
};
const DESIRED: TerminalSize = TerminalSize {
    cols: 117,
    rows: 39,
};
const SEND_WATCHDOG: Duration = Duration::from_secs(4);

pub(super) struct Fixture {
    pub(super) engine: Arc<Engine>,
    pub(super) terminal: TerminalView,
    pub(super) temp: tempfile::TempDir,
    pty: Option<TestPty>,
}

struct TestPty {
    master: OwnedFd,
    _slave: OwnedFd,
    path: PathBuf,
}

fn test_pty() -> TestPty {
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_col: ORIGINAL.cols,
        ws_row: ORIGINAL.rows,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut size,
        )
    };
    assert_eq!(result, 0);
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let mut name = [0; 128];
    let result = unsafe { libc::ttyname_r(slave.as_raw_fd(), name.as_mut_ptr(), name.len()) };
    assert_eq!(result, 0);
    let tty = unsafe { CStr::from_ptr(name.as_ptr()) }
        .to_str()
        .unwrap()
        .into();
    TestPty {
        master,
        _slave: slave,
        path: tty,
    }
}

impl Fixture {
    fn new() -> Self {
        Self::build(SEND_WATCHDOG, Some(test_pty()))
    }

    pub(super) fn paste() -> Self {
        Self::with_paste_watchdog(SEND_WATCHDOG)
    }

    fn engine(
        tmux: PathBuf,
        socket: PathBuf,
        watchdog: Duration,
        provider_sample: Option<ProviderSample>,
    ) -> Engine {
        let mut engine = Engine::new(tmux, socket);
        engine.paste_test.sender_fixture = true;
        engine.paste_test.provider_sample = provider_sample;
        engine.paste_test.send_watchdog = Some(watchdog);
        engine
    }

    pub(super) fn with_paste_watchdog(watchdog: Duration) -> Self {
        Self::build(watchdog, None)
    }

    fn build(watchdog: Duration, pty: Option<TestPty>) -> Self {
        let tty = match &pty {
            Some(pty) => pty.path.clone(),
            None => PathBuf::from("/dev/ttysfixture"),
        };
        let provider_sample = pty.is_none().then_some(ProviderSample {
            size: ORIGINAL,
            sampled_at_us: 1,
        });
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let root = temp.path();
        let runtime_id = Uuid::new_v4();
        let session = Uuid::new_v4();
        let name = tmux_name(session, runtime_id);
        let generation = Uuid::new_v4();
        let pid = std::process::id();
        let tmux = root.join("tmux-fixture");
        let socket = root.join("tmux.sock");
        let engine = Arc::new(Self::engine(
            tmux.clone(),
            socket,
            watchdog,
            provider_sample,
        ));
        let terminal = TerminalView {
            target: TerminalTarget {
                runtime_id,
                generation,
                session: "$0".into(),
                window: "@0".into(),
                pane: "%0".into(),
                pid,
                process_start: process_start(pid).unwrap(),
            },
            view_id: Uuid::new_v4(),
            agent_epoch: engine.agent_epoch,
        };
        fs::write(root.join("grid"), "100 30").unwrap();
        fs::write(root.join("epoch"), "").unwrap();
        fs::write(root.join("intent"), "").unwrap();
        fs::write(root.join("mode"), "").unwrap();
        let launch = Uuid::new_v4().to_string();
        let server = ServerGeneration {
            pid: 987,
            start: 123456,
            token: generation,
        };
        let initial = GeometryReadback {
            target: terminal.target.clone(),
            server,
            name: name.clone(),
            nonce: String::new(),
            tty: tty.clone(),
            epoch: None,
            committed: CommittedGeometry {
                size: ORIGINAL,
                revision: 0,
                intent: None,
            },
            paste_launch: launch,
            paste_marker: String::new(),
            sender_supported: true,
            input_enabled: true,
        };
        fs::write(root.join("launch"), &initial.paste_launch).unwrap();
        fs::write(root.join("marker"), initial.ready_marker(None)).unwrap();
        let script = r##"#!/usr/bin/env python3
from pathlib import Path
import json
import re
import subprocess
import sys
import time

root = Path(__file__).parent
def read(name):
    return (root / name).read_text() if (root / name).exists() else ""
def write(name, value):
    (root / name).write_text(value)
def append(name, data):
    with (root / name).open("ab") as out:
        out.write(data)
def until(name):
    end = time.monotonic() + 15
    while root.is_dir() and not (root / name).exists() and time.monotonic() < end:
        time.sleep(.005)
    return (root / name).exists()
def check(guard, pane, write_input=False):
    match = (
        pane == "%0" and not read("scope") and
        ("#{==:#{@rs_resize_epoch}," + read("epoch") + "}") in guard and
        ("#{==:#{@rs_paste_admission}," + read("marker") + "}") in guard and
        ("#{==:#{@rs_paste_launch}," + read("launch") + "}") in guard and
        "#{==:#{pane_id},%0}" in guard and
        "#{==:#{pane_pid},PANE_PID}" in guard and
        "#{==:#{@rs_create_generation},GENERATION}" in guard and not read("unsupported")
    )
    return match and (not write_input or (
        "#{==:#{pane_input_off},0}" in guard and not read("input-off")
    ))
def rejection(branch):
    return re.search(r"rs-(?:paste-)?rejected-[0-9a-f-]+", branch).group()
def send(args):
    _, _, _, pane, guard, branch, reject = args
    nodes = [node.split() for node in branch.split(" ; ")]
    marker, paste, reply = nodes
    if not check(guard, pane, True) or read("paste-mode") == "reject":
        return rejection(reject)
    assert marker[:5] == ["set-option", "-p", "-t", pane, "@rs_paste_admission"]
    assert paste[:7] == ["paste-buffer", "-p", "-r", "-S", "-d", "-b", paste[6]]
    assert paste[7:] == ["-t", pane] and "~a~" in read("marker")
    assert marker[5] == read("marker").replace("~a~", "~s~", 1)
    assert reply[:5] == ["display-message", "-p", "-l", "-t", pane]
    write("marker", marker[5])
    data = (root / paste[6]).read_bytes()
    if read("bracketed"):
        data = b"\x1b[200~" + data + b"\x1b[201~"
    append("pane0-bytes", data)
    (root / paste[6]).unlink()
    append("effects", b"1")
    return reply[-1]

if sys.argv[1] == "--server-send":
    args = json.loads(sys.argv[2])
    write("server-parked", "1")
    if until("allow-effect"):
        write("late-result.tmp", send(args))
        (root / "late-result.tmp").replace(root / "late-result")
    sys.exit()

args = sys.argv[4:]
command = args[0]
cols, rows = read("grid").split()
epoch, intent, mode = read("epoch"), read("intent"), read("mode")
if command == "list-sessions":
    print("RUNTIME_NAME")
elif command == "list-panes":
    print("RUNTIME_NAME", "%0", "PANE_PID", 0, cols, rows, "fixture", sep="\t")
elif command == "list-commands":
    print("load-buffer (loadb) [-w] [-b buffer-name] path")
    flags = "dpr" if read("probe-mode") == "missing-flag" else "dprS"
    print(f"paste-buffer (pasteb) [-{flags}] [-b buffer-name] [-t target-pane]")
elif command == "display-message":
    if args[-1] == "#{pid}|#{start_time}|#{@rs_create_generation}":
        print("987|123456|GENERATION")
    else:
        prefix = 'PREFIX'.replace('|%0|', '|%1|') if read("scope") else 'PREFIX'
        print(prefix, cols, rows, cols, rows, epoch, intent, read("launch"),
              read("marker"), int(bool(read("input-off"))), int(not read("unsupported")), sep="|")
elif command == "load-buffer":
    assert args[1] == "-b" and args[2].startswith("rs-paste-") and args[3:] == ["-"]
    identifier = args[2].removeprefix("rs-paste-").removeprefix("probe-")
    assert re.fullmatch("[0-9a-f]{32}", identifier)
    (root / args[2]).write_bytes(sys.stdin.buffer.read())
    write("last-load", args[2])
elif command == "save-buffer":
    assert args[1] == "-b" and args[2].startswith("rs-paste-probe-") and args[3:] == ["-"]
    data = (root / args[2]).read_bytes()
    sys.stdout.buffer.write(data[:-1] if read("probe-mode") == "truncated" else data)
elif command == "delete-buffer":
    assert args[1] == "-b"
    (root / args[2]).unlink(missing_ok=True)
elif command == "if-shell":
    _, flags, target, pane, guard, branch, *otherwise = args
    assert (flags, target) == ("-F", "-t")
    write("last-write", guard + "\n" + branch + "\n")
    if branch.startswith("resize-window"):
        if mode == "stale":
            sys.exit()
        if mode == "lost-before":
            sys.exit(1)
        resize, marker, reply = [node.split() for node in branch.split(" ; ")]
        write("grid", resize[4] + " " + resize[6])
        write("intent", marker[5])
        if mode == "lost-after":
            sys.exit(1)
        print(reply[-1])
    elif " ; paste-buffer " in branch:
        append("submissions", b"1")
        write("last-effect-command", json.dumps(args))
        paste_mode = read("paste-mode")
        if paste_mode in ("late", "timeout-before"):
            subprocess.Popen([sys.executable, __file__, "--server-send", json.dumps(args)],
                             stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                             stderr=subprocess.DEVNULL, start_new_session=True)
            if until("late-result"):
                print(read("late-result"))
        else:
            reply = send(args)
            if paste_mode in ("hold-reply", "timeout-after"):
                if not until("allow-reply"):
                    sys.exit(1)
            print(reply)
    elif branch.startswith("send-keys"):
        if check(guard, pane, True):
            send_keys, reply = [node.split() for node in branch.split(" ; ")]
            assert send_keys[:4] == ["send-keys", "-H", "-t", pane]
            assert reply == ["display-message", "-p", "-l", "-t", pane, "rs-input-accepted"]
            append("pane0-bytes", bytes.fromhex(" ".join(send_keys[4:])))
            print(reply[-1])
    elif branch.startswith("set-option"):
        if not check(guard, pane):
            if otherwise:
                print(rejection(otherwise[0]))
            sys.exit()
        for node in branch.split(" ; "):
            words = node.split()
            if words[0] == "display-message":
                assert words[:5] == ["display-message", "-p", "-l", "-t", pane]
                print(words[-1])
            elif words[-2] == "@rs_resize_epoch":
                write("epoch", words[-1])
            elif words[-2] == "@rs_paste_launch":
                write("launch", words[-1])
            elif words[-2] == "@rs_paste_admission":
                ack_reply = read("ack-reply") if "~s~" in read("marker") and "~r~" in words[-1] else ""
                if ack_reply == "before":
                    sys.exit(1)
                write("marker", words[-1])
                if ack_reply == "after":
                    sys.exit(1)
            else:
                sys.exit(91)
    else:
        sys.exit(91)
else:
    sys.exit(92)
"##;
        let prefix = format!(
            "987|123456|{generation}|$0|@0|%0|{pid}|{name}|0|0||0|1|1|manual|{}",
            tty.display()
        );
        fs::write(
            &tmux,
            script
                .replace("PREFIX", &prefix)
                .replace("RUNTIME_NAME", &name)
                .replace("PANE_PID", &pid.to_string())
                .replace("GENERATION", &generation.to_string()),
        )
        .unwrap();
        fs::set_permissions(tmux, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            engine,
            terminal,
            temp,
            pty,
        }
    }

    pub(super) fn restart(&self) -> (Engine, TerminalView) {
        let engine = Self::engine(
            self.engine.tmux.clone(),
            self.engine.socket.clone(),
            self.engine.paste_test.send_watchdog.unwrap(),
            self.engine.paste_test.provider_sample,
        );
        let terminal = TerminalView {
            agent_epoch: engine.agent_epoch,
            view_id: Uuid::new_v4(),
            ..self.terminal.clone()
        };
        (engine, terminal)
    }

    pub(super) async fn acquire(&self, terminal: TerminalView) -> (Uuid, ResizeEpoch) {
        match self
            .engine
            .handle(Request::AcquireTerminal { terminal }, None)
            .await
            .unwrap()
        {
            Response::TerminalControl { token, epoch } => (token, epoch),
            response => panic!("Expected confirmed authority, got {response:?}"),
        }
    }

    async fn resize(
        &self,
        terminal: TerminalView,
        token: Uuid,
        sequence: u64,
        size: TerminalSize,
    ) -> Result<ResizeResult> {
        match self
            .engine
            .handle(
                Request::ResizeTerminal {
                    terminal,
                    token,
                    sequence,
                    size,
                },
                None,
            )
            .await?
        {
            Response::Resize { resize } => Ok(resize),
            response => panic!("Expected a resize admission result, got {response:?}"),
        }
    }

    async fn status(&self, engine: &Engine, terminal: TerminalView) -> TerminalStatus {
        match engine
            .handle(Request::TerminalStatus { terminal }, None)
            .await
            .unwrap()
        {
            Response::TerminalStatus { status } => status,
            response => panic!("Expected a scoped status, got {response:?}"),
        }
    }
}

#[test]
fn w1_resize_controlled_clock_keeps_resize_and_input_sequences_independent() {
    let now = Instant::now();
    let terminal = TerminalView {
        view_id: Uuid::new_v4(),
        agent_epoch: Uuid::new_v4(),
        target: TerminalTarget {
            runtime_id: Uuid::new_v4(),
            generation: Uuid::new_v4(),
            session: "$0".into(),
            window: "@0".into(),
            pane: "%0".into(),
            pid: 2,
            process_start: 1,
        },
    };
    let runtime = terminal.target.runtime_id;
    let epoch = ResizeEpoch {
        agent: terminal.agent_epoch,
        owner: Uuid::new_v4(),
    };
    let mut owners = Ownership::default();
    let token = owners
        .grant(runtime, Some(terminal.clone()), Some(epoch), now)
        .unwrap();
    let mut wrong_view = terminal.clone();
    wrong_view.view_id = Uuid::new_v4();
    assert!(owners.resize(&wrong_view, token, epoch, 0, now).is_err());
    assert!(owners.resize(&terminal, token, epoch, 1, now).is_err());
    owners.resize(&terminal, token, epoch, 0, now).unwrap();
    owners.input(runtime, token, 0, now).unwrap();
    assert!(owners.resize(&terminal, token, epoch, 0, now).is_err());
    assert!(
        owners
            .resize(&terminal, token, epoch, 1, now + Duration::from_secs(15))
            .is_err()
    );
    let successor = ResizeEpoch {
        owner: Uuid::new_v4(),
        ..epoch
    };
    let later = now + Duration::from_secs(16);
    let next = owners
        .grant(runtime, Some(terminal.clone()), Some(successor), later)
        .unwrap();
    owners.release(runtime, token);
    assert!(owners.resize(&terminal, token, epoch, 1, later).is_err());
    owners.resize(&terminal, next, successor, 0, later).unwrap();
    owners.input(runtime, next, 0, later).unwrap();
}

#[tokio::test]
async fn w1_resize_admission_has_exact_scope_bounds_and_independent_provider_sample() {
    let f = Fixture::new();
    let (token, epoch) = f.acquire(f.terminal.clone()).await;
    for size in [
        TerminalSize { cols: 1, rows: 30 },
        TerminalSize {
            cols: 100,
            rows: 201,
        },
    ] {
        assert!(f.resize(f.terminal.clone(), token, 0, size).await.is_err());
    }
    let mut wrong = f.terminal.clone();
    wrong.target.generation = Uuid::new_v4();
    assert!(f.resize(wrong, token, 0, DESIRED).await.is_err());
    assert!(
        f.resize(f.terminal.clone(), Uuid::new_v4(), 0, DESIRED)
            .await
            .is_err()
    );
    assert!(
        f.resize(f.terminal.clone(), token, 1, DESIRED)
            .await
            .is_err()
    );
    let ResizeResult::Committed { status, .. } = f
        .resize(f.terminal.clone(), token, 0, DESIRED)
        .await
        .unwrap()
    else {
        panic!("Expected a committed target");
    };
    assert_eq!(status.epoch, Some(epoch));
    assert_eq!(status.committed.revision, 1);
    assert_eq!(status.committed.size, DESIRED);
    assert_eq!(status.observation, ObservationState::Pending);
    assert!(status.provider.is_none());

    let witness = f.status(&f.engine, f.terminal.clone()).await;
    assert_eq!(witness.committed, status.committed);
    assert_eq!(witness.provider.unwrap().size, ORIGINAL);
    assert_eq!(witness.observation, ObservationState::Pending);
    let command = fs::read_to_string(f.temp.path().join("last-write")).unwrap();
    let (guard, branch) = command.trim_end().split_once('\n').unwrap();
    assert!(guard.contains(&epoch_text(Some(epoch))));
    for field in [
        "@rs_create_generation",
        "@rs_resize_epoch",
        "pane_tty",
        "pane_pid",
        "after-resize-window",
        "after-set-option",
    ] {
        assert!(guard.contains(field), "{field}");
    }
    assert!(
        branch.starts_with(
            "resize-window -t @0 -x 117 -y 39 ; set-option -p -t %0 @rs_resize_intent "
        )
    );
    assert!(!branch.contains("wait-for") && !guard.contains("#("));
}

#[tokio::test]
async fn w1_resize_lost_replies_revoke_and_restart_recovers_from_sampled_predecessor() {
    for (mode, revision) in [("lost-before", 0), ("lost-after", 1)] {
        let f = Fixture::new();
        let (token, first) = f.acquire(f.terminal.clone()).await;
        fs::write(f.temp.path().join("mode"), mode).unwrap();
        let ResizeResult::Uncertain { intent, .. } = f
            .resize(f.terminal.clone(), token, 0, DESIRED)
            .await
            .unwrap()
        else {
            panic!("Expected an uncertain result");
        };
        assert_eq!(intent.epoch, first);
        assert!(
            f.resize(f.terminal.clone(), token, 1, DESIRED)
                .await
                .is_err()
        );
        let status = f.status(&f.engine, f.terminal.clone()).await;
        assert_eq!(status.committed.revision, revision);

        let (restarted, terminal) = f.restart();
        assert!(matches!(
            restarted
                .handle(
                    Request::TerminalStatus {
                        terminal: f.terminal.clone()
                    },
                    None
                )
                .await
                .unwrap(),
            Response::TerminalChanged {}
        ));
        let Response::TerminalControl { epoch, .. } = restarted
            .handle(
                Request::AcquireTerminal {
                    terminal: terminal.clone(),
                },
                None,
            )
            .await
            .unwrap()
        else {
            panic!("Expected new authority after reconciliation");
        };
        assert_ne!(epoch, first);
        let witness = f.status(&restarted, terminal).await;
        assert_eq!(witness.committed.revision, revision);
        assert_eq!(witness.provider.unwrap().size, ORIGINAL);
    }
}

#[tokio::test]
async fn w1_resize_same_size_and_aba_use_intent_identity_instead_of_dimension_equality() {
    let f = Fixture::new();
    let (token, _) = f.acquire(f.terminal.clone()).await;
    let mut previous = None;
    for (sequence, size) in [ORIGINAL, DESIRED, ORIGINAL].into_iter().enumerate() {
        let ResizeResult::Committed { status, .. } = f
            .resize(f.terminal.clone(), token, sequence as u64, size)
            .await
            .unwrap()
        else {
            panic!("Expected a committed resize");
        };
        assert_eq!(status.committed.revision, sequence as u64 + 1);
        assert_eq!(status.committed.size, size);
        assert_ne!(previous, status.committed.intent);
        previous = status.committed.intent;
    }
    let status = f.status(&f.engine, f.terminal.clone()).await;
    assert_eq!(status.provider.unwrap().size, ORIGINAL);
    assert_eq!(status.committed.revision, 3);
}

#[tokio::test]
async fn w1_resize_false_guard_and_obsolete_authority_are_rejected() {
    let f = Fixture::new();
    let (token, epoch) = f.acquire(f.terminal.clone()).await;
    fs::write(f.temp.path().join("mode"), "stale").unwrap();
    assert!(matches!(
        f.resize(f.terminal.clone(), token, 0, DESIRED)
            .await
            .unwrap(),
        ResizeResult::Rejected { .. }
    ));
    let (successor, new_epoch) = f.acquire(f.terminal.clone()).await;
    assert_ne!(new_epoch, epoch);
    assert_ne!(successor, token);
    assert!(
        f.resize(f.terminal.clone(), token, 1, DESIRED)
            .await
            .is_err()
    );
    let status = f.status(&f.engine, f.terminal.clone()).await;
    assert_eq!(status.committed.revision, 0);
    assert_eq!(status.epoch, Some(new_epoch));
}

#[tokio::test]
async fn w1_resize_reconciliation_waits_for_a_fresh_identity_bracketed_sample() {
    let f = Fixture::new();
    let before = f.engine.scoped_geometry(&f.terminal).await.unwrap();
    fs::write(f.temp.path().join("grid"), "115 35\n").unwrap();
    assert!(f.engine.sample_provider(&before).await.is_err());

    let set_grid = |size: TerminalSize| {
        let winsize = libc::winsize {
            ws_col: size.cols,
            ws_row: size.rows,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = f.pty.as_ref().expect("Resize fixture requires a real PTY");
        let applied =
            unsafe { libc::ioctl(pty.master.as_raw_fd(), libc::TIOCSWINSZ, &raw const winsize) };
        assert_eq!(applied, 0);
    };
    set_grid(TerminalSize { cols: 0, rows: 0 });
    let acquire = || Request::AcquireTerminal {
        terminal: f.terminal.clone(),
    };
    assert!(f.engine.handle(acquire(), None).await.is_err());
    assert!(
        f.engine
            .lane(f.terminal.target.runtime_id)
            .lock()
            .await
            .reconcile
    );
    assert!(
        f.status(&f.engine, f.terminal.clone())
            .await
            .provider
            .is_none()
    );

    set_grid(ORIGINAL);
    let (_, successor) = f.acquire(f.terminal.clone()).await;
    let current = f.status(&f.engine, f.terminal.clone()).await;
    assert_eq!(current.epoch, Some(successor));
    assert_ne!(current.provider.unwrap().size, current.committed.size);
    assert!(
        !f.engine
            .lane(f.terminal.target.runtime_id)
            .lock()
            .await
            .reconcile
    );
}
