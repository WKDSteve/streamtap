"""Tests of check-color-literals.py: a colour is found wherever a page states one in its CSS (a .css file, a <style>
block, a style attribute, a style directive, an SVG paint attribute); a selector, a comment, a string, url(...), the
script and the text of the markup pass; a run that reads no file is no pass.

    python -m unittest discover tools
"""

import importlib.util
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

SPEC = importlib.util.spec_from_file_location(
    "check_color_literals", Path(__file__).with_name("check-color-literals.py")
)
colors = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(colors)


def css(text):
    return colors.problems("ui/src/x.css", text)


def svelte(text):
    return colors.problems("ui/src/views/X.svelte", text)


class Found(unittest.TestCase):
    def test_hex_and_functions_in_a_declaration(self):
        text = ".a {\n  color: #fff;\n  background: rgb(0 0 0 / 0.45);\n  border-color: #1f5fbf80;\n}\n"
        self.assertEqual(css(text), [(2, "#fff"), (3, "rgb("), (4, "#1f5fbf80")])

    def test_every_colour_function_in_any_case(self):
        for fn in ["rgba", "hsl", "hsla", "hwb", "lab", "lch", "oklab", "oklch", "color", "RGB", "Hsl", "OKLCH"]:
            with self.subTest(fn=fn):
                self.assertEqual(css(f".a {{ color: {fn}(1 2 3); }}"), [(1, f"{fn}(")])

    def test_a_custom_property_outside_the_tokens(self):
        self.assertEqual(css(":root { --x: #123456; }"), [(1, "#123456")])

    def test_the_last_declaration_of_a_block_and_of_a_file_without_a_semicolon(self):
        self.assertEqual(css(".a { color: #123 }"), [(1, "#123")])
        self.assertEqual(css("color: #123"), [(1, "#123")])

    def test_a_multi_line_value_and_important(self):
        text = ".a {\n  box-shadow:\n    0 1px 2px #000,\n    0 0 0 1px var(--line) !important;\n}\n"
        self.assertEqual(css(text), [(3, "#000")])
        self.assertEqual(css(".a { color: #fff !important; }"), [(1, "#fff")])

    def test_a_colour_after_a_quoted_string_or_url(self):
        # A quote used to end the value early and hide what followed it.
        self.assertEqual(css(".a { background: url('x.png') #fff; }"), [(1, "#fff")])
        self.assertEqual(css('.a { background: url("data:,x") #000; }'), [(1, "#000")])
        self.assertEqual(css(".a { font-family: 'A B', sans-serif; color: #abc; }"), [(1, "#abc")])

    def test_nested_rules_and_media_blocks(self):
        text = "@media (min-width: 40rem) {\n  .a {\n    &:hover { color: #0f0; }\n  }\n}\n"
        self.assertEqual(css(text), [(3, "#0f0")])

    def test_the_style_block_of_a_component(self):
        text = "<div>x</div>\n<style>\n  .a { box-shadow: 0 6px 20px rgb(0 0 0 / 0.25); }\n</style>\n"
        self.assertEqual(svelte(text), [(3, "rgb(")])

    def test_a_style_attribute_with_either_quote_and_an_expression(self):
        self.assertEqual(svelte('<div style="color: #abc">x</div>'), [(1, "#abc")])
        self.assertEqual(svelte("<div style='color:#abc'>x</div>"), [(1, "#abc")])
        self.assertEqual(svelte('<div style="color: {dark ? \'#111\' : c}">x</div>'), [(1, "#111")])
        self.assertEqual(svelte("<div style={`color: #222`}>x</div>"), [(1, "#222")])

    def test_style_directives(self):
        for text, literal in [
            ('<div style:color="#fff">x</div>', "#fff"),
            ('<div style:background={"#fff"}>x</div>', "#fff"),
            ('<div style:--x="#123456">x</div>', "#123456"),
            ("<div style:color|important={`rgb(1,2,3)`}>x</div>", "rgb("),
        ]:
            with self.subTest(text=text):
                self.assertEqual(svelte(text), [(1, literal)])

    def test_svg_paint_attributes(self):
        for attribute in ["fill", "stroke", "stop-color", "flood-color", "lighting-color", "color"]:
            with self.subTest(attribute=attribute):
                self.assertEqual(svelte(f'<svg><path {attribute}="#c00"/></svg>'), [(1, "#c00")])
        self.assertEqual(svelte("<rect fill={'#c00'} />"), [(1, "#c00")])

    def test_a_style_block_after_a_slash_star_in_the_markup(self):
        # Comment stripping over the whole markup used to swallow the declarations after a `/*` in the text.
        text = "<p>a /* b</p>\n<style>\n  .a { color: #fff; }\n  /* note */\n</style>\n"
        self.assertEqual(svelte(text), [(3, "#fff")])


class Passed(unittest.TestCase):
    def test_tokens_and_their_mixes(self):
        text = ".a { color: var(--text); background: color-mix(in srgb, var(--accent) 14%, transparent); }"
        self.assertEqual(css(text), [])

    def test_a_selector_with_a_hex_looking_id(self):
        self.assertEqual(css("a:hover #add, #face { color: var(--text); }"), [])
        self.assertEqual(css("a:not(#abc), b { color: currentColor; }"), [])

    def test_strings_comments_and_urls(self):
        self.assertEqual(css('.a::before { content: "#fff rgb(1 2 3)"; }'), [])
        self.assertEqual(css(".a { /* was color: #fff; */ color: var(--text); }"), [])
        self.assertEqual(css(".a { fill: url(#fade); mask: url('#m'); }"), [])
        self.assertEqual(css('.a::after { content: "/*"; color: var(--text); }'), [])

    def test_the_script_the_text_and_other_attributes_of_the_markup(self):
        text = (
            "<!-- color: #fff; -->\n<script>\n  el.style.color = 'rgb(1 2 3)';\n</script>\n"
            '<a href="#add">Issue: #123;</a>\n<input placeholder="Colour: #ffffff" />\n'
            '<svg><use href="#face" /><path fill="url(#grad)" d="M0 0" /></svg>\n'
            '<div style:color="var(--text)" style="color: var(--muted)">x</div>\n'
            "<style>\n  .a { color: var(--text); }\n</style>\n"
        )
        self.assertEqual(svelte(text), [])


class Files(unittest.TestCase):
    def test_the_stylesheets_under_a_root_and_not_the_tokens(self):
        roots, tokens = ["ui-kit/src"], "ui-kit/src/tokens.css"
        self.assertTrue(colors.checked("ui-kit/src/base.css", roots, tokens))
        self.assertTrue(colors.checked("ui-kit/src/MenuBar.svelte", roots, tokens))
        self.assertFalse(colors.checked("ui-kit/src/tokens.css", roots, tokens))
        self.assertFalse(colors.checked("ui-kit/src/sse.ts", roots, tokens))
        self.assertFalse(colors.checked("ui-kit/srcx/a.css", roots, tokens))
        self.assertFalse(colors.checked("docs/style.css", roots, tokens))

    def test_several_token_files(self):
        roots = ["ui/src"]
        tokens = ["ui/src/tokens.css", "ui\\src\\palette.css"]
        self.assertFalse(colors.checked("ui/src/tokens.css", roots, tokens))
        self.assertFalse(colors.checked("ui/src/palette.css", roots, tokens))
        self.assertTrue(colors.checked("ui/src/app.css", roots, tokens))
        self.assertTrue(colors.checked("ui/src/palette.css", roots, []))

    def test_a_root_in_another_spelling(self):
        for root in ["ui-kit\\src", "./ui-kit/src", "ui-kit/src/", ".\\ui-kit\\src\\"]:
            with self.subTest(root=root):
                self.assertTrue(colors.checked("ui-kit/src/base.css", [root], ".\\ui-kit\\src\\tokens.css"))
                self.assertFalse(colors.checked("ui-kit/src/tokens.css", [root], ".\\ui-kit\\src\\tokens.css"))


class Main(unittest.TestCase):
    """main() over a real throwaway repository: the paths are git's, whatever the current folder."""

    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.top = self.dir.name
        os.makedirs(os.path.join(self.top, "ui", "src", "views"))
        files = {
            "ui/src/app.css": ".a { color: var(--text); }\n",
            "ui/src/palette.css": ":root { --accent: light-dark(#0f6e56, #5dcaa5); }\n",
            "ui/src/views/Bad.svelte": "<style>\n  .b { color: #fff; }\n</style>\n",
        }
        for path, text in files.items():
            with open(os.path.join(self.top, path), "w", encoding="utf-8", newline="\n") as handle:
                handle.write(text)
        for args in (["init", "-q"], ["add", "-A"]):
            subprocess.run(["git", *args], cwd=self.top, check=True, capture_output=True)
        self.cwd = os.getcwd()

    def tearDown(self):
        os.chdir(self.cwd)
        self.dir.cleanup()

    def run_main(self, argv, cwd=None):
        os.chdir(cwd or self.top)
        with mock.patch("builtins.print") as printed:
            code = colors.main(argv)
        return code, [str(c.args[0]) for c in printed.call_args_list]

    def test_a_finding_fails_and_is_named(self):
        code, lines = self.run_main(["--root", "ui/src", "--tokens", "ui/src/palette.css"])
        self.assertEqual(code, 1)
        self.assertIn("ui/src/views/Bad.svelte:2: #fff", lines)
        self.assertFalse(any("palette.css" in line for line in lines))

    def test_the_palette_is_a_finding_unless_it_is_named(self):
        os.remove(os.path.join(self.top, "ui", "src", "views", "Bad.svelte"))
        subprocess.run(["git", "add", "-A"], cwd=self.top, check=True, capture_output=True)
        code, lines = self.run_main(["--root", "ui/src"])
        self.assertEqual(code, 1)
        self.assertIn("ui/src/palette.css:1: #0f6e56", lines)
        code, _ = self.run_main(["--root", "ui/src", "--tokens", "ui/src/tokens.css", "--tokens", "ui/src/palette.css"])
        self.assertEqual(code, 0)

    def test_from_a_subfolder_and_with_backslashes(self):
        code, _ = self.run_main(["--root", "ui\\src"], cwd=os.path.join(self.top, "ui"))
        self.assertEqual(code, 1)

    def test_a_root_that_holds_no_stylesheet_is_no_pass(self):
        code, _ = self.run_main(["--root", "nope/src"])
        self.assertEqual(code, 2)

    def test_a_clean_tree_passes(self):
        os.remove(os.path.join(self.top, "ui", "src", "views", "Bad.svelte"))
        subprocess.run(["git", "add", "-A"], cwd=self.top, check=True, capture_output=True)
        code, _ = self.run_main(["--tokens", "ui/src/palette.css"])
        self.assertEqual(code, 0)


if __name__ == "__main__":
    unittest.main()
