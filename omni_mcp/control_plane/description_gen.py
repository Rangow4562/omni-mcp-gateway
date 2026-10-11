"""
LLM Description Generator - Generates enriched, agent-friendly tool descriptions from IDL schemas and comments.
"""
from __future__ import annotations
from typing import Optional, Callable
from omni_mcp.control_plane.idl_parser import MethodDefinition


class ToolDescriptionGenerator:
    """Enriches raw IDL method names and schemas into descriptive instructions for AI agents."""

    def __init__(self, llm_callable: Optional[Callable[[str], str]] = None) -> None:
        self._llm_callable = llm_callable

    def generate_description(self, service_name: str, method: MethodDefinition) -> str:
        if self._llm_callable:
            prompt = (
                f"Service: {service_name}\n"
                f"Method: {method.name}\n"
                f"Doc: {method.doc_comment}\n"
                f"Fields: {[f.name + ':' + f.field_type for f in method.fields]}\n"
                "Generate a concise, agent-friendly tool description for MCP."
            )
            return self._llm_callable(prompt)

        # High-quality heuristic enrichment
        humanized_name = " ".join(
            word.lower() for word in
            __import__("re").findall(r"[A-Z]?[a-z]+|[A-Z]+(?=[A-Z][a-z]|\d|\W|$)|\d+", method.name)
        )

        doc = method.doc_comment.strip()
        if doc:
            base = f"{doc}."
        else:
            base = f"Invokes {method.name} on {service_name} to {humanized_name}."

        fields_summary = ", ".join(f"`{f.name}` ({f.field_type})" for f in method.fields)
        if fields_summary:
            base += f" Accepts parameters: {fields_summary}."

        base += f" Routed over {method.protocol.value} via Uber MCP Gateway."
        return base

