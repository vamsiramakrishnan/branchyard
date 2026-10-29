"""Real Office/PDF files, independent section ownership and atomic failure paths. No model calls."""

import copy
import importlib.util
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
import zipfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from unittest import mock
from xml.etree import ElementTree

ROOT = Path(__file__).resolve().parents[1]
SDK = ROOT / "sdk/python"
sys.path.insert(0, str(SDK))

from branchyard_artifacts import (  # noqa: E402  # noqa: E402
    ArtifactError,
    Unavailable,
    common,
    deck,
    docx,
    manifest,
    native,
    pdf,
    pptx,
    xlsx,
)
from branchyard_artifacts.__main__ import main  # noqa: E402
from branchyard_artifacts.doctor import doctor, uno_python  # noqa: E402

EXAMPLES = SDK / "branchyard_artifacts/examples"
OPTIONAL = all(
    importlib.util.find_spec(name) for name in ["docx", "openpyxl", "pptx", "pypdf", "reportlab", "jsonschema", "PIL"]
)
FONTS = shutil.which("fc-match") is not None


class WorkspaceCase(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def spec(self, value, name="spec.json"):
        path = self.root / name
        path.write_text(json.dumps(value))
        return path


class FileTests(WorkspaceCase):
    def test_atomic_writer_preserves_previous_output_on_failure(self):
        out = self.root / "report.pdf"
        out.write_bytes(b"original")
        with self.assertRaisesRegex(RuntimeError, "stop"), common.output_file(out) as staged:
            staged.write_bytes(b"partial")
            raise RuntimeError("stop")
        self.assertEqual(out.read_bytes(), b"original")
        self.assertEqual(list(self.root.iterdir()), [out])

    def test_boundaries_refuse_symlinks_traversal_nan_and_boolean_measures(self):
        path = self.root / "data.txt"
        path.write_text("evidence")
        (self.root / "alias.txt").symlink_to(path)
        for relative in ["../outside", "alias.txt", "/absolute", "missing"]:
            with self.subTest(relative=relative), self.assertRaises(ArtifactError):
                common.contained(self.root, relative)
        for value in [True, float("nan"), float("inf"), "1"]:
            with self.subTest(value=value), self.assertRaises(ArtifactError):
                common.finite(value, "measure")
        path = self.root / "bad.json"
        path.write_text('{"value": NaN}')
        with self.assertRaisesRegex(ArtifactError, "non-finite"):
            common.load_json(path)
        path.write_text('{"value": 1e999}')
        with self.assertRaisesRegex(ArtifactError, "finite"):
            common.load_json(path)

    def test_manifest_is_deterministic_scoped_and_does_not_publish(self):
        one, two = self.root / "one.pdf", self.root / "two.pptx"
        one.write_bytes(b"pdf fixture")
        two.write_bytes(b"presentation fixture")
        out = self.root / "artifacts.json"
        result = manifest.create([two, one], out)
        original = out.read_bytes()
        manifest.create([one, two], out)
        self.assertEqual(out.read_bytes(), original)
        value = json.loads(original)
        self.assertEqual(value["schema_version"], 1)
        self.assertEqual(value["files"][0]["path"], "one.pdf")
        self.assertEqual(value["files"][0]["sha256"], common.digest(one))
        self.assertFalse(result["published"])
        for files in [[one, one], [out], [ROOT / "README.md"]]:
            with self.subTest(files=files), self.assertRaises(ArtifactError):
                manifest.create(files, out)
        self.assertEqual(out.read_bytes(), original)

    def test_missing_dependency_and_native_requirements_are_unavailable(self):
        with mock.patch("importlib.import_module", side_effect=ImportError("fixture")), self.assertRaises(Unavailable):
            common.library("docx")
        with mock.patch("shutil.which", return_value=None):
            result = doctor(["render", "pivot", "unknown"])
            self.assertEqual(result["status"], "unavailable")
            self.assertEqual(result["missing"], ["render", "pivot", "unknown"])
            with self.assertRaises(Unavailable):
                native.render(self.root / "input.pptx", self.root / "preview")
            with self.assertRaises(Unavailable):
                native.create_pivot(self.root / "input.xlsx", EXAMPLES / "pivot.json", self.root / "pivot.xlsx")

    def test_cli_works_without_importing_optional_libraries(self):
        code = """
import sys
sys.path.insert(0, sys.argv[1])
import branchyard, branchyard_client, branchyard_artifacts
from branchyard_artifacts.__main__ import parser
parser().parse_args(['doctor'])
assert not any(name in sys.modules for name in ['docx','pptx','openpyxl','pypdf','reportlab','jsonschema','PIL'])
"""
        result = subprocess.run([sys.executable, "-I", "-c", code, str(SDK)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        with mock.patch("sys.stdout"):
            self.assertEqual(main(["doctor", "--require", "unknown"]), 3)
            with mock.patch.object(pdf, "library", side_effect=Unavailable("Missing pypdf")):
                self.assertEqual(main(["pdf", "read", str(self.root / "missing.pdf")]), 3)
            self.assertEqual(
                main(["pdf", "create", str(self.root / "missing.json"), "--out", str(self.root / "out.pdf")]), 1
            )


@unittest.skipUnless(OPTIONAL, "install branchyard[artifacts] to inspect real document files")
class FormatTests(WorkspaceCase):
    def test_docx_update_preserves_tables_header_footer_and_target_style(self):
        out = self.root / "brief.docx"
        value = json.loads((EXAMPLES / "docx.json").read_text())
        value["header"] = "Evidence header"
        value["tables"] = [{"headers": ["Source", "Value"], "rows": [["Report", "12"]]}]
        docx.create(self.spec(value), out)
        original = out.read_bytes()
        edit = self.spec({"schema_version": 1, "paragraphs": {"1": "Revised finding"}}, "edit.json")
        updated = self.root / "updated.docx"
        docx.update(out, edit, updated)
        self.assertEqual(out.read_bytes(), original)
        read = docx.read(updated)
        self.assertEqual(read["paragraphs"][1]["text"], "Revised finding")
        self.assertEqual(read["paragraphs"][1]["space_after_pt"], 8)
        self.assertEqual(read["tables"][0][1], ["Report", "12"])
        self.assertIn("Evidence header", read["headers"])
        self.assertIn("Prepared for review", read["footers"])
        with zipfile.ZipFile(updated) as archive:
            self.assertIn("word/document.xml", archive.namelist())
        invalid = self.spec({"schema_version": 1, "paragraphs": {"999": "bad"}})
        with self.assertRaises(ArtifactError):
            docx.update(updated, invalid, updated)
        self.assertEqual(docx.read(updated), read)

    def test_xlsx_native_chart_formula_table_and_update_preservation(self):
        value = json.loads((EXAMPLES / "xlsx.json").read_text())
        value["sheets"][0]["tables"] = [{"name": "Revenue", "range": "A1:B3"}]
        value["charts"] = [
            {
                "type": "column",
                "sheet": "Data",
                "title": "Revenue",
                "unit": "USD millions",
                "number_format": "0.0",
                "categories": "A2:A3",
                "values": "B1:B3",
                "anchor": "D2",
            }
        ]
        out = self.root / "data.xlsx"
        xlsx.create(self.spec(value), out)
        original = out.read_bytes()
        xlsx.update(
            out,
            self.spec({"schema_version": 1, "sheets": [{"name": "Data", "cells": {"B2": 21}}]}),
            self.root / "edited.xlsx",
        )
        self.assertEqual(out.read_bytes(), original)
        read = xlsx.read(self.root / "edited.xlsx")
        sheet = read["sheets"][0]
        self.assertEqual(sheet["cells"]["B2"]["value"], 21)
        self.assertEqual(sheet["cells"]["B4"]["value"], "=SUM(B2:B3)")
        self.assertEqual(sheet["cells"]["B2"]["number_format"], "0.0")
        self.assertEqual(sheet["tables"], ["Revenue"])
        self.assertEqual(sheet["charts"], 1)
        self.assertEqual(read["calculated"], "unavailable")
        with zipfile.ZipFile(self.root / "edited.xlsx") as archive:
            chart = ElementTree.fromstring(archive.read("xl/charts/chart1.xml"))
            self.assertIn("USD millions", "".join(chart.itertext()))
        value["sheets"][0]["cells"]["B2"] = float("inf")
        with self.assertRaises(ArtifactError):
            xlsx.create(self.spec(value), out)
        self.assertEqual(out.read_bytes(), original)

    def test_pdf_literal_text_metadata_and_overlay_preserve_pages(self):
        value = {
            "schema_version": 1,
            "title": "Literal PDF",
            "pages": [
                {"title": "First", "paragraphs": ["<img src='https://example.com'> is literal text"]},
                {"title": "Second", "paragraphs": ["Retained evidence"]},
            ],
        }
        out, edited = self.root / "brief.pdf", self.root / "edited.pdf"
        pdf.create(self.spec(value), out)
        original = out.read_bytes()
        self.assertTrue(original.startswith(b"%PDF-"))
        pdf.update(
            out,
            self.spec(
                {
                    "schema_version": 1,
                    "overlays": [{"page": 0, "text": "Reviewed", "x_pt": 50, "y_pt": 50, "font_size_pt": 12}],
                    "metadata": {"author": "Reviewer"},
                }
            ),
            edited,
        )
        read = pdf.read(edited)
        self.assertEqual(len(read["pages"]), 2)
        self.assertIn("Reviewed", read["pages"][0]["text"])
        self.assertIn("<img src=", read["pages"][0]["text"])
        self.assertIn("Retained evidence", read["pages"][1]["text"])
        self.assertEqual(read["metadata"]["/Author"], "Reviewer")
        self.assertEqual(out.read_bytes(), original)
        self.assertEqual(pdf.check(edited)["rendered"], "unavailable")

    @unittest.skipUnless(FONTS, "fontconfig is required for deck text measurement")
    def test_pptx_chart_is_editable_and_text_update_preserves_its_workbook(self):
        source = self.root / "deck.pptx"
        pptx.create(EXAMPLES / "deck.json", source)
        with zipfile.ZipFile(source) as archive:
            embedded = {name: archive.read(name) for name in archive.namelist() if name.startswith("ppt/embeddings/")}
        self.assertTrue(embedded)
        edit = {"schema_version": 1, "replacements": [{"slide": 0, "shape": 0, "text": "Revised revenue"}]}
        updated = self.root / "edited.pptx"
        pptx.update(source, self.spec(edit), updated)
        read = pptx.read(updated)
        self.assertEqual(read["slides"][0]["shapes"][0]["text"], "Revised revenue")
        chart = next(shape["chart"] for shape in read["slides"][0]["shapes"] if "chart" in shape)
        self.assertEqual(chart["series"][0]["values"], [12.0, 14.0, 18.0])
        with zipfile.ZipFile(updated) as archive:
            self.assertEqual({name: archive.read(name) for name in embedded}, embedded)
        self.assertIn("https://example.com/source", read["slides"][0]["notes"])

    def test_wrong_format_and_unknown_request_fields_fail(self):
        cases = [
            (docx, common.library("docx.opc.exceptions").PackageNotFoundError),
            (xlsx, zipfile.BadZipFile),
            (pdf, common.library("pypdf.errors").PdfReadError),
            (pptx, common.library("pptx.exc").PackageNotFoundError),
        ]
        for module, error in cases:
            path = self.root / ("broken." + module.__name__.rsplit(".", 1)[-1])
            path.write_bytes(b"not an Office document or PDF")
            with self.subTest(module=module.__name__), self.assertRaises(error):
                module.check(path)
        with self.assertRaisesRegex(ArtifactError, "Unknown fields"):
            pdf.create(self.spec({"schema_version": 1, "pages": [{}], "execute": "forbidden"}), self.root / "bad.pdf")


@unittest.skipUnless(OPTIONAL and FONTS, "artifact libraries and fontconfig are required")
class DeckTests(WorkspaceCase):
    def section_files(self, count=1, slides=1):
        design = json.loads((EXAMPLES / "deck.json").read_text())["design"]
        base = json.loads((EXAMPLES / "section.json").read_text())

        def worker(index):
            # Each worker owns one directory and file; none writes the final presentation.
            directory = self.root / ("worker-" + str(index))
            directory.mkdir()
            section = copy.deepcopy(base)
            section["id"] = "section" + str(index)
            section["slides"] = []
            for number in range(slides):
                slide = copy.deepcopy(base["slides"][0])
                slide["id"] = "slide" + str(number)
                slide["title"] = "Section " + str(index) + ", slide " + str(number)
                section["slides"].append(slide)
            path = directory / "section.json"
            path.write_text(json.dumps(section, sort_keys=True))
            return {"id": section["id"], "path": path.relative_to(self.root).as_posix(), "sha256": common.digest(path)}

        with ThreadPoolExecutor(max_workers=3) as workers:
            sections = list(workers.map(worker, range(count)))
        plan = {
            "schema_version": 1,
            "kind": "deck",
            "id": "independent-sections",
            "design": design,
            "sections": sections,
            "slide_order": [section["id"] + "/slide" + str(n) for section in sections for n in range(slides)],
        }
        return self.spec(plan, "deck.json"), plan

    def test_60_slide_assembly_is_reproducible_and_contains_editable_charts(self):
        spec, _ = self.section_files(count=3, slides=20)
        first, second = self.root / "first.pptx", self.root / "second.pptx"
        result = deck.assemble(spec, first)
        deck.assemble(spec, second)
        self.assertEqual(first.read_bytes(), second.read_bytes())
        self.assertEqual(result["slides"], 60)
        self.assertEqual(len(result["slide_order"]), 60)
        reopened = pptx.check(first)
        self.assertEqual(reopened["editable_charts"], 60)
        self.assertEqual(reopened["rendered"], "unavailable")

    def test_all_native_chart_kinds_preserve_editable_values_and_declared_design(self):
        _, plan = self.section_files()
        section_path = self.root / plan["sections"][0]["path"]
        section = common.load_json(section_path)
        for kind in ["column", "bar", "line", "pie"]:
            with self.subTest(kind=kind):
                section["slides"][0]["elements"][0]["chart_type"] = kind
                section_path.write_text(json.dumps(section))
                plan["sections"][0]["sha256"] = common.digest(section_path)
                out = self.root / (kind + ".pptx")
                deck.assemble(self.spec(plan), out)
                presentation = common.library("pptx").Presentation(out)
                chart = next(shape.chart for shape in presentation.slides[0].shapes if shape.has_chart)
                self.assertEqual(list(chart.series[0].values), [12, 14, 18])
                self.assertIn("USD millions", chart.chart_title.text_frame.text)
                self.assertEqual(chart.chart_title.text_frame.paragraphs[0].font.name, plan["design"]["font"])
                if kind == "pie":
                    self.assertTrue(chart.has_legend)
                    self.assertTrue(chart.plots[0].data_labels.show_category_name)
                else:
                    self.assertEqual(chart.value_axis.tick_labels.number_format, "0.0")

    def test_exact_inventory_digest_design_geometry_and_text_fail_before_replacement(self):
        path, plan = self.section_files()
        out = self.root / "protected.pptx"
        out.write_bytes(b"prior output")
        cases = []
        omitted = copy.deepcopy(plan)
        omitted["slide_order"] = ["unknown/slide"]
        cases.append(omitted)
        duplicate = copy.deepcopy(plan)
        duplicate["sections"] *= 2
        cases.append(duplicate)
        bad_digest = copy.deepcopy(plan)
        bad_digest["sections"][0]["sha256"] = "0" * 64
        cases.append(bad_digest)
        invalid_number = copy.deepcopy(plan)
        invalid_number["design"]["width_in"] = True
        cases.append(invalid_number)
        for case in cases:
            with self.subTest(case=case), self.assertRaises(ArtifactError):
                deck.assemble(self.spec(case, "bad.json"), out)
            self.assertEqual(out.read_bytes(), b"prior output")
        section_path = self.root / plan["sections"][0]["path"]
        original = common.load_json(section_path)
        for mutate in [
            lambda s: s.update(design_id="other"),
            lambda s: s["slides"][0]["elements"][0].update(box=[12, 2, 5, 3]),
            lambda s: s["slides"][0]["elements"][1].update(text="excessive " * 300),
            lambda s: s["slides"][0].update(citations=[]),
            lambda s: s["slides"][0]["elements"][0]["series"][0].update(values=[1]),
        ]:
            section = copy.deepcopy(original)
            mutate(section)
            section_path.write_text(json.dumps(section))
            plan["sections"][0]["sha256"] = common.digest(section_path)
            with self.assertRaises(ArtifactError):
                deck.assemble(self.spec(plan), out)
            self.assertEqual(out.read_bytes(), b"prior output")
        self.assertTrue(path.exists())

    def test_assets_table_template_and_section_check(self):
        path, plan = self.section_files()
        section_path = self.root / plan["sections"][0]["path"]
        image = section_path.parent / "asset.png"
        common.library("PIL.Image").new("RGB", (60, 40), "blue").save(image)
        section = common.load_json(section_path)
        section["assets"] = [{"id": "figure", "path": image.name, "sha256": common.digest(image)}]
        section["slides"][0]["elements"] = [
            {"kind": "image", "box": [0.7, 1.5, 3, 2], "asset_id": "figure"},
            {
                "kind": "table",
                "box": [4, 1.5, 7, 2],
                "headers": ["Item", "Value"],
                "rows": [["A", 12]],
                "font_size_pt": 14,
            },
        ]
        section_path.write_text(json.dumps(section))
        plan["sections"][0]["sha256"] = common.digest(section_path)
        self.assertEqual(deck.validate_section(section_path)["geometry"], "unavailable")
        self.assertEqual(deck.validate_section(section_path, path)["geometry"], "passed")
        presentation = common.library("pptx").Presentation()
        presentation.slide_width = common.library("pptx.util").Inches(plan["design"]["width_in"])
        presentation.slide_height = common.library("pptx.util").Inches(plan["design"]["height_in"])
        template = self.root / "template.pptx"
        presentation.save(template)
        plan["template"] = {"path": template.name, "sha256": common.digest(template)}
        result = deck.assemble(self.spec(plan), self.root / "images.pptx")
        self.assertEqual(result["slides"], 1)
        image.write_bytes(b"changed")
        with self.assertRaisesRegex(ArtifactError, "SHA-256"):
            deck.assemble(self.spec(plan), self.root / "images.pptx")


@unittest.skipUnless(OPTIONAL and shutil.which("libreoffice") and uno_python(), "native LibreOffice UNO is optional")
class NativeTests(WorkspaceCase):
    def test_native_pivot_contains_cache_records_and_real_aggregate_values(self):
        source = self.root / "source.xlsx"
        xlsx.create(EXAMPLES / "xlsx.json", source)
        out = self.root / "pivot.xlsx"
        result = xlsx.pivot(source, EXAMPLES / "pivot.json", out)
        self.assertEqual(result["native_pivots"], 1)
        self.assertEqual(result["calculated"], "passed")
        workbook = common.library("openpyxl").load_workbook(out, data_only=True)
        self.assertIn(("Q1", 12), list(workbook["Summary"].values))
        self.assertIn(("Total Result", 26), list(workbook["Summary"].values))
        self.assertEqual(workbook["Data"]["B4"].value, 26)
        with zipfile.ZipFile(out) as archive:
            definition = ElementTree.fromstring(archive.read("xl/pivotCache/pivotCacheDefinition1.xml"))
            self.assertEqual(definition.attrib["recordCount"], "2")
            self.assertIn("xl/pivotCache/pivotCacheRecords1.xml", archive.namelist())

    @unittest.skipUnless(shutil.which("pdftoppm"), "poppler rasterizer is optional")
    def test_native_render_counts_and_decodes_every_page_without_claiming_visual_review(self):
        source = self.root / "input.docx"
        docx.create(EXAMPLES / "docx.json", source)
        preview = self.root / "preview"
        result = native.render(source, preview)
        self.assertEqual(result["rendered"], "passed")
        self.assertEqual(result["visual_review"], "not_performed")
        self.assertEqual(len(list(preview.glob("page-*.png"))), result["pages"])
        with self.assertRaisesRegex(ArtifactError, "already exists"):
            native.render(source, preview)


if __name__ == "__main__":
    unittest.main()
