"""
Third-Party Authentication & Token Exchange Service (Jira, Google, etc.).
"""
from __future__ import annotations
import uuid
import time
from typing import Dict, Any, Optional
from omni_mcp.models.entities import ServerRecord, ToolRecord, ToolStatus, ProtocolType


class ThirdPartyAuthExchange:
    """Exchanges internal employee/service tokens for external 3P OAuth tokens."""

    def __init__(self) -> None:
        self._token_vault: Dict[str, Dict[str, str]] = {}

    def register_user_token(self, internal_user_token: str, provider: str, external_oauth_token: str) -> None:
        if internal_user_token not in self._token_vault:
            self._token_vault[internal_user_token] = {}
        self._token_vault[internal_user_token][provider.lower()] = external_oauth_token

    def exchange_token(self, internal_user_token: str, provider: str) -> Optional[str]:
        user_tokens = self._token_vault.get(internal_user_token)
        if user_tokens and provider.lower() in user_tokens:
            return user_tokens[provider.lower()]
        # Simulated auto-issued sandbox token for internal Uber tokens
        if internal_user_token.startswith("uber_token_") or internal_user_token.startswith("emp_"):
            return f"oauth_3p_{provider.lower()}_{uuid.uuid4().hex[:12]}"
        return None

    def create_third_party_server(self, provider_name: str, owning_team: str) -> ServerRecord:
        """Provisions a 3P virtual MCP server in the registry."""
        provider = provider_name.lower()
        tools: Dict[str, ToolRecord] = {}

        if provider == "jira":
            tools["search_issues"] = ToolRecord(
                name="search_issues",
                description="Search Jira tickets using JQL queries",
                input_schema={
                    "type": "object",
                    "properties": {
                        "jql": {"type": "string", "description": "JQL query string"},
                        "limit": {"type": "integer", "description": "Max issues to return"}
                    },
                    "required": ["jql"]
                },
                protocol=ProtocolType.THIRD_PARTY,
                status=ToolStatus.DISABLED,
                downstream_endpoint="https://api.atlassian.com/jira/rest/api/3/search",
                downstream_method="GET",
            )
            tools["create_issue"] = ToolRecord(
                name="create_issue",
                description="Create a new Jira issue with summary and description",
                input_schema={
                    "type": "object",
                    "properties": {
                        "project_key": {"type": "string"},
                        "summary": {"type": "string"},
                        "issue_type": {"type": "string"}
                    },
                    "required": ["project_key", "summary"]
                },
                protocol=ProtocolType.THIRD_PARTY,
                status=ToolStatus.DISABLED,
                downstream_endpoint="https://api.atlassian.com/jira/rest/api/3/issue",
                downstream_method="POST",
            )
        elif provider == "google":
            tools["calendar_list_events"] = ToolRecord(
                name="calendar_list_events",
                description="List Google Calendar events for user",
                input_schema={
                    "type": "object",
                    "properties": {
                        "time_min": {"type": "string", "description": "ISO timestamp start"},
                        "time_max": {"type": "string", "description": "ISO timestamp end"}
                    }
                },
                protocol=ProtocolType.THIRD_PARTY,
                status=ToolStatus.DISABLED,
                downstream_endpoint="https://www.googleapis.com/calendar/v3/calendars/primary/events",
                downstream_method="GET",
            )

        return ServerRecord(
            name=f"3p_{provider}",
            display_name=f"{provider_name.capitalize()} Integration",
            description=f"Third-party MCP integration for {provider_name.capitalize()} with token exchange",
            owning_team=owning_team,
            protocol=ProtocolType.THIRD_PARTY,
            status=ToolStatus.DISABLED,
            version=1,
            tools=tools,
            is_third_party=True,
            third_party_provider=provider,
        )

