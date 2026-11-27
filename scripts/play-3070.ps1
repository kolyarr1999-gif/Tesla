param(
    [Parameter(Mandatory)][string]$Game,
    [ValidateSet('series-s', 'series-x')][string]$Console = 'series-s',
    [switch]$FullHD,
    [ValidateRange(0, 3)][int]$MipDrop = 1,
    [int]$CardBuffersMB = 1024,
    [int]$CardWindowsMB = 512,
    [ValidateRange(0, 4096)][int]$SendSpareMB = 768,
    [ValidateRange(1, 128)][int]$JoinLists = 48,
    [ValidateSet('Quality', 'Off')][string]$Dlss = 'Off',
    [switch]$Hooks,
    [switch]$ProbeRays,
    [switch]$RayTracing,
    [switch]$FrameGeneration,
    [switch]$OriginalWaterLight,
    [switch]$OriginalScheduler,
    [switch]$OriginalSceneLoading,
    [switch]$PrepareCutscenes,
    [switch]$WarmOnly,
    [switch]$Prewarm
)
$ErrorActionPreference = 'Stop'
if ($FullHD) { $Console = 'series-s' }
$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root 'target\release\victory.exe'
if (-not (Test-Path -LiteralPath $exe)) {
    $exe = Join-Path $root 'victory.exe'
}
$sceneExe = Join-Path $root 'target\release\victory-scene.exe'
if (!$OriginalSceneLoading -and (Test-Path -LiteralPath $sceneExe -PathType Leaf)) {
    $exe = $sceneExe
}
$dlssExe = Join-Path $root 'target\release\victory-dlss.exe'
if ($Dlss -eq 'Quality' -and $Console -eq 'series-s' -and (Test-Path -LiteralPath $dlssExe -PathType Leaf)) {
    $exe = $dlssExe
}
if (-not (Test-Path -LiteralPath $exe)) {
    throw "Release binary not found: $exe"
}
if (-not (Test-Path -LiteralPath $Game)) {
    throw "Game not found: $Game"
}
if (Get-Process victory,victory-dlss,victory-scene -ErrorAction SilentlyContinue) {
    Write-Host "victory.exe is already running." -ForegroundColor Yellow
    exit 1
}

$streamlineDir = $env:VICTORY_STREAMLINE_DIR
# Clear inherited VICTORY_* except path overrides (keeps default save & cache)
$keep = 'VICTORY_CONFIG', 'VICTORY_GAME_DIR', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR'
Get-ChildItem Env:VICTORY_* -ErrorAction SilentlyContinue | Where-Object { $_.Name -notin $keep } | ForEach-Object { Remove-Item "Env:$($_.Name)" }

$gameDir = Split-Path -Parent (Resolve-Path -LiteralPath $Game)
$set = [ordered]@{
    VICTORY_GAME_DIR             = $gameDir
    VICTORY_PIPELINE_BOOK        = '1'
    VICTORY_LOG                  = 'info'
    VICTORY_FRAME_INTERVAL_MS    = '16.667'

    # RTX 3070 8GB VRAM budget + Scarlett page pool (-1.2 GB texture VRAM)
    VICTORY_DYNAMIC_SLOTS        = '524288'
    VICTORY_MAX_PRELUDES         = '32'
    VICTORY_CARD_BUFFERS_MB      = "$CardBuffersMB"
    VICTORY_CARD_WINDOWS_MB      = "$CardWindowsMB"
    VICTORY_SEND_SPARE_MB        = "$SendSpareMB"
    VICTORY_TEXTURE_MIP_DROP     = "$MipDrop"
    VICTORY_TEXTURE_ROOM_PERCENT = '94'
    VICTORY_PAGE_POOL            = '1'
    VICTORY_STAGING_PACK         = '1'
    VICTORY_LEVELS_ON_DEMAND     = '0'

    # Scarlett CPU submit & upload wins (+ 3050 Nsight A/B tweaks)
    VICTORY_WAIT_RESIDENT        = '0'
    VICTORY_JOIN_COPIES          = '0'
    VICTORY_REFRESH_ONCE         = '1'
    VICTORY_CARD_REHASH_RUNS     = '1'
    VICTORY_BACK_OWN_VIEW        = '1'
    VICTORY_WIDE_MERGE           = '1'
    VICTORY_MIRROR_FAST          = '1'
    VICTORY_PIPELINE_WAIT_MS     = '20'
    VICTORY_ASYNC_SUBMIT         = '1'
    VICTORY_QUEUE2               = '1'
    VICTORY_INDIRECT_WINDOW      = '1'
    VICTORY_TEXTURE_NO_WAIT      = '1'
    VICTORY_COPY_PLAIN           = '1'
    VICTORY_WIDE_TEXTURES        = '1'
    VICTORY_INDIRECT_GATHER      = '1'
    VICTORY_LATE_WAIT_LISTS      = '1024'
    VICTORY_CARD_FILLS_BATCH     = '0'
    VICTORY_GATHER_DISPATCH      = '0'
    VICTORY_RELAY                = '2'
    VICTORY_WIDE_BACK            = '1'
    VICTORY_RELAY_COPY           = '1'
    VICTORY_CARD_STEPS           = '2'
    VICTORY_INDIRECT_BATCH       = '2'
    VICTORY_MAP_THREADS          = '1'
    VICTORY_CARD_AWAKE           = '0'
    VICTORY_SHOW_AFTER           = '1'
    VICTORY_JOIN_PARTS           = '8'
    VICTORY_JOIN_LISTS           = "$JoinLists"
    VICTORY_UPLOAD_AHEAD         = '1'
    VICTORY_CARD_BUFFERS_MIN_KB  = '64'
    VICTORY_POOL_DISCARD         = '1'
    VICTORY_FILL_TITLE           = '1'
    VICTORY_PARTIAL_DELTA        = '1'
    VICTORY_BAKE_DELTA           = '1'
    VICTORY_MAP_LOCK_FINE        = '1'
    VICTORY_POOL_CAP             = '1'
    VICTORY_TABLE_MEMO           = '1'
    VICTORY_LIST_ALIAS_EXACT     = '1'
    VICTORY_LIST_HOLD            = '1'
    VICTORY_EARLY_TEXTURES_MS    = '40'
    VICTORY_BAKE_TEXTURES_MS     = '10000'
    VICTORY_HEAP_PASS_MAX        = '0'
    VICTORY_HEAP_CHANGED_MAX     = '0'
    VICTORY_LEAVE_OUT            = '02384a60fda6f2d0'
    VICTORY_WC_NO_BACK           = '1'
    VICTORY_WC_NO_BACK_MIN_KB    = '1024'
    VICTORY_SYNC_IN_PLACE        = '1'
    VICTORY_SPANS_QUICK          = '1'
    VICTORY_SYNC_FINE            = '1'
    VICTORY_HEAP_SYNC_FIRST      = '1'
    VICTORY_HEAP_SYNC_AT_SUBMIT  = '1'
    VICTORY_SOFT_VIEWS           = 'copies'
    VICTORY_SAMPLERS_MADE        = '1'
    VICTORY_TABLE_NAMED_ONLY     = '1'
    VICTORY_QUEUE_MARKS          = '1'
    VICTORY_PORT_CONSTANTS       = '1'
    VICTORY_EARLY_DEPTH          = '1'
}
# Work around dark water tiles on NVIDIA, after scarlet-fps and VIctory-NVIDIA 35c1560.
# The unmodified two tiled light passes can be restored for visual comparison.
if (!$OriginalWaterLight) {
    $set.VICTORY_LEAVE_OUT += ',5dc3b25986622949,6f225b641c5b0708'
    Write-Host 'Water: dark-tile workaround enabled'
}
if ($Dlss -eq 'Quality') {
    if ($Console -eq 'series-s') {
        if (!$streamlineDir -or !(Test-Path -LiteralPath (Join-Path $streamlineDir 'sl.interposer.dll') -PathType Leaf)) {
            throw 'Point VICTORY_STREAMLINE_DIR at the bin\x64 folder of NVIDIA Streamline SDK.'
        }
        $set.VICTORY_DLSS = '1'
        $set.VICTORY_STREAMLINE_DIR = $streamlineDir
        Write-Host 'Upscaling requested: DLSS Quality'
    } else {
        Write-Host 'DLSS Quality is available on the Series S profile. Using the title upscaler.' -ForegroundColor Yellow
    }
}

if ($FrameGeneration) { Write-Warning 'Frame generation is disabled for native FPS testing.' }
if ($Hooks) {
    $set.VICTORY_GPU_ONLY = '1'
    $set.VICTORY_KNOWN_WRITES = '1'
}
if ($ProbeRays -and -not $RayTracing) {
    $set.VICTORY_PROBE_RAYS = '1'
    $set.VICTORY_PLANE_NOW = '1'
} elseif (-not $RayTracing) {
    $set.VICTORY_AMBIENT = '0.8'
    $set.VICTORY_AMBIENT_AIR = '1.5'
}
if ($RayTracing) { $set.VICTORY_BVH_BUILDER = '1' }

# Keep the submitter's helpers ahead of background work and save the
# pipeline library on its own thread at most once every three minutes.
if (!$OriginalScheduler) {
    $set.VICTORY_HELPER_PRIORITY = '1'
    $set.VICTORY_LIBRARY_SAVE_S = '180'
}

if ($OriginalSceneLoading) { $set.VICTORY_REMAP_RESCAN = '0' }

foreach ($k in $set.Keys) { Set-Item -Path "Env:$k" -Value $set[$k] }

if ($Prewarm) {
    Write-Host 'Prewarming shaders and pipelines...' -ForegroundColor Cyan
    & $exe shaders $gameDir
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
if ($WarmOnly -or $PrepareCutscenes) {
    Write-Host 'Warming pipelines requested in previous runs...' -ForegroundColor Cyan
    & $exe warm --used
    if ($WarmOnly -or $LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
} elseif ($Prewarm) {
    & $exe warm
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}

$runsDir = Join-Path $root 'runs'
New-Item -ItemType Directory -Force -Path $runsDir | Out-Null
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$logPath = Join-Path $runsDir "3070-$stamp.log"

Write-Host "=== victory-scarlett RTX 3070 8GB (release, $Console, MipDrop=$MipDrop, PagePool=1, SendSpareMB=$SendSpareMB, JoinLists=$JoinLists) ===" -ForegroundColor Green
if ($Console -eq 'series-s') {
    $upscaler = if ($Dlss -eq 'Quality') { 'DLSS Quality requested' } else { 'title upscaling' }
    Write-Host "Series S output: 1920x1080 ($upscaler)"
}
Write-Host "Log: $logPath"
$ErrorActionPreference = 'Continue'
& $exe run $Game --execute --console $Console 2>&1 | Tee-Object -FilePath $logPath
exit $LASTEXITCODE
