"""
Production Configuration Management for Omni MCP Platform.
Loads settings from environment variables with sensible production defaults.
"""
from __future__ import annotations
import os
from dataclasses import dataclass, field
from typing import List


@dataclass
class OmniConfig:
    env: str = field(default_factory=lambda: os.getenv("OMNI_ENV", "production"))
    host: str = field(default_factory=lambda: os.getenv("OMNI_HOST", "0.0.0.0"))
    port: int = field(default_factory=lambda: int(os.getenv("OMNI_PORT", "8080")))
    log_level: str = field(default_factory=lambda: os.getenv("OMNI_LOG_LEVEL", "INFO").upper())
    data_dir: str = field(
        default_factory=lambda: os.getenv(
            "OMNI_DATA_DIR",
            os.path.join(os.path.expanduser("~"), ".omni", "data")
        )
    )
    cache_refresh_interval_sec: float = field(
        default_factory=lambda: float(os.getenv("OMNI_CACHE_REFRESH_INTERVAL", "2.0"))
    )
    default_rate_limit_rpm: int = field(
        default_factory=lambda: int(os.getenv("OMNI_DEFAULT_RATE_LIMIT_RPM", "1200"))
    )
    max_body_size_bytes: int = field(
        default_factory=lambda: int(os.getenv("OMNI_MAX_BODY_SIZE_BYTES", str(10 * 1024 * 1024)))
    )
    enable_pii_redaction: bool = field(
        default_factory=lambda: os.getenv("OMNI_ENABLE_PII_REDACTION", "true").lower() in ("true", "1", "yes")
    )
    cors_allowed_origins: str = field(default_factory=lambda: os.getenv("OMNI_CORS_ORIGINS", "*"))
    auth_secret: str = field(default_factory=lambda: os.getenv("OMNI_AUTH_SECRET", "omni_enterprise_secret_key"))


_GLOBAL_CONFIG: OmniConfig = None


def get_config() -> OmniConfig:
    global _GLOBAL_CONFIG
    if _GLOBAL_CONFIG is None:
        _GLOBAL_CONFIG = OmniConfig()
        os.makedirs(_GLOBAL_CONFIG.data_dir, exist_ok=True)
    return _GLOBAL_CONFIG


def set_config(cfg: OmniConfig) -> None:
    global _GLOBAL_CONFIG
    _GLOBAL_CONFIG = cfg

