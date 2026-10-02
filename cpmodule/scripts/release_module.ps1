# 模块发版入口：semver 校验 → 干净树检查 → tag 查重 → 自动 bump Cargo.toml 并提交
# → 打 tag 推送触发 CI 矩阵构建（CI 从 tag 注入版本，构建产物与 tag 一致）。
# 用法：.\scripts\release_module.ps1 -Version 0.1.0
param(
    [Parameter(Mandatory = $true)]
    [string]$Version
)
$ErrorActionPreference = "Stop"

# 1. 版本格式
if ($Version -notmatch '^(?<major>\d+)\.(?<minor>\d+)\.(?<patch>\d+)(?:[-+][0-9A-Za-z.-]+)?$') {
    throw "Version must be semver, e.g. 1.2.3 or 1.2.3-beta.1"
}

# 2. 干净工作树（bump 提交必须基于已提交状态）
git diff --quiet; if ($LASTEXITCODE -ne 0) { throw "Unstaged changes. Commit or 'git stash -u' first." }
git diff --cached --quiet; if ($LASTEXITCODE -ne 0) { throw "Staged but uncommitted changes. Commit first." }

# 3. tag 查重（本地 + 远端）
$tagName = "module-v$Version"
if (git tag -l $tagName) { throw "Tag '$tagName' already exists locally." }
$remoteTag = git ls-remote --tags origin "refs/tags/$tagName"
if ($LASTEXITCODE -ne 0) { throw "Could not reach 'origin' to check for existing tags." }
if ($remoteTag) { throw "Tag '$tagName' already exists on origin." }

# 4. 自动 bump cpmodule/Cargo.toml 并提交（tag = 版本唯一来源，toml 跟随）
python cpmodule/scripts/set_version.py $Version
if ($LASTEXITCODE -ne 0) { throw "set_version.py failed." }
$branch = git branch --show-current
git add cpmodule/Cargo.toml
$diff = git diff --cached --quiet; if ($LASTEXITCODE -eq 0) {
    Write-Host "cpmodule/Cargo.toml already at $Version; skipping bump commit."
} else {
    git commit -m "chore(release): cp-spotify-module $Version"
    if ($LASTEXITCODE -ne 0) { throw "Failed to commit version bump." }
    git push origin HEAD:main
    if ($LASTEXITCODE -ne 0) { throw "Failed to push bump commit." }
}

# 5. 打 tag 推送 → GitHub Actions matrix 构建并发布 Release
git tag -a $tagName -m "cp-spotify-module $Version"
if ($LASTEXITCODE -ne 0) { throw "Failed to create tag." }
git push origin $tagName
if ($LASTEXITCODE -ne 0) { throw "Failed to push tag." }

Write-Host "Published $tagName. GitHub Actions will build all platforms and upload the release." -ForegroundColor Green
