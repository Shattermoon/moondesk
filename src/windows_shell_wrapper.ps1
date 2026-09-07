# Windows PowerShell 5.1 rejects pipeline-chain operators at execution time, but its own parser
# still tokenizes AndAnd/OrOr correctly. Use that token stream for compatibility rewriting so
# comments, barewords, block comments, quoted strings, and here-strings keep native lexical rules.
$__moondesk_source = $env:MOONDESK_INTERNAL_WINDOWS_COMMAND
Remove-Item Env:MOONDESK_INTERNAL_WINDOWS_COMMAND -ErrorAction SilentlyContinue

# Hardened Windows hosts can force Constrained Language Mode, where the reflection/type creation
# used by the compatibility transformer is unavailable. Preserve native PowerShell behavior there
# instead of failing before an otherwise ordinary command can run.
if ($ExecutionContext.SessionState.LanguageMode -eq 'ConstrainedLanguage') {
    $__moondesk_converted = $__moondesk_source
    $__moondesk_converted += "`n`n`$global:__MOONDESK_CLM_FINAL_OK___MOONDESK_SUFFIX__=`$?`n"
    $__moondesk_converted += "`$global:__MOONDESK_CLM_FINAL_CODE___MOONDESK_SUFFIX__=if (`$global:__MOONDESK_CLM_FINAL_OK___MOONDESK_SUFFIX__) {0} elseif (`$LASTEXITCODE -ne 0) {`$LASTEXITCODE} else {1}`n"
    Invoke-Expression $__moondesk_converted

    $__moondesk_clm_ok = Get-Variable -Name '__MOONDESK_CLM_FINAL_OK___MOONDESK_SUFFIX__' -Scope Global -ValueOnly -ErrorAction SilentlyContinue
    $__moondesk_clm_code = Get-Variable -Name '__MOONDESK_CLM_FINAL_CODE___MOONDESK_SUFFIX__' -Scope Global -ValueOnly -ErrorAction SilentlyContinue
    if ($null -eq $__moondesk_clm_ok) {
        exit 1
    }
    if (-not $__moondesk_clm_ok) {
        if ($null -ne $__moondesk_clm_code) {
            exit $__moondesk_clm_code
        }
        exit 1
    }
    exit 0
}

[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$OutputEncoding = [Console]::OutputEncoding

$global:__MOONDESK_CHAIN_EPOCH___MOONDESK_SUFFIX__ = [int64]0
$global:__MOONDESK_CHAIN_LAST_CODE___MOONDESK_SUFFIX__ = 0
$global:__MOONDESK_FINAL_EPOCH_BEFORE___MOONDESK_SUFFIX__ = [int64]0
$script:__moondesk_env_prefix_id = [int64]0

# Windows PowerShell 5.1 cannot preserve an existing empty process-environment value through
# either Env: or Environment.SetEnvironmentVariable: both treat an empty string as deletion.
# Reuse the .NET Framework Win32 binding so empty and absent values stay distinguishable.
$script:__MOONDESK_WIN32_NATIVE_TYPE___MOONDESK_SUFFIX__ = [System.Object].Assembly.GetType('Microsoft.Win32.Win32Native')
$script:__MOONDESK_NATIVE_SET_ENV___MOONDESK_SUFFIX__ = if ($null -ne $script:__MOONDESK_WIN32_NATIVE_TYPE___MOONDESK_SUFFIX__) {
    $script:__MOONDESK_WIN32_NATIVE_TYPE___MOONDESK_SUFFIX__.GetMethod(
        'SetEnvironmentVariable',
        [System.Reflection.BindingFlags]'NonPublic,Static'
    )
} else {
    $null
}

function Convert-MoonDeskEnvPrefix([string]$segment) {
    $remaining = $segment
    $assignments = New-Object System.Collections.Generic.List[object]
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

        [void]$assignments.Add([pscustomobject]@{
            Name = $match.Groups['name'].Value
            Value = $raw
        })
        $remaining = $remaining.Substring($match.Length)
    }

    if ($assignments.Count -eq 0 -or [string]::IsNullOrWhiteSpace($remaining)) {
        return $segment
    }
    if ($null -eq $script:__MOONDESK_NATIVE_SET_ENV___MOONDESK_SUFFIX__) {
        throw 'MoonDesk could not access the Windows process environment setter.'
    }

    $scopeId = $script:__moondesk_env_prefix_id
    $script:__moondesk_env_prefix_id = [int64]$script:__moondesk_env_prefix_id + 1
    $okVar = '__MOONDESK_ENV_OK___MOONDESK_SUFFIX___' + $scopeId
    $codeVar = '__MOONDESK_ENV_CODE___MOONDESK_SUFFIX___' + $scopeId
    $snapshotVar = '__MOONDESK_ENV_SNAPSHOT___MOONDESK_SUFFIX___' + $scopeId
    $builder = New-Object System.Text.StringBuilder

    [void]$builder.Append(
        '$' + $snapshotVar + '=[Environment]::GetEnvironmentVariables([EnvironmentVariableTarget]::Process)' + "`n"
    )
    for ($index = 0; $index -lt $assignments.Count; $index++) {
        $assignment = $assignments[$index]
        $existsVar = '__MOONDESK_ENV_EXISTED_' + $index + '___MOONDESK_SUFFIX___' + $scopeId
        $valueVar = '__MOONDESK_ENV_VALUE_' + $index + '___MOONDESK_SUFFIX___' + $scopeId
        [void]$builder.Append(
            '$' + $existsVar + '=@($' + $snapshotVar + '.Keys) -contains ''' + $assignment.Name + '''' + "`n"
        )
        [void]$builder.Append(
            '$' + $valueVar + '=if ($' + $existsVar + ') {[string][Environment]::GetEnvironmentVariable(''' +
            $assignment.Name + ''',[EnvironmentVariableTarget]::Process)} else {$null}' + "`n"
        )
    }

    [void]$builder.Append("try {`n")
    foreach ($assignment in $assignments) {
        $quoted = "'" + ([string]$assignment.Value).Replace("'", "''") + "'"
        [void]$builder.Append(
            'if (-not $script:__MOONDESK_NATIVE_SET_ENV___MOONDESK_SUFFIX__.Invoke($null,@(''' +
            $assignment.Name + ''',' + $quoted +
            '))) { throw ''MoonDesk could not set the temporary Windows process environment.'' }' + "`n"
        )
    }
    [void]$builder.Append("`$global:LASTEXITCODE=0`n")
    $trimmedRemaining = $remaining.Trim()
    $plainParenthesized = $trimmedRemaining.StartsWith('(') -and $trimmedRemaining.EndsWith(')')
    [void]$builder.Append($remaining.TrimStart())
    [void]$builder.Append("`n`$" + $okVar + "=`$?`n")
    if ($plainParenthesized) {
        [void]$builder.Append(
            '$' + $codeVar + '=if ($LASTEXITCODE -ne 0) {$LASTEXITCODE} elseif ($' + $okVar + ') {0} else {1}' + "`n"
        )
    } else {
        [void]$builder.Append(
            '$' + $codeVar + '=if ($' + $okVar + ') {0} elseif ($LASTEXITCODE -ne 0) {$LASTEXITCODE} else {1}' + "`n"
        )
    }
    [void]$builder.Append("} finally {`n")
    for ($index = $assignments.Count - 1; $index -ge 0; $index--) {
        $assignment = $assignments[$index]
        $existsVar = '__MOONDESK_ENV_EXISTED_' + $index + '___MOONDESK_SUFFIX___' + $scopeId
        $valueVar = '__MOONDESK_ENV_VALUE_' + $index + '___MOONDESK_SUFFIX___' + $scopeId
        [void]$builder.Append('if ($' + $existsVar + ") {`n")
        [void]$builder.Append(
            'if (-not $script:__MOONDESK_NATIVE_SET_ENV___MOONDESK_SUFFIX__.Invoke($null,@(''' +
            $assignment.Name + ''',$' + $valueVar +
            '))) { throw ''MoonDesk could not restore the Windows process environment.'' }' + "`n"
        )
        [void]$builder.Append("} else {`n")
        [void]$builder.Append(
            'if (-not $script:__MOONDESK_NATIVE_SET_ENV___MOONDESK_SUFFIX__.Invoke($null,@(''' +
            $assignment.Name + ''',$null))) { throw ''MoonDesk could not restore the Windows process environment.'' }' + "`n"
        )
        [void]$builder.Append("}`n")
    }
    [void]$builder.Append("}`n")
    [void]$builder.Append(
        'if ($' + $codeVar + ' -eq 0) {$global:LASTEXITCODE=0} else {& $env:ComSpec /d /c ("exit " + $' + $codeVar + ') >$null 2>$null}' + "`n"
    )
    return $builder.ToString()
}

function Convert-MoonDeskEnvPrefixes([string]$text) {
    while ($true) {
        $tokens = $null
        $parseErrors = $null
        $ast = [System.Management.Automation.Language.Parser]::ParseInput(
            $text,
            [ref]$tokens,
            [ref]$parseErrors
        )
        $target = $null
        $targetReplacement = $null
        foreach ($pipeline in $ast.FindAll({
            param($node)
            $node -is [System.Management.Automation.Language.PipelineAst]
        }, $true)) {
            $candidate = $pipeline.Extent.Text
            $replacement = Convert-MoonDeskEnvPrefix $candidate
            if ($replacement -eq $candidate) {
                continue
            }
            if ($null -eq $target -or
                $pipeline.Extent.StartOffset -gt $target.Extent.StartOffset -or
                ($pipeline.Extent.StartOffset -eq $target.Extent.StartOffset -and
                 $pipeline.Extent.EndOffset -lt $target.Extent.EndOffset)) {
                $target = $pipeline
                $targetReplacement = $replacement
            }
        }

        if ($null -eq $target) {
            return $text
        }

        $start = [Math]::Min([int]$target.Extent.StartOffset, $text.Length)
        $end = [Math]::Min([int]$target.Extent.EndOffset, $text.Length)
        $text = $text.Substring(0, $start) + $targetReplacement + $text.Substring($end)
    }
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
    # Ordinary PowerShell must not pay for chain bookkeeping or have its automatic status touched.
    # Only scripts that actually contain tokenized &&/|| operators need statement snapshots.
    $hasChain = $false
    foreach ($token in $tokens) {
        if ($token.Kind -eq [System.Management.Automation.Language.TokenKind]::AndAnd -or
            $token.Kind -eq [System.Management.Automation.Language.TokenKind]::OrOr) {
            $hasChain = $true
            break
        }
    }
    if (-not $hasChain) {
        return $text
    }

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

    # Capturing/updating the epoch uses successful assignments, which would otherwise turn `$?` true.
    # Restore an incoming false status with the module-qualified built-in cmdlet. -ErrorAction Ignore
    # changes `$?` without emitting output, appending to `$Error`, or changing `$LASTEXITCODE`.
    $snapshot = "`n`$__MOONDESK_STATUS_BEFORE_SNAPSHOT___MOONDESK_SUFFIX__=`$?`n" +
        "`$global:__MOONDESK_FINAL_EPOCH_BEFORE___MOONDESK_SUFFIX__=`$global:__MOONDESK_CHAIN_EPOCH___MOONDESK_SUFFIX__`n" +
        "if (-not `$__MOONDESK_STATUS_BEFORE_SNAPSHOT___MOONDESK_SUFFIX__) { Microsoft.PowerShell.Utility\Write-Error 'MoonDesk status restore' -ErrorAction Ignore }`n"
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
        [void]$builder.Append($parts[0])
        Add-MoonDeskChainStatus $builder
        for ($index = 0; $index -lt $targetOperators.Count; $index++) {
            if ($targetOperators[$index].Kind -eq [System.Management.Automation.Language.TokenKind]::AndAnd) {
                [void]$builder.Append("if (`$__MOONDESK_CHAIN_OK___MOONDESK_SUFFIX__) {`n")
            } else {
                [void]$builder.Append("if (-not `$__MOONDESK_CHAIN_OK___MOONDESK_SUFFIX__) {`n")
            }
            Add-MoonDeskChainSegmentPrelude $builder
            [void]$builder.Append($parts[$index + 1])
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
    $__moondesk_source = Add-MoonDeskStatementSnapshots $__moondesk_source
    $__moondesk_converted = Convert-MoonDeskChains $__moondesk_source
    $__moondesk_converted = Convert-MoonDeskEnvPrefixes $__moondesk_converted
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
