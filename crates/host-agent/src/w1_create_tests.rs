use super::*;
use crate::{
    catalog::{Operation, OperationStage, ServerGeneration},
    model::{
        CreateState, ObservationState, PaneLiveness, PasteBinding, PasteResult, ProviderProfile,
        RuntimeOrigin, TerminalSize, TerminalStatus, tmux_name,
    },
    product_launch::{LAUNCH_COMMAND, LaunchGuard, NONCE_ENV, read_setup_fault, resolve_workspace},
};
use std::{
    fs::TryLockError,
    io::{Error, ErrorKind},
};
use tokio::{sync::mpsc, task::JoinHandle};
use uuid::Uuid;

const TMUX: &str = r##"#!/bin/sh
set -eu
cd "$(dirname "$3")"
token=""
if test -f generation; then token=$(cat generation); fi
guard_equals() {
  case "$guard" in *"#{==:#{$1},$2}"*) return 0 ;; *) return 1 ;; esac
}
option_value() {
  if test -f "$1"; then cat "$1"; fi
}
valid_buffer() {
  case "$1" in rs-paste-*) ;; *) return 1 ;; esac
  id=${1#rs-paste-}
  id=${id#probe-}
  test "${#id}" = 32 || return 1
  case "$id" in *[!0-9a-f]*) return 1 ;; esac
}
custody_option() {
  compact=$(printf '%s' "$token" | tr -d '-')
  printf '@rs_paste_custody_42_100_%s_%02d' "$compact" "$1"
}
custody_snapshot() {
  guard_equals pid 42 && guard_equals start_time 100 &&
    guard_equals @rs_create_generation "$token" || return 0
  for hook in after-if-shell after-show-options after-display-message command-error; do
    guard_equals "$hook" "" || return 0
  done
  set -f
  set -- $1
  index=0
  while test "$index" -lt 16; do
    option=$(custody_option "$index")
    test "$1 $2 $3" = 'show-options -sq -F'
    test "$4 $5" = "'#{option_name} #{?#{m/r:^[a-zA-Z0-9:/_~-]+\$,#{option_value}},#{option_value},invalid}'"
    test "$6 $7" = "$option ;"
    if test -f "custody/$option"; then
      value=$(cat "custody/$option"; printf .)
      value=${value%.}
      case "$value" in ''|*[!a-zA-Z0-9:/_~-]*) value=invalid ;; esac
      printf '%s %s\n' "$option" "$value"
    fi
    shift 7
    index=$((index + 1))
  done
  test "$*" = 'display-message -p -l rs-custody-snapshot'
  printf 'rs-custody-snapshot\n'
}
case "$4" in
  display-message)
    if test "${6-}" = -t; then
      if test "$#" = 8 && test -f geometry-1; then
        printf '%s\n' "$7" >> geometry-samples
        sample=$(awk 'END {print NR}' geometry-samples)
        cat "geometry-$sample"
        exit
      fi
      for name in $(cat names); do
        IFS='|' read -r session window pane pid < "$name.pane"
        if test "$pane" != "$7"; then continue; fi
        if test "$#" = 24; then
          format='#{pid}|#{start_time}|#{@rs_create_generation}|#{session_id}|#{window_id}|#{pane_id}|#{pane_pid}|#{session_name}|#{pane_dead}|#{@rs_create_nonce}'
          test "$8" = "$format" && test "${24}" = "$format"
          test "$9 ${10} ${11} ${12} ${13} ${14} ${15} ${16} ${17} ${18} ${19} ${20} ${21} ${22} ${23}" = "; show-environment -s -t =$name ; show-options -p -t $pane ; display-message -p -t $pane"
          nonce=$(option_value "$name.nonce")
          stamp=$(option_value "$name.stamp")
          marker=""
          if test -n "$stamp"; then marker="$nonce:$stamp"; fi
          row="$(cat "$name.pane")|$name|$(cat "$name.dead")|$marker"
          printf '42|100|%s|%s\n' "$token" "$row"
          test ! -f "$name.inventory-fault" || exit 1
          if test -f "$name.nonce"; then printf "RS_CREATE_NONCE='%s'; export RS_CREATE_NONCE;\n" "$nonce"; fi
          if test -f "$name.stamp"; then printf "@rs_create_nonce '%s'\n" "$marker"; fi
          if test -f "$name.inventory-replacement"; then row=$(cat "$name.inventory-replacement"); fi
          next_token=$token
          if test -f "$name.inventory-generation"; then next_token=$(cat "$name.inventory-generation"); fi
          printf '42|100|%s|%s\n' "$next_token" "$row"
          exit 0
        fi
        if test "$8" = '#{@rs_create_initializer}|#{@rs_create_birth}'; then
          printf '%s|%s\n' "$(option_value "$name.initializer")" "$(option_value "$name.birth")"
          exit 0
        fi
        nonce=$(option_value "$name.nonce")
        stamp=$(option_value "$name.stamp")
        proof=""
        if test -n "$stamp"; then proof="$nonce:$stamp"; fi
        epoch=$(cat epoch 2>/dev/null || :)
        launch=$(cat paste-launch 2>/dev/null || :)
        marker=$(cat marker 2>/dev/null || :)
        case "$launch" in ??????????????8*) launch="$launch~$(option_value "$name.birth")" ;; esac
        printf '42|100|%s|%s|%s|0|0|%s|%s|1|1|manual|%s|100|30|100|30|%s||%s|%s|0|1\n' \
          "$token" "$(cat "$name.pane")" "$name" "$proof" "$(cat "$name.dead")" \
          "$(cat tty)" "$epoch" "$launch" "$marker"
        exit 0
      done
      exit 1
    fi
    printf '42|100|%s\n' "$token"
    if test "$#" -gt 6; then
      if test -f names; then cat names; fi
    fi ;;
  if-shell)
    if test "$6" = -t; then
      case "$9" in
        'set-option -s'*' @rs_paste_custody_'*|'delete-buffer '*' ; set-option -s @rs_paste_custody_'*)
          test "$5" = -F && test "$#" = 9 && test "$3" = "$PWD/tmux.sock"
          guard=$8
          target=$7
          set -f
          set -- $9
          cleanup=""
          if test "$1" = delete-buffer; then
            test "$#" = 8 && test "$2" = -b && valid_buffer "$3" && test "$4" = ';'
            cleanup=$3
            shift 4
          fi
          test "$#" = 4 && test "$1" = set-option
          flag=$2
          option=$3
          value=$4
          index=0
          while test "$index" -lt 16 && test "$option" != "$(custody_option "$index")"; do
            index=$((index + 1))
          done
          test "$index" -lt 16
          prior=$(option_value "custody/$option"; printf .)
          prior=${prior%.}
          guard_equals "$option" "$prior" || exit 0
          case "$flag" in
            -so) test ! -e "custody/$option" || exit 1 ;;
            -s) test -n "$prior" || exit 1 ;;
            *) exit 1 ;;
          esac
          test "${#value}" -le 2048
          case "$value" in *[!a-zA-Z0-9:/_~-]*) exit 1 ;; esac
          version=${value%%~*}
          rest=${value#*~}
          mutation=${rest%%~*}
          state=${rest#*~}
          test "$version" = v1 && test "${#mutation}" -eq 36
          case "$mutation" in *[!0-9a-f-]*) exit 1 ;; esac
          case "$prior" in "v1~$mutation~"*) exit 1 ;; esac
          for name in $(cat names); do
            IFS='|' read -r session window pane pid < "$name.pane"
            if test "$pane" != "$target"; then continue; fi
            guard_equals pid 42 && guard_equals start_time 100 &&
              guard_equals @rs_create_generation "$token" && guard_equals session_name "$name" &&
              guard_equals session_id "$session" && guard_equals window_id "$window" &&
              guard_equals pane_id "$pane" && guard_equals pane_pid "$pid" &&
              guard_equals pane_tty "$(cat tty)" && guard_equals pane_dead "$(cat "$name.dead")" &&
              guard_equals @rs_create_nonce "$(cat "$name.nonce"):$(cat "$name.stamp")" &&
              guard_equals @rs_resize_epoch "$(option_value epoch)" &&
              guard_equals @rs_paste_launch "$(option_value paste-launch)" &&
              guard_equals @rs_paste_admission "$(option_value marker)" &&
              guard_equals session_windows 1 && guard_equals window_panes 1 &&
              guard_equals window_index 0 && guard_equals pane_index 0 &&
              guard_equals window-size manual && guard_equals version 3.8 &&
              guard_equals socket_path "$PWD/tmux.sock" || exit 0
            case "$(option_value paste-launch)" in
              ??????????????8*) guard_equals @rs_create_birth "$(cat "$name.birth")" || exit 0 ;;
            esac
            for hook in after-if-shell after-load-buffer after-save-buffer after-paste-buffer after-delete-buffer after-send-keys after-set-option after-display-message after-resize-window command-error; do
              guard_equals "$hook" "" || exit 0
            done
            case "$state" in
              "possible~$name~$(cat tty)~$(cat marker)")
                case "$prior" in ''|v1~*~free) ;; *) exit 1 ;; esac
                case "$(cat marker)" in v1~*~"$token"~42~100~*~l~*) ;; *) exit 1 ;; esac
                valid_buffer "${state##*~}" ;;
              completed~*)
                old=${prior#*~}; old=${old#*~}
                test "$old" = "possible~${state#*~}" || exit 1 ;;
              free)
                case "$prior" in v1~*~possible~*|v1~*~completed~*) ;; *) exit 1 ;; esac ;;
              *) exit 1 ;;
            esac
            if test -n "$cleanup"; then
              test "$state" = free && test "$cleanup" = "${prior##*~}"
              rm "buffers/$cleanup"
              printf '%s\n' "$cleanup" >> buffer-deletions
            fi
            mkdir -p custody
            if test "$flag" = -so; then
              (set -C; printf '%s' "$value" > "custody/$option")
            else
              printf '%s' "$value" > "custody/$option"
            fi
            exit 0
          done
          exit 1 ;;
        'load-buffer '*|'delete-buffer '*)
          test "$5" = -F && test "$#" = 9 || exit 1
          socket="$PWD/tmux.sock"
          test "$3" = "$socket" || exit 0
          target=$7
          guard=$8
          branch=$9
          set -f
          set -- $branch
          action=$1
          test "$2" = -b && valid_buffer "$3" || exit 1
          buffer=$3
          shift 3
          if test "$action" = load-buffer; then
            test "$1" = - || exit 1
            shift
          fi
          test "$#" = 7 && test "$1 $2 $3 $4 $5 $6" = "; display-message -p -l -t $target" || exit 1
          witness=$7
          case "$action:$witness" in
            load-buffer:rs-paste-loaded-*|delete-buffer:rs-paste-cleanup-*) ;;
            *) exit 1 ;;
          esac
          for name in $(cat names); do
            IFS='|' read -r session window pane pid < "$name.pane"
            if test "$pane" != "$target"; then continue; fi
            dead=$(cat "$name.dead")
            tty=$(cat tty)
            nonce=$(cat "$name.nonce")
            stamp=$(cat "$name.stamp")
            epoch=$(option_value epoch)
            launch=$(option_value paste-launch)
            marker=$(option_value marker)
            test "$dead" = 0 &&
              guard_equals pid 42 && guard_equals start_time 100 &&
              guard_equals @rs_create_generation "$token" &&
              guard_equals session_name "$name" && guard_equals session_id "$session" &&
              guard_equals window_id "$window" && guard_equals pane_id "$pane" &&
              guard_equals pane_pid "$pid" && guard_equals pane_tty "$tty" &&
              guard_equals @rs_create_nonce "$nonce:$stamp" &&
              guard_equals @rs_resize_epoch "$epoch" &&
              guard_equals @rs_paste_launch "$launch" &&
              guard_equals @rs_paste_admission "$marker" &&
              guard_equals version 3.8 && guard_equals socket_path "$socket" &&
              guard_equals session_windows 1 && guard_equals window_panes 1 &&
              guard_equals pane_dead 0 && guard_equals window-size manual || exit 0
            for hook in after-load-buffer after-save-buffer after-paste-buffer after-delete-buffer after-send-keys after-set-option after-display-message after-resize-window command-error; do
              guard_equals "$hook" "" || exit 0
            done
            if test "$action" = load-buffer; then
              case "$guard" in
                *'#{==:#{@rs_paste_custody_'*)
                  index=0
                  while test "$index" -lt 16; do
                    option=$(custody_option "$index")
                    case "$guard" in *"#{==:#{$option},"*) break ;; esac
                    index=$((index + 1))
                  done
                  test "$index" -lt 16 || exit 0
                  prior=$(option_value "custody/$option")
                  guard_equals "$option" "$prior" && guard_equals after-if-shell "" || exit 0
                  case "$prior" in
                    v1~*~possible~"$name"~"$tty"~"$marker")
                      test "${prior##*~}" = "$buffer" || exit 0 ;;
                    *) exit 0 ;;
                  esac ;;
              esac
              cat > "buffers/$buffer"
              printf 'load\n' >> buffer-loads
              printf '%s\n' "$guard" > last-buffer-guard
              printf '%s\n' "$buffer" > last-buffer-load
            else
              guard_equals after-list-buffers "" || exit 0
              rm "buffers/$buffer"
              printf '%s\n' "$buffer" >> buffer-deletions
            fi
            printf '%s\n' "$witness"
            exit 0
          done
          exit 0 ;;
        'set-option -p -o -t '*' @rs_paste_launch '*)
          printf '%s\n' "$9" | awk '{print $7}' > paste-launch
          printf '%s\n' "$9" | awk '{for(i=1;i<NF;i++) if ($i=="@rs_paste_admission") print $(i+1)}' > marker ;;
        'set-option -p -t '*' @rs_create_birth '*)
          guard=$8
          target=$7
          for name in $(cat names); do
            IFS='|' read -r session window pane pid < "$name.pane"
            if test "$pane" != "$target"; then continue; fi
            init=$(cat "$name.initializer")
            birth=$(cat "$name.birth")
            nonce=$(cat "$name.nonce")
            stamp=$(cat "$name.stamp")
            guard_equals pid 42 && guard_equals start_time 100 &&
              guard_equals @rs_create_generation "$token" && guard_equals session_name "$name" &&
              guard_equals session_id "$session" && guard_equals window_id "$window" &&
              guard_equals pane_id "$pane" && guard_equals pane_pid "$pid" &&
              guard_equals pane_tty "$(cat tty)" && guard_equals pane_input_off 0 &&
              guard_equals @rs_create_nonce "$nonce:$stamp" && guard_equals pane_dead 0 &&
              guard_equals @rs_create_initializer "$init" && guard_equals @rs_create_birth "$birth" &&
              guard_equals @rs_resize_epoch "$(option_value epoch)" &&
              guard_equals @rs_paste_launch "$(option_value paste-launch)" &&
              guard_equals @rs_paste_admission "$(option_value marker)" &&
              guard_equals session_windows 1 && guard_equals window_panes 1 &&
              guard_equals window_index 0 && guard_equals pane_index 0 &&
              guard_equals window-size manual &&
              guard_equals version 3.8 && guard_equals socket_path "$3" || exit 0
            for hook in after-if-shell after-load-buffer after-save-buffer after-paste-buffer after-delete-buffer after-send-keys after-set-option after-display-message after-resize-window command-error; do
              guard_equals "$hook" "" || exit 0
            done
            case "$birth" in "$init~"*) ;; *) exit 1 ;; esac
            set -f
            set -- $9
            test "$#" = 20
            test "$1 $2 $3 $4 $5 $6" = "set-option -p -t $pane @rs_create_birth 'used~$birth'"
            test "$7 $8 $9 ${10} ${11} ${12}" = "; set-option -po -t $pane @rs_paste_launch"
            test "${14} ${15} ${16} ${17} ${18} ${19}" = "; set-option -po -t $pane @rs_paste_admission"
            test ! -e paste-launch && test ! -e marker
            printf 'used~%s\n' "$birth" > "$name.birth"
            printf '%s\n' "${13}" > paste-launch
            printf '%s\n' "${20}" > marker
            exit 0
          done
          exit 1 ;;
        'set-option -p '*' @rs_resize_epoch '*)
          printf '%s\n' "$9" | awk '{for(i=1;i<NF;i++) if ($i=="@rs_resize_epoch") print $(i+1)}' > epoch
          printf '%s\n' "$9" | awk '{for(i=1;i<NF;i++) if ($i=="@rs_paste_admission") print $(i+1)}' > marker
          printf 'rs-fenced\n' ;;
        'resize-window '*)
          printf '%s\n' "$9" >> resizes
          printf 'rs-committed\n' ;;
        'send-keys '*)
          printf '%s\n' "$9" >> inputs
          printf 'rs-input-accepted\n' ;;
        *) printf '%s\n' "$9" >> unexpected-terminal-mutations; exit 1 ;;
      esac
      exit 0
    fi
    case "$7" in
      'show-options -sq -F '*)
        test "$5" = -F && test "$#" = 7
        guard=$6
        custody_snapshot "$7" ;;
      'set-option -s @rs_create_generation '*)
        test -z "$token"
        printf '%s\n' "$7" | awk '{print $4}' > generation ;;
      'new-session '*)
        branch=$7
        guard=$6
        name=$(printf '%s\n' "$branch" | awk '{for(i=1;i<NF;i++) if ($i=="-s") {print $(i+1); exit}}')
        nonce=$(printf '%s\n' "$branch" | awk '{for(i=1;i<=NF;i++) if ($i~/^RS_CREATE_NONCE=/) {sub(/^RS_CREATE_NONCE=/,"",$i); print $i; exit}}')
        if test -f roll-generation-at-spawn; then
          cat roll-generation-at-spawn > generation
          token=$(cat generation)
        fi
        guard_equals pid 42 && guard_equals start_time 100 &&
          guard_equals @rs_create_generation "$token" || exit 0
        for hook in after-if-shell after-new-session session-created session-window-changed window-linked window-layout-changed after-set-environment after-set-option command-error; do
          guard_equals "$hook" "" || exit 1
        done
        test ! -e "$name.pane" || exit 1
        test -n "$name"
        if test -z "$nonce"; then
          case "$branch" in *" -- /usr/bin/env "*" ; set-option -t =$name:0.0 @rs_command "*) ;; *) exit 1 ;; esac
          printf '%s\n' "$name" >> diagnostic-dispatched
          printf '%s\n' "$name" >> names
          printf '$99|@99|%%99|299\n' > "$name.pane"
          printf '0\n' > "$name.dead"
          cat "$name.pane"
          exit 0
        fi
        ticket=$(printf '%s\n' "$branch" | awk '{for(i=1;i<=NF;i++) if ($i~/^RS_CREATE_TICKET=/) {sub(/^RS_CREATE_TICKET=/,"",$i); print $i; exit}}')
        stamp_format='#{session_id}:#{window_id}:#{pane_id}:#{pane_pid}'
        init_format='#{session_id}~#{window_id}~#{pane_id}~#{pane_pid}'
        prefix=$(printf '%s\n' "$branch" | awk -F"'" '{print $4}')
        case "$branch" in
          *"__managed-launch "*" ; set-environment -r -t =$name RS_CREATE_TICKET ; set-option -Fpo -t =$name:0.0 @rs_create_initializer '$prefix' ; set-option -Fpo -t =$name:0.0 @rs_create_nonce '$nonce:$stamp_format' ; set-option -p -t =$name:0.0 @rs_command codex") ;;
          *) exit 1 ;;
        esac
        prefix=${prefix%"$init_format"}
        case "$prefix" in "v1~42~100~$token~"*"~$nonce~$ticket~") ;; *) exit 1 ;; esac
        printf '%s %s\n' "$name" "$nonce" >> dispatched
        mode=normal
        if test -f mode; then mode=$(cat mode); fi
        if test "$mode" = delay; then exec /bin/sleep 20; fi
        count=$(wc -l < dispatched | tr -d ' ')
        slot=$(option_value pane-slot)
        if test -z "$slot"; then slot=$count; fi
        if test "$mode" != hidden; then printf '%s\n' "$name" >> names; fi
        printf '%s\n' "$nonce" > "$name.nonce"
        pid=$((300 + count))
        if test -f pane-pid; then pid=$(cat pane-pid); fi
        printf '$%s|@%s|%%%s|%s\n' "$slot" "$slot" "$slot" "$pid" > "$name.pane"
        init="$prefix$(tr '|' '~' < "$name.pane")"
        printf '%s\n' "$init" > "$name.initializer"
        tr '|' ':' < "$name.pane" > "$name.stamp"
        printf '0\n' > "$name.dead"
        receipt=$(cat "$name.pane")
        if test "$mode" = exited; then
          printf '1\n' > "$name.dead"
          printf '%s|\n' "${receipt%|*}" > "$name.pane"
        fi
        if test -f birth-device && test "$mode" != exited; then
          capsule=$(printf '%s\n' "$branch" | awk '{for(i=1;i<NF;i++) if ($i=="__managed-launch") {print $(i+1); exit}}')
          python3 - "$capsule" "$token" "$name" "$init" "$nonce" "$ticket" "$PWD" <<'PY'
import json
from pathlib import Path
import sys
import uuid
encoded, token, name, init, nonce, ticket, root = sys.argv[1:]
capsule = json.loads(bytes.fromhex(encoded))
birth = capsule["birth"]
assert capsule["nonce"] == nonce and birth["ticket"] == ticket and uuid.UUID(ticket).version == 4
assert birth["server"] == {"pid": 42, "start": 100, "token": token}
assert name == "rs_" + uuid.UUID(birth["session"]).hex + "_" + uuid.UUID(birth["runtime"]).hex
assert init.split("~")[4:8] == [birth["operation"], birth["runtime"], nonce, ticket]
assert Path(capsule["service_state"]) == Path(root) and Path(birth["tmux"]) == Path(root) / "tmux"
PY
          test ! -e "$name.birth"
          printf '%s~%s\n' "$init" "$(cat birth-device)" > "$name.birth"
        fi
        if test "$mode" = effect-lost || test "$mode" = hidden; then exit 1; fi
        printf '%s\n' "$receipt" ;;
      *) exit 1 ;;
    esac ;;
  show-environment)
    test "$#" -eq 14
    case "$6" in =rs_*) ;; *) exit 1 ;; esac
    name=${6#=}
    test "$5 $7 $8 $9 ${10} ${11} ${12} ${13}" = "-t RS_CREATE_NONCE ; list-panes -s -t =$name:0 -F"
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
      printf '%s\t%s\t%s\t%s\t100\t30\tfixture' "$name" "$pane" "$pid" "$(cat "$name.dead")"
      case "${7-}" in
        *'#{session_id}|#{window_id}') printf '\t42|100|%s|%s|%s' "$token" "$session" "$window" ;;
      esac
      printf '\n'
    done ;;
  new-session)
    test "$1" = -N && test "$2" = -S
    test "$5 $6 $7 ${9}" = "-d -P -F -s"
    name=${10}
    printf '%s\n' "$name" >> diagnostic-dispatched
    printf '%s\n' "$name" >> names
    printf '$99|@99|%%99|299\n' > "$name.pane"
    printf '0\n' > "$name.dead"
    cat "$name.pane" ;;
  set-option)
    test "$5" = -t && test "$7" = @rs_command ;;
  list-commands)
    printf 'paste-buffer [-dprS]\nload-buffer path\n' ;;
  save-buffer)
    test "$#" = 7 && test "$5" = -b && valid_buffer "$6" && test "$7" = - || exit 1
    test -f "buffers/$6"
    if test -f corrupt-buffer-readback; then
      printf 'altered'
    else
      cat "buffers/$6"
    fi ;;
  load-buffer|delete-buffer)
    printf '%s\n' "$4" >> unexpected-terminal-mutations
    exit 1 ;;
  capture-pane)
    test "$6" = -t
    name=$(cat names)
    test "$7" = "=$name:0.0"
    cat output ;;
  *) exit 1 ;;
esac
"##;

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    catalog: PathBuf,
    workspace: PathBuf,
    catalog_busy: Option<mpsc::Sender<()>>,
}

struct FixtureServer(JoinHandle<Result<()>>);

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.0.abort();
    }
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
        fs::create_dir(root.join("buffers")).unwrap();
        let catalog = root.join("catalog");
        catalog::provision(&catalog).unwrap();
        tests::fixture_program(&root.join("tmux"), TMUX);
        tests::fixture_program(&root.join("codex"), "#!/bin/sh\nexit 0\n");
        Self {
            _temp: temp,
            root,
            catalog,
            workspace,
            catalog_busy: None,
        }
    }

    async fn lane(&self) -> CatalogLane {
        let catalog = self.catalog.clone();
        let path = std::env::join_paths([&self.root]).unwrap();
        let busy = self
            .catalog_busy
            .clone()
            .map(|observed| (catalog.clone(), observed));
        CatalogLane::start_fixture(move || Ok(catalog), ServiceProfiles::new(Some(&path)), busy)
            .await
            .expect("fixture catalog worker did not report startup")
    }

    async fn serve(&self, lane: CatalogLane) -> FixtureServer {
        self.serve_engine(self.engine(), lane).await
    }

    fn engine(&self) -> Engine {
        Engine::new(self.root.join("tmux"), self.root.join("tmux.sock"))
    }

    async fn serve_engine(&self, engine: Engine, lane: CatalogLane) -> FixtureServer {
        let job = FixtureServer(tokio::spawn(serve_local(
            engine,
            Arc::new(AgentLock::acquire(&self.root).unwrap()),
            Some(lane),
        )));
        self.wait_listening(&job).await;
        job
    }

    async fn wait_listening(&self, job: &FixtureServer) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                assert!(
                    !job.0.is_finished(),
                    "fixture agent stopped before readiness"
                );
                match request(&self.root, &Request::Health {}).await {
                    Ok(Response::Health {
                        agent_pid,
                        remote_execution_enabled: false,
                    }) if agent_pid == std::process::id() => return,
                    Ok(response) => {
                        panic!("unexpected fixture agent health response: {response:?}")
                    }
                    Err(error)
                        if error.downcast_ref::<Error>().is_some_and(|cause| {
                            matches!(
                                cause.kind(),
                                ErrorKind::NotFound | ErrorKind::ConnectionRefused
                            )
                        }) => {}
                    Err(error) => panic!("fixture agent health request failed: {error:#}"),
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("fixture agent did not answer health within three seconds");
    }

    async fn restart(&self, job: FixtureServer) -> FixtureServer {
        stop(job).await;
        self.wait_restart_released().await;
        self.serve(self.lane().await).await
    }

    async fn wait_restart_released(&self) {
        self.wait_catalog_released().await;
        let timeout = Duration::from_secs(3);
        wait_lock_release(&self.root.join("agent.lock"), timeout, || {})
            .await
            .expect("agent ownership did not release");
    }

    async fn wait_catalog_released(&self) {
        wait_lock_release(
            &self.catalog.join("catalog.lock"),
            Duration::from_secs(3),
            || {},
        )
        .await
        .expect("catalog ownership did not release");
    }

    async fn hold_catalog(&self) -> File {
        self.wait_catalog_released().await;
        let holder = OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.catalog.join("catalog.lock"))
            .unwrap();
        holder.try_lock().expect("could not hold fixture catalog");
        holder
    }

    fn buffer_snapshot(&self) -> (std::collections::BTreeMap<String, Vec<u8>>, Vec<u8>) {
        let buffers = fs::read_dir(self.root.join("buffers"))
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().into_string().unwrap(),
                    fs::read(entry.path()).unwrap(),
                )
            })
            .collect();
        let deletions = match fs::read(self.root.join("buffer-deletions")) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("could not read deletion ledger: {error}"),
        };
        (buffers, deletions)
    }

    fn birth_device(&self, tty: &str) {
        let device = fs::metadata(tty).unwrap();
        let start = crate::engine::launch_fixture_process_start(std::process::id()).unwrap();
        self.set("tty", tty);
        self.set("pane-pid", std::process::id().to_string());
        self.set(
            "birth-device",
            format!(
                "{start}~{tty}~{}~{}~{}",
                device.dev(),
                device.ino(),
                device.rdev()
            ),
        );
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

pub(super) async fn wait_lock_release(
    path: &Path,
    timeout: Duration,
    mut on_contention: impl FnMut(),
) -> std::io::Result<()> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    tokio::time::timeout(timeout, async {
        loop {
            match file.try_lock() {
                Ok(()) => return file.unlock(),
                Err(TryLockError::WouldBlock) => on_contention(),
                Err(TryLockError::Error(error)) => return Err(error),
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .map_err(|_| Error::new(ErrorKind::TimedOut, "fixture lock remained owned"))?
}

async fn stop(mut job: FixtureServer) {
    job.0.abort();
    assert!(
        tokio::time::timeout(Duration::from_secs(3), &mut job.0)
            .await
            .expect("fixture agent did not stop")
            .unwrap_err()
            .is_cancelled()
    );
}

fn observation_pty() -> (File, File, String) {
    use nix::libc;
    use std::{
        ffi::CStr,
        os::fd::{AsRawFd, FromRawFd},
    };

    let (mut master, mut slave) = (-1, -1);
    let mut size = libc::winsize {
        ws_col: 100,
        ws_row: 30,
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
    let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    let mut tty = [0; 128];
    assert_eq!(
        unsafe { libc::ttyname_r(slave.as_raw_fd(), tty.as_mut_ptr(), tty.len()) },
        0
    );
    let tty = unsafe { CStr::from_ptr(tty.as_ptr()) }
        .to_str()
        .unwrap()
        .to_owned();
    (master, slave, tty)
}

fn create(key: Uuid, workspace_id: Uuid, reservation: Option<Uuid>) -> ProductRequest {
    ProductRequest::CreateManaged {
        key,
        workspace_id,
        profile: ProviderProfile::Codex,
        reservation,
    }
}

fn diagnostic_create(fixture: &Fixture, session_id: Uuid, runtime_id: Uuid) -> Request {
    Request::Create {
        session_id,
        runtime_id,
        cwd: fixture.workspace.clone(),
        argv: vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
    }
}

fn seed_visible(fixture: &Fixture, count: usize) -> Vec<String> {
    let mut names = Vec::new();
    for slot in 30..30 + count {
        let name = tmux_name(Uuid::new_v4(), Uuid::new_v4());
        fixture.set(
            &format!("{name}.pane"),
            format!("${slot}|@{slot}|%{slot}|{slot}\n"),
        );
        fixture.set(&format!("{name}.dead"), "0\n");
        names.push(name);
    }
    fixture.set(
        "names",
        names
            .iter()
            .map(|name| format!("{name}\n"))
            .collect::<String>(),
    );
    names
}

#[tokio::test]
async fn w1_create_shared_capacity_keeps_original_reservation_and_deduplicates_visible_marks() {
    for already_visible in [false, true] {
        let fixture = Fixture::new();
        let lane = fixture.lane().await;
        let job = fixture.serve(lane.clone()).await;
        let workspace = fixture.register().await;
        let names = seed_visible(&fixture, if already_visible { 14 } else { 15 });
        fixture.set(
            "mode",
            if already_visible {
                "effect-lost"
            } else {
                "hidden"
            },
        );
        let key = Uuid::new_v4();
        let create = create(key, workspace, None);
        assert!(matches!(
            fixture.call(&create).await,
            CreateState::Unknown { .. }
        ));
        let original = lane
            .run(move |db, _| Ok(db.operation(key)?.unwrap()))
            .await
            .unwrap();
        assert_eq!(original.stage, OperationStage::Marked);
        let command = diagnostic_create(&fixture, Uuid::new_v4(), Uuid::new_v4());
        let response = request(&fixture.root, &command).await.unwrap();
        if already_visible {
            assert!(matches!(response, Response::Created { .. }), "{response:?}");
            assert!(fixture.root.join("diagnostic-dispatched").exists());
        } else {
            assert!(
                matches!(response, Response::Error { message } if message.contains("limit is 16"))
            );
            assert!(!fixture.root.join("diagnostic-dispatched").exists());
            fixture.set(
                "names",
                names
                    .iter()
                    .chain(std::iter::once(&original.target))
                    .map(|name| format!("{name}\n"))
                    .collect::<String>(),
            );
        }
        let recovered = fixture.status(key).await;
        let CreateState::Confirmed {
            outcome,
            current: PaneLiveness::Live,
            ..
        } = &recovered
        else {
            panic!("original creation did not settle: {recovered:?}");
        };
        assert_eq!(outcome.runtime.session_id, original.session_id);
        assert_eq!(outcome.runtime.runtime_id, original.runtime_id);
        assert_eq!(outcome.runtime.operation_id, Some(original.id));
        assert_eq!(fixture.call(&create).await, recovered);
        let Response::Runtimes { runtimes } =
            request(&fixture.root, &Request::List {}).await.unwrap()
        else {
            panic!("expected capacity-bounded inventory");
        };
        assert_eq!(runtimes.len(), 16);
        assert_eq!(fixture.dispatches(), 1);
        stop(job).await;
    }
}

#[tokio::test]
async fn w1_create_generation_rollover_at_dispatch_cannot_spawn_through_either_route() {
    for managed in [false, true] {
        let fixture = Fixture::new();
        let job = fixture.serve(fixture.lane().await).await;
        let workspace = fixture.register().await;
        let names = seed_visible(&fixture, 1);
        let before = fs::read(fixture.root.join(format!("{}.pane", names[0]))).unwrap();
        let generation = Uuid::new_v4().to_string();
        fixture.set("generation", Uuid::new_v4().to_string());
        fixture.set("roll-generation-at-spawn", &generation);
        if managed {
            let key = Uuid::new_v4();
            let command = create(key, workspace, None);
            assert!(matches!(
                fixture.call(&command).await,
                CreateState::Unknown { .. }
            ));
            assert!(matches!(
                fixture.status(key).await,
                CreateState::Unknown { .. }
            ));
        } else {
            let command = diagnostic_create(&fixture, Uuid::new_v4(), Uuid::new_v4());
            assert!(matches!(request(&fixture.root, &command).await.unwrap(),
                    Response::Error { message } if message.contains("admitted generation")));
        }
        assert_eq!(fixture.dispatches(), 0);
        assert!(!fixture.root.join("diagnostic-dispatched").exists());
        assert_eq!(
            fs::read_to_string(fixture.root.join("generation")).unwrap(),
            generation
        );
        assert_eq!(
            fs::read_to_string(fixture.root.join("names")).unwrap(),
            format!("{}\n", names[0])
        );
        assert_eq!(
            fs::read(fixture.root.join(format!("{}.pane", names[0]))).unwrap(),
            before
        );
        stop(job).await;
    }
}

#[tokio::test]
async fn w1_create_admission_rejects_duplicate_ids_and_overflow_but_ignores_foreign_marks() {
    let fixture = Fixture::new();
    let lane = fixture.lane().await;
    let job = fixture.serve(lane).await;
    let workspace = fixture.register().await;
    let names = seed_visible(&fixture, 15);
    fixture.set("mode", "hidden");
    assert!(matches!(
        fixture.call(&create(Uuid::new_v4(), workspace, None)).await,
        CreateState::Unknown { .. }
    ));
    fixture.set("generation", Uuid::new_v4().to_string());
    let (session, runtime) = crate::model::parse_name(&names[0]).unwrap();
    for (session, runtime) in [(session, Uuid::new_v4()), (Uuid::new_v4(), runtime)] {
        assert!(
            matches!(request(&fixture.root, &diagnostic_create(&fixture, session, runtime)).await.unwrap(),
                Response::Error { message } if message.contains("already exists"))
        );
    }
    assert!(!fixture.root.join("diagnostic-dispatched").exists());
    assert!(matches!(
        request(
            &fixture.root,
            &diagnostic_create(&fixture, Uuid::new_v4(), Uuid::new_v4())
        )
        .await
        .unwrap(),
        Response::Created { .. }
    ));
    seed_visible(&fixture, 17);
    assert_eq!(
        fixture.call(&create(Uuid::new_v4(), workspace, None)).await,
        CreateState::Failed {
            issue: ProductIssue::RuntimeLimit
        }
    );
    let dispatches = fs::read(fixture.root.join("diagnostic-dispatched")).unwrap();
    assert!(matches!(
        request(
            &fixture.root,
            &diagnostic_create(&fixture, Uuid::new_v4(), Uuid::new_v4())
        )
        .await
        .unwrap(),
        Response::Error { .. }
    ));
    assert_eq!(
        fs::read(fixture.root.join("diagnostic-dispatched")).unwrap(),
        dispatches
    );
    assert_eq!(fixture.dispatches(), 1);
    stop(job).await;
}

#[tokio::test]
async fn w1_create_catalog_free_diagnostic_uses_visible_capacity() {
    let fixture = Fixture::new();
    seed_visible(&fixture, 15);
    let job = FixtureServer(tokio::spawn(serve_local(
        fixture.engine(),
        Arc::new(AgentLock::acquire(&fixture.root).unwrap()),
        None,
    )));
    fixture.wait_listening(&job).await;
    for expected_created in [true, false] {
        let response = request(
            &fixture.root,
            &diagnostic_create(&fixture, Uuid::new_v4(), Uuid::new_v4()),
        )
        .await
        .unwrap();
        assert_eq!(
            matches!(response, Response::Created { .. }),
            expected_created,
            "{response:?}"
        );
    }
    assert_eq!(
        fs::read_to_string(fixture.root.join("diagnostic-dispatched"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    stop(job).await;
}

#[tokio::test]
async fn w1_create_restart_waits_for_agent_lock() {
    let fixture = Fixture::new();
    let path = fixture.root.join("agent.lock");
    let owner = AgentLock::acquire(&fixture.root).unwrap();
    let (observed, contention) = tokio::sync::oneshot::channel();
    let mut observed = Some(observed);
    let wait = wait_lock_release(&path, Duration::from_secs(3), || {
        if let Some(signal) = observed.take() {
            signal.send(()).unwrap();
        }
    });
    let release = async {
        tokio::time::timeout(Duration::from_secs(2), contention)
            .await
            .expect("waiter never observed contention")
            .unwrap();
        let error = AgentLock::acquire(&fixture.root).err().unwrap();
        assert!(matches!(
            error.downcast_ref::<TryLockError>(),
            Some(TryLockError::WouldBlock)
        ));
        drop(owner);
    };
    let (result, ()) = tokio::join!(wait, release);
    result.unwrap();

    let owner = AgentLock::acquire(&fixture.root).unwrap();
    let mut attempts = 0;
    let error = wait_lock_release(&path, Duration::from_millis(30), || {
        attempts += 1;
        let error = AgentLock::acquire(&fixture.root).err().unwrap();
        assert!(matches!(
            error.downcast_ref::<TryLockError>(),
            Some(TryLockError::WouldBlock)
        ));
    })
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::TimedOut);
    assert!(attempts > 0);

    let error = wait_lock_release(&fixture.root, Duration::from_secs(3), || {
        panic!("unexpected lock I/O failure was treated as contention");
    })
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::IsADirectory);
    drop(owner);
    assert!(AgentLock::acquire(&fixture.root).is_ok());
}

#[tokio::test]
async fn w1_create_guarded_paste_probe() {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    let fixture = Fixture::new();
    let (_master, _slave, tty) = observation_pty();
    fixture.birth_device(&tty);
    let unrelated = "rs-paste-11111111111111111111111111111111";
    fixture.set(&format!("buffers/{unrelated}"), b"unrelated");
    fixture.set("corrupt-buffer-readback", b"");
    let lane = fixture.lane().await;
    let job = fixture
        .serve_engine(fixture.engine().with_fixture_sender(), lane)
        .await;
    let workspace = fixture.register().await;
    let CreateState::Confirmed { outcome, .. } =
        fixture.call(&create(Uuid::new_v4(), workspace, None)).await
    else {
        panic!("fixture did not create a live pane");
    };
    let runtime_id = outcome.runtime.runtime_id;
    let Response::TerminalOpened { status } = request(
        &fixture.root,
        &Request::OpenTerminal {
            runtime_id,
            view_id: Uuid::new_v4(),
        },
    )
    .await
    .unwrap() else {
        panic!("fixture pane unavailable");
    };
    let response = request(
        &fixture.root,
        &Request::AcquireTerminal {
            terminal: status.terminal,
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(response, Response::ControlRecovering { .. }),
        "{response:?}"
    );
    assert!(!fixture.root.join("paste-launch").exists());
    let (buffers, deletions) = fixture.buffer_snapshot();
    let probe = fs::read_to_string(fixture.root.join("last-buffer-load")).unwrap();
    assert!(probe.starts_with("rs-paste-probe-"));
    assert_eq!(deletions, probe.as_bytes());
    assert_eq!(
        buffers,
        [(unrelated.to_owned(), b"unrelated".to_vec())].into()
    );
    assert_eq!(
        fs::read(fixture.root.join("buffer-loads")).unwrap(),
        b"load\n"
    );
    fs::remove_file(fixture.root.join("corrupt-buffer-readback")).unwrap();

    let guard = fs::read_to_string(fixture.root.join("last-buffer-guard")).unwrap();
    let guard = guard.trim_end();
    let rejected = guard.replace("#{==:#{pane_id},%1}", "#{==:#{pane_id},%2}");
    assert_ne!(guard, rejected);
    let name = probe.trim_end();
    let bytes = "  first\n\tСніг ☃\n ".as_bytes();
    let run = |args: &[&str], input: Option<&[u8]>| {
        let mut command = Command::new(fixture.root.join("tmux"));
        command
            .args(["-N", "-S"])
            .arg(fixture.root.join("tmux.sock"))
            .args(args)
            .stdout(Stdio::piped());
        command.stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let mut child = command.spawn().unwrap();
        if let Some(input) = input {
            child.stdin.take().unwrap().write_all(input).unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{:?}", output.status);
        output.stdout
    };
    let load =
        format!("load-buffer -b {name} - ; display-message -p -l -t %1 rs-paste-loaded-control");
    let delete =
        format!("delete-buffer -b {name} ; display-message -p -l -t %1 rs-paste-cleanup-control");
    let cleanup_guard = format!("#{{&&:{guard},#{{==:#{{after-list-buffers}},}}}}");
    let rejected_cleanup = cleanup_guard.replace("#{==:#{pane_id},%1}", "#{==:#{pane_id},%2}");
    let guarded = |guard, branch, input| run(&["if-shell", "-F", "-t", "%1", guard, branch], input);
    let before = fixture.buffer_snapshot();
    assert!(guarded(&rejected, &load, None).is_empty());
    assert_eq!(fixture.buffer_snapshot(), before);
    assert_eq!(
        guarded(guard, &load, Some(bytes)),
        b"rs-paste-loaded-control\n"
    );
    assert_eq!(run(&["save-buffer", "-b", name, "-"], None), bytes);
    let loaded = fixture.buffer_snapshot();
    assert_eq!(loaded.0[name], bytes);
    assert!(guarded(&rejected_cleanup, &delete, None).is_empty());
    assert_eq!(fixture.buffer_snapshot(), loaded);
    assert_eq!(
        guarded(&cleanup_guard, &delete, None),
        b"rs-paste-cleanup-control\n"
    );
    let (buffers, deletions) = fixture.buffer_snapshot();
    assert_eq!(buffers, before.0);
    assert_eq!(deletions, format!("{name}\n{name}\n").as_bytes());
    assert_eq!(
        fs::read(fixture.root.join("buffer-loads")).unwrap(),
        b"load\nload\n"
    );
    stop(job).await;
}

#[tokio::test]
async fn w1_create_inventory_corroborates_origin() {
    let fixture = Fixture::new();
    let lane = fixture.lane().await;
    let job = fixture.serve(lane).await;
    let workspace = fixture.register().await;
    let legacy = tmux_name(Uuid::new_v4(), Uuid::new_v4());
    fixture.set("names", format!("{legacy}\n"));
    fixture.set(&format!("{legacy}.pane"), "$9|@9|%9|309\n");
    fixture.set(&format!("{legacy}.dead"), "0\n");
    let Response::Runtimes { runtimes } = request(&fixture.root, &Request::List {}).await.unwrap()
    else {
        panic!("expected surviving legacy pane");
    };
    assert_eq!(runtimes.len(), 1);
    assert_eq!(runtimes[0].origin, RuntimeOrigin::Legacy);
    assert!(!fixture.root.join("generation").exists());

    let key = Uuid::new_v4();
    fixture.set("mode", "effect-lost");
    assert!(matches!(
        fixture.call(&create(key, workspace, None)).await,
        CreateState::Unknown { .. }
    ));
    let names = fs::read_to_string(fixture.root.join("names")).unwrap();
    let managed = names.lines().find(|name| *name != legacy).unwrap();
    let stamp_file = format!("{managed}.stamp");
    let nonce_file = format!("{managed}.nonce");
    let stamp = fs::read(fixture.root.join(&stamp_file)).unwrap();
    let nonce = fs::read(fixture.root.join(&nonce_file)).unwrap();
    fs::remove_file(fixture.root.join(&stamp_file)).unwrap();

    let inventory = async || {
        let Response::Runtimes { runtimes } =
            request(&fixture.root, &Request::List {}).await.unwrap()
        else {
            panic!("expected inventory for both surviving panes");
        };
        assert_eq!(runtimes.len(), 2);
        let legacy = runtimes
            .iter()
            .find(|runtime| runtime.tmux_name == legacy)
            .unwrap()
            .clone();
        let managed = runtimes
            .into_iter()
            .find(|runtime| runtime.tmux_name == managed)
            .unwrap();
        (legacy, managed)
    };
    let verify = async || {
        let (legacy, managed) = inventory().await;
        assert_eq!(
            (legacy.origin, legacy.workspace_id, legacy.operation_id),
            (RuntimeOrigin::Legacy, None, None)
        );
        assert_eq!(
            (managed.origin, managed.workspace_id, managed.operation_id),
            (RuntimeOrigin::Managed, None, None)
        );
    };
    verify().await;

    let catalog_path = fixture.catalog.join("catalog.sqlite3");
    let retained = fixture.catalog.join("retained.sqlite3");
    fs::rename(&catalog_path, &retained).unwrap();
    verify().await;
    assert!(matches!(
        fixture.status(key).await,
        CreateState::Unknown {
            issue: Some(ProductIssue::CatalogMissing)
        }
    ));

    fs::remove_file(fixture.root.join(&nonce_file)).unwrap();
    for invalid_stamp in ["", "malformed"] {
        fixture.set(&stamp_file, invalid_stamp);
        verify().await;
    }

    let fault = format!("{legacy}.inventory-fault");
    fixture.set(&fault, "");
    assert_eq!(inventory().await.0.origin, RuntimeOrigin::Managed);
    fs::remove_file(fixture.root.join(fault)).unwrap();
    for replacement in [
        format!("$9|@9|%9|999|{legacy}|0|"),
        format!("$1|@1|%9|309|{legacy}|0|"),
    ] {
        fixture.set(&format!("{legacy}.inventory-replacement"), replacement);
        assert_eq!(inventory().await.0.origin, RuntimeOrigin::Managed);
    }
    fs::remove_file(fixture.root.join(format!("{legacy}.inventory-replacement"))).unwrap();
    let replaced_server = format!("{legacy}.inventory-generation");
    fixture.set(&replaced_server, Uuid::new_v4().to_string());
    assert_eq!(inventory().await.0.origin, RuntimeOrigin::Managed);
    fs::remove_file(fixture.root.join(replaced_server)).unwrap();

    fixture.set(&nonce_file, nonce);
    fixture.set(&stamp_file, stamp);
    fs::rename(&retained, &catalog_path).unwrap();
    let job = fixture.restart(job).await;
    let CreateState::Confirmed {
        outcome,
        current: PaneLiveness::Live,
        ..
    } = fixture.status(key).await
    else {
        panic!("expected the original managed launch after catalog recovery");
    };
    let (legacy, managed) = inventory().await;
    assert_eq!(legacy.origin, RuntimeOrigin::Legacy);
    assert_eq!(managed.origin, RuntimeOrigin::Managed);
    assert_eq!(managed.workspace_id, Some(workspace));
    assert!(managed.operation_id.is_some());
    assert_eq!(managed, outcome.runtime);
    assert_eq!(fixture.dispatches(), 1);
    stop(job).await;
}

#[tokio::test]
async fn w1_create_terminal_status_classifies_first_and_second_sample_replacement() {
    let fixture = Fixture::new();
    let (_master, _slave, tty) = observation_pty();
    fixture.set("tty", &tty);
    fixture.set("pane-pid", std::process::id().to_string());
    let job = fixture.serve(fixture.lane().await).await;
    let workspace = fixture.register().await;
    let launch = async || {
        let CreateState::Confirmed {
            outcome,
            current: PaneLiveness::Live,
            ..
        } = fixture.call(&create(Uuid::new_v4(), workspace, None)).await
        else {
            panic!("expected a catalog-corroborated live launch");
        };
        outcome.runtime
    };
    let original = launch().await;
    let open = request(
        &fixture.root,
        &Request::OpenTerminal {
            runtime_id: original.runtime_id,
            view_id: Uuid::new_v4(),
        },
    )
    .await
    .unwrap();
    let Response::TerminalOpened { status: opened } = open else {
        panic!("expected live terminal proof, got {open:?}");
    };
    assert_eq!(opened.observation, ObservationState::Observed);
    assert_eq!(opened.provider.unwrap().size, opened.committed.size);

    fixture.set("pane-slot", "1");
    let replacement = launch().await;
    assert_eq!(original.pane_id, replacement.pane_id);
    assert_ne!(original.runtime_id, replacement.runtime_id);
    let read = |path: &str| fs::read_to_string(fixture.root.join(path)).unwrap();
    let geometry = |runtime: &crate::model::Runtime, device: &str| {
        let name = &runtime.tmux_name;
        let token = read("generation");
        let pane = read(&format!("{name}.pane"));
        let nonce = read(&format!("{name}.nonce"));
        let stamp = read(&format!("{name}.stamp"));
        format!(
            "42|100|{}|{}|{name}|0|0|{}:{}|0|1|1|manual|{device}|100|30|100|30|||||0|1\n",
            token.trim(),
            pane.trim(),
            nonce.trim(),
            stamp.trim()
        )
    };
    let live = geometry(&original, &tty);
    let no_device = geometry(&original, "/dev/pts/999999999");
    let replaced = geometry(&replacement, &tty).replace("|100|30|100|30", "|1|30|1|30");
    let sample = async |terminal, rows: &[&str], count| {
        fixture.set("geometry-samples", "");
        for (index, row) in rows.iter().enumerate() {
            fixture.set(&format!("geometry-{}", index + 1), row);
        }
        let reply = request(&fixture.root, &Request::TerminalStatus { terminal })
            .await
            .unwrap();
        assert_eq!(
            read("geometry-samples"),
            format!("{}\n", original.pane_id).repeat(count)
        );
        reply
    };
    for (rows, count) in [
        ([replaced.as_str(), &live], 1),
        ([live.as_str(), &replaced], 2),
        ([no_device.as_str(), &replaced], 2),
    ] {
        assert!(matches!(
            sample(opened.terminal.clone(), &rows, count).await,
            Response::TerminalChanged {}
        ));
    }
    let mut older = opened.terminal.clone();
    older.target.process_start += 1;
    assert!(matches!(
        sample(older, &[&live], 1).await,
        Response::TerminalChanged {}
    ));
    let replacement_nonce_path = format!("{}.nonce", replacement.tmux_name);
    let replacement_nonce = read(&replacement_nonce_path);
    fixture.set(&replacement_nonce_path, Uuid::new_v4().to_string());
    assert!(matches!(
        sample(opened.terminal.clone(), &[&replaced], 1).await,
        Response::Error { .. }
    ));
    let uncorroborated = sample(opened.terminal.clone(), &[&live, &replaced], 2).await;
    assert!(
        matches!(&uncorroborated, Response::TerminalStatus { status }
            if status.terminal == opened.terminal
                && status.committed == opened.committed
                && status.observation == ObservationState::Unavailable
                && status.provider.is_none()),
        "{uncorroborated:?}"
    );
    fixture.set(&replacement_nonce_path, replacement_nonce);

    let mut exited: Vec<_> = live.split('|').collect();
    exited[6] = "";
    exited[11] = "1";
    let exited = exited.join("|");
    assert!(matches!(
        sample(opened.terminal.clone(), &[&exited], 1).await,
        Response::Error { .. }
    ));
    let malformed = live.replacen("|manual|", "|unsupported|", 1);
    assert!(matches!(
        sample(opened.terminal.clone(), &[&malformed], 1).await,
        Response::Error { .. }
    ));
    let nonce_path = format!("{}.nonce", original.tmux_name);
    let nonce = read(&nonce_path);
    fixture.set(&nonce_path, Uuid::new_v4().to_string());
    assert!(matches!(
        sample(opened.terminal.clone(), &[&live], 1).await,
        Response::Error { .. }
    ));
    fixture.set(&nonce_path, nonce);

    let mut resized: Vec<_> = live.split('|').collect();
    let epoch = format!("{}:{}", opened.terminal.agent_epoch, Uuid::new_v4());
    let intent = format!("{epoch}:{}:1:105:33", Uuid::new_v4());
    resized[16..22].copy_from_slice(&["105", "33", "105", "33", &epoch, &intent]);
    let resized = resized.join("|");
    for rows in [
        [&live, &exited],
        [&no_device, &no_device],
        [&live, &resized],
    ] {
        let reply = sample(opened.terminal.clone(), &rows.map(String::as_str), 2).await;
        let Response::TerminalStatus { status } = reply else {
            panic!("expected same-scope unavailable provider: {reply:?}");
        };
        assert_eq!(status.terminal, opened.terminal);
        assert_eq!(status.committed, opened.committed);
        assert_eq!(status.observation, ObservationState::Unavailable);
        assert!(status.provider.is_none());
    }
    let reply = sample(opened.terminal.clone(), &[&resized, &resized], 2).await;
    let Response::TerminalStatus { status } = reply else {
        panic!("expected ordered geometry readback: {reply:?}");
    };
    assert_eq!(status.terminal, opened.terminal);
    assert_eq!(status.committed.revision, 1);
    assert_eq!(
        status.committed.size,
        TerminalSize {
            cols: 105,
            rows: 33
        }
    );
    assert_eq!(status.observation, ObservationState::Pending);
    assert_eq!(status.provider.unwrap().size, opened.committed.size);
    assert_eq!(fixture.dispatches(), 2);
    stop(job).await;
}

#[tokio::test]
async fn w1_create_catalog_fault_read_only() {
    let fixture = Fixture::new();
    let (_master, _slave, tty) = observation_pty();
    fixture.birth_device(&tty);
    let local = async |message| request(&fixture.root, &message).await.unwrap();
    let read = |path| fs::read_to_string(fixture.root.join(path)).unwrap();
    let lane = fixture.lane().await;
    let job = fixture
        .serve_engine(fixture.engine().with_fixture_sender(), lane.clone())
        .await;
    let workspace_id = fixture.register().await;
    let CreateState::Confirmed {
        outcome,
        current: PaneLiveness::Live,
        ..
    } = fixture
        .call(&create(Uuid::new_v4(), workspace_id, None))
        .await
    else {
        panic!("fixture did not create a live pane");
    };
    let runtime = outcome.runtime;
    let runtime_id = runtime.runtime_id;
    let original_size = TerminalSize {
        cols: 100,
        rows: 30,
    };
    let open = async || -> TerminalStatus {
        let response = local(Request::OpenTerminal {
            runtime_id,
            view_id: Uuid::new_v4(),
        })
        .await;
        let Response::TerminalOpened { status } = response else {
            panic!("managed observation unavailable: {response:?}");
        };
        status
    };
    let before = open().await;
    assert_eq!(before.observation, ObservationState::Observed);
    assert_eq!(before.provider.unwrap().size, original_size);
    let target = &before.terminal.target;
    assert_eq!(target.runtime_id, runtime_id);
    assert_eq!(target.generation.to_string(), read("generation").trim());
    assert_eq!(
        (&*target.session, &*target.window, &*target.pane),
        ("$1", "@1", "%1")
    );
    assert_eq!(target.pid, std::process::id());
    assert!(target.process_start > 0);
    let acquire = || Request::AcquireTerminal {
        terminal: before.terminal.clone(),
    };
    let response = local(acquire()).await;
    let Response::TerminalControl { token, epoch } = response else {
        panic!("healthy pane did not grant control: {response:?}");
    };
    let binding = PasteBinding {
        terminal: before.terminal.clone(),
        token,
        epoch,
        incarnation: Uuid::new_v4(),
        operation: 1,
        sequence: 0,
    };
    let ordinary = |token| Request::Input {
        runtime_id,
        token,
        sequence: 0,
        data: b"x".to_vec(),
        paste: false,
    };
    let release = |token| Request::Release { runtime_id, token };
    let stage = Request::PasteStage {
        binding: binding.clone(),
        offset: 0,
        data: b"x".to_vec(),
    };
    assert!(matches!(
        local(stage).await,
        Response::Paste {
            paste: PasteResult::Staged {}
        }
    ));

    let path = fixture.catalog.join("catalog.sqlite3");
    let retained = fixture.catalog.join("retained.sqlite3");
    let buffers_before_fault = fixture.buffer_snapshot();
    fs::rename(&path, &retained).unwrap();
    let blocked_key = Uuid::new_v4();
    assert_eq!(
        fixture.call(&create(blocked_key, workspace_id, None)).await,
        CreateState::Unknown {
            issue: Some(ProductIssue::CatalogMissing)
        }
    );
    assert!(
        matches!(local(diagnostic_create(&fixture, Uuid::new_v4(), Uuid::new_v4())).await,
            Response::Error { message } if message == "Creation capacity unavailable: CatalogMissing")
    );
    assert!(!fixture.root.join("diagnostic-dispatched").exists());
    let Response::TerminalStatus { status } = local(Request::TerminalStatus {
        terminal: before.terminal.clone(),
    })
    .await
    else {
        panic!("current observer lost status");
    };
    assert_eq!(status.terminal, before.terminal);
    assert_eq!(status.provider.unwrap().size, original_size);
    let fault = open().await;
    assert_eq!(&fault.terminal.target, target);
    assert_eq!(fault.committed, status.committed);
    assert_eq!(fault.observation, ObservationState::Observed);
    let mut stale = fault.terminal;
    stale.target.process_start += 1;
    assert!(matches!(
        local(Request::TerminalStatus { terminal: stale }).await,
        Response::TerminalChanged {}
    ));
    let Response::Runtimes { runtimes } = local(Request::List {}).await else {
        panic!("managed pane missing from inventory");
    };
    assert_eq!(runtimes.len(), 1);
    assert_eq!(
        (runtimes[0].session_id, runtimes[0].runtime_id),
        (runtime.session_id, runtime_id)
    );
    assert_eq!(
        (runtimes[0].workspace_id, runtimes[0].operation_id),
        (None, None)
    );
    for frame in ["first frame\n", "next frame\n"] {
        fixture.set("output", frame);
        let Response::Snapshot { text } = local(Request::Snapshot { runtime_id }).await else {
            panic!("pane output unavailable");
        };
        assert_eq!(text, frame);
    }

    let resize = Request::ResizeTerminal {
        terminal: before.terminal.clone(),
        token,
        sequence: 0,
        size: TerminalSize {
            cols: 117,
            rows: 39,
        },
    };
    let marker = read("marker");
    let epoch = read("epoch");
    for mutation in [resize, ordinary(token)] {
        let response = local(mutation).await;
        assert!(
            matches!(&response, Response::Error { message } if message == "Managed terminal catalog unavailable: CatalogMissing"),
            "{response:?}"
        );
    }
    let commit = Request::PasteCommit { binding, bytes: 1 };
    assert!(matches!(
        local(commit).await,
        Response::Paste {
            paste: PasteResult::Unavailable {}
        }
    ));
    assert!(matches!(local(release(token)).await, Response::Accepted {}));
    for mutation in [acquire(), Request::Acquire { runtime_id }] {
        let response = local(mutation).await;
        assert!(
            matches!(&response, Response::Error { message } if message == "Managed terminal catalog unavailable: CatalogMissing"),
            "{response:?}"
        );
    }
    assert_eq!(read("marker"), marker);
    assert_eq!(read("epoch"), epoch);
    assert_eq!(read("buffer-loads"), "load\n");
    assert_eq!(fixture.buffer_snapshot(), buffers_before_fault);
    for effect in ["resizes", "inputs", "unexpected-terminal-mutations"] {
        assert!(!fixture.root.join(effect).exists());
    }
    assert_eq!(fixture.dispatches(), 1);

    fs::rename(&retained, &path).unwrap();
    drop(lane);
    stop(job).await;
    fixture.wait_restart_released().await;
    let lane = fixture.lane().await;
    let job = fixture
        .serve_engine(fixture.engine().with_fixture_sender(), lane.clone())
        .await;
    let restored = open().await;
    assert_eq!(&restored.terminal.target, target);
    assert!(matches!(
        fixture.status(blocked_key).await,
        CreateState::Absent {}
    ));
    let restored_acquire = || Request::AcquireTerminal {
        terminal: restored.terminal.clone(),
    };
    let response = local(restored_acquire()).await;
    let Response::TerminalControl {
        token: restored_token,
        ..
    } = response
    else {
        panic!("restored pane did not grant control: {response:?}");
    };
    assert_ne!(restored_token, token);
    assert!(matches!(
        local(Request::Acquire { runtime_id }).await,
        Response::Error { .. }
    ));
    assert!(matches!(
        local(ordinary(token)).await,
        Response::Error { .. }
    ));
    assert!(matches!(
        local(ordinary(restored_token)).await,
        Response::Accepted {}
    ));
    assert_eq!(read("inputs").lines().count(), 1);
    assert!(matches!(
        local(release(restored_token)).await,
        Response::Accepted {}
    ));
    let Response::Control { token: cli_token } = local(Request::Acquire { runtime_id }).await
    else {
        panic!("restored pane did not grant command line control");
    };
    assert!(matches!(
        local(release(cli_token)).await,
        Response::Accepted {}
    ));

    let changed_generation = serde_json::to_string(&ServerGeneration {
        pid: 42,
        start: 100,
        token: Uuid::new_v4(),
    })
    .unwrap();
    lane.run(move |db, _| {
        assert_eq!(
            db.connection_for_test()
                .execute(
                    "UPDATE runtime_attempts SET generation = ?1 WHERE id = ?2",
                    rusqlite::params![changed_generation, runtime_id.to_string()],
                )
                .unwrap(),
            1
        );
        Ok(())
    })
    .await
    .unwrap();
    let readiness = ProductRequest::WorkspaceReadiness {
        workspace_id,
        profile: ProviderProfile::Codex,
    };
    assert_eq!(
        product_request(&fixture.root, &readiness).await.unwrap(),
        ProductResponse::Ready {}
    );
    for mutation in [
        Request::OpenTerminal {
            runtime_id,
            view_id: Uuid::new_v4(),
        },
        restored_acquire(),
        Request::Acquire { runtime_id },
    ] {
        let response = local(mutation).await;
        assert!(
            matches!(&response, Response::Error { message } if message == "Managed terminal identity changed"),
            "{response:?}"
        );
    }
    let mismatched_view = Request::TerminalStatus {
        terminal: restored.terminal,
    };
    assert!(matches!(
        local(mismatched_view).await,
        Response::TerminalChanged {}
    ));
    assert_eq!(fixture.dispatches(), 1);
    stop(job).await;
}

async fn release_on_catalog_busy(holder: File, mut contention: mpsc::Receiver<()>) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(2), contention.recv())
        .await
        .context("fixture catalog did not report Busy before release")?
        .context("fixture catalog worker stopped without reporting Busy")?;
    holder.unlock()?;
    drop(holder);
    Ok(())
}

async fn assert_catalog_handoff(fixture: &mut Fixture, server: FixtureServer) {
    let locator = fixture.workspace.to_str().unwrap().to_owned();
    let identity = resolve_workspace(&fixture.workspace).unwrap();
    let workspace_id = fixture.register().await;
    stop(server).await;
    fixture.catalog_busy = None;
    fixture.wait_restart_released().await;

    let lane = fixture.lane().await;
    let server = fixture.serve(lane.clone()).await;
    let readiness = ProductRequest::WorkspaceReadiness {
        workspace_id,
        profile: ProviderProfile::Codex,
    };
    assert_eq!(
        product_request(&fixture.root, &readiness).await.unwrap(),
        ProductResponse::Ready {}
    );
    let saved = lane
        .run(move |db, profiles| {
            let (locator, launch) = db.preflight(workspace_id, ProviderProfile::Codex, profiles)?;
            Ok((locator, launch.workspace))
        })
        .await
        .unwrap();
    assert_eq!(saved, (locator, identity));
    assert_eq!(fixture.register().await, workspace_id);

    fs::rename(&fixture.workspace, fixture.root.join("original-workspace")).unwrap();
    fs::create_dir(&fixture.workspace).unwrap();
    assert_eq!(
        product_request(&fixture.root, &readiness).await.unwrap(),
        ProductResponse::Blocked {
            issue: ProductIssue::WorkspaceChanged,
        }
    );
    stop(server).await;
}

#[tokio::test]
async fn w1_create_ordinary_catalog_fixture_registers_after_lock_handoff() {
    let mut fixture = Fixture::new();
    let holder = fixture.hold_catalog().await;
    let (observed, contention) = mpsc::channel(1);
    fixture.catalog_busy = Some(observed);
    let startup = async { fixture.serve(fixture.lane().await).await };
    let (server, release) = tokio::join!(startup, release_on_catalog_busy(holder, contention));
    release.unwrap();
    assert_catalog_handoff(&mut fixture, server).await;
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn w1_create_debug_catalog_fixture_registers_after_lock_handoff() {
    let mut fixture = Fixture::new();
    let holder = fixture.hold_catalog().await;
    let (observed, contention) = mpsc::channel(1);
    let root = fixture.root.clone();
    let catalog = fixture.catalog.clone();
    let server = FixtureServer(tokio::spawn(async move {
        serve_catalog_fixture_with(
            root.clone(),
            root.join("tmux"),
            catalog.clone(),
            root.as_os_str(),
            |_| Ok(()),
            Some((catalog, observed)),
        )
        .await
    }));
    let ((), release) = tokio::join!(
        fixture.wait_listening(&server),
        release_on_catalog_busy(holder, contention)
    );
    release.unwrap();
    assert_catalog_handoff(&mut fixture, server).await;
}

#[cfg(debug_assertions)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn w1_create_state_guard_precedes_catalog() {
    use tokio::sync::oneshot;

    async fn start(
        fixture_start: bool,
        root: PathBuf,
        catalog: PathBuf,
        on_catalog: impl FnOnce() + Send + 'static,
    ) -> Result<()> {
        let tmux = root.join("tmux");
        let path = root.clone();
        if fixture_start {
            serve_catalog_fixture_with(
                root,
                tmux,
                catalog,
                path.as_os_str(),
                move |directory| {
                    on_catalog();
                    catalog::provision(directory)
                },
                None,
            )
            .await
        } else {
            serve_with_catalog_directory(
                root,
                tmux,
                move || {
                    on_catalog();
                    Ok(catalog)
                },
                ServiceProfiles::new(Some(path.as_os_str())),
            )
            .await
        }
    }

    for fixture_wins in [false, true] {
        let fixture = Fixture::new();
        let catalog = if fixture_wins {
            fixture.root.join("unprovisioned")
        } else {
            fixture.wait_catalog_released().await;
            fixture.catalog.clone()
        };
        let (reached, pending) = oneshot::channel();
        let (release, resume) = std::sync::mpsc::sync_channel(1);
        let winner = FixtureServer(tokio::spawn(start(
            fixture_wins,
            fixture.root.clone(),
            catalog.clone(),
            move || {
                reached.send(()).unwrap();
                resume.recv_timeout(Duration::from_secs(5)).unwrap();
            },
        )));
        tokio::time::timeout(Duration::from_secs(3), pending)
            .await
            .unwrap()
            .unwrap();

        let (opened, loser_work) = oneshot::channel();
        let loser = start(!fixture_wins, fixture.root.clone(), catalog, move || {
            let _ = opened.send(());
        })
        .await;
        assert!(
            loser
                .unwrap_err()
                .to_string()
                .contains("another agent already owns this state directory")
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(3), loser_work)
                .await
                .unwrap()
                .is_err(),
            "a losing start attempted catalog work"
        );
        release.send(()).unwrap();
        fixture.wait_listening(&winner).await;
        let workspace = fixture.register().await;
        assert_eq!(
            product_request(
                &fixture.root,
                &ProductRequest::WorkspaceReadiness {
                    workspace_id: workspace,
                    profile: ProviderProfile::Codex,
                }
            )
            .await
            .unwrap(),
            ProductResponse::Ready {}
        );
        stop(winner).await;
    }
}

#[tokio::test]
async fn w1_create_concurrent_equal_keys_finalize_once_and_preserve_original_result() {
    let fixture = Fixture::new();
    let job = fixture.serve(fixture.lane().await).await;
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
    fixture.set(&format!("{}.pane", runtime.tmux_name), "$1|@1|%1|\n");
    let job = fixture.restart(job).await;
    assert_eq!(
        fixture.call(&request).await,
        CreateState::Confirmed {
            outcome: outcome.clone(),
            current: PaneLiveness::Exited,
            setup_issue: None,
        }
    );
    // @contract: A replaced pane changes current liveness and preserves the stored launch outcome.
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
    let lane = fixture.lane().await;
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
    let lane = fixture.lane().await;
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
    let lane = fixture.lane().await;
    let job = fixture.serve(lane.clone()).await;
    let workspace = fixture.register().await;
    let (key, id) = fixture.reserve(&lane, workspace).await;
    // @invariant: A committed marker blocks redispatch even when its acknowledgement is lost.
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
    let job = fixture.serve(fixture.lane().await).await;
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
    let stamp_path = format!("{name}.stamp");
    let nonce = fs::read(fixture.root.join(&nonce_path)).unwrap();
    let pane = fs::read(fixture.root.join(&pane_path)).unwrap();
    let stamp = fs::read(fixture.root.join(&stamp_path)).unwrap();
    let generation = fs::read(fixture.root.join("generation")).unwrap();
    for (file, invalid, original) in [
        (nonce_path.as_str(), Uuid::new_v4().to_string(), &nonce),
        (pane_path.as_str(), "$1|@1|%99|301\n".into(), &pane),
        (pane_path.as_str(), "$1|@1|%1|302\n".into(), &pane),
        (pane_path.as_str(), "$1|@1|%1|\n".into(), &pane),
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
    fixture.set(&pane_path, "$1|@1|%99|\n");
    assert!(matches!(
        fixture.status(key).await,
        CreateState::Unknown { .. }
    ));
    fixture.set(&pane_path, "$1|@1|%1|\n");
    fixture.set(&stamp_path, "$1:@1:%1:\n");
    assert!(matches!(
        fixture.status(key).await,
        CreateState::Unknown { .. }
    ));
    fixture.set(&stamp_path, stamp);
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
    assert_eq!(outcome.runtime.pane_pid, None);
    assert_eq!(
        crate::model::parse_name(name),
        Some((outcome.runtime.session_id, outcome.runtime.runtime_id))
    );
    assert_eq!(fixture.dispatches(), 1);
    stop(job).await;
}

#[tokio::test]
async fn w1_create_unproved_or_previously_used_birth_stays_read_only_after_reconciliation() {
    for mode in [
        "missing",
        "used",
        "other-ticket",
        "other-generation",
        "prior-send",
    ] {
        let fixture = Fixture::new();
        let (_master, _slave, tty) = observation_pty();
        fixture.birth_device(&tty);
        let job = fixture
            .serve_engine(fixture.engine().with_fixture_sender(), fixture.lane().await)
            .await;
        let workspace = fixture.register().await;
        let key = Uuid::new_v4();
        fixture.set("mode", "effect-lost");
        assert!(matches!(
            fixture.call(&create(key, workspace, None)).await,
            CreateState::Unknown { .. }
        ));
        let name = fs::read_to_string(fixture.root.join("names")).unwrap();
        let name = name.trim_end();
        let file = format!("{name}.birth");
        let birth = fs::read_to_string(fixture.root.join(&file)).unwrap();
        let changed = match mode {
            "missing" => {
                fs::remove_file(fixture.root.join(&file)).unwrap();
                None
            }
            "used" => Some(format!("used~{}", birth.trim_end())),
            "other-ticket" | "other-generation" => {
                let mut fields: Vec<_> = birth.trim_end().split('~').map(str::to_owned).collect();
                fields[if mode == "other-ticket" { 7 } else { 3 }] = Uuid::new_v4().to_string();
                Some(fields.join("~"))
            }
            "prior-send" => {
                let fields: Vec<_> = birth.trim_end().split('~').collect();
                let launch = Uuid::new_v4();
                let epoch = format!("{}:{}", Uuid::new_v4(), Uuid::new_v4());
                let paste_nonce = Uuid::new_v4();
                let buffer = Uuid::new_v4().simple();
                fixture.set("epoch", &epoch);
                fixture.set("paste-launch", launch.to_string());
                fixture.set("marker", format!(
                    "v1~{}~{}~42~100~1~1~1~{}~{}~{}~{launch}~{epoch}~s~{paste_nonce}~1~rs-paste-{buffer}",
                    fields[5], fields[3], fields[11], fields[12], fields[6],
                ));
                Some(birth.trim_end().to_owned())
            }
            _ => unreachable!(),
        };
        if let Some(edited) = &changed {
            fixture.set(&file, edited);
        }
        let marker = fs::read(fixture.root.join("marker")).ok();
        let recovered = fixture.status(key).await;
        let CreateState::Confirmed {
            outcome,
            current: PaneLiveness::Live,
            ..
        } = &recovered
        else {
            panic!("original pane lost observation in {mode}: {recovered:?}");
        };
        let runtime_id = outcome.runtime.runtime_id;
        let local = async |request_value| request(&fixture.root, &request_value).await.unwrap();
        let opened = local(Request::OpenTerminal {
            runtime_id,
            view_id: Uuid::new_v4(),
        })
        .await;
        let Response::TerminalOpened { status } = opened else {
            panic!("pane unavailable in {mode}: {opened:?}");
        };
        let acquire = local(Request::AcquireTerminal {
            terminal: status.terminal,
        })
        .await;
        assert!(
            matches!(acquire, Response::ControlRecovering { .. }),
            "{mode}: {acquire:?}"
        );
        assert!(matches!(
            local(Request::Input {
                runtime_id,
                token: Uuid::new_v4(),
                sequence: 0,
                data: b"x".to_vec(),
                paste: false
            })
            .await,
            Response::Error { .. }
        ));
        assert_eq!(fixture.status(key).await, recovered);
        assert_eq!(fixture.dispatches(), 1);
        assert_eq!(fs::read(fixture.root.join("marker")).ok(), marker);
        assert_eq!(fs::read_to_string(fixture.root.join(file)).ok(), changed);
        for effect in ["inputs", "buffer-loads", "unexpected-terminal-mutations"] {
            assert!(!fixture.root.join(effect).exists(), "{mode}: {effect}");
        }
        stop(job).await;
    }
}

#[tokio::test]
async fn w1_create_collision_aborts_without_stamping_the_existing_pane() {
    let fixture = Fixture::new();
    let lane = fixture.lane().await;
    let job = fixture.serve(lane.clone()).await;
    let workspace = fixture.register().await;
    let (key, runtime_id) = fixture.reserve(&lane, workspace).await;
    let original = lane
        .run(move |db, _| Ok(db.operation(key)?.unwrap()))
        .await
        .unwrap();
    fixture.set(&format!("{}.pane", original.target), "$4|@4|%4|304\n");
    let command = create(key, workspace, Some(runtime_id));
    assert!(matches!(
        fixture.call(&command).await,
        CreateState::Unknown { .. }
    ));
    assert!(matches!(
        fixture.status(key).await,
        CreateState::Unknown { .. }
    ));
    assert_eq!(fixture.dispatches(), 0);
    for option in ["initializer", "birth", "nonce", "stamp"] {
        assert!(
            !fixture
                .root
                .join(format!("{}.{option}", original.target))
                .exists()
        );
    }
    assert_eq!(
        fs::read_to_string(fixture.root.join(format!("{}.pane", original.target))).unwrap(),
        "$4|@4|%4|304\n"
    );
    stop(job).await;
}

#[cfg(target_os = "macos")]
struct PrivateCreateTmux {
    binary: PathBuf,
    socket: PathBuf,
}

#[cfg(target_os = "macos")]
impl PrivateCreateTmux {
    fn command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.binary);
        command
            .args(["-N", "-S"])
            .arg(&self.socket)
            .env_remove("TMUX");
        command
    }
}

#[cfg(target_os = "macos")]
impl Drop for PrivateCreateTmux {
    fn drop(&mut self) {
        let _ = self.command().arg("kill-server").output();
    }
}

#[cfg(target_os = "macos")]
async fn check_real_create_reply_loss(replacement: bool) {
    use std::process::Command;

    let fixture = Fixture::new();
    let tmux = PrivateCreateTmux {
        binary: find_executable("tmux")
            .expect("tmux 3.8 is required for private launch verification"),
        socket: fixture.root.join("tmux.sock"),
    };
    assert_eq!(
        Command::new(&tmux.binary)
            .arg("-V")
            .output()
            .unwrap()
            .stdout,
        b"tmux 3.8\n"
    );
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/tmux.conf");
    let started = Command::new(&tmux.binary)
        .arg("-f")
        .arg(config)
        .arg("-S")
        .arg(&tmux.socket)
        .args([
            "new-session",
            "-d",
            "-s",
            "canary",
            "--",
            "/bin/sleep",
            "75",
        ])
        .env_remove("TMUX")
        .output()
        .unwrap();
    assert!(started.status.success(), "{started:?}");
    tests::fixture_program(
        &fixture.root.join("tmux"),
        &format!(
            "#!/bin/sh\nset -eu\ncase \"${{7-}}\" in\n  'new-session '*' __managed-launch '*)\n    '{bin}' \"$@\" > '{root}/real-receipt'\n    printf 'spawned\\n' >> '{root}/real-spawns'\n    exit 1 ;;\nesac\nexec '{bin}' \"$@\"\n",
            bin = tmux.binary.display(),
            root = fixture.root.display(),
        ),
    );
    let launch = fixture.root.join("launch");
    tests::fixture_program(
        &launch,
        &format!(
            "#!/bin/sh\ntest \"$1\" = __managed-launch || exit 2\nexport RS_W1_LAUNCH_POLICY=\"$2\"\nexec '{}' --exact w1_create_tests::launcher_child_fixture --ignored\n",
            std::env::current_exe().unwrap().display(),
        ),
    );
    tests::fixture_program(
        &fixture.root.join("codex"),
        &format!(
            "#!/bin/sh\nset -eu\ntest -z \"${{RS_CREATE_NONCE+x}}${{RS_CREATE_TICKET+x}}\"\n/bin/stty -echo -icanon min 1 time 0\nprintf 'start\\n' >> '{root}/provider-starts'\n/bin/dd bs=1 count=1 of='{root}/provider-input' 2>/dev/null\nexec /bin/sleep 45\n",
            root = fixture.root.display(),
        ),
    );
    let lane = fixture.lane().await;
    let job = fixture
        .serve_engine(
            fixture.engine().with_creation_fixture(Some(launch), None),
            lane.clone(),
        )
        .await;
    let workspace = fixture.register().await;
    let key = Uuid::new_v4();
    let command = create(key, workspace, None);
    assert_eq!(
        fixture.call(&command).await,
        CreateState::Unknown {
            issue: Some(ProductIssue::TmuxUnavailable)
        }
    );
    assert_eq!(
        fs::read(fixture.root.join("real-spawns")).unwrap(),
        b"spawned\n"
    );
    let original = lane
        .run(move |db, _| Ok(db.operation(key)?.unwrap()))
        .await
        .unwrap();
    assert_eq!(original.stage, OperationStage::Marked);
    let receipt = fs::read_to_string(fixture.root.join("real-receipt")).unwrap();
    let pid: u32 = receipt
        .trim_end()
        .rsplit_once('|')
        .unwrap()
        .1
        .parse()
        .unwrap();
    let start = crate::engine::launch_fixture_process_start(pid).unwrap();
    let pane = receipt.split('|').nth(2).unwrap();
    let pane_scope = || {
        let output = tmux
            .command()
            .args([
                "display-message",
                "-p",
                "-t",
                pane,
                "#{pane_id}|#{pane_pid}|#{pane_tty}|#{@rs_create_nonce}",
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        output.stdout
    };
    let before = pane_scope();
    let wait = async |name: &str| {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(content) = fs::read(fixture.root.join(name))
                    && !content.is_empty()
                {
                    return content;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap()
    };
    assert_eq!(wait("provider-starts").await, b"start\n");
    let mask = tmux
        .command()
        .args([
            "show-environment",
            "-t",
            &format!("={}", original.target),
            "RS_CREATE_TICKET",
        ])
        .output()
        .unwrap();
    assert!(mask.status.success(), "{mask:?}");
    assert_eq!(mask.stdout, b"-RS_CREATE_TICKET\n");
    drop(lane);
    stop(job).await;
    fixture.wait_restart_released().await;
    let job = fixture
        .serve_engine(
            fixture
                .engine()
                .with_creation_fixture(None, replacement.then_some(start + 1)),
            fixture.lane().await,
        )
        .await;
    let recovered = fixture.status(key).await;
    let CreateState::Confirmed {
        outcome,
        current: PaneLiveness::Live,
        ..
    } = &recovered
    else {
        panic!("original guarded launch did not recover: {recovered:?}");
    };
    let runtime = &outcome.runtime;
    assert_eq!(
        (runtime.session_id, runtime.runtime_id, runtime.operation_id),
        (original.session_id, original.runtime_id, Some(original.id))
    );
    let open = Request::OpenTerminal {
        runtime_id: runtime.runtime_id,
        view_id: Uuid::new_v4(),
    };
    let Response::TerminalOpened { status } = request(&fixture.root, &open).await.unwrap() else {
        panic!("original pane cannot be observed");
    };
    assert_eq!(
        status.terminal.target.process_start,
        start + u64::from(replacement)
    );
    assert_eq!(status.terminal.target.pid, pid);
    assert_eq!(status.terminal.target.pane, runtime.pane_id);
    assert_eq!(pane_scope(), before);
    let acquire = Request::AcquireTerminal {
        terminal: status.terminal,
    };
    let granted = request(&fixture.root, &acquire).await.unwrap();
    let token = if replacement {
        assert!(
            matches!(granted, Response::ControlRecovering { .. }),
            "{granted:?}"
        );
        Uuid::new_v4()
    } else {
        let Response::TerminalControl { token, .. } = granted else {
            panic!("original child did not grant fresh control: {granted:?}");
        };
        token
    };
    let input = Request::Input {
        runtime_id: runtime.runtime_id,
        token,
        sequence: 0,
        data: b"x".to_vec(),
        paste: false,
    };
    let reply = request(&fixture.root, &input).await.unwrap();
    if replacement {
        assert!(matches!(reply, Response::Error { .. }), "{reply:?}");
        assert!(
            fs::read(fixture.root.join("provider-input"))
                .unwrap_or_default()
                .is_empty()
        );
        assert!(matches!(
            request(&fixture.root, &acquire).await.unwrap(),
            Response::ControlRecovering { .. }
        ));
    } else {
        assert!(matches!(reply, Response::Accepted {}), "{reply:?}");
        assert_eq!(wait("provider-input").await, b"x");
        assert!(matches!(
            request(&fixture.root, &input).await.unwrap(),
            Response::Error { .. }
        ));
    }
    assert_eq!(fixture.call(&command).await, recovered);
    assert_eq!(
        fs::read(fixture.root.join("real-spawns")).unwrap(),
        b"spawned\n"
    );
    assert_eq!(
        fs::read(fixture.root.join("provider-starts")).unwrap(),
        b"start\n"
    );
    if !replacement {
        let pane = runtime.pane_id.as_str();
        let birth = || {
            tmux.command()
                .args(["show-options", "-pqv", "-t", pane, "@rs_create_birth"])
                .output()
                .unwrap()
        };
        let used = birth();
        assert!(
            used.status.success() && used.stdout.starts_with(b"used~v1~"),
            "{used:?}"
        );
        fs::remove_file(fixture.root.join("provider-starts")).unwrap();
        fs::remove_file(fixture.root.join("provider-input")).unwrap();
        let respawn = tmux
            .command()
            .args(["respawn-pane", "-k", "-t", pane])
            .output()
            .unwrap();
        assert!(respawn.status.success(), "{respawn:?}");
        assert_eq!(wait("provider-starts").await, b"start\n");
        assert_eq!(birth().stdout, used.stdout);
        assert!(matches!(
            fixture.status(key).await,
            CreateState::Confirmed {
                current: PaneLiveness::Unavailable,
                ..
            }
        ));
        assert!(matches!(
            request(&fixture.root, &acquire).await.unwrap(),
            Response::Error { .. }
        ));
        let stale = Request::Input {
            runtime_id: runtime.runtime_id,
            token,
            sequence: 1,
            data: b"y".to_vec(),
            paste: false,
        };
        assert!(matches!(
            request(&fixture.root, &stale).await.unwrap(),
            Response::Error { .. }
        ));
        assert!(
            fs::read(fixture.root.join("provider-input"))
                .unwrap_or_default()
                .is_empty()
        );
        assert_eq!(
            fs::read(fixture.root.join("real-spawns")).unwrap(),
            b"spawned\n"
        );
        let diagnostic = Request::Create {
            session_id: Uuid::new_v4(),
            runtime_id: Uuid::new_v4(),
            cwd: fixture.workspace.clone(),
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf '%s\\n%s' \"$(/bin/pwd)\" \"$2\" > \"$1.tmp\"; /bin/mv \"$1.tmp\" \"$1\""
                    .into(),
                "fixture".into(),
                fixture
                    .root
                    .join("diagnostic-output")
                    .to_str()
                    .unwrap()
                    .into(),
                "literal #{} $name ' \" ; value".into(),
            ],
        };
        let created = request(&fixture.root, &diagnostic).await.unwrap();
        assert!(matches!(created, Response::Created { .. }), "{created:?}");
        assert_eq!(
            wait("diagnostic-output").await,
            format!(
                "{}\nliteral #{{}} $name ' \" ; value",
                fixture.workspace.display()
            )
            .as_bytes()
        );
    }
    stop(job).await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn w1_create_real_lost_tmux_reply_keeps_original_birth_and_admits_fresh_input() {
    check_real_create_reply_loss(false).await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn w1_create_real_lost_tmux_reply_holds_same_pid_with_changed_kernel_start() {
    check_real_create_reply_loss(true).await;
}

#[tokio::test]
async fn w1_create_quick_exit_with_no_current_pid_finalizes_and_survives_restart() {
    let fixture = Fixture::new();
    let job = fixture.serve(fixture.lane().await).await;
    let workspace = fixture.register().await;
    let key = Uuid::new_v4();
    fixture.set("mode", "exited");
    let request = create(key, workspace, None);
    let launched = fixture.call(&request).await;
    let CreateState::Confirmed {
        outcome,
        current: PaneLiveness::Exited,
        setup_issue: None,
    } = &launched
    else {
        panic!("expected a confirmed retained pane");
    };
    let runtime = &outcome.runtime;
    assert_eq!(runtime.pane_id, "%1");
    assert_eq!(runtime.pane_pid, None);
    assert_eq!(runtime.origin, RuntimeOrigin::Managed);
    assert_eq!(runtime.workspace_id, Some(workspace));
    assert!(runtime.exited);

    let job = fixture.restart(job).await;
    assert_eq!(fixture.status(key).await, launched);
    assert_eq!(fixture.call(&request).await, launched);
    let Response::Runtimes { runtimes } = crate::request(&fixture.root, &Request::List {})
        .await
        .unwrap()
    else {
        panic!("expected inventory");
    };
    assert_eq!(runtimes.as_slice(), std::slice::from_ref(runtime));
    assert_eq!(fixture.dispatches(), 1);
    stop(job).await;
}

#[tokio::test]
async fn w1_create_delayed_dispatch_times_out_and_does_not_send_again() {
    let fixture = Fixture::new();
    let job = fixture.serve(fixture.lane().await).await;
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
        let lane = fixture.lane().await;
        let job = fixture.serve(lane.clone()).await;
        let workspace = fixture.register().await;
        let (key, id) = fixture.reserve(&lane, workspace).await;
        let legacy = [
            tmux_name(Uuid::new_v4(), Uuid::new_v4()),
            tmux_name(Uuid::new_v4(), Uuid::new_v4()),
        ];
        fixture.set("names", format!("{}\n{}\n", legacy[0], legacy[1]));
        for (index, name) in legacy.iter().enumerate() {
            let pid = if index == 0 { "90" } else { "" };
            fixture.set(
                &format!("{name}.pane"),
                format!("$9|@9|%{}|{pid}\n", 9 + index),
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
            assert_eq!(runtime.pane_pid, (index == 0).then_some(90));
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
            "#!/bin/sh\ntest -z \"$RS_CREATE_NONCE$RS_CREATE_TICKET\" || exit 20\n/bin/pwd > '{}'\n",
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
        birth: None,
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
            .env("RS_CREATE_TICKET", Uuid::new_v4().to_string())
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
