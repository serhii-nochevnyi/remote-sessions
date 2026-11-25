"""Validate the redacted physical iPhone evidence against the shipped W2 vectors."""

import argparse
from collections import Counter
from dataclasses import dataclass, field
import hashlib
import json
import math
from pathlib import Path
import re
import sys


MANIFEST_SHA256 = "8c08369864c9dd4ee6de19c8cf86171b179e928f276eebe2f9f89389ea353056"
COMMANDS = ("w2_begin_attempt", "w2_append_input", "w2_read_attempt")
GRANTS = {"allow-" + command.replace("_", "-"): [command] for command in COMMANDS}
CAPABILITY = {
    "identifier": "w2-fixture", "platforms": ["iOS"], "local": True,
    "windows": ["w2-fixture"], "permissions": list(GRANTS),
}
LOCKS = ("package-lock.json", "Cargo.lock", "apps/client/w2-fixture/src-tauri/Cargo.lock")
TOOLS = ("node", "npm", "vite", "tauri_cli", "tauri", "wry", "xterm", "cargo", "rustc")
OBSERVATIONS = {
    "english-return": ("cursor_visible", "return_pressed_once"),
    "ukrainian-return": ("cursor_visible", "ukrainian_key_pressed_once", "return_pressed_once"),
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
RECOVERY = {"app-switch", "lock-foreground"}
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


@dataclass(frozen=True)
class Result:
    status: str
    detail: str = ""


def need(condition, detail):
    if not condition:
        raise EvidenceError(detail)


def measured(condition, detail):
    if not condition:
        raise MeasuredFailure(detail)


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
    need((data["shell"] == "tauri") == (role != "comparison"), "cohort shell does not match its role")
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
    if role != "comparison":
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
    need(named(entry, "build_log_ref", "archive_ref", "symbol") and digest(entry.get("build_log_sha256"))
         and entry.get("source_revision") == data["source_revision"]
         and entry.get("inspected_bundle_sha256") == data["bundle_sha256"]
         and entry.get("installed_bundle_sha256") == install["installed_bundle_sha256"]
         and entry.get("verified_before_install") is True and entry.get("launcher_link_verified") is True
         and entry.get("defined_global_symbol") is True and type(entry.get("cli_exit_status")) is int
         and entry["cli_exit_status"] == 0, "missing or unbound pre-install generated entry validation")
    if role != "comparison":
        need(entry["symbol"] == "_start_app" and entry.get("cli_start_app_validation") is True,
             "locked CLI and linked archive must both verify the generated start_app entry")
    return {key: entry[key] for key in ("build_log_ref", "build_log_sha256", "source_revision")} | {
        "bundle_sha256": data["bundle_sha256"],
    }


def check_boundary(boundary, binding):
    need(boundary.get("binding") == binding and named(boundary, "ref"), "effective boundary lacks signed build binding")
    need(all(digest(boundary.get(key)) for key in ("config_sha256", "capabilities_sha256", "acl_sha256")),
         "effective config and generated ACL digests are required")
    need(boundary.get("capability") == CAPABILITY and boundary.get("capability", {}).get("local") is True
         and boundary.get("file_capabilities") == {}
         and boundary.get("resolved_grants") == GRANTS
         and boundary.get("handlers") == list(COMMANDS)
         and boundary.get("window_labels") == ["w2-fixture"], "effective iOS capability or command boundary differs")
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
    need(3 <= len(ops) <= 5 and ops == [COMMANDS[0], *([COMMANDS[1]] * (len(ops) - 2)), COMMANDS[2]],
         "offline run must actually begin, append and read")
    desktop = obj(boundary.get("desktop"), "same-worktree Mac regression")
    need(desktop.get("binding") == binding and named(desktop, "ref")
         and desktop.get("inventory") in ("empty", "synthetic")
         and all(desktop.get(key) is True for key in ("scaffold_status", "local_inventory", "fixture_commands_absent")),
         "safe desktop invokes and registry separation must be verified on the signed worktree")


def cohort_readiness(cohort, role):
    try:
        data = obj(cohort.get("tuple"), "measured tuple")
        check_tuple(data, role)
        need(cohort.get("fingerprint") == fingerprint(data), "cohort fingerprint does not match the measured tuple")
        check_pre_xcode(obj(cohort.get("pre_xcode"), "pre-Xcode gate"), data["source_revision"])
        check_ci(obj(cohort.get("ci"), "repository CI"), data["source_revision"])
        binding = check_build(obj(cohort.get("build"), "signed build provenance"), data, role)
        check_boundary(obj(cohort.get("boundary"), "effective boundary"), binding)
    except EvidenceError as error:
        return Result("inconclusive", str(error))
    return Result("pass")


def qualify(recipe, case, data):
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
    need(recipe.get("id") == fingerprint(frozen), "recipe ID does not match its frozen procedure")
    rehearsals = items(recipe.get("rehearsals"), "recipe rehearsals")
    seen = set()
    for rehearsal in rehearsals:
        rehearsal = obj(rehearsal, "rehearsal")
        need(named(rehearsal, "id", "ref") and rehearsal["id"] not in seen
             and rehearsal.get("recipe_id") == recipe["id"]
             and rehearsal.get("preedit") == preedit and rehearsal.get("end_state") == recipe["end_state"],
             "rehearsal has missing, duplicate or different visible outcomes")
        seen.add(rehearsal["id"])
    need(len(seen) >= 2, "two distinct observable rehearsals are required")
    return seen


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
    need(last_sample["bytes"] * 2 == len(checkpoint["hex"])
         and number(checkpoint.get("nextSequence"), 1)
         and last_sample["nativeSequence"] == checkpoint.get("nextSequence"),
         "trace checkpoint counters do not match the native checkpoint")
    need(not events["append delivery unconfirmed"], "native append delivery is uncertain")
    need(all(event["bytes"] > 0 for name in ("admission", "rejection", "append dispatch", "append acknowledgment")
             for event in events[name]), "input or append evidence must identify nonempty data")

    # Preserve accepted pre-end input as a failure even when the last sample was
    # empty and the eventual final vector is exactly the intended one.
    admissions = events["admission"]
    dispatches = events["append dispatch"]
    acks = events["append acknowledgment"]
    need(len(admissions) == len(dispatches) == len(acks), "incomplete admission, dispatch or acknowledgment sequence")
    previous_ack = start
    for index, (admit, dispatch, ack) in enumerate(zip(admissions, dispatches, acks), 1):
        need(previous_ack < admit["sequence"] < dispatch["sequence"] < ack["sequence"] < trace_length
             and dispatch["nativeSequence"] == ack["nativeSequence"] == index
             and admit["bytes"] == dispatch["bytes"] == ack["bytes"], "append order, byte counts or native sequence is inconsistent")
        previous_ack = ack["sequence"]
    lengths = [event["bytes"] for event in acks]
    need(named(final, "ref") and final["ref"] != checkpoint["ref"]
         and final_event["bytes"] * 2 == len(final["hex"]) and number(final.get("nextSequence"), 1)
         and final_event["nativeSequence"] == final["nextSequence"] == len(acks) + 1
         and final["acceptedEvents"] == len(acks) and sum(lengths) == final_event["bytes"]
         and final.get("eventLengths", lengths) == lengths,
         "final native snapshot does not reconcile with acknowledged appends")
    for sample in samples:
        accepted = [ack for ack in acks if ack["sequence"] < sample["sequence"]]
        need(sample["bytes"] == sum(ack["bytes"] for ack in accepted)
             and sample["nativeSequence"] == len(accepted) + 1, "checkpoint counters do not reconcile with the append trace")
    need(checkpoint["acceptedEvents"] == checkpoint["nextSequence"] - 1,
         "checkpoint event count does not reconcile with native sequence")
    measured(not any(ack["sequence"] < end for ack in acks), "native input accepted before compositionend")
    need(all(admit["sequence"] > end for admit in admissions)
         and all(event["sequence"] > end for event in events["rejection"]),
         "premature onData or uncertain pre-end native acceptance")
    need(all(sample["bytes"] == 0 and sample["nativeSequence"] == 1 for sample in samples)
         and not checkpoint["hex"] and checkpoint["acceptedEvents"] == 0,
         "active preedit checkpoint must contain zero native input")
    measured(len(acks) <= int(bool(expected)), "duplicate or unexpected composition append")
    measured(final["hex"] == expected and len(acks) == int(bool(expected)), "composition final bytes differ from the manifest")
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


def recovery(attempt, identity, expected):
    old = obj(attempt.get("recovery"), "foreground recovery")
    initial = obj(old.get("before"), "pre-interruption native snapshot")
    old_id = attempt_identity(initial)
    need(old_id[0] == identity[0] and number(old_id[1], 1) and number(old_id[2], 1)
         and old_id[1] < identity[1] and old_id[2] < identity[2], "foreground restart did not identify a fresh attempt and generation")
    before = snapshot(initial, old_id)
    retained = snapshot(old.get("retained"), old_id)
    need(old.get("delivery_uncertain") is False and type(old.get("old_generation_extra_event")) is bool,
         "retained snapshot or delivery is uncertain")
    return (before["hex"] == retained["hex"] == expected
            and before["acceptedEvents"] == retained["acceptedEvents"]
            and not old["old_generation_extra_event"])


def evaluate_attempt(attempt, case, cohort, qualification):
    case_id = case["id"]
    try:
        need(attempt.get("cohort_fingerprint") == cohort["fingerprint"], "attempt belongs to a different cohort tuple")
        need(named(attempt, "launch_id", "ref") and number(attempt.get("attemptId"), 1)
             and number(attempt.get("generation"), 1), "missing native attempt identity or observation reference")
        language = attempt.get("keyboard_language")
        need(language == cohort["tuple"]["keyboard"]["scenario_languages"][case_id]
             and attempt.get("orientation") == cohort["tuple"]["orientation"], "mixed keyboard or orientation conditions")
        if case_id in PASTE:
            need(attempt.get("route") in ("sample", "native"), "record sample or native paste route")
            order = [1] if attempt["route"] == "sample" else [2]
        else:
            order = list(range(1, len(case["actions"]) + 1))
        need(attempt.get("action_order") == order and all(type(i) is int for i in attempt["action_order"]),
             "missing or unordered prescribed actions")
        observed = obj(attempt.get("observations"), "visible observations")
        need(all(type(observed.get(key)) is bool for key in OBSERVATIONS[case_id]),
             "missing or ambiguous required visible observation")
        viewport_clear = check_layout(attempt, case_id) if case_id in LAYOUT_CASES else True
        identity = (case_id, attempt["attemptId"], attempt["generation"])
        final = snapshot(attempt.get("final"), identity)
        need(type(attempt.get("start_empty")) is bool and type(attempt.get("replayed")) is bool
             and type(attempt.get("capture_error")) is bool, "missing start, replay or capture-error observation")
        if case_id in COMPOSITION:
            need(not qualification.detail, qualification.detail)
            recipe = cohort["recipes"][case_id]
            need(attempt.get("recipe_id") == recipe["id"] and language == recipe["keyboard_language"],
                 "measured gesture does not use the frozen qualified recipe")
            need(observed["preedit_visible"], "checkpoint was not observed during visible preedit")
            need(attempt["id"] not in qualification.rehearsals, "qualification rehearsal is not a fresh measured attempt")
            check_composition(attempt, final, identity, case["expected"]["afterHex"])
        if case_id in RECOVERY:
            measured(recovery(attempt, identity, case["expected"]["beforeHex"]), "retained native bytes or events changed")
        measured(attempt["start_empty"], "unexpected startup input")
        measured(not attempt["replayed"], "replayed input")
        measured(final["hex"] == case["expected"]["afterHex"], "final bytes differ from the manifest")
        need(not attempt["capture_error"], "native capture reported an unresolved error")
        for key in OBSERVATIONS[case_id]:
            measured(observed[key], f"visible observation failed: {key}")
        measured(viewport_clear, "prompt or cursor obscured in a measured viewport")
    except MeasuredFailure as error:
        return Result("failing", str(error))
    except EvidenceError as error:
        return Result("inconclusive", str(error))
    return Result("pass")


@dataclass
class Qualification:
    rehearsals: set
    detail: str = ""


def qualifications(cohort, cases):
    recipes = obj(cohort.get("recipes", {}), "composition recipes")
    result = {}
    all_rehearsals = set()
    for case_id in COMPOSITION:
        try:
            recipe = obj(recipes.get(case_id), f"{case_id} recipe")
            rehearsals = qualify(recipe, cases[case_id], cohort["tuple"])
            need(not (rehearsals & all_rehearsals), "rehearsal ID is reused across recipes")
            all_rehearsals.update(rehearsals)
            result[case_id] = Qualification(rehearsals)
        except EvidenceError as error:
            result[case_id] = Qualification(set(), str(error))
    return result


@dataclass
class CaseRecord:
    required: tuple
    clean: dict = field(default_factory=dict)
    open_attempts: dict = field(default_factory=dict)

    def add(self, attempt, result):
        self.open_attempts[attempt["id"]] = (attempt, result)
        if result.status != "pass":
            self.clean.clear()
            return
        route = attempt.get("route") if self.required == ("sample", "native") else None
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
        records = [self.open_attempts[attempt_id] for attempt_id in ids]
        attempt, result = records[0]
        if result.status != "failing":
            return None
        conditions = tuple(attempt.get(key) for key in ("route", "keyboard_language", "recipe_id"))
        same_conditions = [value for run, value in self.open_attempts.values()
                           if tuple(run.get(key) for key in ("route", "keyboard_language", "recipe_id")) == conditions]
        if (same_conditions[-1] == result
                and all(value == result and tuple(run.get(key) for key in ("route", "keyboard_language", "recipe_id")) == conditions
                        for run, value in records)):
            return result, conditions
        return None


def native_keys(attempt):
    """The fixture counters can reset only on a separately identified app launch."""
    pairs = [(attempt.get("attemptId"), attempt.get("generation"))]
    if isinstance(attempt.get("recovery"), dict) and isinstance(attempt["recovery"].get("before"), dict):
        _, native_id, generation = attempt_identity(attempt["recovery"]["before"])
        pairs.append((native_id, generation))
    for native_id, generation in pairs:
        if number(native_id, 1) and number(generation, 1) and named(attempt, "launch_id"):
            yield attempt["launch_id"], native_id, generation


def assess_cohort(cohort, role, cases, ids):
    readiness = cohort_readiness(cohort, role)
    recipes = qualifications(cohort, cases) if readiness.status == "pass" else {}
    rows = {case_id: CaseRecord(("sample", "native") if case_id in PASTE else (None,))
            for case_id in cases}
    attempts = [obj(value, "attempt") for value in items(cohort.get("attempts", []), "cohort attempts")]
    rehearsal_ids = {key for recipe in recipes.values() for key in recipe.rehearsals}
    # Validate report IDs before native counters so a copied row cannot mask a duplicate report ID.
    for attempt in attempts:
        need(named(attempt, "id") and attempt["id"] not in ids and attempt["id"] not in rehearsal_ids,
             "duplicate measured attempt ID or reused rehearsal")
        ids.add(attempt["id"])
        case_id = attempt.get("case_id")
        need(string(case_id) and case_id in rows, "unknown or missing mandatory case ID")

    keys = [key for attempt in attempts for key in native_keys(attempt)]
    attempt_ids = Counter((launch, attempt_id) for launch, attempt_id, _ in keys)
    generations = Counter((launch, generation) for launch, _, generation in keys)
    for attempt in attempts:
        case_id = attempt["case_id"]
        if readiness.status != "pass":
            result = readiness
        elif any(attempt_ids[launch, native_id] > 1 or generations[launch, generation] > 1
                 for launch, native_id, generation in native_keys(attempt)):
            result = Result("inconclusive", "native attempt or generation is reused within an app launch")
        else:
            result = evaluate_attempt(attempt, cases[case_id], cohort, recipes.get(case_id))
        need(attempt.get("status") == result.status,
             f"{attempt['id']}: expected {result.status}" + (f" ({result.detail})" if result.detail else ""))
        if result.status != "pass":
            need(named(attempt, "reason"), f"{attempt['id']}: record the reason for the non-pass")
        rows[case_id].add(attempt, result)
    return readiness, rows


def matching_conditions(original, updated):
    before, after = original["tuple"], updated["tuple"]
    return all(before[key] == after[key] for key in (
        "device_alias", "device_model", "ios_version", "ios_build", "keyboard", "orientation", "fixture_sha256",
    )) and before["bundle_sha256"] != after["bundle_sha256"]


def matching_recipes(original, updated):
    recipes = obj(original.get("recipes", {}), "original composition recipes")
    for case_id, recipe in obj(updated.get("recipes", {}), "updated composition recipes").items():
        if case_id in recipes:
            need(isinstance(recipe, dict) and isinstance(recipes[case_id], dict)
                 and recipe.get("id") == recipes[case_id].get("id"), "another build changed a frozen composition recipe")


def correction_result(correction, cohorts, assessed):
    if not correction:
        return False, []
    need(named(correction, "case_id", "layer", "change") and correction.get("case_id") in OBSERVATIONS,
         "record one layer-specific correction and its mandatory case")
    scope = items(correction.get("scope"), "correction scope")
    need(scope and all(string(path) for path in scope), "record the exact correction scope")
    case = correction["case_id"]
    repeats = items(correction.get("baseline_attempt_ids"), "baseline failure attempts")
    need(len(repeats) == 2 and all(string(value) for value in repeats) and repeats[0] != repeats[1],
         "name two distinct baseline failure attempts")
    original = assessed["baseline"][1][case]
    reproduced = original.reproduction(repeats)
    need(reproduced is not None, "correction requires the same unresolved measured failure twice on the baseline")
    if assessed["corrected"][0].status != "pass":
        return False, repeats
    need(matching_conditions(cohorts["baseline"], cohorts["corrected"])
         and cohorts["baseline"]["tuple"]["source_revision"] != cohorts["corrected"]["tuple"]["source_revision"],
         "correction requires a different signed revision and bundle under the same physical conditions")
    matching_recipes(cohorts["baseline"], cohorts["corrected"])
    corrected = assessed["corrected"][1][case]
    for attempt_id in reversed(corrected.open_attempts):
        if corrected.reproduction([attempt_id]) == reproduced:
            return True, [*repeats, attempt_id]
    return False, repeats


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
    if "execution" in report:
        check_execution(obj(report["execution"], "checkout observations"))
    entries = obj(report.get("cohorts"), "cohorts")
    need(set(entries) <= set(COHORTS), "only a baseline, one correction and one comparison cohort are allowed")
    cohorts = {role: obj(entries.get(role, {}), role) for role in COHORTS}
    ids = set()
    assessed = {role: assess_cohort(cohort, role, cases, ids) for role, cohort in cohorts.items()}
    correction = obj(report.get("correction", {}), "correction")
    need(bool(correction) or "corrected" not in entries, "corrected evidence requires an attributed correction")
    eligible, supporting = correction_result(correction, cohorts, assessed)
    if "comparison" in entries:
        need(eligible, "comparison requires a persistent corrected-build mandatory failure")
        if assessed["comparison"][0].status == "pass":
            comparison = cohorts["comparison"]
            need(matching_conditions(cohorts["baseline"], comparison)
                 and matching_conditions(cohorts["corrected"], comparison),
                 "comparison must reuse the fixture and physical conditions on a different signed shell build")
            matching_recipes(cohorts["baseline"], comparison)
            matching_recipes(cohorts["corrected"], comparison)

    role = "corrected" if correction and assessed["corrected"][0].status == "pass" else "baseline"
    readiness, rows = assessed[role]
    if readiness.status != "pass":
        status = "inconclusive" if cohorts[role].get("attempts") else "setup-hold"
        disposition = "inconclusive"
    elif all(row.passed for row in rows.values()):
        status, disposition = "pass", "continue-tauri-evaluation"
        supporting = [attempt_id for row in rows.values() for attempt_id in row.support]
    elif eligible:
        status, disposition = "failing", "comparison-eligible"
        comparison_rows = assessed["comparison"][1]
        if all(row.passed for row in comparison_rows.values()):
            disposition = "review-comparison"
            supporting.extend(attempt_id for row in comparison_rows.values() for attempt_id in row.support)
    else:
        failures = [attempt_id for row in rows.values() for attempt_id, (_, result) in row.open_attempts.items()
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
