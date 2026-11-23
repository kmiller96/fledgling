import re
import sys
import tomllib
from pathlib import Path

path = Path("Cargo.toml")
manifest = path.read_text()
current = tomllib.loads(manifest)["package"]["version"]
if not re.fullmatch(r"(0|[1-9][0-9]*)(\.(0|[1-9][0-9]*)){2}", current):
    sys.exit(f"Expected a stable version, got {current!r}")

parts = list(map(int, current.split(".")))
index = ("major", "minor", "patch").index(sys.argv[1])
parts[index:] = [parts[index] + 1] + [0] * (2 - index)
new = ".".join(map(str, parts))

updated, count = re.subn(
    rf'^version = "{re.escape(current)}"$', f'version = "{new}"', manifest, flags=re.M
)
if count != 1 or tomllib.loads(updated)["package"]["version"] != new:
    sys.exit("Could not update the package version in Cargo.toml")

path.write_text(updated)
print(current, new)
