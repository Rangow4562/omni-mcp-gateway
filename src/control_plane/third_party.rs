//! Third-Party Authentication & Token Exchange Service (Jira, Google, GitHub, Slack).

use crate::logging::now_epoch_millis;
use crate::models::{ProtocolType, ServerRecord, ToolRecord, ToolStatus};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

#[derive(Clone, Default)]
pub struct ThirdPartyAuthExchange {
    token_vault: Arc<RwLock<HashMap<String, HashMap<String, String>>>>,
}

impl ThirdPartyAuthExchange {
    pub fn new() -> Self {
        Self {
            token_vault: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn register_user_token(
        &self,
        internal_user_token: &str,
        provider: &str,
        external_oauth_token: &str,
    ) {
        let mut guard = self.token_vault.write().unwrap();
        guard
            .entry(internal_user_token.to_string())
            .or_default()
            .insert(provider.to_lowercase(), external_oauth_token.to_string());
    }

    pub fn exchange_token(&self, internal_user_token: &str, provider: &str) -> Option<String> {
        let provider_lc = provider.to_lowercase();
        {
            let guard = self.token_vault.read().unwrap();
            if let Some(user_tokens) = guard.get(internal_user_token) {
                if let Some(tok) = user_tokens.get(&provider_lc) {
                    return Some(tok.clone());
                }
            }
        }
        if internal_user_token.starts_with("uber_token_") || internal_user_token.starts_with("emp_")
        {
            return Some(format!(
                "oauth_3p_{}_{:012x}",
                provider_lc,
                now_epoch_millis()
            ));
        }
        None
    }

    pub fn create_third_party_server(&self, provider_name: &str, owning_team: &str) -> ServerRecord {
        let provider = provider_name.to_lowercase();
        let mut tools = BTreeMap::new();

        if provider == "jira" {
            let mut search = ToolRecord::new(
                "search_issues",
                "Search Jira tickets using JQL queries",
                json!({
                    "type": "object",
                    "properties": {
                        "jql": {"type": "string", "description": "JQL query string"},
                        "limit": {"type": "integer", "description": "Max issues to return"}
                    },
                    "required": ["jql"]
                }),
            );
            search.protocol = ProtocolType::ThirdParty;
            search.status = ToolStatus::Disabled;
            search.downstream_endpoint =
                "https://api.atlassian.com/jira/rest/api/3/search".to_string();
            search.downstream_method = "GET".to_string();
            search.metadata = json!({"provider": "jira"});
            tools.insert("search_issues".to_string(), search);

            let mut create = ToolRecord::new(
                "create_issue",
                "Create a new Jira issue with summary and description",
                json!({
                    "type": "object",
                    "properties": {
                        "project_key": {"type": "string"},
                        "summary": {"type": "string"},
                        "issue_type": {"type": "string"}
                    },
                    "required": ["project_key", "summary"]
                }),
            );
            create.protocol = ProtocolType::ThirdParty;
            create.status = ToolStatus::Disabled;
            create.downstream_endpoint =
                "https://api.atlassian.com/jira/rest/api/3/issue".to_string();
            create.downstream_method = "POST".to_string();
            create.metadata = json!({"provider": "jira"});
            tools.insert("create_issue".to_string(), create);
        } else if provider == "google" {
            let mut cal = ToolRecord::new(
                "calendar_list_events",
                "List Google Calendar events for user",
                json!({
                    "type": "object",
                    "properties": {
                        "time_min": {"type": "string", "description": "ISO timestamp start"},
                        "time_max": {"type": "string", "description": "ISO timestamp end"}
                    }
                }),
            );
            cal.protocol = ProtocolType::ThirdParty;
            cal.status = ToolStatus::Disabled;
            cal.downstream_endpoint =
                "https://www.googleapis.com/calendar/v3/calendars/primary/events".to_string();
            cal.downstream_method = "GET".to_string();
            cal.metadata = json!({"provider": "google"});
            tools.insert("calendar_list_events".to_string(), cal);
        }

        let capitalized = capitalize_word(&provider);
        let mut srv = ServerRecord::new(
            format!("3p_{}", provider),
            format!("{} Integration", capitalized),
            format!(
                "Third-party MCP integration for {} with token exchange",
                capitalized
            ),
            owning_team,
        );
        srv.protocol = ProtocolType::ThirdParty;
        srv.status = ToolStatus::Disabled;
        srv.tools = tools;
        srv.is_third_party = true;
        srv.third_party_provider = Some(provider);
        srv
    }
}

fn capitalize_word(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
    }
}
