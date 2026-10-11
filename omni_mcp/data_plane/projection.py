"""
Response Projection Engine - GraphQL-like field-level projection pattern for MCP tool responses.
"""
from __future__ import annotations
from typing import Any, Dict, List, Optional


class ResponseProjectionEngine:
    """Trims runtime JSON responses by retaining only explicitly projected nested fields."""

    @classmethod
    def apply_projection(cls, response_data: Any, projection_paths: Optional[List[str]]) -> Any:
        if not projection_paths:
            return response_data
        if not isinstance(response_data, dict):
            return response_data

        result: Dict[str, Any] = {}
        for path in projection_paths:
            parts = path.strip().split(".")
            cls._extract_nested_path(response_data, parts, result)

        return result

    @classmethod
    def _extract_nested_path(cls, source: Any, parts: List[str], target: Dict[str, Any]) -> None:
        if not parts or not isinstance(source, dict):
            return

        current_key = parts[0]
        # Handle array indexing notation, e.g. "trips[]" or "items[].id"
        is_array = current_key.endswith("[]")
        clean_key = current_key[:-2] if is_array else current_key

        if clean_key not in source:
            return

        source_val = source[clean_key]

        if len(parts) == 1:
            # Leaf field
            target[clean_key] = source_val
            return

        # Intermediate field
        remaining_parts = parts[1:]
        if is_array:
            if not isinstance(source_val, list):
                return
            if clean_key not in target:
                target[clean_key] = []

            # Project each element in array
            for idx, item in enumerate(source_val):
                if idx >= len(target[clean_key]):
                    target[clean_key].append({})
                if isinstance(item, dict):
                    cls._extract_nested_path(item, remaining_parts, target[clean_key][idx])
                else:
                    target[clean_key][idx] = item
        else:
            if clean_key not in target or not isinstance(target[clean_key], dict):
                target[clean_key] = {}
            cls._extract_nested_path(source_val, remaining_parts, target[clean_key])

