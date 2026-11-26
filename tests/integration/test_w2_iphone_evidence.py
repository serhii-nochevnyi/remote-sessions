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
            {"id": f"{cohort['tuple']['source_revision']}-{case_id}-rehearsal-{i}",
             "ref": f"synthetic capture {cohort['tuple']['source_revision']}/{case_id}/{i}",
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
    binding = {key: cohort["tuple"][key] for key in ("source_revision", "bundle_sha256")}
    launch = {"id": f"synthetic launch {order}", "order": order,
              "ref": f"synthetic launch capture {binding['source_revision']}/{binding['bundle_sha256']}/{order}",
              "binding": binding, "measurements": []}
    cohort["launches"].append(launch)
    return launch


def retain_measurement(cohort, attempt):
    launch = cohort["launches"][-1] if cohort["launches"] else start_launch(cohort)
    position = launch["measurements"][-1]["position"] + 1 if launch["measurements"] else 1
    attempt.update(launch_id=launch["id"], launch_ref=launch["ref"], measurement_position=position)
    attempt.setdefault("ref", f"{launch['ref']}/measurement/{position}")
    launch["measurements"].append({"attempt_id": attempt["id"], "position": position,
                                   "attemptId": attempt["attemptId"], "generation": attempt["generation"],
                                   "ref": attempt["ref"]})


def retained_record(cohort, attempt):
    return next(record for launch in cohort["launches"] for record in launch["measurements"]
                if record["attempt_id"] == attempt["id"])


def set_measurement_counters(cohort, attempt, **counters):
    attempt.update(counters)
    attempt["final"].update(counters)
    retained_record(cohort, attempt).update(counters)


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
        "status": "pass",
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
    measurement = retained_record(cohort, attempt)
    for key in ("attemptId", "generation"):
        attempt[key] = measurement[key] = restart["previous"][key]
    attempt.pop("final")
    attempt["observations"]["started_fresh"] = False
    return mark(attempt, "inconclusive" if outcome == "uncertain" else "failing")


def set_old_recovery_snapshots(attempt, before_hex, retained_hex, *, capture_error):
    recovery = attempt["recovery"]
    restart = recovery["restart"]
    recovery.update(before=native(restart["previous"], before_hex),
                    retained=native(restart["previous"], retained_hex))
    restart["action_completed"] = False
    attempt["capture_error"] = capture_error
    attempt["observations"]["prompt_visible"] = True


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

    def test_exact_final_bytes_after_append_timeout_cannot_support_a_clean_matrix(self):
        report = self.complete(self.baseline)
        row = next(row for row in self.baseline["attempts"] if row["case_id"] == "composition-commit")
        row["checkpoint"]["eventLengths"] = []
        row["final"]["eventLengths"] = [len(row["final"]["hex"]) // 2]
        self.validate(report, "pass")

        receipt = next(evt for evt in row["trace"]["events"] if evt["event"] == "append acknowledgment")
        receipt["event"] = "append delivery unconfirmed"
        self.reject(report, "native append delivery is uncertain")

        mark(row, "inconclusive")
        self.reject(report, "report status must be inconclusive")
        report["status"] = "inconclusive"
        report["reasons"] = ["Composition append receipt missed its deadline"]
        self.reject(report, "shell disposition must be inconclusive")
        report["decision"]["disposition"] = "inconclusive"
        report["decision"]["supporting_attempt_ids"] = [row["id"]]
        self.reject(report, "disposition must reference the attempts that establish it")
        report["decision"]["supporting_attempt_ids"] = []
        self.validate(report, "inconclusive")

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

    def test_fresh_recovery_cannot_reuse_a_bound_before_identity(self):
        case = self.cases["app-switch"]
        first = self.sample(case["id"])
        add_attempt(self.baseline, case, "fresh")
        report = report_for(self.baseline, "inconclusive", "inconclusive", [])
        self.validate(report, "inconclusive")
        native_id, generation = first["attemptId"], first["generation"]
        for counters in ((native_id, generation), (native_id, generation + 1), (native_id + 1, generation)):
            with self.subTest(old_counters=counters):
                mutated = copy.deepcopy(report)
                cohort = mutated["cohorts"]["baseline"]
                reused, fresh = cohort["attempts"]
                recovery = fresh["recovery"]
                for record in (recovery["before"], recovery["retained"], recovery["restart"]["previous"]):
                    record.update(attemptId=counters[0], generation=counters[1])
                self.reject(mutated, f"{reused['id']}: expected inconclusive .*is reused")
                mark(reused, "inconclusive")
                self.reject(mutated, f"{fresh['id']}: expected inconclusive .*is reused")
                mark(fresh, "inconclusive")
                self.validate(report_for(cohort, "inconclusive", "inconclusive", []), "inconclusive")

    def test_fresh_recovery_requires_greater_generation_and_exact_input(self):
        row = self.sample("lock-foreground")
        report = report_for(self.baseline, "inconclusive", "inconclusive", [])
        self.validate(report, "inconclusive")
        row["final"]["hex"] = "61"
        self.reject(report, "final bytes differ")
        row["final"]["hex"] = self.cases["lock-foreground"]["expected"]["afterHex"]
        row["recovery"]["restart"]["observed"]["generation"] -= 1
        self.reject(report, "fresh attempt and generation")

    def test_plausible_old_counters_need_matching_recovery_evidence_before_counting_as_reuse(self):
        first = self.sample("app-switch")
        second = add_attempt(self.baseline, self.cases["app-switch"], "second")
        complete = report_for(self.baseline, "inconclusive", "inconclusive", [])
        for defect, reason in (
            ("case", "lacks the old native attempt"),
            ("retained", "different attempt or generation"),
            ("previous", "not bound to the old attempt"),
            ("action", "missing completed foreground restart action"),
            ("observed", "did not identify a fresh attempt and generation"),
            ("visible", "conflicts with the visible fresh-start observation"),
        ):
            with self.subTest(defect=defect):
                report = copy.deepcopy(complete)
                row = report["cohorts"]["baseline"]["attempts"][1]
                recovery = row["recovery"]
                restart = recovery["restart"]
                for record in (recovery["before"], recovery["retained"], restart["previous"]):
                    record["attemptId"] = first["attemptId"]
                if defect == "case":
                    recovery["before"]["caseId"] = "lock-foreground"
                elif defect in ("retained", "previous"):
                    proof = recovery["retained"] if defect == "retained" else restart["previous"]
                    proof["attemptId"] += 1
                elif defect == "action":
                    restart["action_completed"] = False
                elif defect == "observed":
                    restart["observed"]["generation"] -= 1
                else:
                    row["observations"].pop("started_fresh")
                self.reject(report, f"{second['id']}: expected inconclusive .*{reason}")
                mark(row, "inconclusive")
                self.validate(report, "inconclusive")

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
        attempts.append(attempts.pop(0))
        self.validate(report, "pass")

        early.update(launch_ref="conflicting capture", attemptId=later[0]["attemptId"],
                     generation=later[0]["generation"], recovery=copy.deepcopy(later[0]["recovery"]))
        early["recovery"]["before"].update(attemptId=later[1]["attemptId"], generation=later[1]["generation"])
        self.validate(report, "pass")
        early["status"] = "pass"
        self.reject(report, "does not match its retained launch and measurement")
        mark(early, "inconclusive")
        support[support.index(later[0]["id"])] = early["id"]
        self.reject(report, "disposition must reference the attempts that establish it")

    def test_conflicting_old_recovery_snapshot_cannot_poison_later_bound_measurements(self):
        for case_id in sorted(evidence.RECOVERY):
            cohort = bound_cohort()
            make_recipes(cohort, self.cases)
            early = add_attempt(cohort, self.cases[case_id], "early")
            complete = self.complete(cohort)
            same_case = [row for row in cohort["attempts"][1:] if row["case_id"] == case_id]
            unrelated = [row for row in cohort["attempts"] if row["case_id"] == "english-return"]
            self.assertEqual(len(same_case), 2)
            self.assertEqual(len(unrelated), 2)
            for defect, counters, grade, reason in (
                ("wrong case", {"attemptId": same_case[0]["attemptId"]}, "inconclusive", "lacks the old native attempt"),
                ("conflicting retained", {"generation": unrelated[0]["generation"]}, "inconclusive",
                 "different attempt or generation"),
                ("conflicting previous", {"attemptId": same_case[-1]["attemptId"] + 10}, "inconclusive",
                 "not bound to the old attempt"),
                ("non-increasing fresh", {"generation": same_case[-1]["generation"] + 10}, "inconclusive",
                 "fresh attempt and generation"),
                ("mutation with conflicting previous",
                 {"attemptId": same_case[0]["attemptId"], "generation": unrelated[0]["generation"]},
                 "failing", "retained native bytes or events changed"),
                ("initial mismatch with later generation", {"generation": same_case[-1]["generation"] + 10},
                 "failing", "initial bytes differ from the manifest"),
            ):
                with self.subTest(case=case_id, defect=defect):
                    report = copy.deepcopy(complete)
                    invalid = report["cohorts"]["baseline"]["attempts"][0]
                    recovery = invalid["recovery"]
                    restart = recovery["restart"]
                    if defect == "mutation with conflicting previous":
                        set_old_recovery_snapshots(invalid, "78", "79", capture_error=True)
                    elif defect == "initial mismatch with later generation":
                        set_old_recovery_snapshots(invalid, "78", "78", capture_error=False)
                    recovery["before"].update(counters)
                    if defect == "wrong case":
                        recovery["before"]["caseId"] = "english-return"
                    if defect != "conflicting retained":
                        recovery["retained"].update(counters)
                    if defect not in ("conflicting previous", "mutation with conflicting previous"):
                        restart["previous"].update(counters)

                    self.reject(report, f"{early['id']}: expected {grade} .*{reason}")
                    mark(invalid, grade)
                    support = report["decision"]["supporting_attempt_ids"]
                    support.remove(early["id"])
                    self.validate(report, "pass")
                    for later in (*same_case, *unrelated):
                        with self.subTest(missing_support=later["id"]):
                            report["decision"]["supporting_attempt_ids"] = [key for key in support if key != later["id"]]
                            self.reject(report, "disposition must reference the attempts that establish it")
                    report["decision"]["supporting_attempt_ids"] = [*support, early["id"]]
                    self.reject(report, "disposition must reference the attempts that establish it")

    def test_bound_recovery_primary_remains_in_chronology_when_old_snapshot_conflicts(self):
        early = self.sample("app-switch")
        report = self.complete(self.baseline)
        following = self.baseline["attempts"][1]
        early["recovery"]["before"]["caseId"] = following["case_id"]
        mark(early, "inconclusive")
        report["decision"]["supporting_attempt_ids"].remove(early["id"])
        self.validate(report, "pass")

        measurement = self.baseline["launches"][0]["measurements"][0]
        for value, reason in ((following["generation"], "is reused"),
                              (following["generation"] + 1, "is out of order")):
            with self.subTest(primary_generation=value):
                early["generation"] = measurement["generation"] = value
                early["recovery"]["restart"]["observed"]["generation"] = early["final"]["generation"] = value
                self.reject(report, f"{following['id']}: expected inconclusive .*{reason}")

    def test_partially_bound_retained_counters_disqualify_later_passes(self):
        case = self.cases["english-return"]
        for native_id, generation, diagnostic in (
            (4, 4, "is reused"),
            (4, 2, "is reused"),
            (2, 4, "is reused"),
            (5, 2, "is out of order"),
            (2, 5, "is out of order"),
        ):
            with self.subTest(attemptId=native_id, generation=generation):
                cohort = bound_cohort()
                make_recipes(cohort, self.cases)
                early = add_attempt(cohort, case, "partial")
                report = self.complete(cohort)
                reused, fresh = [row for row in cohort["attempts"][1:] if row["case_id"] == case["id"]]
                cohort["launches"][0]["measurements"][0].update(attemptId=native_id, generation=generation)
                early.pop("launch_ref")
                mark(early, "inconclusive")
                report["decision"]["supporting_attempt_ids"].remove(early["id"])
                self.reject(report, f"{reused['id']}: expected inconclusive .*{diagnostic}")

                mark(reused, "inconclusive")
                self.reject(report, "report status must be inconclusive")
                held = report_for(cohort, "inconclusive", "inconclusive", [])
                self.validate(held, "inconclusive")
                self.reject(report_for(cohort, "inconclusive", "inconclusive", [reused["id"], fresh["id"]]),
                            "disposition must reference the attempts that establish it")

    def test_reused_partial_identity_cannot_support_failure_reproduction(self):
        case = self.cases["english-return"]
        early = self.sample(case["id"])
        reused = add_attempt(self.baseline, case, "reused")
        fresh = add_attempt(self.baseline, case, "fresh")
        self.baseline["launches"][0]["measurements"][0].update(attemptId=reused["attemptId"],
                                                              generation=reused["generation"])
        early.pop("launch_ref")
        mark(early, "inconclusive")
        for row in (reused, fresh):
            row["observations"]["cursor_visible"] = False
            mark(row, "failing")
        report = report_for(self.baseline, "failing", "mandatory-failure", [reused["id"], fresh["id"]])
        self.reject(report, f"{reused['id']}: expected inconclusive .*is reused")
        mark(reused, "inconclusive")
        report["decision"]["supporting_attempt_ids"].remove(reused["id"])
        self.validate(report, "failing")
        report["correction"] = {"case_id": case["id"], "layer": "synthetic viewport layer",
                                "change": "synthetic scoped correction", "scope": ["synthetic source path"],
                                "baseline_attempt_ids": [reused["id"], fresh["id"]]}
        self.reject(report, "correction requires the same unresolved measured failure twice")

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
        for ref in (None, "different observation"):
            with self.subTest(retained_ref=ref):
                record = retained_record(self.baseline, row)
                record.pop("ref")
                if ref is not None:
                    record["ref"] = ref
                self.reject(report, "retained measurement observation reference")
                mark(row, "inconclusive")
                self.validate(report_for(self.baseline, "inconclusive", "inconclusive", []), "inconclusive")
                record["ref"] = row["ref"]
                mark(row, "pass")
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

    def correction_fixture(self, case_id="english-return", failed_observation="cursor_visible", *, route=None):
        case = self.cases[case_id]
        baseline_ids = []
        for tag in ("original", "repeat"):
            row = add_attempt(self.baseline, case, tag, route)
            if failed_observation is None:
                row["start_empty"] = False
            else:
                row["observations"][failed_observation] = False
            baseline_ids.append(mark(row, "failing"))
        corrected = bound_cohort("3", "4")
        corrected["tuple"]["app"]["version"] = "0.1.1"
        corrected["tuple"]["app"]["build"] = "2"
        corrected["build"]["installation"]["app"] = copy.deepcopy(corrected["tuple"]["app"])
        corrected["fingerprint"] = evidence.fingerprint(corrected["tuple"])
        make_recipes(corrected, self.cases)
        row = add_attempt(corrected, case, "corrected", route)
        if failed_observation is None:
            row["start_empty"] = False
        else:
            row["observations"][failed_observation] = False
        corrected_id = mark(row, "failing")
        report = report_for(self.baseline, "failing", "comparison-eligible", [*baseline_ids, corrected_id])
        report["cohorts"]["corrected"] = corrected
        report["correction"] = {"case_id": case["id"], "layer": "synthetic viewport layer",
                                "change": "synthetic scoped correction", "scope": ["synthetic source path"],
                                "baseline_attempt_ids": baseline_ids}
        return report, corrected

    def test_correction_rejects_the_same_measurement_under_relabeling(self):
        original, corrected = self.correction_fixture()
        first = self.baseline["attempts"][0]
        independent = corrected["attempts"][0]
        for key in ("attemptId", "generation"):
            self.assertEqual(first[key], independent[key])
        self.validate(original, "failing")

        for relabel in ("none", "report ID", "cohort fingerprint", "launch label", "distinct launch"):
            with self.subTest(relabel=relabel):
                report = copy.deepcopy(original)
                before, after = (report["cohorts"][role] for role in ("baseline", "corrected"))
                first = before["attempts"][0]
                row = after["attempts"][0]
                launch = after["launches"][0]
                record = retained_record(after, row)
                record["ref"] = row["ref"] = first["ref"]
                if relabel != "distinct launch":
                    launch["ref"] = row["launch_ref"] = first["launch_ref"]
                if relabel == "report ID":
                    record["attempt_id"] = row["id"] = "relabeled corrected observation"
                    report["decision"]["supporting_attempt_ids"][-1] = row["id"]
                if relabel == "cohort fingerprint":
                    after["tuple"]["app"]["build"] = "3"
                    after["build"]["installation"]["app"]["build"] = "3"
                    after["fingerprint"] = row["cohort_fingerprint"] = evidence.fingerprint(after["tuple"])
                    for recipe in after["recipes"].values():
                        for rehearsal in recipe["rehearsals"]:
                            rehearsal["cohort_fingerprint"] = after["fingerprint"]
                if relabel == "launch label":
                    launch["id"] = row["launch_id"] = "relabeled corrected launch"
                diagnostic = "shared measured observation" if relabel == "distinct launch" else "shared launch recording"
                self.reject(report, f"{row['id']}: expected inconclusive .*{diagnostic}")
                self.assert_correction_is_incomplete(report, row)

    def assert_correction_is_incomplete(self, report, held):
        baseline_ids = report["correction"]["baseline_attempt_ids"]
        mark(held, "inconclusive")
        self.reject(report, "report status must be inconclusive")
        forged = copy.deepcopy(report)
        self.add_comparison(forged, native_cohort())
        self.reject(forged, "persistent corrected-build mandatory failure")
        report["status"] = "inconclusive"
        report["decision"].update(disposition="inconclusive", supporting_attempt_ids=baseline_ids.copy())
        self.validate(report, "inconclusive")
        report["decision"]["supporting_attempt_ids"].append(held["id"])
        self.reject(report, "disposition must reference the attempts that establish it")

    def assert_only_first_baseline_failure(self, report, first, held):
        mark(held, "inconclusive")
        self.reject(report, "same unresolved measured failure twice on the baseline")
        baseline = report["cohorts"]["baseline"]
        honest = report_for(baseline, "failing", "mandatory-failure", [first["id"]])
        self.validate(honest, "failing")
        honest["decision"]["supporting_attempt_ids"].append(held["id"])
        self.reject(honest, "disposition must reference the attempts that establish it")

    def test_shared_measured_recording_requires_independent_windows_in_every_signed_build(self):
        original, _ = self.correction_fixture()
        self.validate(original, "failing")
        for role in ("baseline", "corrected"):
            for defect, start, end in (
                ("adjacent", 100, 200), ("disjoint", 120, 200), ("identical", 0, 100),
                ("overlapping", 99, 200), ("missing earlier", 100, 200), ("missing later", None, None),
                ("both missing", None, None),
                ("negative", -1, 100), ("boolean", False, 100), ("fractional", 100, 200.5),
                ("empty", 100, 100), ("reversed", 200, 100),
            ):
                with self.subTest(role=role, segment=defect):
                    report = copy.deepcopy(original)
                    before = report["cohorts"]["baseline"]
                    after = report["cohorts"][role]
                    first, row = before["attempts"][0], after["attempts"][-1]
                    earlier, later = retained_record(before, first), retained_record(after, row)
                    if role == "baseline":
                        later["attempt_id"] = row["id"] = "relabeled baseline observation"
                        later["position"] = row["measurement_position"] = 7
                        set_measurement_counters(after, row, attemptId=10, generation=11)
                        report["correction"]["baseline_attempt_ids"][1] = row["id"]
                        report["decision"]["supporting_attempt_ids"][1] = row["id"]
                    if role == "corrected":
                        before["launches"][0]["capture_segment"] = {"start_ms": 0, "end_ms": 300}
                        after["launches"][0].update(ref=first["launch_ref"],
                                                   capture_segment={"start_ms": 300, "end_ms": 600})
                        row["launch_ref"] = first["launch_ref"]
                    later["ref"] = row["ref"] = first["ref"]
                    if defect not in ("missing earlier", "both missing"):
                        earlier["capture_segment"] = {"start_ms": 0, "end_ms": 100}
                    if defect not in ("missing later", "both missing"):
                        later["capture_segment"] = {"start_ms": start, "end_ms": end}
                    if defect in ("adjacent", "disjoint"):
                        self.validate(report, "failing")
                    else:
                        diagnostic = ("shared measured observation" if defect in
                                      ("identical", "overlapping", "missing earlier", "missing later", "both missing")
                                      else "retained measurement capture segment")
                        self.reject(report, f"{row['id']}: expected inconclusive .*{diagnostic}")
                        if role == "baseline":
                            self.assert_only_first_baseline_failure(report, first, row)
                        else:
                            self.assert_correction_is_incomplete(report, row)

    def test_shared_launch_recording_requires_independent_windows_across_builds(self):
        original, _ = self.correction_fixture()
        for equal_counter in ("attemptId", "generation"):
            for defect in ("missing", "identical", "overlapping", "malformed", "adjacent"):
                with self.subTest(equal_counter=equal_counter, launch_segment=defect):
                    report = copy.deepcopy(original)
                    before, after = (report["cohorts"][role] for role in ("baseline", "corrected"))
                    first, row = before["attempts"][0], after["attempts"][0]
                    changed_counter = "generation" if equal_counter == "attemptId" else "attemptId"
                    set_measurement_counters(after, row, **{changed_counter: 6})
                    self.assertEqual(first[equal_counter], row[equal_counter])
                    self.assertNotEqual(first["ref"], row["ref"])
                    self.validate(report, "failing")

                    before["launches"][0]["capture_segment"] = {"start_ms": 0, "end_ms": 100}
                    launch = after["launches"][0]
                    launch["ref"] = row["launch_ref"] = first["launch_ref"]
                    if defect != "missing":
                        start = {"identical": 0, "overlapping": 99, "malformed": True, "adjacent": 100}[defect]
                        launch["capture_segment"] = {"start_ms": start, "end_ms": 100 if defect == "identical" else 200}
                    if defect == "adjacent":
                        self.validate(report, "failing")
                    else:
                        diagnostic = "retained launch capture segment" if defect == "malformed" else "shared launch recording"
                        self.reject(report, f"{row['id']}: expected inconclusive .*{diagnostic}")
                        self.assert_correction_is_incomplete(report, row)

    def test_cross_build_recording_windows_apply_across_launch_and_measurement_refs(self):
        original, _ = self.correction_fixture()
        for donor_role, recipient_role in (
            ("baseline", "corrected"), ("baseline", "comparison"), ("corrected", "comparison"),
        ):
            for earlier_is_launch in (True, False):
                for segment in ("missing", "overlapping", "adjacent"):
                    with self.subTest(donor=donor_role, recipient=recipient_role,
                                      earlier_is_launch=earlier_is_launch, segment=segment):
                        report = copy.deepcopy(original)
                        correction_support = report["decision"]["supporting_attempt_ids"].copy()
                        recipient = report["cohorts"]["corrected"]
                        if recipient_role == "comparison":
                            recipient = native_cohort()
                            self.add_comparison(report, recipient)
                        row = recipient["attempts"][-1]
                        launch = recipient["launches"][-1]
                        if recipient_role == "comparison" and not earlier_is_launch:
                            remove_attempts(recipient, row)
                            launch = start_launch(recipient)
                            retain_measurement(recipient, row)
                            recipient["attempts"].append(row)
                        donor = report["cohorts"][donor_role]
                        if earlier_is_launch:
                            earlier = donor["launches"][0]
                            later, field = retained_record(recipient, row), "ref"
                        else:
                            earlier = retained_record(donor, donor["attempts"][0])
                            later, field = launch, "launch_ref"
                        later["ref"] = row[field] = earlier["ref"]
                        if segment != "missing":
                            earlier["capture_segment"] = {"start_ms": 0, "end_ms": 100}
                            later["capture_segment"] = {
                                "start_ms": 100 if segment == "adjacent" else 99, "end_ms": 200,
                            }
                        if segment == "adjacent":
                            self.validate(report, "failing")
                        else:
                            self.reject(report, f"{row['id']}: expected inconclusive .*shared launch and measurement")
                            if recipient_role == "corrected":
                                self.assert_correction_is_incomplete(report, row)
                            else:
                                mark(row, "inconclusive")
                                self.assert_comparison_uses_only_correction(report, correction_support)

    def test_same_signed_launch_context_can_overlap_its_distinct_measurement_windows(self):
        report, _ = self.correction_fixture()
        launch = self.baseline["launches"][0]
        launch["capture_segment"] = {"start_ms": 0, "end_ms": 200}
        for index, row in enumerate(self.baseline["attempts"]):
            row["ref"] = launch["ref"]
            retained_record(self.baseline, row).update(
                ref=launch["ref"], capture_segment={"start_ms": index * 100, "end_ms": (index + 1) * 100},
            )
        self.validate(report, "failing")

    def test_each_cohort_holds_missing_or_wrong_retained_source_and_bundle(self):
        original, corrected = self.correction_fixture()
        first, second = self.baseline["attempts"]
        remove_attempts(self.baseline, second)
        start_launch(self.baseline)
        retain_measurement(self.baseline, second)
        self.baseline["attempts"].append(second)
        set_measurement_counters(self.baseline, second, attemptId=first["attemptId"])
        support = original["decision"]["supporting_attempt_ids"].copy()
        self.add_comparison(original, native_cohort())
        comparison = original["cohorts"]["comparison"]
        last = comparison["attempts"][-1]
        remove_attempts(comparison, last)
        start_launch(comparison)
        retain_measurement(comparison, last)
        comparison["attempts"].append(last)
        self.validate(original, "failing")

        for role in ("baseline", "corrected", "comparison"):
            for key in ("binding", "source_revision", "bundle_sha256"):
                for defect in ("missing", "foreign"):
                    with self.subTest(role=role, binding_key=key, defect=defect):
                        report = copy.deepcopy(original)
                        cohort = report["cohorts"][role]
                        launch, row = cohort["launches"][-1], cohort["attempts"][-1]
                        foreign = self.baseline if role != "baseline" else corrected
                        if key == "binding":
                            if defect == "missing":
                                launch.pop("binding")
                            else:
                                launch["binding"] = foreign["launches"][0]["binding"].copy()
                                if role == "baseline":
                                    launch["ref"] = row["launch_ref"] = foreign["launches"][0]["ref"]
                                    ref = foreign["attempts"][0]["ref"]
                                    retained_record(cohort, row)["ref"] = row["ref"] = ref
                        elif defect == "missing":
                            launch["binding"].pop(key)
                        else:
                            launch["binding"][key] = foreign["tuple"][key]
                        self.reject(report, f"{row['id']}: expected inconclusive .*retained launch signed binding")
                        if role == "baseline":
                            self.assert_only_first_baseline_failure(report, first, row)
                        elif role == "corrected":
                            del report["cohorts"]["comparison"]
                            report["decision"].update(disposition="comparison-eligible", supporting_attempt_ids=support.copy())
                            self.assert_correction_is_incomplete(report, row)
                        else:
                            mark(row, "inconclusive")
                            self.assert_comparison_uses_only_correction(report, support)

    def test_unknown_trailing_conditions_hold_baseline_and_corrected_support(self):
        for case_id, failure, route, changes in (
            ("english-return", "cursor_visible", None,
             (("keyboard_language", None), ("keyboard_language", "uk"), ("orientation", None),
              ("orientation", "landscape"))),
            ("paste-short", "focused_prompt", "sample",
             (("route", None), ("route", "unknown"), ("route", "native"))),
            ("composition-commit", None, None,
             (("recipe_id", None), ("recipe_id", "unqualified gesture"))),
        ):
            self.setUp()
            original, _ = self.correction_fixture(case_id, failure, route=route)
            self.validate(original, "failing")
            for role in ("baseline", "corrected"):
                for positioned in (True, False):
                    for key, value in changes:
                        with self.subTest(case=case_id, role=role, positioned=positioned, condition=key, value=value):
                            report = copy.deepcopy(original)
                            cohort = report["cohorts"][role]
                            hold = add_attempt(cohort, self.cases[case_id], "trailing-hold", route)
                            hold["capture_error"] = True
                            if value is None:
                                hold.pop(key)
                            else:
                                hold[key] = value
                            if not positioned:
                                cohort["launches"][-1]["measurements"].pop()
                            mark(hold, "inconclusive")
                            cohort["attempts"].insert(0, cohort["attempts"].pop())
                            support = report["decision"]["supporting_attempt_ids"].copy()
                            if role == "baseline":
                                self.reject(report, "same unresolved measured failure twice on the baseline")
                                honest = report_for(cohort, "failing", "mandatory-failure", support[:2])
                            else:
                                self.reject(report, "shell disposition must be mandatory-failure")
                                self.add_comparison(report, native_cohort())
                                self.reject(report, "persistent corrected-build mandatory failure")
                                del report["cohorts"]["comparison"]
                                report["decision"].update(disposition="mandatory-failure", supporting_attempt_ids=support)
                                honest = report
                            self.validate(honest, "failing")
                            honest["decision"]["supporting_attempt_ids"].append(hold["id"])
                            self.reject(honest, "disposition must reference the attempts that establish it")

    def test_only_fresh_independent_failures_support_correction_after_historical_holds(self):
        report, _ = self.correction_fixture()
        case = self.cases["english-return"]
        stale = report["decision"]["supporting_attempt_ids"].copy()
        fresh = {}
        for role, cohort in report["cohorts"].items():
            hold = add_attempt(cohort, case, "historical-hold")
            hold.pop("launch_id")
            hold.pop("keyboard_language")
            mark(hold, "inconclusive")
            fresh[role] = []
            for index in range(2 if role == "baseline" else 1):
                failure = add_attempt(cohort, case, f"fresh-{index}")
                failure["observations"]["cursor_visible"] = False
                fresh[role].append(mark(failure, "failing"))
        report["correction"]["baseline_attempt_ids"] = fresh["baseline"]
        support = [*fresh["baseline"], *fresh["corrected"]]
        report["decision"]["supporting_attempt_ids"] = support.copy()
        self.validate(report, "failing")
        report["correction"]["baseline_attempt_ids"] = [stale[0], fresh["baseline"][1]]
        self.reject(report, "same unresolved measured failure twice on the baseline")
        report["correction"]["baseline_attempt_ids"] = fresh["baseline"]
        report["decision"]["supporting_attempt_ids"][-1] = stale[-1]
        self.reject(report, "disposition must reference the attempts that establish it")

    def test_only_a_retained_verified_different_paste_route_can_exclude_a_hold(self):
        report, _ = self.correction_fixture("paste-short", "focused_prompt", route="sample")
        for role, cohort in report["cohorts"].items():
            hold = add_attempt(cohort, self.cases["paste-short"], "native-hold", "native")
            hold["capture_error"] = True
            mark(hold, "inconclusive")
            self.validate(report, "failing")
            record = cohort["launches"][-1]["measurements"].pop()
            diagnostic = ("same unresolved measured failure twice on the baseline" if role == "baseline"
                          else "shell disposition must be mandatory-failure")
            self.reject(report, diagnostic)
            cohort["launches"][-1]["measurements"].append(record)
        self.validate(report, "failing")

    def test_a_measured_composition_failure_needs_the_verified_procedure_for_reproduction(self):
        report, corrected = self.correction_fixture("composition-commit", None)
        self.validate(report, "failing")
        persistent = corrected["attempts"][0]
        persistent.pop("checkpoint")
        self.reject(report, "shell disposition must be mandatory-failure")
        report["decision"]["disposition"] = "mandatory-failure"
        self.validate(report, "failing")
        self.add_comparison(report, native_cohort())
        self.reject(report, "persistent corrected-build mandatory failure")

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

    def test_unconfirmed_typing_actions_cannot_support_reproduction_or_persistence(self):
        for case_id, wrong_hex, actions in (
            ("english-return", "6f6b", ("return_pressed_once",)),
            ("ukrainian-return", "d197", ("ukrainian_key_pressed_once", "return_pressed_once")),
        ):
            self.baseline = bound_cohort()
            make_recipes(self.baseline, self.cases)
            confirmed, _ = self.correction_fixture(case_id)
            for cohort in confirmed["cohorts"].values():
                for row in cohort["attempts"]:
                    row["observations"]["cursor_visible"] = True
                    row["final"]["hex"] = wrong_hex
            self.validate(confirmed, "failing")

            for action in actions:
                for missing in (False, True):
                    for role in ("baseline", "corrected"):
                        with self.subTest(case=case_id, action=action, missing=missing, role=role):
                            report = copy.deepcopy(confirmed)
                            rows = report["cohorts"][role]["attempts"]
                            for row in rows:
                                if missing:
                                    del row["observations"][action]
                                else:
                                    row["observations"][action] = False
                            self.reject(report, f"expected inconclusive.*unconfirmed prescribed typing action: {action}")
                            for row in rows:
                                mark(row, "inconclusive")

                            if role == "baseline":
                                self.reject(report, "same unresolved measured failure twice")
                                del report["cohorts"]["corrected"], report["correction"]
                                support = []
                            else:
                                self.add_comparison(report, native_cohort())
                                self.reject(report, "persistent corrected-build mandatory failure")
                                del report["cohorts"]["comparison"]
                                support = report["correction"]["baseline_attempt_ids"]
                            report.update(status="inconclusive")
                            report["decision"].update(disposition="inconclusive", supporting_attempt_ids=support)
                            self.validate(report, "inconclusive")
                            report["decision"]["supporting_attempt_ids"] = [*support, *(row["id"] for row in rows)]
                            self.reject(report, "disposition must reference the attempts that establish it")

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

    def old_recovery_correction(self, case_id, before_hex, retained_hex, *, separate_launches=False):
        self.baseline = bound_cohort()
        make_recipes(self.baseline, self.cases)
        report, corrected = self.correction_fixture(case_id, "prompt_visible")
        case = self.cases[case_id]
        if separate_launches:
            remove_attempts(self.baseline, self.baseline["attempts"][1])
            start_launch(self.baseline)
            add_attempt(self.baseline, case, "repeat")
        remove_attempts(corrected, corrected["attempts"][0])
        for suffix in ("prior-one", "prior-two"):
            add_attempt(corrected, self.cases["english-return"], suffix)
        persistent = add_attempt(corrected, case, "corrected")
        for row in (*self.baseline["attempts"], persistent):
            set_old_recovery_snapshots(row, before_hex, retained_hex, capture_error=before_hex != retained_hex)
            mark(row, "failing")
        return report

    def test_shared_old_recovery_observation_cannot_authorize_comparison(self):
        for case_id, before_hex, retained_hex, detail in (
            ("app-switch", "78", "79", "retained native bytes or events changed"),
            ("lock-foreground", "78", "78", "initial bytes differ from the manifest"),
        ):
            with self.subTest(case=case_id):
                independent = self.old_recovery_correction(case_id, before_hex, retained_hex)
                self.validate(independent, "failing")
                for shared_counters in (("attemptId", "generation"), ("attemptId",), ("generation",)):
                    with self.subTest(shared_counters=shared_counters):
                        report = copy.deepcopy(independent)
                        cohort = report["cohorts"]["baseline"]
                        first, second = cohort["attempts"]
                        old = first["recovery"]["before"]
                        recovery = second["recovery"]
                        for snapshot in (recovery["before"], recovery["retained"], recovery["restart"]["previous"]):
                            snapshot.update({key: old[key] for key in shared_counters})

                        honest = report_for(cohort, "failing", "mandatory-failure", [first["id"], second["id"]])
                        for row in (first, second):
                            mark(row, "inconclusive")
                            self.reject(honest, f"{row['id']}: expected failing .*{detail}")
                            mark(row, "failing")
                        self.validate(honest, "failing")

                        candidate = copy.deepcopy(report)
                        self.add_comparison(candidate, native_cohort())
                        for claim in (report, candidate):
                            with self.subTest(disposition=claim["decision"]["disposition"]):
                                self.reject(claim, "same unresolved measured failure twice on the baseline")

    def test_independent_old_recovery_observations_require_a_matching_corrected_failure(self):
        for case_id, before_hex, retained_hex in (
            ("app-switch", "78", "79"),
            ("lock-foreground", "78", "78"),
        ):
            for separate_launches in (False, True):
                with self.subTest(case=case_id, separate_launches=separate_launches):
                    report = self.old_recovery_correction(
                        case_id, before_hex, retained_hex, separate_launches=separate_launches,
                    )
                    self.validate(report, "failing")
                    baseline_ids = report["correction"]["baseline_attempt_ids"]
                    for corrected_failure in ("missing", "different"):
                        with self.subTest(corrected_failure=corrected_failure):
                            incomplete = copy.deepcopy(report)
                            corrected = incomplete["cohorts"]["corrected"]
                            failure = corrected["attempts"][-1]
                            if corrected_failure == "missing":
                                remove_attempts(corrected, failure)
                                incomplete["status"] = "inconclusive"
                                disposition, support = "inconclusive", baseline_ids.copy()
                            else:
                                set_old_recovery_snapshots(
                                    failure, "7a", "7b" if before_hex != retained_hex else "7a",
                                    capture_error=before_hex != retained_hex,
                                )
                                disposition, support = "mandatory-failure", [*baseline_ids, failure["id"]]
                            incomplete["decision"].update(disposition=disposition, supporting_attempt_ids=support)
                            self.validate(incomplete, incomplete["status"])
                            self.add_comparison(incomplete, native_cohort())
                            self.reject(incomplete, "persistent corrected-build mandatory failure")

    def test_old_recovery_reproduction_requires_the_latest_unresolved_failure_signature(self):
        case_id = "app-switch"
        report = self.old_recovery_correction(case_id, "78", "79")
        latest = add_attempt(self.baseline, self.cases[case_id], "latest")
        set_old_recovery_snapshots(latest, "78", "79", capture_error=True)
        mark(latest, "failing")
        self.validate(report, "failing")
        latest["recovery"]["retained"]["hex"] = "7a"
        self.reject(report, "same unresolved measured failure twice on the baseline")

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

    def test_unconfirmed_typing_action_preserves_independent_measured_failures(self):
        for failure, expected in (
            ("cursor_visible", "visible observation failed: cursor_visible"),
            ("start_empty", "unexpected startup input"),
            ("replayed", "replayed input"),
        ):
            with self.subTest(failure=failure):
                row = self.sample("english-return")
                row["observations"]["return_pressed_once"] = False
                row["final"]["hex"] = "6f6b"
                if failure == "cursor_visible":
                    row["observations"][failure] = False
                else:
                    row[failure] = failure == "replayed"
                self.reject(report_for(self.baseline), f"expected failing.*{expected}")
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

    def test_comparison_requires_the_declared_candidate_shell(self):
        report, _ = self.correction_fixture()
        correction_support = report["decision"]["supporting_attempt_ids"].copy()
        alternative = native_cohort()
        alternative["tuple"]["shell"] = "other-wkwebview"
        alternative["fingerprint"] = evidence.fingerprint(alternative["tuple"])
        self.add_comparison(report, alternative)
        self.reject(report, "cohort shell does not match its role")
        for row in alternative["attempts"]:
            mark(row, "inconclusive")
        self.reject(report, "shell disposition must be comparison-eligible")
        report["decision"].update(disposition="comparison-eligible", supporting_attempt_ids=correction_support)
        self.validate(report, "failing")

    def test_comparison_requires_qualified_recipes_on_each_signed_cohort(self):
        case_id = "composition-cancel"
        complete, _ = self.correction_fixture()
        correction_support = complete["decision"]["supporting_attempt_ids"].copy()
        self.add_comparison(complete, native_cohort())

        for role, donor in (("baseline", "corrected"), ("corrected", "comparison"), ("comparison", "baseline")):
            for defect, reason in (
                ("missing recipe", f"{case_id} recipe requires an object"),
                ("missing rehearsal", "two distinct observable rehearsals"),
                ("borrowed rehearsals", "rehearsal belongs to a different signed cohort"),
                ("reused rehearsal ID", "rehearsal ID is reused across recipes"),
                ("changed steps", "recipe ID does not match its frozen procedure"),
            ):
                with self.subTest(role=role, defect=defect):
                    report = copy.deepcopy(complete)
                    cohort = report["cohorts"][role]
                    recipe = cohort["recipes"][case_id]
                    if defect == "missing recipe":
                        del cohort["recipes"][case_id]
                    elif defect == "missing rehearsal":
                        recipe["rehearsals"].pop()
                    elif defect == "borrowed rehearsals":
                        recipe["rehearsals"] = report["cohorts"][donor]["recipes"][case_id]["rehearsals"]
                    elif defect == "reused rehearsal ID":
                        recipe["rehearsals"][0]["id"] = cohort["recipes"]["composition-commit"]["rehearsals"][0]["id"]
                    else:
                        recipe["steps"].append("Changed frozen gesture")

                    if role == "comparison":
                        self.reject(report, reason)
                        for row in cohort["attempts"]:
                            if row["case_id"] == case_id:
                                mark(row, "inconclusive")
                    self.reject(report, "shell disposition must be comparison-eligible")
                    report["decision"].update(disposition="comparison-eligible",
                                              supporting_attempt_ids=correction_support)
                    self.validate(report, "failing")

    def assert_comparison_uses_only_correction(self, report, correction_support):
        self.reject(report, "shell disposition must be comparison-eligible")
        report["decision"].update(disposition="comparison-eligible", supporting_attempt_ids=correction_support)
        self.validate(report, "failing")
        comparison = report["cohorts"]["comparison"]
        report["decision"]["supporting_attempt_ids"] = [*correction_support, comparison["attempts"][0]["id"]]
        self.reject(report, "disposition must reference the attempts that establish it")

    def test_candidate_matrix_cannot_count_a_shared_measured_observation_twice(self):
        complete, _ = self.correction_fixture()
        correction_support = complete["decision"]["supporting_attempt_ids"].copy()
        self.add_comparison(complete, native_cohort())
        self.validate(complete, "failing")
        for scope in ("baseline", "corrected", "same case", "different case", "different paste route"):
            with self.subTest(scope=scope):
                report = copy.deepcopy(complete)
                candidate = report["cohorts"]["comparison"]
                english = [row for row in candidate["attempts"] if row["case_id"] == "english-return"]
                ukrainian = [row for row in candidate["attempts"] if row["case_id"] == "ukrainian-return"]
                paste = [row for row in candidate["attempts"] if row["case_id"] == "paste-short"]
                donor = report["cohorts"].get(scope, candidate)
                if scope in ("baseline", "corrected"):
                    source, row = donor["attempts"][0], english[1]
                elif scope == "same case":
                    source, row = english
                elif scope == "different case":
                    source, row = english[0], ukrainian[1]
                else:
                    source = next(row for row in paste if row["route"] == "sample")
                    row = [row for row in paste if row["route"] == "native"][-1]
                earlier, later = retained_record(donor, source), retained_record(candidate, row)
                support = report["decision"]["supporting_attempt_ids"]
                support[support.index(row["id"])] = "relabeled candidate observation"
                later["attempt_id"] = row["id"] = "relabeled candidate observation"
                later["ref"] = row["ref"] = source["ref"]
                earlier["capture_segment"] = {"start_ms": 0, "end_ms": 100}
                later["capture_segment"] = {"start_ms": 100, "end_ms": 200}
                self.validate(report, "failing")

                later["capture_segment"] = earlier["capture_segment"].copy()
                self.reject(report, f"{row['id']}: expected inconclusive .*shared measured observation")
                mark(row, "inconclusive")
                self.assert_comparison_uses_only_correction(report, correction_support)

    def test_shared_recording_is_checked_against_all_earlier_admitted_measurements(self):
        report, corrected = self.correction_fixture()
        correction_support = report["decision"]["supporting_attempt_ids"].copy()
        candidate = native_cohort()
        self.add_comparison(report, candidate)
        first, second = self.baseline["attempts"][0], corrected["attempts"][0]
        third = next(row for row in candidate["attempts"] if row["case_id"] == "english-return")
        shared = first["ref"]
        for cohort, row, start in ((self.baseline, first, 0), (corrected, second, 100), (candidate, third, 200)):
            row["ref"] = shared
            retained_record(cohort, row).update(ref=shared, capture_segment={"start_ms": start, "end_ms": start + 100})
        self.validate(report, "failing")
        retained_record(candidate, third)["capture_segment"] = {"start_ms": 1, "end_ms": 99}
        self.reject(report, f"{third['id']}: expected inconclusive .*shared measured observation")
        mark(third, "inconclusive")
        self.assert_comparison_uses_only_correction(report, correction_support)

    def test_comparison_rejects_borrowed_recordings_even_with_the_recipient_binding(self):
        complete, _ = self.correction_fixture()
        correction_support = complete["decision"]["supporting_attempt_ids"].copy()
        self.add_comparison(complete, native_cohort())
        self.validate(complete, "failing")

        case_id = "composition-cancel"
        for role, donor in (("baseline", "corrected"), ("corrected", "comparison"), ("comparison", "baseline")):
            for rename in (False, True):
                with self.subTest(role=role, donor=donor, renamed=rename):
                    report = copy.deepcopy(complete)
                    recipient = report["cohorts"][role]
                    recipe = recipient["recipes"][case_id]
                    borrowed = report["cohorts"][donor]["recipes"][case_id]["rehearsals"][0]
                    rehearsal = recipe["rehearsals"][0]
                    rehearsal.update(id=rehearsal["id"] if rename else borrowed["id"], ref=borrowed["ref"],
                                     cohort_fingerprint=recipient["fingerprint"], recipe_id=recipe["id"],
                                     preedit=recipe["preedit"], end_state=recipe["end_state"])
                    self.assert_comparison_uses_only_correction(report, correction_support)

    def test_comparison_allows_shared_recordings_only_for_independent_segments(self):
        complete, _ = self.correction_fixture()
        correction_support = complete["decision"]["supporting_attempt_ids"].copy()
        self.add_comparison(complete, native_cohort())
        baseline = complete["cohorts"]["baseline"]["recipes"]
        comparison = complete["cohorts"]["comparison"]["recipes"]
        commit = comparison["composition-commit"]["rehearsals"]
        cancel = comparison["composition-cancel"]["rehearsals"]
        cancel.append(dict(cancel[-1], id=f"{cancel[-1]['id']}-additional"))
        shared = [*commit, baseline["composition-cancel"]["rehearsals"][0], cancel[-1]]
        for index, rehearsal in enumerate(shared):
            rehearsal.update(ref="synthetic shared recording",
                             capture_segment={"start_ms": index * 100, "end_ms": (index + 1) * 100})
        self.validate(complete, "failing")

        for scope, case_id, index, start in (
            ("same recipe", "composition-commit", 1, 0),
            ("different recipes", "composition-cancel", 2, 100),
            ("different builds", "composition-cancel", 2, 200),
        ):
            for defect in ("identical", "overlapping", "missing"):
                with self.subTest(scope=scope, defect=defect):
                    report = copy.deepcopy(complete)
                    candidate = report["cohorts"]["comparison"]
                    rehearsal = candidate["recipes"][case_id]["rehearsals"][index]
                    if defect == "missing":
                        del rehearsal["capture_segment"]
                    else:
                        offset = 25 if defect == "overlapping" else 0
                        rehearsal["capture_segment"] = {"start_ms": start + offset, "end_ms": start + offset + 100}
                    if scope != "different builds" or defect == "missing":
                        self.reject(report, "expected inconclusive .*rehearsal recording")
                        for row in candidate["attempts"]:
                            if row["case_id"] == case_id:
                                mark(row, "inconclusive")
                    self.assert_comparison_uses_only_correction(report, correction_support)

    def test_comparison_validates_supplied_segments_even_for_unshared_recordings(self):
        complete, _ = self.correction_fixture()
        correction_support = complete["decision"]["supporting_attempt_ids"].copy()
        self.add_comparison(complete, native_cohort())
        case_id = "composition-cancel"
        for segment in (None, [0, 10], {"start_ms": 0}, {"start_ms": True, "end_ms": 10},
                        {"start_ms": 0, "end_ms": 10.0}, {"start_ms": -1, "end_ms": 10},
                        {"start_ms": 0, "end_ms": 0}, {"start_ms": 10, "end_ms": 1}):
            with self.subTest(segment=segment):
                report = copy.deepcopy(complete)
                candidate = report["cohorts"]["comparison"]
                candidate["recipes"][case_id]["rehearsals"][0]["capture_segment"] = segment
                self.reject(report, "expected inconclusive .*rehearsal capture segment")
                for row in candidate["attempts"]:
                    if row["case_id"] == case_id:
                        mark(row, "inconclusive")
                self.assert_comparison_uses_only_correction(report, correction_support)

    def test_comparison_requires_the_same_verified_frozen_recipes(self):
        report, _ = self.correction_fixture()
        correction_support = report["decision"]["supporting_attempt_ids"].copy()
        alternative = native_cohort()
        self.add_comparison(report, alternative)
        case_id = "composition-replacement"
        recipe = alternative["recipes"][case_id]
        recipe["steps"].append("Different qualified gesture")
        recipe["id"] = evidence.fingerprint({key: value for key, value in recipe.items()
                                             if key not in ("id", "rehearsals")})
        for rehearsal in recipe["rehearsals"]:
            rehearsal["recipe_id"] = recipe["id"]
        for row in alternative["attempts"]:
            if row["case_id"] == case_id:
                row["recipe_id"] = recipe["id"]
        self.reject(report, "shell disposition must be comparison-eligible")
        report["decision"].update(disposition="comparison-eligible", supporting_attempt_ids=correction_support)
        self.validate(report, "failing")

    def test_comparison_requires_a_complete_clean_matrix(self):
        report, _ = self.correction_fixture()
        correction_support = report["decision"]["supporting_attempt_ids"].copy()
        alternative = native_cohort()
        self.add_comparison(report, alternative)
        attempt = alternative["attempts"][-1]
        remove_attempts(alternative, attempt)
        self.reject(report, "shell disposition must be comparison-eligible")
        report["decision"].update(disposition="comparison-eligible", supporting_attempt_ids=correction_support)
        self.validate(report, "failing")
        report["decision"]["supporting_attempt_ids"] = [*correction_support, alternative["attempts"][0]["id"]]
        self.reject(report, "disposition must reference the attempts that establish it")

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
