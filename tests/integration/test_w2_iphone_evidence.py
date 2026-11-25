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
        "attempts": [],
    }


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


def add_attempt(cohort, case, suffix, route=None):
    case_id = case["id"]
    index = 2 * len(cohort["attempts"]) + 2
    identity = {"caseId": case_id, "attemptId": index, "generation": index}
    attempt = {
        "id": f"{cohort['tuple']['source_revision'][0]}-{case_id}-{suffix}", "case_id": case_id,
        "cohort_fingerprint": cohort["fingerprint"], "attemptId": index, "generation": index,
        "launch_id": "synthetic launch", "ref": "synthetic attempt", "status": "pass",
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
                               "delivery_uncertain": False, "old_generation_extra_event": False}
    cohort["attempts"].append(attempt)
    return attempt


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
        self.baseline["attempts"].clear()
        row = self.sample("app-switch")
        row["recovery"]["retained"]["hex"] = "6161"
        self.reject(report_for(self.baseline), "retained native bytes")

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
                self.baseline["attempts"] = [row for row in self.baseline["attempts"]
                                             if (row["case_id"], row.get("route")) != (case_id, route)]
                self.reject(complete, "report status must be inconclusive")
                self.baseline["attempts"].clear()

    def test_duplicate_attempt_ids_and_duplicate_native_runs(self):
        row = self.sample()
        duplicate = copy.deepcopy(row)
        self.baseline["attempts"].append(duplicate)
        self.reject(report_for(self.baseline), "duplicate measured attempt ID")
        duplicate["id"] += "-renamed"
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
        first, second = self.baseline["attempts"][:2]
        second.update(launch_id="synthetic relaunch", attemptId=first["attemptId"], generation=first["generation"])
        second["final"].update(attemptId=first["attemptId"], generation=first["generation"])
        self.validate(report, "pass")

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

    def correction_fixture(self):
        case = self.cases["english-return"]
        baseline_ids = []
        for tag in ("original", "repeat"):
            row = add_attempt(self.baseline, case, tag)
            row["observations"]["cursor_visible"] = False
            baseline_ids.append(mark(row, "failing"))
        corrected = bound_cohort("3", "4")
        make_recipes(corrected, self.cases)
        row = add_attempt(corrected, case, "corrected")
        row["observations"]["cursor_visible"] = False
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
        self.baseline["attempts"].pop()
        self.reject(report, "same unresolved measured failure twice")

    def test_correction_gate_rejects_comparison_without_corrected_failure(self):
        report, corrected = self.correction_fixture()
        corrected["attempts"].clear()
        report["cohorts"]["comparison"] = {}
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

    def test_correction_gate_rejects_a_different_failed_case_or_mixed_conditions(self):
        report, corrected = self.correction_fixture()
        corrected["attempts"].clear()
        other = add_attempt(corrected, self.cases["ukrainian-return"], "other")
        other["observations"]["cursor_visible"] = False
        mark(other, "failing")
        self.reject(report, "shell disposition must be mandatory-failure")
        corrected["tuple"]["device_alias"] = "other phone"
        corrected["fingerprint"] = evidence.fingerprint(corrected["tuple"])
        other["cohort_fingerprint"] = corrected["fingerprint"]
        self.reject(report, "same physical conditions")

    def test_successful_correction_supports_continued_tauri_evaluation(self):
        report, corrected = self.correction_fixture()
        corrected["attempts"].clear()
        passed = self.complete(corrected)
        report.update(status="pass", reasons=[], decision=passed["decision"])
        self.validate(report, "pass")

    def test_comparison_uses_same_fixture_and_keeps_production_decisions_open(self):
        report, _ = self.correction_fixture()
        alternative = bound_cohort("5", "6")
        alternative["tuple"]["shell"] = "synthetic comparison shell"
        alternative["fingerprint"] = evidence.fingerprint(alternative["tuple"])
        make_recipes(alternative, self.cases)
        compared = self.complete(alternative)
        report["cohorts"]["comparison"] = alternative
        report["decision"]["disposition"] = "review-comparison"
        report["decision"]["rationale"] = "Synthetic comparison restored the visible cursor in the affected layer."
        report["decision"]["supporting_attempt_ids"].extend(compared["decision"]["supporting_attempt_ids"])
        self.validate(report, "failing")
        report["decision"]["minimum_ios"] = "16.0"
        self.reject(report, "minimum iOS stay open")


if __name__ == "__main__":
    unittest.main()
