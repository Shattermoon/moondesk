use std::collections::{BTreeMap, HashSet};
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::{Map, Value};

const CONTRACT_JSON: &str = include_str!("browser_contract_v1_7.json");

#[derive(Clone, Debug, Deserialize)]
struct BrowserContractFile {
    version: String,
    commands: BTreeMap<String, Vec<BrowserArgSpec>>,
}

#[derive(Clone, Debug, Deserialize)]
struct BrowserArgSpec {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    required: bool,
    #[serde(rename = "enum", default)]
    choices: Vec<String>,
    #[serde(default)]
    default: Option<Value>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserOutputFormat {
    Markdown,
    Json,
}

#[derive(Clone, Debug)]
pub struct ParsedBrowserInvocation {
    pub arguments: Map<String, Value>,
    pub output_format: BrowserOutputFormat,
}

fn contract() -> Result<&'static BrowserContractFile, String> {
    static CONTRACT: OnceLock<Result<BrowserContractFile, String>> = OnceLock::new();
    CONTRACT
        .get_or_init(|| {
            let parsed: BrowserContractFile = serde_json::from_str(CONTRACT_JSON)
                .map_err(|error| format!("Embedded browser contract is invalid: {error}"))?;
            if parsed.version != "1.7.0" {
                return Err(format!(
                    "Embedded browser contract version {} does not match pinned 1.7.0",
                    parsed.version
                ));
            }
            Ok(parsed)
        })
        .as_ref()
        .map_err(Clone::clone)
}

pub fn canonical_browser_flag_name(raw: &str) -> Option<String> {
    let flag = raw.strip_prefix('-')?.trim_start_matches('-');
    if flag.is_empty() {
        return None;
    }
    let flag = flag.split_once('=').map_or(flag, |(name, _)| name);
    Some(
        flag.chars()
            .filter(|ch| !matches!(ch, '-' | '_'))
            .flat_map(char::to_lowercase)
            .collect(),
    )
}

fn parse_bool(raw: &str) -> Result<bool, String> {
    match raw.to_ascii_lowercase().as_str() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(format!("expected boolean value, got '{raw}'")),
    }
}

fn parse_scalar(spec: &BrowserArgSpec, raw: &str) -> Result<Value, String> {
    let value = match spec.kind.as_str() {
        "string" => Value::String(raw.to_string()),
        "boolean" => Value::Bool(parse_bool(raw)?),
        "integer" => {
            let parsed = raw
                .parse::<i64>()
                .map_err(|_| format!("argument '{}' expects an integer, got '{raw}'", spec.name))?;
            Value::Number(parsed.into())
        }
        "number" => {
            let parsed = raw
                .parse::<f64>()
                .map_err(|_| format!("argument '{}' expects a number, got '{raw}'", spec.name))?;
            let number = serde_json::Number::from_f64(parsed)
                .ok_or_else(|| format!("argument '{}' must be a finite number", spec.name))?;
            Value::Number(number)
        }
        "array" => {
            return Err(format!(
                "internal browser contract error: '{}' is an array",
                spec.name
            ));
        }
        other => {
            return Err(format!(
                "unsupported browser argument type '{other}' for '{}'",
                spec.name
            ));
        }
    };
    if !spec.choices.is_empty() {
        let Some(candidate) = value.as_str() else {
            return Err(format!(
                "internal browser contract error: enum '{}' is not a string",
                spec.name
            ));
        };
        if !spec.choices.iter().any(|choice| choice == candidate) {
            return Err(format!(
                "argument '{}' must be one of: {}",
                spec.name,
                spec.choices.join(", ")
            ));
        }
    }
    Ok(value)
}

fn strip_flag(raw: &str) -> Option<(&str, Option<&str>)> {
    let raw = raw.strip_prefix("--")?;
    if raw.is_empty() {
        return None;
    }
    Some(
        raw.split_once('=')
            .map_or((raw, None), |(name, value)| (name, Some(value))),
    )
}

fn spec_by_flag<'a>(specs: &'a [BrowserArgSpec], raw_name: &str) -> Option<&'a BrowserArgSpec> {
    let canonical: String = raw_name
        .chars()
        .filter(|ch| !matches!(ch, '-' | '_'))
        .flat_map(char::to_lowercase)
        .collect();
    specs.iter().find(|spec| {
        spec.name
            .chars()
            .filter(|ch| !matches!(ch, '-' | '_'))
            .flat_map(char::to_lowercase)
            .eq(canonical.chars())
    })
}

fn looks_like_local_path(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    raw.starts_with('/')
        || raw.starts_with("\\\\")
        || raw.starts_with("//")
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/'))
}

fn validate_browser_url(raw: &str) -> Result<(), String> {
    let value = raw.trim();
    if value.is_empty() {
        return Err("Browser URL cannot be empty".to_string());
    }
    if value.chars().any(char::is_control) {
        return Err("Browser URL may not contain control characters".to_string());
    }
    if looks_like_local_path(value) {
        return Err("Browser navigation may not target a local filesystem path".to_string());
    }

    let lower = value.to_ascii_lowercase();
    if lower == "about:blank" {
        return Ok(());
    }
    if let Some(inner) = lower.strip_prefix("view-source:") {
        return validate_browser_url(inner);
    }
    if let Some(inner) = lower.strip_prefix("blob:") {
        if inner.starts_with("http://") || inner.starts_with("https://") {
            return Ok(());
        }
        return Err("Browser blob navigation is only allowed for http(s) origins".to_string());
    }

    for scheme in ["http://", "https://", "data:"] {
        if lower.starts_with(scheme) {
            return Ok(());
        }
    }

    Err(format!(
        "Browser navigation scheme is blocked by MoonDesk's host-file boundary: {value}"
    ))
}

fn validate_url_arguments(command: &str, arguments: &Map<String, Value>) -> Result<(), String> {
    match command {
        "new_page" | "navigate_page" => {
            if let Some(url) = arguments.get("url").and_then(Value::as_str) {
                validate_browser_url(url)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_semantic_arguments(
    command: &str,
    arguments: &Map<String, Value>,
) -> Result<(), String> {
    if command == "performance_start_trace"
        && arguments.get("autoStop").and_then(Value::as_bool) == Some(false)
        && arguments.get("filePath").and_then(Value::as_str).is_some()
    {
        return Err(
            "performance_start_trace cannot use --filePath when --autoStop=false because the pinned runtime does not write that file until a later stop; start the trace without --filePath and pass --filePath to performance_stop_trace instead"
                .to_string(),
        );
    }
    Ok(())
}

pub fn parse_browser_cli_invocation(
    command: &str,
    args: &[String],
) -> Result<ParsedBrowserInvocation, String> {
    let contract = contract()?;
    let Some(specs) = contract.commands.get(command) else {
        return Err(format!(
            "Unknown browser command '{command}' for pinned chrome-devtools-mcp@1.7.0"
        ));
    };

    let required = specs
        .iter()
        .filter(|spec| spec.required)
        .collect::<Vec<_>>();
    let mut arguments = Map::new();
    let mut required_index = 0usize;
    let mut seen_flags = HashSet::new();
    let mut output_format = BrowserOutputFormat::Markdown;
    let mut index = 0usize;

    while index < args.len() {
        let raw = &args[index];
        if !raw.starts_with('-') {
            let Some(spec) = required.get(required_index) else {
                return Err(format!(
                    "Browser command '{command}' received unexpected positional argument '{raw}'"
                ));
            };
            arguments.insert(spec.name.clone(), parse_scalar(spec, raw)?);
            required_index += 1;
            index += 1;
            continue;
        }

        let Some((raw_name, inline_value)) = strip_flag(raw) else {
            return Err(format!("Unsupported browser argument syntax '{raw}'"));
        };
        let canonical_name: String = raw_name
            .chars()
            .filter(|ch| !matches!(ch, '-' | '_'))
            .flat_map(char::to_lowercase)
            .collect();

        if canonical_name == "outputformat" {
            if !seen_flags.insert("outputformat".to_string()) {
                return Err("--output-format may only be supplied once".to_string());
            }
            let value = if let Some(value) = inline_value {
                value
            } else {
                index += 1;
                args.get(index)
                    .filter(|value| !value.starts_with('-'))
                    .map(String::as_str)
                    .ok_or_else(|| "--output-format requires md or json".to_string())?
            };
            output_format = match value.to_ascii_lowercase().as_str() {
                "md" => BrowserOutputFormat::Markdown,
                "json" => BrowserOutputFormat::Json,
                _ => return Err("--output-format must be md or json".to_string()),
            };
            index += 1;
            continue;
        }

        let (negative_bool, lookup_name) = raw_name
            .strip_prefix("no-")
            .map_or((false, raw_name), |name| (true, name));
        let Some(spec) = spec_by_flag(specs, lookup_name) else {
            return Err(format!(
                "Unknown argument '--{raw_name}' for browser command '{command}'"
            ));
        };
        let flag_key = canonical_browser_flag_name(&format!("--{}", spec.name))
            .unwrap_or_else(|| spec.name.clone());
        if spec.kind != "array" && !seen_flags.insert(flag_key) {
            return Err(format!(
                "Browser argument '--{}' may only be supplied once",
                spec.name
            ));
        }

        if negative_bool {
            if spec.kind != "boolean" || inline_value.is_some() {
                return Err(format!("'--{raw_name}' is only valid for boolean flags"));
            }
            arguments.insert(spec.name.clone(), Value::Bool(false));
            index += 1;
            continue;
        }

        if spec.kind == "array" {
            let entry = arguments
                .entry(spec.name.clone())
                .or_insert_with(|| Value::Array(Vec::new()));
            let Some(values) = entry.as_array_mut() else {
                return Err(format!(
                    "internal browser contract error for '{}'",
                    spec.name
                ));
            };
            if let Some(value) = inline_value {
                values.push(Value::String(value.to_string()));
                index += 1;
                continue;
            }
            index += 1;
            let start = index;
            while index < args.len() && !args[index].starts_with('-') {
                values.push(Value::String(args[index].clone()));
                index += 1;
            }
            if start == index {
                return Err(format!(
                    "Browser array flag '--{}' requires a value",
                    spec.name
                ));
            }
            continue;
        }

        let value = if spec.kind == "boolean" && inline_value.is_none() {
            if let Some(next) = args.get(index + 1).filter(|value| !value.starts_with('-')) {
                if matches!(
                    next.to_ascii_lowercase().as_str(),
                    "true" | "false" | "1" | "0"
                ) {
                    index += 1;
                    parse_scalar(spec, next)?
                } else {
                    Value::Bool(true)
                }
            } else {
                Value::Bool(true)
            }
        } else {
            let raw_value = if let Some(value) = inline_value {
                value
            } else {
                index += 1;
                args.get(index)
                    .filter(|value| !value.starts_with('-'))
                    .map(String::as_str)
                    .ok_or_else(|| format!("Browser flag '--{}' requires a value", spec.name))?
            };
            parse_scalar(spec, raw_value)?
        };
        arguments.insert(spec.name.clone(), value);
        index += 1;
    }

    if required_index != required.len() {
        let missing = required[required_index..]
            .iter()
            .map(|spec| spec.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "Browser command '{command}' is missing required positional argument(s): {missing}"
        ));
    }

    for spec in specs {
        if !arguments.contains_key(&spec.name)
            && let Some(default) = spec.default.clone()
        {
            arguments.insert(spec.name.clone(), default);
        }
    }

    validate_url_arguments(command, &arguments)?;
    validate_semantic_arguments(command, &arguments)?;
    Ok(ParsedBrowserInvocation {
        arguments,
        output_format,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn pinned_contract_parses_required_and_optional_arguments() {
        let parsed = parse_browser_cli_invocation(
            "click",
            &strings(&["1_23", "--dbl-click", "--includeSnapshot=false"]),
        )
        .expect("parse click");
        assert_eq!(
            parsed.arguments.get("uid").and_then(Value::as_str),
            Some("1_23")
        );
        assert_eq!(
            parsed.arguments.get("dblClick").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            parsed
                .arguments
                .get("includeSnapshot")
                .and_then(Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn pinned_contract_rejects_unknown_duplicate_and_missing_arguments() {
        assert!(parse_browser_cli_invocation("click", &[]).is_err());
        assert!(parse_browser_cli_invocation("click", &strings(&["1_1", "--wat=1"])).is_err());
        assert!(
            parse_browser_cli_invocation(
                "click",
                &strings(&["1_1", "--dblClick", "--dbl-click=false"]),
            )
            .is_err()
        );
    }

    #[test]
    fn browser_navigation_blocks_host_file_and_internal_schemes() {
        for url in [
            "file:///C:/Users/example/secret.txt",
            "view-source:file:///etc/passwd",
            "C:\\Users\\example\\secret.txt",
            "/etc/passwd",
            "chrome://settings",
            "javascript:document.body.innerText='x'",
        ] {
            let result = parse_browser_cli_invocation("new_page", &strings(&[url]));
            assert!(result.is_err(), "unexpectedly allowed {url}");
        }
        for url in [
            "http://localhost:3000",
            "https://example.com/path",
            "data:text/html,<h1>test</h1>",
            "about:blank",
            "view-source:https://example.com",
            "blob:https://example.com/id",
        ] {
            parse_browser_cli_invocation("new_page", &strings(&[url]))
                .unwrap_or_else(|error| panic!("unexpectedly rejected {url}: {error}"));
        }
    }

    #[test]
    fn performance_trace_deferred_output_is_rejected_before_dispatch() {
        let error = parse_browser_cli_invocation(
            "performance_start_trace",
            &strings(&["--autoStop=false", "--filePath=reports/trace.json"]),
        )
        .expect_err("deferred trace output must be rejected");
        assert!(error.contains("performance_stop_trace"), "{error}");

        parse_browser_cli_invocation("performance_start_trace", &strings(&["--autoStop=false"]))
            .expect("manual trace without start-time file path should remain valid");
        parse_browser_cli_invocation(
            "performance_start_trace",
            &strings(&["--autoStop=true", "--filePath=reports/trace.json"]),
        )
        .expect("auto-stopped trace may write its own output");
    }

    #[test]
    fn output_format_is_moondesk_owned_not_forwarded_to_mcp() {
        let parsed =
            parse_browser_cli_invocation("list_pages", &strings(&["--output-format=json"]))
                .expect("parse output format");
        assert_eq!(parsed.output_format, BrowserOutputFormat::Json);
        assert!(parsed.arguments.get("output-format").is_none());
    }
}
