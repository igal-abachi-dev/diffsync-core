"""Generates the encoding corpus (backlog E1-S1.4). Run with `python -I gen.py` from this
directory. The files are checked in; the script documents how they were made and lets a
reviewer regenerate and diff them.

Matrix: encodings x line-ending styles. Every file holds the same logical lines:

    LINES = ["first line", "second", "", "<non-ASCII line>", "last line"]

The non-ASCII line depends on what the encoding can hold: Hebrew + emoji for the Unicode
encodings, Hebrew for windows-1255, accented Latin for windows-1252. File names are
`<encoding>__<endings>.txt`.
"""
import pathlib

HERE = pathlib.Path(__file__).parent

UNICODE_LINE = "שלום עולם 😀"
HEBREW_LINE = "שלום עולם"
LATIN_LINE = "café déjà vu"

ENDINGS = {
    "crlf": ("\r\n", True),   # (separator, final ending present)
    "lf": ("\n", True),
    "cr": ("\r", True),
    "nofinal": ("\n", False),  # LF endings, no ending on the last line
}


def lines_for(non_ascii):
    return ["first line", "second", "", non_ascii, "last line"]


def join(lines, sep, final):
    text = sep.join(lines)
    return text + (sep if final else "")


def mixed(lines):
    # one of each ending, no final ending
    seps = ["\r\n", "\n", "\r", "\r\n"]
    out = ""
    for line, sep in zip(lines, seps + [""]):
        out += line + sep
    return out


def utf16(text, le, bom):
    raw = text.encode("utf-16-le" if le else "utf-16-be")
    return (b"\xff\xfe" if le else b"\xfe\xff") + raw if bom else raw


ENCODERS = {
    "utf8": (UNICODE_LINE, lambda t: t.encode("utf-8")),
    "utf8bom": (UNICODE_LINE, lambda t: b"\xef\xbb\xbf" + t.encode("utf-8")),
    "utf16le_bom": (UNICODE_LINE, lambda t: utf16(t, True, True)),
    "utf16be_bom": (UNICODE_LINE, lambda t: utf16(t, False, True)),
    "utf16le_nobom": (UNICODE_LINE, lambda t: utf16(t, True, False)),
    "utf16be_nobom": (UNICODE_LINE, lambda t: utf16(t, False, False)),
    "cp1252": (LATIN_LINE, lambda t: t.encode("cp1252")),
    "cp1255": (HEBREW_LINE, lambda t: t.encode("cp1255")),
}

# Which style combinations are written. UTF-8 and the BOM-less UTF-16 files get the whole
# matrix; the others get the two most telling styles, which keeps the corpus small.
FULL = ["crlf", "lf", "cr", "nofinal", "mixed"]
STYLES = {
    "utf8": FULL,
    "utf8bom": ["crlf", "lf"],
    "utf16le_bom": ["crlf", "lf"],
    "utf16be_bom": ["crlf"],
    "utf16le_nobom": ["crlf", "lf"],
    "utf16be_nobom": ["crlf"],
    "cp1252": ["crlf", "lf"],
    "cp1255": ["crlf", "nofinal"],
}


def main():
    written = []
    for enc, (non_ascii, encode) in ENCODERS.items():
        lines = lines_for(non_ascii)
        for style in STYLES[enc]:
            if style == "mixed":
                text = mixed(lines)
            else:
                sep, final = ENDINGS[style]
                text = join(lines, sep, final)
            path = HERE / f"{enc}__{style}.txt"
            path.write_bytes(encode(text))
            written.append(path.name)
    # edge cases that are not part of the matrix
    (HERE / "empty__none.txt").write_bytes(b"")
    (HERE / "utf8__only_endings.txt").write_bytes(b"\r\n\r\n\n")
    (HERE / "binary__nul.bin").write_bytes(b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x01\x00")
    written += ["empty__none.txt", "utf8__only_endings.txt", "binary__nul.bin"]
    print(f"{len(written)} files")


if __name__ == "__main__":
    main()
