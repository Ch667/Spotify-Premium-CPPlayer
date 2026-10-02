#!/usr/bin/env python3
"""把 semver 版本写入 cpmodule/Cargo.toml（tag → toml 单向同步）。

CI（tag module-v* 触发）：构建前注入版本，产物与 tag 一致。
本地（release_module.ps1）：打 tag 前自动 bump 并提交。
用法：python3 set_version.py 1.2.3[-prerelease]
"""
import re
import sys
from pathlib import Path

CARGO_TOML = Path(__file__).resolve().parent.parent / "Cargo.toml"
SEMVER = re.compile(r"^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$")


def main() -> None:
    if len(sys.argv) != 2 or not SEMVER.match(sys.argv[1]):
        sys.exit(f"usage: {sys.argv[0]} <semver>  (e.g. 1.2.3 or 1.2.3-beta.1)")
    version = sys.argv[1]

    text = CARGO_TOML.read_text(encoding="utf-8")
    # 只动 [package] 段的第一处 version = "..."（本文件无依赖段 version 字段干扰：
    # 依赖版本都写作 { version = ... } 或在 dependencies 段内以缩进出现）。
    new_text, n = re.subn(
        r'(?m)^(version\s*=\s*)"[^"]*"',
        lambda m: f'{m.group(1)}"{version}"',
        text,
        count=1,
    )
    if n != 1:
        sys.exit("failed to locate package version line in Cargo.toml")
    if new_text != text:
        CARGO_TOML.write_text(new_text, encoding="utf-8")
        print(f"cpmodule/Cargo.toml -> {version}")
    else:
        print(f"cpmodule/Cargo.toml already {version}")


if __name__ == "__main__":
    main()
