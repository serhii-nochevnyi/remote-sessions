use super::*;
use crate::{
    catalog::{Operation, OperationStage, ServerGeneration},
    model::{CreateState, PaneLiveness, ProviderProfile, RuntimeOrigin, tmux_name},
    product_launch::{LAUNCH_COMMAND, LaunchGuard, NONCE_ENV, read_setup_fault},
};
use tokio::task::JoinHandle;
use uuid::Uuid;

const TMUX: &str = r#"#!/bin/sh
set -eu
cd "$(dirname "$3")"
token=""
if test -f generation; then token=$(cat generation); fi
case "$4" in
  display-message)
    printf '42|100|%s\n' "$token"
    if test "$#" -gt 6; then
      if test -f names; then cat names; fi
    fi ;;
  if-shell)
    case "$7" in
      'set-option -s @rs_create_generation '*)
        test -z "$token"
        printf '%s\n' "$7" | awk '{print $4}' > generation ;;
      'new-session '*)
        branch=$7
        name=$(printf '%s\n' "$branch" | awk '{for(i=1;i<NF;i++) if ($i=="-s") {print $(i+1); exit}}')
        nonce=$(printf '%s\n' "$branch" | awk '{for(i=1;i<=NF;i++) if ($i~/^RS_CREATE_NONCE=/) {sub(/^RS_CREATE_NONCE=/,"",$i); print $i; exit}}')
        case "$6" in *"$token"*) ;; *) exit 0 ;; esac
        test -n "$name"
        test -n "$nonce"
        case "$branch" in *"__managed-launch"*"-Fp"*"$name:0.0"*) ;; *) exit 1 ;; esac
        printf '%s %s\n' "$name" "$nonce" >> dispatched
        mode=normal
        if test -f mode; then mode=$(cat mode); fi
        if test "$mode" = delay; then exec /bin/sleep 20; fi
        count=$(wc -l < dispatched | tr -d ' ')
        printf '%s\n' "$name" >> names
        printf '%s\n' "$nonce" > "$name.nonce"
        printf '$%s|@%s|%%%s|%s\n' "$count" "$count" "$count" "$((300 + count))" > "$name.pane"
        tr '|' ':' < "$name.pane" > "$name.stamp"
        printf '0\n' > "$name.dead"
        if test "$mode" = exited; then printf '1\n' > "$name.dead"; fi
        if test "$mode" = effect-lost; then exit 1; fi
        cat "$name.pane" ;;
      *) exit 1 ;;
    esac ;;
  show-environment)
    name=$(printf '%s\n' "$6" | cut -c2-)
    nonce=$(cat "$name.nonce")
    pane=$(cat "$name.pane")
    stamp=$(cat "$name.stamp")
    dead=$(cat "$name.dead")
    printf 'RS_CREATE_NONCE=%s\n42|100|%s|%s|%s|0|0|%s:%s|%s|100|30\n' "$nonce" "$token" "$pane" "$name" "$nonce" "$stamp" "$dead" ;;
  list-sessions)
    if test -f names; then cat names; fi ;;
  list-panes)
    for name in $(cat names); do
      IFS='|' read -r session window pane pid < "$name.pane"
      printf '%s\t%s\t%s\t%s\t100\t30\tfixture\n' "$name" "$pane" "$pid" "$(cat "$name.dead")"
    done ;;
  *) exit 1 ;;
esac
"#;

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    catalog: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in("/tmp")
            .unwrap();
        let root = temp.path().canonicalize().unwrap();
        let workspace = root.join("Папка #{} $' ;");
        fs::create_dir(&workspace).unwrap();
        let catalog = root.join("catalog");
        catalog::provision(&catalog).unwrap();
        tests::fixture_program(&root.join("tmux"), TMUX);
        tests::fixture_program(&root.join("codex"), "#!/bin/sh\nexit 0\n");
        Self {
            _temp: temp,
            root,
            catalog,
            workspace,
        }
    }

    fn lane(&self) -> CatalogLane {
        let catalog = self.catalog.clone();
        let path = std::env::join_paths([&self.root]).unwrap();
        CatalogLane::start(move || Ok(catalog), ServiceProfiles::new(Some(&path)))
    }

    async fn serve(&self, lane: CatalogLane) -> JoinHandle<Result<()>> {
        let job = tokio::spawn(serve_local(
            self.root.clone(),
            self.root.join("tmux"),
            Some(lane),
        ));
        tokio::time::timeout(Duration::from_secs(3), async {
            while !self.root.join("agent.sock").exists() {
                assert!(!job.is_finished());
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        job
    }

    async fn restart(&self, job: JoinHandle<Result<()>>) -> JoinHandle<Result<()>> {
        stop(job).await;
        let file = File::open(self.catalog.join("catalog.lock")).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while file.try_lock().is_err() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        drop(file);
        self.serve(self.lane()).await
    }

    async fn register(&self) -> Uuid {
        let ProductResponse::WorkspaceRegistered { workspace } = product_request(
            &self.root,
            &ProductRequest::RegisterWorkspace {
                locator: self.workspace.to_str().unwrap().into(),
            },
        )
        .await
        .unwrap() else {
            panic!("expected workspace")
        };
        workspace.workspace_id
    }

    async fn call(&self, request: &ProductRequest) -> CreateState {
        let ProductResponse::Create { state } = product_request(&self.root, request).await.unwrap()
        else {
            panic!("expected create response")
        };
        state
    }

    async fn status(&self, key: Uuid) -> CreateState {
        self.call(&ProductRequest::CreateStatus { key }).await
    }

    fn set(&self, path: &str, content: impl AsRef<[u8]>) {
        fs::write(self.root.join(path), content).unwrap();
    }

    fn dispatches(&self) -> usize {
        fs::read_to_string(self.root.join("dispatched")).map_or(0, |file| file.lines().count())
    }

    async fn reserve(&self, lane: &CatalogLane, workspace_id: Uuid) -> (Uuid, Uuid) {
        let token = Uuid::new_v4();
        self.set("generation", token.to_string());
        let key = Uuid::new_v4();
        let runtime_id = Uuid::new_v4();
        lane.run(move |db, service| {
            let profile = ProviderProfile::Codex;
            let (locator, policy) = db.preflight(workspace_id, profile, service)?;
            let session_id = Uuid::new_v4();
            assert!(db.reserve(&Operation {
                key,
                id: Uuid::new_v4(),
                workspace_id,
                profile,
                session_id,
                runtime_id,
                target: tmux_name(session_id, runtime_id),
                nonce: Uuid::new_v4(),
                generation: ServerGeneration {
                    pid: 42,
                    start: 100,
                    token
                },
                locator,
                policy,
                stage: OperationStage::Reserved,
            })?);
            Ok(())
        })
        .await
        .unwrap();
        (key, runtime_id)
    }
}

async fn stop(job: JoinHandle<Result<()>>) {
    job.abort();
    assert!(job.await.unwrap_err().is_cancelled());
}

fn create(key: Uuid, workspace_id: Uuid, reservation: Option<Uuid>) -> ProductRequest {
    ProductRequest::CreateManaged {
        key,
        workspace_id,
        profile: ProviderProfile::Codex,
        reservation,
    }
}

#[tokio::test]
async fn w1_create_concurrent_equal_keys_finalize_once_and_preserve_original_result() {
    let fixture = Fixture::new();
    let job = fixture.serve(fixture.lane()).await;
    let workspace = fixture.register().await;
    let key = Uuid::new_v4();
    assert_eq!(fixture.status(key).await, CreateState::Absent {});
    let request = create(key, workspace, None);
    let (first, second) = tokio::join!(fixture.call(&request), fixture.call(&request));
    assert_eq!(first, second);
    let CreateState::Confirmed {
        outcome,
        current: PaneLiveness::Live,
        ..
    } = first
    else {
        panic!("expected one confirmed launch")
    };
    let runtime = outcome.runtime.clone();
    assert_eq!(fixture.dispatches(), 1);
    assert_eq!(runtime.origin, RuntimeOrigin::Managed);
    assert_eq!(runtime.workspace_id, Some(workspace));
    let conflict = ProductRequest::CreateManaged {
        key,
        workspace_id: workspace,
        profile: ProviderProfile::ClaudeCode,
        reservation: None,
    };
    assert_eq!(
        fixture.call(&conflict).await,
        CreateState::Failed {
            issue: ProductIssue::CreateConflict
        }
    );
    assert_eq!(
        fixture
            .call(&create(key, workspace, Some(Uuid::new_v4())))
            .await,
        CreateState::Failed {
            issue: ProductIssue::CreateConflict
        }
    );

    fs::rename(&fixture.workspace, fixture.root.join("moved")).unwrap();
    fixture.set(&format!("{}.dead", runtime.tmux_name), "1\n");
    let job = fixture.restart(job).await;
    assert_eq!(
        fixture.call(&request).await,
        CreateState::Confirmed {
            outcome: outcome.clone(),
            current: PaneLiveness::Exited,
            setup_issue: None,
        }
    );
    // Replaced pane evidence affects the current observation without rewriting history.
    fixture.set(&format!("{}.pane", runtime.tmux_name), "$1|@1|%9|309\n");
    assert_eq!(
        fixture.status(key).await,
        CreateState::Confirmed {
            outcome,
            current: PaneLiveness::Unavailable,
            setup_issue: None,
        }
    );
    assert_eq!(fixture.dispatches(), 1);
    stop(job).await;
}

#[tokio::test]
async fn w1_create_reserved_retry_uses_original_ids_and_absent_retry_cannot_allocate() {
    let fixture = Fixture::new();
    let lane = fixture.lane();
    let job = fixture.serve(lane.clone()).await;
    let workspace = fixture.register().await;
    let missing = create(Uuid::new_v4(), workspace, Some(Uuid::new_v4()));
    assert!(matches!(
        fixture.call(&missing).await,
        CreateState::Unknown { .. }
    ));
    let invalid = create(Uuid::new_v4(), Uuid::new_v4(), None);
    assert_eq!(
        fixture.call(&invalid).await,
        CreateState::Failed {
            issue: ProductIssue::WorkspaceUnknown
        }
    );
    for fields in [
        serde_json::json!({"profile": "shell"}),
        serde_json::json!({"argv": ["arbitrary"]}),
        serde_json::json!({"cwd": "/tmp"}),
        serde_json::json!({"env": {"PATH": "/tmp"}}),
    ] {
        let mut request = serde_json::json!({"op":"create_managed", "key":Uuid::new_v4(),
            "workspace_id":workspace, "profile":"codex"});
        request
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        let response: ProductEnvelope<ProductResponse> =
            exchange(&fixture.root, &serde_json::json!({"product": request}))
                .await
                .unwrap();
        assert_eq!(
            response.product,
            ProductResponse::Blocked {
                issue: ProductIssue::InvalidRequest
            }
        );
    }
    let (key, id) = fixture.reserve(&lane, workspace).await;
    assert_eq!(
        fixture.status(key).await,
        CreateState::Pending { reservation: id }
    );
    assert_eq!(fixture.dispatches(), 0);
    let CreateState::Confirmed { outcome, .. } =
        fixture.call(&create(key, workspace, Some(id))).await
    else {
        panic!("reserved runtime did not launch")
    };
    assert_eq!(outcome.runtime.runtime_id, id);
    assert_eq!(fixture.dispatches(), 1);
    stop(job).await;
}

#[tokio::test]
async fn w1_create_catalog_corruption_before_marker_fails_closed_and_keeps_evidence() {
    let fixture = Fixture::new();
    let lane = fixture.lane();
    let job = fixture.serve(lane.clone()).await;
    let workspace = fixture.register().await;
    let (key, id) = fixture.reserve(&lane, workspace).await;
    let path = fixture.catalog.join("catalog.sqlite3");
    let original = fs::read(&path).unwrap();
    let mut damaged = original.clone();
    damaged[0] ^= 0x80;
    fs::write(&path, &damaged).unwrap();
    assert_eq!(
        fixture.call(&create(key, workspace, Some(id))).await,
        CreateState::Unknown {
            issue: Some(ProductIssue::CatalogUnavailable)
        }
    );
    assert_eq!(fixture.dispatches(), 0);
    assert_eq!(fs::read(path).unwrap(), damaged);
    stop(job).await;
}

#[tokio::test]
async fn w1_create_uncertain_marker_ack_and_after_marker_before_send_are_status_only() {
    let fixture = Fixture::new();
    let lane = fixture.lane();
    let job = fixture.serve(lane.clone()).await;
    let workspace = fixture.register().await;
    let (key, id) = fixture.reserve(&lane, workspace).await;
    // Simulate a durable commit followed by loss of the worker's acknowledgement.
    let timed_out = lane
        .run(move |db, _| {
            assert!(db.mark(key, id)?);
            std::thread::sleep(Duration::from_millis(2150));
            Ok(())
        })
        .await;
    assert_eq!(timed_out, Err(ProductIssue::WorkspaceCheckTimedOut));
    for attempt in 0..2 {
        if attempt == 1 {
            fixture.set("generation", Uuid::new_v4().to_string());
        }
        assert!(matches!(
            fixture.status(key).await,
            CreateState::Unknown { .. }
        ));
        assert!(matches!(
            fixture.call(&create(key, workspace, None)).await,
            CreateState::Unknown { .. }
        ));
    }
    let won_again = lane.run(move |db, _| db.mark(key, id)).await.unwrap();
    assert!(!won_again);
    assert_eq!(fixture.dispatches(), 0);
    stop(job).await;
}

#[tokio::test]
async fn w1_create_effect_before_reply_requires_exact_generation_nonce_and_pane() {
    let fixture = Fixture::new();
    let job = fixture.serve(fixture.lane()).await;
    let workspace = fixture.register().await;
    let key = Uuid::new_v4();
    fixture.set("mode", "effect-lost");
    assert!(matches!(
        fixture.call(&create(key, workspace, None)).await,
        CreateState::Unknown { .. }
    ));
    assert_eq!(fixture.dispatches(), 1);
    let name = fs::read_to_string(fixture.root.join("names")).unwrap();
    let name = name.trim_end();
    let nonce_path = format!("{name}.nonce");
    let pane_path = format!("{name}.pane");
    let nonce = fs::read(fixture.root.join(&nonce_path)).unwrap();
    let pane = fs::read(fixture.root.join(&pane_path)).unwrap();
    let generation = fs::read(fixture.root.join("generation")).unwrap();
    for (file, invalid, original) in [
        (nonce_path.as_str(), Uuid::new_v4().to_string(), &nonce),
        (pane_path.as_str(), "$1|@1|%99|301\n".into(), &pane),
        ("generation", Uuid::new_v4().to_string(), &generation),
    ] {
        fixture.set(file, invalid);
        assert!(matches!(
            fixture.status(key).await,
            CreateState::Unknown { .. }
        ));
        fixture.set(file, original);
    }
    fs::remove_file(fixture.root.join(&nonce_path)).unwrap();
    assert!(matches!(
        fixture.status(key).await,
        CreateState::Unknown { .. }
    ));
    fixture.set(&nonce_path, nonce);
    fixture.set(&format!("{name}.dead"), "1\n");
    let job = fixture.restart(job).await;
    let CreateState::Confirmed {
        outcome,
        current: PaneLiveness::Exited,
        ..
    } = fixture.status(key).await
    else {
        panic!("expected retained pane")
    };
    assert!(outcome.runtime.exited);
    assert_eq!(
        crate::model::parse_name(name),
        Some((outcome.runtime.session_id, outcome.runtime.runtime_id))
    );
    assert_eq!(fixture.dispatches(), 1);
    stop(job).await;
}

#[tokio::test]
async fn w1_create_delayed_dispatch_times_out_and_does_not_send_again() {
    let fixture = Fixture::new();
    let job = fixture.serve(fixture.lane()).await;
    let workspace = fixture.register().await;
    let key = Uuid::new_v4();
    fixture.set("mode", "delay");
    let request = create(key, workspace, None);
    assert!(matches!(
        fixture.call(&request).await,
        CreateState::Unknown { .. }
    ));
    fs::remove_file(fixture.root.join("mode")).unwrap();
    assert!(matches!(
        fixture.status(key).await,
        CreateState::Unknown { .. }
    ));
    assert!(matches!(
        fixture.call(&request).await,
        CreateState::Unknown { .. }
    ));
    assert_eq!(fixture.dispatches(), 1);
    assert!(!fixture.root.join("names").exists());
    stop(job).await;
}

#[tokio::test]
async fn w1_create_sqlite_full_blocks_failed_marker_or_finalization_and_preserves_legacy() {
    for full_at in ["marked", "finished"] {
        let fixture = Fixture::new();
        let lane = fixture.lane();
        let job = fixture.serve(lane.clone()).await;
        let workspace = fixture.register().await;
        let (key, id) = fixture.reserve(&lane, workspace).await;
        let legacy = [
            tmux_name(Uuid::new_v4(), Uuid::new_v4()),
            tmux_name(Uuid::new_v4(), Uuid::new_v4()),
        ];
        fixture.set("names", format!("{}\n{}\n", legacy[0], legacy[1]));
        for (index, name) in legacy.iter().enumerate() {
            fixture.set(
                &format!("{name}.pane"),
                format!("$9|@9|%{}|{}\n", 9 + index, 90 + index),
            );
            fixture.set(&format!("{name}.dead"), format!("{index}\n"));
        }
        lane.run(move |db, _| {
            let sqlite = db.connection_for_test();
            sqlite.execute_batch(&format!(
                "CREATE TABLE scratch (value BLOB); CREATE TRIGGER prevent_advance AFTER UPDATE ON create_operations
                   WHEN NEW.stage = '{full_at}' BEGIN INSERT INTO scratch VALUES (zeroblob(1048576)); END;"
            )).unwrap();
            let pages: i64 = sqlite.pragma_query_value(None, "page_count", |row| row.get(0)).unwrap();
            sqlite.pragma_update(None, "max_page_count", pages).unwrap();
            Ok(())
        }).await.unwrap();
        assert_eq!(
            fixture.call(&create(key, workspace, Some(id))).await,
            CreateState::Unknown {
                issue: Some(ProductIssue::CatalogUnavailable)
            }
        );
        let count = usize::from(full_at == "finished");
        assert_eq!(fixture.dispatches(), count);
        let Response::Runtimes { runtimes } =
            request(&fixture.root, &Request::List {}).await.unwrap()
        else {
            panic!("legacy inventory unavailable after SQLite fault");
        };
        for (index, name) in legacy.iter().enumerate() {
            let runtime = runtimes
                .iter()
                .find(|runtime| runtime.tmux_name == *name)
                .unwrap();
            assert_eq!(runtime.origin, RuntimeOrigin::Legacy);
            assert_eq!((runtime.workspace_id, runtime.operation_id), (None, None));
            assert_eq!(runtime.exited, index == 1);
        }
        assert!(matches!(
            fixture.status(key).await,
            CreateState::Unknown { .. }
        ));
        assert_eq!(fixture.dispatches(), count);
        stop(job).await;
        drop(lane);
        let db = rusqlite::Connection::open(fixture.catalog.join("catalog.sqlite3")).unwrap();
        let stage: String = db
            .query_row(
                "SELECT stage FROM create_operations WHERE key = ?1",
                [key.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stage,
            if full_at == "marked" {
                "reserved"
            } else {
                "marked"
            }
        );
    }
}

#[test]
#[ignore = "launched only in the guarded child process fixture"]
fn launcher_child_fixture() {
    let policy = std::env::var_os("RS_W1_LAUNCH_POLICY").unwrap();
    let result = product_launch::managed_launch_argument(&[LAUNCH_COMMAND.into(), policy]).unwrap();
    assert!(result.is_err());
    std::process::exit(17);
}

#[test]
fn w1_create_guard_rejects_tmux_fallback_and_replaced_cwd_before_provider_exec() {
    let fixture = Fixture::new();
    let witness = fixture.root.join("provider-started");
    tests::fixture_program(
        &fixture.root.join("codex"),
        &format!(
            "#!/bin/sh\ntest -z \"$RS_CREATE_NONCE\" || exit 20\n/bin/pwd > '{}'\n",
            witness.display()
        ),
    );
    let path = std::env::join_paths([&fixture.root]).unwrap();
    let profiles = ServiceProfiles::new(Some(&path));
    let workspace = product_launch::resolve_workspace(&fixture.workspace).unwrap();
    let profile = ProviderProfile::Codex;
    let guard = LaunchGuard {
        locator: fixture.workspace.to_str().unwrap().into(),
        profile,
        policy: profiles
            .preflight(&fixture.workspace, &workspace, profile)
            .unwrap(),
        service_state: fixture.root.clone(),
        nonce: Uuid::new_v4(),
    };
    let run = |directory: &Path, guard: &LaunchGuard| {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "w1_create_tests::launcher_child_fixture",
                "--ignored",
            ])
            .env("RS_W1_LAUNCH_POLICY", guard.argument().unwrap())
            .env(NONCE_ENV, guard.nonce.to_string())
            .current_dir(directory)
            .output()
            .unwrap()
            .status
            .code()
    };
    assert_eq!(run(&fixture.workspace, &guard), Some(0));
    assert_eq!(
        fs::read_to_string(&witness).unwrap().trim_end(),
        fixture.workspace.to_str().unwrap()
    );
    fs::remove_file(&witness).unwrap();
    let moved = fixture.root.join("moved");
    fs::rename(&fixture.workspace, moved).unwrap();
    assert_eq!(run(&fixture.root, &guard), Some(17));
    assert_eq!(
        read_setup_fault(&fixture.root, guard.nonce),
        Some(ProductIssue::WorkspaceChanged)
    );
    fs::create_dir(&fixture.workspace).unwrap();
    let guard = LaunchGuard {
        nonce: Uuid::new_v4(),
        ..guard
    };
    assert_eq!(run(&fixture.workspace, &guard), Some(17));
    assert_eq!(
        read_setup_fault(&fixture.root, guard.nonce),
        Some(ProductIssue::WorkspaceChanged)
    );
    assert!(!witness.exists());
}
