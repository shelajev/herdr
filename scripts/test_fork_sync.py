"""Fixtures for the merge-only fork sync engine.

Every test builds real local Git repositories, so the assertions are about what
Git actually does rather than about strings the engine printed. Nothing here
touches GitHub: `prepare()` is pure local history work, which is exactly why it
is separated from `publish()`.
"""

from __future__ import annotations

import pathlib
import subprocess
import tempfile
import unittest

from scripts import fork_sync


def git(repo, *args, check=True):
    result = subprocess.run(
        ["git", "-C", str(repo), *args], capture_output=True, text=True
    )
    if check and result.returncode != 0:
        raise AssertionError(f"git {args} failed: {result.stderr}")
    return result.stdout.strip()


def commit(repo, path, content, message):
    (pathlib.Path(repo) / path).parent.mkdir(parents=True, exist_ok=True)
    (pathlib.Path(repo) / path).write_text(content)
    git(repo, "add", path)
    git(repo, "-c", "user.name=t", "-c", "user.email=t@example.com",
        "commit", "-q", "-m", message)
    return git(repo, "rev-parse", "HEAD")


class ForkSyncFixture(unittest.TestCase):
    """A fork that shares history with upstream and then diverges.

    Both remotes are bare, like the real ones: pushing into a non-bare
    checkout's current branch is refused by git, and modelling that wrong would
    make the fixtures test something other than the real topology.
    """

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = pathlib.Path(self.tmp.name)
        self.addCleanup(self.tmp.cleanup)

        # upstream: bare remote plus a working clone used to author commits.
        self.upstream = root / "upstream.git"
        git(root, "init", "-q", "--bare", "-b", "master", str(self.upstream))
        self.upstream_wc = root / "upstream-wc"
        git(root, "clone", "-q", str(self.upstream), str(self.upstream_wc))
        self.shared = commit(self.upstream_wc, "README.md", "shared\n", "feat: shared base")
        git(self.upstream_wc, "push", "-q", "origin", "master")

        # fork: seeded from upstream, then carrying fork-only work.
        self.fork = root / "fork.git"
        git(root, "clone", "-q", "--bare", str(self.upstream), str(self.fork))
        self.fork_wc = root / "fork-wc"
        git(root, "clone", "-q", str(self.fork), str(self.fork_wc))
        self.fork_only = commit(self.fork_wc, "src/tasks.rs", "fork\n", "feat: add task crew")
        git(self.fork_wc, "push", "-q", "origin", "master")

        # the working clone the sync engine operates in
        self.work = root / "work"
        git(root, "clone", "-q", str(self.fork), str(self.work))
        git(self.work, "remote", "add", "upstream", str(self.upstream))
        self.refresh()

    def refresh(self):
        git(self.work, "fetch", "-q", "--prune", "origin")
        git(self.work, "fetch", "-q", "upstream")

    def advance_upstream(self, name="feat: upstream work", path="UPSTREAM.md"):
        head = commit(self.upstream_wc, path, name, name)
        git(self.upstream_wc, "push", "-q", "origin", "master")
        self.refresh()
        return head

    def advance_fork(self, name="fix: fork work", path="FORK.md"):
        head = commit(self.fork_wc, path, name, name)
        git(self.fork_wc, "push", "-q", "origin", "master")
        self.refresh()
        return head

    def publish_branch(self, candidate):
        """Simulate the push half, so the next run sees an existing branch."""
        git(self.work, "push", "-q", "origin", f"HEAD:refs/heads/{candidate.branch}")
        self.refresh()

    def land_pull_request(self, candidate):
        """Simulate the maintainer merging the sync pull request into master."""
        git(self.fork_wc, "fetch", "-q", "origin", candidate.branch)
        git(self.fork_wc, "-c", "user.name=t", "-c", "user.email=t@example.com",
            "merge", "-q", "--no-ff", "-m", "chore: land upstream sync", "FETCH_HEAD")
        git(self.fork_wc, "push", "-q", "origin", "master")
        self.refresh()

    def use_fork_remote_url(self):
        """Point origin at the real fork URL.

        The local fixture remote is a path, which the repository guard rightly
        refuses; publish-path tests need to get past the guard to exercise what
        comes after it. Nothing is pushed: the candidate is either a no-op or
        unvalidated.
        """
        git(self.work, "remote", "set-url", "origin",
            f"https://github.com/{fork_sync.FORK_REPO}.git")

    def assert_ancestor(self, ancestor, descendant, why):
        self.assertEqual(
            0,
            subprocess.run(
                ["git", "-C", str(self.work), "merge-base", "--is-ancestor",
                 ancestor, descendant],
            ).returncode,
            why,
        )


class TestPrepare(ForkSyncFixture):
    def test_first_sync_merges_upstream_and_keeps_both_histories(self):
        upstream_head = self.advance_upstream()
        candidate = fork_sync.prepare(str(self.work))

        self.assertEqual(candidate.status, "ready")
        # Both parents are reachable: nothing was rebased away.
        for required in (candidate.fork_head, upstream_head):
            self.assert_ancestor(
                required, candidate.candidate_head,
                f"{required} must be an ancestor of the candidate",
            )
        self.assertIn(self.fork_only, candidate.fork_only_commits)

    def test_repeated_runs_are_a_no_op(self):
        self.advance_upstream()
        first = fork_sync.prepare(str(self.work))
        self.publish_branch(first)

        second = fork_sync.prepare(str(self.work))
        self.assertEqual(second.status, "up-to-date")
        self.assertEqual(second.candidate_head, first.candidate_head)
        self.assertFalse(second.needs_publish)

    def test_a_new_upstream_commit_extends_the_existing_branch(self):
        self.advance_upstream("feat: first", "ONE.md")
        first = fork_sync.prepare(str(self.work))
        self.publish_branch(first)

        self.advance_upstream("feat: second", "TWO.md")
        second = fork_sync.prepare(str(self.work))

        self.assertEqual(second.status, "ready")
        # The branch was extended, not recreated: the previous candidate is
        # still in its history, so an open pull request keeps its review.
        self.assert_ancestor(
            first.candidate_head, second.candidate_head,
            "the previous candidate must remain in the branch history",
        )

    def test_fork_master_advancing_under_an_open_branch_keeps_that_branch(self):
        """The case the review named: master moves while a sync PR is open.

        The branch must keep its history and gain a merge of fork master, rather
        than being discarded and rebuilt from master.
        """
        self.advance_upstream("feat: upstream one", "ONE.md")
        first = fork_sync.prepare(str(self.work))
        self.publish_branch(first)

        # Master moves on independently; the sync pull request is still open.
        new_fork_head = self.advance_fork("fix: urgent fork fix", "HOTFIX.md")
        second = fork_sync.prepare(str(self.work))

        self.assertEqual(second.status, "ready")
        self.assertEqual(second.fork_head, new_fork_head)
        for required, why in (
            (first.candidate_head, "the open branch's history must be retained"),
            (new_fork_head, "the new fork master commit must be merged in"),
            (self.fork_only, "original fork work must still be reachable"),
        ):
            self.assert_ancestor(required, second.candidate_head, why)

    def test_a_merged_branch_restarts_from_fork_master(self):
        """Once the pull request lands, the next run starts fresh."""
        self.advance_upstream()
        first = fork_sync.prepare(str(self.work))
        self.publish_branch(first)

        self.land_pull_request(first)

        self.advance_upstream("feat: after landing", "AFTER.md")
        second = fork_sync.prepare(str(self.work))
        self.assertEqual(second.status, "ready")
        self.assert_ancestor(
            second.fork_head, second.candidate_head,
            "the restarted branch must build on the landed fork master",
        )

    def test_a_conflict_fails_visibly_and_leaves_no_candidate(self):
        commit(self.upstream_wc, "shared.txt", "upstream side\n", "feat: upstream edit")
        git(self.upstream_wc, "push", "-q", "origin", "master")
        commit(self.fork_wc, "shared.txt", "fork side\n", "fix: fork edit")
        git(self.fork_wc, "push", "-q", "origin", "master")
        self.refresh()

        with self.assertRaises(fork_sync.SyncError) as caught:
            fork_sync.prepare(str(self.work))
        self.assertIn("conflict", str(caught.exception).lower())
        # The merge was aborted, so the working tree is usable for the next run.
        self.assertEqual("", git(self.work, "diff", "--name-only", "--diff-filter=U"))

    def test_fork_master_is_never_moved_or_rewritten(self):
        before = git(self.work, "rev-parse", "refs/remotes/origin/master")
        self.advance_upstream()
        fork_sync.prepare(str(self.work))
        self.assertEqual(before, git(self.work, "rev-parse", "refs/remotes/origin/master"))
        self.assertEqual(
            self.fork_only,
            git(self.fork, "rev-parse", "master"),
            "the fork repository's master must be untouched by preparing a candidate",
        )


class TestPublishGuards(ForkSyncFixture):
    def test_a_conflict_or_failed_check_has_no_publish_path(self):
        self.advance_upstream()
        candidate = fork_sync.prepare(str(self.work))
        self.use_fork_remote_url()
        with self.assertRaises(fork_sync.SyncError) as caught:
            fork_sync.publish(str(self.work), candidate, validated=False)
        self.assertIn("checks did not pass", str(caught.exception))

    def test_a_no_op_publishes_nothing(self):
        candidate = fork_sync.prepare(str(self.work))
        self.assertEqual(candidate.status, "up-to-date")
        self.use_fork_remote_url()
        self.assertEqual(
            "nothing to publish",
            fork_sync.publish(str(self.work), candidate, validated=True, dry_run=True),
        )

    def test_publishing_to_a_non_fork_remote_is_refused(self):
        self.advance_upstream()
        candidate = fork_sync.prepare(str(self.work))
        git(self.work, "remote", "set-url", "origin",
            "https://github.com/herdrdev/herdr.git")
        with self.assertRaises(fork_sync.SyncError) as caught:
            fork_sync.publish(str(self.work), candidate, validated=True, dry_run=True)
        self.assertIn("refusing to sync another repository", str(caught.exception))

    def test_a_commit_after_validation_is_not_published(self):
        """`validated=True` vouches for one commit, not for whatever came after."""
        self.advance_upstream()
        candidate = fork_sync.prepare(str(self.work))
        self.use_fork_remote_url()

        # Something lands in the checkout after the checks ran.
        commit(self.work, "LATE.md", "unvalidated\n", "fix: sneaked in after validation")
        self.assertNotEqual(
            candidate.candidate_head, git(self.work, "rev-parse", "HEAD")
        )

        with self.assertRaises(fork_sync.SyncError) as caught:
            fork_sync.publish(str(self.work), candidate, validated=True, dry_run=True)
        self.assertIn("refusing to publish an unvalidated commit", str(caught.exception))

    def test_a_dirty_checkout_is_not_published(self):
        """An edited tree would push something other than what was validated."""
        self.advance_upstream()
        candidate = fork_sync.prepare(str(self.work))
        self.use_fork_remote_url()

        (pathlib.Path(self.work) / "src" / "tasks.rs").write_text("edited after validation\n")

        with self.assertRaises(fork_sync.SyncError) as caught:
            fork_sync.publish(str(self.work), candidate, validated=True, dry_run=True)
        self.assertIn("uncommitted changes", str(caught.exception))

    def test_an_untouched_validated_candidate_still_publishes(self):
        """The drift guard must not block the normal path."""
        self.advance_upstream()
        candidate = fork_sync.prepare(str(self.work))
        self.use_fork_remote_url()
        self.assertIn(
            candidate.candidate_head,
            fork_sync.publish(str(self.work), candidate, validated=True, dry_run=True),
        )

    def test_only_the_fork_repository_is_accepted(self):
        for url in (
            "https://github.com/shelajev/herdr",
            "https://github.com/shelajev/herdr.git",
            "git@github.com:shelajev/herdr.git",
            "ssh://git@github.com/shelajev/herdr",
        ):
            git(self.work, "remote", "set-url", "origin", url)
            fork_sync.assert_fork_repository(str(self.work))

        for url in (
            "https://github.com/herdrdev/herdr.git",
            # A lookalike path on another host, which a suffix match would accept.
            "https://example.com/shelajev/herdr.git",
            "https://github.com/attacker/shelajev/herdr.git",
            "git@gitlab.com:shelajev/herdr.git",
        ):
            git(self.work, "remote", "set-url", "origin", url)
            with self.assertRaises(fork_sync.SyncError, msg=f"{url} must be refused"):
                fork_sync.assert_fork_repository(str(self.work))


class TestMergeSubject(unittest.TestCase):
    def test_generated_merge_subjects_satisfy_the_commit_linter(self):
        """Merge subjects are validated like any other subject on a push."""
        from scripts import conventional_commits

        for subject in (
            fork_sync.MERGE_SUBJECT.format(upstream_short="5da0a01e"),
            "chore: merge fork master dd4e3770",
        ):
            self.assertTrue(
                conventional_commits.valid_subject(subject),
                f"{subject!r} would fail the conventional-commits job",
            )


if __name__ == "__main__":
    unittest.main()
