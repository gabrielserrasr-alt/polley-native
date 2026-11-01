# Gera o instalador do Polley nativo: native\release\Polley-Nativo-Setup-<versão>.exe
# e o release\latest.json assinado (atualização automática).
# Uso (na pasta native):  powershell -ExecutionPolicy Bypass -File installer\build.ps1 -Novidades "o que mudou"
# Depois, envie os DOIS arquivos para o bucket "releases" do Supabase (Storage).

param([string]$Novidades = "")

$ErrorActionPreference = "Stop"
$native = Split-Path $PSScriptRoot -Parent
Set-Location $native
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"

# 1. Compila o app (versão otimizada).
cargo build --release -p polley
if ($LASTEXITCODE -ne 0) { throw "a compilação falhou" }

# 2. Versão = a do polley\Cargo.toml.
$version = (Select-String -Path "polley\Cargo.toml" -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value

# 3. NSIS: o instalado no Windows ou o que o electron-builder já baixou para o Polley v1.
$makensis = @(
    "${env:ProgramFiles(x86)}\NSIS\makensis.exe",
    "$env:ProgramFiles\NSIS\makensis.exe"
) + (Get-ChildItem "$env:LOCALAPPDATA\electron-builder\Cache\nsis-*\*\makensis.exe" -ErrorAction SilentlyContinue | ForEach-Object FullName) |
    Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $makensis) { throw "NSIS não encontrado (instale em https://nsis.sourceforge.io)" }

New-Item -ItemType Directory -Force "release" | Out-Null
& $makensis /V2 "/DVERSION=$version" "installer\polley.nsi"
if ($LASTEXITCODE -ne 0) { throw "o NSIS falhou" }

$setup = Get-Item "release\Polley-Nativo-Setup-$version.exe"
"Pronto: $($setup.FullName) ($([math]::Round($setup.Length / 1MB, 1)) MB)"

# 4. latest.json assinado com a chave privada (%USERPROFILE%\.polley\update-key.pk8).
$envFile = Join-Path (Split-Path $native -Parent) ".env"
$supabase = (Select-String -Path $envFile -Pattern 'VITE_SUPABASE_URL\s*=\s*(\S+)').Matches[0].Groups[1].Value.TrimEnd('/')
$url = "$supabase/storage/v1/object/public/releases/$($setup.Name)"
cargo run -q --release -p polley-release -- assinar $setup.FullName $version $url "release\latest.json" $Novidades
if ($LASTEXITCODE -ne 0) { throw "não consegui assinar a atualização" }
"Envie para o bucket 'releases' do Supabase: $($setup.Name) e latest.json (substituindo o antigo)"
