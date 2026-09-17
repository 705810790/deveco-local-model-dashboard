$base = 'http://127.0.0.1:777'

function TestEP($name, $url, $method, $body, $timeout) {
    try {
        $params = New-Object System.Collections.Hashtable
        $params['Uri'] = $url
        $params['UseBasicParsing'] = $true
        $params['TimeoutSec'] = $timeout
        if ($method) { $params['Method'] = $method }
        if ($body) { $params['Body'] = $body; $params['ContentType'] = 'application/json' }
        $r = Invoke-WebRequest @params
        $c = $r.Content
        if ($c.Length -gt 400) { $c = $c.Substring(0, 400) }
        Write-Host "[OK] $name -> $($r.StatusCode): $c"
    } catch {
        $code = $_.Exception.Response.StatusCode.value__
        Write-Host "[ERR] $name -> $code : $($_.Exception.Message)"
    }
}

Write-Host '===== POST endpoints ====='
TestEP 'probe' "$base/v2/probe" 'POST' '{"model":"GLM-5.1"}' 70
TestEP 'lan-check' "$base/v2/lan-check" 'POST' '{}' 15
TestEP 'chat' "$base/v2/chat/completions" 'POST' '{"model":"GLM-5.1","messages":[{"role":"user","content":"say hi"}],"stream":false,"max_tokens":16}' 30
TestEP 'anthropic' "$base/v2/anthropic/v1/messages" 'POST' '{"model":"GLM-5.1","messages":[{"role":"user","content":"say hi"}],"stream":false,"max_tokens":16}' 30
TestEP 'responses' "$base/v2/responses" 'POST' '{"model":"GLM-5.1","input":"say hi","stream":false,"max_output_tokens":16}' 30

Write-Host ''
Write-Host '===== no-prefix routes ====='
TestEP 'GET /status' "$base/status" 'GET' $null 5
TestEP 'GET /models' "$base/models" 'GET' $null 5
TestEP 'GET /network' "$base/network" 'GET' $null 5
TestEP 'GET /dashboard' "$base/dashboard" 'GET' $null 5
TestEP 'GET /' "$base/" 'GET' $null 5

Write-Host ''
Write-Host '===== edge cases ====='
TestEP 'responses-no-model' "$base/v2/responses" 'POST' '{"input":"hi"}' 5
TestEP 'chat-bad-json' "$base/v2/chat/completions" 'POST' 'not-json' 5
TestEP 'responses-glm52' "$base/v2/responses" 'POST' '{"model":"glm-5.2","input":"hi","stream":false,"max_output_tokens":16}' 30
TestEP '404' "$base/v2/nonexistent" 'GET' $null 5

Write-Host ''
Write-Host '===== LAN toggle ====='
TestEP 'lan-on' "$base/v2/network" 'POST' '{"lan_access":true}' 5
Start-Sleep 1
TestEP 'lan-check-after-on' "$base/v2/lan-check" 'POST' '{}' 15
TestEP 'lan-off' "$base/v2/network" 'POST' '{"lan_access":false}' 5

Write-Host ''
Write-Host '===== Dashboard HTML ====='
try {
    $d = (Invoke-WebRequest "$base/v2/dashboard" -UseBasicParsing -TimeoutSec 5).Content
    Write-Host "HTML length: $($d.Length)"
    Write-Host "has title: $($d -match 'title')"
    Write-Host "has refreshStatus: $($d -match 'refreshStatus')"
    Write-Host "has refreshModels: $($d -match 'refreshModels')"
    Write-Host "has runLanCheck: $($d -match 'runLanCheck')"
    Write-Host "has toggleAutostart: $($d -match 'toggleAutostart')"
    Write-Host "has shutdownService: $($d -match 'shutdownService')"
    Write-Host "has probeAll: $($d -match 'probeAll')"
} catch {
    Write-Host "[ERR] Dashboard: $($_.Exception.Message)"
}
