#!/usr/bin/env python3
"""Package the cross-platform ROMX GUI release artifacts."""

from __future__ import annotations

import argparse
import hashlib
import re
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
        "--platform",
        choices=("macos", "linux", "windows"),
        required=True,
        help="Release platform layout",
    )
    return parser.parse_args()


def workspace_version(root: Path) -> str:
    in_workspace_package = False
    for line in (root / "Cargo.toml").read_text(encoding="utf-8").splitlines():
        section = line.strip()
        if section.startswith("["):
            in_workspace_package = section == "[workspace.package]"
            continue
        if in_workspace_package:
            match = re.match(r'^version\s*=\s*["\']([^"\']+)["\']\s*$', line.strip())
            if match:
                return match.group(1)
    raise RuntimeError("workspace package version is missing from Cargo.toml")


def copy_locales(root: Path, destination: Path) -> None:
    shutil.copytree(root / "crates" / "romx-gui" / "locales", destination)


def write_macos_bundle(
    root: Path, release_dir: Path, package_dir: Path, target: str
) -> None:
    """Create a standard macOS application bundle with bundled locales."""

    app_dir = package_dir / "romx-gui.app"
    contents_dir = app_dir / "Contents"
    macos_dir = contents_dir / "MacOS"
    resources_dir = contents_dir / "Resources"
    macos_dir.mkdir(parents=True)
    resources_dir.mkdir()

    executable = release_dir / "romx-gui"
    if not executable.is_file():
        raise FileNotFoundError(f"release binary not found: {executable}")
    shutil.copy2(executable, macos_dir / "romx-gui")
    (macos_dir / "romx-gui").chmod(0o755)
    copy_locales(root, resources_dir / "locales")

    version = workspace_version(root)
    minimum_system_version = "11.0" if target.startswith("aarch64-") else "10.15"
    (contents_dir / "Info.plist").write_text(
        """<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\"
  \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
  <key>CFBundleDisplayName</key>
  <string>ROMX</string>
  <key>CFBundleExecutable</key>
  <string>romx-gui</string>
  <key>CFBundleIdentifier</key>
  <string>org.romx.gui</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>ROMX</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>VERSION_PLACEHOLDER</string>
  <key>CFBundleVersion</key>
  <string>VERSION_PLACEHOLDER</string>
  <key>LSMinimumSystemVersion</key>
  <string>MINIMUM_SYSTEM_VERSION</string>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
""".replace("VERSION_PLACEHOLDER", version)
        .replace("MINIMUM_SYSTEM_VERSION", minimum_system_version),
        encoding="utf-8",
    )


def copy_single_binary(
    root: Path, release_dir: Path, package_dir: Path, platform: str
) -> None:
    suffix = ".exe" if platform == "windows" else ""
    source = release_dir / f"romx-gui{suffix}"
    if not source.is_file():
        raise FileNotFoundError(f"release binary not found: {source}")
    destination = package_dir / source.name
    shutil.copy2(source, destination)
    if platform != "windows":
        destination.chmod(0o755)

    # The two built-in locales keep the binary self-contained. The external
    # directory is copied alongside it so users can add or replace languages
    # without rebuilding the application.
    copy_locales(root, package_dir / "locales")


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

    if args.platform == "macos":
        write_macos_bundle(root, release_dir, package_dir, args.target)
    else:
        copy_single_binary(root, release_dir, package_dir, args.platform)

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
