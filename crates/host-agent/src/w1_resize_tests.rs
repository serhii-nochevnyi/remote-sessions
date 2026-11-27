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
            state: PaneState::Live,
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
def key(name, pane):
    return name if pane == "%0" else name + "-1"
def pane_row(pane):
    if pane == "%0" and not read("removed-0"):
        return 'PREFIX'.split("|")
    if pane == "%1" and not read("removed-1") and read("second-pane"):
        return read("second-pane").split("|")
    return None
def panes():
    return [(pane, row) for pane in ("%0", "%1") if (row := pane_row(pane))]
def check(guard, pane, write_input=False):
    row = pane_row(pane)
    if row is None:
        return False
    expected = {
        "pid": row[0], "start_time": row[1], "@rs_create_generation": row[2],
        "session_id": row[3], "window_id": row[4], "pane_id": row[5],
        "pane_pid": row[6], "session_name": row[7], "pane_tty": row[15],
        "@rs_resize_epoch": read(key("epoch", pane)),
        "@rs_paste_admission": read(key("marker", pane)),
        "@rs_paste_launch": read(key("launch", pane)),
    }
    match = (
        not read("scope") and not read("unsupported") and
        all(("#{{==:#{{{}}},{}}}".format(field, value)) in guard
            for field, value in expected.items())
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
    marker_file = key("marker", pane)
    assert paste[7:] == ["-t", pane] and "~a~" in read(marker_file)
    assert marker[5] == read(marker_file).replace("~a~", "~s~", 1)
    assert reply[:5] == ["display-message", "-p", "-l", "-t", pane]
    write(marker_file, marker[5])
    data = (root / paste[6]).read_bytes()
    if read("bracketed"):
        data = b"\x1b[200~" + data + b"\x1b[201~"
    append("pane" + pane[1:] + "-bytes", data)
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
if read("private-fixture"):
    import socket
    assert sys.argv[1:4] == ["-N", "-S", str(root / "tmux.sock")]
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as conn:
        conn.connect(sys.argv[3])
        assert conn.recv(1) == b"1"
command = args[0]
pane = args[args.index("-t") + 1] if "-t" in args else "%0"
mode = read("mode")
if command == "list-sessions":
    for name in dict.fromkeys(row[7] for _, row in panes()):
        print(name)
elif command == "list-panes":
    for pane, row in panes():
        cols, rows = read(key("grid", pane)).split()
        print(row[7], pane, row[6], 0, cols, rows, "fixture", sep="\t")
elif command == "list-commands":
    print("load-buffer (loadb) [-w] [-b buffer-name] path")
    flags = "dpr" if read("probe-mode") == "missing-flag" else "dprS"
    print(f"paste-buffer (pasteb) [-{flags}] [-b buffer-name] [-t target-pane]")
elif command == "display-message":
    if args[2] == "#{pid}|#{start_time}|#{@rs_create_generation}":
        print("987|123456|GENERATION")
        if args[3:]:
            assert args[3:] == [";", "list-panes", "-a", "-F", "#{pane_id}"]
            if read("absence-unconfirmed"):
                sys.exit(1)
            for listed, _ in panes():
                print(listed)
    else:
        append("geometry-targets", (pane + "\n").encode())
        row = pane_row(pane)
        if row is None:
            sys.exit(1)
        if read("scope"):
            row[5] = "%1"
        cols, rows = read(key("grid", pane)).split()
        print("|".join(row), cols, rows, cols, rows,
              read(key("epoch", pane)), read(key("intent", pane)), read(key("launch", pane)),
              read(key("marker", pane)), int(bool(read("input-off"))),
              int(not read("unsupported")), sep="|")
elif command == "save-buffer":
    assert args[1] == "-b" and args[2].startswith("rs-paste-probe-") and args[3:] == ["-"]
    data = (root / args[2]).read_bytes()
    sys.stdout.buffer.write(data[:-1] if read("probe-mode") == "truncated" else data)
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
    elif branch.startswith(("load-buffer", "list-buffers", "delete-buffer", "display-message")):
        nodes = [node.split() for node in branch.split(" ; ")]
        action, reply = nodes
        if action[0] == "display-message":
            action, reply = reply, action
        cleanup_marker = action[0] == "delete-buffer" and reply[0] == "set-option"
        if cleanup_marker:
            assert reply[:5] == ["set-option", "-p", "-t", pane, "@rs_paste_admission"]
        else:
            assert reply[:5] == ["display-message", "-p", "-l", "-t", pane]
        if not check(guard, pane):
            sys.exit()
        if action[0] == "load-buffer":
            _, flag, buffer, source = action
            assert (flag, source) == ("-b", "-") and buffer.startswith("rs-paste-")
            identifier = buffer.removeprefix("rs-paste-").removeprefix("probe-")
            assert re.fullmatch("[0-9a-f]{32}", identifier)
            (root / buffer).write_bytes(sys.stdin.buffer.read())
            write("last-load", buffer)
        elif action[0] == "list-buffers":
            assert action[1:4] == ["-F", "present", "-f"]
            buffer = re.fullmatch(r"'#\{==:#\{buffer_name\},(rs-paste-[0-9a-f]{32})\}'",
                                  action[4]).group(1)
            if (root / buffer).exists():
                print("present")
        elif action[0] == "delete-buffer":
            _, flag, buffer = action
            assert flag == "-b"
            if read("cleanup-blocked"):
                sys.exit(1)
            (root / buffer).unlink()
            append("deletions", (buffer + "\n").encode())
            if cleanup_marker:
                write(key("marker", pane), reply[-1])
                sys.exit()
        else:
            assert action == ["kill-session", "-t", "=" + pane_row(pane)[7]]
            write("removed-" + pane[1:], "1")
        print(reply[-1])
    elif " ; paste-buffer " in branch:
        append("submissions", b"1")
        write("last-effect-command", json.dumps(args))
        paste_mode = read("paste-mode")
        if paste_mode == "lost-before":
            sys.exit(1)
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
            append("pane" + pane[1:] + "-bytes", bytes.fromhex(" ".join(send_keys[4:])))
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
                write(key("epoch", pane), words[-1])
            elif words[-2] == "@rs_paste_launch":
                write(key("launch", pane), words[-1])
            elif words[-2] == "@rs_paste_admission":
                marker_file = key("marker", pane)
                ack_reply = read("ack-reply") if "~s~" in read(marker_file) and "~r~" in words[-1] else ""
                if ack_reply == "before":
                    sys.exit(1)
                write(marker_file, words[-1])
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

    pub(super) fn second_pane(&self, replacement: bool) -> (TerminalView, tokio::process::Child) {
        let child = Command::new("/bin/sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let runtime_id = if replacement {
            self.terminal.target.runtime_id
        } else {
            Uuid::new_v4()
        };
        let target = TerminalTarget {
            runtime_id,
            generation: self.terminal.target.generation,
            session: "$1".into(),
            window: "@1".into(),
            pane: "%1".into(),
            pid,
            process_start: process_start(pid).unwrap(),
        };
        let name = tmux_name(Uuid::new_v4(), runtime_id);
        let tty = "/dev/ttyssecond";
        let read = GeometryReadback {
            target: target.clone(),
            state: PaneState::Live,
            server: ServerGeneration {
                pid: 987,
                start: 123456,
                token: target.generation,
            },
            name: name.clone(),
            nonce: String::new(),
            tty: tty.into(),
            epoch: None,
            committed: CommittedGeometry {
                size: ORIGINAL,
                revision: 0,
                intent: None,
            },
            paste_launch: Uuid::new_v4().to_string(),
            paste_marker: String::new(),
            sender_supported: true,
            input_enabled: true,
        };
        let root = self.temp.path();
        fs::write(root.join("grid-1"), "100 30").unwrap();
        fs::write(root.join("launch-1"), &read.paste_launch).unwrap();
        fs::write(root.join("marker-1"), read.ready_marker(None)).unwrap();
        fs::write(
            root.join("second-pane"),
            format!(
                "987|123456|{}|$1|@1|%1|{pid}|{name}|0|0||0|1|1|manual|{tty}",
                target.generation
            ),
        )
        .unwrap();
        (
            TerminalView {
                target,
                agent_epoch: self.terminal.agent_epoch,
                view_id: Uuid::new_v4(),
            },
            child,
        )
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

#[cfg(debug_assertions)]
#[tokio::test]
async fn w1_resize_private_fixture_socket() {
    use std::os::unix::fs::{DirBuilderExt, symlink};
    use tokio::net::UnixListener;

    let f = Fixture::new();
    let root = f.temp.path();
    let private_dir = |name| {
        let directory = root.join(name);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        directory
    };
    fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
    let endpoint = private_dir("agent");
    let alias = private_dir("alias").join("tmux.sock");
    let exposed_directory = private_dir("exposed");
    let exposed = exposed_directory.join("tmux.sock");
    fs::set_permissions(&exposed_directory, fs::Permissions::from_mode(0o755)).unwrap();
    let _exposed_socket = UnixListener::bind(&exposed).unwrap();
    let tmux = UnixListener::bind(&f.engine.socket).unwrap();
    symlink(&f.engine.socket, &alias).unwrap();
    fs::write(root.join("private-fixture"), "1").unwrap();
    let server = tokio::spawn(async move {
        while let Ok((mut stream, _)) = tmux.accept().await {
            stream.write_all(b"1").await.unwrap();
        }
    });
    let mut wrong = f.terminal.target.clone();
    wrong.generation = Uuid::new_v4();
    for (directory, socket, target, expected) in [
        (
            &endpoint,
            &alias,
            &f.terminal.target,
            "private real tmux socket",
        ),
        (
            &endpoint,
            &exposed,
            &f.terminal.target,
            "private real tmux socket",
        ),
        (
            &endpoint,
            &f.engine.socket,
            &wrong,
            "fixture target or sender changed",
        ),
    ] {
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            crate::serve_resize_fixture(
                directory.clone(),
                f.engine.tmux.clone(),
                socket.clone(),
                target.clone(),
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(!endpoint.join("agent.sock").exists());
    }
    let error = crate::serve_resize_fixture(
        root.into(),
        f.engine.tmux.clone(),
        f.engine.socket.clone(),
        f.terminal.target.clone(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("isolated agent directory"));

    let agent = tokio::spawn(crate::serve_resize_fixture(
        endpoint.clone(),
        f.engine.tmux.clone(),
        f.engine.socket.clone(),
        f.terminal.target.clone(),
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !endpoint.join("agent.sock").exists() {
            assert!(!agent.is_finished());
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let blocked = crate::serve_resize_fixture(
        endpoint.clone(),
        f.engine.tmux.clone(),
        f.engine.socket.clone(),
        f.terminal.target.clone(),
    )
    .await
    .unwrap_err();
    assert!(blocked.to_string().contains("another agent already owns"));

    let open = |runtime_id| Request::OpenTerminal {
        runtime_id,
        view_id: Uuid::new_v4(),
    };
    assert!(matches!(
        crate::request(&endpoint, &open(Uuid::new_v4()))
            .await
            .unwrap(),
        Response::Error { .. }
    ));
    let Response::TerminalOpened { status } =
        crate::request(&endpoint, &open(f.terminal.target.runtime_id))
            .await
            .unwrap()
    else {
        panic!("Expected a view from the isolated fixture");
    };
    assert_eq!(status.terminal.target, f.terminal.target);
    let terminal = status.terminal;
    let mut wrong = terminal.clone();
    wrong.target.window = "@1".into();
    assert!(matches!(
        crate::request(&endpoint, &Request::AcquireTerminal { terminal: wrong })
            .await
            .unwrap(),
        Response::Error { .. }
    ));
    assert!(!root.join("last-write").exists());
    let Response::TerminalControl { token, epoch } = crate::request(
        &endpoint,
        &Request::AcquireTerminal {
            terminal: terminal.clone(),
        },
    )
    .await
    .unwrap() else {
        panic!("Expected the exact first writer grant");
    };
    assert!(!token.is_nil());
    let Response::TerminalStatus { status } = crate::request(
        &endpoint,
        &Request::TerminalStatus {
            terminal: terminal.clone(),
        },
    )
    .await
    .unwrap() else {
        panic!("Expected independent exact fixture readback");
    };
    assert_eq!(status.terminal, terminal);
    assert_eq!(status.epoch, Some(epoch));
    assert_eq!(status.provider.unwrap().size, ORIGINAL);
    assert!(f.engine.socket.exists());
    agent.abort();
    let _ = agent.await;
    server.abort();
    let _ = server.await;
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn w1_resize_exact_expiry_admission() {
    let mut f = Fixture::new();
    Arc::get_mut(&mut f.engine).unwrap().resize_fixture_target = Some(f.terminal.target.clone());
    let now = Instant::now();
    *f.engine.paste_test.now.lock().unwrap() = Some(now);
    let (token, epoch) = f.acquire(f.terminal.clone()).await;
    let resize = |terminal, token, sequence, size| Request::ResizeTerminal {
        terminal,
        token,
        sequence,
        size,
    };
    let exact = |sequence, size| resize(f.terminal.clone(), token, sequence, size);
    let exchange = async |resize_probe, epoch, attempt| {
        let frame = serde_json::to_vec(&crate::ResizeProbeRequest {
            resize_probe,
            epoch,
            attempt,
        })
        .unwrap();
        let response =
            crate::dispatch(&frame, &f.engine, &tokio::sync::Mutex::new(()), None).await?;
        Ok::<Response, anyhow::Error>(serde_json::from_slice(&response)?)
    };
    let deadline = now + INPUT_LEASE_TTL;
    *f.engine.paste_test.now.lock().unwrap() = Some(deadline - Duration::from_nanos(1));
    let Response::Resize {
        resize: ResizeResult::Committed { status: before, .. },
    } = exchange(exact(0, DESIRED), epoch, Uuid::new_v4())
        .await
        .unwrap()
    else {
        panic!("Exact resize before the deadline must commit");
    };
    let root = f.temp.path();
    let last_write = fs::read(root.join("last-write")).unwrap();
    let mut wrong_view = f.terminal.clone();
    wrong_view.view_id = Uuid::new_v4();
    let mut wrong_target = f.terminal.clone();
    wrong_target.target.generation = Uuid::new_v4();
    let wrong_epoch = ResizeEpoch {
        owner: Uuid::new_v4(),
        ..epoch
    };
    for now in [deadline, deadline + Duration::from_nanos(1)] {
        *f.engine.paste_test.now.lock().unwrap() = Some(now);
        for (request, claimed_epoch) in [
            (resize(wrong_view.clone(), token, 1, ORIGINAL), epoch),
            (resize(wrong_target.clone(), token, 1, ORIGINAL), epoch),
            (
                resize(f.terminal.clone(), Uuid::new_v4(), 1, ORIGINAL),
                epoch,
            ),
            (exact(1, ORIGINAL), wrong_epoch),
            (exact(0, ORIGINAL), epoch),
            (exact(2, ORIGINAL), epoch),
            (exact(1, TerminalSize { cols: 1, rows: 2 }), epoch),
        ] {
            assert!(
                exchange(request, claimed_epoch, Uuid::new_v4())
                    .await
                    .is_err()
            );
        }
        let lane = f
            .engine
            .existing_lane(f.terminal.target.runtime_id)
            .unwrap();
        let busy = lane.lock().await;
        assert!(
            exchange(exact(1, ORIGINAL), epoch, Uuid::new_v4())
                .await
                .is_err()
        );
        drop(busy);
        let attempt = Uuid::new_v4();
        assert!(matches!(
            exchange(exact(1, ORIGINAL), epoch, attempt).await.unwrap(),
            Response::Error { message } if message == format!("resize admission expired: {attempt}")
        ));
        assert_eq!(fs::read(root.join("last-write")).unwrap(), last_write);
        let after = f.status(&f.engine, f.terminal.clone()).await;
        assert_eq!(after.terminal, before.terminal);
        assert_eq!(after.epoch, before.epoch);
        assert_eq!(after.committed, before.committed);
        assert_eq!(after.provider.unwrap().size, ORIGINAL);
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

#[cfg(target_os = "macos")]
mod exited_stop {
    use super::*;

    const DRAFT: &[u8] = "fixture draft\n\t日本語".as_bytes();
    const CANARY: &str = "unrelated fixture buffer";
    const SCOPE: &str = "#{pid}|#{start_time}|#{@rs_create_generation}|#{session_id}|#{window_id}|#{pane_id}|#{pane_pid}|#{pane_dead}";
    const PARKED_SEND: &str = r#"#!/usr/bin/env python3
from pathlib import Path
import os
import subprocess
import sys

root = Path(__file__).parent
assert sys.argv[1:4] == ["-N", "-S", str(root / "tmux.sock")]
cli = ["tmux", *sys.argv[1:4]]
args = sys.argv[4:]
if args[0:3] == ["if-shell", "-F", "-t"]:
    branch = args[5]
    if " ; paste-buffer " in branch:
        with (root / "delayed-send").open("xb") as reply:
            subprocess.Popen([*cli, "display-message", "-p", "-l", "parked", ";",
                              "wait-for", "-L", "old-paste", ";", *args],
                             stdin=subprocess.DEVNULL, stdout=reply, stderr=reply,
                             start_new_session=True)
        sys.exit(1)
    if branch.startswith(("list-buffers ", "delete-buffer ")) and (root / "block-cleanup").is_file():
        sys.exit(1)
    if branch.startswith("delete-buffer ") and (root / "lose-cleanup-reply").is_file():
        result = subprocess.run([*cli, *args], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL)
        if result.returncode == 0:
            (root / "cleanup-reply-lost").touch()
        sys.exit(1)
os.execvp(cli[0], [*cli, *args])
"#;

    struct PrivateTmux {
        temp: tempfile::TempDir,
        stock: Engine,
        canary: String,
        survivor: String,
    }

    impl PrivateTmux {
        async fn start() -> Self {
            let temp = tempfile::tempdir_in("/tmp").unwrap();
            let root = temp.path();
            fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
            let config = root.join("tmux.conf");
            let socket = root.join("tmux.sock");
            fs::write(&config, include_str!("../../../config/tmux.conf")).unwrap();
            let wrapper = root.join("tmux-fixture");
            fs::write(&wrapper, PARKED_SEND).unwrap();
            fs::set_permissions(wrapper, fs::Permissions::from_mode(0o700)).unwrap();

            let output = Command::new("tmux")
                .arg("-S")
                .arg(&socket)
                .arg("-f")
                .arg(config)
                .args([
                    "new-session",
                    "-d",
                    "-s",
                    "survivor",
                    "--",
                    "/bin/sh",
                    "-c",
                    "/bin/stty raw -echo; exec /bin/cat > \"$1\"",
                    "fixture",
                ])
                .arg(root.join("survivor-input"))
                .env_remove("TMUX")
                .stdin(Stdio::null())
                .output()
                .await
                .unwrap();

            let mut fixture = Self {
                temp,
                stock: Engine::new("tmux".into(), socket),
                canary: format!("rs-paste-{}", Uuid::new_v4().simple()),
                survivor: String::new(),
            };
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            fixture.wait_file("survivor-input").await;
            fixture.survivor = fixture.scope("=survivor:0.0").await;
            fixture
                .tmux(&["set-buffer", "-b", &fixture.canary, CANARY])
                .await;
            fixture
        }

        fn engine(&self) -> Engine {
            let engine = Engine::new(
                self.temp.path().join("tmux-fixture"),
                self.stock.socket.clone(),
            );
            *engine.paste_test.now.lock().unwrap() = Some(Instant::now());
            engine
        }

        async fn tmux(&self, args: &[&str]) -> String {
            self.stock.command(args).await.unwrap()
        }

        async fn scope(&self, pane: &str) -> String {
            self.tmux(&["display-message", "-p", "-t", pane, SCOPE])
                .await
        }

        async fn wait_file(&self, name: &str) {
            tokio::time::timeout(Duration::from_secs(3), async {
                while !self.temp.path().join(name).is_file() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }

        async fn create(&mut self, engine: &Engine) -> Runtime {
            let response = engine
                .handle(
                    Request::Create {
                        session_id: Uuid::new_v4(),
                        runtime_id: Uuid::new_v4(),
                        cwd: self.temp.path().into(),
                        argv: vec!["/bin/sh".into(), "-c".into(), "read -r line".into()],
                    },
                    None,
                )
                .await
                .unwrap();
            let Response::Created { runtime } = response else {
                panic!("Expected a runtime, got {response:?}");
            };
            self.survivor = self.scope("=survivor:0.0").await;
            runtime
        }

        async fn exit(&self, engine: &Engine, runtime: &Runtime) -> Runtime {
            self.tmux(&["send-keys", "-t", &runtime.pane_id, "Enter"])
                .await;
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let retained = engine.find(runtime.runtime_id).await.unwrap();
                    if retained.exited {
                        return retained;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("The real pane must exit and be retained")
        }

        async fn marker(&self, pane: &str) -> String {
            self.tmux(&[
                "display-message",
                "-p",
                "-t",
                pane,
                "#{@rs_paste_admission}",
            ])
            .await
            .trim_end()
            .into()
        }

        async fn set_marker(&self, pane: &str, marker: &str) {
            self.tmux(&["set-option", "-p", "-t", pane, PASTE_OPTION, marker])
                .await;
        }

        async fn assert_buffer(&self, name: &str, expected: Option<&[u8]>) {
            let names = self.tmux(&["list-buffers", "-F", "#{buffer_name}"]).await;
            assert_eq!(names.lines().any(|item| item == name), expected.is_some());
            if let Some(bytes) = expected {
                assert_eq!(
                    self.tmux(&["save-buffer", "-b", name, "-"])
                        .await
                        .as_bytes(),
                    bytes
                );
            }
        }

        async fn assert_survivor(&self) {
            assert_eq!(self.scope("=survivor:0.0").await, self.survivor);
            self.assert_buffer(&self.canary, Some(CANARY.as_bytes()))
                .await;
            assert!(
                fs::read(self.temp.path().join("survivor-input"))
                    .unwrap()
                    .is_empty()
            );
        }

        async fn stop(&self, engine: &Engine, runtime: &Runtime) -> Result<Response> {
            engine
                .handle(
                    Request::Stop {
                        runtime_id: runtime.runtime_id,
                        expected_pane_id: runtime.pane_id.clone(),
                    },
                    None,
                )
                .await
        }

        fn block_cleanup(&self, blocked: bool) {
            let flag = self.temp.path().join("block-cleanup");
            if blocked {
                fs::write(flag, "").unwrap();
            } else {
                fs::remove_file(flag).unwrap();
            }
        }

        async fn assert_stopped(&self, runtime: &Runtime, buffer: &PasteBuffer) {
            let panes = self.tmux(&["list-panes", "-a", "-F", "#{pane_id}"]).await;
            assert!(!panes.lines().any(|pane| pane == runtime.pane_id));
            let sessions = self.tmux(&["list-sessions", "-F", "#{session_name}"]).await;
            assert_eq!(sessions.trim_end(), "survivor");
            self.assert_buffer(&buffer.name, None).await;
            self.assert_survivor().await;
        }
    }

    impl Drop for PrivateTmux {
        fn drop(&mut self) {
            let _ = std::process::Command::new(&self.stock.tmux)
                .args(["-N", "-S"])
                .arg(&self.stock.socket)
                .arg("kill-server")
                .env_remove("TMUX")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    #[tokio::test]
    async fn w1_resize_exited_stop_reclaims_only_bound_paste() {
        let mut fixture = PrivateTmux::start().await;
        let engine = fixture.engine();
        let runtime = fixture.create(&engine).await;
        let pane = &runtime.pane_id;
        let read = engine.geometry(runtime.runtime_id, pane).await.unwrap();
        let terminal = TerminalView {
            target: read.target,
            view_id: Uuid::new_v4(),
            agent_epoch: engine.agent_epoch,
        };
        let Response::TerminalControl { token, epoch } = engine
            .handle(
                Request::AcquireTerminal {
                    terminal: terminal.clone(),
                },
                None,
            )
            .await
            .unwrap()
        else {
            panic!("Expected a real pane writer");
        };
        let binding = PasteBinding {
            terminal,
            token,
            epoch,
            incarnation: Uuid::new_v4(),
            operation: 1,
            sequence: 0,
        };
        assert!(matches!(
            engine
                .handle(
                    Request::PasteStage {
                        binding: binding.clone(),
                        offset: 0,
                        data: DRAFT.into()
                    },
                    None
                )
                .await
                .unwrap(),
            Response::Paste {
                paste: PasteResult::Staged {}
            }
        ));
        let commit = || Request::PasteCommit {
            binding: binding.clone(),
            bytes: DRAFT.len(),
        };
        fixture.tmux(&["wait-for", "-L", "old-paste"]).await;
        assert!(matches!(
            engine.handle(commit(), None).await.unwrap(),
            Response::Paste {
                paste: PasteResult::Unknown {}
            }
        ));
        let reply = fixture.temp.path().join("delayed-send");
        tokio::time::timeout(Duration::from_secs(3), async {
            while fs::read_to_string(&reply).unwrap_or_default() != "parked\n" {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("The original sender must be queued before exit");
        let armed = engine.geometry(runtime.runtime_id, pane).await.unwrap();
        let buffer = PasteBuffer::from_marker(&armed).unwrap();
        assert_eq!(armed.paste_marker, buffer.marker(&armed, 'a'));
        fixture.assert_buffer(&buffer.name, Some(DRAFT)).await;

        let retained = fixture.exit(&engine, &runtime).await;
        let scope = fixture.scope(pane).await;
        let screen = fixture.tmux(&["capture-pane", "-p", "-t", pane]).await;
        let foreign = armed.paste_marker.replace(&buffer.name, &fixture.canary);
        fixture.set_marker(pane, &foreign).await;
        assert!(fixture.stop(&engine, &retained).await.is_err());
        assert_eq!(fixture.marker(pane).await, foreign);
        assert_eq!(fixture.scope(pane).await, scope);
        fixture.assert_buffer(&buffer.name, Some(DRAFT)).await;
        fixture.assert_survivor().await;
        fixture.set_marker(pane, &armed.paste_marker).await;
        fixture.block_cleanup(true);
        assert!(fixture.stop(&engine, &retained).await.is_err());
        assert_eq!(fixture.scope(pane).await, scope);
        assert_eq!(fixture.marker(pane).await, buffer.marker(&armed, 'q'));

        fixture.tmux(&["wait-for", "-U", "old-paste"]).await;
        let rejected = format!("parked\nrs-paste-rejected-{}\n", buffer.nonce);
        tokio::time::timeout(Duration::from_secs(3), async {
            while fs::read_to_string(&reply).unwrap() != rejected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("The queued guard must reject while the original pane still exists");
        let mut loading = armed;
        loading.paste_marker = buffer.marker(&loading, 'l');
        assert!(
            engine
                .load_paste(&loading, &buffer.name, b"stale load")
                .await
                .is_err()
        );
        fixture.assert_buffer(&buffer.name, Some(DRAFT)).await;
        assert_eq!(
            fixture.tmux(&["capture-pane", "-p", "-t", pane]).await,
            screen
        );
        fixture.block_cleanup(false);
        fixture.assert_survivor().await;

        assert!(
            matches!(fixture.stop(&engine, &retained).await.unwrap(), Response::Stopped { runtime_id } if runtime_id == runtime.runtime_id)
        );
        assert!(matches!(
            engine.handle(commit(), None).await.unwrap(),
            Response::Paste {
                paste: PasteResult::Stale {}
            }
        ));
        assert!(
            engine
                .handle(
                    Request::Input {
                        runtime_id: runtime.runtime_id,
                        token,
                        sequence: 0,
                        data: vec![b'K'],
                        paste: false,
                    },
                    None,
                )
                .await
                .is_err()
        );
        fixture.assert_stopped(&retained, &buffer).await;
    }

    #[tokio::test]
    async fn w1_resize_exited_stop_recovers_paste_custody_after_restart() {
        let mut fixture = PrivateTmux::start().await;
        let engine = fixture.engine();
        let runtime = fixture.create(&engine).await;
        let pane = &runtime.pane_id;
        let legacy = engine.geometry(runtime.runtime_id, pane).await.unwrap();
        let identity = PaneIdentity {
            session: legacy.target.session.clone(),
            window: legacy.target.window.clone(),
            pane: pane.clone(),
            pid: legacy.target.pid,
        };
        let session = format!("={}", runtime.tmux_name);
        let nonce = Uuid::new_v4();
        let stamp = format!(
            "{nonce}:{}:{}:{pane}:{}",
            identity.session, identity.window, identity.pid
        );
        fixture
            .tmux(&[
                "set-environment",
                "-t",
                &session,
                NONCE_ENV,
                &nonce.to_string(),
                ";",
                "set-option",
                "-p",
                "-t",
                pane,
                PANE_OPTION,
                &stamp,
                ";",
                "set-option",
                "-pu",
                "-t",
                pane,
                PASTE_LAUNCH_OPTION,
                ";",
                "set-option",
                "-pu",
                "-t",
                pane,
                PASTE_OPTION,
            ])
            .await;
        engine
            .stamp_paste(&runtime, legacy.server, &identity, Some(nonce))
            .await
            .unwrap();
        let ready = engine.geometry(runtime.runtime_id, pane).await.unwrap();
        let ready = engine
            .rotate_epoch(
                &ready,
                ResizeEpoch {
                    agent: engine.agent_epoch,
                    owner: Uuid::new_v4(),
                },
            )
            .await
            .unwrap();
        let buffer = PasteBuffer {
            nonce: Uuid::new_v4(),
            operation: 1,
            name: format!("rs-paste-{}", Uuid::new_v4().simple()),
        };
        let loading = engine
            .transition_marker(&ready, &buffer.marker(&ready, 'l'))
            .await
            .unwrap()
            .unwrap();
        engine
            .load_paste(&loading, &buffer.name, DRAFT)
            .await
            .unwrap();
        let retained = fixture.exit(&engine, &runtime).await;
        let scope = fixture.scope(pane).await;
        let old_epoch = engine.agent_epoch;
        drop(engine);
        let engine = fixture.engine();
        assert_ne!(engine.agent_epoch, old_epoch);
        assert!(engine.existing_lane(runtime.runtime_id).is_none());
        assert_eq!(fixture.marker(pane).await, loading.paste_marker);
        fixture.assert_buffer(&buffer.name, Some(DRAFT)).await;

        let foreign = loading.paste_marker.replace(&buffer.name, &fixture.canary);
        let foreign = foreign.replace(
            &legacy.server.token.to_string(),
            &Uuid::new_v4().to_string(),
        );
        for marker in ["malformed".to_owned(), foreign] {
            fixture.set_marker(pane, &marker).await;
            assert!(fixture.stop(&engine, &retained).await.is_err());
            assert_eq!(fixture.marker(pane).await, marker);
            assert_eq!(fixture.scope(pane).await, scope);
            fixture.assert_buffer(&buffer.name, Some(DRAFT)).await;
            fixture.assert_survivor().await;
        }
        fixture.set_marker(pane, &loading.paste_marker).await;
        let replacement_generation = Uuid::new_v4().to_string();
        fixture
            .tmux(&[
                "set-option",
                "-s",
                GENERATION_OPTION,
                &replacement_generation,
            ])
            .await;
        let replaced_scope = fixture.scope(pane).await;
        assert_ne!(replaced_scope, scope);
        assert!(fixture.stop(&engine, &retained).await.is_err());
        assert_eq!(fixture.marker(pane).await, loading.paste_marker);
        assert_eq!(fixture.scope(pane).await, replaced_scope);
        fixture.assert_buffer(&buffer.name, Some(DRAFT)).await;
        fixture
            .assert_buffer(&fixture.canary, Some(CANARY.as_bytes()))
            .await;
        fixture
            .tmux(&[
                "set-option",
                "-s",
                GENERATION_OPTION,
                &legacy.server.token.to_string(),
            ])
            .await;
        fixture
            .tmux(&[
                "set-environment",
                "-t",
                &session,
                NONCE_ENV,
                &Uuid::new_v4().to_string(),
            ])
            .await;
        assert!(fixture.stop(&engine, &retained).await.is_err());
        assert_eq!(fixture.marker(pane).await, loading.paste_marker);
        assert_eq!(fixture.scope(pane).await, scope);
        fixture.assert_buffer(&buffer.name, Some(DRAFT)).await;
        fixture
            .tmux(&[
                "set-environment",
                "-t",
                &session,
                NONCE_ENV,
                &nonce.to_string(),
            ])
            .await;

        fixture.block_cleanup(true);
        assert!(fixture.stop(&engine, &retained).await.is_err());
        let fenced = buffer.marker(&loading, 'f');
        assert_eq!(fixture.marker(pane).await, fenced);
        assert_eq!(fixture.scope(pane).await, scope);
        assert!(
            engine
                .transition_marker(&loading, &buffer.marker(&loading, 'a'))
                .await
                .unwrap()
                .is_none()
        );
        fixture.assert_buffer(&buffer.name, Some(DRAFT)).await;
        fixture.assert_survivor().await;
        fixture.block_cleanup(false);
        fixture.tmux(&["delete-buffer", "-b", &buffer.name]).await;
        let engine = fixture.engine();
        assert!(fixture.stop(&engine, &retained).await.is_err());
        assert_eq!(fixture.marker(pane).await, fenced);
        assert_eq!(fixture.scope(pane).await, scope);
        fixture.assert_buffer(&buffer.name, None).await;
        fixture.assert_survivor().await;

        fixture
            .tmux(&[
                "set-buffer",
                "-b",
                &buffer.name,
                std::str::from_utf8(DRAFT).unwrap(),
            ])
            .await;
        fs::write(fixture.temp.path().join("lose-cleanup-reply"), "").unwrap();
        assert!(
            matches!(fixture.stop(&engine, &retained).await.unwrap(), Response::Stopped { runtime_id } if runtime_id == runtime.runtime_id)
        );
        assert!(fixture.temp.path().join("cleanup-reply-lost").is_file());
        fixture.assert_stopped(&retained, &buffer).await;
    }
}
