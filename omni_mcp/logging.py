"""
Structured Production Logging and Telemetry for Omni MCP.
Emits structured JSON log events for high observability and audit compliance.
"""
from __future__ import annotations
import json
import logging
import sys
import time
from typing import Any, Dict, Optional


class JSONFormatter(logging.Formatter):
    def format(self, record: logging.LogRecord) -> str:
        log_obj: Dict[str, Any] = {
            "timestamp": self.formatTime(record, self.datefmt) if self.datefmt else time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(record.created)),
            "level": record.levelname,
            "logger": record.name,
            "message": record.getMessage(),
        }

        # Include custom extra metadata if present
        if hasattr(record, "trace_id"):
            log_obj["trace_id"] = record.trace_id
        if hasattr(record, "actor"):
            log_obj["actor"] = record.actor
        if hasattr(record, "service"):
            log_obj["service"] = record.service
        if hasattr(record, "duration_ms"):
            log_obj["duration_ms"] = record.duration_ms
        if hasattr(record, "status_code"):
            log_obj["status_code"] = record.status_code

        if record.exc_info:
            log_obj["exception"] = self.formatException(record.exc_info)

        return json.dumps(log_obj)


def setup_logger(name: str = "omni_mcp", level: str = "INFO") -> logging.Logger:
    logger = logging.getLogger(name)
    logger.setLevel(getattr(logging, level.upper(), logging.INFO))
    if not logger.handlers:
        handler = logging.StreamHandler(sys.stdout)
        handler.setFormatter(JSONFormatter())
        logger.addHandler(handler)
    return logger


logger = setup_logger()

