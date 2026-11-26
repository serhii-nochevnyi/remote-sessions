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


MANIFEST_SHA256 = "3825bfd16e9380d222b4a07b24966758cc090ed93bbc4ab6951e18a6cac5580c"
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

    def __init__(self, detail, observed):
        super().__init__(detail)
        self.observed = observed


@dataclass(frozen=True)
class Result:
    status: str
    detail: str = ""
    observed: tuple = ()


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


def qualify(recipe, case, data, cohort_fingerprint):
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
        need(rehearsal.get("cohort_fingerprint") == cohort_fingerprint,
             "recipe rehearsal belongs to a different signed cohort")
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


def check_recovery(attempt, identity, expected, started_fresh, recovery_prefix):
    old = obj(attempt.get("recovery"), "foreground recovery")
    initial = obj(old.get("before"), "pre-interruption native snapshot")
    old_id = attempt_identity(initial)
    need(old_id[0] == identity[0] and number(old_id[1], 1) and number(old_id[2], 1),
         "foreground restart lacks the old native attempt and generation")
    before = snapshot(initial, old_id)
    retained = snapshot(old.get("retained"), old_id)
    need(old.get("delivery_uncertain") is False and type(old.get("old_generation_extra_event")) is bool,
         "retained snapshot or delivery is uncertain")
    if not recovery_prefix:
        extra_event = old["old_generation_extra_event"]
        before_hex, retained_hex = before["hex"], retained["hex"]
        if not same_native_input(before, retained) or extra_event:
            before_count, retained_count = before["acceptedEvents"], retained["acceptedEvents"]
            observed_input = (before_hex, retained_hex, before_count, retained_count, extra_event)
            if before_hex == retained_hex and before_count == retained_count and not extra_event:
                observed_input += (tuple(before["eventLengths"]), tuple(retained["eventLengths"]))
            raise MeasuredFailure("retained native bytes or events changed", observed_input)
        # Action 1 and the bound old snapshots establish the read. The capture flag confirms delivery.
        if attempt.get("capture_error") is False:
            measured(before_hex == expected, "initial bytes differ from the manifest", before_hex)
    restart = obj(old.get("restart"), "foreground restart observation")
    previous = obj(restart.get("previous"), "restart's old attempt identity")
    need(named(restart, "ref") and attempt_identity(previous) == old_id
         and number(previous.get("attemptId"), 1) and number(previous.get("generation"), 1),
         "foreground restart observation is not bound to the old attempt")
    outcome = restart.get("outcome")
    need(outcome in ("fresh", "same-generation", "no-new-generation"), "foreground restart outcome is uncertain")
    need(restart.get("action_completed") is True, "missing completed foreground restart action")
    expected_fresh = outcome == "fresh"
    need(started_fresh is expected_fresh or (started_fresh is None and not recovery_prefix),
         "foreground restart conflicts with the visible fresh-start observation")
    if outcome == "no-new-generation":
        failure = obj(restart.get("begin_failure"), "recorded begin failure")
        need(identity == old_id and attempt_identity(failure) == old_id and named(failure, "ref")
             and number(failure.get("attemptId"), 1) and number(failure.get("generation"), 1)
             and "observed" not in restart and "final" not in attempt,
             "begin failure belongs to a different attempt or conflicts with a new generation")
        raise MeasuredFailure("foreground restart produced no new generation", ())
    if outcome == "same-generation":
        observed = snapshot(restart.get("observed"), old_id)
        need(identity == old_id and "final" not in attempt and same_native_input(observed, retained)
             and "begin_failure" not in restart, "unchanged generation does not match the retained native snapshot")
        raise MeasuredFailure("foreground restart kept the same generation", ())

    observed = obj(restart.get("observed"), "restart's fresh attempt identity")
    need(attempt_identity(observed) == identity and number(observed.get("attemptId"), 1)
         and number(observed.get("generation"), 1)
         and old_id[1] < identity[1] and old_id[2] < identity[2] and "begin_failure" not in restart,
         "foreground restart did not identify a fresh attempt and generation")
    need(not recovery_prefix, "fresh foreground restart requires all prescribed actions")


def check_capture(attempt, case, cohort, qualification, observed, identity, recovery_prefix):
    case_id = case["id"]
    expected = case["expected"]["afterHex"]
    if case_id in RECOVERY:
        transition = OBSERVATIONS[case_id][0]
        need(observed.get(transition) is True, "foreground transition was not established")
        check_recovery(attempt, identity, case["expected"]["beforeHex"], observed.get("started_fresh"), recovery_prefix)
    final = None
    if case_id in COMPOSITION:
        need(not qualification.detail, qualification.detail)
        recipe = cohort["recipes"][case_id]
        need(attempt.get("recipe_id") == recipe["id"] and attempt["keyboard_language"] == recipe["keyboard_language"],
             "measured gesture does not use the frozen qualified recipe")
        need(all(observed.get(key) is True for key in OBSERVATIONS[case_id]),
             "measured preedit and end state do not establish the qualified gesture")
        need(attempt["id"] not in qualification.rehearsals, "qualification rehearsal is not a fresh measured attempt")
        final = snapshot(attempt.get("final"), identity)
        check_composition(attempt, final, identity, expected)
    need(attempt.get("capture_error") is False, "native capture reported an unresolved error")
    return final if final is not None else snapshot(attempt.get("final"), identity)


def evaluate_attempt(attempt, case, cohort, qualification):
    case_id = case["id"]
    try:
        need(attempt.get("cohort_fingerprint") == cohort["fingerprint"], "attempt belongs to a different cohort tuple")
        need(named(attempt, "ref"), "missing attempt observation reference")
        language = attempt.get("keyboard_language")
        need(language == cohort["tuple"]["keyboard"]["scenario_languages"][case_id]
             and attempt.get("orientation") == cohort["tuple"]["orientation"], "mixed keyboard or orientation conditions")
        if case_id in PASTE:
            need(attempt.get("route") in ("sample", "native"), "record sample or native paste route")
            order = [1] if attempt["route"] == "sample" else [2]
        else:
            order = list(range(1, len(case["actions"]) + 1))
        actions = attempt.get("action_order")
        recovery_prefix = case_id in RECOVERY and actions == order[:-1]
        need((actions == order or recovery_prefix) and all(type(i) is int for i in actions),
             "missing or unordered prescribed actions")
        observed = obj(attempt.get("observations"), "visible observations")
        identity = (case_id, attempt["attemptId"], attempt["generation"])
        final, capture_issue = None, None
        try:
            final = check_capture(attempt, case, cohort, qualification, observed, identity, recovery_prefix)
        except MeasuredFailure:
            raise
        except EvidenceError as error:
            # The shorter order is eligible only through a corroborated failed restart.
            if recovery_prefix:
                raise
            capture_issue = error
        measured(attempt.get("start_empty") is not False, "unexpected startup input")
        measured(attempt.get("replayed") is not True, "replayed input")
        if final is not None:
            measured(final["hex"] == case["expected"]["afterHex"],
                     "final bytes differ from the manifest", final["hex"])
        for key in OBSERVATIONS[case_id]:
            if case_id in COMPOSITION or key == "started_fresh":
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
        return Result("failing", str(error), error.observed)
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
            rehearsals = qualify(recipe, cases[case_id], cohort["tuple"], cohort["fingerprint"])
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
            measurements[record["attempt_id"]] = (order, position, launch["id"], launch["ref"],
                                                   record["attemptId"], record["generation"])
            last_position = position
    return measurements


def measurement_positions(attempts, records):
    positions = {}
    for attempt in attempts:
        record = records.get(attempt["id"])
        position, native_id, generation = (attempt.get(key) for key in ("measurement_position", "attemptId", "generation"))
        if (record and all(number(value, 1) for value in (position, native_id, generation))
                and record[1:] == (position, attempt.get("launch_id"), attempt.get("launch_ref"), native_id, generation)):
            positions[attempt["id"]] = record[:2]
    return positions


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


def native_keys(attempt):
    """The fixture counters can reset only on a separately identified app launch."""
    pairs = []
    if (attempt["case_id"] in RECOVERY and isinstance(attempt.get("recovery"), dict)
            and isinstance(attempt["recovery"].get("before"), dict)):
        _, native_id, generation = attempt_identity(attempt["recovery"]["before"])
        pairs.append((native_id, generation))
    pairs.append((attempt.get("attemptId"), attempt.get("generation")))
    keys = [(attempt["launch_id"], native_id, generation) for native_id, generation in pairs
            if number(native_id, 1) and number(generation, 1)]
    return tuple(dict.fromkeys(keys))


def restart_reference(attempt):
    recovery = attempt.get("recovery")
    if attempt["case_id"] in RECOVERY and isinstance(recovery, dict):
        restart = recovery.get("restart")
        if isinstance(restart, dict) and named(restart, "ref"):
            return restart["ref"]
    return None


def out_of_order_attempts(attempts):
    latest = {}
    out_of_order = set()
    for attempt in attempts:
        for launch, native_id, generation in native_keys(attempt):
            last_id, last_generation = latest.get(launch, (0, 0))
            if native_id <= last_id or generation <= last_generation:
                out_of_order.add(attempt["id"])
            latest[launch] = (max(last_id, native_id), max(last_generation, generation))
    return out_of_order


def assess_cohort(cohort, role, cases, ids):
    readiness = cohort_readiness(cohort, role)
    recipes = qualifications(cohort, cases) if readiness.status == "pass" else {}
    rows = {case_id: CaseRecord(("sample", "native") if case_id in PASTE else (None,))
            for case_id in cases}
    attempts = [obj(value, "attempt") for value in items(cohort.get("attempts", []), "cohort attempts")]
    rehearsal_ids = {key for recipe in recipes.values() for key in recipe.rehearsals}
    for attempt in attempts:
        need(named(attempt, "id") and attempt["id"] not in ids and attempt["id"] not in rehearsal_ids,
             "duplicate measured attempt ID or reused rehearsal")
        ids.add(attempt["id"])
        case_id = attempt.get("case_id")
        need(string(case_id) and case_id in rows, "unknown or missing mandatory case ID")

    records = retained_measurements(cohort)
    need(records.keys() <= {attempt["id"] for attempt in attempts}, "retained measurement has no submitted attempt")
    bound_positions = measurement_positions(attempts, records)
    moved = moved_attempts(attempts, bound_positions)
    # Retained records place holds even if their submitted rows cannot be evaluated.
    ordered = sorted((attempt for attempt in attempts if attempt["id"] in records),
                     key=lambda attempt: records[attempt["id"]][:2])
    unpositioned = [attempt for attempt in attempts if attempt["id"] not in records]
    bound = [attempt for attempt in ordered if attempt["id"] in bound_positions]
    keys = [key for attempt in bound for key in native_keys(attempt)]
    attempt_ids = Counter((launch, attempt_id) for launch, attempt_id, _ in keys)
    generations = Counter((launch, generation) for launch, _, generation in keys)
    restarts = Counter(ref for attempt in bound if (ref := restart_reference(attempt)) is not None)
    out_of_order = out_of_order_attempts(bound)
    for attempt in [*ordered, *unpositioned]:
        attempt_id = attempt["id"]
        case_id = attempt["case_id"]
        if readiness.status != "pass":
            result = readiness
        elif attempt_id not in bound_positions:
            result = Result("inconclusive", "attempt does not match its retained launch and measurement")
        elif any(attempt_ids[launch, native_id] > 1 or generations[launch, generation] > 1
                 for launch, native_id, generation in native_keys(attempt)):
            result = Result("inconclusive", "native attempt or generation is reused within an app launch")
        elif attempt_id in out_of_order:
            result = Result("inconclusive", "native attempt or generation is out of order within an app launch")
        elif restarts[restart_reference(attempt)] > 1:
            result = Result("inconclusive", "foreground restart observation is reused")
        else:
            result = evaluate_attempt(attempt, cases[case_id], cohort, recipes.get(case_id))
            # Keep verified failures at their retained positions and disqualify moved passes.
            if result.status == "pass" and attempt_id in moved:
                result = Result("inconclusive",
                                "native attempt or generation is out of order in the submitted measurements")
        need(attempt.get("status") == result.status,
             f"{attempt['id']}: expected {result.status}" + (f" ({result.detail})" if result.detail else ""))
        if result.status != "pass":
            need(named(attempt, "reason"), f"{attempt['id']}: record the reason for the non-pass")
        rows[case_id].add(attempt, result)
    return readiness, rows


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


def matching_recipes(original, updated, require_original=False):
    recipes = obj(original.get("recipes", {}), "original composition recipes")
    for case_id, recipe in obj(updated.get("recipes", {}), "updated composition recipes").items():
        if require_original and case_id in COMPOSITION:
            need(case_id in recipes, "comparison requires the baseline's frozen composition recipes")
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
    for execution in items(report.get("previous_executions", []), "previous checkout observations"):
        check_execution(obj(execution, "previous checkout observation"))
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
            need(matching_conditions(cohorts["baseline"], comparison, allow_shell_change=True)
                 and matching_conditions(cohorts["corrected"], comparison, allow_shell_change=True),
                 "comparison must reuse the fixture and physical conditions on a different signed shell build")
            matching_recipes(cohorts["baseline"], comparison, require_original=True)
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
