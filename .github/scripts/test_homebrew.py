import argparse
from pathlib import Path
import re
import tempfile
import unittest

import homebrew
import release


def checksums(tag="v0.5.0", targets=release.TARGETS):
    return {f"{release.archive_stem(tag, target)}.tar.gz": f"{index:064x}" for index, target in enumerate(targets)}


class HomebrewTests(unittest.TestCase):
    def test_formula_installs_the_macos_and_static_linux_archives(self):
        sums = checksums()
        text = homebrew.formula("v0.5.0", sums)
        urls = re.findall(r'url "([^"]+)"\n\s+sha256 "([0-9a-f]{64})"', text)
        expected = {"aarch64-apple-darwin", "x86_64-apple-darwin", "aarch64-unknown-linux-musl",
                    "x86_64-unknown-linux-musl"}
        self.assertEqual({url.rsplit("/", 1)[1] for url, _ in urls},
                         {f"tickertape-v0.5.0-{target}.tar.gz" for target in expected})
        for url, digest in urls:
            self.assertTrue(url.startswith("https://github.com/thomas0903/tickertape/releases/download/v0.5.0/"))
            self.assertEqual(sums[url.rsplit("/", 1)[1]], digest)
        self.assertNotIn("linux-gnu", text)
        # Homebrew scans the version from the URL; an explicit version fails `brew audit --strict`.
        self.assertNotRegex(text, r"^\s+version ")
        self.assertIn("test do", text)
        self.assertEqual(homebrew.formula_version(text), (0, 5, 0))

    def test_formula_blocks_pair_each_platform_with_its_archive(self):
        text = homebrew.formula("v0.5.0", checksums())
        blocks = re.findall(r"(on_macos|on_linux) do\n(.*?)\n  end\n", text, re.DOTALL)
        self.assertEqual([name for name, _ in blocks], ["on_macos", "on_linux"])
        for name, body in blocks:
            arm, intel = re.findall(r"on_(arm|intel) do\n\s+url \"([^\"]+)\"", body)
            self.assertEqual((arm[0], intel[0]), ("arm", "intel"))
            self.assertIn("aarch64-", arm[1])
            self.assertIn("x86_64-", intel[1])
            self.assertIn("apple-darwin" if name == "on_macos" else "linux-musl", arm[1] + intel[1])

    def test_missing_archives_and_malformed_sums_are_errors(self):
        sums = checksums()
        del sums["tickertape-v0.5.0-aarch64-unknown-linux-musl.tar.gz"]
        with self.assertRaisesRegex(ValueError, "aarch64-unknown-linux-musl"):
            homebrew.formula("v0.5.0", sums)
        for line in ("abc  file.tar.gz", f"{'0' * 64} file.tar.gz", f"{'0' * 64}  two words"):
            with self.subTest(line=line), self.assertRaisesRegex(ValueError, "malformed"):
                homebrew.parse_checksums(line + "\n")
        with self.assertRaises(ValueError):
            homebrew.formula("main", checksums())

    def test_backfilling_an_older_tag_keeps_a_newer_formula(self):
        newer = homebrew.formula("v0.10.0", checksums("v0.10.0"))
        self.assertTrue(homebrew.should_write(None, "v0.5.0"))
        self.assertTrue(homebrew.should_write(newer, "v0.10.0"))
        self.assertTrue(homebrew.should_write(newer, "v0.10.1"))
        self.assertFalse(homebrew.should_write(newer, "v0.9.0"))
        self.assertTrue(homebrew.should_write("# README only\n", "v0.5.0"))

    def test_generate_writes_the_formula_unless_the_tap_is_newer(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            sums = root / "SHA256SUMS"
            sums.write_text("".join(f"{digest}  {name}\n" for name, digest in checksums().items()))
            output = root / "tap" / "Formula" / "tickertape.rb"
            args = argparse.Namespace(tag="v0.5.0", checksums=sums, output=output)
            homebrew.generate(args)
            self.assertEqual(output.read_text(), homebrew.formula("v0.5.0", checksums()))
            output.write_text(homebrew.formula("v0.6.0", checksums("v0.6.0")))
            homebrew.generate(args)
            self.assertEqual(homebrew.formula_version(output.read_text()), (0, 6, 0))


if __name__ == "__main__":
    unittest.main()
