# 模块发版入口：semver 校验 → 干净树检查 → tag 查重 → 打 tag 触发 CI 矩阵构建。
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

# 2. 与 cpmodule/Cargo.toml 一致（版本唯一来源）
$cargoVersion = (Select-String -Path cpmodule/Cargo.toml -Pattern '^version\s*=\s*"(.+)"').Matches[0].Groups[1].Value
if ($cargoVersion -ne $Version) {
    throw "cpmodule/Cargo.toml version is '$cargoVersion'. Bump it to '$Version' first (version 唯一来源)。"
}

# 3. 干净工作树（构建产物必须可追溯到已提交状态）
git diff --quiet; if ($LASTEXITCODE -ne 0) { throw "Unstaged changes. Commit or 'git stash -u' first." }
git diff --cached --quiet; if ($LASTEXITCODE -ne 0) { throw "Staged but uncommitted changes. Commit first." }

# 4. tag 查重（本地 + 远端）
$tagName = "module-v$Version"
if (git tag -l $tagName) { throw "Tag '$tagName' already exists locally." }
$remoteTag = git ls-remote --tags origin "refs/tags/$tagName"
if ($LASTEXITCODE -ne 0) { throw "Could not reach 'origin' to check for existing tags." }
if ($remoteTag) { throw "Tag '$tagName' already exists on origin." }

# 5. 打 tag 推送 → GitHub Actions matrix 构建并发布 Release
git tag -a $tagName -m "cp-spotify-module $Version"
if ($LASTEXITCODE -ne 0) { throw "Failed to create tag." }
git push origin $tagName
if ($LASTEXITCODE -ne 0) { throw "Failed to push tag." }

Write-Host "Published $tagName. GitHub Actions will build all platforms and upload the release." -ForegroundColor Green
