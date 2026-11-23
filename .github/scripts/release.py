#!/usr/bin/env python3
"""Release orchestration using the Python standard library, Cargo and GitHub CLI."""

import argparse
import base64
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request
import uuid
import xml.etree.ElementTree as ElementTree
import zipfile


CRATES = ("tickertape", "tickertape-cli")
# The Python bindings (ADR 0029): outside the workspace, versioned with it, published to PyPI by python-wheels.yml.
BINDINGS = Path("bindings") / "python"
TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
)
# cargo-binstall and Homebrew install the static musl archive on every Linux machine: it needs no particular glibc.
LINUX_INSTALL_TARGETS = {
    "x86_64-unknown-linux-gnu": "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-gnu": "aarch64-unknown-linux-musl",
}
# The C library archives (ADR 0031): the glibc Linux and macOS targets. Rust's musl targets link the C library
# statically and build no usable shared library, so there is no musl C archive.
FFI = Path("crates") / "tickertape-ffi"
C_TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
)
# The Java binding (ADR 0037) on Maven Central: one natives JAR per C target, named as the binding's loader expects.
JAVA = Path("bindings") / "java"
MAVEN_GROUP = "io.github.thomas0903"
MAVEN_ARTIFACT = "tickertape"
JAVA_PLATFORMS = {
    "x86_64-unknown-linux-gnu": "linux-x86_64",
    "aarch64-unknown-linux-gnu": "linux-aarch64",
    "x86_64-apple-darwin": "macos-x86_64",
    "aarch64-apple-darwin": "macos-aarch64",
}
CENTRAL_API = "https://central.sonatype.com/api/v1/publisher"
MAVEN_REPOSITORY = "https://repo1.maven.org/maven2"


def archive_stem(tag, target):
    """The archive's file name without `.tar.gz`, which is also its single top-level directory."""
    return f"tickertape-{tag}-{target}"


def c_archive_stem(tag, target):
    """The C library archive's file name without `.tar.gz`, which is also its single top-level directory."""
    return f"tickertape-c-{tag}-{target}"


def run(*args, cwd=None):
    return subprocess.check_output(args, cwd=cwd, text=True).strip()


def gh_json(*args):
    return json.loads(run("gh", *args))


def fetch(url):
    request = urllib.request.Request(url, headers={"User-Agent": "tickertape-release"})
    return urllib.request.urlopen(request, timeout=30)


def fetch_json(url):
    with fetch(url) as response:
        return json.load(response)


def version_for_tag(tag):
    if not re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", tag):
        raise ValueError("release tags must be stable versions in the form vX.Y.Z")
    return tag[1:]


def validate_versions(source, version):
    with (source / "Cargo.toml").open("rb") as file:
        workspace = tomllib.load(file)["workspace"]
    if workspace["package"]["version"] != version:
        raise ValueError("tag does not match the workspace package version")
    if workspace["dependencies"]["tickertape"]["version"] != version:
        raise ValueError("workspace tickertape dependency does not match the tag")
    for name in CRATES:
        with (source / "crates" / name / "Cargo.toml").open("rb") as file:
            package = tomllib.load(file)["package"]
        if package["name"] != name or package["version"] != {"workspace": True}:
            raise ValueError(f"{name} must inherit the workspace version")
    # Members this workflow does not release (tests, benches, examples, the C ABI crate) must not be publishable,
    # so that adding a crate is a deliberate decision rather than a crate silently left out of releases.
    for member in workspace.get("members", []):
        with (source / member / "Cargo.toml").open("rb") as file:
            package = tomllib.load(file)["package"]
        if package["name"] not in CRATES and package.get("publish", True) not in (False, []):
            raise ValueError(f"{package['name']} is not released by this workflow; set publish = false "
                             "or add it to CRATES")
    validate_bindings_version(source, version)
    validate_java_version(source, version)


def pom_version(source):
    """The `<version>` of the Java binding's pom.xml, or None for tags from before the binding existed."""
    pom = source / JAVA / "pom.xml"
    if not pom.exists():
        return None
    namespace = {"m": "http://maven.apache.org/POM/4.0.0"}
    root = ElementTree.parse(pom).getroot()
    coordinates = tuple(root.findtext(f"m:{key}", namespaces=namespace) for key in ("groupId", "artifactId"))
    if coordinates != (MAVEN_GROUP, MAVEN_ARTIFACT):
        raise ValueError(f"{JAVA}/pom.xml must have the coordinates {MAVEN_GROUP}:{MAVEN_ARTIFACT}")
    return root.findtext("m:version", namespaces=namespace)


def validate_java_version(source, version):
    found = pom_version(source)
    if found is not None and found != version:
        raise ValueError(f"{JAVA}/pom.xml version does not match the tag")


def validate_bindings_version(source, version):
    manifest = source / BINDINGS / "Cargo.toml"
    if not manifest.exists():
        return  # release tags from before the bindings existed
    with manifest.open("rb") as file:
        if tomllib.load(file)["package"]["version"] != version:
            raise ValueError(f"{BINDINGS}/Cargo.toml version does not match the tag")
    with (source / BINDINGS / "Cargo.lock").open("rb") as file:
        locked = {package["name"]: package["version"] for package in tomllib.load(file)["package"]}
    if locked.get("tickertape") != version or locked.get("tickertape-python") != version:
        raise ValueError(f"{BINDINGS}/Cargo.lock is stale; run cargo update --workspace in {BINDINGS}")


def require_green_ci(runs, sha):
    matching = [run for run in runs if run["head_sha"] == sha
                and run["head_branch"] == "main" and run["event"] == "push"]
    latest = max(matching, key=lambda run: run["run_number"], default=None)
    if latest is None or latest["status"] != "completed" or latest["conclusion"] != "success":
        raise ValueError("the tag commit must have a successful CI push run on main; rerun after CI passes")


def release_notes(changelog, version):
    match = re.search(rf"^## \[{re.escape(version)}\][^\n]*\n(.*?)(?=^## \[|\Z)",
                      changelog, re.MULTILINE | re.DOTALL)
    if match is None:
        raise ValueError(f"CHANGELOG.md has no section for {version}")
    # Reference definitions belong to the whole changelog, not the final section.
    notes = re.split(r"^\[[^\]]+\]: ", match.group(1), maxsplit=1, flags=re.MULTILINE)[0].strip()
    return (notes + "\n\n```sh\ncargo add tickertape\ncargo install tickertape-cli --locked\n```\n\n"
            f"[API documentation](https://docs.rs/tickertape/{version}/tickertape/) · "
            "[CLI binary installation](https://github.com/thomas0903/tickertape/blob/main/docs/install.md)\n")


def tag_commit(source, tag):
    """The commit that the tag `refs/tags/<tag>` points to. Never a branch (or any other ref) with the same name."""
    result = subprocess.run(["git", "rev-parse", "--verify", "--quiet", f"refs/tags/{tag}^{{commit}}"],
                            cwd=source, capture_output=True, text=True)
    if result.returncode != 0:
        raise ValueError(f"{tag} is not a tag in the source checkout")
    return result.stdout.strip()


BINSTALL_SUFFIXES = {"tgz": ".tar.gz", "tar.gz": ".tar.gz"}


def render_binstall(template, values):
    """Render a cargo-binstall template; unknown placeholders are errors, not empty strings."""
    def replace(match):
        key = match.group(1)
        if key not in values:
            raise ValueError(f"unsupported cargo-binstall placeholder {{ {key} }}")
        return values[key]
    return re.sub(r"\{\s*([a-z-]+)\s*\}", replace, template)


def binstall_metadata(source):
    """Return the CLI crate's `[package.metadata.binstall]` table, or None for tags from before it existed."""
    with (source / "crates" / "tickertape-cli" / "Cargo.toml").open("rb") as file:
        package = tomllib.load(file)["package"]
    return package.get("metadata", {}).get("binstall")


def check_binstall(source, tag):
    """Check that cargo-binstall resolves every release target to an archive this script packages.

    Returns False when the tag has no metadata (older releases), True when it matches, and raises otherwise."""
    metadata = binstall_metadata(source)
    if metadata is None:
        return False
    with (source / "Cargo.toml").open("rb") as file:
        repo = tomllib.load(file)["workspace"]["package"]["repository"]
    version = version_for_tag(tag)
    for target in TARGETS:
        settings = {key: value for key, value in metadata.items() if key != "overrides"}
        settings.update(metadata.get("overrides", {}).get(target, {}))
        suffix = BINSTALL_SUFFIXES.get(settings.get("pkg-fmt"))
        if suffix is None or "pkg-url" not in settings or "bin-dir" not in settings:
            raise ValueError(f"cargo-binstall metadata for {target} needs pkg-url, bin-dir and pkg-fmt = \"tgz\"")
        values = {"name": "tickertape-cli", "version": version, "target": target, "repo": repo,
                  "bin": "tickertape", "binary-ext": "", "archive-suffix": suffix, "archive-format": "tgz"}
        archive = archive_stem(tag, LINUX_INSTALL_TARGETS.get(target, target))
        expected_url = f"{repo}/releases/download/{tag}/{archive}.tar.gz"
        if render_binstall(settings["pkg-url"], values) != expected_url:
            raise ValueError(f"cargo-binstall pkg-url for {target} does not resolve to {expected_url}")
        if render_binstall(settings["bin-dir"], values) != f"{archive}/tickertape":
            raise ValueError(f"cargo-binstall bin-dir for {target} does not resolve to {archive}/tickertape")
    return True


def has_c_library(source):
    """Whether the tag can build the C library archive (tags from before it existed cannot)."""
    return (source / FFI / "scripts" / "install-test.sh").exists()


def prepare(args):
    source = args.source.resolve()
    version = version_for_tag(args.tag)
    validate_versions(source, version)
    sha = run("git", "rev-parse", "HEAD", cwd=source)
    if tag_commit(source, args.tag) != sha:
        raise ValueError("source checkout does not match the release tag")
    subprocess.run(["git", "merge-base", "--is-ancestor", sha, "origin/main"], cwd=source, check=True)
    repo = os.environ["GITHUB_REPOSITORY"]
    runs = gh_json("api", f"repos/{repo}/actions/workflows/ci.yml/runs?head_sha={sha}&per_page=100")
    require_green_ci(runs["workflow_runs"], sha)
    binstall = check_binstall(source, args.tag)
    args.notes.write_text(release_notes((source / "CHANGELOG.md").read_text(), version))
    c_library = has_c_library(source)
    outputs = {"tag": args.tag, "version": version, "sha": sha, "binstall": str(binstall).lower(),
               "c_library": str(c_library).lower(),
               "java": str(c_library and pom_version(source) is not None).lower()}
    with Path(os.environ["GITHUB_OUTPUT"]).open("a") as file:
        for key, value in outputs.items():
            file.write(f"{key}={value}\n")
    print(json.dumps(outputs))


def published_source(name, version):
    """Return the published commit, or None for a version that does not exist."""
    try:
        metadata = fetch_json(f"https://crates.io/api/v1/crates/{name}/{version}")["version"]
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise
    if metadata["yanked"]:
        raise ValueError(f"{name} {version} is yanked; refusing to release it")
    with fetch(f"https://static.crates.io/crates/{name}/{name}-{version}.crate") as response:
        archive = response.read()
    if hashlib.sha256(archive).hexdigest() != metadata["checksum"]:
        raise ValueError(f"checksum mismatch for {name} {version}")
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as crate:
        vcs = json.load(crate.extractfile(f"{name}-{version}/.cargo_vcs_info.json"))
    if vcs["git"].get("dirty", False):
        raise ValueError(f"{name} {version} was published from a dirty checkout")
    return vcs["git"]["sha1"]


def publish(args):
    validate_versions(args.source, args.version)
    # Cargo waits for index availability before publishing the dependent CLI.
    for name in CRATES:
        existing = published_source(name, args.version)
        if existing is not None:
            if existing != args.sha:
                raise ValueError(f"{name} {args.version} already exists from a different commit")
            print(f"{name} {args.version} is already published from {args.sha}; keeping it")
            continue
        subprocess.run(["cargo", "publish", "--locked", "-p", name], cwd=args.source, check=True)


def requirement(target):
    if target.endswith("-linux-musl"):
        return "This Linux binary is statically linked with musl and needs no C library on the system."
    if target.endswith("-linux-gnu"):
        return "This Linux binary requires glibc 2.35 or later."
    return "This macOS binary requires macOS 11 or later. It is unsigned and is not notarized."


def package(args):
    version = version_for_tag(args.tag)
    if args.target not in TARGETS:
        raise ValueError("unsupported release target")
    name = archive_stem(args.tag, args.target)
    binary = args.source / "target" / args.target / "release/tickertape"
    files = [("tickertape", binary.read_bytes(), 0o755)]
    for filename in ("LICENSE-MIT", "LICENSE-APACHE"):
        files.append((filename, (args.source / filename).read_bytes(), 0o644))
    readme = (f"tickertape {version} CLI for {args.target}\n\n"
              "Run ./tickertape --help, or copy tickertape to a directory on your PATH.\n"
              "Verify the archive against SHA256SUMS from the same GitHub release.\n"
              f"{requirement(args.target)}\n\n"
              "Installation: https://github.com/thomas0903/tickertape/blob/main/docs/install.md\n"
              "Crash contract: https://github.com/thomas0903/tickertape/blob/main/docs/crash-contract.md\n")
    files.append(("README.txt", readme.encode(), 0o644))
    epoch = int(run("git", "show", "-s", "--format=%ct", "HEAD", cwd=args.source))
    args.output.mkdir(parents=True, exist_ok=True)
    destination = args.output / f"{name}.tar.gz"
    with destination.open("wb") as file:
        with gzip.GzipFile(filename="", fileobj=file, mode="wb", mtime=epoch) as compressed:
            with tarfile.open(fileobj=compressed, mode="w") as archive:
                for filename, data, mode in files:
                    info = tarfile.TarInfo(f"{name}/{filename}")
                    info.size, info.mode, info.mtime = len(data), mode, epoch
                    archive.addfile(info, io.BytesIO(data))
    print(destination)


# What `release.py sbom --package` describes: (manifest, file cargo-cyclonedx writes next to it, component name).
SBOM_PACKAGES = {
    "cli": (Path("crates") / "tickertape-cli" / "Cargo.toml", "tickertape_bin.cdx.json", "tickertape"),
    "c-library": (FFI / "Cargo.toml", "tickertape_cdylib-staticlib.cdx.json", "tickertape"),
    "python": (BINDINGS / "Cargo.toml", "_native_cdylib.cdx.json", "_native"),
}


def sbom_name(args):
    """The SBOM's file name without `.cdx.json`: the archive's stem for the archives, else `--name`."""
    if args.name is not None:
        return args.name
    if args.tag is None:
        raise ValueError("--tag or --name is required")
    if args.package == "cli":
        return archive_stem(args.tag, args.target)
    if args.package == "c-library":
        return c_archive_stem(args.tag, args.target)
    raise ValueError(f"--name is required for --package {args.package}")


def sbom(args):
    """Write a CycloneDX SBOM of a package's locked dependency graph for one target.

    The output is reproducible: cargo-cyclonedx takes its timestamp from SOURCE_DATE_EPOCH (the commit time), and
    the checkout's absolute path is replaced so that reruns on any runner produce the same bytes. Without --target
    (the Python sdist) it covers every platform."""
    if args.tag is not None:
        version_for_tag(args.tag)
    if args.target is not None and args.target not in TARGETS:
        raise ValueError("unsupported release target")
    source = args.source.resolve()
    epoch = run("git", "show", "-s", "--format=%ct", "HEAD", cwd=source)
    if args.package == "java":
        document = java_sbom(source, epoch)
    else:
        manifest, generated, component = SBOM_PACKAGES[args.package]
        command = ["cargo", "cyclonedx", "--manifest-path", str(manifest), "--format", "json",
                   "--spec-version", "1.5", "--describe", "binaries"]
        if args.target is not None:
            command += ["--target", args.target]
        subprocess.run(command, cwd=source, check=True, env={**os.environ, "SOURCE_DATE_EPOCH": epoch})
        document = (source / manifest.parent / generated).read_text().replace(f"file://{source}", "file://.")
        if json.loads(document)["metadata"]["component"]["name"] != component:
            raise ValueError(f"the SBOM does not describe {component}")
        document = with_serial_number(document, f"{args.target or 'any'}|{sbom_name(args)}")
    args.output.mkdir(parents=True, exist_ok=True)
    destination = args.output / f"{sbom_name(args)}.cdx.json"
    destination.write_text(document)
    print(destination)


def with_serial_number(document, variant):
    """The CycloneDX document with a `serialNumber`, added after `specVersion` if it has none.

    cargo-cyclonedx writes none when SOURCE_DATE_EPOCH is set, and the attestation action (actions/attest) accepts a
    CycloneDX document only with `bomFormat`, `specVersion` and `serialNumber`. Reproducible: the number derives
    from the described component's purl (which holds the version) and `variant` (the target and the file's name)."""
    bom = json.loads(document)
    if not bom.get("bomFormat") or not bom.get("specVersion"):
        raise ValueError("the SBOM has no bomFormat or specVersion")
    if bom.get("serialNumber"):
        return document
    purl = bom["metadata"]["component"]["purl"]
    serial = f"urn:uuid:{uuid.uuid5(uuid.NAMESPACE_URL, f'{purl}|{variant}')}"
    ordered = {}
    for key, value in bom.items():
        ordered[key] = value
        if key == "specVersion":
            ordered["serialNumber"] = serial
    return json.dumps(ordered, indent=2, ensure_ascii=False) + "\n"


def java_sbom(source, epoch):
    """A CycloneDX SBOM of the Java binding's JAR, its sources and its Javadoc.

    The binding has no runtime dependencies (its pom has test dependencies only, and a test checks that), so the
    document lists none; the library in the natives JARs has the C library's SBOM. Reproducible: the serial number
    derives from the coordinates and the timestamp is the commit time."""
    version = pom_version(source)
    if version is None:
        raise ValueError(f"{JAVA}/pom.xml does not exist")
    purl = f"pkg:maven/{MAVEN_GROUP}/{MAVEN_ARTIFACT}@{version}"
    timestamp = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(int(epoch)))
    document = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": f"urn:uuid:{uuid.uuid5(uuid.NAMESPACE_URL, purl)}",
        "version": 1,
        "metadata": {
            "timestamp": timestamp,
            "tools": [{"name": "release.py", "vendor": "tickertape"}],
            "component": {
                "type": "library", "bom-ref": purl, "group": MAVEN_GROUP, "name": MAVEN_ARTIFACT,
                "version": version, "purl": purl,
                "licenses": [{"expression": "MIT OR Apache-2.0"}],
                "externalReferences": [{"type": "vcs", "url": "https://github.com/thomas0903/tickertape"}],
            },
        },
        "components": [],
        "dependencies": [{"ref": purl, "dependsOn": []}],
    }
    return json.dumps(document, indent=2) + "\n"


def shared_library_names(version, target):
    """(file, soname or install name, link name) of the C archive's shared library.

    Before 1.0 the soname carries the minor version (`libtickertape.so.0.4`), as cargo-c names it; from 1.0 the
    major version only (`libtickertape.so.1`; ADR 0028 -> Versioning). macOS uses the same scheme in its install
    names (`libtickertape.0.4.dylib`, `libtickertape.1.dylib`)."""
    major, minor, _ = version.split(".")
    compatible = major if major != "0" else f"0.{minor}"
    if target.endswith("-apple-darwin"):
        return (f"libtickertape.{version}.dylib", f"libtickertape.{compatible}.dylib", "libtickertape.dylib")
    return (f"libtickertape.so.{version}", f"libtickertape.so.{compatible}", "libtickertape.so")


def c_build_command(version, target):
    """Build the C library for the archive: both library kinds, the shared one with its soname or install name."""
    _, soname, _ = shared_library_names(version, target)
    if target.endswith("-apple-darwin"):
        link = f"-Wl,-install_name,@rpath/{soname}"
    else:
        link = f"-Wl,-soname,{soname}"
    return ["cargo", "rustc", "--locked", "--release", "-p", "tickertape-ffi", "--lib", "--target", target,
            "--crate-type", "cdylib,staticlib", "--", "-C", f"link-arg={link}", "--print", "native-static-libs"]


def native_static_libs(output):
    """The system libraries a static link needs, from rustc's `native-static-libs` note."""
    # With CARGO_TERM_COLOR=always (as in CI) the note ends in a colour reset: remove terminal escapes first.
    match = re.search(r"native-static-libs: ([^\n]*)", re.sub(r"\x1b\[[0-9;]*m", "", output))
    if match is None or not match.group(1).strip():
        raise ValueError("rustc printed no native-static-libs note; was the library rebuilt?")
    return match.group(1).strip()


def built_dir(source, target):
    """Where Cargo puts the release build for `target`: under CARGO_TARGET_DIR if set (relative to the source)."""
    return source / os.environ.get("CARGO_TARGET_DIR", "target") / target / "release"


def c_build(args):
    version = version_for_tag(args.tag)
    if args.target not in C_TARGETS:
        raise ValueError("unsupported C library target")
    result = subprocess.run(c_build_command(version, args.target), cwd=args.source, text=True,
                            stderr=subprocess.PIPE)
    print(result.stderr, end="", flush=True)
    if result.returncode != 0:
        raise SystemExit(result.returncode)
    libraries = native_static_libs(result.stderr)
    (built_dir(args.source, args.target) / "native-static-libs.txt").write_text(libraries + "\n")
    print(f"native static libraries: {libraries}")


def pkg_config(version, static_libs, static):
    """`tickertape.pc` (shared) or `tickertape-static.pc`, relocatable: paths are relative to the file itself."""
    if static:
        libs = f"Libs: ${{libdir}}/libtickertape.a {static_libs}\n"
        description = "tickertape C ABI, static library"
    else:
        libs = f"Libs: -L${{libdir}} -ltickertape\nLibs.private: {static_libs}\n"
        description = "tickertape C ABI, a local append-only event journal on memory-mapped files"
    return ("prefix=${pcfiledir}/../..\n"
            "libdir=${prefix}/lib\n"
            "includedir=${prefix}/include\n\n"
            f"Name: tickertape{'-static' if static else ''}\n"
            f"Description: {description}\n"
            f"Version: {version}\n"
            "URL: https://github.com/thomas0903/tickertape\n"
            f"{libs}"
            "Cflags: -I${includedir}\n")


def c_readme(version, target):
    file, soname, link = shared_library_names(version, target)
    return (f"tickertape {version} C library for {target}\n\n"
            "include/tickertape.h    the C header (C99, C++11)\n"
            "include/tickertape.hpp  the C++17 binding, header-only, over the C header\n"
            f"lib/{file}  the shared library ({soname} and {link} link to it)\n"
            "lib/libtickertape.a     the static library\n"
            "lib/pkgconfig           tickertape.pc (shared) and tickertape-static.pc\n\n"
            "The .pc files find the directories relative to themselves, so the archive works wherever it is unpacked:\n"
            "  export PKG_CONFIG_PATH=\"$PWD/lib/pkgconfig\"\n"
            "  cc app.c $(pkg-config --cflags --libs tickertape) -Wl,-rpath,\"$PWD/lib\" -o app\n"
            "  cc app.c $(pkg-config --cflags --libs tickertape-static) -o app\n\n"
            f"{requirement(target).replace('binary', 'library')}\n"
            "Before 1.0 the C ABI is unstable: a program requires exactly this library version.\n"
            "Verify the archive against SHA256SUMS from the same GitHub release.\n\n"
            "Installation: https://github.com/thomas0903/tickertape/blob/main/docs/install.md\n"
            "C API: https://github.com/thomas0903/tickertape/blob/main/docs/c-api.md\n")


def write_tar_gz(destination, root, entries, epoch):
    """A reproducible .tar.gz: entries are (name, bytes, mode), (name, None, mode) for a directory or
    (name, "->target", 0o777) for a symbolic link, all under the directory `root`."""
    with destination.open("wb") as file:
        with gzip.GzipFile(filename="", fileobj=file, mode="wb", mtime=epoch) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as archive:
                for name, data, mode in entries:
                    info = tarfile.TarInfo(f"{root}/{name}" if name else root)
                    info.mode, info.mtime = mode, epoch
                    if data is None:
                        info.type = tarfile.DIRTYPE
                        archive.addfile(info)
                    elif isinstance(data, str):
                        info.type, info.linkname = tarfile.SYMTYPE, data.removeprefix("->")
                        archive.addfile(info)
                    else:
                        info.size = len(data)
                        archive.addfile(info, io.BytesIO(data))


def c_package(args):
    """Package the C library archive: headers, both libraries, pkg-config files, licences and a README."""
    version = version_for_tag(args.tag)
    if args.target not in C_TARGETS:
        raise ValueError("unsupported C library target")
    built = built_dir(args.source, args.target)
    file, soname, link = shared_library_names(version, args.target)
    extension = "dylib" if args.target.endswith("-apple-darwin") else "so"
    static_libs = (built / "native-static-libs.txt").read_text().strip()
    entries = [("", None, 0o755), ("include", None, 0o755)]
    for header in ("tickertape.h", "tickertape.hpp"):
        entries.append((f"include/{header}", (args.source / FFI / "include" / header).read_bytes(), 0o644))
    entries += [("lib", None, 0o755),
                (f"lib/{file}", (built / f"libtickertape.{extension}").read_bytes(), 0o755),
                (f"lib/{soname}", f"->{file}", 0o777),
                (f"lib/{link}", f"->{soname}", 0o777),
                ("lib/libtickertape.a", (built / "libtickertape.a").read_bytes(), 0o644),
                ("lib/pkgconfig", None, 0o755),
                ("lib/pkgconfig/tickertape.pc", pkg_config(version, static_libs, False).encode(), 0o644),
                ("lib/pkgconfig/tickertape-static.pc", pkg_config(version, static_libs, True).encode(), 0o644)]
    for filename in ("LICENSE-MIT", "LICENSE-APACHE"):
        entries.append((filename, (args.source / filename).read_bytes(), 0o644))
    entries.append(("README.txt", c_readme(version, args.target).encode(), 0o644))
    epoch = int(run("git", "show", "-s", "--format=%ct", "HEAD", cwd=args.source))
    args.output.mkdir(parents=True, exist_ok=True)
    name = c_archive_stem(args.tag, args.target)
    destination = args.output / f"{name}.tar.gz"
    write_tar_gz(destination, name, entries, epoch)
    print(destination)


def release_files(tag, c_library=True):
    """Every file the GitHub release carries besides SHA256SUMS: a CLI archive and its SBOM per target, and a C
    library archive and its SBOM per C target (unless the tag predates them)."""
    stems = [archive_stem(tag, target) for target in TARGETS]
    if c_library:
        stems += [c_archive_stem(tag, target) for target in C_TARGETS]
    return {f"{stem}{suffix}" for stem in stems for suffix in (".tar.gz", ".cdx.json")}


def checksums_extend(existing, new):
    """A release's SHA256SUMS may only gain lines (binaries added to an older release), never change or lose one."""
    return set(existing.splitlines()) <= set(new.splitlines())


# How often a newly created draft is looked up in the listing, five seconds apart.
NEW_RELEASE_LOOKUPS = 13


def find_release(repo, tag):
    """The GitHub release for `tag`, draft or published, or None.

    From the listing: `releases/tags/<tag>` answers 404 for a draft (the first v0.5.1 run created its draft and then
    failed there), and a listing does not treat every `release view` failure as a missing release."""
    releases = gh_json("api", f"repos/{repo}/releases?per_page=100", "--paginate", "--slurp")
    return next((release for page in releases for release in page if release["tag_name"] == tag), None)


def github_release(args):
    repo = os.environ["GITHUB_REPOSITORY"]
    files = sorted([*args.artifacts.glob("*.tar.gz"), *args.artifacts.glob("*.cdx.json")])
    if {path.name for path in files} != release_files(args.tag, args.c_library):
        raise ValueError("expected exactly one archive and one SBOM for each release target and C library target")
    checksums = args.artifacts / "SHA256SUMS"
    checksums.write_text("".join(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n"
                                 for path in files))
    existing = find_release(repo, args.tag)
    if existing is None:
        if not args.create:
            raise ValueError("binary-only runs require an existing GitHub release")
        subprocess.run(["gh", "release", "create", args.tag, "--repo", repo, "--verify-tag",
                        "--draft", "--title", f"tickertape {args.tag}",
                        "--notes-file", str(args.notes)], check=True)
        # The listing can lag the creation (the v1.0.0 run asked once, at once, and failed here): ask for a minute.
        for attempt in range(NEW_RELEASE_LOOKUPS):
            if attempt:
                time.sleep(5)
            existing = find_release(repo, args.tag)
            if existing is not None:
                break
        else:
            raise ValueError(f"the draft release for {args.tag} was created but is not listed")
    asset_names = {asset["name"] for asset in existing["assets"]}
    # Compare every existing asset before uploading anything, so that a refused run leaves the release unchanged.
    uploads = []
    for path in [*files, checksums]:
        if path.name not in asset_names:
            uploads.append([str(path)])
            continue
        with tempfile.TemporaryDirectory() as temporary:
            subprocess.run(["gh", "release", "download", args.tag, "--repo", repo,
                            "--pattern", path.name, "--dir", temporary], check=True)
            existing_bytes = Path(temporary, path.name).read_bytes()
        if existing_bytes == path.read_bytes():
            continue
        if path == checksums and checksums_extend(existing_bytes.decode(), path.read_text()):
            uploads.append([str(path), "--clobber"])
            continue
        raise ValueError(f"existing asset {path.name} differs; refusing to overwrite it")
    # SHA256SUMS comes last, after every file it lists.
    for upload in uploads:
        subprocess.run(["gh", "release", "upload", args.tag, *upload, "--repo", repo], check=True)
    if existing["draft"]:
        subprocess.run(["gh", "release", "edit", args.tag, "--repo", repo,
                        "--draft=false", "--latest"], check=True)


def maven_files(version):
    """The files of one Maven Central release of the Java binding, before signatures and checksums."""
    stem = f"{MAVEN_ARTIFACT}-{version}"
    return {f"{stem}.pom", f"{stem}.jar", f"{stem}-sources.jar", f"{stem}-javadoc.jar",
            *(f"{stem}-natives-{platform}.jar" for platform in JAVA_PLATFORMS.values())}


def maven_path(version):
    return f"{MAVEN_GROUP.replace('.', '/')}/{MAVEN_ARTIFACT}/{version}"


def gpg_sign(paths, home):
    """Detach-sign each file with the key in MAVEN_GPG_PRIVATE_KEY, in a temporary keyring that only this run uses.

    The key and its passphrase reach gpg on standard input, never on a command line."""
    gpg = ["gpg", "--homedir", str(home), "--batch", "--no-tty", "--quiet"]
    subprocess.run([*gpg, "--import"], input=os.environ["MAVEN_GPG_PRIVATE_KEY"].encode(), check=True)
    passphrase = os.environ.get("MAVEN_GPG_PASSPHRASE", "").encode()
    for path in paths:
        subprocess.run([*gpg, "--pinentry-mode", "loopback", "--passphrase-fd", "0", "--armor",
                        "--detach-sign", "--output", f"{path}.asc", str(path)], input=passphrase, check=True)
        subprocess.run([*gpg, "--verify", f"{path}.asc", str(path)], check=True)


def maven_bundle(args):
    """Write the Central Portal upload bundle: the Maven repository layout with checksums and, with --sign, signatures.

    The files come from the attested build; this step runs no build code. The zip is reproducible."""
    version = version_for_tag(args.tag)
    present = {path.name for path in args.artifacts.iterdir() if path.suffix in (".jar", ".pom")}
    if present != maven_files(version):
        raise ValueError(f"expected exactly the Maven files {sorted(maven_files(version))}, found {sorted(present)}")
    paths = sorted(args.artifacts / name for name in maven_files(version))
    with tempfile.TemporaryDirectory() as temporary:
        signatures = []
        if args.sign:
            copies = []
            for path in paths:
                copy = Path(temporary, path.name)
                copy.write_bytes(path.read_bytes())
                copies.append(copy)
            home = Path(temporary, "gnupg")
            home.mkdir(mode=0o700)
            gpg_sign(copies, home)
            signatures = [Path(f"{copy}.asc") for copy in copies]
        with zipfile.ZipFile(args.output, "w", zipfile.ZIP_DEFLATED) as bundle:
            for path in [*paths, *signatures]:
                data = path.read_bytes()
                members = [(path.name, data)]
                if path.suffix != ".asc":
                    members += [(f"{path.name}.{name}", hashlib.new(name, data).hexdigest().encode())
                                for name in ("md5", "sha1", "sha256", "sha512")]
                for name, content in members:
                    info = zipfile.ZipInfo(f"{maven_path(version)}/{name}", date_time=(1980, 1, 1, 0, 0, 0))
                    info.external_attr = 0o644 << 16
                    bundle.writestr(info, content, zipfile.ZIP_DEFLATED)
    print(args.output)


def central_request(url, data=None, headers=None):
    token = base64.b64encode(f"{os.environ['MAVEN_CENTRAL_USERNAME']}:{os.environ['MAVEN_CENTRAL_PASSWORD']}"
                             .encode()).decode()
    request = urllib.request.Request(url, data=data, method="POST" if data is not None else "GET",
                                     headers={"Authorization": f"Bearer {token}",
                                              "User-Agent": "tickertape-release", **(headers or {})})
    with urllib.request.urlopen(request, timeout=120) as response:
        return response.read()


def multipart(field, filename, data):
    """A multipart/form-data body with one file field: (body, content type)."""
    boundary = f"tickertape-{uuid.uuid4().hex}"
    body = (f"--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{filename}\"\r\n"
            "Content-Type: application/octet-stream\r\n\r\n").encode() + data + f"\r\n--{boundary}--\r\n".encode()
    return body, f"multipart/form-data; boundary={boundary}"


def maven_upload(args):
    """Upload the signed bundle to the Central Portal for automatic publication, then wait until it is published.

    A version Central already has is left alone (Central files are immutable; `maven-verify` compares them)."""
    version = version_for_tag(args.tag)
    query = urllib.parse.urlencode({"namespace": MAVEN_GROUP, "name": MAVEN_ARTIFACT, "version": version})
    if json.loads(central_request(f"{CENTRAL_API}/published?{query}")).get("published"):
        print(f"{MAVEN_GROUP}:{MAVEN_ARTIFACT}:{version} is already on Maven Central; keeping it")
        return
    body, content_type = multipart("bundle", args.bundle.name, args.bundle.read_bytes())
    query = urllib.parse.urlencode({"name": f"{MAVEN_ARTIFACT} {version}", "publishingType": "AUTOMATIC"})
    deployment = central_request(f"{CENTRAL_API}/upload?{query}", body, {"Content-Type": content_type}).decode()
    deployment = deployment.strip()
    print(f"deployment {deployment}", flush=True)
    deadline = time.monotonic() + args.timeout
    while True:
        status = json.loads(central_request(f"{CENTRAL_API}/status?id={urllib.parse.quote(deployment)}", b""))
        state = status.get("deploymentState")
        print(f"deployment state: {state}", flush=True)
        if state == "PUBLISHED":
            return
        if state == "FAILED":
            raise ValueError(f"Central refused the deployment: {json.dumps(status.get('errors'), indent=2)}")
        if time.monotonic() >= deadline:
            raise TimeoutError(f"deployment {deployment} is still {state}; check it in the Central Portal")
        time.sleep(30)


def maven_verify(args):
    """Download every file of the release from Maven Central (waiting for it to appear) and compare it byte for byte
    with the attested build."""
    version = version_for_tag(args.tag)
    args.output.mkdir(parents=True, exist_ok=True)
    pending = set(maven_files(version))
    deadline = time.monotonic() + args.timeout
    while pending:
        for name in sorted(pending):
            try:
                with fetch(f"{MAVEN_REPOSITORY}/{maven_path(version)}/{name}") as response:
                    data = response.read()
            except urllib.error.HTTPError as error:
                if error.code != 404:
                    raise
                continue
            except (urllib.error.URLError, TimeoutError):
                continue
            if data != (args.artifacts / name).read_bytes():
                raise ValueError(f"{name} on Maven Central differs from the attested build")
            (args.output / name).write_bytes(data)
            pending.remove(name)
            print(f"{name}: on Maven Central and identical", flush=True)
        if pending:
            if time.monotonic() >= deadline:
                raise TimeoutError(f"not on Maven Central yet: {sorted(pending)}")
            time.sleep(30)


def verify(args):
    deadline = time.monotonic() + args.timeout
    pending = set(CRATES)
    while pending:
        for name in list(pending):
            try:
                source = published_source(name, args.version)
                if source is None:
                    continue
                if source != args.sha:
                    raise ValueError(f"published {name} source differs from the release tag")
                status = fetch_json(f"https://docs.rs/crate/{name}/{args.version}/status.json")
                if status.get("doc_status") is True and status.get("version") == args.version:
                    pending.remove(name)
                    print(f"{name} {args.version}: published source and docs.rs verified", flush=True)
            except urllib.error.HTTPError as error:
                if error.code not in (404, 429, 500, 502, 503, 504):
                    raise
            except (urllib.error.URLError, TimeoutError):
                pass
        if pending:
            if time.monotonic() >= deadline:
                raise TimeoutError(f"publication/docs.rs verification timed out: {sorted(pending)}")
            time.sleep(20)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    command = commands.add_parser("prepare")
    command.add_argument("--source", type=Path, required=True)
    command.add_argument("--tag", required=True)
    command.add_argument("--notes", type=Path, required=True)
    command.set_defaults(function=prepare)
    command = commands.add_parser("publish")
    command.add_argument("--source", type=Path, required=True)
    command.add_argument("--version", required=True)
    command.add_argument("--sha", required=True)
    command.set_defaults(function=publish)
    command = commands.add_parser("package")
    command.add_argument("--source", type=Path, required=True)
    command.add_argument("--tag", required=True)
    command.add_argument("--target", required=True)
    command.add_argument("--output", type=Path, required=True)
    command.set_defaults(function=package)
    command = commands.add_parser("sbom")
    command.add_argument("--source", type=Path, required=True)
    command.add_argument("--package", choices=[*SBOM_PACKAGES, "java"], default="cli")
    command.add_argument("--tag")
    command.add_argument("--target")
    command.add_argument("--name", help="file name of the SBOM without .cdx.json (default: the archive's stem)")
    command.add_argument("--output", type=Path, required=True)
    command.set_defaults(function=sbom)
    for name, function in (("c-build", c_build), ("c-package", c_package)):
        command = commands.add_parser(name)
        command.add_argument("--source", type=Path, required=True)
        command.add_argument("--tag", required=True)
        command.add_argument("--target", required=True)
        if function is c_package:
            command.add_argument("--output", type=Path, required=True)
        command.set_defaults(function=function)
    command = commands.add_parser("maven-bundle")
    command.add_argument("--tag", required=True)
    command.add_argument("--artifacts", type=Path, required=True)
    command.add_argument("--output", type=Path, required=True)
    command.add_argument("--sign", action="store_true")
    command.set_defaults(function=maven_bundle)
    command = commands.add_parser("maven-upload")
    command.add_argument("--tag", required=True)
    command.add_argument("--bundle", type=Path, required=True)
    command.add_argument("--timeout", type=int, default=1800)
    command.set_defaults(function=maven_upload)
    command = commands.add_parser("maven-verify")
    command.add_argument("--tag", required=True)
    command.add_argument("--artifacts", type=Path, required=True)
    command.add_argument("--output", type=Path, required=True)
    command.add_argument("--timeout", type=int, default=3600)
    command.set_defaults(function=maven_verify)
    command = commands.add_parser("check-binstall")
    command.add_argument("--source", type=Path, required=True)
    command.add_argument("--tag", required=True)
    command.set_defaults(function=lambda args: print(
        "cargo-binstall metadata matches the release archives" if check_binstall(args.source, args.tag)
        else "no cargo-binstall metadata"))
    command = commands.add_parser("github-release")
    command.add_argument("--tag", required=True)
    command.add_argument("--artifacts", type=Path, required=True)
    command.add_argument("--notes", type=Path, required=True)
    command.add_argument("--create", action="store_true")
    command.add_argument("--c-library", action="store_true", help="the release has C library archives")
    command.set_defaults(function=github_release)
    command = commands.add_parser("verify")
    command.add_argument("--version", required=True)
    command.add_argument("--sha", required=True)
    command.add_argument("--timeout", type=int, default=900)
    command.set_defaults(function=verify)
    args = parser.parse_args()
    args.function(args)


if __name__ == "__main__":
    main()
