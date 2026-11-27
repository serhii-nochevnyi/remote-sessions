"""File-only guard for the deferred W1 handoff."""

import copy
import hashlib
import json
from pathlib import Path
import re
import shutil
from tempfile import TemporaryDirectory
import unittest
from urllib.parse import unquote, urlsplit


ROOT = Path(__file__).resolve().parents[2]
REPORT = "docs/W1-VERIFICATION.md"
EVIDENCE = "docs/W1-EVIDENCE.json"
WORKFLOW = "docs/LOCAL-DESKTOP-WORKFLOW.md"
PLAN = "docs/W1-PLAN.md"
ROADMAP = "docs/ROADMAP.md"
ADR = "docs/decisions/ADR-13-local-desktop-workflow.md"
FOLLOW_UP = "https://github.com/serhii-nochevnyi/remote-sessions/issues/10"
INSPECTED_REVISION = "086365f61368678fb780cfa1e7f4befa9d606276"
SCOPED_ARGV = "python3 -m unittest discover -s tests/integration -p test_w1_evidence.py".split()
REVIEWED_CASE_TEXT_SHA256 = "57455de156e47e9eeecab18de079801947213a10c06af3971a93790155eb08df"

PARENT_GATES = frozenset("R1 R2 R3 R4 R5 R6 R7".split())
PREPARATION_CASES = frozenset("""
    R6-PREP-PROBE-SENDER R6-PREP-CATALOG-REOPEN R6-PREP-LIVE-WORKLOAD
    R6-PREP-EVIDENCE-CONSUMER R6-PREP-NEGATIVE-FIXTURES
""".split())
INSTALLED_CASES = frozenset("""
    R1-DURABLE-BOUNDARIES R1-EQUAL-CONCURRENT R1-CONFLICT-KEY R1-LOST-REPLY
    R1-EARLY-EXIT R1-TMUX-OUTAGE-GENERATION R1-UNKNOWN-DISTINCT-OP R1-LEGACY-CONTINUITY
    R2-INTERNAL-CATALOG R2-DISK-FULL-WRITE R2-CORRUPT R2-NEWER-SCHEMA
    R2-MIGRATION-ROLLBACK R2-TOMBSTONES-PANES
    R3-CANCEL-PICKER R3-FORGED-WORKSPACE R3-MISSING-NONEXEC-CLI R3-CHANGED-DENIED-PATH
    R3-SYMLINK-NONUTF8 R3-CLAUDE-INTERNAL R3-CLAUDE-KINGSTON R3-CODEX-INTERNAL
    R3-CODEX-KINGSTON
    R4-EXPIRED-NO-SUCCESSOR R4-PRIOR-PROPAGATION R4-STALE-BEFORE-GUARD
    R4-YIELDING-CONTROL R4-DELAYED-SETTER R4-TIMEOUT-PRECOMMIT R4-TIMEOUT-POSTCOMMIT
    R4-AGENT-RESTART R4-SAME-SIZE-ABA R4-CORRELATED-READBACK R4-SPLIT-OUTPUT-REDRAW
    R4-CLAUDE-OWNER R4-CLAUDE-SPECTATOR R4-CODEX-OWNER R4-CODEX-SPECTATOR R4-RECONNECT
    R4-CLAUDE-SPECTATOR-REJECTION R4-CODEX-SPECTATOR-REJECTION R4-STALE-LEASE-REJECTION
    R5-STARTUP-PRESSURE R5-STEADY-PRESSURE R5-STALL-ISOLATION R5-HEALTHY-PROGRESS
    R5-PRESSURE-RESIZE R5-FRESH-REDRAW R5-TWO-SESSION-ISOLATION
    R6-SPLIT-CSI R6-SPLIT-DCS R6-SPLIT-OSC R6-FOCUS R6-COLOR
    R6-CLAUDE-NORMAL-SHORT R6-CLAUDE-NORMAL-MULTICHUNK
    R6-CLAUDE-BRACKETED-SHORT R6-CLAUDE-BRACKETED-MULTICHUNK
    R6-CODEX-NORMAL-SHORT R6-CODEX-NORMAL-MULTICHUNK
    R6-CODEX-BRACKETED-SHORT R6-CODEX-BRACKETED-MULTICHUNK
    R6-STOCK-SAFETY R6-EXPIRED-BEFORE-ADMISSION R6-STALE-HANDOFF R6-LEGACY-REJECTION
    R6-MISSING-SENDER-PROOF R6-ADMITTED-POST-TTL R6-KNOWN-HANDOFF R6-KNOWN-RESTART
    R6-POST-MARKER-ERROR R6-LOST-REPLY-BEFORE-BYTES R6-LOST-REPLY-AFTER-BYTES
    R6-TIMEOUT-QUARANTINE R6-RESTART-QUARANTINE R6-FRESH-PANE
    R7-CLAUDE-CLI R7-CODEX-CLI R7-CLAUDE-WEBVIEW R7-CODEX-WEBVIEW
    R7-CLAUDE-WEBVIEW-MENU R7-CODEX-WEBVIEW-MENU
    R7-KEYBOARD-REORDER-RESTORE R7-ZOOM-EXPAND-FULLSCREEN R7-APP-AGENT-RESTART
    R7-RUNTIME-BRIDGE-REGRESSION R7-PHYSICAL-DEVICE
    R7-CLAUDE-CREATE-ONE-TAB R7-CODEX-CREATE-ONE-TAB R7-RAPID-CONCURRENT-TABS
    R7-BRIDGE-PROVENANCE R7-INSTALLED-PROVENANCE R7-PASTE-PROVENANCE
""".split())
CASE_STATUS = dict.fromkeys(PARENT_GATES | PREPARATION_CASES, "DEFERRED")
CASE_STATUS.update(dict.fromkeys(INSTALLED_CASES, "UNRUN"))

SPECTATOR_REJECTIONS = {
    "R4-CLAUDE-SPECTATOR-REJECTION": "Claude Code",
    "R4-CODEX-SPECTATOR-REJECTION": "Codex",
}
STALE_REJECTION = "R4-STALE-LEASE-REJECTION"
GRIDS = dict.fromkeys(
    "committed_tmux_target committed_target_revision provider_tty bridge_pty_client xterm".split()
)
PROVENANCE = dict.fromkeys("""
    tested_commit tested_tree producer_log producer_record producer_digest tool_versions
    host_executable probe_executable owner_app_bundle victim_app_bundle operator_evidence
    private_capture_digest cleanup
""".split())
NATIVE_FIELDS = dict.fromkeys("""
    outcome tested_commit tested_tree provider_executable provider_version tmux_version
    os_version sqlite_version runtime_id server_generation session_id window_id pane_id
    pid process_start view_id agent_epoch owner_epoch paste_receipt submitted_model_prompts
""".split())
CASE_OBSERVATIONS = dict.fromkeys(("outcome", "submitted_model_prompts"))
REJECTION_OBSERVATIONS = {
    **CASE_OBSERVATIONS,
    **dict.fromkeys("""
        host_rejection request spectator_view_id stale_view_id owner_epoch previous_owner_epoch
        runtime_id server_generation window_id pane_id
    """.split()),
    "owner_grids_before": GRIDS,
    "owner_grids_after": GRIDS,
}
DOC_LINKS = {
    "README.md": (WORKFLOW, REPORT, EVIDENCE),
    WORKFLOW: (REPORT, EVIDENCE, ADR, "docs/DEVELOPMENT.md", "docs/RUNTIME-LAB.md"),
    PLAN: (WORKFLOW, REPORT),
    ROADMAP: (WORKFLOW, REPORT),
    ADR: (WORKFLOW, REPORT, EVIDENCE),
    REPORT: (EVIDENCE,),
}
LINK = re.compile(r"(?<!!)\[[^]\n]+\]\(([^)\s]+)\)")
NON_RENDERED_LINK_CONTEXT = re.compile(r"<!--.*?-->|^```[^\n]*\n.*?^```[ \t]*$|`[^`\n]+`", re.MULTILINE | re.DOTALL)
FALSE_OUTCOME_CLAIM = re.compile(
    r"\b(?:native|installed)(?:\s+(?:acceptance|outcome|verification|checks?))?"
    r"[:* =]+\s*(?:(?:has|is)\s+)?pass(?:ed)?\b"
    r"|\b(?:W1|P0|pilot)(?:\s+completion)?\s*[:* ]*\s*(?:is\s+)?complete[d]?\b"
    r"|\bobserved\s+(?:0|zero|\d+)\s+(?:submitted\s+)?(?:model\s+)?prompts?\b"
    r"|\b(?:submitted\s+)?model prompts?\s*[:*]+\s*(?:0|zero|\d+)\b",
    re.IGNORECASE,
)


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def read_file(root, relative):
    path = root / relative
    require(path.is_file(), f"Missing required file: {relative}")
    require(path.resolve().is_relative_to(root.resolve()), f"File escapes repository: {relative}")
    return path.read_text(encoding="utf-8")


def unique_object(pairs):
    obj = {}
    for key, value in pairs:
        require(key not in obj, f"{EVIDENCE}: duplicate JSON field {key}")
        obj[key] = value
    return obj


def load_report(root):
    return json.loads(read_file(root, EVIDENCE), object_pairs_hook=unique_object), read_file(root, REPORT)


def require_relative_link(root, page, markdown, target):
    expected = (root / target).resolve()
    for link in LINK.findall(NON_RENDERED_LINK_CONTEXT.sub("", markdown)):
        url = urlsplit(link)
        relative = unquote(url.path)
        if url.scheme or url.netloc or url.query or url.fragment or not relative or relative.startswith("/"):
            continue
        if (root / page).parent.joinpath(relative).resolve() == expected:
            read_file(root, target)
            return
    raise AssertionError(f"{page}: missing relative Markdown link to {target}")


def require_markdown_field(markdown, label, expected, page=REPORT):
    prefix = f"**{label}:** "
    values = [
        line.removeprefix(prefix).rstrip().removesuffix("\\")
        for line in markdown.splitlines()
        if line.startswith(prefix)
    ]
    require(values == [expected], f"{page}: expected exactly one {label} value of {expected}")


def require_follow_up(markdown, page):
    require_markdown_field(markdown, "Follow-up", f"OPEN — [GitHub issue #10]({FOLLOW_UP})", page)


def index_cases(rows, label):
    indexed = {}
    for case_id, row in rows:
        require(isinstance(case_id, str), f"{label}: case ID must be a string")
        require(case_id not in indexed, f"{label}: duplicate case ID {case_id}")
        indexed[case_id] = row
    missing = CASE_STATUS.keys() - indexed.keys()
    extra = indexed.keys() - CASE_STATUS.keys()
    require(not (missing or extra), f"{label}: missing cases {sorted(missing)}, unexpected cases {sorted(extra)}")
    return indexed


def require_rejection_witness(case_id, witness):
    common = (
        "different-size", "explicit host rejection", "request", "owner epoch",
        "runtime/server/window/pane", "before and after", "committed tmux target/revision",
        "provider tty", "bridge pty/client", "actual xterm grid", "unchanged dimensions and target revision",
    )
    if case_id in SPECTATOR_REJECTIONS:
        distinct = (
            SPECTATOR_REJECTIONS[case_id].lower(), "spectator resize", "spectator view",
            "legitimate owner retains control", "same owner's four grids", "independent",
            "no unauthorized geometry effect", "spectator readback as a separate witness",
        )
    else:
        distinct = ("new different-size resize", "stale lease after handoff", "current owner's four grids",
                    "independently")
    for phrase in (*common, *distinct):
        require(phrase in witness.lower(), f"{case_id}: owed witness must include {phrase!r}")


def require_held_case_text(report, case_id, reason, owed_witness):
    for field, value in (("reason", reason), ("owed_witness", owed_witness)):
        claim = FALSE_OUTCOME_CLAIM.search(value)
        require(not claim, f"{report}: {case_id}: contradictory {field} claim: {claim and claim[0]}")


def require_reviewed_case_text(report, triples):
    canonical = json.dumps(sorted(triples), ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    require(
        hashlib.sha256(canonical).hexdigest() == REVIEWED_CASE_TEXT_SHA256,
        f"{report}: held case text differs from the reviewed baseline; baseline review required",
    )


def validate_evidence(root, data, markdown):
    require(isinstance(data, dict) and data.keys() == {
        "schema", "installed_acceptance", "completion", "follow_up", "source_handoff",
        "preparation", "native_observations", "cases",
    }, f"{EVIDENCE}: expected the held report fields")
    require(data["schema"] == "w1-deferred-report-1", f"{EVIDENCE}: unexpected report schema")
    require(data["installed_acceptance"] == "DEFERRED", "Installed acceptance must be DEFERRED")
    require(data["completion"] == dict.fromkeys(("w1", "p0", "pilot"), "HELD"),
            "W1, P0 and pilot completion must all be HELD")
    require(data["follow_up"] == {"status": "OPEN", "url": FOLLOW_UP},
            f"{EVIDENCE}: expected OPEN follow-up {FOLLOW_UP}")
    require(data["source_handoff"] == {
        "inspected_revision": INSPECTED_REVISION,
        "requirements": ["REQ-11", "REQ-12"],
        "scoped_check": {
            "argv": SCOPED_ARGV, "status": "PENDING", "exit_status": None,
            "test_count": None, "test_names": None,
        },
    }, "Source handoff must identify the inspected revision and pending scoped check")
    preparation = data["preparation"]
    require(isinstance(preparation, dict) and preparation.keys() == {
        "positive_paste_producer", "paste_mode_build_record_supported",
        "positive_paste_workload_supported", "positive_paste_evidence_consumer_supported",
        "test_paste_evidence_supported",
    }, "Unexpected paste preparation fields")
    require(
        preparation["positive_paste_producer"] == "HOLD"
        and preparation["paste_mode_build_record_supported"] is True
        and preparation["positive_paste_workload_supported"] is False
        and preparation["positive_paste_evidence_consumer_supported"] is False
        and preparation["test_paste_evidence_supported"] is False,
        "Paste preparation must retain HOLD and accurately report unsupported positive routes",
    )
    require(data["native_observations"] == {
        **NATIVE_FIELDS, "geometry": GRIDS,
        "provenance": dict.fromkeys(("bridge", "installed", "paste"), PROVENANCE),
    }, "Native observations, prompt counts and mode-specific provenance must remain explicit and unknown")

    for label, expected in (
        ("Installed acceptance", data["installed_acceptance"]),
        ("W1 completion", data["completion"]["w1"]),
        ("P0 completion", data["completion"]["p0"]),
        ("Pilot completion", data["completion"]["pilot"]),
        ("Native outcome", "Unknown"),
        ("Submitted model prompts", "Unknown"),
        ("Scoped source check", data["source_handoff"]["scoped_check"]["status"]),
        ("Positive paste producer", preparation["positive_paste_producer"]),
    ):
        require_markdown_field(markdown, label, expected)
    require_follow_up(markdown, REPORT)
    require_relative_link(root, REPORT, markdown, EVIDENCE)

    rows = data["cases"]
    require(isinstance(rows, list), f"{EVIDENCE}: cases must be a list")
    for row in rows:
        require(isinstance(row, dict) and row.keys() == {
            "id", "status", "reason", "owed_witness", "observations",
        }, f"{EVIDENCE}: each case must contain the held case fields")
    json_cases = index_cases(((row["id"], row) for row in rows), EVIDENCE)
    markdown_rows = []
    prose = []
    for line in markdown.splitlines():
        if re.match(r"^\|\s*`R[1-7]", line):
            cells = [cell.strip() for cell in line.strip("|").split("|")]
            require(len(cells) == 5 and cells[0].endswith("`"), f"{REPORT}: malformed case row {line}")
            markdown_rows.append((cells[0][1:-1], cells[1:]))
        else:
            prose.append(line)
    markdown_cases = index_cases(markdown_rows, REPORT)
    false_claim = FALSE_OUTCOME_CLAIM.search("\n".join(prose))
    require(not false_claim, f"{REPORT}: contradictory acceptance claim: {false_claim and false_claim[0]}")

    for case_id, status in CASE_STATUS.items():
        row = json_cases[case_id]
        require(row["status"] == status, f"{case_id}: expected {status}")
        for field in ("reason", "owed_witness"):
            require(
                isinstance(row[field], str) and row[field].strip() and "\n" not in row[field],
                f"{case_id}: missing or multiline {field}",
            )
        require_held_case_text(EVIDENCE, case_id, row["reason"], row["owed_witness"])
        is_rejection = case_id in SPECTATOR_REJECTIONS or case_id == STALE_REJECTION
        expected_observations = REJECTION_OBSERVATIONS if is_rejection else CASE_OBSERVATIONS
        require(row["observations"] == expected_observations, f"{case_id}: observations must remain unknown")
        if is_rejection:
            require_rejection_witness(case_id, row["owed_witness"])
        markdown_case = markdown_cases[case_id]
        require_held_case_text(REPORT, case_id, markdown_case[1], markdown_case[3])
        require(
            markdown_case == [status, row["reason"], "Unknown", row["owed_witness"]],
            f"{REPORT}: {case_id} status, reason, prompt observation or owed witness differs from {EVIDENCE}",
        )

    require_reviewed_case_text(EVIDENCE, (
        (case_id, row["reason"], row["owed_witness"]) for case_id, row in json_cases.items()
    ))
    require_reviewed_case_text(REPORT, (
        (case_id, reason, witness) for case_id, (_, reason, _, witness) in markdown_cases.items()
    ))


def validate_public_docs(root):
    pages = {page: read_file(root, page) for page in DOC_LINKS}
    for page, targets in DOC_LINKS.items():
        markdown = pages[page]
        require_markdown_field(markdown, "Installed acceptance", "DEFERRED", page)
        require_follow_up(markdown, page)
        for target in targets:
            require_relative_link(root, page, markdown, target)
    for gate in sorted(PARENT_GATES):
        require(re.search(rf"\b{gate}\b", pages[WORKFLOW]), f"{WORKFLOW}: missing {gate} reference")
    validate_evidence(root, *load_report(root))


class W1EvidenceTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.data, cls.markdown = load_report(ROOT)

    def test_repository_deferred_report(self):
        validate_evidence(ROOT, self.data, self.markdown)
        print("W1EvidenceTest.test_repository_deferred_report", flush=True)

    def test_rejects_native_pass(self):
        data = copy.deepcopy(self.data)
        data["installed_acceptance"] = "PASS"
        with self.assertRaisesRegex(AssertionError, "acceptance must be DEFERRED"):
            validate_evidence(ROOT, data, self.markdown)
        data["installed_acceptance"] = "DEFERRED"
        data["native_observations"]["outcome"] = "PASS"
        with self.assertRaisesRegex(AssertionError, "Native observations"):
            validate_evidence(ROOT, data, self.markdown)

    def test_rejects_each_completion_claim(self):
        for scope in ("w1", "p0", "pilot"):
            with self.subTest(scope=scope):
                data = copy.deepcopy(self.data)
                data["completion"][scope] = "COMPLETE"
                with self.assertRaisesRegex(AssertionError, "completion must all be HELD"):
                    validate_evidence(ROOT, data, self.markdown)

    def test_rejects_observed_prompt_counts(self):
        workflows = (
            "R7-CLAUDE-CLI", "R7-CODEX-CLI", "R7-CLAUDE-WEBVIEW", "R7-CODEX-WEBVIEW",
            "R7-CLAUDE-WEBVIEW-MENU", "R7-CODEX-WEBVIEW-MENU", "R6-CODEX-BRACKETED-MULTICHUNK",
        )
        for value in (0, 1, -1, "0", False, {}):
            for case_id in (None, *workflows):
                with self.subTest(case=case_id, value=value):
                    data = copy.deepcopy(self.data)
                    observations = data["native_observations"] if case_id is None else next(
                        row["observations"] for row in data["cases"] if row["id"] == case_id
                    )
                    observations["submitted_model_prompts"] = value
                    with self.assertRaisesRegex(AssertionError, "observations"):
                        validate_evidence(ROOT, data, self.markdown)

    def test_rejects_missing_obligations_in_either_or_both_reports(self):
        for case_id in sorted(CASE_STATUS):
            line_prefix = f"| `{case_id}` |"
            without_markdown_row = "\n".join(
                line for line in self.markdown.splitlines() if not line.startswith(line_prefix)
            )
            without_json_row = {
                **self.data, "cases": [row for row in self.data["cases"] if row["id"] != case_id]
            }
            for variant, data, markdown in (
                ("JSON", without_json_row, self.markdown),
                ("Markdown", self.data, without_markdown_row),
                ("both", without_json_row, without_markdown_row),
            ):
                with self.subTest(case=case_id, variant=variant):
                    with self.assertRaisesRegex(AssertionError, rf"missing cases .*'{case_id}'"):
                        validate_evidence(ROOT, data, markdown)

    def test_rejects_duplicate_cases(self):
        row = self.data["cases"][0]
        data = {**self.data, "cases": [*self.data["cases"], row]}
        with self.assertRaisesRegex(AssertionError, "duplicate case ID"):
            validate_evidence(ROOT, data, self.markdown)
        line = next(line for line in self.markdown.splitlines() if line.startswith("| `R1` |"))
        with self.assertRaisesRegex(AssertionError, "duplicate case ID"):
            validate_evidence(ROOT, self.data, self.markdown + line + "\n")

    def test_rejects_readback_as_spectator_rejection(self):
        for case_id in SPECTATOR_REJECTIONS:
            with self.subTest(case=case_id):
                data = copy.deepcopy(self.data)
                cases = {row["id"]: row for row in data["cases"]}
                row = cases[case_id]
                readback = cases[case_id.removesuffix("-REJECTION")]["owed_witness"]
                markdown = self.markdown.replace(row["owed_witness"], readback, 1)
                row["owed_witness"] = readback
                with self.assertRaisesRegex(AssertionError, rf"{case_id}: owed witness"):
                    validate_evidence(ROOT, data, markdown)

    def test_rejects_observed_rejection_and_grids(self):
        for case_id in (*SPECTATOR_REJECTIONS, STALE_REJECTION):
            for field in ("host_rejection", "owner_grids_before", "owner_grids_after"):
                with self.subTest(case=case_id, field=field):
                    data = copy.deepcopy(self.data)
                    observations = next(row["observations"] for row in data["cases"] if row["id"] == case_id)
                    if field == "host_rejection":
                        observations[field] = "rejected"
                    else:
                        observations[field]["committed_target_revision"] = 3
                    with self.assertRaisesRegex(AssertionError, rf"{case_id}: observations"):
                        validate_evidence(ROOT, data, self.markdown)

    def test_rejects_invented_positive_paste_routes(self):
        for field, value in (
            ("positive_paste_producer", "PASS"),
            ("positive_paste_workload_supported", True),
            ("positive_paste_evidence_consumer_supported", True),
            ("test_paste_evidence_supported", True),
        ):
            with self.subTest(field=field):
                data = copy.deepcopy(self.data)
                data["preparation"][field] = value
                with self.assertRaisesRegex(AssertionError, "Paste preparation must retain HOLD"):
                    validate_evidence(ROOT, data, self.markdown)

    def test_rejects_provenance_and_generic_acceptance_override(self):
        plausible_provenance = {
            "tested_commit": "a" * 40, "tested_tree": "b" * 40, "producer_digest": "c" * 64,
            "owner_app_bundle": {"executable_digest": "d" * 64},
        }
        data = copy.deepcopy(self.data)
        data["native_observations"]["provenance"]["installed"].update(plausible_provenance)
        with self.assertRaisesRegex(AssertionError, "provenance must remain explicit and unknown"):
            validate_evidence(ROOT, data, self.markdown)
        data["installed_acceptance"] = "PASS"
        with self.assertRaisesRegex(AssertionError, "acceptance must be DEFERRED"):
            validate_evidence(ROOT, data, self.markdown)
        for field, value in (("accepted", True), ("approval", "approved"), ("receipt", "a" * 64)):
            with self.subTest(field=field):
                data = {**self.data, field: value}
                with self.assertRaisesRegex(AssertionError, "expected the held report fields"):
                    validate_evidence(ROOT, data, self.markdown)

    def test_rejects_unobserved_source_check_result(self):
        data = copy.deepcopy(self.data)
        data["source_handoff"]["scoped_check"].update(status="PASS", exit_status=0)
        with self.assertRaisesRegex(AssertionError, "pending scoped check"):
            validate_evidence(ROOT, data, self.markdown)

    def test_rejects_missing_or_substituted_follow_up(self):
        for follow_up in ({}, {"status": "OPEN", "url": FOLLOW_UP.replace("/10", "/79")},
                          {"status": "CLOSED", "url": FOLLOW_UP}):
            with self.subTest(follow_up=follow_up):
                with self.assertRaisesRegex(AssertionError, "expected OPEN follow-up"):
                    validate_evidence(ROOT, {**self.data, "follow_up": follow_up}, self.markdown)
        for markdown in (self.markdown.replace(FOLLOW_UP, FOLLOW_UP.replace("/10", "/79")),
                         "\n".join(line for line in self.markdown.splitlines() if FOLLOW_UP not in line)):
            with self.assertRaisesRegex(AssertionError, "expected exactly one Follow-up"):
                validate_evidence(ROOT, self.data, markdown)

    def test_rejects_markdown_native_pass_and_observed_zero_prompts(self):
        for claim in ("Native acceptance passed.", "Observed zero submitted model prompts."):
            with self.subTest(claim=claim), TemporaryDirectory() as directory:
                root = Path(directory)
                (root / REPORT).parent.mkdir()
                shutil.copyfile(ROOT / EVIDENCE, root / EVIDENCE)
                (root / REPORT).write_text(self.markdown + "\n" + claim + "\n", encoding="utf-8")
                with self.assertRaisesRegex(AssertionError, "contradictory acceptance claim"):
                    validate_evidence(root, *load_report(root))

    def test_rejects_markdown_case_status_and_prompt_changes(self):
        for new_value in ("PASS | changed | Unknown", "UNRUN | changed | 0"):
            with self.subTest(value=new_value):
                markdown = re.sub(
                    r"(\| `R7-CLAUDE-CLI` \| )UNRUN \| [^|]+ \| Unknown",
                    lambda match: match[1] + new_value,
                    self.markdown,
                    count=1,
                )
                with self.assertRaisesRegex(AssertionError, "R7-CLAUDE-CLI.*differs"):
                    validate_evidence(ROOT, self.data, markdown)

    def case_text_claim_fixture(self, field, claim):
        case_id = "R7-CLAUDE-CLI"
        data = copy.deepcopy(self.data)
        row = next(row for row in data["cases"] if row["id"] == case_id)
        original = f"| `{case_id}` | {row['status']} | {row['reason']} | Unknown | {row['owed_witness']} |"
        self.assertEqual(self.markdown.splitlines().count(original), 1)
        row[field] += f" {claim}"
        changed = f"| `{case_id}` | {row['status']} | {row['reason']} | Unknown | {row['owed_witness']} |"
        return case_id, data, self.markdown.replace(original, changed, 1)

    def test_rejects_paired_owed_witness_prompt_claim(self):
        case_id, data, markdown = self.case_text_claim_fixture(
            "owed_witness", "Observed 2 submitted model prompts."
        )
        with self.assertRaisesRegex(AssertionError, rf"{case_id}: contradictory owed_witness claim"):
            validate_evidence(ROOT, data, markdown)
        print("W1EvidenceTest.test_rejects_paired_owed_witness_prompt_claim", flush=True)

    def test_rejects_paired_installed_outcome_claim(self):
        for field in ("reason", "owed_witness"):
            with self.subTest(field=field):
                case_id, data, markdown = self.case_text_claim_fixture(field, "Installed outcome passed.")
                with self.assertRaisesRegex(AssertionError, rf"{case_id}: contradictory {field} claim"):
                    validate_evidence(ROOT, data, markdown)
                with self.assertRaisesRegex(
                    AssertionError, rf"{re.escape(REPORT)}: {case_id}: contradictory {field} claim"
                ):
                    validate_evidence(ROOT, self.data, markdown)
        print("W1EvidenceTest.test_rejects_paired_installed_outcome_claim", flush=True)

    def assert_paired_edit_requires_baseline_review(self, text):
        for field in ("reason", "owed_witness"):
            with self.subTest(field=field):
                _, data, markdown = self.case_text_claim_fixture(field, text)
                with self.assertRaisesRegex(AssertionError, "baseline review required"):
                    validate_evidence(ROOT, data, markdown)
        print(self.id(), flush=True)

    def test_rejects_paired_count_first_prompt_claim(self):
        self.assert_paired_edit_requires_baseline_review("Zero submitted model prompts were observed.")

    def test_rejects_paired_numeral_prompt_claim(self):
        self.assert_paired_edit_requires_baseline_review("0 submitted model prompts were observed.")

    def test_rejects_paired_synonymous_prompt_claim(self):
        self.assert_paired_edit_requires_baseline_review("No requests were sent to the model.")

    def test_rejects_paired_localized_prompt_claim(self):
        self.assert_paired_edit_requires_baseline_review("Не зафіксовано жодного надісланого запиту до моделі.")

    def test_rejects_paired_innocuous_wording_edit(self):
        self.assert_paired_edit_requires_baseline_review("Record the corresponding details in the follow-up.")

    def test_rejects_paired_future_witness_append(self):
        self.assert_paired_edit_requires_baseline_review("The installed run must verify zero submitted model prompts.")

    def test_rejects_missing_json_link(self):
        markdown = self.markdown.replace("(W1-EVIDENCE.json)", "(unrelated.json)")
        with self.assertRaisesRegex(AssertionError, "missing relative Markdown link to docs/W1-EVIDENCE"):
            validate_evidence(ROOT, self.data, markdown)


class W1PublicDocsTest(unittest.TestCase):
    def test_repository_public_docs(self):
        validate_public_docs(ROOT)
        print("W1PublicDocsTest.test_repository_public_docs", flush=True)

    def docs_fixture(self):
        directory = TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        files = set(DOC_LINKS) | {target for targets in DOC_LINKS.values() for target in targets}
        for relative in sorted(files):
            destination = root / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / relative, destination)
        validate_public_docs(root)
        return root

    def test_rejects_missing_workflow_file(self):
        root = self.docs_fixture()
        (root / WORKFLOW).unlink()
        with self.assertRaisesRegex(AssertionError, f"Missing required file: {re.escape(WORKFLOW)}"):
            validate_public_docs(root)

    def test_rejects_missing_readme_workflow_link(self):
        root = self.docs_fixture()
        readme = root / "README.md"
        markdown = readme.read_text(encoding="utf-8")
        link = re.compile(rf"\[([^]\n]+)\]\({re.escape(WORKFLOW)}\)")
        readme.write_text(link.sub(r"\1", markdown), encoding="utf-8")
        message = "README.md: missing relative Markdown link to " + re.escape(WORKFLOW)
        with self.assertRaisesRegex(AssertionError, message):
            validate_public_docs(root)


if __name__ == "__main__":
    unittest.main()
