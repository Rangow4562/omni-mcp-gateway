"""
In-Memory Config Cache - High-performance data plane state refreshed at fixed cadence.
"""
from __future__ import annotations
import copy
import threading
import time
from typing import Dict, Optional, List
from omni_mcp.models.entities import ServerRecord
from omni_mcp.control_plane.registry import MCPRegistry


class InMemoryConfigCache:
    """Consumes configuration from the Control Plane at a fixed cadence to allow real-time runtime updates without restarts."""

    def __init__(self, registry: MCPRegistry, refresh_interval_seconds: float = 5.0) -> None:
        self.registry = registry
        self.refresh_interval = refresh_interval_seconds
        self._lock = threading.RLock()
        self._cache: Dict[str, ServerRecord] = {}
        self._last_refresh_timestamp: float = 0.0
        self._running = False
        self._thread: Optional[threading.Thread] = None
        # Initial sync
        self.refresh_now()

    def refresh_now(self) -> None:
        """Explicitly refresh the in-memory cache from the control plane registry."""
        with self._lock:
            all_servers = self.registry.list_servers()
            new_cache: Dict[str, ServerRecord] = {}
            for s in all_servers:
                new_cache[s.name] = s
            self._cache = new_cache
            self._last_refresh_timestamp = time.time()

    def get_server(self, server_name: str) -> Optional[ServerRecord]:
        with self._lock:
            server = self._cache.get(server_name)
            return copy.deepcopy(server) if server else None

    def list_servers(self) -> List[ServerRecord]:
        with self._lock:
            return [copy.deepcopy(s) for s in self._cache.values()]

    def start_background_refresh(self) -> None:
        self._running = True
        self._thread = threading.Thread(target=self._loop, daemon=True)
        self._thread.start()

    def stop_background_refresh(self) -> None:
        self._running = False
        if self._thread and self._thread.is_alive():
            self._thread.join(timeout=1.0)

    def _loop(self) -> None:
        while self._running:
            time.sleep(self.refresh_interval)
            try:
                self.refresh_now()
            except Exception:
                pass

