#!/usr/bin/env python3
"""CPPlayer 音源模块打包：staging 目录 + manifest.json → 可导入 zip（含 version.json）。

用法示例（Android，staging 内已有 lib/<abi>/libcp_spotify.so）：
    python pack_module.py --platform android --version 0.1.0 \
        --stage target/cp_stage --repo owner/repo

桌面端 staging 根下放 cp_spotify.dll / libcp_spotify.dylib / libcp_spotify.so。
产物：<out>/cp-spotify-module-<platform>-<version>.zip 与 version-<platform>.json
"""

import argparse
import json
import zipfile
from pathlib import Path

# 安装页展示标签（宿主 1.9.x 按 `ProviderCapability.sdkLabels` 映射；
# 运行期能力以 `MusicProvider.capabilities()` 为准，这里只是展示）。
CAPABILITIES = [
    "search",
    "playback",
    "albumExplorer",
    "artistExplorer",
]

ENTRY = {
    "android": "libcp_spotify.so",
    "windows": "cp_spotify.dll",
    "macos": "libcp_spotify.dylib",
    "linux": "libcp_spotify.so",
}

DEFAULT_ABI = {
    "android": ["arm64-v8a", "armeabi-v7a", "x86_64"],
    "windows": ["x86_64"],
    "macos": ["aarch64"],
    "linux": ["x86_64"],
}


def build_manifest(platform: str, version: str, repo: str) -> dict:
    # kmp-sdk-v1：宿主 v1.8.8 起只认这个契约。`type=native` + `contract` 缺一不可，
    # 旧的 apiMap / loginMethods / targetAppPackage 已不再生效，不要再写。
    # id 与旧 JSON 版（cp.provider.spotify.v2）保持不同：旧包不能被新协议加载，
    # 用新 id 让两者在设备上互不覆盖，也方便一眼区分。
    return {
        "id": "cp.provider.spotify.sdk",
        "name": "Spotify (librespot)",
        "version": version,
        "type": "native",
        "contract": "kmp-sdk-v1",
        "entryPoint": ENTRY[platform],
        "supportedAbis": DEFAULT_ABI[platform],
        "capabilities": CAPABILITIES,
        "updateUrl": f"https://github.com/{repo}/releases/latest/download/version-{platform}.json",
    }


def zip_staging(stage: Path, manifest: dict, out: Path) -> Path:
    out.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as zf:
        for p in sorted(stage.rglob("*")):
            if p.is_file():
                zf.write(p, p.relative_to(stage).as_posix())
        zf.writestr("manifest.json", json.dumps(manifest, indent=2, ensure_ascii=False))
    return out


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--platform", required=True, choices=list(ENTRY))
    ap.add_argument("--version", required=True)
    ap.add_argument("--stage", required=True, type=Path)
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--repo", default="librespot-org/librespot", help="owner/repo 用于 updateUrl")
    args = ap.parse_args()

    stage: Path = args.stage
    if not stage.is_dir():
        raise SystemExit(f"staging dir not found: {stage}")

    manifest = build_manifest(args.platform, args.version, args.repo)

    # 校验产物确实存在。
    # 宿主按 File(root, entryPoint) 解析入口，要求它是包内真实文件；Android 的库在
    # lib/<abi>/ 下，所以 entryPoint 必须写成带路径的形式（NATIVE.md §构建与验证），
    # 光写文件名会在宿主里报 "entryPoint is missing"。宿主随后按设备 ABI 选 lib/<abi>/。
    if args.platform == "android":
        lib_dir = stage / "lib"
        abis = sorted(d.name for d in lib_dir.glob("*") if (d / ENTRY["android"]).is_file()) if lib_dir.is_dir() else []
        if not abis:
            raise SystemExit(f"no {ENTRY['android']} under {stage}/lib/<abi>/")
        manifest["supportedAbis"] = abis
        # 优先 arm64（现代手机），否则取第一个存在的。
        primary = "arm64-v8a" if "arm64-v8a" in abis else abis[0]
        manifest["entryPoint"] = f"lib/{primary}/{ENTRY['android']}"
    else:
        entry = stage / manifest["entryPoint"]
        if not entry.is_file():
            raise SystemExit(f"missing native lib: {entry}")

    out_zip = zip_staging(stage, manifest, args.out / f"cp-spotify-module-{args.platform}-{args.version}.zip")

    version_json = {
        "version": args.version,
        "downloadUrl": manifest["updateUrl"].replace(
            f"version-{args.platform}.json",
            out_zip.name,
        ),
        "changelog": f"cp-spotify-module {args.version}",
    }
    vj = args.out / f"version-{args.platform}.json"
    vj.write_text(json.dumps(version_json, indent=2), encoding="utf-8")

    print(f"packed {out_zip}")
    print(f"version file {vj}")


if __name__ == "__main__":
    main()
