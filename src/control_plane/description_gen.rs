//! LLM Description Generator - Generates enriched, agent-friendly tool descriptions from IDL schemas and comments.

use crate::control_plane::idl_parser::MethodDefinition;
use std::sync::Arc;

pub type LlmCallback = Arc<dyn Fn(&str) -> String + Send + Sync>;

#[derive(Clone, Default)]
pub struct ToolDescriptionGenerator {
    llm_callable: Option<LlmCallback>,
}

impl ToolDescriptionGenerator {
    pub fn new() -> Self {
        Self { llm_callable: None }
    }

    pub fn with_callable(callable: LlmCallback) -> Self {
        Self {
            llm_callable: Some(callable),
        }
    }

    pub fn generate_description(&self, service_name: &str, method: &MethodDefinition) -> String {
        if let Some(cb) = &self.llm_callable {
            let fields_list: Vec<String> = method
                .fields
                .iter()
                .map(|f| format!("{}:{}", f.name, f.field_type))
                .collect();
            let prompt = format!(
                "Service: {}\nMethod: {}\nDoc: {}\nFields: {:?}\nGenerate a concise, agent-friendly tool description for MCP.",
                service_name, method.name, method.doc_comment, fields_list
            );
            return cb(&prompt);
        }

        let humanized_name = split_camel_case(&method.name);
        let doc = method.doc_comment.trim();
        let mut base = if !doc.is_empty() {
            format!("{}.", doc.trim_end_matches('.'))
        } else {
            format!(
                "Invokes {} on {} to {}.",
                method.name, service_name, humanized_name
            )
        };

        if !method.fields.is_empty() {
            let fields_summary: Vec<String> = method
                .fields
                .iter()
                .map(|f| format!("`{}` ({})", f.name, f.field_type))
                .collect();
            base.push_str(&format!(
                " Accepts parameters: {}.",
                fields_summary.join(", ")
            ));
        }

        base.push_str(&format!(
            " Routed over {} via Uber MCP Gateway.",
            method.protocol.as_str()
        ));
        base
    }
}

fn split_camel_case(ident: &str) -> String {
    let mut words = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = ident.chars().collect();
    for i in 0..chars.len() {
        let c = chars[i];
        if c == '_' || c == '-' {
            if !current.is_empty() {
                words.push(current.to_lowercase());
                current.clear();
            }
            continue;
        }
        if c.is_uppercase() && !current.is_empty() {
            let prev_upper = i > 0 && chars[i - 1].is_uppercase();
            let next_lower = i + 1 < chars.len() && chars[i + 1].is_lowercase();
            if !prev_upper || next_lower {
                words.push(current.to_lowercase());
                current.clear();
            }
        }
        current.push(c);
    }
    if !current.is_empty() {
        words.push(current.to_lowercase());
    }
    words.join(" ")
}
