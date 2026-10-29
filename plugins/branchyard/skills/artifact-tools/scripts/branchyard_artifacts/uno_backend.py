"""System-Python UNO subprocess. Invoked only by the explicit native pivot operation."""

from __future__ import annotations

import json
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any


def run(payload: dict[str, Any]) -> None:
    """Use native DataPilot APIs and store a real XLSX pivot table and cache."""
    import uno

    work = Path(payload["work"])
    pipe = "branchyard_" + work.name.replace("-", "_")
    process = subprocess.Popen(
        [
            payload["office"],
            "-env:UserInstallation=" + (work / "profile").as_uri(),
            "--headless",
            "--norestore",
            "--nodefault",
            "--accept=pipe,name=" + pipe + ";urp;StarOffice.ComponentContext",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    document = None
    try:
        local = uno.getComponentContext()
        resolver = local.ServiceManager.createInstanceWithContext("com.sun.star.bridge.UnoUrlResolver", local)
        deadline = time.monotonic() + 20
        while True:
            try:
                context = resolver.resolve("uno:pipe,name=" + pipe + ";urp;StarOffice.ComponentContext")
                break
            except uno.getClass("com.sun.star.connection.NoConnectException"):
                if process.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("LibreOffice UNO did not become ready") from None
                threading.Event().wait(0.05)
        desktop = context.ServiceManager.createInstanceWithContext("com.sun.star.frame.Desktop", context)

        def property_value(name: str, value: Any) -> Any:
            prop = uno.createUnoStruct("com.sun.star.beans.PropertyValue")
            prop.Name, prop.Value = name, value
            return prop

        document = desktop.loadComponentFromURL(
            uno.systemPathToFileUrl(payload["input"]),
            "_blank",
            0,
            (property_value("Hidden", True), property_value("ReadOnly", False)),
        )
        if document is None:
            raise RuntimeError("LibreOffice could not open the source workbook")
        spec = payload["spec"]
        sheets = document.getSheets()
        source = sheets.getByName(spec["source_sheet"])
        if not sheets.hasByName(spec["target_sheet"]):
            sheets.insertNewByName(spec["target_sheet"], sheets.getCount())
        target = sheets.getByName(spec["target_sheet"])
        tables = target.getDataPilotTables()
        descriptor = tables.createDataPilotDescriptor()
        descriptor.SourceRange = source.getCellRangeByName(spec["source_range"]).getRangeAddress()
        fields = descriptor.getDataPilotFields()
        assigned = set()
        for orientation, names in [("ROW", spec["rows"]), ("COLUMN", spec["columns"])]:
            for name in names:
                if name in assigned:
                    raise ValueError("Pivot field assigned twice: " + name)
                assigned.add(name)
                fields.getByName(name).Orientation = uno.Enum(
                    "com.sun.star.sheet.DataPilotFieldOrientation", orientation
                )
        for field in spec["data"]:
            if field["field"] in assigned:
                raise ValueError("Pivot data field assigned twice: " + field["field"])
            assigned.add(field["field"])
            item = fields.getByName(field["field"])
            item.Orientation = uno.Enum("com.sun.star.sheet.DataPilotFieldOrientation", "DATA")
            item.Function = uno.Enum("com.sun.star.sheet.GeneralFunction", field["function"])
        tables.insertNewByName(
            spec["name"], target.getCellRangeByName(spec["target_cell"]).getCellAddress(), descriptor
        )
        document.calculateAll()
        document.storeAsURL(
            uno.systemPathToFileUrl(payload["output"]),
            (property_value("FilterName", "Calc MS Excel 2007 XML"), property_value("Overwrite", True)),
        )
    finally:
        if document is not None:
            document.close(True)
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


if __name__ == "__main__":
    run(json.loads(Path(sys.argv[1]).read_text()))
