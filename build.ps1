param(
    [switch]$debug,
    [switch]$clean,
    [switch]$check,
    [switch]$test,
    [switch]$h,
    [switch]$help
)

if ($h -or $help) {
    Write-Host "deveco-local-model-dashboard 构建脚本" -ForegroundColor Cyan
    Write-Host ""
    Write-Host "用法: .\build.ps1 [参数]"
    Write-Host ""
    Write-Host "参数:"
    Write-Host "  (无参数)      release 编译（默认）"
    Write-Host "  -debug        debug 编译（保留调试符号）"
    Write-Host "  -clean        清除 target/ 目录"
    Write-Host "  -check        编译前先跑 cargo check"
    Write-Host "  -test         运行 cargo test"
    Write-Host "  -h, -help     显示此帮助信息"
    Write-Host ""
    Write-Host "组合示例:"
    Write-Host "  .\build.ps1                    # release 编译"
    Write-Host "  .\build.ps1 -debug             # debug 编译"
    Write-Host "  .\build.ps1 -clean -check      # 清除后 check + 编译"
    exit 0
}

$ErrorActionPreference = "Stop"

$repo = Split-Path -Parent $MyInvocation.MyCommand.Path

# ── 查找隔离 Rust 工具链（来自同级的 atomcode-deveco 项目） ──
$toolchainDir = Join-Path $repo "..\atomcode-deveco"
$rustupHome = Join-Path $toolchainDir ".rustup"
$cargoHome  = Join-Path $toolchainDir ".cargo"

if (Test-Path $rustupHome) {
    $env:RUSTUP_HOME = $rustupHome
    $env:CARGO_HOME  = $cargoHome
    $env:PATH = "$cargoHome\bin;$env:PATH"
    Write-Host "使用隔离 Rust 工具链: $cargoHome" -ForegroundColor Gray
} else {
    Write-Host "未找到隔离工具链，使用系统 Rust" -ForegroundColor Gray
}

Set-Location $repo

# 默认 release；-debug 进入 debug 模式
$profileName = if ($debug) { "debug" } else { "release" }
$targetDir   = Join-Path $repo "target"
$profileDir  = Join-Path $targetDir $profileName
$binName     = if ($IsWindows) { "deveco-local-model-dashboard.exe" } else { "deveco-local-model-dashboard" }
$binPath     = Join-Path $profileDir $binName

# ── 清除 ──
if ($clean) {
    Write-Host "==> 清除编译产物 (target/)" -ForegroundColor Cyan
    if (Test-Path $targetDir) {
        Remove-Item -Recurse -Force $targetDir
        Write-Host "    已删除 $targetDir" -ForegroundColor Green
    } else {
        Write-Host "    target/ 不存在，无需清除" -ForegroundColor Gray
    }
    $outputDir = Join-Path $repo "output"
    if (Test-Path $outputDir) {
        Remove-Item -Recurse -Force $outputDir
        Write-Host "    已删除 $outputDir" -ForegroundColor Green
    }
    # 如果只有 -clean，清除后直接退出
    if (-not $check -and -not $test) {
        Write-Host "==> 清除完成" -ForegroundColor Green
        exit 0
    }
}

# ── 测试 ──
if ($test) {
    Write-Host "==> cargo test" -ForegroundColor Cyan
    & cargo test
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    if (-not $check) { exit 0 }
}

# ── 检查 ──
if ($check) {
    Write-Host "==> cargo check" -ForegroundColor Cyan
    & cargo check
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}

# ── 编译 ──
$buildArgs = @("build")
if (-not $debug) { $buildArgs += "--release" }
Write-Host ("==> cargo {0}" -f ($buildArgs -join " ")) -ForegroundColor Cyan
if ($debug) {
    Write-Host "    [debug 模式] 保留调试符号，编译速度更快，体积更大" -ForegroundColor Yellow
} else {
    Write-Host "    [release 模式] 优化体积与性能（默认）" -ForegroundColor Gray
}
& cargo @buildArgs
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host ""
Write-Host "==> 编译产物：" -ForegroundColor Green
if (Test-Path $binPath) {
    $bin = Get-Item $binPath
    Write-Host ("    {0}  ({1:N1} MB)" -f $bin.FullName, ($bin.Length / 1MB))
}

# ── 复制到 output/ 目录 ──
$outputDir = Join-Path $repo "output"
if (-not (Test-Path $outputDir)) {
    New-Item -ItemType Directory -Path $outputDir -Force | Out-Null
}
$outputPath = Join-Path $outputDir $binName
Copy-Item $binPath $outputPath -Force
Write-Host "    已复制到 $outputPath" -ForegroundColor Gray

Write-Host ""
Write-Host "==> 完成" -ForegroundColor Green
$extendDir = Join-Path $toolchainDir "extend"
Write-Host "    如需打包进 atomcode-deveco，复制到 $extendDir 后运行 atomcode-deveco\build.ps1 -pack" -ForegroundColor Gray
