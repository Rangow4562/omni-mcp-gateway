//! Security Engine: ABAC charter policies (humans, services, agents) and high-speed DFA PII redaction.

use crate::json::JsonValue;
use crate::models::{CharterPolicy, ServerRecord, ToolRecord};

pub struct PIIRedactor;

impl PIIRedactor {
    /// Out-of-the-box zero-allocation-friendly scanner for PII, secrets, and sensitive data patterns.
    pub fn redact_text(input: &str) -> String {
        let s1 = redact_emails(input);
        let s2 = redact_bearer_tokens(&s1);
        let s3 = redact_secrets(&s2);
        let s4 = redact_credit_cards(&s3);
        let s5 = redact_ssns(&s4);
        redact_phones(&s5)
    }

    pub fn redact_payload(data: &JsonValue) -> JsonValue {
        match data {
            JsonValue::Object(entries) => {
                let mut out = Vec::with_capacity(entries.len());
                for (k, v) in entries {
                    out.push((k.clone(), Self::redact_payload(v)));
                }
                JsonValue::Object(out)
            }
            JsonValue::Array(items) => {
                JsonValue::Array(items.iter().map(Self::redact_payload).collect())
            }
            JsonValue::Str(s) => JsonValue::Str(Self::redact_text(s)),
            other => other.clone(),
        }
    }
}

fn is_email_user_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'%' | b'+' | b'-')
}

fn is_email_domain_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-')
}

fn redact_emails(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut cursor = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'@' && i > 0 && i + 3 < bytes.len() {
            let mut start = i;
            while start > cursor && is_email_user_char(bytes[start - 1]) {
                start -= 1;
            }
            let mut end = i + 1;
            while end < bytes.len() && is_email_domain_char(bytes[end]) {
                end += 1;
            }
            // Trim trailing dot
            while end > i + 1 && bytes[end - 1] == b'.' {
                end -= 1;
            }
            let domain = &s[i + 1..end];
            if start < i && domain.contains('.') {
                if let Some((_, tld)) = domain.rsplit_once('.') {
                    if tld.len() >= 2 && tld.bytes().all(|b| b.is_ascii_alphabetic()) {
                        out.push_str(&s[cursor..start]);
                        out.push_str("[REDACTED_EMAIL]");
                        cursor = end;
                        i = end;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }
    out.push_str(&s[cursor..]);
    out
}

fn redact_bearer_tokens(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut cursor = 0usize;
    let mut i = 0usize;

    while i + 7 <= bytes.len() {
        if s[i..i + 7].eq_ignore_ascii_case("bearer ") {
            let prefix_end = i + 7;
            let mut tok_start = prefix_end;
            while tok_start < bytes.len() && bytes[tok_start] == b' ' {
                tok_start += 1;
            }
            let mut tok_end = tok_start;
            while tok_end < bytes.len()
                && (bytes[tok_end].is_ascii_alphanumeric()
                    || matches!(bytes[tok_end], b'_' | b'-' | b'.'))
            {
                tok_end += 1;
            }
            if tok_end - tok_start >= 20 {
                out.push_str(&s[cursor..tok_start]);
                out.push_str("[REDACTED_TOKEN]");
                cursor = tok_end;
                i = tok_end;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&s[cursor..]);
    out
}

fn redact_secrets(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut cursor = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        // AWS Key: AKIA... (20 chars total)
        if i + 20 <= bytes.len() && &s[i..i + 4] == "AKIA" {
            let is_aws = s[i + 4..i + 20]
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
            if is_aws {
                out.push_str(&s[cursor..i]);
                out.push_str("[REDACTED_AWS_KEY]");
                cursor = i + 20;
                i = cursor;
                continue;
            }
        }

        // OpenAI / Anthropic / GitHub / NPM API Keys (sk-..., ghp_..., gho_..., npm_...)
        for prefix in &["sk-", "ghp_", "gho_", "npm_"] {
            let p_len = prefix.len();
            if i + p_len < bytes.len() && &s[i..i + p_len] == *prefix {
                let mut end = i + p_len;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'_' | b'-'))
                {
                    end += 1;
                }
                if end - (i + p_len) >= 16 {
                    out.push_str(&s[cursor..i]);
                    out.push_str("[REDACTED_API_KEY]");
                    cursor = end;
                    i = end;
                    break;
                }
            }
        }
        i += 1;
    }
    out.push_str(&s[cursor..]);
    out
}

fn redact_credit_cards(s: &str) -> String {
    // Matches 4 groups of 4 digits separated by ' ' or '-' or contiguous
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut cursor = 0usize;
    let mut i = 0usize;

    while i + 16 <= bytes.len() {
        let prev_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        if prev_ok && bytes[i].is_ascii_digit() {
            if let Some(end) = match_credit_card_at(bytes, i) {
                let next_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
                if next_ok {
                    out.push_str(&s[cursor..i]);
                    out.push_str("[REDACTED_CARD]");
                    cursor = end;
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }
    out.push_str(&s[cursor..]);
    out
}

fn match_credit_card_at(bytes: &[u8], start: usize) -> Option<usize> {
    let mut pos = start;
    for group in 0..4 {
        for _ in 0..4 {
            if pos >= bytes.len() || !bytes[pos].is_ascii_digit() {
                return None;
            }
            pos += 1;
        }
        if group < 3 && pos < bytes.len() && matches!(bytes[pos], b' ' | b'-') {
            pos += 1;
        }
    }
    Some(pos)
}

fn redact_ssns(s: &str) -> String {
    // Matches \b\d{3}-\d{2}-\d{4}\b
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut cursor = 0usize;
    let mut i = 0usize;

    while i + 11 <= bytes.len() {
        let prev_ok = i == 0 || (!bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'-');
        if prev_ok
            && bytes[i..i + 3].iter().all(|b| b.is_ascii_digit())
            && bytes[i + 3] == b'-'
            && bytes[i + 4..i + 6].iter().all(|b| b.is_ascii_digit())
            && bytes[i + 6] == b'-'
            && bytes[i + 7..i + 11].iter().all(|b| b.is_ascii_digit())
        {
            let end = i + 11;
            let next_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
            if next_ok {
                out.push_str(&s[cursor..i]);
                out.push_str("[REDACTED_SSN]");
                cursor = end;
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&s[cursor..]);
    out
}

fn redact_phones(s: &str) -> String {
    // Matches (?:\+?\d{1,3}[-.\s]?)?\(?\d{3}\)?[-.\s]?\d{3}[-.\s]?\d{4}
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut cursor = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        let prev_ok = i == 0 || (!bytes[i - 1].is_ascii_alphanumeric());
        if prev_ok && (bytes[i] == b'+' || bytes[i] == b'(' || bytes[i].is_ascii_digit()) {
            if let Some(end) = match_phone_at(bytes, i) {
                let next_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
                if next_ok {
                    out.push_str(&s[cursor..i]);
                    out.push_str("[REDACTED_PHONE]");
                    cursor = end;
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }
    out.push_str(&s[cursor..]);
    out
}

fn match_phone_at(bytes: &[u8], start: usize) -> Option<usize> {
    let mut pos = start;
    // Try with optional country code first, then without
    if let Some(end) = try_phone_country(bytes, pos).or_else(|| try_phone_local(bytes, pos)) {
        return Some(end);
    }
    pos = start;
    let _ = pos;
    None
}

fn try_phone_country(bytes: &[u8], start: usize) -> Option<usize> {
    let mut pos = start;
    if pos < bytes.len() && bytes[pos] == b'+' {
        pos += 1;
    }
    let mut cc_digits = 0;
    while pos < bytes.len() && bytes[pos].is_ascii_digit() && cc_digits < 3 {
        pos += 1;
        cc_digits += 1;
    }
    if cc_digits == 0 {
        return None;
    }
    if pos < bytes.len() && matches!(bytes[pos], b'-' | b'.' | b' ') {
        pos += 1;
    } else {
        return None;
    }
    try_phone_local(bytes, pos)
}

fn try_phone_local(bytes: &[u8], start: usize) -> Option<usize> {
    let mut pos = start;
    let has_open_paren = if pos < bytes.len() && bytes[pos] == b'(' {
        pos += 1;
        true
    } else {
        false
    };
    for _ in 0..3 {
        if pos >= bytes.len() || !bytes[pos].is_ascii_digit() {
            return None;
        }
        pos += 1;
    }
    if has_open_paren {
        if pos >= bytes.len() || bytes[pos] != b')' {
            return None;
        }
        pos += 1;
    }
    if pos < bytes.len() && matches!(bytes[pos], b'-' | b'.' | b' ') {
        pos += 1;
    }
    for _ in 0..3 {
        if pos >= bytes.len() || !bytes[pos].is_ascii_digit() {
            return None;
        }
        pos += 1;
    }
    if pos < bytes.len() && matches!(bytes[pos], b'-' | b'.' | b' ') {
        pos += 1;
    }
    for _ in 0..4 {
        if pos >= bytes.len() || !bytes[pos].is_ascii_digit() {
            return None;
        }
        pos += 1;
    }
    Some(pos)
}

pub struct SecurityCharterEngine;

impl SecurityCharterEngine {
    /// Evaluates ABAC charter policies at server and tool level based on caller actor.
    pub fn evaluate_access(
        server: &ServerRecord,
        tool: &ToolRecord,
        caller_actor: &str,
        _user_token: Option<&str>,
    ) -> (bool, Option<String>) {
        let effective_policy: &CharterPolicy =
            tool.charter_policy.as_ref().unwrap_or(&server.charter_policy);

        if !effective_policy.is_actor_allowed(caller_actor) {
            return (
                false,
                Some(format!(
                    "Actor '{}' is not authorized to invoke tool '{}' under Charter Policy",
                    caller_actor, tool.name
                )),
            );
        }

        if !effective_policy.is_tool_allowed(&tool.name) {
            return (
                false,
                Some(format!(
                    "Tool '{}' is restricted by Charter Policy on server '{}'",
                    tool.name, server.name
                )),
            );
        }

        (true, None)
    }
}

