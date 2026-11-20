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
            args = argparse.Namespace(tag="v0.4.0", artifacts=Path(temporary), create=False, c_library=True)
            with self.assertRaisesRegex(ValueError, "each release target"):
                release.github_release(args)
            for name in release.release_files("v0.4.0") - {"tickertape-v0.4.0-x86_64-apple-darwin.cdx.json"}:
                Path(temporary, name).write_text(name)
            with self.assertRaisesRegex(ValueError, "SBOM for each release target"):
                release.github_release(args)
            Path(temporary, "tickertape-v0.4.0-x86_64-apple-darwin.cdx.json").write_text("sbom")
            # A tag from before the C library archives: the C files are then unexpected.
            args.c_library = False
            with self.assertRaisesRegex(ValueError, "each release target"):
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
            args = argparse.Namespace(tag="v0.4.0", artifacts=artifacts, create=False, notes=None, c_library=True)
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
        files = release.release_files("v0.5.0", c_library=False)
        self.assertEqual(len(files), 2 * len(release.TARGETS))
        for target in ("x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"):
            self.assertIn(f"tickertape-v0.5.0-{target}.tar.gz", files)
            self.assertIn(f"tickertape-v0.5.0-{target}.cdx.json", files)

    def test_release_files_include_a_c_archive_and_sbom_per_c_target(self):
        files = release.release_files("v0.5.0")
        self.assertEqual(len(files), 2 * len(release.TARGETS) + 2 * len(release.C_TARGETS))
        self.assertEqual(set(release.C_TARGETS), set(release.JAVA_PLATFORMS))
        for target in release.C_TARGETS:
            self.assertIn(f"tickertape-c-v0.5.0-{target}.tar.gz", files)
            self.assertIn(f"tickertape-c-v0.5.0-{target}.cdx.json", files)
        self.assertFalse(any("-c-" in name and "musl" in name for name in files))

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


class CLibraryTests(unittest.TestCase):
    def test_soname_has_the_minor_version_before_1_0_and_the_major_version_after(self):
        self.assertEqual(release.shared_library_names("0.4.0", "x86_64-unknown-linux-gnu"),
                         ("libtickertape.so.0.4.0", "libtickertape.so.0.4", "libtickertape.so"))
        self.assertEqual(release.shared_library_names("1.2.3", "aarch64-unknown-linux-gnu"),
                         ("libtickertape.so.1.2.3", "libtickertape.so.1", "libtickertape.so"))
        self.assertEqual(release.shared_library_names("0.4.0", "aarch64-apple-darwin"),
                         ("libtickertape.0.4.0.dylib", "libtickertape.0.4.dylib", "libtickertape.dylib"))
        self.assertEqual(release.shared_library_names("1.2.3", "x86_64-apple-darwin")[1], "libtickertape.1.dylib")

    def test_build_sets_the_soname_or_install_name_and_builds_both_libraries(self):
        linux = release.c_build_command("0.4.0", "x86_64-unknown-linux-gnu")
        self.assertIn("link-arg=-Wl,-soname,libtickertape.so.0.4", linux)
        self.assertEqual(linux[linux.index("--crate-type") + 1], "cdylib,staticlib")
        self.assertIn("--locked", linux)
        macos = release.c_build_command("1.0.0", "aarch64-apple-darwin")
        self.assertIn("link-arg=-Wl,-install_name,@rpath/libtickertape.1.dylib", macos)

    def test_native_static_libs_come_from_rustcs_note(self):
        output = "   Compiling tickertape-ffi\nnote: native-static-libs: -lgcc_s -lutil -lc\n\n    Finished\n"
        self.assertEqual(release.native_static_libs(output), "-lgcc_s -lutil -lc")
        coloured = "note: native-static-libs: -lSystem -lc -lm\x1b[0m\n\x1b[1m\x1b[92m    Finished\x1b[0m\n"
        self.assertEqual(release.native_static_libs(coloured), "-lSystem -lc -lm")
        with self.assertRaisesRegex(ValueError, "native-static-libs"):
            release.native_static_libs("    Finished `release` profile\n")

    def test_unsupported_targets_are_refused(self):
        for function in (release.c_build, release.c_package):
            args = argparse.Namespace(source=Path("."), tag="v0.4.0", target="x86_64-unknown-linux-musl",
                                      output=Path("dist"))
            with self.subTest(function=function.__name__), self.assertRaisesRegex(ValueError, "C library target"):
                function(args)

    def test_pkg_config_files_are_relocatable(self):
        shared = release.pkg_config("0.4.0", "-lgcc_s -lc", static=False)
        static = release.pkg_config("0.4.0", "-lgcc_s -lc", static=True)
        for text in (shared, static):
            self.assertTrue(text.startswith("prefix=${pcfiledir}/../..\n"))
            self.assertIn("Version: 0.4.0\n", text)
            self.assertIn("Cflags: -I${includedir}\n", text)
        self.assertIn("Libs: -L${libdir} -ltickertape\nLibs.private: -lgcc_s -lc\n", shared)
        self.assertIn("Name: tickertape-static\n", static)
        self.assertIn("Libs: ${libdir}/libtickertape.a -lgcc_s -lc\n", static)

    def package(self, source, target):
        built = source / "target" / target / "release"
        built.mkdir(parents=True, exist_ok=True)
        extension = "dylib" if target.endswith("-apple-darwin") else "so"
        (built / f"libtickertape.{extension}").write_bytes(b"shared library")
        (built / "libtickertape.a").write_bytes(b"static library")
        (built / "native-static-libs.txt").write_text("-lgcc_s -lc\n")
        include = source / release.FFI / "include"
        include.mkdir(parents=True, exist_ok=True)
        for name in ("tickertape.h", "tickertape.hpp", "LICENSE-MIT", "LICENSE-APACHE"):
            (include / name if name.startswith("tickertape") else source / name).write_text(name)
        args = argparse.Namespace(source=source, tag="v0.4.0", target=target, output=source / "dist")
        environment = {key: value for key, value in release.os.environ.items() if key != "CARGO_TARGET_DIR"}
        with patch.object(release, "run", return_value="1234567890"), \
             patch.dict(release.os.environ, environment, clear=True):
            release.c_package(args)
        return args.output / f"tickertape-c-v0.4.0-{target}.tar.gz"

    def test_archive_layout_links_and_reproducibility(self):
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary)
            archive = self.package(source, "x86_64-unknown-linux-gnu")
            first = archive.read_bytes()
            self.assertEqual(first, self.package(source, "x86_64-unknown-linux-gnu").read_bytes())
            stem = "tickertape-c-v0.4.0-x86_64-unknown-linux-gnu"
            with tarfile.open(archive) as tar:
                members = {member.name.removeprefix(stem).lstrip("/"): member for member in tar}
                self.assertEqual(set(members), {
                    "", "include", "include/tickertape.h", "include/tickertape.hpp", "lib",
                    "lib/libtickertape.so.0.4.0", "lib/libtickertape.so.0.4", "lib/libtickertape.so",
                    "lib/libtickertape.a", "lib/pkgconfig", "lib/pkgconfig/tickertape.pc",
                    "lib/pkgconfig/tickertape-static.pc", "LICENSE-MIT", "LICENSE-APACHE", "README.txt"})
                self.assertTrue(all(member.name.startswith(stem) for member in tar))
                self.assertEqual(members["lib/libtickertape.so.0.4"].linkname, "libtickertape.so.0.4.0")
                self.assertEqual(members["lib/libtickertape.so"].linkname, "libtickertape.so.0.4")
                self.assertTrue(members["lib/libtickertape.so.0.4"].issym())
                self.assertEqual(members["lib/libtickertape.so.0.4.0"].mode, 0o755)
                self.assertEqual(tar.extractfile(members["lib/libtickertape.so.0.4.0"]).read(), b"shared library")
                pc = tar.extractfile(members["lib/pkgconfig/tickertape.pc"]).read().decode()
                self.assertIn("Libs.private: -lgcc_s -lc", pc)
                readme = tar.extractfile(members["README.txt"]).read().decode()
                self.assertIn("glibc 2.35", readme)
            macos = self.package(source, "aarch64-apple-darwin")
            with tarfile.open(macos) as tar:
                names = {Path(member.name).name for member in tar}
            self.assertLessEqual({"libtickertape.0.4.0.dylib", "libtickertape.0.4.dylib", "libtickertape.dylib"}, names)

    def test_tags_have_the_c_library_once_its_install_test_exists(self):
        self.assertTrue(release.has_c_library(Path(__file__).resolve().parents[2]))
        with tempfile.TemporaryDirectory() as temporary:
            self.assertFalse(release.has_c_library(Path(temporary)))


class SbomTests(unittest.TestCase):
    def test_default_names_are_the_archive_stems(self):
        def name(**kwargs):
            values = {"package": "cli", "tag": "v0.4.0", "target": "x86_64-apple-darwin", "name": None} | kwargs
            return release.sbom_name(argparse.Namespace(**values))

        self.assertEqual(name(), "tickertape-v0.4.0-x86_64-apple-darwin")
        self.assertEqual(name(package="c-library"), "tickertape-c-v0.4.0-x86_64-apple-darwin")
        self.assertEqual(name(package="python", name="python-wheel"), "python-wheel")
        with self.assertRaisesRegex(ValueError, "--name"):
            name(package="python")
        with self.assertRaisesRegex(ValueError, "--tag or --name"):
            name(tag=None)

    def test_cargo_packages_write_the_file_release_py_reads(self):
        """cargo-cyclonedx names its output after the package's targets; check it against the manifests."""
        source = Path(__file__).resolve().parents[2]
        for package, (manifest, generated, component) in release.SBOM_PACKAGES.items():
            with self.subTest(package=package), (source / manifest).open("rb") as file:
                cargo = release.tomllib.load(file)
                if "bin" in cargo:
                    self.assertEqual(generated, f"{cargo['bin'][0]['name']}_bin.cdx.json")
                else:
                    kinds = "-".join(cargo["lib"]["crate-type"])
                    self.assertEqual(generated, f"{cargo['lib']['name']}_{kinds}.cdx.json")
                self.assertEqual(component, cargo.get("bin", [cargo.get("lib")])[0]["name"])

    def test_java_sbom_is_reproducible_and_describes_the_maven_coordinates(self):
        source = Path(__file__).resolve().parents[2]
        first = release.java_sbom(source, "1234567890")
        self.assertEqual(first, release.java_sbom(source, "1234567890"))
        document = json.loads(first)
        version = release.pom_version(source)
        component = document["metadata"]["component"]
        self.assertEqual(component["purl"], f"pkg:maven/io.github.thomas0903/tickertape@{version}")
        self.assertEqual(document["metadata"]["timestamp"], "2009-02-13T23:31:30Z")
        self.assertEqual(document["components"], [])

    def test_java_binding_has_no_runtime_dependencies(self):
        """The Java SBOM lists no components; that is true only while every pom dependency is test-scoped."""
        source = Path(__file__).resolve().parents[2]
        namespace = {"m": "http://maven.apache.org/POM/4.0.0"}
        root = release.ElementTree.parse(source / release.JAVA / "pom.xml").getroot()
        dependencies = root.findall("m:dependencies/m:dependency", namespace)
        self.assertTrue(dependencies)
        for dependency in dependencies:
            self.assertEqual(dependency.findtext("m:scope", namespaces=namespace), "test")


class MavenTests(unittest.TestCase):
    def write_pom(self, root, group="io.github.thomas0903", artifact="tickertape", version="0.4.0"):
        pom = root / "bindings" / "java" / "pom.xml"
        pom.parent.mkdir(parents=True, exist_ok=True)
        pom.write_text(f'<project xmlns="http://maven.apache.org/POM/4.0.0"><groupId>{group}</groupId>'
                       f"<artifactId>{artifact}</artifactId><version>{version}</version></project>")

    def test_pom_version_follows_the_tag(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            release.validate_java_version(root, "0.5.0")  # older tags have no Java binding
            self.write_pom(root)
            release.validate_java_version(root, "0.4.0")
            with self.assertRaisesRegex(ValueError, "pom.xml version"):
                release.validate_java_version(root, "0.5.0")
            self.write_pom(root, group="com.example")
            with self.assertRaisesRegex(ValueError, "coordinates"):
                release.pom_version(root)

    def test_pom_builds_a_natives_jar_for_every_platform(self):
        pom = (Path(__file__).resolve().parents[2] / release.JAVA / "pom.xml").read_text()
        for platform in release.JAVA_PLATFORMS.values():
            self.assertIn(f"<classifier>natives-{platform}</classifier>", pom)
            self.assertIn(f"<classesDirectory>${{project.build.directory}}/natives/{platform}</classesDirectory>", pom)

    def artifacts(self, directory, version="0.4.0"):
        directory.mkdir(parents=True, exist_ok=True)
        for name in release.maven_files(version):
            (directory / name).write_bytes(name.encode())

    def test_bundle_has_the_repository_layout_and_every_checksum(self):
        with tempfile.TemporaryDirectory() as temporary:
            artifacts = Path(temporary, "maven")
            self.artifacts(artifacts)
            self.assertEqual(len(release.maven_files("0.4.0")), 8)
            args = argparse.Namespace(tag="v0.4.0", artifacts=artifacts, output=Path(temporary, "a.zip"), sign=False)
            release.maven_bundle(args)
            first = args.output.read_bytes()
            release.maven_bundle(args)
            self.assertEqual(first, args.output.read_bytes())
            with release.zipfile.ZipFile(args.output) as bundle:
                names = set(bundle.namelist())
                prefix = "io/github/thomas0903/tickertape/0.4.0/"
                self.assertTrue(all(name.startswith(prefix) for name in names))
                self.assertEqual(len(names), 8 * 5)
                jar = "tickertape-0.4.0-natives-linux-aarch64.jar"
                self.assertEqual(bundle.read(prefix + jar + ".sha1").decode(),
                                 hashlib.sha1(jar.encode()).hexdigest())
                self.assertEqual(bundle.read(prefix + jar + ".md5").decode(), hashlib.md5(jar.encode()).hexdigest())

    def test_bundle_refuses_missing_or_extra_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            artifacts = Path(temporary, "maven")
            self.artifacts(artifacts)
            args = argparse.Namespace(tag="v0.4.0", artifacts=artifacts, output=Path(temporary, "a.zip"), sign=False)
            (artifacts / "tickertape-0.4.0-javadoc.jar").unlink()
            with self.assertRaisesRegex(ValueError, "expected exactly"):
                release.maven_bundle(args)
            self.artifacts(artifacts)
            (artifacts / "tickertape-0.4.0-natives-windows-x86_64.jar").write_text("x")
            with self.assertRaisesRegex(ValueError, "expected exactly"):
                release.maven_bundle(args)
            args.tag = "v0.5.0"
            (artifacts / "tickertape-0.4.0-natives-windows-x86_64.jar").unlink()
            with self.assertRaisesRegex(ValueError, "expected exactly"):
                release.maven_bundle(args)

    def test_signed_bundle_has_a_signature_per_file(self):
        def sign(paths, home):
            for path in paths:
                Path(f"{path}.asc").write_text("signature")

        with tempfile.TemporaryDirectory() as temporary:
            artifacts = Path(temporary, "maven")
            self.artifacts(artifacts)
            args = argparse.Namespace(tag="v0.4.0", artifacts=artifacts, output=Path(temporary, "a.zip"), sign=True)
            with patch.object(release, "gpg_sign", side_effect=sign) as gpg:
                release.maven_bundle(args)
            self.assertEqual(len(gpg.call_args.args[0]), 8)
            with release.zipfile.ZipFile(args.output) as bundle:
                names = bundle.namelist()
            self.assertEqual(len(names), 8 * 6)
            self.assertIn("io/github/thomas0903/tickertape/0.4.0/tickertape-0.4.0.pom.asc", names)
            self.assertFalse(any(name.endswith(".asc.sha1") for name in names))
            # Signing works on copies: the attested files stay as they were.
            self.assertEqual(sorted(path.name for path in artifacts.iterdir()), sorted(release.maven_files("0.4.0")))

    @unittest.skipUnless(release.subprocess.run(["sh", "-c", "command -v gpg"], capture_output=True).returncode == 0,
                         "gpg is not installed")
    def test_gpg_signs_with_the_key_and_passphrase_from_the_environment(self):
        with tempfile.TemporaryDirectory() as temporary:
            keyring = Path(temporary, "maker")
            keyring.mkdir(mode=0o700)
            gpg = ["gpg", "--homedir", str(keyring), "--batch", "--quiet", "--pinentry-mode", "loopback"]
            subprocess.run([*gpg, "--passphrase", "secret", "--quick-gen-key", "Test <test@example.invalid>",
                            "ed25519", "sign", "never"], check=True, capture_output=True)
            key = subprocess.run([*gpg, "--passphrase", "secret", "--armor", "--export-secret-keys"],
                                 check=True, capture_output=True, text=True).stdout
            file = Path(temporary, "tickertape-0.4.0.jar")
            file.write_bytes(b"jar")
            home = Path(temporary, "signer")
            home.mkdir(mode=0o700)
            with patch.dict(release.os.environ, MAVEN_GPG_PRIVATE_KEY=key, MAVEN_GPG_PASSPHRASE="secret"):
                release.gpg_sign([file], home)
            self.assertIn("BEGIN PGP SIGNATURE", Path(f"{file}.asc").read_text())
            unsigned = Path(temporary, "tickertape-0.4.0.pom")
            unsigned.write_bytes(b"pom")
            other = Path(temporary, "other")
            other.mkdir(mode=0o700)
            with patch.dict(release.os.environ, MAVEN_GPG_PRIVATE_KEY=key, MAVEN_GPG_PASSPHRASE="wrong"), \
                 self.assertRaises(subprocess.CalledProcessError):
                release.gpg_sign([unsigned], other)
            self.assertFalse(Path(f"{unsigned}.asc").exists())

    def test_multipart_body_carries_the_bundle(self):
        body, content_type = release.multipart("bundle", "bundle.zip", b"zip bytes")
        boundary = content_type.split("boundary=")[1]
        self.assertTrue(body.startswith(f"--{boundary}\r\n".encode()))
        self.assertIn(b'name="bundle"; filename="bundle.zip"', body)
        self.assertIn(b"\r\n\r\nzip bytes\r\n", body)
        self.assertTrue(body.endswith(f"--{boundary}--\r\n".encode()))

    def upload(self, responses):
        calls = []

        def request(url, data=None, headers=None):
            calls.append(url)
            return responses.pop(0)

        with tempfile.TemporaryDirectory() as temporary:
            bundle = Path(temporary, "bundle.zip")
            bundle.write_bytes(b"zip")
            args = argparse.Namespace(tag="v0.4.0", bundle=bundle, timeout=600)
            with patch.object(release, "central_request", side_effect=request), \
                 patch.object(release.time, "sleep"):
                release.maven_upload(args)
        return calls

    def test_upload_waits_until_published(self):
        calls = self.upload([b'{"published": false}', b"deployment-id\n",
                             b'{"deploymentState": "VALIDATING"}', b'{"deploymentState": "PUBLISHED"}'])
        self.assertIn("namespace=io.github.thomas0903&name=tickertape&version=0.4.0", calls[0])
        self.assertIn("publishingType=AUTOMATIC", calls[1])
        self.assertTrue(calls[2].endswith("status?id=deployment-id"))
        self.assertEqual(len(calls), 4)

    def test_upload_keeps_a_published_version_and_reports_a_refusal(self):
        self.assertEqual(len(self.upload([b'{"published": true}'])), 1)
        with self.assertRaisesRegex(ValueError, "refused"):
            self.upload([b'{"published": false}', b"id", b'{"deploymentState": "FAILED", "errors": {"x": ["y"]}}'])

    def test_verify_compares_every_file_with_the_build(self):
        with tempfile.TemporaryDirectory() as temporary:
            artifacts = Path(temporary, "maven")
            self.artifacts(artifacts)
            args = argparse.Namespace(tag="v0.4.0", artifacts=artifacts, output=Path(temporary, "central"),
                                      timeout=600)

            def fetch(url):
                return io.BytesIO(url.rsplit("/", 1)[1].encode())

            with patch.object(release, "fetch", side_effect=fetch):
                release.maven_verify(args)
            self.assertEqual({path.name for path in args.output.iterdir()}, release.maven_files("0.4.0"))
            (artifacts / "tickertape-0.4.0.jar").write_text("rebuilt")
            with patch.object(release, "fetch", side_effect=fetch), self.assertRaisesRegex(ValueError, "differs"):
                release.maven_verify(args)


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
