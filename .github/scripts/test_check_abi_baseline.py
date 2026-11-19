"""Tests for check_abi_baseline.py (needs a C compiler: `$CC`, default `cc`)."""

from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import check_abi_baseline as abi  # noqa: E402

REPO = Path(__file__).resolve().parents[2]
HEADER = REPO / "crates/tickertape-ffi/include/tickertape.h"
BASELINE = REPO / "crates/tickertape-ffi/abi/tickertape-baseline.h"

BASE = """
#ifndef TICKERTAPE_H
#define TICKERTAPE_H
#include <stdint.h>
#include <stddef.h>
#define TICKERTAPE_ABI_VERSION 1
#define TICKERTAPE_VERSION_MAJOR 1
#define TICKERTAPE_VERSION_NUMBER 1000000
enum tickertape_status_code
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : int32_t
#endif
 {
  /** Success. */
  TICKERTAPE_OK = 0,
  TICKERTAPE_E_IO = -2,
};
typedef int32_t tickertape_status;
typedef struct tickertape_writer tickertape_writer;
typedef struct tickertape_record {
  uint64_t seq;
  const uint8_t *payload;
  size_t payload_len;
} tickertape_record;
typedef struct tickertape_verify_report {
  uint64_t struct_size;
  uint64_t records;
} tickertape_verify_report;
typedef int32_t (*tickertape_fill_fn)(void *user_data, uint8_t *buf, size_t len);
#ifdef __cplusplus
extern "C" {
#endif
/** Appends. */
tickertape_status tickertape_writer_append(tickertape_writer *writer, const uint8_t *payload, size_t payload_len);
uint32_t tickertape_version_number(void);
#ifdef __cplusplus
}
#endif
#endif
"""


class BaselineTest(unittest.TestCase):
    def check(self, header: str, baseline: str = BASE) -> list[str]:
        with tempfile.TemporaryDirectory() as tmp:
            base_path, header_path = Path(tmp, "base.h"), Path(tmp, "header.h")
            base_path.write_text(baseline)
            header_path.write_text(header)
            return abi.problems(abi.declarations(base_path), abi.declarations(header_path))

    def assert_breaks(self, header: str, fragment: str) -> None:
        found = self.check(header)
        self.assertTrue(any(fragment in p for p in found), found)

    def test_unchanged_and_reformatted_pass(self) -> None:
        self.assertEqual(self.check(BASE), [])
        self.assertEqual(self.check(BASE.replace("uint8_t *payload, size_t", "uint8_t*  payload,\n size_t")), [])

    def test_additions_pass(self) -> None:
        added = BASE.replace("uint32_t tickertape_version_number(void);",
                             "uint32_t tickertape_version_number(void);\nvoid tickertape_new_function(void);")
        added = added.replace("TICKERTAPE_E_IO = -2,", "TICKERTAPE_E_IO = -2,\n  TICKERTAPE_E_NEW = -3,")
        added = added.replace("  uint64_t records;\n", "  uint64_t records;\n  uint64_t added_later;\n")
        added = added.replace("#define TICKERTAPE_ABI_VERSION 1", "#define TICKERTAPE_ABI_VERSION 1\n#define TICKERTAPE_NEW 7")
        self.assertEqual(self.check(added), [])

    def test_version_macros_may_change(self) -> None:
        bumped = BASE.replace("TICKERTAPE_VERSION_NUMBER 1000000", "TICKERTAPE_VERSION_NUMBER 1003000")
        self.assertEqual(self.check(bumped), [])

    def test_removing_or_changing_a_function_fails(self) -> None:
        self.assert_breaks(BASE.replace("uint32_t tickertape_version_number(void);", ""),
                           "function tickertape_version_number was removed")
        self.assert_breaks(BASE.replace("uint32_t tickertape_version_number", "uint64_t tickertape_version_number"),
                           "function tickertape_version_number changed")
        self.assert_breaks(BASE.replace("const uint8_t *payload, size_t payload_len);",
                                        "size_t payload_len, const uint8_t *payload);"),
                           "function tickertape_writer_append changed")

    def test_changing_a_constant_or_enumerator_fails(self) -> None:
        self.assert_breaks(BASE.replace("TICKERTAPE_ABI_VERSION 1", "TICKERTAPE_ABI_VERSION 2"),
                           "constant TICKERTAPE_ABI_VERSION changed")
        self.assert_breaks(BASE.replace("TICKERTAPE_E_IO = -2", "TICKERTAPE_E_IO = -3"),
                           "enumerator TICKERTAPE_E_IO changed")
        self.assert_breaks(BASE.replace("  TICKERTAPE_E_IO = -2,\n", ""), "enumerator TICKERTAPE_E_IO was removed")

    def test_changing_a_frozen_struct_fails(self) -> None:
        swapped = BASE.replace("  const uint8_t *payload;\n  size_t payload_len;\n",
                               "  size_t payload_len;\n  const uint8_t *payload;\n")
        self.assert_breaks(swapped, "struct tickertape_record changed")
        grown = BASE.replace("  size_t payload_len;\n} tickertape_record;",
                             "  size_t payload_len;\n  uint32_t more;\n} tickertape_record;")
        self.assert_breaks(grown, "struct tickertape_record changed")

    def test_extensible_structs_only_grow_at_the_end(self) -> None:
        inserted = BASE.replace("  uint64_t struct_size;\n", "  uint64_t struct_size;\n  uint64_t inserted;\n")
        self.assert_breaks(inserted, "extensible struct tickertape_verify_report changed")

    def test_typedefs_are_frozen(self) -> None:
        self.assert_breaks(BASE.replace("uint8_t *buf, size_t len", "uint8_t *buf, uint64_t len"),
                           "typedef tickertape_fill_fn changed")
        self.assert_breaks(BASE.replace("typedef struct tickertape_writer tickertape_writer;", ""),
                           "typedef tickertape_writer was removed")

    def test_the_committed_header_against_itself_and_the_baseline(self) -> None:
        current = abi.declarations(HEADER)
        kinds = {kind for kind, _ in current}
        self.assertEqual(kinds, {"constant", "enumerator", "function", "struct", "typedef"})
        self.assertIn(("function", "tickertape_writer_append_batch"), current)
        self.assertEqual(current[("struct", "tickertape_verify_report")][0], abi.EXTENSIBLE_FIRST_FIELD)
        self.assertEqual(abi.problems(current, current), [])
        self.assertEqual(abi.problems(abi.declarations(BASELINE), current), [])
        self.assertEqual(abi.main(["check", str(BASELINE), str(HEADER)]), 0)


if __name__ == "__main__":
    unittest.main()
