"""
Production Rate Limiter - Token bucket and sliding-window rate limiting per caller actor and token.
"""
from __future__ import annotations
import threading
import time
from typing import Dict, Tuple


class RateLimiter:
    """Thread-safe token bucket rate limiter."""

    def __init__(self, default_rpm: int = 1200) -> None:
        self.default_rpm = default_rpm
        self._lock = threading.RLock()
        self._buckets: Dict[str, Tuple[float, float]] = {}  # key -> (current_tokens, last_refreshed_time)

    def check_rate_limit(self, key: str, max_rpm: int = 0) -> Tuple[bool, float]:
        """
        Returns (is_allowed, remaining_tokens).
        """
        rpm = max_rpm if max_rpm > 0 else self.default_rpm
        capacity = float(rpm)
        fill_rate = capacity / 60.0  # tokens per second
        now = time.time()

        with self._lock:
            if key not in self._buckets:
                self._buckets[key] = (capacity - 1.0, now)
                return True, capacity - 1.0

            tokens, last_time = self._buckets[key]
            elapsed = now - last_time
            # Refill tokens
            tokens = min(capacity, tokens + elapsed * fill_rate)

            if tokens >= 1.0:
                tokens -= 1.0
                self._buckets[key] = (tokens, now)
                return True, tokens
            else:
                self._buckets[key] = (tokens, now)
                return False, 0.0

    def reset(self) -> None:
        with self._lock:
            self._buckets.clear()

