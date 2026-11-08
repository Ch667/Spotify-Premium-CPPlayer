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

# 宿主内部方法名 → 本模块端点（Provider Standard v3 §3.2：未映射的方法按原名直传，
# 这里把实现过的都显式列出来，读 manifest 就能知道这个音源到底支持什么）。
API_MAP = {
    "cloudsearch": "search",
    "song/url/v1": "song/url",
    "song/url/v1/302": "song/url",
    "song/download/url/v1": "song/url",
    "song/detail": "song/detail",
    "lyric/new": "lyric",
    # 曲库浏览（v0.2.0 新增）
    "album": "album",
    "artist/detail": "artist/detail",
    "artist/songs": "artist/songs",
    "artist/top/song": "artist/songs",
    "artist/album": "artist/albums",
    "playlist/detail": "playlist/detail",
    "playlist/track/all": "playlist/track/all",
    # 账户（设备码授权 → 宿主 QR 通道）
    "login/qr/key": "login/qr/key",
    "login/qr/create": "login/qr/create",
    "login/qr/check": "login/qr/check",
    "login/status": "login/status",
    "logout": "logout",
}

# 能力网格：只声明真正实现的。声明后未列出的能力在 UI 上隐藏（指南 §4），
# 所以这里刻意不写 comments / cloudDrive / recommend —— Spotify 没有这三样，
# 留着入口只会点开一片空白。
CAPABILITIES = [
    "search",
    "playback",
    "albumExplorer",
    "artistExplorer",
    "qrLogin",
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
    zip_name = f"cp-spotify-module-{platform}-{version}.zip"
    return {
        "id": "cp.provider.spotify",
        "name": "Spotify (librespot)",
        "version": version,
        "type": "jni",
        "entryPoint": ENTRY[platform],
        "supportedAbis": DEFAULT_ABI[platform],
        "capabilities": CAPABILITIES,
        "apiMap": API_MAP,
        # 账户页登录渠道：设备码流程映射到宿主 QR 通道（扫码打开 spotify.com/pair）
        "loginMethods": ["qr"],
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

    # 校验产物确实存在
    if args.platform == "android":
        abis = [d.name for d in (stage / "lib").glob("*") if (d / manifest["entryPoint"]).is_file()] if (stage / "lib").is_dir() else []
        if not abis:
            raise SystemExit(f"no {manifest['entryPoint']} under {stage}/lib/<abi>/")
        manifest["supportedAbis"] = abis
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
