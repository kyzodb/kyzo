#!/usr/bin/env python3
# Copyright 2026, The KyzoDB Authors. MPL-2.0.
"""Board management for the KyzoDB Work board.

The single authority for reading and writing epics and stories on GitHub.
Raw ``gh`` JSON is validated directly into the domain models below — parse,
don't validate: an issue that does not conform to the epic or story schema
fails loudly here and is never half-edited by a command.

The board commands live in this file as pydantic models — CreateEpic,
CreateStory, UpdateEpic, UpdateStory, MoveIssue — each declaring its exact
fields from the schemas, with an execute() that constructs the target state
and emits it through the one gh boundary. The command models are the public
API; the CLI only maps argv onto them.
"""

import argparse
import json
import subprocess
import sys
from enum import Enum

from pydantic import BaseModel, ConfigDict, Field

# ---------------------------------------------------------------------------
# Domain
# ---------------------------------------------------------------------------


class IssueState(str, Enum):
    OPEN = "OPEN"
    CLOSED = "CLOSED"


class MilestoneTitle(str, Enum):
    NOW = "1-Now"
    NEXT = "2-Next"
    LATER = "3-Later"


class LabelName(str, Enum):
    FEATURE = "Feature"
    BUG = "Bug"
    PERFORMANCE = "Performance"
    SECURITY = "Security"
    DEMO = "Demo"


class ColumnName(str, Enum):
    TODO = "Todo"
    IN_PROGRESS = "In Progress"
    DONE = "Done"


class Author(BaseModel):
    id: str
    is_bot: bool
    login: str
    name: str


class CommentAuthor(BaseModel):
    login: str


class UserCount(BaseModel):
    totalCount: int


class ReactionGroup(BaseModel):
    content: str
    users: UserCount


class Comment(BaseModel):
    id: str
    author: CommentAuthor
    authorAssociation: str
    body: str
    createdAt: str
    includesCreatedEdit: bool

    @property
    def rendered(self) -> str:
        return f"— {self.author.login} ({self.createdAt[:10]}): {self.body.rstrip()}"
    isMinimized: bool
    minimizedReason: str
    reactionGroups: list[ReactionGroup]
    url: str
    viewerDidAuthor: bool


class Label(BaseModel):
    id: str
    # The mirror holds GitHub's string; LabelName is a str-enum, so a lawful
    # member IS a str. Classification law binds where the label is consumed.
    name: str
    description: str
    color: str


class Assignee(BaseModel):
    id: str
    login: str
    name: str


class MilestoneInfo(BaseModel):
    number: int
    # The mirror holds GitHub's string (drift stays visible). Commands accept
    # only MilestoneTitle — drift is representable here, never settable.
    title: str
    description: str
    dueOn: str | None


class IssueType(BaseModel):
    id: str
    name: str


class IssueRef(BaseModel):
    id: str
    number: int
    state: IssueState
    title: str
    url: str


class SubIssues(BaseModel):
    nodes: list[IssueRef]
    totalCount: int


class SubIssuesSummary(BaseModel):
    completed: int
    percentCompleted: int
    total: int


class IssueLink(BaseModel):
    id: str
    number: int
    title: str
    url: str


class IssueLinks(BaseModel):
    nodes: list[IssueLink]
    totalCount: int


class ProjectStatus(BaseModel):
    optionId: str
    name: ColumnName


class ProjectItem(BaseModel):
    status: ProjectStatus
    title: str


class Issue(BaseModel):
    number: int
    title: str
    body: str
    state: IssueState
    stateReason: str
    closed: bool
    closedAt: str | None
    createdAt: str
    updatedAt: str
    id: str
    url: str
    isPinned: bool
    issueType: IssueType | None
    author: Author
    labels: list[Label]
    assignees: list[Assignee]
    milestone: MilestoneInfo | None
    parent: IssueRef | None
    subIssues: SubIssues
    subIssuesSummary: SubIssuesSummary
    comments: list[Comment]
    projectItems: list[ProjectItem]

    @property
    def rendered(self) -> str:
        facts = " ".join(
            (
                f"state={self.state.value}",
                f"labels={','.join(l.name for l in self.labels) or 'none'}",
                f"milestone={self.milestone.title if self.milestone else 'none'}",
                f"parent=#{self.parent.number}" if self.parent else "parent=none",
            )
        )
        subs = " ".join(f"#{s.number}" for s in self.subIssues.nodes)
        return "\n\n".join(
            part
            for part in (
                f"#{self.number} {self.title}\n{facts}" + (f"\nsubs: {subs}" if subs else ""),
                self.body.rstrip(),
                "\n".join(c.rendered for c in self.comments),
            )
            if part
        )
    blockedBy: IssueLinks
    blocking: IssueLinks
    closedByPullRequestsReferences: list[IssueLink]
    reactionGroups: list[ReactionGroup]


# ---------------------------------------------------------------------------
# Contract — the epic and story body schemas. The markdown is the wire form
# of a body: rendered as a projection of the model, lifted back by a boundary
# constructor that names exactly what is missing when a body does not conform.
# ---------------------------------------------------------------------------


class ChoiceType(str, Enum):
    REPRESENTATION = "Representation"
    AUTHORITY_BOUNDARY = "Authority Boundary"
    EXECUTION_CURRENCY = "Execution Currency"
    CACHE_INVALIDATION = "Cache Invalidation"
    STORAGE_CONTRACT = "Storage Contract"
    ORDERING_INVARIANT = "Ordering Invariant"
    ADMISSION_PATH = "Admission Path"
    EVALUATOR_RULE = "Evaluator Rule"
    ALGORITHM = "Algorithm"
    BENCHMARK = "Benchmark"
    FAILURE_PATH = "Failure Path"
    EVIDENCE_BOUNDARY = "Evidence Boundary"


def _sections(text: str) -> dict[str, str]:
    """Split a body into its '## ' sections. Boundary lift, used only here."""
    parts = ("\n" + text).split("\n## ")
    return {
        part.splitlines()[0].strip(): "\n".join(part.splitlines()[1:]).strip()
        for part in parts[1:]
    }


def _section(sections: dict[str, str], name: str) -> str:
    if name not in sections:
        raise ValueError(f"body has no '## {name}' section")
    return sections[name]


def _is_label_line(line: str) -> bool:
    key, colon, _ = line.partition(":")
    return bool(colon) and bool(key) and key[0].isupper() and key.replace(" ", "").isalpha()


def _labeled(text: str, key: str, where: str) -> str:
    """The value of a 'Key: ...' line, running to the next labeled line."""
    marker = f"{key}:"
    lines = text.splitlines()
    starts = [i for i, line in enumerate(lines) if line.startswith(marker)]
    if not starts:
        raise ValueError(f"'{where}' section is missing '{key}:'")
    start = starts[0]
    collected = [lines[start][len(marker):].strip()]
    for line in lines[start + 1:]:
        if _is_label_line(line):
            break
        collected.append(line)
    return "\n".join(collected).strip()


class Task(BaseModel):
    model_config = ConfigDict(frozen=True)
    checked: bool
    text: str

    @property
    def markdown(self) -> str:
        return f"- [{'x' if self.checked else ' '}] {self.text}"

    @classmethod
    def all_from(cls, text: str) -> tuple["Task", ...]:
        return tuple(
            cls(checked=line[3] == "x", text=line[6:].strip())
            for line in text.splitlines()
            if line.startswith("- [ ] ") or line.startswith("- [x] ")
        )


class Description(BaseModel):
    model_config = ConfigDict(frozen=True)
    actor: str = Field(min_length=1)
    want: str = Field(min_length=1)
    so_that: str = Field(min_length=1)

    @property
    def markdown(self) -> str:
        return f"As a {self.actor},\nI want {self.want},\nso that {self.so_that}."

    @classmethod
    def from_markdown(cls, text: str) -> "Description":
        t = text.strip()
        prefix = "As an " if t.startswith("As an ") else "As a " if t.startswith("As a ") else ""
        if not prefix:
            raise ValueError("'Description' does not begin 'As a …'")
        actor, sep1, rest = t.removeprefix(prefix).partition(",\nI want ")
        want, sep2, so_that = rest.partition(",\nso that ")
        if not sep1 or not sep2:
            raise ValueError("'Description' does not match As a / I want / so that")
        return cls(actor=actor.strip(), want=want.strip(), so_that=so_that.strip().rstrip("."))


class Condemned(BaseModel):
    model_config = ConfigDict(frozen=True)
    path: str = Field(min_length=1)
    reason: str = Field(min_length=1)
    closure_test: str = Field(min_length=1)

    @property
    def markdown(self) -> str:
        return (
            f"Path: {self.path}\n\nReason: {self.reason}\n\n"
            f"Closure test: {self.closure_test}"
        )

    @classmethod
    def from_markdown(cls, text: str) -> "Condemned":
        return cls(
            path=_labeled(text, "Path", "Condemned"),
            reason=_labeled(text, "Reason", "Condemned"),
            closure_test=_labeled(text, "Closure test", "Condemned"),
        )


class EngineeringChoice(BaseModel):
    model_config = ConfigDict(frozen=True)
    choice: str = Field(min_length=1)
    choice_type: ChoiceType
    consequence: str = Field(min_length=1)
    evidence_needed: str = Field(min_length=1)

    @property
    def markdown(self) -> str:
        return (
            f"Choice: {self.choice}\n\nChoice type: {self.choice_type.value}\n\n"
            f"Consequence: {self.consequence}\n\nEvidence needed: {self.evidence_needed}"
        )

    @classmethod
    def from_markdown(cls, text: str) -> "EngineeringChoice":
        return cls(
            choice=_labeled(text, "Choice", "Engineering Choice"),
            choice_type=ChoiceType(_labeled(text, "Choice type", "Engineering Choice")),
            consequence=_labeled(text, "Consequence", "Engineering Choice"),
            evidence_needed=_labeled(text, "Evidence needed", "Engineering Choice"),
        )


class StoryBody(BaseModel):
    model_config = ConfigDict(frozen=True)
    label: LabelName
    milestone: MilestoneTitle
    description: Description
    condemned: Condemned
    engineering_choice: EngineeringChoice
    context: str = Field(min_length=1)
    tasks: tuple[Task, ...] = Field(min_length=1)
    definition_of_done: tuple[Task, ...] = Field(min_length=1)

    @property
    def markdown(self) -> str:
        # label and milestone are GitHub metadata (chip + milestone), never body text
        return "\n".join(
            [
                "## Description",
                "",
                self.description.markdown,
                "",
                "## Condemned",
                "",
                self.condemned.markdown,
                "",
                "## Engineering Choice",
                "",
                self.engineering_choice.markdown,
                "",
                "## Context",
                "",
                self.context,
                "",
                "## Tasks",
                "",
                *[t.markdown for t in self.tasks],
                "",
                "## Definition of Done",
                "",
                *[t.markdown for t in self.definition_of_done],
                "",
            ]
        )

    @classmethod
    def from_issue(cls, issue: Issue) -> "StoryBody":
        class_labels = [l.name for l in issue.labels if l.name in set(LabelName)]
        if not class_labels or issue.milestone is None:
            raise ValueError(
                f"story #{issue.number} must carry one of the five labels and a milestone"
            )
        sections = _sections(issue.body)
        return cls(
            label=LabelName(class_labels[0]),
            milestone=MilestoneTitle(issue.milestone.title),
            description=Description.from_markdown(_section(sections, "Description")),
            condemned=Condemned.from_markdown(_section(sections, "Condemned")),
            engineering_choice=EngineeringChoice.from_markdown(
                _section(sections, "Engineering Choice")
            ),
            context=_section(sections, "Context"),
            tasks=Task.all_from(_section(sections, "Tasks")),
            definition_of_done=Task.all_from(_section(sections, "Definition of Done")),
        )

    def with_task(self, needle: str, checked: bool) -> "StoryBody":
        hits = tuple(t for t in self.tasks if needle in t.text and t.checked is not checked)
        if len(hits) != 1:
            raise ValueError(f"'{needle}' matches {len(hits)} flippable tasks; need exactly 1")
        return self.model_copy(
            update={
                "tasks": tuple(
                    Task(checked=checked, text=t.text) if t is hits[0] else t
                    for t in self.tasks
                )
            }
        )


class EpicBody(BaseModel):
    model_config = ConfigDict(frozen=True)
    label: LabelName
    outcome_description: str = Field(min_length=1)

    @property
    def markdown(self) -> str:
        # the label is GitHub metadata (chip), never body text
        return f"## Outcome Description\n\n{self.outcome_description}\n"


# ---------------------------------------------------------------------------
# Boundary — the one gh door. Serialization and raw values live here only.
# ---------------------------------------------------------------------------

OWNER = "kyzodb"
REPO = "kyzo"
PROJECT = 1
FIELDS = (
    "assignees,author,blockedBy,blocking,body,closed,closedAt,"
    "closedByPullRequestsReferences,comments,createdAt,id,isPinned,issueType,"
    "labels,milestone,number,parent,projectItems,reactionGroups,state,"
    "stateReason,subIssues,subIssuesSummary,title,updatedAt,url"
)
def _gh(*args: str, stdin: str | None = None) -> str:
    proc = subprocess.run(["gh", *args], capture_output=True, text=True, input=stdin)
    if proc.returncode != 0:
        raise RuntimeError(f"gh {args[0]} {args[1] if len(args) > 1 else ''}: {proc.stderr.strip()}")
    return proc.stdout


def fetch(number: int) -> Issue:
    raw = _gh("issue", "view", str(number), "--repo", f"{OWNER}/{REPO}", "--json", FIELDS)
    return Issue.model_validate(json.loads(raw))


def _create_issue(title: str, body_md: str, milestone: MilestoneTitle, label: LabelName | None) -> int:
    args = ["issue", "create", "--repo", f"{OWNER}/{REPO}", "--title", title,
            "--milestone", milestone.value, "--body-file", "-"]
    if label is not None:
        args += ["--label", label.value]
    url = _gh(*args, stdin=body_md).strip()
    return int(url.rsplit("/", 1)[1])


def _edit_body(number: int, body_md: str) -> None:
    _gh("issue", "edit", str(number), "--repo", f"{OWNER}/{REPO}", "--body-file", "-", stdin=body_md)


def _add_comment(number: int, text: str) -> None:
    _gh("issue", "comment", str(number), "--repo", f"{OWNER}/{REPO}", "--body", text)


def _set_milestone(number: int, milestone: MilestoneTitle) -> None:
    _gh("issue", "edit", str(number), "--repo", f"{OWNER}/{REPO}", "--milestone", milestone.value)


def _set_label(number: int, label: LabelName) -> None:
    others = ",".join(l.value for l in LabelName if l is not label)
    _gh("issue", "edit", str(number), "--repo", f"{OWNER}/{REPO}",
        "--add-label", label.value, "--remove-label", others)


_SUB_ISSUE_MUTATION = (
    "mutation($p:ID!,$c:ID!){{{op}(input:{{issueId:$p,subIssueId:$c}}){{issue{{number}}}}}}"
)


def _attach(parent: int, child: int) -> None:
    _gh("api", "graphql", "-F", f"p={fetch(parent).id}", "-F", f"c={fetch(child).id}",
        "-f", f"query={_SUB_ISSUE_MUTATION.format(op='addSubIssue')}")


def _detach(parent: int, child: int) -> None:
    _gh("api", "graphql", "-F", f"p={fetch(parent).id}", "-F", f"c={fetch(child).id}",
        "-f", f"query={_SUB_ISSUE_MUTATION.format(op='removeSubIssue')}")


def _card_id(number: int) -> str | None:
    items = json.loads(_gh("project", "item-list", str(PROJECT), "--owner", OWNER,
                           "--format", "json", "--limit", "500"))
    ids = [i["id"] for i in items["items"] if i.get("content", {}).get("number") == number]
    return ids[0] if ids else None


def _place_card(number: int) -> str:
    reply = json.loads(
        _gh("project", "item-add", str(PROJECT), "--owner", OWNER, "--format", "json",
            "--url", f"https://github.com/{OWNER}/{REPO}/issues/{number}")
    )
    return reply["id"]


def _set_column(item_id: str, column: ColumnName) -> None:
    project_id = json.loads(
        _gh("project", "view", str(PROJECT), "--owner", OWNER, "--format", "json")
    )["id"]
    fields = json.loads(
        _gh("project", "field-list", str(PROJECT), "--owner", OWNER, "--format", "json")
    )
    status = next(f for f in fields["fields"] if f["name"] == "Status")
    option = next(o for o in status["options"] if o["name"] == column.value)
    _gh("project", "item-edit", "--id", item_id, "--project-id", project_id,
        "--field-id", status["id"], "--single-select-option-id", option["id"])


# ---------------------------------------------------------------------------
# Commands — the public API. Construct one, call execute().
# Each update op is its own model and knows how to apply itself: selection
# happened when the op was constructed, so no body ever branches on a case.
# ---------------------------------------------------------------------------


class CreateEpic(BaseModel):
    model_config = ConfigDict(frozen=True)
    name: str = Field(min_length=1)
    body: EpicBody
    milestone: MilestoneTitle
    column: ColumnName

    def execute(self) -> str:
        number = _create_issue(self.name, self.body.markdown, self.milestone, self.body.label)
        _set_column(_place_card(number), self.column)
        return f"create-epic: #{number}"


class CreateStory(BaseModel):
    model_config = ConfigDict(frozen=True)
    name: str = Field(min_length=1)
    # epic is None only by operator decision at creation; parentless stories
    # surface in every review of the board.
    epic: int | None
    column: ColumnName
    body: StoryBody

    def execute(self) -> str:
        number = _create_issue(self.name, self.body.markdown, self.body.milestone, self.body.label)
        if self.epic is not None:
            _attach(self.epic, number)
        _set_column(_place_card(number), self.column)
        return f"create-story: #{number}"


class SetOutcome(BaseModel):
    model_config = ConfigDict(frozen=True)
    body: EpicBody

    def apply(self, number: int) -> None:
        _edit_body(number, self.body.markdown)
        _set_label(number, self.body.label)  # body and chip agree by construction


class AddComment(BaseModel):
    model_config = ConfigDict(frozen=True)
    comment: str = Field(min_length=1)

    def apply(self, number: int) -> None:
        _add_comment(number, self.comment)


class CheckTask(BaseModel):
    model_config = ConfigDict(frozen=True)
    task: str = Field(min_length=1)

    def apply(self, number: int) -> None:
        story = StoryBody.from_issue(fetch(number))
        _edit_body(number, story.with_task(self.task, checked=True).markdown)


class UncheckTask(BaseModel):
    model_config = ConfigDict(frozen=True)
    task: str = Field(min_length=1)

    def apply(self, number: int) -> None:
        story = StoryBody.from_issue(fetch(number))
        _edit_body(number, story.with_task(self.task, checked=False).markdown)


class ReplaceBody(BaseModel):
    model_config = ConfigDict(frozen=True)
    body: StoryBody

    def apply(self, number: int) -> None:
        _edit_body(number, self.body.markdown)
        _set_label(number, self.body.label)          # chip and milestone agree with the
        _set_milestone(number, self.body.milestone)  # contract by construction


class SetEpic(BaseModel):
    model_config = ConfigDict(frozen=True)
    epic: int

    def apply(self, number: int) -> None:
        current = fetch(number).parent
        if current is not None:
            _detach(current.number, number)
        _attach(self.epic, number)


class SetLabel(BaseModel):
    model_config = ConfigDict(frozen=True)
    label: LabelName

    def apply(self, number: int) -> None:
        _set_label(number, self.label)


class UpdateEpic(BaseModel):
    model_config = ConfigDict(frozen=True)
    number: int
    op: SetOutcome | AddComment

    def execute(self) -> str:
        self.op.apply(self.number)
        return f"update-epic: #{self.number}"


class UpdateStory(BaseModel):
    model_config = ConfigDict(frozen=True)
    number: int
    op: CheckTask | UncheckTask | ReplaceBody | AddComment | SetEpic | SetLabel

    def execute(self) -> str:
        self.op.apply(self.number)
        return f"update-story: #{self.number}"


class MoveIssue(BaseModel):
    model_config = ConfigDict(frozen=True)
    number: int
    column: ColumnName | None = None
    milestone: MilestoneTitle | None = None

    focus: bool = False  # focus = In Progress column + the "focus" state label

    def execute(self) -> str:
        if self.column is None and self.milestone is None:
            raise ValueError("move-issue needs --column and/or --milestone")
        if self.column is not None:
            item_id = _card_id(self.number)
            if item_id is None:
                raise RuntimeError(
                    f"#{self.number} has no card on the board — surface the drift, don't repair it"
                )
            _set_column(item_id, self.column)
            if self.focus:
                _gh("issue", "edit", str(self.number), "--repo", f"{OWNER}/{REPO}",
                    "--add-label", "focus")
            else:
                _gh("issue", "edit", str(self.number), "--repo", f"{OWNER}/{REPO}",
                    "--remove-label", "focus")
        if self.milestone is not None:
            _set_milestone(self.number, self.milestone)
        return f"move-issue: #{self.number}"


class DeleteIssues(BaseModel):
    model_config = ConfigDict(frozen=True)
    numbers: list[int] = Field(min_length=1)

    def execute(self) -> str:
        for n in self.numbers:
            _gh("issue", "delete", str(n), "--repo", f"{OWNER}/{REPO}", "--yes")
        return "delete-issue: " + " ".join(f"#{n}" for n in self.numbers)


class ReadIssues(BaseModel):
    model_config = ConfigDict(frozen=True)
    numbers: list[int] = Field(min_length=1)

    def execute(self) -> str:
        divider = "\n\n" + "=" * 40 + "\n\n"
        return divider.join(fetch(n).rendered for n in self.numbers)


# ---------------------------------------------------------------------------
# CLI — argv mapped onto the command models, nothing else.
# ---------------------------------------------------------------------------

COLUMNS = {"todo": ColumnName.TODO, "focus": ColumnName.IN_PROGRESS, "done": ColumnName.DONE}

STORY_FIELD_FLAGS = (
    "label", "milestone", "actor", "want", "so_that", "condemned_path",
    "condemned_reason", "closure_test", "choice", "choice_type", "consequence",
    "evidence_needed", "context",
)


def _add_story_body_flags(p: argparse.ArgumentParser, required: bool) -> None:
    for flag in STORY_FIELD_FLAGS:
        p.add_argument(f"--{flag.replace('_', '-')}", required=required)
    p.add_argument("--task", action="append", required=required)
    p.add_argument("--dod", action="append", required=required)


def _story_body(a: argparse.Namespace) -> StoryBody:
    return StoryBody(
        label=LabelName(a.label),
        milestone=MilestoneTitle(a.milestone),
        description=Description(actor=a.actor, want=a.want, so_that=a.so_that),
        condemned=Condemned(
            path=a.condemned_path, reason=a.condemned_reason, closure_test=a.closure_test
        ),
        engineering_choice=EngineeringChoice(
            choice=a.choice, choice_type=ChoiceType(a.choice_type),
            consequence=a.consequence, evidence_needed=a.evidence_needed,
        ),
        context=a.context,
        tasks=tuple(Task(checked=False, text=t) for t in a.task or ()),
        definition_of_done=tuple(Task(checked=False, text=t) for t in a.dod or ()),
    )


def _story_op(a: argparse.Namespace):
    ops = {
        "check": lambda v: CheckTask(task=v),
        "uncheck": lambda v: UncheckTask(task=v),
        "comment": lambda v: AddComment(comment=v),
        "epic": lambda v: SetEpic(epic=v),
        "label_to": lambda v: SetLabel(label=LabelName(v)),
    }
    given = {name: getattr(a, name) for name in ops if getattr(a, name, None) is not None}
    if a.replace_body:
        given["replace_body"] = True
    if len(given) != 1:
        raise SystemExit("update-story takes exactly one operation")
    if "replace_body" in given:
        return ReplaceBody(body=_story_body(a))
    name, value = next(iter(given.items()))
    return ops[name](value)


def main() -> int:
    cli = argparse.ArgumentParser(prog="manage-board.py")
    sub = cli.add_subparsers(dest="command", required=True)

    p = sub.add_parser("create-epic")
    p.add_argument("--name", required=True)
    p.add_argument("--label", required=True, choices=[l.value for l in LabelName])
    p.add_argument("--outcome", required=True)
    p.add_argument("--milestone", required=True, choices=[m.value for m in MilestoneTitle])
    p.add_argument("--column", required=True, choices=list(COLUMNS))

    p = sub.add_parser("create-story")
    p.add_argument("--name", required=True)
    p.add_argument("--epic", type=int)  # omit ONLY by operator decision
    p.add_argument("--column", required=True, choices=list(COLUMNS))
    _add_story_body_flags(p, required=True)

    p = sub.add_parser("update-epic")
    p.add_argument("number", type=int)
    g = p.add_mutually_exclusive_group(required=True)
    g.add_argument("--outcome")
    g.add_argument("--comment")
    p.add_argument("--label", choices=[l.value for l in LabelName])  # required with --outcome

    p = sub.add_parser("update-story")
    p.add_argument("number", type=int)
    p.add_argument("--check")
    p.add_argument("--uncheck")
    p.add_argument("--comment")
    p.add_argument("--epic", type=int)
    p.add_argument("--label-to", dest="label_to")
    p.add_argument("--replace-body", action="store_true")
    _add_story_body_flags(p, required=False)

    p = sub.add_parser("move-issue")
    p.add_argument("number", type=int)
    p.add_argument("--column", choices=list(COLUMNS))
    p.add_argument("--milestone", choices=[m.value for m in MilestoneTitle])

    p = sub.add_parser("delete-issue")
    p.add_argument("numbers", type=int, nargs="+")

    p = sub.add_parser("read-issue")
    p.add_argument("numbers", type=int, nargs="+")

    a = cli.parse_args()
    commands = {
        "create-epic": lambda: CreateEpic(
            name=a.name,
            body=EpicBody(label=LabelName(a.label), outcome_description=a.outcome),
            milestone=MilestoneTitle(a.milestone), column=COLUMNS[a.column],
        ),
        "create-story": lambda: CreateStory(
            name=a.name, epic=a.epic, column=COLUMNS[a.column], body=_story_body(a),
        ),
        "update-epic": lambda: UpdateEpic(
            number=a.number,
            op=SetOutcome(
                body=EpicBody(
                    label=LabelName(a.label or cli.error("--outcome requires --label")),
                    outcome_description=a.outcome,
                )
            )
            if a.outcome else AddComment(comment=a.comment),
        ),
        "update-story": lambda: UpdateStory(number=a.number, op=_story_op(a)),
        "move-issue": lambda: MoveIssue(
            number=a.number,
            column=COLUMNS[a.column] if a.column else None,
            milestone=MilestoneTitle(a.milestone) if a.milestone else None,
            focus=a.column == "focus",
        ),
        "delete-issue": lambda: DeleteIssues(numbers=a.numbers),
        "read-issue": lambda: ReadIssues(numbers=a.numbers),
    }
    print(commands[a.command]().execute())
    return 0


if __name__ == "__main__":
    sys.exit(main())
