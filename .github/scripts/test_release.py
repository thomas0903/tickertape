import argparse
import hashlib
import io
import json
from pathlib import Path
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
            for name in release.release_files("v0.4.0") - {"tickertape-v0.4.0-x86_64-apple-darwin.cdx.json"}:
                Path(temporary, name).write_text(name)
            with self.assertRaisesRegex(ValueError, "SBOM for each release target"):
                release.github_release(args)

    def release_run(self, existing_assets, uploads=None):
        """Run github_release against a fake existing release; return the `gh release upload` calls."""
        with tempfile.TemporaryDirectory() as temporary, \
             patch.dict(release.os.environ, GITHUB_REPOSITORY="owner/repo"):
            artifacts = Path(temporary, "dist")
            artifacts.mkdir()
            for name in release.release_files("v0.4.0"):
                (artifacts / name).write_text(name)
            sums = "".join(f"{hashlib.sha256(name.encode()).hexdigest()}  {name}\n"
                           for name in sorted(release.release_files("v0.4.0")))
            existing_assets = {name: (sums if content is None else content)
                               for name, content in existing_assets.items()}
            uploads = [] if uploads is None else uploads

            def run(command, check):
                if command[:3] == ["gh", "release", "download"]:
                    name = command[command.index("--pattern") + 1]
                    Path(command[command.index("--dir") + 1], name).write_text(existing_assets[name])
                elif command[:3] == ["gh", "release", "upload"]:
                    uploads.append(command)

            published = {"tag_name": "v0.4.0", "draft": False, "assets": [{"name": n} for n in existing_assets]}
            args = argparse.Namespace(tag="v0.4.0", artifacts=artifacts, create=False, notes=None)
            with patch.object(release, "gh_json", return_value=[[published]]), \
                 patch.object(release.subprocess, "run", side_effect=run):
                release.github_release(args)
            return uploads

    def test_backfill_adds_missing_files_and_extends_checksums(self):
        old = "tickertape-v0.4.0-x86_64-apple-darwin.tar.gz"
        sums = f"{hashlib.sha256(old.encode()).hexdigest()}  {old}\n"
        uploads = self.release_run({old: old, "SHA256SUMS": sums})
        uploaded = [Path(command[4]).name for command in uploads]
        self.assertNotIn(old, uploaded)
        self.assertEqual(set(uploaded), release.release_files("v0.4.0") - {old} | {"SHA256SUMS"})
        self.assertEqual(uploaded[-1], "SHA256SUMS")
        self.assertIn("--clobber", uploads[-1])
        self.assertTrue(all("--clobber" not in command for command in uploads[:-1]))
        self.assertEqual(self.release_run({name: name for name in release.release_files("v0.4.0")}
                                          | {"SHA256SUMS": None}), [])

    def test_a_differing_asset_stops_the_run_before_any_upload(self):
        # The differing archive sorts last among the archives, after files that would be new.
        for assets in ({"tickertape-v0.4.0-x86_64-unknown-linux-musl.tar.gz": "other bytes"},
                       {"SHA256SUMS": "0" * 64 + "  tickertape-v0.4.0-x86_64-apple-darwin.tar.gz\n"}):
            uploads = []
            with self.subTest(assets=list(assets)), self.assertRaisesRegex(ValueError, "differs"):
                self.release_run(assets, uploads)
            self.assertEqual(uploads, [])

    def test_release_files_cover_every_target_including_static_linux(self):
        files = release.release_files("v0.5.0")
        self.assertEqual(len(files), 2 * len(release.TARGETS))
        for target in ("x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"):
            self.assertIn(f"tickertape-v0.5.0-{target}.tar.gz", files)
            self.assertIn(f"tickertape-v0.5.0-{target}.cdx.json", files)

    def test_checksums_may_only_gain_lines(self):
        old = "a  one.tar.gz\nb  two.tar.gz\n"
        self.assertTrue(release.checksums_extend(old, old))
        self.assertTrue(release.checksums_extend(old, "a  one.tar.gz\nc  three.tar.gz\nb  two.tar.gz\n"))
        self.assertFalse(release.checksums_extend(old, "a  one.tar.gz\nx  two.tar.gz\n"))
        self.assertFalse(release.checksums_extend(old, "a  one.tar.gz\n"))

    def test_readme_states_each_targets_runtime_requirement(self):
        self.assertIn("statically linked", release.requirement("x86_64-unknown-linux-musl"))
        self.assertIn("glibc 2.35", release.requirement("aarch64-unknown-linux-gnu"))
        self.assertIn("macOS 11", release.requirement("x86_64-apple-darwin"))


class BinstallTests(unittest.TestCase):
    """The cargo-binstall templates in the CLI's Cargo.toml must resolve to the archives `package` writes."""

    source = Path(__file__).resolve().parents[2]

    def copy_with_metadata(self, root, metadata):
        (root / "crates" / "tickertape-cli").mkdir(parents=True)
        (root / "Cargo.toml").write_bytes((self.source / "Cargo.toml").read_bytes())
        manifest = (self.source / "crates" / "tickertape-cli" / "Cargo.toml").read_text()
        # The binstall tables come last in the manifest; keep everything before them.
        manifest = manifest.split("\n[package.metadata.binstall]", 1)[0]
        (root / "crates" / "tickertape-cli" / "Cargo.toml").write_text(manifest + metadata)

    def test_repository_metadata_matches_the_release_archive_names(self):
        self.assertTrue(release.check_binstall(self.source, "v0.4.0"))
        self.assertTrue(release.check_binstall(self.source, "v12.34.56"))

    def test_linux_resolves_to_the_static_musl_archive(self):
        metadata = release.binstall_metadata(self.source)
        for gnu, musl in release.LINUX_INSTALL_TARGETS.items():
            self.assertIn(musl, metadata["overrides"][gnu]["pkg-url"])
        self.assertIn("quick-install", metadata["disabled-strategies"])

    def test_tags_without_metadata_are_not_errors(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.copy_with_metadata(root, "")
            self.assertIsNone(release.binstall_metadata(root))
            self.assertFalse(release.check_binstall(root, "v0.4.0"))

    def test_drift_between_templates_and_archive_names_fails(self):
        good = ('\n[package.metadata.binstall]\n'
                'pkg-url = "{ repo }/releases/download/v{ version }/tickertape-v{ version }-{ target }.tar.gz"\n'
                'bin-dir = "tickertape-v{ version }-{ target }/{ bin }{ binary-ext }"\npkg-fmt = "tgz"\n')
        overrides = "".join(
            f'[package.metadata.binstall.overrides.{gnu}]\n'
            f'pkg-url = "{{ repo }}/releases/download/v{{ version }}/tickertape-v{{ version }}-{musl}.tar.gz"\n'
            f'bin-dir = "tickertape-v{{ version }}-{musl}/{{ bin }}"\n'
            for gnu, musl in release.LINUX_INSTALL_TARGETS.items())
        cases = {
            "good": (good + overrides, None),
            "no tag prefix": (good.replace("download/v{ version }", "download/{ version }") + overrides, "pkg-url"),
            "renamed archive": (good.replace("tickertape-v{ version }-{ target }.tar", "tt-{ target }.tar")
                                + overrides, "pkg-url"),
            "wrong directory": (good.replace('"tickertape-v{ version }-{ target }/', '"{ target }/') + overrides,
                                "bin-dir"),
            "glibc on Linux": (good, "x86_64-unknown-linux-gnu"),
            "unknown placeholder": (good.replace("{ target }.tar", "{ target-libc }.tar") + overrides,
                                    "placeholder"),
            "zip": (good.replace('"tgz"', '"zip"') + overrides, "pkg-fmt"),
        }
        for name, (metadata, error) in cases.items():
            with self.subTest(name), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                self.copy_with_metadata(root, metadata)
                if error is None:
                    self.assertTrue(release.check_binstall(root, "v0.4.0"))
                else:
                    with self.assertRaisesRegex(ValueError, error):
                        release.check_binstall(root, "v0.4.0")


if __name__ == "__main__":
    unittest.main()
