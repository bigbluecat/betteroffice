"""Post-install check for a built wheel: open, edit, recalculate, render.

Run against an installed wheel rather than the source tree, so it catches
packaging faults the test suite cannot see.
"""

from pathlib import Path

import betteroffice_xlsx as bo

PNG_MAGIC = b"\x89PNG\r\n\x1a\n"
FIXTURE = Path(__file__).resolve().parents[3] / "apps" / "demo" / "public" / "sample.xlsx"


def main() -> None:
    workbook = bo.Workbook.open_path(FIXTURE)
    assert workbook.sheet_names == ["Budget", "Summary", "Styled"], workbook.sheet_names

    sheet = workbook["Budget"]
    addend = sheet["C3"]
    sheet["B3"] = 1000
    assert sheet["D3"] == 1000 + addend, f"dependent did not recalculate: {sheet['D3']}"

    png = workbook.render_png("Budget", range="A1:D12")
    assert png.bytes[:8] == PNG_MAGIC, "render did not produce a PNG"
    assert png.width > 0 and png.height > 0

    assert bo.Workbook.open(workbook.save()).sheet_names == workbook.sheet_names

    print(f"wheel smoke test ok: betteroffice-xlsx {bo.__version__}")


if __name__ == "__main__":
    main()
