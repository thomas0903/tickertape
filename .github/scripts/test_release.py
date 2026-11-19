import argparse
import hashlib
import io
import json
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import release


class ReleaseTests(unittest.TestCase):
    def test_tags_are_versions_not_arbitrary_refs(self):
        self.assertEqual(release.version_for_tag("v0.4.0"), "0.4.0")
        for tag in ("main", "v01.2.3", "v0.4.0;echo unsafe", "v0.4.0\n", "v0.4.0-beta.1"):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release.version_for_tag(tag)

    def test_tag_commit_resolves_tags_only(self):
        def git(*args, cwd):
            return subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid", *args],
                                  cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()

        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary)
            git("init", "-q", cwd=repo)
            git("commit", "-q", "--allow-empty", "-m", "first", cwd=repo)
            first = git("rev-parse", "HEAD", cwd=repo)
            git("commit", "-q", "--allow-empty", "-m", "second", cwd=repo)
            second = git("rev-parse", "HEAD", cwd=repo)
            # A branch named like a release tag is not a tag.
            git("branch", "v0.4.0", first, cwd=repo)
            with self.assertRaisesRegex(ValueError, "not a tag"):
                release.tag_commit(repo, "v0.4.0")
            # With both a branch and a tag of that name, the tag wins (annotated tags resolve to their commit).
            git("tag", "-a", "v0.4.0", second, "-m", "release", cwd=repo)
            self.assertEqual(release.tag_commit(repo, "v0.4.0"), second)
            git("tag", "v0.5.0", first, cwd=repo)
            self.assertEqual(release.tag_commit(repo, "v0.5.0"), first)

    def test_prepare_refuses_a_branch_named_like_the_tag(self):
        with tempfile.TemporaryDirectory() as temporary:
            repo = Path(temporary)
            subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
            subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid", "commit", "-q",
                            "--allow-empty", "-m", "c"], cwd=repo, check=True)
            subprocess.run(["git", "branch", "v0.4.0"], cwd=repo, check=True)
            args = argparse.Namespace(source=repo, tag="v0.4.0", notes=repo / "notes.md")
            with patch.object(release, "validate_versions"), \
                 patch.object(release, "gh_json") as gh, \
                 self.assertRaisesRegex(ValueError, "not a tag"):
                release.prepare(args)
            gh.assert_not_called()

    def test_version_validation_checks_both_workspace_versions(self):
        source = Path(__file__).resolve().parents[2]
        with (source / "Cargo.toml").open("rb") as file:
            version = release.tomllib.load(file)["workspace"]["package"]["version"]
        release.validate_versions(source, version)
        with self.assertRaises(ValueError):
            release.validate_versions(source, "999.0.0")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "Cargo.toml").write_text('[workspace.package]\nversion="0.4.0"\n'
                                             '[workspace.dependencies]\ntickertape={version="0.3.0"}\n')
            with self.assertRaisesRegex(ValueError, "dependency"):
                release.validate_versions(root, "0.4.0")

    def test_python_bindings_version_and_lockfile_follow_the_tag(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            release.validate_bindings_version(root, "0.5.0")  # older tags have no bindings
            bindings = root / "bindings" / "python"
            bindings.mkdir(parents=True)
            (bindings / "Cargo.toml").write_text('[package]\nname="tickertape-python"\nversion="0.4.0"\n')
            with self.assertRaisesRegex(ValueError, "Cargo.toml version"):
                release.validate_bindings_version(root, "0.5.0")
            (bindings / "Cargo.toml").write_text('[package]\nname="tickertape-python"\nversion="0.5.0"\n')
            lock = '[[package]]\nname="tickertape"\nversion="{}"\n[[package]]\nname="tickertape-python"\nversion="0.5.0"\n'
            (bindings / "Cargo.lock").write_text(lock.format("0.4.0"))
            with self.assertRaisesRegex(ValueError, "stale"):
                release.validate_bindings_version(root, "0.5.0")
            (bindings / "Cargo.lock").write_text(lock.format("0.5.0"))
            release.validate_bindings_version(root, "0.5.0")

    def test_unreleased_workspace_members_must_not_be_publishable(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "Cargo.toml").write_text('[workspace]\nmembers=["crates/tickertape","crates/tickertape-cli",'
                                             '"crates/extra"]\n[workspace.package]\nversion="0.4.0"\n'
                                             '[workspace.dependencies]\ntickertape={version="0.4.0"}\n')
            for name in ("tickertape", "tickertape-cli", "extra"):
                (root / "crates" / name).mkdir(parents=True)
                (root / "crates" / name / "Cargo.toml").write_text(
                    f'[package]\nname="{name}"\nversion.workspace=true\n')
            with self.assertRaisesRegex(ValueError, "extra is not released"):
                release.validate_versions(root, "0.4.0")
            (root / "crates/extra/Cargo.toml").write_text(
                '[package]\nname="extra"\nversion.workspace=true\npublish=false\n')
            release.validate_versions(root, "0.4.0")

    def test_ci_must_be_the_latest_main_push_for_this_commit(self):
        green = dict(head_sha="abc", head_branch="main", event="push", run_number=1,
                     status="completed", conclusion="success")
        release.require_green_ci([green], "abc")
        for runs in ([], [dict(green, event="pull_request")], [dict(green, head_sha="other")],
                     [dict(green, head_branch="feature")],
                     [green, dict(green, run_number=2, conclusion="failure")],
                     [green, dict(green, run_number=2, status="in_progress", conclusion=None)]):
            with self.subTest(runs=runs), self.assertRaises(ValueError):
                release.require_green_ci(runs, "abc")

    def test_notes_include_only_the_selected_version(self):
        changelog = "# Changelog\n## [0.4.0] - date\nNew stuff\n\n## [0.3.0] - date\nOld stuff\n[0.3.0]: url\n"
        self.assertIn("New stuff", release.release_notes(changelog, "0.4.0"))
        self.assertNotIn("Old stuff", release.release_notes(changelog, "0.4.0"))
        self.assertNotIn("[0.3.0]: url", release.release_notes(changelog, "0.3.0"))
        with self.assertRaises(ValueError):
            release.release_notes(changelog, "0.5.0")

    def test_published_archive_must_match_registry_checksum_and_clean_source(self):
        for dirty in (False, True):
            with self.subTest(dirty=dirty):
                archive = io.BytesIO()
                vcs = json.dumps({"git": {"sha1": "abc", "dirty": dirty}}).encode()
                with tarfile.open(fileobj=archive, mode="w:gz") as tar:
                    info = tarfile.TarInfo("tickertape-0.4.0/.cargo_vcs_info.json")
                    info.size = len(vcs)
                    tar.addfile(info, io.BytesIO(vcs))
                data = archive.getvalue()
                metadata = {"version": {"yanked": False, "checksum": hashlib.sha256(data).hexdigest()}}
                with patch.object(release, "fetch_json", return_value=metadata), \
                     patch.object(release, "fetch", return_value=io.BytesIO(data)):
                    if dirty:
                        with self.assertRaisesRegex(ValueError, "dirty"):
                            release.published_source("tickertape", "0.4.0")
                    else:
                        self.assertEqual(release.published_source("tickertape", "0.4.0"), "abc")
                metadata["version"]["checksum"] = "wrong"
                with patch.object(release, "fetch_json", return_value=metadata), \
                     patch.object(release, "fetch", return_value=io.BytesIO(data)):
                    with self.assertRaisesRegex(ValueError, "checksum"):
                        release.published_source("tickertape", "0.4.0")

    def test_reruns_skip_matching_publications_and_refuse_other_source(self):
        args = argparse.Namespace(source=Path("source"), version="0.4.0", sha="abc")
        with patch.object(release, "validate_versions"), \
             patch.object(release, "published_source", return_value="abc"), \
             patch.object(release.subprocess, "run") as run:
            release.publish(args)
            run.assert_not_called()
        with patch.object(release, "validate_versions"), \
             patch.object(release, "published_source", return_value="other"), \
             patch.object(release.subprocess, "run") as run:
            with self.assertRaisesRegex(ValueError, "different commit"):
                release.publish(args)
            run.assert_not_called()

    def test_package_has_executable_and_licences_and_is_reproducible(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary)
            target = release.TARGETS[0]
            binary = source / "target" / target / "release/tickertape"
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"a CLI binary")
            for name in ("LICENSE-MIT", "LICENSE-APACHE"):
                (source / name).write_text(name)
            args = argparse.Namespace(source=source, tag="v0.4.0", target=target, output=source / "dist")
            with patch.object(release, "run", return_value="1234567890"):
                release.package(args)
                archive = next(args.output.glob("*.tar.gz"))
                first = archive.read_bytes()
                release.package(args)
                self.assertEqual(first, archive.read_bytes())
            with tarfile.open(archive) as tar:
                self.assertEqual({Path(member.name).name for member in tar},
                                 {"tickertape", "LICENSE-MIT", "LICENSE-APACHE", "README.txt"})
                executable = next(member for member in tar.getmembers() if member.name.endswith("/tickertape"))
                self.assertEqual(executable.mode, 0o755)

    def test_release_rejects_incomplete_binary_sets(self):
        with tempfile.TemporaryDirectory() as temporary, \
             patch.dict(release.os.environ, GITHUB_REPOSITORY="owner/repo"):
            args = argparse.Namespace(tag="v0.4.0", artifacts=Path(temporary), create=False)
            with self.assertRaisesRegex(ValueError, "each release target"):
                release.github_release(args)


if __name__ == "__main__":
    unittest.main()
