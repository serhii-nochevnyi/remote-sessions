"""Validate the redacted physical iPhone evidence against the shipped W2 vectors."""

import argparse
from collections import Counter
from contextlib import contextmanager
from dataclasses import dataclass, field, replace
import hashlib
import json
import math
from pathlib import Path
import re
import sys
import unicodedata


MANIFEST_SHA256 = "3825bfd16e9380d222b4a07b24966758cc090ed93bbc4ab6951e18a6cac5580c"
COMMANDS = ("w2_begin_attempt", "w2_append_input", "w2_read_attempt")
GRANTS = {"allow-" + command.replace("_", "-"): [command] for command in COMMANDS}
CAPABILITY = {
    "identifier": "w2-fixture", "platforms": ["iOS"], "local": True,
    "windows": ["w2-fixture"], "permissions": list(GRANTS),
}
LOCKS = ("package-lock.json", "Cargo.lock", "apps/client/w2-fixture/src-tauri/Cargo.lock")
TOOLS = ("node", "npm", "vite", "tauri_cli", "tauri", "wry", "xterm", "cargo", "rustc")
TYPING_ACTIONS = {
    "english-return": ("return_pressed_once",),
    "ukrainian-return": ("ukrainian_key_pressed_once", "return_pressed_once"),
}
OBSERVATIONS = {
    "english-return": ("cursor_visible", *TYPING_ACTIONS["english-return"]),
    "ukrainian-return": ("cursor_visible", *TYPING_ACTIONS["ukrainian-return"]),
    "composition-commit": ("preedit_visible", "end_state_observed"),
    "composition-replacement": ("preedit_visible", "replaced_in_preedit", "end_state_observed"),
    "composition-cancel": ("preedit_visible", "end_state_observed"),
    "paste-short": ("focused_prompt", "paste_available"),
    "paste-multiline": ("focused_prompt", "paste_available"),
    "touch-selection-copy": ("handles_usable", "exact_selection", "exact_clipboard_copy"),
    "touch-scroll": ("history_visible", "prompt_and_cursor_restored"),
    "keyboard-open-cursor": ("keyboard_opened", "prompt_unobscured"),
    "rotation": ("rotated_and_returned", "prompt_unobscured"),
    "app-switch": ("switched_and_returned", "prompt_visible", "started_fresh"),
    "lock-foreground": ("locked_and_returned", "prompt_visible", "started_fresh"),
}
COMPOSITION = {
    "composition-commit": (["é"], "commit"),
    "composition-replacement": (["é", "è"], "commit"),
    "composition-cancel": (["é"], "cancel"),
}
VIEWPORTS = {
    "keyboard-open-cursor": {"initial": "portrait", "keyboard_open": "portrait"},
    "rotation": {"initial": "portrait", "rotated": "landscape", "restored": "portrait"},
}
PASTE = {"paste-short", "paste-multiline"}
PASTE_ACTIONS = {"sample": 1, "native": 2}
RECOVERY = {"app-switch", "lock-foreground"}
RECORDER_OPERATIONS = ("begin", "append", "read")
LAYOUT_CASES = PASTE | RECOVERY | VIEWPORTS.keys() | {"touch-selection-copy", "touch-scroll"}
LAYOUT_EVENTS = {
    "mounted", "container resize", "window resize", "orientation change", "focus",
    "blur", "visibility change", "viewport resize", "viewport scroll",
}
COHORTS = ("baseline", "corrected", "comparison")
PRE_XCODE = ("generated_paths", "vite_build", "declared_boundary", "mac_cargo")


class EvidenceError(ValueError):
    """The record is incomplete or inconsistent."""


class MeasuredFailure(EvidenceError):
    """The recorded procedure and outcome establish a mandatory failure."""

    def __init__(self, detail, observed, *, old_identity=None):
        super().__init__(detail)
        self.observed = observed
        self.old_identity = old_identity


@dataclass(frozen=True)
class Result:
    status: str
    detail: str = ""
    observed: tuple = ()
    old_observation: tuple | None = None
    old_witness_eligible: bool = False

    @property
    def failure_signature(self):
        return self.status, self.detail, self.observed


@dataclass
class Qualification:
    rehearsals: set
    detail: str = ""
    fingerprint: str = ""
    captures: tuple = ()


def need(condition, detail):
    if not condition:
        raise EvidenceError(detail)


def measured(condition, detail, *observed):
    if not condition:
        raise MeasuredFailure(detail, observed)


def obj(value, label):
    need(isinstance(value, dict), f"{label} requires an object")
    return value


def items(value, label):
    need(isinstance(value, list), f"{label} requires an array")
    return value


def string(value):
    return isinstance(value, str) and bool(value.strip())


def named(record, *keys):
    return all(string(record.get(key)) for key in keys)


def number(value, minimum=0):
    return type(value) is int and value >= minimum


def real(value, minimum=0):
    return type(value) in (int, float) and minimum <= value < math.inf


def hex_text(value):
    return isinstance(value, str) and re.fullmatch(r"(?:[0-9a-f]{2})*", value) is not None


def digest(value):
    return hex_text(value) and len(value) == 64


def revision(value):
    return hex_text(value) and len(value) == 40


def literal_paths(paths, label):
    paths = items(paths, label)
    need(paths and all(
        string(path) and path == path.strip() and path == unicodedata.normalize("NFC", path)
        and not re.match(r"^[A-Za-z]:", path) and not re.search(r"[\\*?\[\]{}\x00-\x1f\x7f]", path)
        and not {"", ".", ".."}.intersection(path.split("/"))
        for path in paths
    ), f"{label} requires literal relative paths")
    need(len(paths) == len(set(paths)), f"{label} requires unique paths")
    return paths


def fingerprint(record):
    encoded = json.dumps(record, sort_keys=True, separators=(",", ":"), ensure_ascii=True,
                         allow_nan=False).encode("ascii")
    return hashlib.sha256(encoded).hexdigest()


def load_manifest(path):
    content = path.read_bytes()
    need(hashlib.sha256(content).hexdigest() == MANIFEST_SHA256, "manifest differs from the shipped matrix")
    return {case["id"]: case for case in parse_json(content)["cases"]}


def version_tuple(value):
    need(isinstance(value, str) and re.fullmatch(r"[0-9]{1,4}(?:\.[0-9]{1,4}){0,2}", value),
         "missing or invalid OS or deployment version")
    components = tuple(map(int, value.split(".")))
    return components + (0,) * (3 - len(components))


def check_tuple(data, role):
    need(data.get("destination") == "physical-iphone", "a signed physical iPhone destination is required")
    need(named(data, "shell", "device_alias", "device_model", "ios_build", "deployment_target"),
         "missing shell, redacted device alias, device model, iOS build or deployment target")
    expected_shell = "native-wkwebview" if role == "comparison" else "tauri"
    need(data["shell"] == expected_shell, "cohort shell does not match its role")
    need(version_tuple(data.get("ios_version")) >= version_tuple(data["deployment_target"]),
         "generated deployment target is newer than the tested OS")
    need(data.get("orientation") == "portrait", "record the prescribed starting orientation")
    keyboard = obj(data.get("keyboard"), "keyboard")
    languages = items(keyboard.get("languages"), "keyboard.languages")
    need(all(string(language) for language in languages) and {"en", "uk"} <= set(languages)
         and len(set(languages)) == len(languages), "record the stock English and Ukrainian keyboards")
    scenario_languages = obj(keyboard.get("scenario_languages"), "keyboard.scenario_languages")
    need(set(scenario_languages) == set(OBSERVATIONS)
         and all(language in languages for language in scenario_languages.values())
         and scenario_languages["english-return"] == "en" and scenario_languages["ukrainian-return"] == "uk",
         "bind the selected keyboard for each case")
    settings = obj(keyboard.get("settings"), "keyboard.settings")
    need(settings.get("stock") is True and settings.get("hardware_keyboard") is False
         and all(type(settings.get(key)) is bool for key in ("autocorrection", "predictive")),
         "stock keyboard and measured keyboard settings are required")
    need(named(obj(data.get("xcode"), "xcode"), "version", "build"), "missing Xcode version or build")
    need(named(obj(data.get("sdk"), "sdk"), "version", "build", "root"), "missing iPhoneOS SDK version, build or root")
    need(revision(data.get("source_revision")) and digest(data.get("bundle_sha256")),
         "missing full signed source revision or bundle fingerprint")
    need(data.get("fixture_sha256") == MANIFEST_SHA256, "cohort fixture digest differs from the shipped matrix")
    app = obj(data.get("app"), "signed app identity")
    need(named(app, "identifier", "version", "build"), "app identifier, version and build are required")
    if role == "comparison":
        return
    need(app["identifier"] == "dev.remotesessions.w2fixture", "unexpected signed fixture identifier")
    locks = obj(data.get("locks"), "lockfile digests")
    need(all(digest(locks.get(path)) for path in LOCKS), "missing locked input fingerprints")
    tools = obj(data.get("tools"), "locked tool versions")
    need(named(tools, *TOOLS) and all(tools[tool].split()[:2] == [tool, "1.97.1"] for tool in ("cargo", "rustc")),
         "missing tool versions or wrong Rust pin")


def check_ci(ci, source):
    for name in ("core", "native"):
        job = obj(ci.get(name), f"CI {name}")
        need(job.get("result") == "pass" and job.get("source_revision") == source and named(job, "ref"),
             f"CI {name} requires a passing result for the signed revision")


def check_pre_xcode(preflight, source):
    need(preflight.get("source_revision") == source and named(preflight, "ref"),
         "pre-Xcode gate must identify the signed source and retained results")
    for name in PRE_XCODE:
        need(number(preflight.get(name)) and preflight[name] == 0, f"pre-Xcode {name} did not pass")


def check_build(build, data, role):
    install = obj(build.get("installation"), "installation")
    need(install.get("signature_verified") is True and install.get("physical_install_verified") is True
         and install.get("app") == data["app"] and install.get("installed_bundle_sha256") == data["bundle_sha256"]
         and named(install, "ref"), "missing or unbound verified physical installation")
    settings = obj(build.get("generated_settings"), "generated settings")
    need(settings.get("sdk") == "iphoneos" and settings.get("configuration") == "Release"
         and settings.get("sdkroot") == data["sdk"]["root"]
         and settings.get("deployment_target") == data["deployment_target"]
         and version_tuple(settings.get("minimum_os_version")) == version_tuple(data["deployment_target"])
         and named(settings, "ref"), "generated Release SDK or minimum OS differs from the tested tuple")
    entry = obj(build.get("entry_validation"), "generated entry validation")
    need(named(entry, "build_log_ref") and digest(entry.get("build_log_sha256"))
         and entry.get("source_revision") == data["source_revision"]
         and entry.get("inspected_bundle_sha256") == data["bundle_sha256"]
         and entry.get("installed_bundle_sha256") == install["installed_bundle_sha256"]
         and entry.get("verified_before_install") is True, "missing or unbound pre-install generated entry validation")
    binding = {key: entry[key] for key in ("build_log_ref", "build_log_sha256", "source_revision")} | {
        "bundle_sha256": data["bundle_sha256"],
    }
    if role == "comparison":
        need(settings.get("source_revision") == data["source_revision"]
             and settings.get("bundle_sha256") == install["installed_bundle_sha256"],
             "generated native settings lack signed source and bundle binding")
        need(named(entry, "entry_ref", "registration_ref")
             and digest(entry.get("entry_sha256")) and digest(entry.get("registration_sha256"))
             and entry.get("entry_verified") is True and entry.get("registration_verified") is True,
             "missing verified native entry and registration provenance")
        binding.update(registration_ref=entry["registration_ref"], registration_sha256=entry["registration_sha256"])
    else:
        need(named(entry, "archive_ref", "symbol") and entry.get("launcher_link_verified") is True
             and entry.get("defined_global_symbol") is True and number(entry.get("cli_exit_status"))
             and entry["cli_exit_status"] == 0, "missing generated entry validation from the linked archive and locked CLI")
        need(entry["symbol"] == "_start_app" and entry.get("cli_start_app_validation") is True,
             "locked CLI and linked archive must both verify the generated start_app entry")
    return binding


def check_tauri_boundary(boundary, binding):
    need(all(digest(boundary.get(key)) for key in ("config_sha256", "capabilities_sha256", "acl_sha256")),
         "effective config and generated ACL digests are required")
    need(boundary.get("capability") == CAPABILITY and boundary.get("capability", {}).get("local") is True
         and boundary.get("file_capabilities") == {}
         and boundary.get("resolved_grants") == GRANTS
         and boundary.get("handlers") == list(COMMANDS)
         and boundary.get("window_labels") == ["w2-fixture"], "effective iOS capability or command boundary differs")
    desktop = obj(boundary.get("desktop"), "same-worktree Mac regression")
    need(desktop.get("binding") == binding and named(desktop, "ref")
         and desktop.get("inventory") in ("empty", "synthetic")
         and all(desktop.get(key) is True for key in ("scaffold_status", "local_inventory", "fixture_commands_absent")),
         "safe desktop invokes and registry separation must be verified on the signed worktree")
    return COMMANDS


def check_native_boundary(boundary, binding):
    webview = obj(boundary.get("webview"), "effective native WebView configuration")
    origin = webview.get("bundled_origin")
    local_origin = string(origin) and (origin == "file://" or (
        re.fullmatch(r"[a-z][a-z0-9+.-]*://[a-z0-9][a-z0-9.-]*", origin)
        and origin.split(":", 1)[0] not in {"file", "http", "https", "ws", "wss", "ftp", "ftps"}
    ))
    need(named(webview, "ref") and digest(webview.get("sha256"))
         and local_origin and webview.get("allowed_origins") == [origin]
         and webview.get("bundled_assets_only") is True and webview.get("main_frame_only") is True,
         "effective native WebView requires the sole bundled origin and main frame")
    registration = obj(boundary.get("registration"), "effective native handler registration")
    need(registration.get("ref") == binding["registration_ref"]
         and registration.get("sha256") == binding["registration_sha256"],
         "effective native registration does not match the signed build")
    handlers = obj(registration.get("handlers"), "registered native handlers")
    need(len(handlers) == len(RECORDER_OPERATIONS) and all(string(name) for name in handlers)
         and all(operation in RECORDER_OPERATIONS for operation in handlers.values())
         and set(handlers.values()) == set(RECORDER_OPERATIONS), "native handlers must map to exactly begin, append and read")
    dependencies = obj(boundary.get("dependencies"), "effective native dependency review")
    need(named(boundary, "package_review_ref") and named(dependencies, "ref") and digest(dependencies.get("sha256")),
         "missing native package or dependency review references")
    by_operation = {operation: handler for handler, operation in handlers.items()}
    return tuple(by_operation[operation] for operation in RECORDER_OPERATIONS)


def check_boundary(boundary, binding, role):
    need(boundary.get("binding") == binding and named(boundary, "ref"), "effective boundary lacks signed build binding")
    commands = check_native_boundary(boundary, binding) if role == "comparison" else check_tauri_boundary(boundary, binding)
    need(all(boundary.get(key) is True for key in (
        "plist_reviewed", "entitlements_reviewed", "no_extra_network_or_url_access",
        "ios_dependency_tree_reviewed", "no_host_provider_or_desktop_dependencies",
    )), "signed package or iOS dependency review is incomplete")
    offline = obj(boundary.get("offline_launch"), "offline launch")
    need(offline.get("bundle_sha256") == binding["bundle_sha256"] and named(offline, "ref")
         and all(offline.get(key) is True for key in ("airplane_mode", "wifi_off", "cellular_off", "bundled_screen_visible"))
         and offline.get("startup_hex") == "" and offline.get("tracer_hex") == "d1970d",
         "missing bound offline startup silence or exact tracer")
    ops = items(offline.get("operations"), "offline recorder operations")
    begin, append, read = commands
    need(3 <= len(ops) <= 5 and ops == [begin, *([append] * (len(ops) - 2)), read],
         "offline run must actually begin, append and read")


def cohort_readiness(cohort, role):
    try:
        data = obj(cohort.get("tuple"), "measured tuple")
        check_tuple(data, role)
        need(cohort.get("fingerprint") == fingerprint(data), "cohort fingerprint does not match the measured tuple")
        if role != "comparison":
            check_pre_xcode(obj(cohort.get("pre_xcode"), "pre-Xcode gate"), data["source_revision"])
        check_ci(obj(cohort.get("ci"), "repository CI"), data["source_revision"])
        binding = check_build(obj(cohort.get("build"), "signed build provenance"), data, role)
        check_boundary(obj(cohort.get("boundary"), "effective boundary"), binding, role)
    except EvidenceError as error:
        return Result("inconclusive", str(error))
    return Result("pass")


@dataclass(frozen=True)
class ArtifactProof:
    payload_sha256: str | None = None
    detail: str = ""


def build_artifacts(cohort, readiness):
    if readiness.status != "pass":
        return ArtifactProof()
    build, data = cohort["build"], cohort["tuple"]
    settings, entry = build["generated_settings"], build["entry_validation"]
    installed = build["installation"]["installed_bundle_sha256"]
    try:
        need(settings.get("source_revision") == data["source_revision"]
             and settings.get("bundle_sha256") == installed,
             "generated settings lack signed source and bundle binding")
        payload = obj(build.get("common_payload"), "signed common payload")
        need(named(payload, "review_ref") and digest(payload.get("review_sha256")),
             "common payload requires a retained inventory review")
        binding = {key: entry[key] for key in (
            "source_revision", "build_log_ref", "build_log_sha256", "inspected_bundle_sha256",
        )} | {"installed_bundle_sha256": installed}
        need(all(payload.get(key) == value for key, value in binding.items()),
             "common payload review lacks signed source, build log or inspected and installed bundle binding")
        inventory = items(payload.get("inventory"), "common payload inventory")
        need(all(isinstance(asset, dict) and set(asset) == {"path", "sha256"} and digest(asset["sha256"])
                 for asset in inventory), "common payload inventory requires path and byte digest entries")
        paths = literal_paths([asset["path"] for asset in inventory], "common payload inventory")
        need(paths == sorted(paths), "common payload inventory must be sorted by path")
        verified = fingerprint(inventory)
        need(payload.get("inventory_sha256") == verified, "common payload inventory digest differs")
    except EvidenceError as error:
        return ArtifactProof(detail=str(error))
    return ArtifactProof(verified)


def capture_segment(record, label):
    if "capture_segment" not in record:
        return None
    segment = obj(record["capture_segment"], label)
    start, end = segment.get("start_ms"), segment.get("end_ms")
    need(number(start) and number(end) and start < end,
         f"{label} requires increasing integer milliseconds")
    return start, end


def disjoint_segments(first, second):
    return first is not None and second is not None and (first[1] <= second[0] or second[1] <= first[0])


@dataclass(frozen=True)
class Capture:
    kind: str
    ref: str
    pair: tuple
    window: tuple | None
    owner: tuple
    detail: str = ""

    @classmethod
    def from_record(cls, kind, record, pair, owner=()):
        label = kind if kind == "rehearsal" else f"retained {kind}"
        try:
            window = capture_segment(record, f"{label} capture segment")
        except EvidenceError as error:
            return cls(kind, record["ref"], pair, None, owner, str(error))
        return cls(kind, record["ref"], pair, window, owner)

    def conflict(self, other):
        kinds = {self.kind, other.kind}
        nested = kinds & {"restart", "begin failure"}
        if nested and self.owner == other.owner:
            if "measurement" in kinds:
                inner, outer = (other, self) if self.kind == "measurement" else (self, other)
                return inner.containment_issue(outer)
            return ""
        if ("launch" in kinds and self.pair == other.pair) or disjoint_segments(self.window, other.window):
            return ""
        if kinds == {"launch"}:
            recording = "launch recording across signed builds"
        elif "launch" in kinds:
            observation = (kinds - {"launch"}).pop()
            recording = f"launch and {observation} recording across signed builds"
        elif kinds == {"measurement"}:
            recording = "measured observation"
        elif kinds == {"rehearsal"}:
            recording = "rehearsal recording"
        elif kinds == {"restart"}:
            return "shared foreground restart observation is reused without disjoint capture segments"
        elif nested:
            recording = " and ".join(sorted(kinds)) + " recording"
        else:
            recording = "rehearsal and measured observation"
        return f"shared {recording} requires disjoint capture segments"

    def containment_issue(self, outer):
        if self.ref == outer.ref and self.window is not None and outer.window is not None:
            if not outer.window[0] <= self.window[0] < self.window[1] <= outer.window[1]:
                return f"retained {self.kind} capture segment lies outside the enclosing {outer.kind}"
        return ""


def qualify(recipe, case, cohort, role, pair):
    data = cohort["tuple"]
    preedit, end_action = COMPOSITION[case["id"]]
    language = recipe.get("keyboard_language")
    need(language == data["keyboard"]["scenario_languages"][case["id"]] and recipe.get("keyboard") == data["keyboard"],
         "recipe did not use the bound stock keyboard settings")
    need(recipe.get("case_id") == case["id"] and recipe.get("expected_hex") == case["expected"]["afterHex"]
         and recipe.get("preedit") == preedit and recipe.get("end_action") == end_action
         and named(recipe, "end_state"), "recipe differs from the manifest gesture or vector")
    steps = items(recipe.get("steps"), "recipe steps")
    need(len(steps) >= 2 and all(string(step) for step in steps), "record ordered preedit and end gestures")
    frozen = {key: recipe[key] for key in (
        "case_id", "keyboard_language", "keyboard", "expected_hex", "steps", "preedit", "end_action", "end_state",
    )}
    verified_fingerprint = fingerprint(frozen)
    need(recipe.get("id") == verified_fingerprint, "recipe ID does not match its frozen procedure")
    rehearsals = items(recipe.get("rehearsals"), "recipe rehearsals")
    seen = set()
    captures = []
    detail = ""
    for rehearsal in rehearsals:
        try:
            rehearsal = obj(rehearsal, "rehearsal")
            need(rehearsal.get("cohort_fingerprint") == cohort["fingerprint"],
                 "recipe rehearsal belongs to a different signed cohort")
            need(named(rehearsal, "id", "ref") and rehearsal["id"] not in seen
                 and rehearsal.get("recipe_id") == verified_fingerprint
                 and rehearsal.get("preedit") == preedit and rehearsal.get("end_state") == recipe["end_state"],
                 "rehearsal has missing, duplicate or different visible outcomes")
        except EvidenceError as error:
            detail = detail or str(error)
            continue
        seen.add(rehearsal["id"])
        owner = (role, case["id"], rehearsal["id"])
        captures.append(Capture.from_record("rehearsal", rehearsal, pair, owner))
    if len(seen) < 2:
        detail = detail or "two distinct observable rehearsals are required"
    return Qualification(seen, detail, verified_fingerprint, tuple(captures))


def attempt_identity(record):
    return (record.get("caseId"), record.get("attemptId"), record.get("generation"))


def snapshot(record, identity):
    record = obj(record, "native snapshot")
    need(attempt_identity(record) == identity and number(record.get("attemptId"), 1)
         and number(record.get("generation"), 1), "native snapshot belongs to a different attempt or generation")
    value, count = record.get("hex"), record.get("acceptedEvents")
    need(hex_text(value) and number(count) and count <= 256
         and (count <= len(value) // 2 <= min(count * 1024, 16384)),
         "native snapshot hex or accepted event count is missing or inconsistent")
    lengths = record.get("eventLengths")
    if lengths is not None:
        need(isinstance(lengths, list) and all(number(n, 1) and n <= 1024 for n in lengths)
             and len(lengths) == count and sum(lengths) * 2 == len(value),
             "native event lengths do not match the snapshot")
    return record


TRACE_FIELDS = {
    "start": (), "update": (), "empty update": (), "end": (),
    "active checkpoint": ("bytes", "nativeSequence"),
    "admission": ("bytes",), "rejection": ("bytes",),
    "append dispatch": ("bytes", "nativeSequence"),
    "append acknowledgment": ("bytes", "nativeSequence"),
    "append delivery unconfirmed": ("bytes", "nativeSequence"),
    "final snapshot": ("bytes", "nativeSequence"),
}


def check_trace(trace, identity):
    need(attempt_identity(trace) == identity and number(trace.get("attemptId"), 1)
         and number(trace.get("generation"), 1), "composition trace belongs to a different attempt or generation")
    events = items(trace.get("events"), "composition trace events")
    need(0 < len(events) <= 96 and trace.get("complete") is True, "composition trace is missing or truncated")
    elapsed = 0
    by_event = {event: [] for event in TRACE_FIELDS}
    for sequence, event in enumerate(events, 1):
        event = obj(event, "composition event")
        name = event.get("event")
        need(string(name) and name in TRACE_FIELDS, "unknown or uncertain composition event")
        fields = TRACE_FIELDS[name]
        need(set(event) == {"sequence", "event", "elapsedMs", *fields}, "composition trace must be text-free and complete")
        need(number(event["sequence"], 1) and event["sequence"] == sequence
             and real(event["elapsedMs"], elapsed), "composition event order is missing, duplicated or reversed")
        need(all(number(event[field], int(field == "nativeSequence")) for field in fields),
             "invalid native sequence or byte count in composition trace")
        elapsed = event["elapsedMs"]
        by_event[name].append(event)
    return by_event


def only(events, name):
    need(len(events[name]) == 1, f"expected one composition {name} event")
    return events[name][0]


def check_composition(attempt, final, identity, expected):
    events = check_trace(obj(attempt.get("trace"), "composition trace"), identity)
    start = only(events, "start")["sequence"]
    end = only(events, "end")["sequence"]
    final_event = only(events, "final snapshot")
    trace_length = len(attempt["trace"]["events"])
    need(start == 1 and end < final_event["sequence"] == trace_length, "composition start, end or final read is out of order")
    checkpoint = snapshot(attempt.get("checkpoint"), identity)
    need(checkpoint.get("active") is True and checkpoint.get("focused") is True
         and named(checkpoint, "ref"), "missing visibly active and focused native checkpoint")
    samples = events["active checkpoint"]
    need(samples and number(checkpoint.get("sequence"), 1) and samples[-1]["sequence"] == checkpoint["sequence"],
         "active checkpoint does not identify the last completed trace sample")
    updates = sorted((event["sequence"], active) for name, active in (("update", True), ("empty update", False))
                     for event in events[name])
    need(updates and all(start < seq < end for seq, _ in updates),
         "trace lacks ordered composition updates before compositionend")
    for sample in samples:
        prior = [active for seq, active in updates if seq < sample["sequence"]]
        need(sample["sequence"] < end and prior and prior[-1], "checkpoint outside nonempty preedit")
    last_sample = samples[-1]
    if identity[0] == "composition-replacement":
        need(sum(active for seq, active in updates if seq < last_sample["sequence"]) >= 2,
             "active checkpoint precedes the replacement update")
    need(last_sample["bytes"] * 2 == len(checkpoint["hex"])
         and number(checkpoint.get("nextSequence"), 1)
         and last_sample["nativeSequence"] == checkpoint.get("nextSequence"),
         "trace checkpoint counters do not match the native checkpoint")
    need(all(event["bytes"] > 0 for name in ("admission", "rejection", "append dispatch",
                                             "append acknowledgment", "append delivery unconfirmed")
             for event in events[name]), "input or append evidence must identify nonempty data")

    admissions = events["admission"]
    dispatches = events["append dispatch"]
    acks = events["append acknowledgment"]
    unconfirmed = events["append delivery unconfirmed"]
    need(len(admissions) == len(dispatches), "incomplete admission or dispatch sequence")
    receipts = {}
    for receipt in acks + unconfirmed:
        index = receipt["nativeSequence"]
        need(index <= len(dispatches) and index not in receipts, "append receipt has an unknown or repeated native sequence")
        receipts[index] = receipt
    previous = start
    for index, (admit, dispatch) in enumerate(zip(admissions, dispatches), 1):
        need(previous < admit["sequence"] < dispatch["sequence"] < trace_length
             and dispatch["nativeSequence"] == index and admit["bytes"] == dispatch["bytes"],
             "append order, byte counts or native sequence is inconsistent")
        receipt = receipts.get(index)
        previous = dispatch["sequence"]
        if receipt is not None:
            need(previous < receipt["sequence"] < trace_length and receipt["bytes"] == dispatch["bytes"],
                 "append receipt order or byte count is inconsistent")
            previous = receipt["sequence"]
    lengths = [event["bytes"] for event in dispatches]
    final_count = final["acceptedEvents"]
    final_lengths = lengths[:final_count]
    need(named(final, "ref") and final["ref"] != checkpoint["ref"]
         and final_event["bytes"] * 2 == len(final["hex"]) and number(final.get("nextSequence"), 1)
         and final_event["nativeSequence"] == final["nextSequence"] == final_count + 1
         and final_count <= len(dispatches) and all(ack["nativeSequence"] <= final_count for ack in acks)
         and sum(final_lengths) == final_event["bytes"]
         and final.get("eventLengths", final_lengths) == final_lengths,
         "final native snapshot does not reconcile with dispatched appends")
    accepted = 0
    for sample in samples:
        acknowledged = max((ack["nativeSequence"] for ack in acks if ack["sequence"] < sample["sequence"]), default=0)
        dispatched = sum(dispatch["sequence"] < sample["sequence"] for dispatch in dispatches)
        count = sample["nativeSequence"] - 1
        need(max(accepted, acknowledged) <= count <= dispatched
             and sample["bytes"] == sum(lengths[:count]), "checkpoint counters do not reconcile with the append trace")
        accepted = count
    need(checkpoint["acceptedEvents"] == accepted, "checkpoint event count does not reconcile with native sequence")
    need(final["hex"].startswith(checkpoint["hex"])
         and checkpoint.get("eventLengths", lengths[:accepted]) == lengths[:accepted],
         "checkpoint is not a prefix of the final native snapshot")
    pre_end_ack = max((ack["nativeSequence"] for ack in acks if ack["sequence"] < end), default=0)
    pre_end_count = max(accepted, pre_end_ack)
    pre_end_hex = final["hex"][:2 * sum(lengths[:pre_end_count])]
    measured(pre_end_count == 0, "native input accepted before compositionend", pre_end_hex)
    need(not unconfirmed, "native append delivery is uncertain")
    need(len(acks) == len(dispatches), "incomplete admission, dispatch or acknowledgment sequence")
    need(all(admit["sequence"] > end for admit in admissions)
         and all(event["sequence"] > end for event in events["rejection"]),
         "premature onData or uncertain pre-end native acceptance")
    measured(len(acks) <= int(bool(expected)), "duplicate or unexpected composition append", final["hex"])
    measured(final["hex"] == expected and len(acks) == int(bool(expected)),
             "composition final bytes differ from the manifest", final["hex"])
    measured(not events["rejection"], "unexpected post-end onData callback")


def positive_geometry(value):
    return real(value) and value > 0


def check_layout(attempt, case_id):
    diagnostic = obj(attempt.get("diagnostics"), "WKWebView diagnostics")
    if diagnostic.get("status") == "unavailable":
        need(named(diagnostic, "reason"), "unavailable diagnostic requires a reason")
    else:
        need(diagnostic.get("status") == "available", "record WKWebView diagnostic availability")
        events = items(diagnostic.get("events"), "WKWebView events")
        need(0 < len(events) <= 24, "missing or unbounded WKWebView event log")
        previous = 0
        for event in events:
            event = obj(event, "WKWebView event")
            need(set(event) == {"sequence", "event", "focused", "visible", "width", "height"}
                 and string(event["event"]) and event["event"] in LAYOUT_EVENTS
                 and number(event["sequence"], previous + 1)
                 and type(event["focused"]) is bool and type(event["visible"]) is bool
                 and real(event["width"]) and real(event["height"]),
                 "invalid WKWebView event order or geometry")
            previous = event["sequence"]
    if case_id not in VIEWPORTS:
        return True
    viewports = obj(attempt.get("viewports"), "viewport samples")
    clear = []
    for stage, orientation in VIEWPORTS[case_id].items():
        sample = obj(viewports.get(stage), f"{stage} viewport")
        need(sample.get("orientation") == orientation and positive_geometry(sample.get("width"))
             and positive_geometry(sample.get("height"))
             and all(type(sample.get(key)) is bool for key in ("cursor_visible", "focused", "visible")),
             f"missing {stage} viewport, focus, visibility or cursor observation")
        clear.append(sample["cursor_visible"] and sample["visible"]
                     and (stage != "keyboard_open" or sample["focused"]))
    return all(clear)


def same_native_input(first, second):
    return (first["hex"] == second["hex"] and first["acceptedEvents"] == second["acceptedEvents"]
            and (first.get("eventLengths") is None or second.get("eventLengths") is None
                 or first["eventLengths"] == second["eventLengths"]))


def recovery_snapshots(attempt, identity, observed):
    transition = OBSERVATIONS[identity[0]][0]
    need(observed.get(transition) is True, "foreground transition was not established")
    old = obj(attempt.get("recovery"), "foreground recovery")
    initial = obj(old.get("before"), "pre-interruption native snapshot")
    old_id = attempt_identity(initial)
    need(old_id[0] == identity[0] and number(old_id[1], 1) and number(old_id[2], 1),
         "foreground restart lacks the old native attempt and generation")
    before = snapshot(initial, old_id)
    retained = snapshot(old.get("retained"), old_id)
    need(old.get("delivery_uncertain") is False and type(old.get("old_generation_extra_event")) is bool,
         "retained snapshot or delivery is uncertain")
    return before, retained


def check_restart_outcome(attempt, identity, retained, started_fresh, recovery_prefix):
    old_id = attempt_identity(retained)
    restart = obj(attempt["recovery"].get("restart"), "foreground restart observation")
    previous = obj(restart.get("previous"), "restart's old attempt identity")
    need(named(restart, "ref") and attempt_identity(previous) == old_id
         and number(previous.get("attemptId"), 1) and number(previous.get("generation"), 1),
         "foreground restart observation is not bound to the old attempt")
    outcome = restart.get("outcome")
    need(outcome in ("fresh", "same-generation", "no-new-generation"), "foreground restart outcome is uncertain")
    need(restart.get("action_completed") is True, "missing completed foreground restart action")
    need(started_fresh is (outcome == "fresh"),
         "foreground restart conflicts with the visible fresh-start observation")
    if outcome == "no-new-generation":
        failure = obj(restart.get("begin_failure"), "recorded begin failure")
        need(identity == old_id and attempt_identity(failure) == old_id and named(failure, "ref")
             and number(failure.get("attemptId"), 1) and number(failure.get("generation"), 1)
             and "observed" not in restart and "final" not in attempt,
             "begin failure belongs to a different attempt or conflicts with a new generation")
    elif outcome == "same-generation":
        observed = snapshot(restart.get("observed"), old_id)
        need(identity == old_id and "final" not in attempt and same_native_input(observed, retained)
             and "begin_failure" not in restart, "unchanged generation does not match the retained native snapshot")
    else:
        observed = obj(restart.get("observed"), "restart's fresh attempt identity")
        need(attempt_identity(observed) == identity and number(observed.get("attemptId"), 1)
             and number(observed.get("generation"), 1)
             and old_id[1] < identity[1] and old_id[2] < identity[2] and "begin_failure" not in restart,
             "foreground restart did not identify a fresh attempt and generation")
        need(not recovery_prefix, "fresh foreground restart requires all prescribed actions")
    return outcome


def corroborate_restart(admitted, issue):
    need(admitted is not None, issue or "missing admitted foreground restart capture")
    return admitted.outcome


def check_recovery(attempt, identity, expected, observed, recovery_prefix, admitted, restart_issue):
    before, retained = recovery_snapshots(attempt, identity, observed)
    if not recovery_prefix:
        extra_event = attempt["recovery"]["old_generation_extra_event"]
        before_hex, retained_hex = before["hex"], retained["hex"]
        if not same_native_input(before, retained) or extra_event:
            before_count, retained_count = before["acceptedEvents"], retained["acceptedEvents"]
            observed_input = (before_hex, retained_hex, before_count, retained_count, extra_event)
            if before_hex == retained_hex and before_count == retained_count and not extra_event:
                observed_input += (tuple(before["eventLengths"]), tuple(retained["eventLengths"]))
            raise MeasuredFailure("retained native bytes or events changed", observed_input,
                                  old_identity=attempt_identity(retained))
        if attempt.get("capture_error") is False and before_hex != expected:
            raise MeasuredFailure("initial bytes differ from the manifest", (before_hex,),
                                  old_identity=attempt_identity(retained))
    outcome = corroborate_restart(admitted, restart_issue)
    if outcome == "no-new-generation":
        raise MeasuredFailure("foreground restart produced no new generation", ())
    if outcome == "same-generation":
        raise MeasuredFailure("foreground restart kept the same generation", ())


def check_composition_gesture(attempt, cohort, qualification, observed, identity, expected):
    case_id = identity[0]
    need(not qualification.detail, qualification.detail)
    recipe = cohort["recipes"][case_id]
    need(attempt.get("recipe_id") == qualification.fingerprint
         and attempt["keyboard_language"] == recipe["keyboard_language"],
         "measured gesture does not use the frozen qualified recipe")
    need(all(observed.get(key) is True for key in OBSERVATIONS[case_id]),
         "measured preedit and end state do not establish the qualified gesture")
    need(attempt["id"] not in qualification.rehearsals, "qualification rehearsal is not a fresh measured attempt")
    final = snapshot(attempt.get("final"), identity)
    check_composition(attempt, final, identity, expected)
    return final


def check_capture(attempt, case, cohort, qualification, observed, identity, recovery_prefix, paste_route,
                  admitted, restart_issue):
    case_id = case["id"]
    expected = case["expected"]["afterHex"]
    if case_id in PASTE:
        need(paste_route is not None, "missing or conflicting retained paste action")
    if case_id in RECOVERY:
        check_recovery(attempt, identity, case["expected"]["beforeHex"], observed, recovery_prefix,
                       admitted, restart_issue)
    final = None
    if case_id in COMPOSITION:
        final = check_composition_gesture(attempt, cohort, qualification, observed, identity, expected)
    need(attempt.get("capture_error") is False, "native capture reported an unresolved error")
    for action in TYPING_ACTIONS.get(case_id, ()):
        need(observed.get(action) is True, f"unconfirmed prescribed typing action: {action}")
    return final if final is not None else snapshot(attempt.get("final"), identity)


def check_attempt_procedure(attempt, case):
    case_id = case["id"]
    if case_id in PASTE:
        return obj(attempt.get("observations"), "visible observations"), False
    order = list(range(1, len(case["actions"]) + 1))
    actions = attempt.get("action_order")
    recovery_prefix = case_id in RECOVERY and actions == order[:-1]
    need((actions == order or recovery_prefix) and all(type(i) is int for i in actions),
         "missing or unordered prescribed actions")
    return obj(attempt.get("observations"), "visible observations"), recovery_prefix


def check_attempt_context(attempt, case, cohort):
    case_id = case["id"]
    need(attempt.get("cohort_fingerprint") == cohort["fingerprint"], "attempt belongs to a different cohort tuple")
    need(named(attempt, "ref"), "missing attempt observation reference")
    language = attempt.get("keyboard_language")
    need(language == cohort["tuple"]["keyboard"]["scenario_languages"][case_id]
         and attempt.get("orientation") == cohort["tuple"]["orientation"], "mixed keyboard or orientation conditions")
    return check_attempt_procedure(attempt, case)


def evaluate_attempt(attempt, case, cohort, qualification, retained_launch, paste_route, admitted, restart_issue):
    case_id = case["id"]
    try:
        observed, recovery_prefix = check_attempt_context(attempt, case, cohort)
        identity = (case_id, attempt["attemptId"], attempt["generation"])
        final, capture_issue = None, None
        try:
            final = check_capture(attempt, case, cohort, qualification, observed, identity, recovery_prefix, paste_route,
                                  admitted, restart_issue)
        except MeasuredFailure:
            raise
        except EvidenceError as error:
            if recovery_prefix:
                _, retained = recovery_snapshots(attempt, identity, observed)
                check_restart_outcome(attempt, identity, retained, observed.get("started_fresh"), recovery_prefix)
            capture_issue = error
        measured(attempt.get("start_empty") is not False, "unexpected startup input")
        measured(attempt.get("replayed") is not True, "replayed input")
        if recovery_prefix and capture_issue is not None:
            raise capture_issue
        if final is not None:
            measured(final["hex"] == case["expected"]["afterHex"],
                     "final bytes differ from the manifest", final["hex"])
        typing_actions = TYPING_ACTIONS.get(case_id, ())
        for key in OBSERVATIONS[case_id]:
            if case_id in COMPOSITION or key == "started_fresh" or key in typing_actions:
                continue
            measured(observed.get(key) is not False, f"visible observation failed: {key}")
        viewport_clear = check_layout(attempt, case_id) if case_id in LAYOUT_CASES else True
        measured(viewport_clear, "prompt or cursor obscured in a measured viewport")
        if capture_issue is not None:
            raise capture_issue
        need(all(type(observed.get(key)) is bool for key in OBSERVATIONS[case_id]),
             "missing or ambiguous required visible observation")
        need(type(attempt.get("start_empty")) is bool and type(attempt.get("replayed")) is bool,
             "missing start or replay observation")
    except MeasuredFailure as error:
        old_observation = (retained_launch, *error.old_identity) if error.old_identity is not None else None
        return Result("failing", str(error), error.observed, old_observation)
    except EvidenceError as error:
        return Result("inconclusive", str(error))
    return Result("pass")


def verified_conditions(attempt, case, cohort, qualification, result, paste_route):
    case_id, signed = case["id"], cohort["tuple"]
    conditions = {
        "keyboard_language": signed["keyboard"]["scenario_languages"][case_id],
        "orientation": signed["orientation"],
    }
    for key, expected in conditions.items():
        if attempt.get(key) != expected:
            conditions[key] = None
    if case_id in PASTE:
        conditions["route"] = paste_route
    if case_id not in COMPOSITION:
        return conditions
    conditions["recipe_id"] = None
    if not qualification.detail and conditions["keyboard_language"] is not None:
        try:
            observed, _ = check_attempt_procedure(attempt, case)
        except EvidenceError:
            return conditions
        if result.status != "pass":
            identity = (case_id, attempt["attemptId"], attempt["generation"])
            try:
                check_composition_gesture(attempt, cohort, qualification, observed, identity, case["expected"]["afterHex"])
            except MeasuredFailure:
                pass
            except EvidenceError:
                return conditions
        conditions["recipe_id"] = qualification.fingerprint
    return conditions


def qualifications(cohort, role, pair, cases):
    recipes = obj(cohort.get("recipes", {}), "composition recipes")
    result = {}
    all_rehearsals = set()
    for case_id in COMPOSITION:
        try:
            recipe = obj(recipes.get(case_id), f"{case_id} recipe")
            qualified = qualify(recipe, cases[case_id], cohort, role, pair)
        except EvidenceError as error:
            qualified = Qualification(set(), detail=str(error))
        if not qualified.detail and qualified.rehearsals & all_rehearsals:
            qualified.detail = "rehearsal ID is reused across recipes"
        if not qualified.detail:
            all_rehearsals.update(qualified.rehearsals)
        result[case_id] = qualified
    return result


@dataclass
class CaseRecord:
    required: tuple
    clean: dict = field(default_factory=dict)
    open_attempts: dict = field(default_factory=dict)

    def add(self, attempt, result, conditions):
        self.open_attempts[attempt["id"]] = (result, conditions)
        if result.status != "pass":
            self.clean.clear()
            return
        route = conditions.get("route")
        self.clean.setdefault(route, []).append(attempt["id"])
        if self.passed:
            self.open_attempts.clear()

    @property
    def passed(self):
        return all(len(self.clean.get(route, ())) >= 2 for route in self.required)

    @property
    def support(self):
        return [attempt_id for route in self.required for attempt_id in self.clean[route][-2:]]

    def reproduction(self, ids):
        if not ids or any(attempt_id not in self.open_attempts for attempt_id in ids):
            return None
        result, conditions = self.open_attempts[ids[0]]
        if result.status != "failing" or not conditions or None in conditions.values():
            return None
        signature = result.failure_signature
        latest = None
        supporting = set()
        for attempt_id, (value, established) in self.open_attempts.items():
            if any(observed is not None and conditions[key] != observed for key, observed in established.items()):
                continue
            if value.status == "inconclusive" or established != conditions:
                latest = None
                supporting.clear()
            else:
                latest = value.failure_signature
                if latest == signature and (value.old_observation is None or value.old_witness_eligible):
                    supporting.add(attempt_id)
        if latest != signature or not supporting.issuperset(ids):
            return None
        if len(ids) == 2:
            first, second = result.old_observation, self.open_attempts[ids[1]][0].old_observation
            if first is not None and second is not None:
                launch, case_id, attempt_id, generation = first
                other_launch, other_case, other_id, other_generation = second
                if (launch, case_id) == (other_launch, other_case) and (
                    attempt_id == other_id or generation == other_generation
                ):
                    return None
        return signature, conditions


@dataclass(frozen=True)
class RetainedMeasurement:
    position: tuple
    launch: dict
    record: dict


@dataclass(frozen=True)
class RecoveryProof:
    outcome: str
    old_identity: tuple
    claims: tuple


@dataclass(frozen=True)
class BoundMeasurement:
    retained: RetainedMeasurement
    launch: Capture
    observation: Capture
    recovery: RecoveryProof | None
    recovery_issue: str


def retained_measurements(cohort):
    """Index rows by the launch and position recorded during measurement."""
    measurements = {}
    launch_ids, launch_refs = set(), set()
    last_order = 0
    for launch in items(cohort.get("launches", []), "retained app launches"):
        launch = obj(launch, "retained app launch")
        order = launch.get("order")
        need(named(launch, "id", "ref") and number(order, last_order + 1)
             and launch["id"] not in launch_ids and launch["ref"] not in launch_refs,
             "retained launches require ordered, unique labels and capture references")
        launch_ids.add(launch["id"])
        launch_refs.add(launch["ref"])
        last_order = order
        last_position = 0
        for record in items(launch.get("measurements"), "retained launch measurements"):
            record = obj(record, "retained launch measurement")
            position = record.get("position")
            need(named(record, "attempt_id") and record["attempt_id"] not in measurements
                 and number(position, last_position + 1)
                 and number(record.get("attemptId"), 1) and number(record.get("generation"), 1),
                 "retained measurements require unique attempt IDs and increasing positions")
            measurements[record["attempt_id"]] = RetainedMeasurement((order, position), launch, record)
            last_position = position
    return measurements


def measurement_positions(attempts, records):
    positions = {}
    for attempt in attempts:
        record = records.get(attempt["id"])
        position, native_id, generation = (attempt.get(key) for key in ("measurement_position", "attemptId", "generation"))
        if (record and all(number(value, 1) for value in (position, native_id, generation))
                and position == record.position[1]
                and attempt.get("launch_id") == record.launch["id"]
                and attempt.get("launch_ref") == record.launch["ref"]
                and native_id == record.record["attemptId"] and generation == record.record["generation"]):
            positions[attempt["id"]] = record.position
    return positions


def bind_recovery(retained, attempt, case, cohort, launch, observation):
    observed, prefix = check_attempt_context(attempt, case, cohort)
    record = retained.record
    identity = (case["id"], record["attemptId"], record["generation"])
    _, old = recovery_snapshots(attempt, identity, observed)
    outcome = check_restart_outcome(attempt, identity, old, observed.get("started_fresh"), prefix)

    proof = obj(record.get("recovery_capture"), "retained recovery capture")
    expected = {key: record[key] for key in ("attempt_id", "position", "attemptId", "generation")} | {
        "case_id": case["id"], "launch_id": retained.launch["id"], "launch_ref": launch.ref,
        "source_revision": launch.pair[0], "bundle_sha256": launch.pair[1],
    }
    need(all(proof.get(key) == value for key, value in expected.items())
         and all(number(proof.get(key), 1) for key in ("position", "attemptId", "generation")),
         "retained recovery capture differs from its case, measurement, native identity or signed launch")

    restart = attempt["recovery"]["restart"]
    submitted = [("restart", "restart", restart["ref"])]
    if outcome == "no-new-generation":
        submitted.append(("begin_failure", "begin failure", restart["begin_failure"]["ref"]))
    else:
        need("begin_failure" not in proof, "unexpected retained begin failure for the restart outcome")
    claims = []
    for key, kind, ref in submitted:
        capture = obj(proof.get(key), f"retained {kind} capture")
        need(capture.get("ref") == ref, f"retained {kind} capture reference differs from the submitted proof")
        claim = Capture.from_record(kind, capture, launch.pair, observation.owner)
        issue = claim.detail or claim.containment_issue(launch)
        need(not issue, issue)
        claims.append(claim)
    return RecoveryProof(outcome, attempt_identity(old), tuple(claims))


def bind_measurement(retained, attempt, case, cohort, role, pair):
    launch, record = retained.launch, retained.record
    need(attempt.get("cohort_fingerprint") == cohort["fingerprint"],
         "attempt belongs to a different cohort tuple")
    binding = obj(launch.get("binding"), "retained launch signed binding")
    need((binding.get("source_revision"), binding.get("bundle_sha256")) == pair,
         "retained launch signed binding differs from the signed source or bundle")
    need(named(record, "ref") and record["ref"] == attempt.get("ref"),
         "retained measurement observation reference does not match the attempt")
    launch_capture = Capture.from_record("launch", launch, pair, (role, launch["id"]))
    observation = Capture.from_record("measurement", record, pair, (attempt["id"],))
    recovery, issue = None, ""
    if case["id"] in RECOVERY:
        try:
            recovery = bind_recovery(retained, attempt, case, cohort, launch_capture, observation)
        except EvidenceError as error:
            issue = str(error)
    return BoundMeasurement(retained, launch_capture, observation, recovery, issue)


def verified_paste_route(attempt, record):
    if attempt["case_id"] not in PASTE:
        return None
    proof = record.get("paste_action")
    if not isinstance(proof, dict) or proof.get("case_id") != attempt["case_id"]:
        return None
    route, action, order = proof.get("route"), proof.get("action"), attempt.get("action_order")
    if (string(route) and route in PASTE_ACTIONS and route == attempt.get("route")
            and type(action) is int and action == PASTE_ACTIONS[route]
            and order == [action] and type(order[0]) is int):
        return route
    return None


@dataclass
class MeasuredCaptures:
    recordings: dict = field(default_factory=dict)
    conflicts: dict = field(default_factory=dict)

    def register_context(self, capture):
        peers = self.recordings.setdefault(capture.ref, {})
        if capture in peers:
            return
        for peer in peers:
            if capture.kind == peer.kind == "launch":
                continue
            if conflict := capture.conflict(peer):
                self.conflicts.setdefault(peer, conflict)
                self.conflicts.setdefault(capture, conflict)
        peers[capture] = capture.kind != "launch"

    def issue(self, capture):
        return capture.detail or self.conflicts.get(capture, "")

    def admit(self, *claims):
        for capture in claims:
            issue = self.issue(capture)
            need(not issue, issue)
        for capture in claims:
            for peer, active in self.recordings.get(capture.ref, {}).items():
                if active:
                    conflict = capture.conflict(peer)
                    need(not conflict, conflict)
        for capture in claims:
            self.recordings.setdefault(capture.ref, {})[capture] = True

    def admit_recovery(self, measurement):
        proof = measurement.recovery
        if proof is None:
            return None, measurement.recovery_issue
        try:
            self.admit(*proof.claims)
        except EvidenceError as error:
            return None, str(error)
        return proof, ""

    @contextmanager
    def discard_claims(self):
        recordings, conflicts = self.recordings, self.conflicts
        self.recordings = {ref: peers.copy() for ref, peers in recordings.items()}
        self.conflicts = conflicts.copy()
        try:
            yield
        finally:
            self.recordings, self.conflicts = recordings, conflicts


def moved_attempts(attempts, positions):
    latest = (0, 0)
    moved = set()
    for attempt in attempts:
        attempt_id = attempt["id"]
        if attempt_id in positions:
            position = positions[attempt_id]
            if position < latest:
                moved.add(attempt_id)
            latest = max(latest, position)
    return moved


def native_keys(retained, admitted=None):
    launch = retained.launch["id"]
    native_id, generation = retained.record["attemptId"], retained.record["generation"]
    primary = (launch, native_id, generation)
    if admitted is not None and admitted.outcome == "fresh":
        _, old_id, old_generation = admitted.old_identity
        return ((launch, old_id, old_generation), primary)
    return (primary,)


def out_of_order_attempts(identities):
    latest = {}
    out_of_order = set()
    for attempt_id, keys in identities.items():
        for launch, native_id, generation in keys:
            last_id, last_generation = latest.get(launch, (0, 0))
            if native_id <= last_id or generation <= last_generation:
                out_of_order.add(attempt_id)
            latest[launch] = (max(last_id, native_id), max(last_generation, generation))
    return out_of_order


def chronology_holds(identities):
    keys = [key for row_keys in identities.values() for key in row_keys]
    attempt_ids = Counter((launch, attempt_id) for launch, attempt_id, _ in keys)
    generations = Counter((launch, generation) for launch, _, generation in keys)
    holds = {attempt_id: Result("inconclusive", "native attempt or generation is out of order within an app launch")
             for attempt_id in out_of_order_attempts(identities)}
    for attempt_id, row_keys in identities.items():
        if any(attempt_ids[launch, native_id] > 1 or generations[launch, generation] > 1
               for launch, native_id, generation in row_keys):
            holds[attempt_id] = Result("inconclusive", "native attempt or generation is reused within an app launch")
    return holds


def old_witness_qualifies(attempt, measurement, recovery, old_observation, identities):
    if attempt["observations"].get("started_fresh") is not True or (
        recovery is not None and recovery.outcome != "fresh"
    ):
        return False
    primary, = native_keys(measurement.retained)
    launch, native_id, generation = primary
    _, case_id, old_id, old_generation = old_observation
    try:
        snapshot(attempt.get("final"), (case_id, native_id, generation))
    except EvidenceError:
        return False
    launch_identities = {row_id: keys for row_id, keys in identities.items() if keys[0][0] == launch}
    attempt_id = measurement.retained.record["attempt_id"]
    launch_identities[attempt_id] = ((launch, old_id, old_generation), primary)
    return attempt_id not in chronology_holds(launch_identities)


@dataclass
class CohortAssessment:
    readiness: Result
    rows: dict
    recipes: dict


@dataclass
class CohortEvidence:
    readiness: Result
    artifacts: ArtifactProof
    recipes: dict
    attempts: list
    measurements: dict
    holds: dict
    moved: set
    identities: dict


def prepare_cohort(cohort, role, cases, ids):
    readiness = cohort_readiness(cohort, role)
    artifacts = build_artifacts(cohort, readiness)
    ready = readiness.status == "pass"
    pair = (cohort["tuple"]["source_revision"], cohort["tuple"]["bundle_sha256"]) if ready else None
    recipes = qualifications(cohort, role, pair, cases) if ready else {}
    attempts = [obj(value, "attempt") for value in items(cohort.get("attempts", []), "cohort attempts")]
    rehearsal_ids = {key for recipe in recipes.values() for key in recipe.rehearsals}
    for attempt in attempts:
        need(named(attempt, "id") and attempt["id"] not in ids and attempt["id"] not in rehearsal_ids,
             "duplicate measured attempt ID or reused rehearsal")
        ids.add(attempt["id"])
        case_id = attempt.get("case_id")
        need(string(case_id) and case_id in cases, "unknown or missing mandatory case ID")

    records = retained_measurements(cohort)
    need(records.keys() <= {attempt["id"] for attempt in attempts}, "retained measurement has no submitted attempt")
    bound_positions = measurement_positions(attempts, records)
    moved = moved_attempts(attempts, bound_positions)
    ordered = sorted((attempt for attempt in attempts if attempt["id"] in records),
                     key=lambda attempt: records[attempt["id"]].position)
    unpositioned = [attempt for attempt in attempts if attempt["id"] not in records]
    bound = [attempt for attempt in ordered if attempt["id"] in bound_positions]
    binding_holds, measurements = {}, {}
    if ready:
        for attempt in bound:
            attempt_id = attempt["id"]
            try:
                measurements[attempt_id] = bind_measurement(records[attempt_id], attempt, cases[attempt["case_id"]],
                                                            cohort, role, pair)
            except EvidenceError as error:
                binding_holds[attempt_id] = Result("inconclusive", str(error))
    identities = {attempt["id"]: native_keys(records[attempt["id"]]) for attempt in ordered}
    chronology = [*ordered, *unpositioned]
    for attempt in chronology:
        attempt_id = attempt["id"]
        if not ready:
            binding_holds[attempt_id] = readiness
        elif attempt_id not in bound_positions:
            binding_holds[attempt_id] = Result("inconclusive", "attempt does not match its retained launch and measurement")
    return CohortEvidence(readiness, artifacts, recipes, chronology, measurements, binding_holds, moved, identities)


def prepare_captures(prepared, captures):
    for evidence in prepared.values():
        for qualification in evidence.recipes.values():
            for rehearsal in qualification.captures:
                captures.register_context(rehearsal)
        for measurement in evidence.measurements.values():
            captures.register_context(measurement.launch)
    for evidence in prepared.values():
        for qualification in evidence.recipes.values():
            for rehearsal in qualification.captures:
                qualification.detail = qualification.detail or captures.issue(rehearsal)


def assess_cohort(cohort, prepared, cases, captures):
    identities = prepared.identities.copy()
    holds = chronology_holds(identities) | prepared.holds
    capture_holds, recoveries = {}, {}
    for attempt in prepared.attempts:
        attempt_id = attempt["id"]
        if attempt_id in holds:
            continue
        measurement = prepared.measurements[attempt_id]
        try:
            captures.admit(measurement.launch, measurement.observation)
        except EvidenceError as error:
            capture_holds[attempt_id] = Result("inconclusive", str(error))
        else:
            recovery, issue = captures.admit_recovery(measurement)
            recoveries[attempt_id] = recovery, issue
            identities[attempt_id] = native_keys(measurement.retained, recovery)

    holds = capture_holds | chronology_holds(identities) | prepared.holds
    rows = {case_id: CaseRecord(tuple(PASTE_ACTIONS) if case_id in PASTE else (None,)) for case_id in cases}
    for attempt in prepared.attempts:
        attempt_id, case_id = attempt["id"], attempt["case_id"]
        conditions = {}
        result = holds.get(attempt_id)
        if result is None:
            case, qualification = cases[case_id], prepared.recipes.get(case_id)
            measurement = prepared.measurements[attempt_id]
            retained = measurement.retained
            paste_route = verified_paste_route(attempt, retained.record)
            recovery, issue = recoveries[attempt_id]
            result = evaluate_attempt(attempt, case, cohort, qualification, retained.launch["id"], paste_route,
                                      recovery, issue)
            if result.old_observation is not None:
                eligible = old_witness_qualifies(attempt, measurement, recovery, result.old_observation,
                                                 prepared.identities)
                result = replace(result, old_witness_eligible=eligible)
            conditions = verified_conditions(attempt, case, cohort, qualification, result, paste_route)
            # Keep verified failures at their retained positions and disqualify moved passes.
            if result.status == "pass" and attempt_id in prepared.moved:
                result = Result("inconclusive",
                                "native attempt or generation is out of order in the submitted measurements")
        need(attempt.get("status") == result.status,
             f"{attempt['id']}: expected {result.status}" + (f" ({result.detail})" if result.detail else ""))
        if result.status != "pass":
            need(named(attempt, "reason"), f"{attempt['id']}: record the reason for the non-pass")
        rows[case_id].add(attempt, result, conditions)
    return CohortAssessment(prepared.readiness, rows, prepared.recipes)


def matching_conditions(original, updated, allow_shell_change=False):
    before, after = original["tuple"], updated["tuple"]
    if before["bundle_sha256"] == after["bundle_sha256"]:
        return False

    def stable_conditions(value):
        stable = dict(value)
        stable.pop("source_revision", None)
        stable.pop("bundle_sha256", None)
        if allow_shell_change:
            for key in ("shell", "app", "locks", "tools"):
                stable.pop(key, None)
        else:
            app = dict(stable.get("app", {}))
            app.pop("version", None)
            app.pop("build", None)
            stable["app"] = app
        return stable

    return stable_conditions(before) == stable_conditions(after)


def comparison_recipes_match(assessed):
    if any(assessed[role].readiness.status != "pass" for role in COHORTS):
        return False
    for case_id in COMPOSITION:
        recipes = [assessed[role].recipes[case_id] for role in COHORTS]
        if any(recipe.detail for recipe in recipes):
            return False
        original, corrected, comparison = (recipe.fingerprint for recipe in recipes)
        if not original or original != corrected or original != comparison:
            return False
    return True


def reviewed_correction(correction, cohorts, prepared):
    payloads = {role: prepared[role].artifacts.payload_sha256 for role in ("baseline", "corrected")}
    if not all(payloads.values()):
        return False
    try:
        proof = obj(correction.get("reviewed_diff"), "reviewed correction diff")
        need(named(proof, "diff_ref", "review_ref") and digest(proof.get("diff_sha256"))
             and digest(proof.get("review_sha256")) and proof.get("layer") == correction["layer"],
             "correction requires a retained diff and review for its attributed layer")
        paths = literal_paths(proof.get("changed_paths"), "reviewed correction paths")
        scope = literal_paths(correction.get("scope"), "correction scope")
        need(set(paths) == set(scope), "reviewed correction paths differ from the declared scope")
        for role, payload in payloads.items():
            data = cohorts[role]["tuple"]
            signed = obj(proof.get(role), f"reviewed correction {role} build")
            need(signed.get("source_revision") == data["source_revision"]
                 and signed.get("bundle_sha256") == data["bundle_sha256"]
                 and signed.get("common_payload_sha256") == payload,
                 f"reviewed correction does not bind the signed {role} build and payload")
        need(payloads["baseline"] == payloads["corrected"] or named(proof, "payload_delta_explanation"),
             "reviewed correction must explain the common payload change")
    except EvidenceError:
        return False
    return True


def correction_result(correction, cohorts, assessed, prepared):
    if not correction:
        return False, [], False
    need(named(correction, "case_id", "layer", "change") and correction.get("case_id") in OBSERVATIONS,
         "record one layer-specific correction and its mandatory case")
    case = correction["case_id"]
    repeats = items(correction.get("baseline_attempt_ids"), "baseline failure attempts")
    need(len(repeats) == 2 and all(string(value) for value in repeats) and repeats[0] != repeats[1],
         "name two distinct baseline failure attempts")
    baseline, corrected = assessed["baseline"], assessed["corrected"]
    original = baseline.rows[case]
    reproduced = original.reproduction(repeats)
    need(reproduced is not None, "correction requires the same unresolved measured failure twice on the baseline")
    if corrected.readiness.status != "pass":
        return False, repeats, False
    need(matching_conditions(cohorts["baseline"], cohorts["corrected"])
         and cohorts["baseline"]["tuple"]["source_revision"] != cohorts["corrected"]["tuple"]["source_revision"],
         "correction requires a different signed revision and bundle under the same physical conditions")
    for case_id in COMPOSITION:
        before, after = baseline.recipes[case_id], corrected.recipes[case_id]
        need(before.detail or after.detail or before.fingerprint == after.fingerprint,
             "another build changed a frozen composition recipe")
    if not reviewed_correction(correction, cohorts, prepared):
        return False, repeats, False
    corrected_case = corrected.rows[case]
    for attempt_id in reversed(corrected_case.open_attempts):
        if corrected_case.reproduction([attempt_id]) == reproduced:
            return True, [*repeats, attempt_id], True
    return False, repeats, True


def check_execution(execution):
    need(revision(execution.get("checkout_head")) and named(execution, "observed_at"),
         "execution observations must identify the checkout and date")
    for check in items(execution.get("checks"), "execution checks"):
        check = obj(check, "execution check")
        need(named(check, "name", "detail"), "execution check requires a label and observed detail")
        status = check.get("status")
        exit_code = check.get("exit_code")
        need((status == "pass" and number(exit_code) and exit_code == 0)
             or (status == "failed" and number(exit_code, 1))
             or (status in ("pending", "unavailable") and exit_code is None),
             "execution check status and exit code disagree")


def validate(report, cases):
    report = obj(report, "report")
    need(type(report.get("schema_version")) is int and report["schema_version"] == 1, "unknown report schema version")
    need(report.get("fixture_sha256") == MANIFEST_SHA256, "report fixture digest differs from the shipped matrix")
    for execution in items(report.get("previous_executions", []), "previous checkout observations"):
        check_execution(obj(execution, "previous checkout observation"))
    if "execution" in report:
        check_execution(obj(report["execution"], "checkout observations"))
    entries = obj(report.get("cohorts"), "cohorts")
    need(set(entries) <= set(COHORTS), "only a baseline, one correction and one comparison cohort are allowed")
    cohorts = {role: obj(entries.get(role, {}), role) for role in COHORTS}
    ids = set()
    prepared = {role: prepare_cohort(cohort, role, cases, ids) for role, cohort in cohorts.items()}
    candidate, corrected = (prepared[role].artifacts.payload_sha256 for role in ("comparison", "corrected"))
    candidate_comparable = candidate is not None and candidate == corrected
    shared = prepared if candidate_comparable else {role: prepared[role] for role in ("baseline", "corrected")}
    captures = MeasuredCaptures()
    prepare_captures(shared, captures)
    assessed = {role: assess_cohort(cohorts[role], shared[role], cases, captures) for role in shared}
    if not candidate_comparable and "comparison" in entries:
        with captures.discard_claims():
            prepare_captures({"comparison": prepared["comparison"]}, captures)
            assessed["comparison"] = assess_cohort(cohorts["comparison"], prepared["comparison"], cases, captures)
    correction = obj(report.get("correction", {}), "correction")
    need(bool(correction) or "corrected" not in entries, "corrected evidence requires an attributed correction")
    eligible, supporting, corrected_reviewed = correction_result(correction, cohorts, assessed, prepared)
    if "comparison" in entries:
        need(eligible, "comparison requires a persistent corrected-build mandatory failure")
        if assessed["comparison"].readiness.status == "pass":
            comparison = cohorts["comparison"]
            need(matching_conditions(cohorts["baseline"], comparison, allow_shell_change=True)
                 and matching_conditions(cohorts["corrected"], comparison, allow_shell_change=True),
                 "comparison must reuse the fixture and physical conditions on a different signed shell build")

    role = "corrected" if corrected_reviewed else "baseline"
    readiness, rows = assessed[role].readiness, assessed[role].rows
    if readiness.status != "pass":
        status = "inconclusive" if cohorts[role].get("attempts") else "setup-hold"
        disposition = "inconclusive"
    elif prepared[role].artifacts.payload_sha256 is not None and all(row.passed for row in rows.values()):
        status, disposition = "pass", "continue-tauri-evaluation"
        supporting = [attempt_id for row in rows.values() for attempt_id in row.support]
    elif eligible:
        status, disposition = "failing", "comparison-eligible"
        if candidate_comparable and comparison_recipes_match(assessed):
            comparison_rows = assessed["comparison"].rows
            if all(row.passed for row in comparison_rows.values()):
                disposition = "review-comparison"
                supporting.extend(attempt_id for row in comparison_rows.values() for attempt_id in row.support)
    else:
        failures = [attempt_id for row in rows.values() for attempt_id, (result, _) in row.open_attempts.items()
                    if result.status == "failing"]
        status, disposition = ("failing", "mandatory-failure") if failures else ("inconclusive", "inconclusive")
        supporting = list(dict.fromkeys([*supporting, *failures]))
    need(report.get("status") == status, f"report status must be {status}")
    reasons = items(report.get("reasons"), "report reasons")
    need(all(string(reason) for reason in reasons) and (status == "pass" or reasons),
         "record specific reasons for a non-pass")
    decision = obj(report.get("decision"), "shell disposition")
    need(decision.get("disposition") == disposition, f"shell disposition must be {disposition}")
    need(decision.get("production_shell") == "open" and decision.get("minimum_ios") == "open",
         "production mobile shell and minimum iOS stay open")
    if disposition == "review-comparison":
        need(named(decision, "rationale"), "record the comparison result and its layer attribution")
    references = items(decision.get("supporting_attempt_ids"), "disposition attempt references")
    need(all(string(reference) for reference in references) and Counter(references) == Counter(supporting),
         "disposition must reference the attempts that establish it")
    pending_cases = ", ".join(case_id for case_id, row in rows.items() if not row.passed)
    if readiness.detail:
        return Result(status, readiness.detail)
    if prepared[role].artifacts.detail:
        return Result(status, prepared[role].artifacts.detail)
    return Result(status, f"Open cases: {pending_cases}" if pending_cases else "")


def unique_keys(pairs):
    result = {}
    for key, value in pairs:
        need(key not in result, f"duplicate JSON key: {key}")
        result[key] = value
    return result


def reject_constant(value):
    raise EvidenceError(f"invalid JSON number: {value}")


def parse_json(content):
    return json.loads(content, object_pairs_hook=unique_keys, parse_constant=reject_constant)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    modes = parser.add_mutually_exclusive_group(required=True)
    modes.add_argument("--check-report", action="store_true", help="validate a truthful pass, failure or hold")
    modes.add_argument("--require-pass", action="store_true", help="also require complete signed physical acceptance")
    args = parser.parse_args(argv)
    try:
        cases = load_manifest(args.manifest)
        report = parse_json(args.report.read_bytes())
        result = validate(report, cases)
    except (OSError, ValueError) as error:
        print(f"W2 report invalid: {error}", file=sys.stderr)
        return 2
    print(f"W2 report valid: {result.status}" + (f" ({result.detail})" if result.detail else ""))
    if result.status != "pass":
        for reason in report["reasons"]:
            print(f"- {reason}")
    return int(args.require_pass and result.status != "pass")


if __name__ == "__main__":
    sys.exit(main())
