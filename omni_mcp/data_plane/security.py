"""
Security Engine: ABAC charter policies (humans, services, agents) and out-of-the-box PII redaction.
"""
from __future__ import annotations
import re
from typing import Dict, Any, Optional, Tuple
from omni_mcp.models.entities import (
    ActorType,
    CharterPolicy,
    ServerRecord,
    ToolRecord,
)


class PIIRedactor:
    """Out-of-the-box redaction for PII and sensitive data patterns in tool responses."""

    EMAIL_REGEX = re.compile(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Z|a-z]{2,}\b")
    PHONE_REGEX = re.compile(r"\b(?:\+?\d{1,3}[-.\s]?)?\(?\d{3}\)?[-.\s]?\d{3}[-.\s]?\d{4}\b")
    SSN_REGEX = re.compile(r"\b\d{3}-\d{2}-\d{4}\b")
    CREDIT_CARD_REGEX = re.compile(r"\b(?:\d{4}[-\s]?){3}\d{4}\b")
    BEARER_TOKEN_REGEX = re.compile(r"(Bearer\s+)[A-Za-z0-9_\-\.]{20,}", re.IGNORECASE)

    @classmethod
    def redact_text(cls, text: str) -> str:
        if not isinstance(text, str):
            return text
        text = cls.EMAIL_REGEX.sub("[REDACTED_EMAIL]", text)
        text = cls.PHONE_REGEX.sub("[REDACTED_PHONE]", text)
        text = cls.SSN_REGEX.sub("[REDACTED_SSN]", text)
        text = cls.CREDIT_CARD_REGEX.sub("[REDACTED_CARD]", text)
        text = cls.BEARER_TOKEN_REGEX.sub(r"\1[REDACTED_TOKEN]", text)
        return text

    @classmethod
    def redact_payload(cls, data: Any) -> Any:
        if isinstance(data, dict):
            return {k: cls.redact_payload(v) for k, v in data.items()}
        elif isinstance(data, list):
            return [cls.redact_payload(v) for v in data]
        elif isinstance(data, str):
            return cls.redact_text(data)
        return data


class SecurityCharterEngine:
    """Evaluates ABAC charter policies at server and tool level based on caller actor."""

    @classmethod
    def evaluate_access(
        cls,
        server: ServerRecord,
        tool: ToolRecord,
        caller_actor: str,
        user_token: Optional[str] = None,
    ) -> Tuple[bool, Optional[str]]:
        """
        Determines whether caller is authorized based on server and tool policies.
        Returns: (is_allowed, error_reason)
        """
        # Determine effective policy: tool override takes precedence over server policy
        effective_policy: CharterPolicy = tool.charter_policy or server.charter_policy

        if not effective_policy.is_actor_allowed(caller_actor):
            return False, f"Actor '{caller_actor}' is not authorized to invoke tool '{tool.name}' under Charter Policy"

        if not effective_policy.is_tool_allowed(tool.name):
            return False, f"Tool '{tool.name}' is restricted by Charter Policy on server '{server.name}'"

        return True, None

