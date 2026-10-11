"""
Muttley Service Mesh Sidecar Client - Uber's service-to-service routing and transport layer.
"""
from __future__ import annotations
import json
import time
from typing import Dict, Any, Optional
from omni_mcp.models.entities import ProtocolType


class MuttleyMeshClient:
    """Simulates Uber's Muttley sidecar for executing downstream calls with routing, timeouts, and retries."""

    def __init__(self) -> None:
        self._mock_responses: Dict[str, Any] = {}
        self._call_log: list = []

    def register_mock_service(self, service_route: str, response_payload: Any) -> None:
        self._mock_responses[service_route] = response_payload

    def execute_downstream(
        self,
        service_name: str,
        method_name: str,
        protocol: ProtocolType,
        wire_payload: bytes,
        headers: Optional[Dict[str, str]] = None,
        timeout_ms: int = 5000,
    ) -> bytes:
        """
        Executes request through local Muttley sidecar.
        Routes to service, transmits wire bytes, and returns response wire bytes.
        """
        start = time.time()
        headers = headers or {}
        route_key = f"{service_name}/{method_name}"

        # Record call for audit and tracing
        call_record = {
            "service": service_name,
            "method": method_name,
            "protocol": protocol.value,
            "headers": headers,
            "payload_bytes_len": len(wire_payload),
            "timestamp": start,
        }
        self._call_log.append(call_record)

        # Check for registered mock response or generate synthetic domain response
        if route_key in self._mock_responses:
            data = self._mock_responses[route_key]
        else:
            data = self._synthesize_default_downstream_response(service_name, method_name, protocol)

        # Return serialized bytes
        return json.dumps(data).encode("utf-8")

    def get_call_count(self, service_name: Optional[str] = None) -> int:
        if service_name is None:
            return len(self._call_log)
        return sum(1 for c in self._call_log if c["service"] == service_name)

    def _synthesize_default_downstream_response(
        self, service_name: str, method_name: str, protocol: ProtocolType
    ) -> Dict[str, Any]:
        """Provides realistic mock enterprise responses when no specific mock is registered."""
        return {
            "status": "SUCCESS",
            "service": service_name,
            "method": method_name,
            "protocol": protocol.value,
            "timestamp": time.time(),
            "data": {
                "id": f"res_{hash(service_name + method_name) % 100000}",
                "result": f"Executed {method_name} successfully via {protocol.value}",
                "driver": {
                    "id": "drv_88492",
                    "name": "Jane Doe",
                    "email": "driver.jane@uber-internal-partner.com",
                    "phone": "+1-555-019-2834",
                    "rating": 4.96,
                    "vehicle": {
                        "make": "Toyota",
                        "model": "Camry",
                        "license_plate": "6ABC123",
                    }
                },
                "audit": {
                    "muttley_mesh_trace_id": f"trace-{int(time.time() * 1000)}",
                    "region": "us-east-1",
                    "latency_ms": 12,
                }
            }
        }

