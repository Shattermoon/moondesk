# Windows PowerShell 5.1 rejects pipeline-chain operators at execution time, but its own parser
# still tokenizes AndAnd/OrOr correctly. Use that token stream for compatibility rewriting so
# comments, barewords, block comments, quoted strings, and here-strings keep native lexical rules.
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$OutputEncoding = [Console]::OutputEncoding

$__moondesk_source = $env:MOONDESK_INTERNAL_WINDOWS_COMMAND
Remove-Item Env:MOONDESK_INTERNAL_WINDOWS_COMMAND -ErrorAction SilentlyContinue

$global:__MOONDESK_CHAIN_EPOCH___MOONDESK_SUFFIX__ = [int64]0
$global:__MOONDESK_CHAIN_LAST_CODE___MOONDESK_SUFFIX__ = 0
$global:__MOONDESK_FINAL_EPOCH_BEFORE___MOONDESK_SUFFIX__ = [int64]0

function Convert-MoonDeskEnvPrefix([string]$segment) {
    $remaining = $segment
    $prefix = New-Object System.Text.StringBuilder
    while ($true) {
        $match = [regex]::Match(
            $remaining,
            '^\s*(?<name>[A-Za-z_][A-Za-z0-9_]*)=(?:(?<sq>''(?:''''|[^''])*'')|(?<dq>"(?:`.|[^"])*")|(?<bare>\S+))(?=\s|$)'
        )
        if (-not $match.Success) {
            break
        }

        $raw = if ($match.Groups['sq'].Success) {
            $match.Groups['sq'].Value
        } elseif ($match.Groups['dq'].Success) {
            $match.Groups['dq'].Value
        } else {
            $match.Groups['bare'].Value
        }
        if (($raw.StartsWith("'") -and $raw.EndsWith("'")) -or
            ($raw.StartsWith('"') -and $raw.EndsWith('"'))) {
            $valueTokens = $null
            $valueErrors = $null
            [void][System.Management.Automation.Language.Parser]::ParseInput(
                $raw,
                [ref]$valueTokens,
                [ref]$valueErrors
            )
            if ($valueTokens.Count -gt 0 -and
                $valueTokens[0].PSObject.Properties.Name -contains 'Value') {
                $raw = [string]$valueTokens[0].Value
            } else {
                $raw = $raw.Substring(1, $raw.Length - 2)
            }
        }

        $quoted = "'" + $raw.Replace("'", "''") + "'"
        [void]$prefix.Append('$env:' + $match.Groups['name'].Value + '=' + $quoted + '; ')
        $remaining = $remaining.Substring($match.Length)
    }

    if ($prefix.Length -eq 0) {
        return $segment
    }
    return $prefix.ToString() + $remaining.TrimStart()
}

function Get-MoonDeskTokenRecords($tokens) {
    $depth = 0
    $records = New-Object System.Collections.Generic.List[object]
    foreach ($token in $tokens) {
        $kind = $token.Kind
        [void]$records.Add([pscustomobject]@{ Token = $token; Depth = $depth })
        if ($kind -eq [System.Management.Automation.Language.TokenKind]::LCurly -or
            $kind -eq [System.Management.Automation.Language.TokenKind]::LParen -or
            $kind -eq [System.Management.Automation.Language.TokenKind]::LBracket -or
            $kind -eq [System.Management.Automation.Language.TokenKind]::AtCurly -or
            $kind -eq [System.Management.Automation.Language.TokenKind]::AtParen -or
            $kind -eq [System.Management.Automation.Language.TokenKind]::DollarParen) {
            $depth++
        } elseif ($kind -eq [System.Management.Automation.Language.TokenKind]::RCurly -or
                  $kind -eq [System.Management.Automation.Language.TokenKind]::RParen -or
                  $kind -eq [System.Management.Automation.Language.TokenKind]::RBracket) {
            if ($depth -gt 0) {
                $depth--
            }
        }
    }
    return $records.ToArray()
}

function Add-MoonDeskStatementSnapshots([string]$text) {
    $tokens = $null
    $parseErrors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseInput(
        $text,
        [ref]$tokens,
        [ref]$parseErrors
    )
    $offsets = New-Object 'System.Collections.Generic.HashSet[int]'

    foreach ($block in @($ast.BeginBlock, $ast.ProcessBlock, $ast.EndBlock)) {
        if ($null -eq $block) {
            continue
        }
        foreach ($statement in $block.Statements) {
            [void]$offsets.Add([int]$statement.Extent.StartOffset)
        }
    }
    foreach ($block in $ast.FindAll({
        param($node)
        $node -is [System.Management.Automation.Language.StatementBlockAst]
    }, $true)) {
        foreach ($statement in $block.Statements) {
            [void]$offsets.Add([int]$statement.Extent.StartOffset)
        }
    }
    if ($offsets.Count -eq 0) {
        return $text
    }

    $snapshot = "`n`$global:__MOONDESK_FINAL_EPOCH_BEFORE___MOONDESK_SUFFIX__=`$global:__MOONDESK_CHAIN_EPOCH___MOONDESK_SUFFIX__`n"
    foreach ($offset in @($offsets) | Sort-Object -Descending) {
        $safeOffset = [Math]::Min([int]$offset, $text.Length)
        $text = $text.Substring(0, $safeOffset) + $snapshot + $text.Substring($safeOffset)
    }
    return $text
}

function Test-MoonDeskNewLineContinuation($records, [int]$index, [int]$depth) {
    for ($cursor = $index - 1; $cursor -ge 0; $cursor--) {
        $record = $records[$cursor]
        if ($record.Depth -lt $depth) {
            break
        }
        if ($record.Depth -ne $depth) {
            continue
        }
        if ($record.Token.Kind -eq [System.Management.Automation.Language.TokenKind]::Comment -or
            $record.Token.Kind -eq [System.Management.Automation.Language.TokenKind]::NewLine) {
            continue
        }
        return $record.Token.Kind -eq [System.Management.Automation.Language.TokenKind]::AndAnd -or
            $record.Token.Kind -eq [System.Management.Automation.Language.TokenKind]::OrOr
    }

    for ($cursor = $index + 1; $cursor -lt $records.Count; $cursor++) {
        $record = $records[$cursor]
        if ($record.Depth -lt $depth) {
            break
        }
        if ($record.Depth -ne $depth) {
            continue
        }
        if ($record.Token.Kind -eq [System.Management.Automation.Language.TokenKind]::Comment -or
            $record.Token.Kind -eq [System.Management.Automation.Language.TokenKind]::NewLine) {
            continue
        }
        return $record.Token.Kind -eq [System.Management.Automation.Language.TokenKind]::AndAnd -or
            $record.Token.Kind -eq [System.Management.Automation.Language.TokenKind]::OrOr
    }

    return $false
}

function Add-MoonDeskChainSegmentPrelude([System.Text.StringBuilder]$builder) {
    [void]$builder.Append(
        "`$__MOONDESK_CHAIN_EPOCH_BEFORE___MOONDESK_SUFFIX__=`$global:__MOONDESK_CHAIN_EPOCH___MOONDESK_SUFFIX__`n"
    )
    [void]$builder.Append("`$global:LASTEXITCODE=0`n")
}

function Add-MoonDeskChainStatus([System.Text.StringBuilder]$builder) {
    [void]$builder.Append("`n`$__MOONDESK_CHAIN_RAW_OK___MOONDESK_SUFFIX__=`$?`n")
    [void]$builder.Append(
        "`$__MOONDESK_CHAIN_RAW_CODE___MOONDESK_SUFFIX__=if (`$__MOONDESK_CHAIN_RAW_OK___MOONDESK_SUFFIX__) {0} elseif (`$LASTEXITCODE -ne 0) {`$LASTEXITCODE} else {1}`n"
    )
    [void]$builder.Append(
        "if (`$global:__MOONDESK_CHAIN_EPOCH___MOONDESK_SUFFIX__ -gt `$__MOONDESK_CHAIN_EPOCH_BEFORE___MOONDESK_SUFFIX__) {`n"
    )
    [void]$builder.Append(
        "`$__MOONDESK_CHAIN_CODE___MOONDESK_SUFFIX__=`$global:__MOONDESK_CHAIN_LAST_CODE___MOONDESK_SUFFIX__`n"
    )
    [void]$builder.Append(
        "`$__MOONDESK_CHAIN_OK___MOONDESK_SUFFIX__=(`$__MOONDESK_CHAIN_CODE___MOONDESK_SUFFIX__ -eq 0)`n"
    )
    [void]$builder.Append("} else {`n")
    [void]$builder.Append(
        "`$__MOONDESK_CHAIN_CODE___MOONDESK_SUFFIX__=`$__MOONDESK_CHAIN_RAW_CODE___MOONDESK_SUFFIX__`n"
    )
    [void]$builder.Append(
        "`$__MOONDESK_CHAIN_OK___MOONDESK_SUFFIX__=`$__MOONDESK_CHAIN_RAW_OK___MOONDESK_SUFFIX__`n"
    )
    [void]$builder.Append("}`n")
}

function Convert-MoonDeskChains([string]$text) {
    $text = Convert-MoonDeskEnvPrefix $text

    while ($true) {
        $tokens = $null
        $parseErrors = $null
        [void][System.Management.Automation.Language.Parser]::ParseInput(
            $text,
            [ref]$tokens,
            [ref]$parseErrors
        )
        $records = @(Get-MoonDeskTokenRecords $tokens)
        $operatorIndexes = New-Object System.Collections.Generic.List[int]
        for ($index = 0; $index -lt $records.Count; $index++) {
            $kind = $records[$index].Token.Kind
            if ($kind -eq [System.Management.Automation.Language.TokenKind]::AndAnd -or
                $kind -eq [System.Management.Automation.Language.TokenKind]::OrOr) {
                [void]$operatorIndexes.Add($index)
            }
        }
        if ($operatorIndexes.Count -eq 0) {
            return $text
        }

        $targetIndex = -1
        $targetDepth = -1
        $targetOffset = [int]::MaxValue
        foreach ($operatorIndex in $operatorIndexes) {
            $record = $records[$operatorIndex]
            $offset = $record.Token.Extent.StartOffset
            if ($record.Depth -gt $targetDepth -or
                ($record.Depth -eq $targetDepth -and $offset -lt $targetOffset)) {
                $targetIndex = $operatorIndex
                $targetDepth = $record.Depth
                $targetOffset = $offset
            }
        }
        if ($targetIndex -lt 0) {
            return $text
        }

        $start = 0
        for ($index = $targetIndex - 1; $index -ge 0; $index--) {
            $record = $records[$index]
            if ($record.Depth -lt $targetDepth) {
                $start = [Math]::Min($record.Token.Extent.EndOffset, $text.Length)
                break
            }
            if ($record.Depth -ne $targetDepth) {
                continue
            }
            if ($record.Token.Kind -eq [System.Management.Automation.Language.TokenKind]::Semi) {
                $start = [Math]::Min($record.Token.Extent.EndOffset, $text.Length)
                break
            }
            if ($record.Token.Kind -eq [System.Management.Automation.Language.TokenKind]::NewLine -and
                -not (Test-MoonDeskNewLineContinuation $records $index $targetDepth)) {
                $start = [Math]::Min($record.Token.Extent.EndOffset, $text.Length)
                break
            }
        }

        $end = $text.Length
        for ($index = $targetIndex + 1; $index -lt $records.Count; $index++) {
            $record = $records[$index]
            if ($record.Depth -lt $targetDepth) {
                $end = [Math]::Min($record.Token.Extent.StartOffset, $text.Length)
                break
            }
            if ($record.Depth -ne $targetDepth) {
                continue
            }
            if ($record.Token.Kind -eq [System.Management.Automation.Language.TokenKind]::RCurly -or
                $record.Token.Kind -eq [System.Management.Automation.Language.TokenKind]::RParen -or
                $record.Token.Kind -eq [System.Management.Automation.Language.TokenKind]::RBracket -or
                $record.Token.Kind -eq [System.Management.Automation.Language.TokenKind]::EndOfInput) {
                $end = [Math]::Min($record.Token.Extent.StartOffset, $text.Length)
                break
            }
            if ($record.Token.Kind -eq [System.Management.Automation.Language.TokenKind]::Semi) {
                $end = [Math]::Min($record.Token.Extent.StartOffset, $text.Length)
                break
            }
            if ($record.Token.Kind -eq [System.Management.Automation.Language.TokenKind]::NewLine -and
                -not (Test-MoonDeskNewLineContinuation $records $index $targetDepth)) {
                $end = [Math]::Min($record.Token.Extent.StartOffset, $text.Length)
                break
            }
        }

        if ($end -lt $start) {
            return $text
        }

        $targetOperators = New-Object System.Collections.Generic.List[object]
        foreach ($operatorIndex in $operatorIndexes) {
            $record = $records[$operatorIndex]
            if ($record.Depth -eq $targetDepth -and
                $record.Token.Extent.StartOffset -ge $start -and
                $record.Token.Extent.EndOffset -le $end) {
                [void]$targetOperators.Add($record.Token)
            }
        }
        if ($targetOperators.Count -eq 0) {
            return $text
        }

        $parts = New-Object System.Collections.Generic.List[string]
        $cursor = $start
        foreach ($operator in $targetOperators) {
            [void]$parts.Add($text.Substring($cursor, $operator.Extent.StartOffset - $cursor))
            $cursor = $operator.Extent.EndOffset
        }
        [void]$parts.Add($text.Substring($cursor, $end - $cursor))

        $builder = New-Object System.Text.StringBuilder
        Add-MoonDeskChainSegmentPrelude $builder
        [void]$builder.Append((Convert-MoonDeskEnvPrefix $parts[0]))
        Add-MoonDeskChainStatus $builder
        for ($index = 0; $index -lt $targetOperators.Count; $index++) {
            if ($targetOperators[$index].Kind -eq [System.Management.Automation.Language.TokenKind]::AndAnd) {
                [void]$builder.Append("if (`$__MOONDESK_CHAIN_OK___MOONDESK_SUFFIX__) {`n")
            } else {
                [void]$builder.Append("if (-not `$__MOONDESK_CHAIN_OK___MOONDESK_SUFFIX__) {`n")
            }
            Add-MoonDeskChainSegmentPrelude $builder
            [void]$builder.Append((Convert-MoonDeskEnvPrefix $parts[$index + 1]))
            Add-MoonDeskChainStatus $builder
            [void]$builder.Append("}`n")
        }
        [void]$builder.Append(
            'if (-not $__MOONDESK_CHAIN_OK___MOONDESK_SUFFIX__) { & $env:ComSpec /d /c ("exit " + $__MOONDESK_CHAIN_CODE___MOONDESK_SUFFIX__) >$null 2>$null }' + "`n"
        )
        [void]$builder.Append(
            "`$global:__MOONDESK_CHAIN_EPOCH___MOONDESK_SUFFIX__=[int64]`$global:__MOONDESK_CHAIN_EPOCH___MOONDESK_SUFFIX__+1`n"
        )
        [void]$builder.Append(
            "`$global:__MOONDESK_CHAIN_LAST_CODE___MOONDESK_SUFFIX__=`$__MOONDESK_CHAIN_CODE___MOONDESK_SUFFIX__`n"
        )

        $replacement = $builder.ToString()
        if ($targetDepth -gt 0) {
            $replacement = "& {`n" + $replacement + "}`n"
        }
        $text = $text.Substring(0, $start) + $replacement + $text.Substring($end)
    }
}

$__moondesk_saved_error_action = $ErrorActionPreference
$ErrorActionPreference = 'Stop'
try {
    $__moondesk_source = Convert-MoonDeskEnvPrefix $__moondesk_source
    $__moondesk_source = Add-MoonDeskStatementSnapshots $__moondesk_source
    $__moondesk_converted = Convert-MoonDeskChains $__moondesk_source
} catch {
    [Console]::Error.WriteLine(
        'MoonDesk could not prepare the Windows shell command: ' + $_.Exception.Message
    )
    exit 1
} finally {
    $ErrorActionPreference = $__moondesk_saved_error_action
}

$__moondesk_converted += "`n`n`$__MOONDESK_FINAL_RAW_OK___MOONDESK_SUFFIX__=`$?`n"
$__moondesk_converted += "`$__MOONDESK_FINAL_RAW_CODE___MOONDESK_SUFFIX__=if (`$__MOONDESK_FINAL_RAW_OK___MOONDESK_SUFFIX__) {0} elseif (`$LASTEXITCODE -ne 0) {`$LASTEXITCODE} else {1}`n"
$__moondesk_converted += "if (`$global:__MOONDESK_CHAIN_EPOCH___MOONDESK_SUFFIX__ -gt `$global:__MOONDESK_FINAL_EPOCH_BEFORE___MOONDESK_SUFFIX__) {`n"
$__moondesk_converted += "`$global:__MOONDESK_FINAL_CODE___MOONDESK_SUFFIX__=`$global:__MOONDESK_CHAIN_LAST_CODE___MOONDESK_SUFFIX__`n"
$__moondesk_converted += "`$global:__MOONDESK_FINAL_OK___MOONDESK_SUFFIX__=(`$global:__MOONDESK_FINAL_CODE___MOONDESK_SUFFIX__ -eq 0)`n"
$__moondesk_converted += "} else {`n"
$__moondesk_converted += "`$global:__MOONDESK_FINAL_CODE___MOONDESK_SUFFIX__=`$__MOONDESK_FINAL_RAW_CODE___MOONDESK_SUFFIX__`n"
$__moondesk_converted += "`$global:__MOONDESK_FINAL_OK___MOONDESK_SUFFIX__=`$__MOONDESK_FINAL_RAW_OK___MOONDESK_SUFFIX__`n"
$__moondesk_converted += "}`n"

Invoke-Expression $__moondesk_converted

$__moondesk_final_ok = Get-Variable -Name '__MOONDESK_FINAL_OK___MOONDESK_SUFFIX__' -Scope Global -ValueOnly -ErrorAction SilentlyContinue
$__moondesk_final_code = Get-Variable -Name '__MOONDESK_FINAL_CODE___MOONDESK_SUFFIX__' -Scope Global -ValueOnly -ErrorAction SilentlyContinue

if ($null -ne $__moondesk_final_ok -and -not $__moondesk_final_ok) {
    if ($null -ne $__moondesk_final_code) {
        exit $__moondesk_final_code
    }
    exit 1
}
