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

struct Fixture {
    engine: Engine,
    terminal: TerminalView,
    temp: tempfile::TempDir,
    _master: OwnedFd,
    _slave: OwnedFd,
}

fn test_pty() -> (OwnedFd, OwnedFd, PathBuf) {
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
    (master, slave, tty)
}

impl Fixture {
    fn new() -> Self {
        let (master, slave, tty) = test_pty();
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let root = temp.path();
        let runtime_id = Uuid::new_v4();
        let session = Uuid::new_v4();
        let name = tmux_name(session, runtime_id);
        let generation = Uuid::new_v4();
        let pid = std::process::id();
        let tmux = root.join("tmux-fixture");
        let socket = root.join("tmux.sock");
        let engine = Engine::new(tmux.clone(), socket);
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
        let script = r#"#!/bin/sh
set -eu
root=$(dirname "$0")
read -r cols rows < "$root/grid" || true
epoch=$(cat "$root/epoch")
intent=$(cat "$root/intent")
mode=$(cat "$root/mode")
case "$4" in
  display-message)
    printf '%s|%s|%s|%s|%s|%s|%s\n' 'PREFIX' "$cols" "$rows" "$cols" "$rows" "$epoch" "$intent"
    ;;
  if-shell)
    printf '%s\n%s\n' "$8" "$9" > "$root/last-write"
    case "$9" in
      resize-window*)
        case "$mode" in
          stale) exit 0 ;;
          lost-before) exit 1 ;;
        esac
        set -- $9
        printf '%s %s\n' "$5" "$7" > "$root/grid"
        printf '%s' "${14}" > "$root/intent"
        test "$mode" != lost-after || exit 1
        printf '%s\n' rs-committed
        ;;
      set-option*)
        set -- $9
        for word; do
          if [ "${previous:-}" = '@rs_resize_epoch' ]; then
            printf '%s' "$word" > "$root/epoch"
            break
          fi
          previous=$word
        done
        printf '%s\n' rs-fenced
        ;;
      *) exit 91 ;;
    esac
    ;;
  *) exit 92 ;;
esac
"#;
        let prefix = format!(
            "987|123456|{generation}|$0|@0|%0|{pid}|{name}|0|0||0|1|1|manual|{}",
            tty.display()
        );
        fs::write(&tmux, script.replace("PREFIX", &prefix)).unwrap();
        fs::set_permissions(tmux, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            engine,
            terminal,
            temp,
            _master: master,
            _slave: slave,
        }
    }

    async fn acquire(&self, terminal: TerminalView) -> (Uuid, ResizeEpoch) {
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

        let restarted = Engine::new(f.engine.tmux.clone(), f.engine.socket.clone());
        let terminal = TerminalView {
            agent_epoch: restarted.agent_epoch,
            ..f.terminal.clone()
        };
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
        let applied =
            unsafe { libc::ioctl(f._master.as_raw_fd(), libc::TIOCSWINSZ, &raw const winsize) };
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
