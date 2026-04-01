use serde_json::Value;

/// Converts a raw stream-json NDJSON line to zero or more human-readable display lines.
/// Returns empty vec for lines that should be suppressed (e.g. session metadata).
pub fn format_stream_line(raw: &str) -> Vec<String> {
    let Ok(val): Result<Value, _> = serde_json::from_str(raw) else {
        // Not JSON — show as-is
        return vec![raw.to_string()];
    };

    let event_type = val["type"].as_str().unwrap_or("");
    let subtype = val["subtype"].as_str().unwrap_or("");

    match event_type {
        "system" => match subtype {
            "init" => {
                let model = val["model"].as_str().unwrap_or("unknown");
                vec![format!("[Session] Using model: {}", model)]
            }
            "compact_boundary" => vec!["[Context] Conversation compacted".to_string()],
            _ => vec![], // suppress other system events
        },

        "assistant" => {
            let mut lines = Vec::new();
            if let Some(content) = val["message"]["content"].as_array() {
                for block in content {
                    match block["type"].as_str().unwrap_or("") {
                        "text" => {
                            let text = block["text"].as_str().unwrap_or("").trim();
                            if !text.is_empty() {
                                // Prefix each line of multi-line text
                                for line in text.lines() {
                                    lines.push(format!("[Claude] {}", line));
                                }
                            }
                        }
                        "tool_use" => {
                            let name = block["name"].as_str().unwrap_or("?");
                            let input = &block["input"];
                            let summary = summarize_tool_input(name, input);
                            lines.push(format!("[Tool: {}] {}", name, summary));
                        }
                        _ => {}
                    }
                }
            }
            lines
        }

        "user" => {
            let mut lines = Vec::new();
            if let Some(content) = val["message"]["content"].as_array() {
                for block in content {
                    if block["type"].as_str() == Some("tool_result") {
                        let output = extract_tool_result_text(block);
                        if !output.is_empty() {
                            // Show first 5 lines of tool output, truncate rest
                            let all_lines: Vec<&str> = output.lines().collect();
                            let limit = 5;
                            for line in all_lines.iter().take(limit) {
                                lines.push(format!("  -> {}", line));
                            }
                            if all_lines.len() > limit {
                                lines.push(format!("  -> ... ({} more lines)", all_lines.len() - limit));
                            }
                        }
                    }
                }
            }
            lines
        }

        "tool_progress" => {
            let name = val["tool_name"].as_str().unwrap_or("?");
            let secs = val["elapsed_time_seconds"].as_f64().unwrap_or(0.0);
            vec![format!("[{} running] ({:.1}s)...", name, secs)]
        }

        "result" => {
            let cost = val["total_cost_usd"].as_f64().unwrap_or(0.0);
            let ms = val["duration_ms"].as_u64().unwrap_or(0);
            let input = val["usage"]["input_tokens"].as_u64().unwrap_or(0);
            let output = val["usage"]["output_tokens"].as_u64().unwrap_or(0);
            let total_tokens = input + output;

            match subtype {
                "success" => vec![format!(
                    "[OK] Completed in {}s -- ${:.4} ({} tokens)",
                    ms / 1000, cost, total_tokens
                )],
                other => {
                    let errors = val["errors"]
                        .as_array()
                        .map(|e| e.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join("; "))
                        .unwrap_or_default();
                    vec![format!("[FAIL] Failed ({}) -- {}", other, errors)]
                }
            }
        }

        _ => vec![], // suppress unknown types
    }
}

/// Returns a substring of `s` that is at most `max_chars` Unicode scalar values long,
/// always ending on a valid UTF-8 character boundary.
fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// Produces a short one-line summary of a tool call's input for display.
fn summarize_tool_input(tool_name: &str, input: &Value) -> String {
    match tool_name {
        "Bash" => input["command"].as_str().unwrap_or("").to_string(),
        "Read" => input["file_path"].as_str().unwrap_or("").to_string(),
        "Write" => {
            let path = input["file_path"].as_str().unwrap_or("?");
            let content_len = input["content"].as_str().map(|s| s.len()).unwrap_or(0);
            format!("{} ({} bytes)", path, content_len)
        }
        "Edit" => {
            let path = input["file_path"].as_str().unwrap_or("?");
            format!("{}", path)
        }
        "Glob" => {
            let pattern = input["pattern"].as_str().unwrap_or("?");
            format!("{}", pattern)
        }
        "Grep" => {
            let pattern = input["pattern"].as_str().unwrap_or("?");
            let path = input["path"].as_str().unwrap_or(".");
            format!("{} in {}", pattern, path)
        }
        "Agent" => {
            let desc = input["description"].as_str().unwrap_or("?");
            format!("{}", truncate_chars(desc, 60))
        }
        "WebSearch" => input["query"].as_str().unwrap_or("?").to_string(),
        "WebFetch" => input["url"].as_str().unwrap_or("?").to_string(),
        _ => {
            // Generic: show first string field value or JSON truncated
            if let Some(obj) = input.as_object() {
                if let Some((_, v)) = obj.iter().next() {
                    if let Some(s) = v.as_str() {
                        return truncate_chars(s, 80).to_string();
                    }
                }
            }
            let json = serde_json::to_string(input).unwrap_or_default();
            truncate_chars(&json, 80).to_string()
        }
    }
}

/// Extracts text content from a tool_result block.
fn extract_tool_result_text(block: &Value) -> String {
    // content can be a string or array of blocks
    if let Some(s) = block["content"].as_str() {
        return s.to_string();
    }
    if let Some(arr) = block["content"].as_array() {
        return arr.iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}
