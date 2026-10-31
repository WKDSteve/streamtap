"""Tests of tools/check-control-chars.py: `python -m unittest discover tools`."""

import importlib.util
import pathlib
import unittest

spec = importlib.util.spec_from_file_location(
    "check_control_chars", pathlib.Path(__file__).with_name("check-control-chars.py")
)
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)


def found(text, encoding="utf-8"):
    return check.problems("x", text.encode(encoding))


class ControlCharacters(unittest.TestCase):
    def test_plain_text_and_line_endings_pass(self):
        self.assertEqual(found("a b\r\nc\n\n"), [])

    def test_a_tab_is_refused_except_where_the_format_needs_it(self):
        self.assertEqual(found("a\tb"), [(1, 9)])
        for name in ["Makefile", "build/rules.mk", "data/table.tsv", ".gitmodules"]:
            self.assertEqual(check.problems(name, b"a\tb"), [], name)
        self.assertEqual(check.problems("docs/notes.md", b"a\tb"), [(1, 9)])

    def test_each_refused_range_is_found_with_its_line(self):
        for code in [0x01, 0x08, 0x0B, 0x0C, 0x0E, 0x1F, 0x7F, 0x85, 0x9F, 0xAD,
                     0x200B, 0x200F, 0x2028, 0x2029, 0x202A, 0x202E,
                     0x2060, 0x2064, 0x2066, 0x2069, 0xFEFF]:
            with self.subTest(code=hex(code)):
                self.assertEqual(found("ok\nx" + chr(code) + "y"), [(2, code)])

    def test_a_backspace_in_a_path_is_found(self):
        self.assertEqual(found("install to C:/apps" + chr(8) + "tasks\n"), [(1, 8)])

    def test_a_byte_order_mark_at_the_start_is_not_a_finding_but_later_ones_are(self):
        self.assertEqual(found(chr(0xFEFF) + "text"), [])
        self.assertEqual(found("text" + chr(0xFEFF)), [(1, 0xFEFF)])

    def test_binary_and_non_utf8_files_are_skipped(self):
        self.assertEqual(check.problems("x", b"\x00\x01\x02"), [])
        self.assertEqual(check.problems("x", b"\xff\xfe\x01"), [])

    def test_ordinary_non_ascii_text_passes(self):
        self.assertEqual(found("Gr\u00fc\u00dfe \u2014 \u65e5\u672c\u8a9e \U0001F600"), [])

    def test_a_control_character_in_a_file_name_is_found(self):
        self.assertEqual(check.name_problems("docs/read" + chr(8) + "me.md"), [8])
        self.assertEqual(check.name_problems("docs/a" + chr(0x202E) + "b.md"), [0x202E])
        self.assertEqual(check.name_problems("docs/Grüße/readme.md"), [])

    def test_this_tool_and_its_test_hold_none(self):
        here = pathlib.Path(__file__).parent
        for name in ("check-control-chars.py", "test_check_control_chars.py"):
            self.assertEqual(check.problems(name, (here / name).read_bytes()), [], name)


if __name__ == "__main__":
    unittest.main()
