"""Synthetic controls for the W2 evidence gate. Device observations live in docs."""

import copy
import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

import w2_iphone_evidence as evidence


MANIFEST = Path(__file__).resolve().parents[2] / "apps/client/w2-fixture/cases.json"


def bound_cohort(revision_digit="1", bundle_digit="2"):
    source, bundle = revision_digit * 40, bundle_digit * 64
    data = {
        "shell": "tauri", "destination": "physical-iphone", "device_alias": "synthetic-phone",
        "device_model": "synthetic iPhone", "ios_version": "18.5", "ios_build": "synthetic OS build",
        "orientation": "portrait", "deployment_target": "16.0",
        "keyboard": {
            "languages": ["en", "uk"],
            "scenario_languages": dict.fromkeys(evidence.OBSERVATIONS, "en") | {"ukrainian-return": "uk"},
            "settings": {"stock": True, "hardware_keyboard": False, "autocorrection": False, "predictive": False},
        },
        "xcode": {"version": "synthetic Xcode version", "build": "synthetic Xcode build"},
        "sdk": {"version": "18.5", "build": "synthetic SDK build", "root": "iPhoneOS.sdk"},
        "source_revision": source, "bundle_sha256": bundle, "fixture_sha256": evidence.MANIFEST_SHA256,
        "app": {"identifier": "dev.remotesessions.w2fixture", "version": "0.1.0", "build": "1"},
        "locks": dict.fromkeys(evidence.LOCKS, "a" * 64),
        "tools": dict.fromkeys(evidence.TOOLS, "synthetic version") | {"cargo": "cargo 1.97.1", "rustc": "rustc 1.97.1"},
    }
    binding = {"source_revision": source, "bundle_sha256": bundle, "build_log_ref": "synthetic build log",
               "build_log_sha256": "b" * 64}
    return {
        "tuple": data,
        "fingerprint": evidence.fingerprint(data),
        "pre_xcode": dict.fromkeys(evidence.PRE_XCODE, 0) | {"source_revision": source, "ref": "synthetic preflight"},
        "ci": {job: {"result": "pass", "source_revision": source, "ref": "synthetic CI"} for job in ("core", "native")},
        "build": {
            "installation": {"ref": "synthetic install", "signature_verified": True, "physical_install_verified": True,
                             "installed_bundle_sha256": bundle, "app": copy.deepcopy(data["app"])},
            "generated_settings": {"sdk": "iphoneos", "configuration": "Release", "sdkroot": data["sdk"]["root"],
                                   "deployment_target": "16.0", "minimum_os_version": "16.0.0", "ref": "synthetic settings"},
            "entry_validation": {
                "source_revision": source, "build_log_ref": binding["build_log_ref"],
                "build_log_sha256": binding["build_log_sha256"], "archive_ref": "synthetic archive",
                "symbol": "_start_app", "inspected_bundle_sha256": bundle, "installed_bundle_sha256": bundle,
                "verified_before_install": True, "launcher_link_verified": True, "defined_global_symbol": True,
                "cli_start_app_validation": True, "cli_exit_status": 0,
            },
        },
        "boundary": {
            "binding": binding.copy(), "ref": "synthetic effective ACL", "config_sha256": "c" * 64,
            "capabilities_sha256": "d" * 64, "acl_sha256": "e" * 64,
            "capability": copy.deepcopy(evidence.CAPABILITY), "file_capabilities": {},
            "resolved_grants": copy.deepcopy(evidence.GRANTS), "handlers": list(evidence.COMMANDS),
            "window_labels": ["w2-fixture"], "plist_reviewed": True, "entitlements_reviewed": True,
            "no_extra_network_or_url_access": True, "ios_dependency_tree_reviewed": True,
            "no_host_provider_or_desktop_dependencies": True,
            "offline_launch": {
                "ref": "synthetic offline launch", "bundle_sha256": bundle, "airplane_mode": True,
                "wifi_off": True, "cellular_off": True, "bundled_screen_visible": True,
                "startup_hex": "", "tracer_hex": "d1970d", "operations": list(evidence.COMMANDS),
            },
            "desktop": {"binding": binding.copy(), "ref": "synthetic Mac", "inventory": "empty",
                        "scaffold_status": True, "local_inventory": True, "fixture_commands_absent": True},
        },
        "recipes": {},
        "launches": [],
        "attempts": [],
    }


def native_cohort():
    cohort = bound_cohort("5", "6")
    data = cohort["tuple"]
    data.update(shell="native-wkwebview", app={"identifier": "dev.synthetic.comparison", "version": "0.1", "build": "1"})
    del data["locks"], data["tools"], cohort["pre_xcode"]
    cohort["fingerprint"] = evidence.fingerprint(data)
    build, boundary = cohort["build"], cohort["boundary"]
    build["installation"]["app"] = copy.deepcopy(data["app"])
    build["generated_settings"].update(source_revision=data["source_revision"], bundle_sha256=data["bundle_sha256"])
    entry = build["entry_validation"]
    build["entry_validation"] = {key: entry[key] for key in (
        "source_revision", "build_log_ref", "build_log_sha256", "inspected_bundle_sha256",
        "installed_bundle_sha256", "verified_before_install",
    )} | {
        "entry_ref": "synthetic native app entry", "entry_sha256": "a" * 64, "entry_verified": True,
        "registration_ref": "synthetic native registration", "registration_sha256": "d" * 64,
        "registration_verified": True,
    }
    for key in ("config_sha256", "capabilities_sha256", "acl_sha256", "capability", "file_capabilities",
                "resolved_grants", "handlers", "window_labels", "desktop"):
        del boundary[key]
    boundary.update(
        ref="synthetic effective native boundary", package_review_ref="synthetic signed package review",
        dependencies={"ref": "synthetic native dependencies", "sha256": "e" * 64},
        webview={"ref": "synthetic effective WebView configuration", "sha256": "c" * 64,
                 "bundled_origin": "fixture://local", "allowed_origins": ["fixture://local"],
                 "bundled_assets_only": True, "main_frame_only": True},
        registration={"ref": "synthetic native registration", "sha256": "d" * 64,
                      "handlers": {"beginCase": "begin", "submitInput": "append", "readCase": "read"}},
    )
    boundary["binding"].update(registration_ref=boundary["registration"]["ref"],
                               registration_sha256=boundary["registration"]["sha256"])
    boundary["offline_launch"]["operations"] = ["beginCase", "submitInput", "readCase"]
    return cohort


def make_recipes(cohort, cases):
    for case_id, (preedit, end) in evidence.COMPOSITION.items():
        recipe = {
            "case_id": case_id, "expected_hex": cases[case_id]["expected"]["afterHex"],
            "keyboard_language": "en", "keyboard": copy.deepcopy(cohort["tuple"]["keyboard"]),
            "steps": ["Synthetic preedit step", "Synthetic end step"], "preedit": preedit.copy(),
            "end_action": end, "end_state": "Synthetic visible end state",
        }
        recipe["id"] = evidence.fingerprint(recipe)
        recipe["rehearsals"] = [
            {"id": f"{cohort['tuple']['source_revision']}-{case_id}-rehearsal-{i}", "ref": "synthetic rehearsal",
             "cohort_fingerprint": cohort["fingerprint"], "recipe_id": recipe["id"],
             "preedit": preedit.copy(), "end_state": recipe["end_state"]}
            for i in range(2)
        ]
        cohort["recipes"][case_id] = recipe


def events(*records):
    return [dict(record, sequence=index, elapsedMs=index * 10) for index, record in enumerate(records, 1)]


def event(name, byte_count=None, native_sequence=None):
    record = {"event": name}
    if byte_count is not None:
        record["bytes"] = byte_count
    if native_sequence is not None:
        record["nativeSequence"] = native_sequence
    return record


def native(identity, vector):
    return dict(identity, hex=vector, acceptedEvents=int(bool(vector)))


def composition(attempt, identity, vector):
    size = len(vector) // 2
    rows = [event("start"), event("update"), event("active checkpoint", 0, 1)]
    if attempt["case_id"] == "composition-replacement":
        rows += [event("update"), event("active checkpoint", 0, 1)]
    checkpoint_sequence = len(rows)
    rows.append(event("end"))
    if vector:
        rows.extend([event("admission", size), event("append dispatch", size, 1),
                     event("append acknowledgment", size, 1)])
    next_seq = 2 if vector else 1
    rows.append(event("final snapshot", size, next_seq))
    attempt["trace"] = dict(identity, complete=True, events=events(*rows))
    attempt["checkpoint"] = native(identity, "") | {
        "sequence": checkpoint_sequence, "active": True, "focused": True, "nextSequence": 1, "ref": "synthetic preedit",
    }
    attempt["final"].update(ref="synthetic post-end read", nextSequence=next_seq)


def start_launch(cohort):
    order = len(cohort["launches"]) + 1
    launch = {"id": f"synthetic launch {order}", "order": order,
              "ref": f"synthetic retained launch capture {order}", "measurements": []}
    cohort["launches"].append(launch)
    return launch


def retain_measurement(cohort, attempt):
    launch = cohort["launches"][-1] if cohort["launches"] else start_launch(cohort)
    position = launch["measurements"][-1]["position"] + 1 if launch["measurements"] else 1
    attempt.update(launch_id=launch["id"], launch_ref=launch["ref"], measurement_position=position)
    launch["measurements"].append({"attempt_id": attempt["id"], "position": position,
                                   "attemptId": attempt["attemptId"], "generation": attempt["generation"]})


def remove_attempts(cohort, *removed):
    ids = {row["id"] for row in removed}
    cohort["attempts"][:] = [row for row in cohort["attempts"] if row["id"] not in ids]
    for launch in cohort["launches"]:
        launch["measurements"][:] = [row for row in launch["measurements"] if row["attempt_id"] not in ids]


def add_attempt(cohort, case, suffix, route=None):
    case_id = case["id"]
    launch = cohort["launches"][-1] if cohort["launches"] else start_launch(cohort)
    index = launch["measurements"][-1]["generation"] + 2 if launch["measurements"] else 2
    identity = {"caseId": case_id, "attemptId": index, "generation": index}
    attempt = {
        "id": f"{cohort['tuple']['source_revision'][0]}-{case_id}-{suffix}", "case_id": case_id,
        "cohort_fingerprint": cohort["fingerprint"], "attemptId": index, "generation": index,
        "ref": "synthetic attempt", "status": "pass",
        "keyboard_language": "uk" if case_id == "ukrainian-return" else "en", "orientation": "portrait",
        "action_order": list(range(1, len(case["actions"]) + 1)), "start_empty": True,
        "replayed": False, "capture_error": False,
        "observations": dict.fromkeys(evidence.OBSERVATIONS[case_id], True),
        "final": native(identity, case["expected"]["afterHex"]),
    }
    if case_id in evidence.PASTE:
        attempt.update(route=route, action_order=[1 if route == "sample" else 2])
    if case_id in evidence.LAYOUT_CASES:
        attempt["diagnostics"] = {"status": "unavailable", "reason": "synthetic unavailable signal"}
    if case_id in evidence.VIEWPORTS:
        attempt["viewports"] = {
            stage: {"orientation": orientation, "width": 390, "height": 664,
                    "focused": True, "visible": True, "cursor_visible": True}
            for stage, orientation in evidence.VIEWPORTS[case_id].items()
        }
    if case_id in evidence.COMPOSITION:
        attempt["recipe_id"] = cohort["recipes"][case_id]["id"]
        composition(attempt, identity, case["expected"]["afterHex"])
    if case_id in evidence.RECOVERY:
        old_identity = dict(identity, attemptId=index - 1, generation=index - 1)
        earlier = native(old_identity, case["expected"]["beforeHex"])
        attempt["recovery"] = {"before": earlier, "retained": earlier.copy(),
                               "delivery_uncertain": False, "old_generation_extra_event": False,
                               "restart": {"ref": f"{attempt['id']} restart capture", "action_completed": True,
                                           "outcome": "fresh", "previous": old_identity, "observed": identity}}
    retain_measurement(cohort, attempt)
    cohort["attempts"].append(attempt)
    return attempt


def restart_without_fresh_attempt(cohort, attempt, outcome):
    recovery = attempt["recovery"]
    restart = recovery["restart"]
    restart.update(outcome=outcome)
    restart.pop("observed")
    if outcome == "same-generation":
        restart["observed"] = recovery["retained"].copy()
    if outcome == "no-new-generation":
        restart["begin_failure"] = dict(restart["previous"], ref=f"{restart['ref']} begin failure")
    measurement = next(record for launch in cohort["launches"] for record in launch["measurements"]
                       if record["attempt_id"] == attempt["id"])
    for key in ("attemptId", "generation"):
        attempt[key] = measurement[key] = restart["previous"][key]
    attempt.pop("final")
    attempt["observations"]["started_fresh"] = False
    return mark(attempt, "inconclusive" if outcome == "uncertain" else "failing")


def mark(attempt, status):
    attempt.update(status=status, reason="synthetic negative control")
    return attempt["id"]


def report_for(cohort, status="pass", disposition="continue-tauri-evaluation", support=None):
    return {
        "schema_version": 1, "fixture_sha256": evidence.MANIFEST_SHA256, "status": status,
        "reasons": [] if status == "pass" else ["synthetic non-pass reason"],
        "cohorts": {"baseline": cohort} if cohort is not None else {},
        "decision": {"disposition": disposition, "production_shell": "open", "minimum_ios": "open",
                     "supporting_attempt_ids": [row["id"] for row in cohort["attempts"]] if support is None else support},
    }


class EvidenceTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.cases = evidence.load_manifest(MANIFEST)

    def setUp(self):
        self.baseline = bound_cohort()
        make_recipes(self.baseline, self.cases)

    def complete(self, cohort):
        for case_id, case in self.cases.items():
            for route in ("sample", "native") if case_id in evidence.PASTE else (None,):
                for index in range(2):
                    add_attempt(cohort, case, f"{route}-{index}", route)
        return report_for(cohort)

    def validate(self, report, status):
        self.assertEqual(evidence.validate(report, self.cases).status, status)

    def reject(self, report, message):
        with self.assertRaisesRegex(evidence.EvidenceError, message):
            evidence.validate(report, self.cases)

    def sample(self, case_id="composition-commit"):
        return add_attempt(self.baseline, self.cases[case_id], "trial")

    def test_complete_physical_matrix_and_optional_diagnostics(self):
        report = self.complete(self.baseline)
        self.validate(report, "pass")
        keyboard = next(row for row in self.baseline["attempts"] if row["case_id"] == "keyboard-open-cursor")
        keyboard["diagnostics"] = {"status": "available", "events": [{
            "sequence": 18, "event": "focus", "focused": True, "visible": True, "width": 390, "height": 300,
        }]}
        self.validate(report, "pass")

    def test_effective_capability_contract_matches_the_shipped_declaration(self):
        config = json.loads((MANIFEST.parent / "src-tauri/tauri.conf.json").read_bytes())
        self.assertEqual(config["app"]["security"]["capabilities"], [evidence.CAPABILITY])
        self.assertEqual(set(evidence.OBSERVATIONS), set(self.cases))

    def test_manifest_cannot_be_replaced_and_digest_cannot_be_stale(self):
        report = self.complete(self.baseline)
        report["fixture_sha256"] = "f" * 64
        self.reject(report, "report fixture digest")
        with TemporaryDirectory() as tmp:
            manifest = Path(tmp) / "cases.json"
            manifest.write_bytes(MANIFEST.read_bytes() + b"\n")
            with self.assertRaisesRegex(evidence.EvidenceError, "shipped matrix"):
                evidence.load_manifest(manifest)

    def test_touch_copy_uses_in_fixture_control_and_finishes_on_same_generation(self):
        case = self.cases["touch-selection-copy"]
        self.assertIn("Verify copied token", case["actions"][-1])
        self.assertIn("Read", case["actions"][-1])
        self.assertIn("same generation", case["actions"][-1])
        self.assertEqual(case["expected"], {"beforeHex": "", "afterHex": ""})

    def test_missing_or_simulator_tuple_and_mixed_attempt_fingerprint(self):
        row = self.sample()
        report = report_for(self.baseline)
        for value, reason in ((None, "measured tuple"), ({"destination": "simulator"}, "physical iPhone")):
            with self.subTest(value=value):
                saved = self.baseline["tuple"]
                self.baseline["tuple"] = value
                self.reject(report, reason)
                self.baseline["tuple"] = saved
        row["cohort_fingerprint"] = "f" * 64
        self.reject(report, "different cohort tuple")

    def test_mixed_generated_sdk_or_deployment_settings(self):
        self.sample()
        report = report_for(self.baseline)
        self.baseline["build"]["generated_settings"]["sdkroot"] = "other SDK"
        self.reject(report, "differs from the tested tuple")

    def test_case_attempts_must_use_the_bound_keyboard(self):
        row = self.sample("touch-scroll")
        row["keyboard_language"] = "uk"
        self.reject(report_for(self.baseline), "mixed keyboard or orientation")

    def test_deployment_floor_above_tested_os(self):
        self.sample()
        report = report_for(self.baseline)
        self.baseline["tuple"]["deployment_target"] = "99.1"
        self.reject(report, "newer than the tested OS")

    def test_requires_matching_pre_xcode_and_independent_ci(self):
        self.sample()
        report = report_for(self.baseline)
        self.baseline["pre_xcode"]["mac_cargo"] = 1
        self.reject(report, "mac_cargo did not pass")
        self.baseline["pre_xcode"]["mac_cargo"] = 0
        self.baseline["ci"]["native"]["source_revision"] = "f" * 40
        self.reject(report, "CI native")

    def test_missing_or_unbound_generated_start_app_validation(self):
        self.sample()
        report = report_for(self.baseline)
        entry = self.baseline["build"].pop("entry_validation")
        self.reject(report, "generated entry validation")
        for key, value in (("build_log_sha256", ""), ("build_log_ref", ""),
                           ("source_revision", "f" * 40), ("verified_before_install", False),
                           ("inspected_bundle_sha256", "f" * 64), ("installed_bundle_sha256", "f" * 64),
                           ("cli_start_app_validation", False), ("symbol", "_other")):
            with self.subTest(key=key):
                self.baseline["build"]["entry_validation"] = entry | {key: value}
                self.reject(report, "generated entry|generated start_app")

    def test_effective_boundary_offline_launch_and_desktop_binding(self):
        self.sample()
        report = report_for(self.baseline)
        boundary = self.baseline["boundary"]
        for key, value in (("file_capabilities", {"extra": {}}), ("capability", evidence.CAPABILITY | {"remote": {}}),
                           ("resolved_grants", {"extra": ["scaffold_status"]}), ("handlers", ["local_inventory"])):
            with self.subTest(key=key):
                self.baseline["boundary"] = boundary | {key: value}
                self.reject(report, "effective iOS capability")
        self.baseline["boundary"] = boundary
        boundary["offline_launch"]["bundle_sha256"] = "f" * 64
        self.reject(report, "bound offline startup")
        boundary["offline_launch"]["bundle_sha256"] = self.baseline["tuple"]["bundle_sha256"]
        boundary["desktop"]["fixture_commands_absent"] = False
        self.reject(report, "registry separation")

    def test_unqualified_or_changed_composition_recipe(self):
        row = self.sample()
        report = report_for(self.baseline)
        recipe = self.baseline["recipes"][row["case_id"]]
        rehearsals = recipe["rehearsals"]
        recipe["rehearsals"] = rehearsals[:1]
        self.reject(report, "two distinct observable rehearsals")
        recipe["rehearsals"] = rehearsals
        recipe["steps"].append("different gesture")
        self.reject(report, "frozen procedure")
        recipe["steps"].pop()
        row["recipe_id"] = "f" * 64
        self.reject(report, "frozen qualified recipe")

    def test_rehearsals_must_come_from_the_signed_cohort(self):
        row = self.sample()
        other = bound_cohort("3", "4")
        make_recipes(other, self.cases)
        recipe = self.baseline["recipes"][row["case_id"]]
        recipe["rehearsals"] = other["recipes"][row["case_id"]]["rehearsals"]
        self.reject(report_for(self.baseline), "different signed cohort")

    def test_replacement_checkpoint_follows_both_nonempty_updates(self):
        row = self.sample("composition-replacement")
        trace = row["trace"]["events"]
        row["trace"]["events"] = events(*trace[:4], *trace[5:])
        row["checkpoint"]["sequence"] = 3
        self.reject(report_for(self.baseline), "checkpoint precedes the replacement update")

    def test_checkpoint_required_active_focused_in_order_and_empty(self):
        row = self.sample()
        report = report_for(self.baseline)
        checkpoint = row.pop("checkpoint")
        self.reject(report, "native snapshot")
        for key, value, message in (
            ("active", False, "active and focused"), ("focused", False, "active and focused"),
            ("sequence", 1, "completed trace sample"), ("generation", 999, "different attempt or generation"),
            ("hex", "01", "accepted event count"), ("nextSequence", True, "counters do not match"),
        ):
            with self.subTest(key=key):
                row["checkpoint"] = checkpoint | {key: value}
                self.reject(report, message)

    def test_native_records_belong_to_same_measured_attempt(self):
        self.sample()
        for part, key in (("trace", "attemptId"), ("checkpoint", "attemptId"), ("final", "generation")):
            with self.subTest(part=part):
                report = copy.deepcopy(report_for(self.baseline))
                report["cohorts"]["baseline"]["attempts"][0][part][key] += 1
                self.reject(report, "different attempt or generation")

    def test_before_after_only_missing_end_order_or_truncation_is_held(self):
        row = self.sample()
        report = report_for(self.baseline)
        trace = row.pop("trace")
        self.reject(report, "composition trace")
        row["trace"] = trace | {"complete": False}
        self.reject(report, "truncated")
        row["trace"] = trace | {"events": events(*(evt for evt in trace["events"] if evt["event"] != "end"))}
        self.reject(report, "one composition end")
        row["trace"] = trace
        trace["events"][2]["sequence"] -= 1
        self.reject(report, "duplicated or reversed")

    def test_append_accepted_after_empty_sample_but_before_end_fails(self):
        row = self.sample()
        trace = row["trace"]["events"]
        row["trace"]["events"] = events(*trace[:3], *trace[4:7], trace[3], trace[-1])
        self.reject(report_for(self.baseline), "native input accepted before compositionend")
        failure = mark(row, "failing")
        self.validate(report_for(self.baseline, "failing", "mandatory-failure", [failure]), "failing")

    def test_rejected_pre_end_callback_with_zero_bytes_holds(self):
        row = self.sample("composition-cancel")
        trace = row["trace"]["events"]
        row["trace"]["events"] = events(*trace[:3], event("rejection", 2), *trace[3:])
        self.reject(report_for(self.baseline), "premature onData")
        mark(row, "inconclusive")
        self.validate(report_for(self.baseline, "inconclusive", "inconclusive", []), "inconclusive")

    def test_dispatch_before_end_with_ack_after_is_uncertain(self):
        row = self.sample()
        trace = row["trace"]["events"]
        row["trace"]["events"] = events(*trace[:3], *trace[4:6], trace[3], *trace[6:])
        self.reject(report_for(self.baseline), "uncertain pre-end")

    def test_preedit_snapshot_proves_acceptance_even_with_late_ack(self):
        row = self.sample()
        trace = row["trace"]["events"]
        row["trace"]["events"] = events(*trace[:3], *trace[4:6], event("active checkpoint", 2, 2),
                                          trace[3], *trace[6:])
        row["checkpoint"].update(sequence=6, hex="c3a9", acceptedEvents=1, nextSequence=2)
        failure = mark(row, "failing")
        self.validate(report_for(self.baseline, "failing", "mandatory-failure", [failure]), "failing")
        row["checkpoint"]["hex"] = "c3a8"
        self.reject(report_for(self.baseline, "failing", "mandatory-failure", [failure]), "prefix of the final")

    def test_preedit_native_checkpoint_without_ack_can_prove_failure(self):
        for unconfirmed in (False, True):
            with self.subTest(unconfirmed=unconfirmed):
                row = add_attempt(self.baseline, self.cases["composition-commit"], f"unconfirmed-{unconfirmed}")
                row["capture_error"] = True
                trace = row["trace"]["events"]
                receipt = [event("append delivery unconfirmed", 2, 1)] if unconfirmed else []
                row["trace"]["events"] = events(*trace[:3], *trace[4:6], *receipt,
                                                  event("active checkpoint", 2, 2), trace[3], trace[-1])
                row["checkpoint"].update(sequence=6 + len(receipt), hex="c3a9", acceptedEvents=1,
                                         nextSequence=2, eventLengths=[2])
                row["final"]["eventLengths"] = [2]
                failure_id = mark(row, "failing")
                report = report_for(self.baseline, "failing", "mandatory-failure", [failure_id])
                self.validate(report, "failing")
                row["checkpoint"]["hex"] = "c3a8"
                self.reject(report, "prefix of the final")
                row["checkpoint"]["hex"] = "c3a9"
                row["checkpoint"]["generation"] += 1
                self.reject(report, "different attempt or generation")
                row["checkpoint"]["generation"] -= 1
                row["trace"]["events"] = events(*trace[:2], event("active checkpoint", 2, 2),
                                                  *trace[4:6], trace[3], *receipt, trace[-1])
                row["checkpoint"]["sequence"] = 3
                self.reject(report, "checkpoint counters do not reconcile")
                row["trace"]["events"] = events(*trace[:3], *trace[4:6], trace[3], *receipt, trace[-1])
                row["checkpoint"].update(sequence=3, hex="", acceptedEvents=0, nextSequence=1, eventLengths=[])
                mark(row, "inconclusive")
                self.validate(report_for(self.baseline, "inconclusive", "inconclusive", []), "inconclusive")
                remove_attempts(self.baseline, row)

    def test_unconfirmed_visual_gesture_cannot_be_measured_as_failure(self):
        for case_id, observation in (("composition-commit", "end_state_observed"),
                                     ("composition-replacement", "replaced_in_preedit")):
            with self.subTest(case_id=case_id):
                row = self.sample(case_id)
                row["observations"][observation] = False
                mark(row, "inconclusive")
                self.validate(report_for(self.baseline, "inconclusive", "inconclusive", []), "inconclusive")

    def test_duplicate_post_end_append_is_failure(self):
        row = self.sample()
        trace = row["trace"]["events"]
        row["trace"]["events"] = events(*trace[:-1], event("admission", 2), event("append dispatch", 2, 2),
                                          event("append acknowledgment", 2, 2), event("final snapshot", 4, 3))
        row["final"].update(hex="c3a9c3a9", acceptedEvents=2, nextSequence=3)
        self.reject(report_for(self.baseline), "duplicate or unexpected composition append")

    def test_missing_callback_ack_or_text_in_trace_holds(self):
        row = self.sample()
        trace = row["trace"]["events"]
        row["trace"]["events"] = events(*trace[:-2], trace[-1])
        self.reject(report_for(self.baseline), "incomplete admission")
        row["trace"]["events"] = trace
        trace[1]["data"] = "synthetic text"
        self.reject(report_for(self.baseline), "text-free")

    def test_absent_wkwebview_results_and_wrong_recovery(self):
        row = self.sample("rotation")
        report = report_for(self.baseline)
        row["observations"].pop("prompt_unobscured")
        self.reject(report, "required visible")
        row["observations"]["prompt_unobscured"] = True
        row["viewports"].pop("rotated")
        self.reject(report, "rotated viewport")
        remove_attempts(self.baseline, row)
        row = self.sample("app-switch")
        row["recovery"]["retained"]["hex"] = "6161"
        self.reject(report_for(self.baseline), "retained native bytes")

    def test_restart_failure_is_measured_and_requires_bound_old_proof(self):
        for outcome in ("same-generation", "no-new-generation", "uncertain"):
            with self.subTest(outcome=outcome):
                row = self.sample("app-switch")
                row_id = restart_without_fresh_attempt(self.baseline, row, outcome)
                row["capture_error"] = True
                report = (report_for(self.baseline, "inconclusive", "inconclusive", []) if outcome == "uncertain"
                          else report_for(self.baseline, "failing", "mandatory-failure", [row_id]))
                self.validate(report, report["status"])
                restart = row["recovery"]["restart"]
                if outcome != "uncertain":
                    restart["action_completed"] = False
                    self.reject(report, "completed foreground restart action")
                    restart["action_completed"] = True
                    proof = restart["observed" if outcome == "same-generation" else "begin_failure"]
                    proof["generation"] += 1
                    self.reject(report, "different attempt or generation|begin failure belongs to a different")
                    proof["generation"] -= 1
                    restart["previous"]["generation"] += 1
                    self.reject(report, "not bound to the old attempt")
                remove_attempts(self.baseline, row)

    def test_completed_recovery_prefix_requires_proven_restart_failure(self):
        for case_id in sorted(evidence.RECOVERY):
            for outcome, proof_key in (("same-generation", "observed"), ("no-new-generation", "begin_failure")):
                with self.subTest(case=case_id, outcome=outcome):
                    row = self.sample(case_id)
                    row_id = restart_without_fresh_attempt(self.baseline, row, outcome)
                    row.update(action_order=[1, 2], capture_error=True)
                    failed = report_for(self.baseline, "failing", "mandatory-failure", [row_id])
                    self.validate(failed, "failing")
                    saved = copy.deepcopy(row)
                    transition = evidence.OBSERVATIONS[case_id][0]
                    for label, mutate, reason in (
                        ("uncertain", lambda: restart.update(outcome="uncertain"), "restart outcome is uncertain"),
                        ("absent proof", lambda: restart.pop(proof_key), "requires an object"),
                        ("mismatched proof", lambda: restart[proof_key].update(generation=row["generation"] + 1),
                         "different attempt or generation|begin failure belongs to a different"),
                        ("unbound restart", lambda: restart["previous"].update(generation=row["generation"] + 1),
                         "not bound to the old attempt"),
                        ("incomplete gesture", lambda: restart.update(action_completed=False),
                         "completed foreground restart action"),
                        ("contradictory visible start", lambda: row["observations"].update(started_fresh=True),
                         "conflicts with the visible"),
                        ("missing visible result", lambda: row["observations"].pop("started_fresh"),
                         "visible fresh-start observation"),
                        ("missing return", lambda: row["observations"].pop(transition), "transition was not established"),
                        ("fresh final", lambda: row.update(final=native(
                            dict(restart["previous"], attemptId=row["attemptId"] + 1, generation=row["generation"] + 1),
                            self.cases[case_id]["expected"]["afterHex"])),
                         "conflicts with a new generation|does not match the retained"),
                        ("reordered", lambda: row.update(action_order=[2, 1]), "unordered prescribed actions"),
                        ("boolean action", lambda: row.update(action_order=[True, 2]), "unordered prescribed actions"),
                        ("float action", lambda: row.update(action_order=[1, 2.0]), "unordered prescribed actions"),
                    ):
                        with self.subTest(control=label):
                            row.clear()
                            row.update(copy.deepcopy(saved))
                            restart = row["recovery"]["restart"]
                            # A separate observation cannot qualify an unproven action prefix.
                            row["replayed"] = True
                            mutate()
                            self.reject(failed, reason)
                            mark(row, "inconclusive")
                            self.validate(report_for(self.baseline, "inconclusive", "inconclusive", []), "inconclusive")
                    remove_attempts(self.baseline, row)

    def test_fresh_recovery_needs_the_full_action_order(self):
        for case_id in sorted(evidence.RECOVERY):
            with self.subTest(case=case_id):
                row = self.sample(case_id)
                report = report_for(self.baseline, "inconclusive", "inconclusive", [])
                self.validate(report, "inconclusive")
                row["action_order"] = [1, 2]
                self.reject(report, "fresh foreground restart requires all prescribed actions")
                mark(row, "inconclusive")
                self.validate(report, "inconclusive")
                remove_attempts(self.baseline, row)

    def test_repeated_restart_failure_requires_independent_old_attempts(self):
        case = self.cases["lock-foreground"]
        baseline_ids = []
        for suffix in ("failure", "repeat"):
            row = add_attempt(self.baseline, case, suffix)
            baseline_ids.append(restart_without_fresh_attempt(self.baseline, row, "same-generation"))
        corrected = bound_cohort("3", "4")
        persistent = add_attempt(corrected, case, "persistent")
        persistent_id = restart_without_fresh_attempt(corrected, persistent, "same-generation")
        report = report_for(self.baseline, "failing", "comparison-eligible", [*baseline_ids, persistent_id])
        report["cohorts"]["corrected"] = corrected
        report["correction"] = {"case_id": case["id"], "layer": "synthetic foreground layer",
                                "change": "synthetic scoped correction", "scope": ["synthetic source path"],
                                "baseline_attempt_ids": baseline_ids}
        self.validate(report, "failing")
        original, repeat = self.baseline["attempts"]
        original_ref = repeat["recovery"]["restart"]["ref"]
        repeat["recovery"]["restart"]["ref"] = original["recovery"]["restart"]["ref"]
        self.reject(report, "foreground restart observation is reused")
        repeat["recovery"]["restart"]["ref"] = original_ref
        for key in ("attemptId", "generation"):
            repeat[key] = self.baseline["launches"][0]["measurements"][1][key] = original[key]
        repeat["recovery"] = copy.deepcopy(original["recovery"])
        repeat["recovery"]["restart"]["ref"] = "different synthetic restart reference"
        self.reject(report, "native attempt or generation is reused")

    def test_fresh_recovery_requires_greater_generation_and_exact_input(self):
        row = self.sample("lock-foreground")
        report = report_for(self.baseline, "inconclusive", "inconclusive", [])
        self.validate(report, "inconclusive")
        row["final"]["hex"] = "61"
        self.reject(report, "final bytes differ")
        row["final"]["hex"] = self.cases["lock-foreground"]["expected"]["afterHex"]
        row["recovery"]["restart"]["observed"]["generation"] -= 1
        self.reject(report, "fresh attempt and generation")

    def test_mismatched_bytes_or_replay(self):
        row = self.sample("english-return")
        row["final"]["hex"] = "6f6b0d6f6b0d"
        self.reject(report_for(self.baseline), "final bytes differ")
        row["final"]["hex"] = "6f6b0d"
        row["replayed"] = True
        self.reject(report_for(self.baseline), "replayed input")

    def test_missing_case_or_paste_route_cannot_pass(self):
        for case_id, route in (("rotation", None), ("paste-short", "native")):
            with self.subTest(case=case_id, route=route):
                complete = self.complete(self.baseline)
                remove_attempts(self.baseline, *(row for row in self.baseline["attempts"]
                                                 if (row["case_id"], row.get("route")) == (case_id, route)))
                self.reject(complete, "report status must be inconclusive")
                remove_attempts(self.baseline, *self.baseline["attempts"])

    def test_duplicate_attempt_ids_and_duplicate_native_runs(self):
        row = self.sample()
        duplicate = copy.deepcopy(row)
        self.baseline["attempts"].append(duplicate)
        self.reject(report_for(self.baseline), "duplicate measured attempt ID")
        duplicate["id"] += "-renamed"
        retain_measurement(self.baseline, duplicate)
        self.reject(report_for(self.baseline), "native attempt or generation is reused")

    def test_failure_keeps_old_evidence_and_requires_two_fresh_passes(self):
        self.complete(self.baseline)
        case = self.cases["english-return"]
        failure = add_attempt(self.baseline, case, "failure")
        failure["observations"]["cursor_visible"] = False
        failure_id = mark(failure, "failing")
        add_attempt(self.baseline, case, "fresh-1")
        self.validate(report_for(self.baseline, "failing", "mandatory-failure", [failure_id]), "failing")
        add_attempt(self.baseline, case, "fresh-2")
        support = [row["id"] for row in self.baseline["attempts"] if row["case_id"] != case["id"]]
        support.extend(row["id"] for row in self.baseline["attempts"][-2:])
        self.validate(report_for(self.baseline, support=support), "pass")

    def test_older_native_attempts_cannot_be_reordered_as_fresh_passes(self):
        report = self.complete(self.baseline)
        case = self.cases["english-return"]
        failure = add_attempt(self.baseline, case, "failure")
        failure["observations"]["cursor_visible"] = False
        failure_id = mark(failure, "failing")
        attempts = self.baseline["attempts"]
        first, second = attempts[:2]
        attempts[:] = [*attempts[2:], first, second]
        self.reject(report, "native attempt or generation is out of order")
        mark(first, "inconclusive")
        mark(second, "inconclusive")
        held = report_for(self.baseline, "failing", "mandatory-failure", [failure_id])
        self.validate(held, "failing")

    def test_native_counters_can_restart_on_a_separate_app_launch(self):
        report = self.complete(self.baseline)
        first, second = self.baseline["attempts"][-2:]
        remove_attempts(self.baseline, second)
        start_launch(self.baseline)
        case = self.cases[second["case_id"]]
        fresh = add_attempt(self.baseline, case, "relaunched")
        self.assertLess(fresh["generation"], first["generation"])
        report["decision"]["supporting_attempt_ids"][-1] = fresh["id"]
        self.validate(report, "pass")

    def test_closed_launch_successes_cannot_clear_a_new_failure(self):
        complete = self.complete(self.baseline)
        case = self.cases["english-return"]
        attempts = self.baseline["attempts"]
        old_rows = attempts.copy()
        first, second = old_rows[:2]
        start_launch(self.baseline)
        failed = add_attempt(self.baseline, case, "new-failure")
        failed["observations"]["cursor_visible"] = False
        failure_id = mark(failed, "failing")
        held = report_for(self.baseline, "failing", "mandatory-failure", [failure_id])
        self.validate(held, "failing")
        for submitted in ([*old_rows[2:], failed, first, second], [failed, *old_rows]):
            attempts[:] = submitted
            forged = report_for(self.baseline, support=complete["decision"]["supporting_attempt_ids"])
            self.reject(forged, "out of order")
            moved = submitted[submitted.index(failed) + 1:]
            for row in moved:
                mark(row, "inconclusive")
            self.validate(held, "failing")
            for row in moved:
                row.update(status="pass")
                row.pop("reason")
        attempts[:] = [*old_rows[2:], failed, first, second]
        for row in (first, second):
            mark(row, "inconclusive")
        self.validate(held, "failing")
        start_launch(self.baseline)
        later = [add_attempt(self.baseline, case, f"later-{i}") for i in range(2)]
        self.assertEqual(later[0]["generation"], failed["generation"])
        support = [row["id"] for row in old_rows[2:]] + [row["id"] for row in later]
        self.validate(report_for(self.baseline, support=support), "pass")

    def test_partially_bound_history_precedes_later_independent_passes(self):
        early = self.sample("app-switch")
        report = self.complete(self.baseline)
        attempts = self.baseline["attempts"]
        later = [row for row in attempts[1:] if row["case_id"] == early["case_id"]]
        self.assertEqual(len(later), 2)
        support = report["decision"]["supporting_attempt_ids"]
        support.remove(early["id"])
        early.pop("launch_ref")
        mark(early, "inconclusive")
        # Submitted placement cannot move the early hold after the two retained passes.
        attempts.append(attempts.pop(0))
        self.validate(report, "pass")

        # Conflicting claims cannot count or affect the bound rows' native identity and restart checks.
        early.update(launch_ref="conflicting capture", attemptId=later[0]["attemptId"],
                     generation=later[0]["generation"], recovery=copy.deepcopy(later[0]["recovery"]))
        self.validate(report, "pass")
        early["status"] = "pass"
        self.reject(report, "does not match its retained launch and measurement")
        mark(early, "inconclusive")
        support[support.index(later[0]["id"])] = early["id"]
        self.reject(report, "disposition must reference the attempts that establish it")

    def test_later_and_unpositioned_partial_rows_still_hold_the_case(self):
        self.complete(self.baseline)
        start_launch(self.baseline)
        late = self.sample("app-switch")
        late.pop("launch_ref")
        mark(late, "inconclusive")
        attempts = self.baseline["attempts"]
        attempts.insert(0, attempts.pop())
        held = report_for(self.baseline, "inconclusive", "inconclusive", [])
        self.validate(held, "inconclusive")
        self.reject(report_for(self.baseline, support=[row["id"] for row in attempts if row is not late]),
                    "report status must be inconclusive")

        self.baseline["launches"][-1]["measurements"].clear()
        self.validate(held, "inconclusive")

    def test_attempt_must_match_retained_launch_and_measurement(self):
        row = self.sample("english-return")
        report = report_for(self.baseline)
        for change in ({"launch_id": "unknown"}, {"launch_ref": "wrong"}, {"measurement_position": 2},
                       {"attemptId": 23, "generation": 23}):
            with self.subTest(change=change):
                original = row.copy()
                row.update(change)
                self.reject(report, "retained launch and measurement")
                row.update(original)
        self.baseline["attempts"].clear()
        self.reject(report, "retained measurement has no submitted attempt")

    def test_launch_records_require_independent_references_and_order(self):
        self.sample("english-return")
        later = start_launch(self.baseline)
        add_attempt(self.baseline, self.cases["english-return"], "later")
        report = report_for(self.baseline, "inconclusive", "inconclusive", [])
        first = self.baseline["launches"][0]
        original = later.copy()
        for key in ("ref", "order", "id"):
            with self.subTest(key=key):
                later[key] = first[key]
                self.reject(report, "retained launches require ordered, unique labels and capture references")
                later.update(original)

    def test_truthful_setup_hold_and_inconclusive_cli_modes(self):
        hold = report_for(None, "setup-hold", "inconclusive", [])
        self.validate(hold, "setup-hold")
        row = self.sample()
        row.pop("checkpoint")
        mark(row, "inconclusive")
        self.validate(report_for(self.baseline, "inconclusive", "inconclusive", []), "inconclusive")
        with TemporaryDirectory() as tmp:
            path = Path(tmp) / "report.json"
            path.write_text(json.dumps(hold), encoding="utf-8")
            argv = ["--manifest", str(MANIFEST), "--report", str(path)]
            self.assertEqual(evidence.main([*argv, "--check-report"]), 0)
            self.assertEqual(evidence.main([*argv, "--require-pass"]), 1)

    def correction_fixture(self, case_id="english-return", failed_observation="cursor_visible"):
        case = self.cases[case_id]
        baseline_ids = []
        for tag in ("original", "repeat"):
            row = add_attempt(self.baseline, case, tag)
            row["observations"][failed_observation] = False
            baseline_ids.append(mark(row, "failing"))
        corrected = bound_cohort("3", "4")
        corrected["tuple"]["app"]["version"] = "0.1.1"
        corrected["tuple"]["app"]["build"] = "2"
        corrected["build"]["installation"]["app"] = copy.deepcopy(corrected["tuple"]["app"])
        corrected["fingerprint"] = evidence.fingerprint(corrected["tuple"])
        make_recipes(corrected, self.cases)
        row = add_attempt(corrected, case, "corrected")
        row["observations"][failed_observation] = False
        corrected_id = mark(row, "failing")
        report = report_for(self.baseline, "failing", "comparison-eligible", [*baseline_ids, corrected_id])
        report["cohorts"]["corrected"] = corrected
        report["correction"] = {"case_id": case["id"], "layer": "synthetic viewport layer",
                                "change": "synthetic scoped correction", "scope": ["synthetic source path"],
                                "baseline_attempt_ids": baseline_ids}
        return report, corrected

    def test_correction_gate_requires_baseline_reproduction_and_persistent_corrected_failure(self):
        report, _ = self.correction_fixture()
        self.validate(report, "failing")
        remove_attempts(self.baseline, self.baseline["attempts"][-1])
        self.reject(report, "same unresolved measured failure twice")

    def test_correction_gate_rejects_comparison_without_corrected_failure(self):
        report, corrected = self.correction_fixture()
        remove_attempts(corrected, *corrected["attempts"])
        self.add_comparison(report, native_cohort())
        self.reject(report, "persistent corrected-build mandatory failure")

    def test_correction_gate_distinguishes_wrong_byte_results(self):
        report, corrected = self.correction_fixture()
        for row in self.baseline["attempts"] + corrected["attempts"]:
            row["observations"]["cursor_visible"] = True
            row["final"]["hex"] = "6f6b"
        self.validate(report, "failing")
        self.baseline["attempts"][1]["final"]["hex"] = "6f6b0d0d"
        self.reject(report, "same unresolved measured failure twice")
        self.baseline["attempts"][1]["final"]["hex"] = "6f6b"
        corrected["attempts"][0]["final"]["hex"] = "6f6b0d0d"
        self.reject(report, "shell disposition must be mandatory-failure")

    def test_partial_readback_with_unresolved_return_cannot_authorize_comparison(self):
        report, corrected = self.correction_fixture()
        for row in self.baseline["attempts"] + corrected["attempts"]:
            row["observations"]["cursor_visible"] = True
            row["final"]["hex"] = "6f6b"
        self.validate(report, "failing")
        for row in self.baseline["attempts"]:
            row["capture_error"] = True
            mark(row, "inconclusive")
        self.reject(report, "same unresolved measured failure twice")
        for row in self.baseline["attempts"]:
            row["capture_error"] = False
            mark(row, "failing")
        persistent = corrected["attempts"][0]
        persistent["capture_error"] = True
        mark(persistent, "inconclusive")
        report["cohorts"]["comparison"] = {}
        self.reject(report, "persistent corrected-build mandatory failure")
        del report["cohorts"]["comparison"]
        report.update(status="inconclusive")
        report["decision"].update(disposition="inconclusive",
                                  supporting_attempt_ids=report["correction"]["baseline_attempt_ids"])
        self.validate(report, "inconclusive")

    def test_unchanged_recovery_read_requires_confirmed_initial_delivery_for_comparison(self):
        report, corrected = self.correction_fixture("app-switch", "prompt_visible")
        baseline_rows = self.baseline["attempts"]
        persistent = corrected["attempts"][0]
        baseline_ids = report["correction"]["baseline_attempt_ids"]
        for vector in ("", "78"):
            with self.subTest(initial_hex=vector):
                for row in [*baseline_rows, persistent]:
                    recovery = row["recovery"]
                    row.update(capture_error=False)
                    row["observations"]["prompt_visible"] = True
                    initial = native(recovery["restart"]["previous"], vector)
                    recovery.update(before=initial, retained=initial.copy())
                    mark(row, "failing")
                self.validate(report, "failing")
                mark(baseline_rows[0], "inconclusive")
                self.reject(report, "initial bytes differ from the manifest")
                mark(baseline_rows[0], "failing")

                for row in baseline_rows:
                    row["capture_error"] = True
                    mark(row, "inconclusive")
                self.validate(report_for(self.baseline, "inconclusive", "inconclusive", []), "inconclusive")
                self.reject(report, "same unresolved measured failure twice")

                for row in baseline_rows:
                    row["capture_error"] = False
                    mark(row, "failing")
                persistent.pop("capture_error")
                mark(persistent, "inconclusive")
                report["cohorts"]["comparison"] = {}
                self.reject(report, "persistent corrected-build mandatory failure")
                del report["cohorts"]["comparison"]
                held = dict(report, status="inconclusive", decision=dict(
                    report["decision"], disposition="inconclusive", supporting_attempt_ids=baseline_ids))
                self.validate(held, "inconclusive")

    def test_bound_retained_changes_count_with_unresolved_initial_delivery(self):
        for label, before_hex, retained_hex, before_lengths, retained_lengths, extra in (
            ("bytes", "78", "79", [1], [1], False),
            ("event count", "6162", "6162", [2], [1, 1], False),
            ("event lengths", "616263", "616263", [1, 2], [2, 1], False),
            ("observed extra input", "78", "78", [1], [1], True),
        ):
            with self.subTest(change=label):
                row = self.sample("app-switch")
                row["capture_error"] = True
                recovery = row["recovery"]
                identity = recovery["restart"]["previous"]
                recovery["before"] = dict(native(identity, before_hex), acceptedEvents=len(before_lengths),
                                          eventLengths=before_lengths)
                recovery["retained"] = dict(native(identity, retained_hex), acceptedEvents=len(retained_lengths),
                                            eventLengths=retained_lengths)
                recovery["old_generation_extra_event"] = extra
                self.reject(report_for(self.baseline), "retained native bytes or events changed")
                row_id = mark(row, "failing")
                self.validate(report_for(self.baseline, "failing", "mandatory-failure", [row_id]), "failing")
                recovery["retained"]["generation"] += 1
                self.reject(report_for(self.baseline), "different attempt or generation")
                remove_attempts(self.baseline, row)

    def test_failed_restart_reproduces_with_unresolved_initial_delivery(self):
        report, corrected = self.correction_fixture("lock-foreground", "prompt_visible")
        for cohort in (self.baseline, corrected):
            for row in cohort["attempts"]:
                row.update(capture_error=True)
                row["observations"]["prompt_visible"] = True
                recovery = row["recovery"]
                recovery["before"] = native(recovery["restart"]["previous"], "")
                recovery["retained"] = recovery["before"].copy()
                # Retain the legacy full-order baseline, alongside a truthful corrected prefix.
                if cohort is corrected:
                    row["action_order"] = [1, 2]
                restart_without_fresh_attempt(cohort, row, "same-generation")
        self.validate(report, "failing")

    def test_unresolved_capture_preserves_independent_measured_failures(self):
        for case_id, failure, expected in (
            ("english-return", {"start_empty": False}, "unexpected startup input"),
            ("english-return", {"replayed": True}, "replayed input"),
            ("english-return", {"observations": {"cursor_visible": False}}, "visible observation failed"),
            ("keyboard-open-cursor", {}, "prompt or cursor obscured"),
            ("app-switch", {}, "retained native bytes"),
        ):
            with self.subTest(case=case_id, expected=expected):
                row = self.sample(case_id)
                row.update(failure, capture_error=True)
                row.pop("final")
                if case_id == "keyboard-open-cursor":
                    row["viewports"]["keyboard_open"]["cursor_visible"] = False
                if case_id == "app-switch":
                    row["recovery"]["retained"]["hex"] = "6161"
                self.reject(report_for(self.baseline), expected)
                failure_id = mark(row, "failing")
                self.validate(report_for(self.baseline, "failing", "mandatory-failure", [failure_id]), "failing")
                remove_attempts(self.baseline, row)

    def test_correction_gate_rejects_a_different_failed_case_or_mixed_conditions(self):
        report, corrected = self.correction_fixture()
        remove_attempts(corrected, *corrected["attempts"])
        other = add_attempt(corrected, self.cases["ukrainian-return"], "other")
        other["observations"]["cursor_visible"] = False
        mark(other, "failing")
        self.reject(report, "shell disposition must be mandatory-failure")
        corrected["tuple"]["device_alias"] = "other phone"
        corrected["fingerprint"] = evidence.fingerprint(corrected["tuple"])
        other["cohort_fingerprint"] = corrected["fingerprint"]
        self.reject(report, "same physical conditions")

    def test_correction_gate_rejects_mixed_toolchain_or_locked_inputs(self):
        original_baseline = copy.deepcopy(self.baseline)
        mutations = [
            ("deployment target", lambda tuple_: tuple_.__setitem__("deployment_target", "17.0")),
            ("Xcode build", lambda tuple_: tuple_["xcode"].__setitem__("build", "different Xcode build")),
            ("SDK root", lambda tuple_: tuple_["sdk"].__setitem__("root", "different SDK root")),
            ("lockfile", lambda tuple_: tuple_["locks"].__setitem__("Cargo.lock", "f" * 64)),
            ("tool version", lambda tuple_: tuple_["tools"].__setitem__("vite", "different Vite version")),
        ]
        for condition, mutate in mutations:
            with self.subTest(condition=condition):
                self.baseline = copy.deepcopy(original_baseline)
                report, corrected = self.correction_fixture()
                tuple_ = corrected["tuple"]
                mutate(tuple_)
                if tuple_["deployment_target"] != "16.0":
                    corrected["build"]["generated_settings"]["deployment_target"] = tuple_["deployment_target"]
                    corrected["build"]["generated_settings"]["minimum_os_version"] = tuple_["deployment_target"]
                corrected["build"]["generated_settings"]["sdkroot"] = tuple_["sdk"]["root"]
                corrected["fingerprint"] = evidence.fingerprint(tuple_)
                for attempt in corrected["attempts"]:
                    attempt["cohort_fingerprint"] = corrected["fingerprint"]
                self.reject(report, "same physical conditions")

    def test_successful_correction_supports_continued_tauri_evaluation(self):
        report, corrected = self.correction_fixture()
        remove_attempts(corrected, *corrected["attempts"])
        passed = self.complete(corrected)
        report.update(status="pass", reasons=[], decision=passed["decision"])
        self.validate(report, "pass")

    def add_comparison(self, report, alternative):
        make_recipes(alternative, self.cases)
        compared = self.complete(alternative)
        report["cohorts"]["comparison"] = alternative
        report["decision"]["disposition"] = "review-comparison"
        report["decision"]["rationale"] = "Synthetic comparison restored the visible cursor in the affected layer."
        report["decision"]["supporting_attempt_ids"].extend(compared["decision"]["supporting_attempt_ids"])

    def test_comparison_uses_same_fixture_and_keeps_production_decisions_open(self):
        report, _ = self.correction_fixture()
        self.add_comparison(report, native_cohort())
        self.validate(report, "failing")
        report["decision"]["minimum_ios"] = "16.0"
        self.reject(report, "minimum iOS stay open")

    def test_comparison_requires_own_bound_provenance_and_exact_local_recorder(self):
        report, _ = self.correction_fixture()
        self.add_comparison(report, native_cohort())
        for path, value, reason in (
            (("build", "installation", "signature_verified"), False, "verified physical installation"),
            (("build", "generated_settings", "configuration"), "Debug", "generated Release SDK"),
            (("build", "generated_settings", "bundle_sha256"), "2" * 64, "settings lack signed source and bundle binding"),
            (("build", "entry_validation", "entry_ref"), "", "native entry and registration"),
            (("build", "entry_validation", "build_log_sha256"), "", "unbound.*entry validation"),
            (("build", "entry_validation", "source_revision"), "1" * 40, "unbound.*entry validation"),
            (("build", "entry_validation", "installed_bundle_sha256"), "2" * 64, "unbound.*entry validation"),
            (("boundary", "registration", "sha256"), "1" * 64, "registration.*signed build"),
            (("boundary", "registration", "handlers"),
             {"one": "begin", "two": "append", "three": "read", "other": "read"}, "exactly begin, append and read"),
            (("boundary", "webview", "allowed_origins"),
             ["fixture://local", "https://external.invalid"], "sole bundled origin"),
            (("boundary", "webview", "bundled_assets_only"), False, "sole bundled origin"),
            (("boundary", "dependencies", "ref"), "", "native package or dependency"),
            (("boundary", "no_host_provider_or_desktop_dependencies"), False, "dependency review"),
            (("boundary", "offline_launch", "startup_hex"), "0d", "bound offline startup"),
            (("boundary", "offline_launch", "operations"), list(evidence.COMMANDS), "actually begin, append and read"),
            (("ci", "native", "source_revision"), "1" * 40, "CI native"),
        ):
            with self.subTest(path=path):
                mutated = copy.deepcopy(report)
                record = mutated["cohorts"]["comparison"]
                for key in path[:-1]:
                    record = record[key]
                record[path[-1]] = value
                self.reject(mutated, reason)

    def test_comparison_rejects_changed_fixture_or_physical_conditions(self):
        for key, value, message in (
            ("sdk", {"version": "18.5", "build": "different SDK build", "root": "iPhoneOS.sdk"},
             "comparison must reuse the fixture and physical conditions"),
            ("device_alias", "different iPhone", "comparison must reuse the fixture and physical conditions"),
            ("fixture_sha256", "f" * 64, "cohort fixture digest"),
        ):
            with self.subTest(key=key):
                self.baseline = bound_cohort()
                make_recipes(self.baseline, self.cases)
                report, _ = self.correction_fixture()
                alternative = native_cohort()
                alternative["tuple"][key] = value
                alternative["fingerprint"] = evidence.fingerprint(alternative["tuple"])
                self.add_comparison(report, alternative)
                self.reject(report, message)

    def test_comparison_requires_shared_recipes_and_complete_matrix(self):
        report, _ = self.correction_fixture()
        alternative = native_cohort()
        self.add_comparison(report, alternative)
        recipe = self.baseline["recipes"].pop("composition-commit")
        self.reject(report, "baseline's frozen composition recipes")
        self.baseline["recipes"]["composition-commit"] = recipe
        attempt = alternative["attempts"][-1]
        remove_attempts(alternative, attempt)
        self.reject(report, "shell disposition must be comparison-eligible")

    def test_both_tauri_roles_require_exact_effective_acl(self):
        report, _ = self.correction_fixture()
        self.add_comparison(report, native_cohort())
        for role in ("baseline", "corrected"):
            with self.subTest(role=role):
                mutated = copy.deepcopy(report)
                mutated["cohorts"][role]["boundary"]["resolved_grants"] = {}
                self.reject(mutated, "effective iOS capability")


if __name__ == "__main__":
    unittest.main()
