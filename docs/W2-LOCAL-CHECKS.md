# W2 iPhone fixture local verification

Recorded on 2026-11-26.

- Tested commit `5e9977b137fb2c24a3635c5b56a2338ed2323007`
- Tested tree `bdbefa16536fa779d6cb377c15aed232e598becc`

All eight local checks passed on this revision. Repository CI and signed-device acceptance require their own evidence for the revision selected for signing.

The bundled, offline Tauri and xterm fixture uses a bounded native recorder for synthetic interactions only. It has no host, provider CLI, relay, remote transport, or network input path. Remote runtime remains disabled.

## Fixture and evidence checks

Scoped local verification SHA-256 `fb51c62f41dfd463bea34a5249586176eb73e3289ba01f59e2989444d139c46f`.

### Mounted fixture TAP suite (exit status 0)

```bash
bash -ec 'export RUSTUP_HOME="$PWD/.cache/rustup" CARGO_HOME="$PWD/.cache/cargo"; mkdir -p "$RUSTUP_HOME" "$CARGO_HOME"; export PATH="$CARGO_HOME/bin:$PATH"; if ! W2_RUSTUP=$(type -P rustup); then w2_git=$(git rev-parse --path-format=absolute --git-common-dir); W2_RUSTUP="${w2_git%/*}/.cache/cargo/bin/rustup"; fi; test -x "$W2_RUSTUP"; export W2_RUSTUP; if ! "$W2_RUSTUP" which --toolchain 1.97.1 cargo >/dev/null 2>&1 || ! "$W2_RUSTUP" which --toolchain 1.97.1 rustc >/dev/null 2>&1; then "$W2_RUSTUP" toolchain install 1.97.1 --profile minimal --component cargo --component rustc; fi; "$W2_RUSTUP" run 1.97.1 cargo -V; "$W2_RUSTUP" run 1.97.1 rustc -V; status=0; out=$(node --test --test-reporter=tap apps/client/w2-fixture/fixture.test.ts 2>&1) || status=$?; printf "%s\n" "$out"; test "$status" -eq 0; printf "%s\n" "$out" | grep -Eq "^ok [0-9]+ - xterm input reaches the native recorder$"; printf "%s\n" "$out" | grep -Eq "^ok [0-9]+ - mandatory cases use independent vectors and exact native case IDs$"; printf "%s\n" "$out" | grep -Eq "^ok [0-9]+ - paste vectors match xterm newline normalization$"; printf "%s\n" "$out" | grep -Eq "^ok [0-9]+ - production case controls launch every mandatory manifest vector$"; printf "%s\n" "$out" | grep -Eq "^ok [0-9]+ - viewport updates keep one terminal and dispose listeners$"; printf "%s\n" "$out" | grep -Eq "^ok [0-9]+ - mounted fixture wires layout without remounting its terminal$"; printf "%s\n" "$out" | grep -Eq "^ok [0-9]+ - mounted composition checkpoint reads native bytes before commit without moving focus$"; printf "%s\n" "$out" | grep -Eq "^ok [0-9]+ - foreground restart preserves evidence without replay$"; printf "%s\n" "$out" | grep -Eq "^# pass [1-9][0-9]*$"'
```

### Fixture TypeScript check (exit status 0)

```bash
node node_modules/typescript/bin/tsc --noEmit -p apps/client/w2-fixture/tsconfig.json
```

### Guarded bundle TAP suite (exit status 0)

```bash
bash -ec 'status=0; out=$(node --test --test-reporter=tap apps/client/w2-fixture/bundle.test.ts 2>&1) || status=$?; printf "%s\n" "$out"; test "$status" -eq 0; printf "%s\n" "$out" | grep -Eq "^ok [0-9]+ - guarded bundle rejects desktop modules and external assets$"; printf "%s\n" "$out" | grep -Eq "^# pass [1-9][0-9]*$"'
```

### Production Vite build (exit status 0)

```bash
node node_modules/vite/bin/vite.js build apps/client/w2-fixture --config apps/client/w2-fixture/vite.config.ts
```

### Python evidence-validator unit suite (exit status 0)

```bash
bash -ec 'out=$(python3 -m unittest discover -s tests/integration -p test_w2_iphone_evidence.py 2>&1); printf "%s\\n" "$out"; [[ "$out" =~ Ran[[:space:]][1-9][0-9]*[[:space:]]tests? ]]'
```

### Retained-report validation in check-report mode (exit status 0)

```bash
python3 tests/integration/w2_iphone_evidence.py --manifest apps/client/w2-fixture/cases.json --report docs/W2-IPHONE-EVIDENCE.json --check-report
```

## Repository checks

| Recorded command | Exit status | Retained log SHA-256 |
| --- | --- | --- |
| `rustup run 1.97.1 make check` | 0 | `534695af5677a258385e24392d6c2d31ebbc4ec9ae2cd96e958c03f71cd74916` |
| `rustup run 1.97.1 make check-native` | 0 | `d1ad034372741aa247fd7e88df6c2687512f8b05f5e78b80d819939b45c6154f` |

## Signed-device acceptance

Native app launch and signed physical iPhone acceptance were unrun. The signed-device outcome is unknown and acceptance is deferred. The [retained evidence report](W2-IPHONE-EVIDENCE.json) records `setup-hold` with no measured cohorts. The successful report validation confirms that the recorded hold is valid.

Follow the [signed iPhone acceptance guide](W2-IPHONE-ACCEPTANCE.md) for two clean passes per mandatory case on the signed physical device. Verify that intended input arrives exactly once, the active cursor is visible, touch selection, scrolling, and paste work, and fresh input arrives after foreground recovery.

The production mobile shell and minimum supported iOS version remain open pending review of signed-device evidence and generated build settings. Evaluate an alternate shell only if the same mandatory failure repeats on the signed baseline and persists after one bounded, layer-specific correction.
