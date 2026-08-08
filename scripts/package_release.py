#!/usr/bin/env python3
"""Package the cross-platform ROMX binaries and runtime locale files."""

from __future__ import annotations

import argparse
import hashlib
import shutil
import tarfile
import zipfile
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True, help="Rust target triple")
    parser.add_argument("--name", required=True, help="Package name")
    parser.add_argument(
        "--archive", choices=("zip", "tar.gz"), required=True, help="Archive format"
    )
    parser.add_argument(
        "--windows", action="store_true", help="Use .exe binary names"
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    root = Path(__file__).resolve().parents[1]
    release_dir = root / "target" / args.target / "release"
    package_dir = root / "dist" / args.name
    archive_path = root / "dist" / (
        f"{args.name}.zip" if args.archive == "zip" else f"{args.name}.tar.gz"
    )

    if package_dir.exists():
        shutil.rmtree(package_dir)
    package_dir.mkdir(parents=True)
    archive_path.unlink(missing_ok=True)

    suffix = ".exe" if args.windows else ""
    for binary in ("romx-gui", "romx"):
        source = release_dir / f"{binary}{suffix}"
        if not source.is_file():
            raise FileNotFoundError(f"release binary not found: {source}")
        shutil.copy2(source, package_dir / source.name)

    shutil.copytree(
        root / "crates" / "romx-gui" / "locales", package_dir / "locales"
    )
    readme = root / "README.md"
    if readme.is_file():
        shutil.copy2(readme, package_dir / "README.md")

    if args.archive == "zip":
        with zipfile.ZipFile(
            archive_path, "w", compression=zipfile.ZIP_DEFLATED
        ) as archive:
            for path in package_dir.rglob("*"):
                if path.is_file():
                    archive.write(path, path.relative_to(package_dir.parent))
    else:
        with tarfile.open(archive_path, "w:gz") as archive:
            archive.add(package_dir, arcname=package_dir.name)

    digest = hashlib.sha256(archive_path.read_bytes()).hexdigest()
    checksum_path = Path(f"{archive_path}.sha256")
    checksum_path.write_text(f"{digest}  {archive_path.name}\n", encoding="utf-8")
    print(archive_path)


if __name__ == "__main__":
    main()
