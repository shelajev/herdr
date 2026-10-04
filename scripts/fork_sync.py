#!/usr/bin/env python3
"""Prepare a merge-only upstream sync candidate for the shelajev/herdr fork.

The fork carries work upstream does not have (the `herdr task` crew commands,
the crew kit, the build repairs). Keeping it current must therefore *merge*
upstream into the fork, never rewrite the fork onto upstream: a rebase, squash,
reset, or force-push would silently drop fork commits, and a fork whose history
has been rewritten cannot be reviewed against what was tested.

This module is deliberately split in two:

* `prepare()` does all the Git work in a local clone and returns a `Candidate`
  describing the result. It never talks to GitHub and never pushes, so the
  fixtures can exercise every interesting history without network access.
* `publish()` pushes that candidate and opens or updates one pull request. It
  refuses to run on a candidate that is not clean and validated.

Keeping them apart is the point: a conflict or a failed check cannot reach a
publish path, because `publish()` has nothing to publish unless `prepare()`
already succeeded and the caller recorded that validation passed.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import pathlib
import subprocess
import sys

#: The only repository this tool may touch. Running it anywhere else is a bug,
#: not a configuration option: the point is to sync *our* fork from upstream,
#: and pushing a generated branch into another repository would be an unasked
#: for write to someone else's project.
FORK_REPO = "shelajev/herdr"
UPSTREAM_REPO = "herdrdev/herdr"

#: One stable generated branch, reused across runs so the pull request stays a
#: single rolling review rather than a new one every night.
SYNC_BRANCH = "automation/upstream-sync"

#: Merge subjects must pass scripts/conventional_commits.py, which validates
#: every subject in a pushed range and has no merge exemption.
MERGE_SUBJECT = "chore: merge upstream {upstream_short}"


class SyncError(RuntimeError):
    """A sync that must fail visibly rather than produce a partial result."""


@dataclasses.dataclass
class Candidate:
    """The outcome of preparing a sync candidate."""

    #: "up-to-date" when upstream is already merged, "ready" when a new merge
    #: commit exists on the sync branch.
    status: str
    fork_head: str
    upstream_head: str
    #: Commit at the tip of the sync branch; equal to `fork_head` for a no-op.
    candidate_head: str
    branch: str = SYNC_BRANCH
    #: Fork commits that are not in upstream, i.e. what the merge must preserve.
    fork_only_commits: tuple[str, ...] = ()

    @property
    def needs_publish(self) -> bool:
        return self.status == "ready"

    def to_json(self) -> str:
        return json.dumps(dataclasses.asdict(self), indent=2, sort_keys=True)

    @classmethod
    def from_json(cls, raw: str) -> "Candidate":
        data = json.loads(raw)
        data["fork_only_commits"] = tuple(data.get("fork_only_commits", ()))
        return cls(**data)


def git(repo: str, *args: str, check: bool = True) -> str:
    """Run one git command in `repo` and return its stdout."""
    result = subprocess.run(
        ["git", "-C", repo, *args],
        capture_output=True,
        text=True,
    )
    if check and result.returncode != 0:
        raise SyncError(
            f"git {' '.join(args)} failed ({result.returncode}): {result.stderr.strip()}"
        )
    return result.stdout


def _rev(repo: str, ref: str) -> str:
    return git(repo, "rev-parse", ref).strip()


def _ref_exists(repo: str, ref: str) -> bool:
    result = subprocess.run(
        ["git", "-C", repo, "rev-parse", "--verify", "--quiet", ref],
        capture_output=True,
        text=True,
    )
    return result.returncode == 0


def _is_ancestor(repo: str, ancestor: str, descendant: str) -> bool:
    result = subprocess.run(
        ["git", "-C", repo, "merge-base", "--is-ancestor", ancestor, descendant],
        capture_output=True,
        text=True,
    )
    return result.returncode == 0


#: Exactly the forms git writes for this fork's remote. A suffix match would
#: accept `github.com/attacker/not-shelajev/herdr` or an entirely different
#: host, so the host and path are both pinned.
_ALLOWED_REMOTE_URLS = frozenset(
    {
        f"https://github.com/{FORK_REPO}",
        f"https://github.com/{FORK_REPO}.git",
        f"ssh://git@github.com/{FORK_REPO}",
        f"ssh://git@github.com/{FORK_REPO}.git",
        f"git@github.com:{FORK_REPO}",
        f"git@github.com:{FORK_REPO}.git",
    }
)


def assert_fork_repository(repo: str, remote: str = "origin") -> None:
    """Refuse to operate on anything but the fork.

    Checked against the configured remote URL rather than a CI variable so the
    guard holds when the script is run by hand too. The comparison is against an
    exact set of spellings: `endswith` would accept a lookalike path on any
    host, and this function is the only thing standing between a generated
    branch and someone else's repository.
    """
    url = git(repo, "remote", "get-url", remote).strip()
    if url not in _ALLOWED_REMOTE_URLS:
        raise SyncError(
            f"{remote} is {url!r}, which is not github.com/{FORK_REPO}; "
            "refusing to sync another repository"
        )


def prepare(
    repo: str,
    *,
    fork_ref: str = "origin/master",
    upstream_ref: str = "upstream/master",
    branch: str = SYNC_BRANCH,
) -> Candidate:
    """Build or update the sync branch by merging upstream into the fork.

    Returns a `Candidate`. Raises `SyncError` on conflict, so a conflicted merge
    can never be mistaken for a publishable result.
    """
    fork_head = _rev(repo, fork_ref)
    upstream_head = _rev(repo, upstream_ref)

    # Where does the rolling branch start? Reuse the existing branch whenever it
    # exists, so an open pull request keeps its history and its review instead of
    # being replaced by a fresh candidate every time anything moves.
    #
    # The previous version of this only reused the branch when fork master was
    # already an ancestor of it, which made the "fork master advanced" case
    # below unreachable and silently discarded the branch's history in exactly
    # the situation it matters: an open sync pull request while master moves.
    existing = None
    if _ref_exists(repo, f"refs/remotes/origin/{branch}"):
        existing = _rev(repo, f"refs/remotes/origin/{branch}")

    # A branch that is fully merged into fork master has served its purpose (the
    # pull request landed). Start the next one from master rather than extending
    # a branch with nothing left to offer.
    if existing is not None and _is_ancestor(repo, existing, fork_head):
        existing = None

    base = existing if existing is not None else fork_head

    if _is_ancestor(repo, upstream_head, base) and _is_ancestor(repo, fork_head, base):
        # Already merged. Report a no-op rather than creating an empty merge, so
        # repeated runs are idempotent and do not churn the pull request.
        return Candidate(
            status="up-to-date",
            fork_head=fork_head,
            upstream_head=upstream_head,
            candidate_head=base,
            branch=branch,
            fork_only_commits=fork_only_commits(repo, fork_head, upstream_head),
        )

    git(repo, "checkout", "-B", branch, base)
    if not _is_ancestor(repo, fork_head, base):
        # Fork master moved on under the branch. Bring it in by merging, never by
        # resetting the branch onto it: the branch's own merge history is what a
        # reviewer has already looked at.
        _merge(repo, fork_head, f"chore: merge fork master {fork_head[:8]}")
    if not _is_ancestor(repo, upstream_head, _rev(repo, "HEAD")):
        _merge(repo, upstream_head, MERGE_SUBJECT.format(upstream_short=upstream_head[:8]))

    candidate_head = _rev(repo, "HEAD")
    for required in (fork_head, upstream_head):
        if not _is_ancestor(repo, required, candidate_head):
            raise SyncError(
                f"{required} is not an ancestor of the prepared candidate; refusing to continue"
            )
    return Candidate(
        status="ready",
        fork_head=fork_head,
        upstream_head=upstream_head,
        candidate_head=candidate_head,
        branch=branch,
        fork_only_commits=fork_only_commits(repo, fork_head, upstream_head),
    )


def _merge(repo: str, ref: str, subject: str) -> None:
    result = subprocess.run(
        ["git", "-C", repo, "merge", "--no-ff", "-m", subject, ref],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        conflicts = git(repo, "diff", "--name-only", "--diff-filter=U", check=False).split()
        git(repo, "merge", "--abort", check=False)
        raise SyncError(
            "upstream merge conflicts and needs a human: "
            + (", ".join(conflicts) if conflicts else result.stderr.strip())
        )


def fork_only_commits(repo: str, fork_ref: str, upstream_ref: str) -> tuple[str, ...]:
    """Fork commits not contained in upstream — what the sync must preserve."""
    output = git(repo, "rev-list", f"{upstream_ref}..{fork_ref}", check=False)
    return tuple(line.strip() for line in output.splitlines() if line.strip())


def publish(
    repo: str,
    candidate: Candidate,
    *,
    validated: bool,
    remote: str = "origin",
    dry_run: bool = False,
) -> str:
    """Push the candidate branch and open or update one pull request.

    `validated` must be the real outcome of running the checks against
    `candidate.candidate_head`. There is no default: a caller that has not run
    the checks cannot accidentally publish.
    """
    assert_fork_repository(repo, remote)
    if not candidate.needs_publish:
        return "nothing to publish"
    if not validated:
        raise SyncError(
            "refusing to publish a candidate whose checks did not pass; "
            "fix the failure or let the sync fail visibly"
        )

    # `validated` describes one exact commit. Between the checks and this push
    # the working clone may have moved on — another step committed, something
    # wrote to the tree, a retry prepared a newer candidate. Pushing then would
    # publish code nothing ran the checks against, with a green run to vouch for
    # it. Re-read the state and refuse on any drift.
    head = _rev(repo, "HEAD")
    if head != candidate.candidate_head:
        raise SyncError(
            f"the checkout is at {head} but the validated candidate is "
            f"{candidate.candidate_head}; refusing to publish an unvalidated commit"
        )
    dirty = git(repo, "status", "--porcelain").strip()
    if dirty:
        raise SyncError(
            "the checkout has uncommitted changes, so the pushed branch would not be "
            f"what was validated: {dirty.splitlines()[0]}"
        )

    if dry_run:
        return f"would push {candidate.candidate_head} to {remote}/{candidate.branch}"

    # A plain (non-forced) push. If someone else advanced the branch meanwhile,
    # git rejects this and the run fails visibly instead of overwriting them.
    git(repo, "push", remote, f"HEAD:refs/heads/{candidate.branch}")
    return f"pushed {candidate.candidate_head} to {remote}/{candidate.branch}"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default=".")
    subcommands = parser.add_subparsers(dest="command", required=True)

    check = subcommands.add_parser(
        "check-repository", help="verify the checkout is the fork and exit"
    )
    check.add_argument("--remote", default="origin")

    prepare_parser = subcommands.add_parser(
        "prepare", help="build or update the sync branch locally"
    )
    prepare_parser.add_argument("--fork-ref", default="origin/master")
    prepare_parser.add_argument("--upstream-ref", default="upstream/master")
    prepare_parser.add_argument("--branch", default=SYNC_BRANCH)
    prepare_parser.add_argument(
        "--out", help="write the candidate JSON here instead of stdout"
    )

    publish_parser = subcommands.add_parser(
        "publish", help="push a validated candidate to the fork"
    )
    publish_parser.add_argument("--candidate", required=True)
    publish_parser.add_argument("--remote", default="origin")
    publish_parser.add_argument(
        "--validated",
        action="store_true",
        help="assert the checks actually passed against this candidate",
    )
    publish_parser.add_argument("--dry-run", action="store_true")

    args = parser.parse_args(argv)

    try:
        if args.command == "check-repository":
            assert_fork_repository(args.repo, args.remote)
            print(f"{args.repo} is {FORK_REPO}")
            return 0

        if args.command == "prepare":
            candidate = prepare(
                args.repo,
                fork_ref=args.fork_ref,
                upstream_ref=args.upstream_ref,
                branch=args.branch,
            )
            rendered = candidate.to_json()
            if args.out:
                pathlib.Path(args.out).write_text(rendered, encoding="utf-8")
                print(f"status={candidate.status}")
                print(f"head={candidate.candidate_head}")
            else:
                print(rendered)
            return 0

        candidate = Candidate.from_json(
            pathlib.Path(args.candidate).read_text(encoding="utf-8")
        )
        print(
            publish(
                args.repo,
                candidate,
                validated=args.validated,
                remote=args.remote,
                dry_run=args.dry_run,
            )
        )
        return 0
    except SyncError as error:
        print(f"fork sync failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
