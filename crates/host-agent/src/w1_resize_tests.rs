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
    pub(super) fn new() -> Self {
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
            diagnostic: None,
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
import shlex
import socket
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
    return name if pane == "%0" else name + "-" + pane[1:]
def pane_row(pane):
    if pane == "%0" and not read("removed-0"):
        return 'PREFIX'.split("|")
    if pane == "%1" and not read("removed-1") and read("second-pane"):
        return read("second-pane").split("|")
    if pane == "%2" and not read("removed-2") and read("canary-pane"):
        return read("canary-pane").split("|")
    return None
def panes():
    return [(pane, row) for pane in ("%0", "%1", "%2") if (row := pane_row(pane))]
def options():
    return [f"@rs_paste_custody_987_123456_{'GENERATION'.replace('-', '')}_{i:02}" for i in range(16)]
def custody_path(option):
    assert option in options()
    return root / "custody" / option
def custody_value(value):
    assert len(value) <= 2048 and re.fullmatch(r"[a-zA-Z0-9:/_~-]+", value)
    version, nonce, state, *fields = value.split("~")
    assert version in ("v1", "v2") and re.fullmatch(r"[0-9a-f-]{36}", nonce)
    if state == "free":
        assert version == "v1" and not fields
    elif version == "v2":
        assert state in ("possible", "completed") and len(fields) == 17
        assert fields[0] == "probe" and fields[2:5] == ["GENERATION", "987", "123456"]
        assert fields[-1] == "rs-paste-probe-" + fields[-2].replace("-", "")
    else:
        assert state in ("possible", "completed") and len(fields) == 19
        assert fields[2] == "v1" and fields[4:7] == ["GENERATION", "987", "123456"]
        assert fields[-4] == "l" and re.fullmatch(r"rs-paste-[0-9a-f]{32}", fields[-1])
    return nonce, state, fields
def allows(guard, expected):
    seen = set()
    def compare(match):
        operator, field, value = match.groups()
        assert field in expected
        seen.add(field)
        return str(int((expected[field] == value) == (operator == "==")))
    guard = re.sub(r"#\{([!=]=):#\{([^}]+)\},([^}]*)\}", compare, guard)
    assert seen == expected.keys()
    while "#{" in guard:
        reduced = re.sub(r"#\{&&:([01]),([01])\}", lambda m: str(int(m[1] == m[2] == "1")), guard)
        assert reduced != guard
        guard = reduced
    return guard == "1"
def custody_server():
    return {"pid": "987", "start_time": "123456", "@rs_create_generation": "GENERATION",
            "after-if-shell": "", "after-display-message": "", "command-error": ""}
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
    if "#{==:#{after-kill-session},}" in guard:
        expected.update({
            "window_index": row[8], "pane_index": row[9], "@rs_create_nonce": row[10],
            "pane_dead": row[11], "session_windows": row[12], "window_panes": row[13],
            "window-size": row[14], "after-if-shell": "", "after-set-option": "",
            "after-resize-window": "", "after-display-message": "", "after-kill-session": "",
            "command-error": "",
        })
        return not read("scope") and allows(guard, expected)
    slots = re.findall(r"#\{[!=]=:#\{(@rs_paste_custody_[^}]+)\},", guard)
    if slots or "#{==:#{after-if-shell},}" in guard:
        if slots:
            option, = slots
            path = custody_path(option)
            expected[option] = path.read_text() if path.exists() else ""
        expected.update(custody_server(), **{
            "window_index": row[8], "pane_index": row[9], "@rs_create_nonce": row[10],
            "pane_dead": row[11], "session_windows": row[12], "window_panes": row[13],
            "window-size": row[14], "version": "3.7" if read("unsupported") else "3.8",
            "socket_path": str(root / "tmux.sock"),
        })
        for hook in ("load-buffer", "save-buffer", "paste-buffer", "delete-buffer",
                     "send-keys", "set-option", "resize-window"):
            expected["after-" + hook] = ""
        if "after-list-buffers" in guard:
            expected["after-list-buffers"] = ""
        if "#{==:#{@rs_create_initializer},}" in guard:
            expected.update({"@rs_create_initializer": "", "@rs_create_birth": "",
                             "@rs_diagnostic_create": ""})
        if write_input:
            expected["pane_input_off"] = "1" if read("input-off") else "0"
        return not read("scope") and allows(guard, expected)
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
        print(row[7], pane, row[6], row[11], cols, rows, "fixture", sep="\t")
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
        if args[2] == "-t" and args[4:] and ";" in args:
            row = pane_row(pane)
            assert row is not None and row[10] == ""
            name = row[7]
            fmt = "#{pid}|#{start_time}|#{@rs_create_generation}|#{session_id}|#{window_id}|#{pane_id}|#{pane_pid}|#{session_name}|#{pane_dead}|#{@rs_create_nonce}"
            assert args[4:] == [fmt, ";", "show-environment", "-s", "-t", "=" + name,
                                ";", "show-options", "-p", "-t", pane, ";",
                                "display-message", "-p", "-t", pane, fmt]
            value = "|".join(row[:8] + [row[11], row[10]])
            print(value)
            print(value)
            sys.exit()
        if (root / "park-resize-lane").exists():
            (root / "park-resize-lane").unlink()
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as gate:
                gate.connect(str(root / "resize-lane.sock"))
                gate.sendall(b"P")
                assert gate.recv(1) == b"R"
        append("geometry-targets", (pane + "\n").encode())
        row = pane_row(pane)
        if row is None:
            sys.exit(1)
        if read("scope"):
            row[5] = "%1"
        cols, rows = read(key("grid", pane)).split()
        print("|".join(row), cols, rows, cols, rows,
              read("readback-epoch") or read(key("epoch", pane)),
              read(key("intent", pane)), read(key("launch", pane)),
              read(key("marker", pane)), int(bool(read("input-off"))),
              int(not read("unsupported")), sep="|")
elif command == "save-buffer":
    assert args[1] == "-b" and args[2].startswith("rs-paste-probe-") and args[3:] == ["-"]
    data = (root / args[2]).read_bytes()
    sys.stdout.buffer.write(data[:-1] if read("probe-mode") == "truncated" else data)
elif command == "if-shell":
    if args[1:2] == ["-F"] and args[2] != "-t":
        _, _, guard, branch = args
        expected = custody_server()
        expected["after-show-options"] = ""
        if not allows(guard, expected):
            sys.exit()
        nodes = [shlex.split(node) for node in branch.split(" ; ")]
        assert nodes[-1] == ["display-message", "-p", "-l", "rs-custody-snapshot"]
        assert len(nodes) == 17
        row = "#{option_name} #{?#{m/r:^[a-zA-Z0-9:/_~-]+$,#{option_value}},#{option_value},invalid}"
        for words, option in zip(nodes[:-1], options()):
            assert words == ["show-options", "-sq", "-F", row, option]
        mode = read("custody-snapshot")
        if mode == "missing" or (mode == "hide-write" and read("custody-writes")):
            sys.exit()
        if mode == "over-budget":
            print("a" * 40961)
        for option in options():
            path = custody_path(option)
            if path.is_file():
                value = path.read_text()
                value = value if re.fullmatch(r"[a-zA-Z0-9:/_~-]+", value) else "invalid"
                print(option, value)
                if mode == "duplicate-row":
                    print(option, value)
        print("rs-custody-snapshot")
        sys.exit()
    _, flags, target, pane, guard, branch, *otherwise = args
    assert (flags, target) == ("-F", "-t")
    write("last-write", guard + "\n" + branch + "\n")
    if "set-option -s" in branch and "@rs_paste_custody_" in branch:
        nodes = branch.split(" ; ")
        command, flag, option, value = nodes[-1].split()
        assert command == "set-option" and flag in ("-s", "-so")
        path = custody_path(option)
        nonce, state, fields = custody_value(value)
        assert len(otherwise) == int(state == "possible")
        if otherwise:
            shell, flag_arg, target, target_pane, changed, witness = shlex.split(otherwise[0])
            assert (shell, flag_arg, target, target_pane) == ("if-shell", "-F", "-t", pane)
            changed, witness = (
                re.sub(r"\\([0-7]{3})", lambda m: chr(int(m[1], 8)), word)
                for word in (changed, witness)
            )
            assert changed == guard.replace(f"#{{==:#{{{option}}},", f"#{{!=:#{{{option}}},")
            reply = f"rs-paste-custody-conflict-{nonce}"
            assert witness == f"display-message -p -l -t {pane} {reply}"
        if not check(guard, pane):
            if otherwise and check(changed, pane):
                append("custody-conflicts", (pane + "\n").encode())
                print(reply)
            sys.exit()
        prior = path.read_text() if path.exists() else None
        if flag == "-so" and prior is not None:
            sys.exit(1)
        if prior is not None:
            old_nonce, old_state, old_fields = custody_value(prior)
            assert nonce != old_nonce
            assert (old_state, state) in (("free", "possible"), ("possible", "completed"),
                                          ("possible", "free"), ("completed", "free"))
            if state == "completed":
                assert old_fields == fields
        else:
            assert flag == "-so" and state == "possible"
        if state == "possible":
            row = pane_row(pane)
            if fields[0] == "probe":
                assert fields[1] == row[7] and fields[5:9] == [identity[1:] for identity in row[3:6]] + [row[6]]
                assert fields[10] == row[15] and fields[-3] == "local"
                assert not read(key("marker", pane)) and not read(key("launch", pane))
            else:
                assert fields[0] == row[7] and fields[1] == row[15]
                assert "~".join(fields[2:]) == read(key("marker", pane))
        if read("custody-write") == "before":
            sys.exit(1)
        if len(nodes) == 2:
            assert state == "free" and nodes[0] == f"delete-buffer -b {old_fields[-1]}"
            if read("cleanup-blocked"):
                sys.exit(1)
            (root / old_fields[-1]).unlink()
            append("deletions", (old_fields[-1] + "\n").encode())
        else:
            assert len(nodes) == 1
        path.parent.mkdir(exist_ok=True)
        with path.open("x" if flag == "-so" else "w") as out:
            out.write(value)
        append("custody-writes", (option + "\n").encode())
        if read("custody-write") == "lost" or (len(nodes) == 2 and read("lose-cleanup-reply")):
            sys.exit(1)
    elif branch.startswith("resize-window"):
        append("resize-dispatches", b"1")
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
        if action[0] == "load-buffer" and not action[2].startswith("rs-paste-probe-"):
            append("paste-load-attempts", (pane + "\n").encode())
        if not check(guard, pane):
            sys.exit()
        if action[0] == "load-buffer":
            _, flag, buffer, source = action
            assert (flag, source) == ("-b", "-") and buffer.startswith("rs-paste-")
            identifier = buffer.removeprefix("rs-paste-").removeprefix("probe-")
            assert re.fullmatch("[0-9a-f]{32}", identifier)
            if read("load-mode") == "before":
                sys.exit(1)
            slots = re.findall(r"#\{==:#\{(@rs_paste_custody_[^}]+)\},", guard)
            option, = slots
            _, state, fields = custody_value(custody_path(option).read_text())
            assert state == "possible" and fields[-1] == buffer
            (root / buffer).write_bytes(sys.stdin.buffer.read())
            write("last-load", buffer)
            if buffer.startswith("rs-paste-probe-"):
                append("probe-loads", (pane + "\n").encode())
            else:
                append("paste-loads", (pane + "\n").encode())
            if read("load-mode") == "lost":
                sys.exit(1)
        elif action[0] == "list-buffers":
            assert action[1:4] == ["-F", "present", "-f"]
            buffer = re.fullmatch(r"'#\{==:#\{buffer_name\},(rs-paste-(?:probe-)?[0-9a-f]{32})\}'",
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
        if not check(guard, pane, "@rs_paste_launch" in branch):
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
        let script = script
            .replace("PREFIX", &prefix)
            .replace("GENERATION", &generation.to_string());
        let status = std::process::Command::new("python3")
            .args([
                "-c",
                r#"from pathlib import Path
import sys

path = Path(sys.argv[1])
path.write_text(sys.argv[2], encoding="utf-8")
path.chmod(0o700)
"#,
            ])
            .arg(&tmux)
            .arg(script)
            .status()
            .unwrap();
        assert!(status.success(), "Fixture script creation failed: {status}");
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
            diagnostic: None,
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

async fn configured_test_catalog(root: &Path) -> (PathBuf, CatalogLane) {
    let path = root.canonicalize().unwrap().join("catalog");
    crate::catalog::provision(&path).unwrap();
    let directory = path.clone();
    let lane = CatalogLane::start_fixture(
        move || Ok(directory),
        crate::product_launch::ServiceProfiles::new(None),
        None,
    )
    .await
    .unwrap();
    (path, lane)
}

#[derive(Clone, Copy)]
enum PasteRefusal {
    Continuation,
    Commit,
    Scope,
    Epoch,
}

async fn paste_ipc(root: &Path, request: Request) -> PasteResult {
    match crate::request(root, &request).await.unwrap() {
        Response::Paste { paste } => paste,
        reply => panic!("expected a scoped paste response, got {reply:?}"),
    }
}

async fn paste_while_catalog_busy(
    root: &Path,
    catalog: &CatalogLane,
    requests: impl IntoIterator<Item = Request>,
) {
    use std::{future::poll_fn, task::Poll};

    let (entered, waiting) = tokio::sync::oneshot::channel();
    let (release, parked) = std::sync::mpsc::channel();
    let mut active = std::pin::pin!(catalog.run(move |_, _| {
        entered.send(()).unwrap();
        parked
            .recv_timeout(SEND_WATCHDOG)
            .map_err(|_| ProductIssue::WorkspaceCheckTimedOut)
    }));
    assert!(
        poll_fn(|cx| Poll::Ready(active.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    tokio::time::timeout(SEND_WATCHDOG, waiting)
        .await
        .unwrap()
        .unwrap();

    // @contract: A parked worker and four enqueued reads make authorization fail through the real lane.
    let read = || {
        catalog.run(|db, _| {
            db.diagnostic(Uuid::nil(), Uuid::nil())
                .map(|row| row.is_none())
        })
    };
    let mut queued: Vec<_> = (0..4).map(|_| Box::pin(read())).collect();
    for read in &mut queued {
        assert!(
            poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx)))
                .await
                .is_pending()
        );
    }
    assert_eq!(read().await, Err(ProductIssue::CatalogBusy));
    tokio::time::timeout(Duration::from_secs(2), async {
        for request in requests {
            assert_eq!(paste_ipc(root, request).await, PasteResult::Unavailable {});
        }
    })
    .await
    .expect("paste reply must arrive while the catalog worker is parked");
    assert_eq!(read().await, Err(ProductIssue::CatalogBusy));
    release.send(()).unwrap();
    active.await.unwrap();
    for read in queued {
        assert!(read.await.unwrap());
    }
    assert!(read().await.unwrap());
}

async fn configured_paste_refusal(refusal: PasteRefusal) {
    let Fixture {
        engine,
        terminal,
        temp,
        pty: _pty,
    } = Fixture::new();
    let root = temp.path();
    let (_, catalog) = configured_test_catalog(root).await;
    fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
    let guard = Arc::new(crate::AgentLock::acquire(root).unwrap());
    let socket = guard.socket.clone();
    let mut agent = tokio::task::JoinSet::new();
    agent.spawn(crate::serve_local(
        Arc::into_inner(engine).unwrap(),
        guard,
        Some(catalog.clone()),
    ));
    let request = async |message| crate::request(root, &message).await.unwrap();
    let health = || request(Request::Health {});
    let initial_health = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::select! {
            reply = async {
                while !socket.exists() {
                    tokio::task::yield_now().await;
                }
                health().await
            } => reply,
            done = agent.join_next() => panic!("paste fixture exited before Health: {done:?}"),
        }
    })
    .await
    .expect("private paste fixture Health timed out");
    let Response::Health {
        agent_pid,
        remote_execution_enabled: false,
    } = initial_health
    else {
        panic!("expected local-only Health: {initial_health:?}");
    };
    let runtime_id = terminal.target.runtime_id;
    let Response::TerminalOpened { status } = request(Request::OpenTerminal {
        runtime_id,
        view_id: terminal.view_id,
    })
    .await
    else {
        panic!("expected the configured legacy view");
    };
    assert_eq!(status.terminal, terminal);
    let started = Instant::now();
    let Response::TerminalControl { token, epoch } = request(Request::AcquireTerminal {
        terminal: terminal.clone(),
    })
    .await
    else {
        panic!("expected one healthy configured writer grant");
    };
    assert_eq!(epoch.agent, terminal.agent_epoch);
    let old = PasteBinding {
        terminal: terminal.clone(),
        token,
        epoch,
        incarnation: Uuid::new_v4(),
        operation: 2,
        sequence: 0,
    };
    let stage = |binding: &PasteBinding, offset, data: &[u8]| Request::PasteStage {
        binding: binding.clone(),
        offset,
        data: data.to_vec(),
    };
    let commit = |binding: &PasteBinding, bytes| Request::PasteCommit {
        binding: binding.clone(),
        bytes,
    };
    let file = |name| fs::read(root.join(name)).unwrap();
    let epoch_before = file("epoch");
    let marker_before = file("marker");
    let grid_before = file("grid");
    let intent_before = file("intent");
    let last_write = file("last-write");
    let unchanged = || {
        for (name, before) in [
            ("epoch", &epoch_before),
            ("marker", &marker_before),
            ("grid", &grid_before),
            ("intent", &intent_before),
            ("last-write", &last_write),
        ] {
            assert_eq!(&file(name), before, "{name}");
        }
        for name in [
            "paste-load-attempts",
            "paste-loads",
            "submissions",
            "effects",
            "pane0-bytes",
        ] {
            assert!(!root.join(name).exists(), "{name}");
        }
    };
    let old_bytes = b"discard this";
    assert_eq!(
        paste_ipc(root, stage(&old, 0, old_bytes)).await,
        PasteResult::Staged {}
    );
    let control = match refusal {
        PasteRefusal::Continuation | PasteRefusal::Commit => {
            let rejected = match refusal {
                PasteRefusal::Continuation => stage(&old, old_bytes.len(), b" continuation"),
                _ => commit(&old, old_bytes.len()),
            };
            paste_while_catalog_busy(root, &catalog, [rejected]).await;
            None
        }
        PasteRefusal::Scope => Some(("scope", "changed".to_owned())),
        PasteRefusal::Epoch => Some((
            "readback-epoch",
            epoch_text(Some(ResizeEpoch {
                owner: Uuid::new_v4(),
                ..epoch
            })),
        )),
    };
    if let Some((name, value)) = &control {
        fs::write(root.join(name), value).unwrap();
        assert_eq!(
            paste_ipc(root, commit(&old, old_bytes.len())).await,
            PasteResult::Stale {}
        );
        fs::remove_file(root.join(name)).unwrap();
    }
    // @contract: Rejection precedes offset zero so a surviving stage cannot conceal a failed invalidation.
    assert_eq!(
        paste_ipc(root, commit(&old, old_bytes.len())).await,
        PasteResult::Stale {}
    );
    for offset in [0, old_bytes.len()] {
        assert_eq!(
            paste_ipc(root, stage(&old, offset, old_bytes)).await,
            PasteResult::Stale {}
        );
    }
    unchanged();

    let fresh = PasteBinding {
        operation: old.operation + 1,
        ..old.clone()
    };
    let prefix = "fresh ☃".as_bytes();
    let tail = b"\nnew gesture";
    assert_eq!(
        paste_ipc(root, stage(&fresh, 0, prefix)).await,
        PasteResult::Staged {}
    );
    let higher = PasteBinding {
        operation: fresh.operation + 1,
        ..fresh.clone()
    };
    let wrong_sequence = PasteBinding {
        sequence: fresh.sequence + 1,
        ..fresh.clone()
    };
    let wrong_view = PasteBinding {
        terminal: TerminalView {
            view_id: Uuid::new_v4(),
            ..terminal.clone()
        },
        ..fresh.clone()
    };
    let canaries = [&old, &higher, &wrong_sequence, &wrong_view];
    if let Some((name, value)) = &control {
        fs::write(root.join(name), value).unwrap();
        for binding in canaries {
            assert_eq!(
                paste_ipc(root, commit(binding, prefix.len())).await,
                PasteResult::Stale {}
            );
        }
        fs::remove_file(root.join(name)).unwrap();
    } else {
        let requests = canaries.into_iter().map(|binding| match refusal {
            PasteRefusal::Continuation => stage(binding, prefix.len(), tail),
            _ => commit(binding, prefix.len()),
        });
        paste_while_catalog_busy(root, &catalog, requests).await;
    }
    unchanged();
    assert_eq!(
        paste_ipc(root, stage(&fresh, prefix.len(), tail)).await,
        PasteResult::Staged {}
    );
    let payload = [prefix, tail.as_slice()].concat();
    let PasteResult::Committed { receipt } = paste_ipc(root, commit(&fresh, payload.len())).await
    else {
        panic!("unrelated fresh stage did not survive on the original grant");
    };
    assert_eq!(
        paste_ipc(
            root,
            Request::PasteAck {
                binding: fresh.clone(),
                receipt
            }
        )
        .await,
        PasteResult::Acknowledged { writable: true }
    );
    assert_eq!(
        paste_ipc(root, commit(&fresh, payload.len())).await,
        PasteResult::Stale {}
    );
    assert_eq!(
        paste_ipc(root, commit(&old, old_bytes.len())).await,
        PasteResult::Stale {}
    );
    assert_eq!(file("paste-load-attempts"), b"%0\n");
    assert_eq!(file("paste-loads"), b"%0\n");
    assert_eq!(file("submissions"), b"1");
    assert_eq!(file("effects"), b"1");
    assert_eq!(file("pane0-bytes"), payload);
    assert_eq!(file("epoch"), epoch_before);
    assert_eq!(file("grid"), grid_before);
    assert_eq!(file("intent"), intent_before);
    assert!(String::from_utf8(file("marker")).unwrap().contains("~r~"));
    assert!(matches!(health().await, Response::Health {
        agent_pid: current, remote_execution_enabled: false
    } if current == agent_pid));
    let Response::TerminalStatus {
        status: final_status,
    } = request(Request::TerminalStatus { terminal }).await
    else {
        panic!("expected the same view after recovery");
    };
    assert_eq!(final_status.terminal, status.terminal);
    assert_eq!(final_status.epoch, Some(epoch));
    assert_eq!(final_status.committed, status.committed);
    assert_eq!(final_status.provider.unwrap().size, ORIGINAL);
    assert!(started.elapsed() < INPUT_LEASE_TTL);
    agent.abort_all();
    assert!(agent.join_next().await.unwrap().unwrap_err().is_cancelled());
}

#[tokio::test]
async fn w1_terminal_paste_busy_continuation_consumes_stage() {
    configured_paste_refusal(PasteRefusal::Continuation).await;
}

#[tokio::test]
async fn w1_terminal_paste_busy_commit_consumes_stage() {
    configured_paste_refusal(PasteRefusal::Commit).await;
}

#[tokio::test]
async fn w1_terminal_paste_early_scope_refusal_consumes_stage() {
    configured_paste_refusal(PasteRefusal::Scope).await;
}

#[tokio::test]
async fn w1_terminal_paste_early_epoch_refusal_consumes_stage() {
    configured_paste_refusal(PasteRefusal::Epoch).await;
}

#[tokio::test]
async fn w1_configured_legacy_writer_and_stop_make_no_attempts_after_catalog_fault() {
    let fixture = Fixture::new();
    let engine = &fixture.engine;
    let terminal = fixture.terminal.clone();
    let root = fixture.temp.path();
    let runtime_id = terminal.target.runtime_id;
    let (path, catalog) = configured_test_catalog(root).await;
    let send = async |message| engine.handle(message, Some(&catalog)).await;
    let Response::Control { token } = send(Request::Acquire { runtime_id }).await.unwrap() else {
        panic!("stamped legacy CLI writer was ineligible");
    };
    assert!(matches!(
        send(Request::Release { runtime_id, token }).await.unwrap(),
        Response::Accepted {}
    ));
    let acquire = || Request::AcquireTerminal {
        terminal: terminal.clone(),
    };
    let Response::TerminalControl { token, epoch } = send(acquire()).await.unwrap() else {
        panic!("stamped legacy view writer was ineligible");
    };
    let binding = PasteBinding {
        terminal: terminal.clone(),
        token,
        epoch,
        incarnation: Uuid::new_v4(),
        operation: 1,
        sequence: 0,
    };
    let stage = |offset| Request::PasteStage {
        binding: binding.clone(),
        offset,
        data: vec![b'a'],
    };
    assert!(matches!(
        send(stage(0)).await.unwrap(),
        Response::Paste {
            paste: PasteResult::Staged {}
        }
    ));

    let read = engine.scoped_geometry(&terminal).await.unwrap();
    let last_write = fs::read(root.join("last-write")).unwrap();
    fs::rename(path.join("catalog.sqlite3"), path.join("retained.sqlite3")).unwrap();
    for mutation in [
        Request::Acquire { runtime_id },
        acquire(),
        Request::Input {
            runtime_id,
            token,
            sequence: 0,
            data: b"x".to_vec(),
            paste: false,
        },
        Request::ResizeTerminal {
            terminal: terminal.clone(),
            token,
            sequence: 0,
            size: DESIRED,
        },
        Request::Stop {
            runtime_id,
            expected_pane_id: terminal.target.pane.clone(),
        },
    ] {
        let denied = send(mutation).await.unwrap_err();
        assert!(denied.to_string().contains("CatalogMissing"), "{denied:#}");
    }
    for paste in [stage(1), Request::PasteCommit { binding, bytes: 1 }] {
        assert!(matches!(
            send(paste).await.unwrap(),
            Response::Paste {
                paste: PasteResult::Unavailable {}
            }
        ));
    }
    assert!(
        send(Request::Renew { runtime_id, token })
            .await
            .unwrap_err()
            .to_string()
            .contains("CatalogMissing")
    );
    assert_eq!(fs::read(root.join("last-write")).unwrap(), last_write);
    assert_eq!(engine.scoped_geometry(&terminal).await.unwrap(), read);
    assert!(
        engine
            .read_custody(read.server)
            .await
            .unwrap()
            .iter()
            .all(Option::is_none)
    );
    for name in [
        "effects",
        "deletions",
        "submissions",
        "paste-loads",
        "removed-0",
        "pane0-bytes",
    ] {
        assert!(!root.join(name).exists(), "{name}");
    }
}

#[tokio::test]
async fn w1_paste_catalog_fault_at_send_preserves_armed_custody() {
    let fixture = Fixture::new();
    let engine = &fixture.engine;
    *engine.paste_test.now.lock().unwrap() = Some(Instant::now());
    let root = fixture.temp.path();
    let (path, lane) = configured_test_catalog(root).await;
    let terminal = fixture.terminal.clone();
    let response = engine
        .handle(
            Request::AcquireTerminal {
                terminal: terminal.clone(),
            },
            Some(&lane),
        )
        .await
        .unwrap();
    let Response::TerminalControl { token, epoch } = response else {
        panic!("healthy configured legacy writer refused: {response:?}");
    };
    let binding = PasteBinding {
        terminal: terminal.clone(),
        token,
        epoch,
        incarnation: Uuid::new_v4(),
        operation: 1,
        sequence: 0,
    };
    let payload = "catalog fault fixture\n☃".as_bytes();
    assert_eq!(
        engine
            .paste_stage(binding.clone(), 0, payload.to_vec(), Some(&lane))
            .await,
        PasteResult::Staged {}
    );
    let barrier = Arc::new(PasteBarrier::default());
    *engine.paste_test.admission.lock().unwrap() = Some(barrier.clone());
    let commit = tokio::spawn({
        let engine = engine.clone();
        let binding = binding.clone();
        let lane = lane.clone();
        async move {
            engine
                .paste_commit(binding, payload.len(), Some(&lane))
                .await
        }
    });
    tokio::time::timeout(SEND_WATCHDOG, barrier.reached.notified())
        .await
        .unwrap();
    let read = engine.scoped_geometry(&terminal).await.unwrap();
    let buffer = PasteBuffer::from_marker(&read).unwrap();
    assert_eq!(read.paste_marker, buffer.marker(&read, 'a'));
    let prior = engine.read_custody(read.server).await.unwrap();
    assert!(
        prior[0]
            .as_ref()
            .unwrap()
            .active
            .as_ref()
            .unwrap()
            .completed()
    );
    assert_eq!(fs::read(root.join(&buffer.name)).unwrap(), payload);
    let last_write = fs::read(root.join("last-write")).unwrap();
    fs::rename(path.join("catalog.sqlite3"), path.join("retained.sqlite3")).unwrap();
    barrier.proceed.notify_one();
    assert_eq!(commit.await.unwrap(), PasteResult::Unknown {});
    assert_eq!(fs::read(root.join(&buffer.name)).unwrap(), payload);
    assert_eq!(fs::read(root.join("last-write")).unwrap(), last_write);
    let after = engine.read_custody(read.server).await.unwrap();
    for (old, new) in prior.iter().zip(after.iter()) {
        assert_eq!(
            old.as_ref().map(|slot| &slot.value),
            new.as_ref().map(|slot| &slot.value)
        );
    }
    assert_eq!(engine.scoped_geometry(&terminal).await.unwrap(), read);
    for name in ["effects", "deletions", "submissions"] {
        assert!(!root.join(name).exists(), "{name}");
    }
    let status = engine
        .handle(
            Request::TerminalStatus {
                terminal: terminal.clone(),
            },
            Some(&lane),
        )
        .await
        .unwrap();
    assert!(
        matches!(status, Response::TerminalStatus { status } if status.terminal == terminal && status.observation == ObservationState::Observed)
    );
    let stopped = engine
        .handle(
            Request::Stop {
                runtime_id: terminal.target.runtime_id,
                expected_pane_id: terminal.target.pane.clone(),
            },
            Some(&lane),
        )
        .await;
    assert!(stopped.is_err());
    assert_eq!(fs::read(root.join("last-write")).unwrap(), last_write);
}

#[tokio::test]
async fn w1_paste_collision() {
    let fixture = Fixture::paste();
    let engine = &fixture.engine;
    *engine.paste_test.now.lock().unwrap() = Some(Instant::now());
    let root = fixture.temp.path();
    let (second, mut provider) = fixture.second_pane(false);
    assert_ne!(fixture.terminal.target.runtime_id, second.target.runtime_id);
    let acquire = async |terminal: TerminalView| {
        let (token, epoch) = fixture.acquire(terminal.clone()).await;
        PasteBinding {
            terminal,
            token,
            epoch,
            incarnation: Uuid::new_v4(),
            operation: 1,
            sequence: 0,
        }
    };
    let loser = acquire(fixture.terminal.clone()).await;
    let winner = acquire(second).await;
    let server = engine
        .scoped_geometry(&loser.terminal)
        .await
        .unwrap()
        .server;
    assert_eq!(
        engine
            .scoped_geometry(&winner.terminal)
            .await
            .unwrap()
            .server,
        server
    );
    assert!(
        engine
            .read_custody(server)
            .await
            .unwrap()
            .iter()
            .all(Option::is_none)
    );

    let canary_buffer = format!("rs-paste-{}", Uuid::new_v4().simple());
    fs::write(root.join(&canary_buffer), b"paste canary").unwrap();
    fs::write(root.join("unrelated-buffer"), b"unrelated buffer").unwrap();
    let canary_runtime = Uuid::new_v4();
    let canary_name = tmux_name(Uuid::new_v4(), canary_runtime);
    fs::write(
        root.join("canary-pane"),
        format!(
            "{}|{}|{}|$2|@2|%2|{}|{canary_name}|0|0||0|1|1|manual|/dev/ttyscanary",
            server.pid,
            server.start,
            server.token,
            std::process::id()
        ),
    )
    .unwrap();
    fs::write(root.join("grid-2"), "100 30").unwrap();
    let file = |name: &str| fs::read(root.join(name)).unwrap();
    let assert_paste_effects = |loads: &[u8], sends: &[u8]| {
        assert_eq!(file("paste-load-attempts"), loads);
        assert_eq!(file("paste-loads"), loads);
        assert_eq!(file("submissions"), sends);
        assert_eq!(file("effects"), sends);
    };
    let assert_canaries = || {
        assert_eq!(file(&canary_buffer), b"paste canary");
        assert_eq!(file("unrelated-buffer"), b"unrelated buffer");
        for name in ["removed-2", "pane2-bytes", "deletions"] {
            assert!(!root.join(name).exists(), "{name}");
        }
    };
    let stage = async |binding: &PasteBinding, data: &[u8]| {
        engine
            .handle(
                Request::PasteStage {
                    binding: binding.clone(),
                    offset: 0,
                    data: data.into(),
                },
                None,
            )
            .await
            .unwrap()
    };
    let ack = async |binding: &PasteBinding, receipt| {
        assert!(matches!(
            engine
                .handle(
                    Request::PasteAck {
                        binding: binding.clone(),
                        receipt
                    },
                    None
                )
                .await
                .unwrap(),
            Response::Paste {
                paste: PasteResult::Acknowledged { writable: true }
            }
        ));
    };

    let losing_draft = b"losing draft";
    let winning_draft = b"winning draft";
    assert!(matches!(
        stage(&loser, losing_draft).await,
        Response::Paste {
            paste: PasteResult::Staged {}
        }
    ));
    assert!(matches!(
        stage(&winner, winning_draft).await,
        Response::Paste {
            paste: PasteResult::Staged {}
        }
    ));
    let original_commit = || Request::PasteCommit {
        binding: loser.clone(),
        bytes: losing_draft.len(),
    };
    let barrier = Arc::new(PasteBarrier::default());
    *engine.paste_test.custody_claim.lock().unwrap() = Some(barrier.clone());
    let losing_commit = original_commit();
    let committing_engine = Arc::clone(engine);
    let parked = tokio::spawn(async move { committing_engine.handle(losing_commit, None).await });
    tokio::time::timeout(SEND_WATCHDOG, barrier.reached.notified())
        .await
        .unwrap();
    let loading = engine.scoped_geometry(&loser.terminal).await.unwrap();
    let lost_buffer = PasteBuffer::from_marker(&loading).unwrap();
    assert_eq!(loading.paste_marker, lost_buffer.marker(&loading, 'l'));
    assert!(
        engine
            .read_custody(server)
            .await
            .unwrap()
            .iter()
            .all(Option::is_none)
    );
    for name in [
        "custody-writes",
        "paste-load-attempts",
        "paste-loads",
        "submissions",
        "effects",
    ] {
        assert!(!root.join(name).exists(), "{name}");
    }

    let Response::Paste {
        paste: PasteResult::Committed { receipt },
    } = engine
        .handle(
            Request::PasteCommit {
                binding: winner.clone(),
                bytes: winning_draft.len(),
            },
            None,
        )
        .await
        .unwrap()
    else {
        panic!("Expected the second runtime to commit");
    };
    let claimed = engine.read_custody(server).await.unwrap();
    let winner_slot = claimed[0].as_ref().unwrap();
    let winning_claim = winner_slot
        .active
        .as_ref()
        .and_then(CustodyBinding::payload)
        .unwrap();
    assert!(winning_claim.completed);
    assert_eq!(winning_claim.runtime, winner.terminal.target.runtime_id);
    assert_eq!(winning_claim.pane, winner.terminal.target.pane);
    assert!(!root.join(&winning_claim.buffer.name).exists());
    ack(&winner, receipt).await;
    let retired = engine.read_custody(server).await.unwrap();
    let tombstone = retired[0].as_ref().unwrap();
    assert!(tombstone.active.is_none());
    assert_ne!(tombstone.mutation, winner_slot.mutation);
    assert_eq!(retired.iter().flatten().count(), 1);
    assert_paste_effects(b"%1\n", b"1");
    assert_eq!(file("pane1-bytes"), winning_draft);
    assert!(!root.join("pane0-bytes").exists());
    let winner_marker = file("marker-1");

    barrier.proceed.notify_one();
    assert!(matches!(
        tokio::time::timeout(SEND_WATCHDOG, parked)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        Response::Paste {
            paste: PasteResult::Unavailable {}
        }
    ));
    let recovered = engine.scoped_geometry(&loser.terminal).await.unwrap();
    assert!(recovered.same_pane(&loading) && recovered.paste_ready());
    assert_eq!(recovered.epoch, Some(loser.epoch));
    assert_ne!(recovered.paste_marker, loading.paste_marker);
    assert_eq!(file("custody-conflicts"), b"%0\n");
    let after_collision = engine.read_custody(server).await.unwrap();
    assert_eq!(after_collision.iter().flatten().count(), 1);
    assert_eq!(after_collision[0].as_ref().unwrap().value, tombstone.value);
    assert_eq!(file("marker-1"), winner_marker);
    assert!(!root.join(&lost_buffer.name).exists());

    assert!(matches!(
        engine.handle(original_commit(), None).await.unwrap(),
        Response::Paste {
            paste: PasteResult::Stale {}
        }
    ));
    assert!(matches!(
        stage(&loser, losing_draft).await,
        Response::Paste {
            paste: PasteResult::Stale {}
        }
    ));
    assert_paste_effects(b"%1\n", b"1");
    assert_eq!(file("pane1-bytes"), winning_draft);
    assert!(!root.join("pane0-bytes").exists());
    assert_canaries();

    for (binding, sequence, data) in [(&loser, 0, b'A'), (&winner, 1, b'B')] {
        assert!(matches!(
            engine
                .handle(
                    Request::Input {
                        runtime_id: binding.terminal.target.runtime_id,
                        token: binding.token,
                        sequence,
                        data: vec![data],
                        paste: false,
                    },
                    None,
                )
                .await
                .unwrap(),
            Response::Accepted {}
        ));
    }
    assert_eq!(file("pane0-bytes"), b"A");
    assert_eq!(
        file("pane1-bytes"),
        [winning_draft.as_slice(), b"B"].concat()
    );

    let fresh = PasteBinding {
        operation: loser.operation + 1,
        sequence: 1,
        ..loser.clone()
    };
    let fresh_draft = b"fresh draft";
    assert!(matches!(
        stage(&fresh, fresh_draft).await,
        Response::Paste {
            paste: PasteResult::Staged {}
        }
    ));
    let Response::Paste {
        paste: PasteResult::Committed { receipt },
    } = engine
        .handle(
            Request::PasteCommit {
                binding: fresh.clone(),
                bytes: fresh_draft.len(),
            },
            None,
        )
        .await
        .unwrap()
    else {
        panic!("Expected a fresh staged gesture to commit after recovery");
    };
    ack(&fresh, receipt).await;
    let reused = engine.read_custody(server).await.unwrap();
    let free = reused[0].as_ref().unwrap();
    assert!(free.active.is_none());
    assert_ne!(free.mutation, tombstone.mutation);
    assert_eq!(reused.iter().flatten().count(), 1);
    assert_paste_effects(b"%1\n%0\n", b"11");
    assert_eq!(file("pane0-bytes"), [b"A".as_slice(), fresh_draft].concat());
    assert_eq!(
        file("pane1-bytes"),
        [winning_draft.as_slice(), b"B"].concat()
    );

    let stop = async |runtime_id, expected_pane_id: &str| {
        engine
            .handle(
                Request::Stop {
                    runtime_id,
                    expected_pane_id: expected_pane_id.into(),
                },
                None,
            )
            .await
    };
    assert!(
        stop(
            loser.terminal.target.runtime_id,
            &winner.terminal.target.pane
        )
        .await
        .is_err()
    );
    assert!(!root.join("removed-0").exists() && !root.join("removed-1").exists());
    for (binding, removed) in [(&loser, "removed-0"), (&winner, "removed-1")] {
        let target = &binding.terminal.target;
        assert!(matches!(
            stop(target.runtime_id, &target.pane).await.unwrap(),
            Response::Stopped { runtime_id } if runtime_id == target.runtime_id
        ));
        assert_eq!(file(removed), b"1");
        if removed == "removed-0" {
            assert!(!root.join("removed-1").exists());
        }
    }
    assert_canaries();
    let runtimes = engine.list().await.unwrap();
    assert_eq!(runtimes.len(), 1);
    assert_eq!(runtimes[0].runtime_id, canary_runtime);
    assert_eq!(runtimes[0].pane_id, "%2");
    assert_paste_effects(b"%1\n%0\n", b"11");
    provider.kill().await.unwrap();
}

#[tokio::test]
async fn w1_terminal_paste_capability_gap_retains_exact_buffer_custody() {
    let fixture = Fixture::paste();
    let root = fixture.temp.path();
    let (token, epoch) = fixture.acquire(fixture.terminal.clone()).await;
    let old_binding = PasteBinding {
        terminal: fixture.terminal.clone(),
        token,
        epoch,
        incarnation: Uuid::new_v4(),
        operation: 1,
        sequence: 0,
    };
    let runtime = old_binding.terminal.target.runtime_id;
    let pane = &old_binding.terminal.target.pane;
    let ready = fixture.engine.geometry(runtime, pane).await.unwrap();
    let bound = PasteBuffer {
        nonce: Uuid::new_v4(),
        operation: old_binding.operation,
        name: format!("rs-paste-{}", Uuid::new_v4().simple()),
    };
    let draft = b"original staged draft";
    let loading = fixture
        .engine
        .transition_marker(&ready, &bound.marker(&ready, 'l'))
        .await
        .unwrap()
        .unwrap();
    // @contract: Preserve buffer state from a producer predating server custody.
    fs::write(root.join(&bound.name), draft).unwrap();
    let canary = format!("rs-paste-{}", Uuid::new_v4().simple());
    fs::write(root.join(&canary), b"unrelated buffer").unwrap();

    let (engine, original) = fixture.restart();
    assert_ne!(original.agent_epoch, old_binding.terminal.agent_epoch);
    assert!(engine.existing_lane(runtime).is_none());
    fs::write(root.join("unsupported"), "1").unwrap();
    assert!(matches!(
        engine
            .handle(Request::AcquireTerminal { terminal: original }, None)
            .await
            .unwrap(),
        Response::ControlRecovering {
            paste_admission: PasteAdmission::PriorSendUnknown
        }
    ));
    let lane = engine.existing_lane(runtime).unwrap();
    let assert_custody = || async {
        let lane = lane.lock().await;
        let [held] = &lane.paste_quarantines[..] else {
            panic!("expected one quarantine for the original pane");
        };
        assert!(!held.original.sender_supported);
        assert!(held.original.same_pane(&loading));
        assert_eq!(held.original.epoch, Some(epoch));
        assert_eq!(held.original.paste_marker, loading.paste_marker);
        assert!(held.buffer.as_ref() == Some(&bound));
        assert_eq!(fs::read(root.join(&bound.name)).unwrap(), draft);
        assert_eq!(fs::read(root.join(&canary)).unwrap(), b"unrelated buffer");
        assert!(!root.join("deletions").exists());
    };
    assert_custody().await;

    let (mut replacement, _process) = fixture.second_pane(true);
    replacement.agent_epoch = engine.agent_epoch;
    assert_ne!(replacement.target.pane, *pane);
    fs::write(root.join("removed-0"), "1").unwrap();
    let acquire = || Request::AcquireTerminal {
        terminal: replacement.clone(),
    };
    assert!(matches!(
        engine.handle(acquire(), None).await.unwrap(),
        Response::ControlRecovering {
            paste_admission: PasteAdmission::Unavailable
        }
    ));
    assert_custody().await;

    fs::write(root.join("cleanup-blocked"), "1").unwrap();
    fs::remove_file(root.join("unsupported")).unwrap();
    let Response::TerminalControl {
        token: successor, ..
    } = engine.handle(acquire(), None).await.unwrap()
    else {
        panic!("expected the corroborated replacement to acquire control");
    };
    assert_ne!(successor, token);
    assert_custody().await;
    assert!(matches!(
        engine
            .handle(
                Request::Release {
                    runtime_id: runtime,
                    token: successor
                },
                None
            )
            .await
            .unwrap(),
        Response::Accepted {}
    ));
    fs::remove_file(root.join("cleanup-blocked")).unwrap();
    assert!(matches!(
        engine.handle(acquire(), None).await.unwrap(),
        Response::TerminalControl { .. }
    ));
    assert!(!lane.lock().await.has_paste_hold());
    assert!(!root.join(&bound.name).exists());
    assert_eq!(
        fs::read_to_string(root.join("deletions")).unwrap(),
        format!("{}\n", bound.name)
    );
    assert_eq!(fs::read(root.join(&canary)).unwrap(), b"unrelated buffer");
    assert!(matches!(
        engine
            .handle(
                Request::PasteCommit {
                    binding: old_binding,
                    bytes: draft.len()
                },
                None
            )
            .await
            .unwrap(),
        Response::Paste {
            paste: PasteResult::Stale {}
        }
    ));
    for file in ["submissions", "effects", "pane0-bytes", "pane1-bytes"] {
        assert!(!root.join(file).exists());
    }
}

#[tokio::test]
async fn w1_terminal_paste_replacement_preserves_unproved_custody() {
    let fixture = Fixture::paste();
    let root = fixture.temp.path();
    let runtime = fixture.terminal.target.runtime_id;
    let (_, epoch) = fixture.acquire(fixture.terminal.clone()).await;
    let original = fixture.engine.geometry(runtime, "%0").await.unwrap();
    let canary = format!("rs-paste-{}", Uuid::new_v4().simple());
    let marker = PasteBuffer {
        nonce: Uuid::new_v4(),
        operation: 1,
        name: canary.clone(),
    }
    .marker(&original, 'l')
    .replace(&epoch.owner.to_string(), &Uuid::new_v4().to_string());
    fs::write(root.join(&canary), b"unrelated buffer").unwrap();
    fs::write(root.join("marker"), &marker).unwrap();

    let (engine, terminal) = fixture.restart();
    assert!(matches!(
        engine
            .handle(Request::AcquireTerminal { terminal }, None)
            .await
            .unwrap(),
        Response::ControlRecovering {
            paste_admission: PasteAdmission::PriorSendUnknown
        }
    ));
    let (mut replacement, _process) = fixture.second_pane(true);
    replacement.agent_epoch = engine.agent_epoch;
    fs::write(root.join("removed-0"), "1").unwrap();
    assert!(matches!(
        engine
            .handle(
                Request::AcquireTerminal {
                    terminal: replacement
                },
                None
            )
            .await
            .unwrap(),
        Response::TerminalControl { .. }
    ));
    let lane = engine.existing_lane(runtime).unwrap();
    let lane = lane.lock().await;
    let [held] = &lane.paste_quarantines[..] else {
        panic!("expected unresolved original custody");
    };
    assert!(held.buffer.is_none());
    assert_eq!(held.original.paste_marker, marker);
    assert_eq!(fs::read(root.join(&canary)).unwrap(), b"unrelated buffer");
    assert!(!root.join("deletions").exists());
}

async fn start_custody_agent(fixture: &Fixture) -> tokio::task::JoinHandle<Result<()>> {
    let (engine, _) = fixture.restart();
    let root = fixture.temp.path();
    fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
    let guard = Arc::new(crate::AgentLock::acquire(root).unwrap());
    let agent = tokio::spawn(crate::serve_local(engine, guard, None));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            assert!(!agent.is_finished());
            if matches!(
                crate::request(root, &Request::Health {}).await,
                Ok(Response::Health {
                    remote_execution_enabled: false,
                    ..
                })
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    agent
}

async fn stop_custody_agent(root: &Path, agent: tokio::task::JoinHandle<Result<()>>) {
    agent.abort();
    assert!(agent.await.unwrap_err().is_cancelled());
    crate::w1_create_tests::wait_lock_release(
        &root.join("agent.lock"),
        Duration::from_secs(3),
        || {},
    )
    .await
    .expect("custody fixture agent lock did not release");
}

#[tokio::test]
async fn w1_resize_production_paste_custody_survives_pane_loss_and_agent_restart() {
    let fixture = Fixture::paste();
    let root = fixture.temp.path();
    let runtime_id = fixture.terminal.target.runtime_id;
    let local = async |request| crate::request(root, &request).await.unwrap();
    let open = async || {
        let Response::TerminalOpened { status } = local(Request::OpenTerminal {
            runtime_id,
            view_id: Uuid::new_v4(),
        })
        .await
        else {
            panic!("expected exact live managed pane");
        };
        status.terminal
    };
    let acquire = async |terminal| {
        let reply = local(Request::AcquireTerminal { terminal }).await;
        let Response::TerminalControl { token, epoch } = reply else {
            panic!("expected independent writer authority, got {reply:?}");
        };
        (token, epoch)
    };

    let agent = start_custody_agent(&fixture).await;
    let original = open().await;
    let old_pane = original.target.pane.clone();
    let (token, epoch) = acquire(original.clone()).await;
    let binding = PasteBinding {
        terminal: original,
        token,
        epoch,
        incarnation: Uuid::new_v4(),
        operation: 1,
        sequence: 0,
    };
    let draft = "private fixture draft\n\tСніг".as_bytes();
    let commit = || Request::PasteCommit {
        binding: binding.clone(),
        bytes: draft.len(),
    };
    assert!(matches!(
        local(Request::PasteStage {
            binding: binding.clone(),
            offset: 0,
            data: draft.into()
        })
        .await,
        Response::Paste {
            paste: PasteResult::Staged {}
        }
    ));
    fs::write(root.join("load-mode"), "lost").unwrap();
    assert!(matches!(
        local(commit()).await,
        Response::Paste {
            paste: PasteResult::Unknown {}
        }
    ));
    let server = fixture.engine.generation().await.unwrap();
    let snapshot = fixture.engine.read_custody(server).await.unwrap();
    let mut entries = snapshot.iter().flatten();
    let slot = entries.next().expect("expected a production reservation");
    assert!(entries.next().is_none());
    let active = slot
        .active
        .as_ref()
        .and_then(CustodyBinding::payload)
        .unwrap();
    let loading = fixture
        .engine
        .geometry(runtime_id, &old_pane)
        .await
        .unwrap();
    assert!(active.matches(&loading, &active.buffer));
    assert_eq!(active.buffer.operation, binding.operation);
    assert_eq!(active.marker, loading.paste_marker);
    assert!(
        slot.value
            .starts_with(&format!("v1~{}~possible~", slot.mutation))
    );
    assert!(!active.completed);
    assert!(
        !slot
            .value
            .as_bytes()
            .windows(draft.len())
            .any(|bytes| bytes == draft)
    );
    assert!(!slot.value.contains(&token.to_string()));
    let buffer = &active.buffer.name;
    assert_eq!(fs::read_to_string(root.join("last-load")).unwrap(), *buffer);
    let canary_buffer = format!("rs-paste-{}", Uuid::new_v4().simple());
    fs::write(root.join(&canary_buffer), b"canary buffer").unwrap();
    let canary_runtime = Uuid::new_v4();
    let canary_name = tmux_name(Uuid::new_v4(), canary_runtime);
    fs::write(
        root.join("canary-pane"),
        format!(
            "{}|{}|{}|$2|@2|%2|{}|{canary_name}|0|0||0|1|1|manual|/dev/ttyscanary",
            server.pid,
            server.start,
            server.token,
            std::process::id()
        ),
    )
    .unwrap();
    fs::write(root.join("grid-2"), "100 30").unwrap();

    let assert_held = async || {
        let snapshot = fixture.engine.read_custody(server).await.unwrap();
        assert_eq!(snapshot[slot.index].as_ref().unwrap().value, slot.value);
        assert_eq!(fs::read(root.join(buffer)).unwrap(), draft);
        assert_eq!(
            fs::read(root.join(&canary_buffer)).unwrap(),
            b"canary buffer"
        );
        assert!(!root.join("removed-1").exists());
        assert!(!root.join("removed-2").exists());
        assert!(!root.join("deletions").exists());
        assert!(!root.join("submissions").exists());
    };
    assert_held().await;
    let (replacement, mut provider) = fixture.second_pane(true);
    fs::write(root.join("removed-0"), "1").unwrap();
    fs::write(root.join("absence-unconfirmed"), "1").unwrap();
    let before_restart = open().await;
    assert_eq!(before_restart.target, replacement.target);
    let (successor, _) = acquire(before_restart.clone()).await;
    assert_ne!(successor, token);
    assert_held().await;
    stop_custody_agent(root, agent).await;

    let agent = start_custody_agent(&fixture).await;
    let restarted = open().await;
    assert_ne!(restarted.agent_epoch, before_restart.agent_epoch);
    assert_eq!(restarted.target, before_restart.target);
    assert_eq!(fixture.engine.generation().await.unwrap(), server);
    assert!(matches!(
        local(commit()).await,
        Response::Paste {
            paste: PasteResult::Stale {}
        }
    ));
    provider.kill().await.unwrap();
    let row = fs::read_to_string(root.join("second-pane")).unwrap();
    let mut row: Vec<_> = row.split('|').collect();
    row[11] = "1";
    fs::write(root.join("second-pane"), row.join("|")).unwrap();
    let stop = || Request::Stop {
        runtime_id,
        expected_pane_id: replacement.target.pane.clone(),
    };
    assert!(matches!(local(stop()).await, Response::Error { .. }));
    assert_held().await;

    fs::remove_file(root.join("absence-unconfirmed")).unwrap();
    fs::write(root.join("cleanup-blocked"), "1").unwrap();
    assert!(matches!(local(stop()).await, Response::Error { .. }));
    assert_held().await;
    fs::remove_file(root.join("cleanup-blocked")).unwrap();
    fs::write(root.join("lose-cleanup-reply"), "1").unwrap();
    assert!(
        matches!(local(stop()).await, Response::Stopped { runtime_id: stopped } if stopped == runtime_id)
    );
    let retired = fixture.engine.read_custody(server).await.unwrap();
    let free = retired[slot.index].as_ref().unwrap();
    assert!(free.active.is_none());
    assert_ne!(free.mutation, slot.mutation);
    assert_eq!(retired.iter().flatten().count(), 1);
    assert!(!root.join(buffer).exists());
    assert_eq!(
        fs::read_to_string(root.join("deletions")).unwrap(),
        format!("{buffer}\n")
    );
    assert_eq!(
        fs::read(root.join(&canary_buffer)).unwrap(),
        b"canary buffer"
    );
    let Response::Runtimes { runtimes } = local(Request::List {}).await else {
        panic!("expected canary in inventory");
    };
    assert_eq!(runtimes.len(), 1);
    assert_eq!(runtimes[0].runtime_id, canary_runtime);
    assert_eq!(runtimes[0].pane_id, "%2");
    assert!(matches!(
        local(commit()).await,
        Response::Paste {
            paste: PasteResult::Stale {}
        }
    ));
    for file in [
        "submissions",
        "effects",
        "pane0-bytes",
        "pane1-bytes",
        "pane2-bytes",
        "removed-2",
    ] {
        assert!(!root.join(file).exists(), "{file}");
    }
    stop_custody_agent(root, agent).await;
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
        crate::w1_create_tests::wait_lock_release(
            &endpoint.join("agent.lock"),
            Duration::from_secs(3),
            || {},
        )
        .await
        .unwrap_or_else(|error| panic!("fixture lock handoff after {socket:?} failed: {error}"));
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

    let mut agent = tokio::spawn(crate::serve_resize_fixture(
        endpoint.clone(),
        f.engine.tmux.clone(),
        f.engine.socket.clone(),
        f.terminal.target.clone(),
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !endpoint.join("agent.sock").exists() {
            if agent.is_finished() {
                panic!(
                    "fixture agent exited before publishing its socket: {:?}",
                    (&mut agent).await
                );
            }
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

#[tokio::test]
async fn w1_resize_private_contention_refuses_before_admission_and_keeps_original_grant() {
    let f = Fixture::new();
    let root = f.temp.path();
    let directory = root.join("agent");
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let guard = Arc::new(crate::AgentLock::acquire(&directory).unwrap());
    let socket = guard.socket.clone();
    let gate = tokio::net::UnixListener::bind(root.join("resize-lane.sock")).unwrap();
    let engine = Arc::into_inner(f.engine).unwrap();
    *engine.paste_test.now.lock().unwrap() = Some(Instant::now());
    let mut agent = tokio::task::JoinSet::new();
    agent.spawn(crate::serve_local(engine, guard, None));
    tokio::time::timeout(Duration::from_secs(3), async {
        while !socket.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let terminal = f.terminal;
    let runtime_id = terminal.target.runtime_id;
    let send = async |request| crate::request(&directory, &request).await.unwrap();
    let Response::TerminalControl { token, epoch } = send(Request::AcquireTerminal {
        terminal: terminal.clone(),
    })
    .await
    else {
        panic!("expected the original writer grant");
    };
    let resize = |sequence, size| Request::ResizeTerminal {
        terminal: terminal.clone(),
        token,
        sequence,
        size,
    };
    let dispatches = root.join("resize-dispatches");
    let latest = TerminalSize {
        cols: 142,
        rows: 46,
    };
    for (request, size) in [
        (Request::Renew { runtime_id, token }, DESIRED),
        (
            Request::Input {
                runtime_id,
                token,
                sequence: 0,
                data: b"x".to_vec(),
                paste: false,
            },
            latest,
        ),
    ] {
        fs::write(root.join("park-resize-lane"), "1").unwrap();
        let mut writer = std::pin::pin!(send(request));
        let mut parked = tokio::select! {
            result = writer.as_mut() => panic!("writer finished before parking: {result:?}"),
            accepted = tokio::time::timeout(Duration::from_secs(3), gate.accept()) => accepted.unwrap().unwrap().0,
        };
        let mut entered = [0];
        tokio::io::AsyncReadExt::read_exact(&mut parked, &mut entered)
            .await
            .unwrap();
        assert_eq!(&entered, b"P");
        let last_write = fs::read(root.join("last-write")).unwrap();
        let Response::Resize { resize: refused } = send(resize(0, size)).await else {
            panic!("expected a scoped zero-admission response while the lane is parked");
        };
        assert_eq!(
            refused,
            ResizeResult::Busy {
                terminal: terminal.clone(),
                size
            }
        );
        assert!(!dispatches.exists());
        assert_eq!(fs::read(root.join("last-write")).unwrap(), last_write);
        assert_eq!(fs::read_to_string(root.join("grid")).unwrap(), "100 30");
        assert_eq!(fs::read_to_string(root.join("intent")).unwrap(), "");
        parked.write_all(b"R").await.unwrap();
        assert!(matches!(writer.await, Response::Accepted {}));
    }
    assert_eq!(fs::read(root.join("pane0-bytes")).unwrap(), b"x");
    let Response::Resize {
        resize: ResizeResult::Committed { admission, status },
    } = send(resize(0, latest)).await
    else {
        panic!("the latest size must commit at the unconsumed original sequence");
    };
    assert_eq!(status.terminal, terminal);
    assert_eq!(status.epoch, Some(epoch));
    assert_eq!(admission.epoch, epoch);
    assert_eq!(
        status.committed,
        CommittedGeometry {
            size: latest,
            revision: 1,
            intent: Some(admission)
        }
    );
    assert_eq!(fs::read(&dispatches).unwrap(), b"1");
    assert!(matches!(
        send(resize(0, DESIRED)).await,
        Response::Error { .. }
    ));
    assert_eq!(fs::read(dispatches).unwrap(), b"1");
    agent.shutdown().await;
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

    const COMMAND_TIMEOUT: Duration = Duration::from_secs(4);
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
            let mut engine = Engine::new(
                self.temp.path().join("tmux-fixture"),
                self.stock.socket.clone(),
            );
            engine.paste_test.geometry_watchdog = Some(COMMAND_TIMEOUT);
            engine.paste_test.send_watchdog = Some(COMMAND_TIMEOUT);
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
        let acquired = engine
            .handle(
                Request::AcquireTerminal {
                    terminal: terminal.clone(),
                },
                None,
            )
            .await
            .unwrap();
        let Response::TerminalControl { token, epoch } = acquired else {
            panic!("Expected a real pane writer, got {acquired:?}");
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
        let stale = CustodySlot::bind(&loading, 0, &buffer, "possible").unwrap();
        assert!(
            engine
                .load_paste(&loading, b"stale load", &stale)
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
        let mut stamped = engine.geometry(runtime.runtime_id, pane).await.unwrap();
        let launch = Uuid::new_v4().to_string();
        stamped.paste_launch = launch.clone();
        let marker = stamped.ready_marker(None);
        fixture
            .tmux(&[
                "set-option",
                "-po",
                "-t",
                pane,
                PASTE_LAUNCH_OPTION,
                &launch,
                ";",
                "set-option",
                "-po",
                "-t",
                pane,
                PASTE_OPTION,
                &marker,
            ])
            .await;
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
        // @contract: Preserve the historical pane marker and exact buffer without a server record.
        fixture
            .tmux(&[
                "set-buffer",
                "-b",
                &buffer.name,
                std::str::from_utf8(DRAFT).unwrap(),
            ])
            .await;
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

#[tokio::test]
async fn w1_probe_custody_classifies_exact_load_readback_and_retirement() {
    for case in [
        "healthy",
        "missing-flags",
        "wrong-bytes",
        "load-reply-lost",
        "delete-failed",
        "claim-hidden",
        "claim-failed",
        "claim-reply-lost",
        "delete-reply-lost",
    ] {
        let fixture = Fixture::new();
        let engine = &fixture.engine;
        let root = fixture.temp.path();
        for option in ["launch", "marker"] {
            fs::remove_file(root.join(option)).unwrap();
        }
        let read = engine.scoped_geometry(&fixture.terminal).await.unwrap();
        match case {
            "missing-flags" => fs::write(root.join("probe-mode"), "missing-flag"),
            "wrong-bytes" => fs::write(root.join("probe-mode"), "truncated"),
            "load-reply-lost" => fs::write(root.join("load-mode"), "lost"),
            "delete-failed" => fs::write(root.join("cleanup-blocked"), "1"),
            "claim-hidden" => fs::write(root.join("custody-snapshot"), "hide-write"),
            "claim-failed" => fs::write(root.join("custody-write"), "before"),
            "claim-reply-lost" => fs::write(root.join("custody-write"), "lost"),
            "delete-reply-lost" => fs::write(root.join("lose-cleanup-reply"), "1"),
            _ => Ok(()),
        }
        .unwrap();
        let result = engine
            .prove_paste_buffer_io(&read, ProbeSource::CatalogFree)
            .await;
        assert_eq!(
            result.is_ok(),
            matches!(case, "healthy" | "claim-reply-lost" | "delete-reply-lost"),
            "{case}: {result:?}"
        );
        let loads = fs::read(root.join("probe-loads")).unwrap_or_default();
        let loaded = !matches!(case, "missing-flags" | "claim-hidden" | "claim-failed");
        assert_eq!(
            loads,
            if loaded { b"%0\n".as_slice() } else { b"" },
            "{case}"
        );
        let deletions = fs::read_to_string(root.join("deletions")).unwrap_or_default();
        fs::write(root.join("custody-snapshot"), "").unwrap();
        let slots = engine.read_custody(read.server).await.unwrap();
        let slot = slots[0].as_ref();
        match case {
            "missing-flags" | "claim-failed" => assert!(slot.is_none() && deletions.is_empty()),
            "claim-hidden" | "delete-failed" => {
                let slot = slot.unwrap();
                let active = slot
                    .active
                    .as_ref()
                    .and_then(CustodyBinding::probe)
                    .unwrap();
                assert_eq!(active.completed, case == "delete-failed");
                assert_eq!(active.target, read.target);
                assert_eq!(active.tty, read.tty);
                assert_eq!(active.source.encode(), "local");
                assert_eq!(root.join(&active.buffer).exists(), active.completed);
                assert!(deletions.is_empty());
                let repeat = engine
                    .prove_paste_buffer_io(&read, ProbeSource::CatalogFree)
                    .await;
                assert!(repeat.is_err());
                assert_eq!(
                    engine.read_custody(read.server).await.unwrap()[0]
                        .as_ref()
                        .unwrap()
                        .value,
                    slot.value
                );
                assert_eq!(
                    fs::read(root.join("probe-loads")).unwrap_or_default(),
                    loads
                );
            }
            _ => {
                assert!(slot.unwrap().active.is_none());
                let buffer = fs::read_to_string(root.join("last-load")).unwrap();
                assert_eq!(deletions, format!("{buffer}\n"));
                assert!(!root.join(buffer).exists());
            }
        }
        for effect in ["launch", "marker", "pane0-bytes", "submissions", "effects"] {
            assert!(!root.join(effect).exists(), "{case}: {effect}");
        }
    }
}

#[tokio::test]
async fn w1_probe_custody_refuses_invalid_mixed_snapshots_and_stale_originals() {
    let fixture = Fixture::new();
    let engine = &fixture.engine;
    let root = fixture.temp.path();
    let ready = engine.scoped_geometry(&fixture.terminal).await.unwrap();
    let epoch = ResizeEpoch {
        agent: Uuid::new_v4(),
        owner: Uuid::new_v4(),
    };
    let mut payload_read = ready.clone();
    payload_read.epoch = Some(epoch);
    payload_read.target.runtime_id = Uuid::new_v4();
    payload_read.target.pane = "%5".into();
    payload_read.name = tmux_name(Uuid::new_v4(), payload_read.target.runtime_id);
    let buffer = PasteBuffer {
        nonce: Uuid::new_v4(),
        operation: 1,
        name: format!("rs-paste-{}", Uuid::new_v4().simple()),
    };
    let payload = CustodySlot::bind(&payload_read, 1, &buffer, "possible").unwrap();
    for option in ["launch", "marker"] {
        fs::remove_file(root.join(option)).unwrap();
    }
    let read = engine.scoped_geometry(&fixture.terminal).await.unwrap();
    let server = read.server;
    let probe = CustodySlot::bind_probe(&read, 0, ProbeSource::CatalogFree).unwrap();
    let probe_value = probe.value.as_str();
    fs::create_dir(root.join("custody")).unwrap();
    let path = |index| root.join("custody").join(custody_option(server, index));
    fs::write(path(0), probe_value).unwrap();
    fs::write(path(1), &payload.value).unwrap();
    let mixed = engine.read_custody(server).await.unwrap();
    assert_eq!(mixed[0].as_ref().unwrap().value, probe_value);
    assert_eq!(mixed[1].as_ref().unwrap().value, payload.value);

    let distinct = format!("v1~{}~free", Uuid::new_v4());
    let replacement = probe_value.replace("v2~", "v3~");
    let wrong_generation =
        probe_value.replace(&server.token.to_string(), &Uuid::new_v4().to_string());
    let oversized = "x".repeat(CUSTODY_VALUE_LIMIT + 1);
    let extra = format!("{probe_value}~extra");
    let wrong_buffer = probe_value.replace("rs-paste-probe-", "rs-paste-");
    let mut duplicate_runtime = payload_read;
    duplicate_runtime.target.runtime_id = read.target.runtime_id;
    duplicate_runtime.name = read.name.clone();
    let duplicate_runtime = CustodySlot::bind(&duplicate_runtime, 1, &buffer, "possible").unwrap();
    for (first, second) in [
        (replacement.as_str(), payload.value.as_str()),
        (wrong_generation.as_str(), payload.value.as_str()),
        (oversized.as_str(), payload.value.as_str()),
        (extra.as_str(), payload.value.as_str()),
        (wrong_buffer.as_str(), payload.value.as_str()),
        (probe_value, duplicate_runtime.value.as_str()),
    ] {
        fs::write(path(0), first).unwrap();
        fs::write(path(1), second).unwrap();
        assert!(
            engine
                .prove_paste_buffer_io(&read, ProbeSource::CatalogFree)
                .await
                .is_err()
        );
        assert_eq!(fs::read_to_string(path(0)).unwrap(), first);
        assert_eq!(fs::read_to_string(path(1)).unwrap(), second);
    }

    fs::write(path(0), &distinct).unwrap();
    fs::write(path(1), &payload.value).unwrap();
    let stale_snapshot = engine.read_custody(server).await.unwrap();
    let active = probe
        .active
        .as_ref()
        .and_then(CustodyBinding::probe)
        .unwrap();
    assert!(
        engine
            .load_paste(&read, b"stale claim", &probe)
            .await
            .is_err()
    );
    assert!(
        engine
            .retire_custody(&read, &probe, Some(&active.buffer))
            .await
            .is_err()
    );
    let mut wrong = read.clone();
    wrong.target.process_start += 1;
    assert!(
        engine
            .mutate_custody(&wrong, stale_snapshot[0].as_ref(), &probe, None)
            .await
            .is_err()
    );
    wrong = read.clone();
    wrong.target.window = "@9".into();
    assert!(
        engine
            .mutate_custody(&wrong, stale_snapshot[0].as_ref(), &probe, None)
            .await
            .is_err()
    );
    wrong = read.clone();
    wrong.tty = PathBuf::from("/dev/ttys999999");
    assert!(
        engine
            .mutate_custody(&wrong, stale_snapshot[0].as_ref(), &probe, None)
            .await
            .is_err()
    );
    assert_eq!(fs::read_to_string(path(0)).unwrap(), distinct);
    assert_eq!(fs::read_to_string(path(1)).unwrap(), payload.value);
    for index in 0..CUSTODY_SLOTS {
        let mut other = read.clone();
        other.target.runtime_id = Uuid::new_v4();
        other.target.pane = format!("%{}", index + 1);
        other.name = tmux_name(Uuid::new_v4(), other.target.runtime_id);
        let occupied = CustodySlot::bind_probe(&other, index, ProbeSource::CatalogFree).unwrap();
        fs::write(path(index), occupied.value).unwrap();
    }
    assert!(
        engine
            .read_custody(server)
            .await
            .unwrap()
            .iter()
            .all(|slot| slot.as_ref().is_some_and(|slot| slot.active.is_some()))
    );
    assert!(
        engine
            .prove_paste_buffer_io(&read, ProbeSource::CatalogFree)
            .await
            .is_err()
    );
    for effect in [
        "probe-loads",
        "deletions",
        "custody-writes",
        "pane0-bytes",
        "submissions",
        "effects",
    ] {
        assert!(!root.join(effect).exists(), "{effect}");
    }
}
