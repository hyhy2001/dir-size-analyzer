#!/usr/bin/env python3
from __future__ import annotations

import argparse
import importlib.util
import json
import os
import shutil
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


MIN_PARTITION_KB = 500_000

try:
    import fast_scanner  # type: ignore
except Exception:
    fast_scanner = None


def _load_local_fast_scanner() -> object | None:
    script_dir = Path(__file__).resolve().parent
    local_so = script_dir / "fast_scanner.abi3.so"
    if not local_so.exists():
        local_so = script_dir / "lib" / "fast_scanner.abi3.so"
    if not local_so.exists():
        return None

    try:
        spec = importlib.util.spec_from_file_location("fast_scanner", str(local_so))
        if spec is None or spec.loader is None:
            return None
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module
    except Exception:
        return None


_local_fast_scanner = _load_local_fast_scanner()
if _local_fast_scanner is not None:
    fast_scanner = _local_fast_scanner


@dataclass(frozen=True)
class InputBlock:
    server: str
    rows: list[tuple[str, str]]


def parse_input(path: Path) -> list[InputBlock]:
    blocks: list[InputBlock] = []
    server = "UNKNOWN"
    rows: list[tuple[str, str]] = []

    with path.open(encoding="utf-8") as handle:
        for raw_line in handle:
            line = raw_line.strip()
            if not line or line[0] in "#+-":
                continue

            parts = line.split()
            if not parts:
                continue

            keyword = parts[0]
            if keyword == "SERVER":
                server = parts[1] if len(parts) > 1 else "UNKNOWN"
            elif keyword == "END_CHECK":
                if rows:
                    blocks.append(InputBlock(server=server, rows=rows))
                server = "UNKNOWN"
                rows = []
            elif len(parts) >= 2:
                rows.append((parts[0], parts[1]))

    if rows:
        blocks.append(InputBlock(server=server, rows=rows))
    return blocks


def df_info(path: str) -> tuple[str, str, str, str]:
    usage = shutil.disk_usage(path)
    mount = _mount_point(path)
    return (
        mount,
        str(usage.total // 1024),
        str(usage.used // 1024),
        str(usage.free // 1024),
    )


def _mount_point(path: str) -> str:
    current = Path(path).resolve()
    while not os.path.ismount(current):
        parent = current.parent
        if parent == current:
            break
        current = parent
    return str(current)


def du_kb(path: str, workers: int | None = None) -> tuple[int, int, int]:
    if fast_scanner is not None and hasattr(fast_scanner, "scan_dir_info"):
        kb, skipped_perm, skipped_cross_dev = fast_scanner.scan_dir_info(path, workers)
        return int(kb), int(skipped_perm), int(skipped_cross_dev)

    raise RuntimeError(
        "fast_scanner.scan_dir_info is required. "
        "Build it first with: cd rust_scanner && ./build.sh 2.17"
    )


def build_payload(block: InputBlock, timestamp: int, workers: int | None) -> list[dict]:
    projects: dict[str, dict] = {}

    for project, folder in block.rows:
        if not os.path.isdir(folder):
            print(f"\t\tFolder not exist {folder}")
            continue

        project_data = projects.setdefault(
            project,
            {"Project": project, "Date": str(timestamp), "Hard_disk": {}, "Partition": []},
        )

        disk_name, disk_size, disk_used, disk_available = df_info(folder)
        project_data["Hard_disk"].setdefault(
            disk_name,
            {
                "Name": disk_name,
                "Size": disk_size,
                "Used": disk_used,
                "Available": disk_available,
            },
        )

        used, _, _ = du_kb(folder, workers)
        used_human = _format_kb(used)
        if used > MIN_PARTITION_KB:
            project_data["Partition"].append(
                {"Folder": folder, "Used": str(used), "Hard_disk": disk_name}
            )
        print(f"-Project: {project:<12} Folder: {folder:<40} Size: {used_human}")

    output = []
    for project_data in projects.values():
        output.append(
            {
                "Project": project_data["Project"],
                "Date": project_data["Date"],
                "Hard_disk": list(project_data["Hard_disk"].values()),
                "Partition": project_data["Partition"],
            }
        )
    return output


def iter_input_files(args: Iterable[str], setting_dir: Path) -> list[Path]:
    files = [Path(item) for item in args]
    if not files:
        files = sorted(setting_dir.glob("*.hard_disk.info"))
    return [path for path in files if path.exists() and path.is_file()]


def parse_duration(seconds: int) -> str:
    hours = seconds // 3600
    minutes = (seconds // 60) % 60
    secs = seconds % 60
    return f"{hours:02d}:{minutes:02d}:{secs:02d}"


def _format_kb(kb: int) -> str:
    if kb >= 1024 * 1024:
        return f"{kb / (1024 * 1024):.2f} TB"
    if kb >= 1024:
        return f"{kb / 1024:.2f} GB"
    return f"{kb} KB"


def main() -> int:
    parser = argparse.ArgumentParser(description="Directory size analyzer")
    parser.add_argument("inputs", nargs="*", help="*.hard_disk.info input files")
    parser.add_argument("--output-dir", default="INFO")
    parser.add_argument("--setting-dir", default="SETTING")
    parser.add_argument("--workers", type=int, default=None, help="parallel scanner workers")
    args = parser.parse_args()

    script_dir = Path(__file__).resolve().parent
    output_dir = (script_dir / args.output_dir).resolve()
    setting_dir = (script_dir / args.setting_dir).resolve()
    output_dir.mkdir(parents=True, exist_ok=True)

    start = int(time.time())
    input_files = iter_input_files(args.inputs, setting_dir)
    if not input_files:
        print("ERROR: Do not find any hard disk info !!!")
        return 1

    print("##########################")
    print("# Collect server info (python/rust)")
    print("##########################")

    for input_file in input_files:
        for block in parse_input(input_file):
            payload = build_payload(block, start, args.workers)
            output_file = output_dir / f"Hard_disk_info_{block.server}.json"
            output_file.write_text(json.dumps(payload, indent=3, sort_keys=True) + "\n")

    print(f"Job took {parse_duration(int(time.time()) - start)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
