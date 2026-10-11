"""
IDL Parser - Parses Protobuf (.proto) and Thrift (.thrift) definitions into MCP-compatible JSON-RPC schemas.
"""
from __future__ import annotations
import re
from dataclasses import dataclass, field
from typing import Dict, Any, List, Optional
from omni_mcp.models.entities import ProtocolType


@dataclass
class FieldDefinition:
    name: str
    field_type: str
    is_repeated: bool = False
    is_required: bool = False
    doc_comment: str = ""


@dataclass
class MethodDefinition:
    name: str
    request_type: str
    response_type: str
    fields: List[FieldDefinition] = field(default_factory=list)
    doc_comment: str = ""
    protocol: ProtocolType = ProtocolType.GRPC


@dataclass
class ServiceDefinition:
    service_name: str
    package: str
    methods: List[MethodDefinition] = field(default_factory=list)
    doc_comment: str = ""
    protocol: ProtocolType = ProtocolType.GRPC


class IDLParser:
    """Extracts RPC definitions from Protobuf and Thrift definitions and translates to JSON Schema."""

    # Map IDL primitive types to JSON Schema types
    TYPE_MAP = {
        "string": "string",
        "int32": "integer",
        "int64": "integer",
        "uint32": "integer",
        "uint64": "integer",
        "sint32": "integer",
        "sint64": "integer",
        "fixed32": "integer",
        "fixed64": "integer",
        "i16": "integer",
        "i32": "integer",
        "i64": "integer",
        "byte": "integer",
        "bool": "boolean",
        "float": "number",
        "double": "number",
        "bytes": "string",
    }

    @classmethod
    def parse_protobuf(cls, content: str) -> List[ServiceDefinition]:
        """Parses a protobuf string definition."""
        services: List[ServiceDefinition] = []

        # Extract package
        pkg_match = re.search(r"package\s+([\w\.]+);", content)
        package = pkg_match.group(1) if pkg_match else "uber.service"

        # Extract messages to get field details
        messages: Dict[str, List[FieldDefinition]] = {}
        msg_pattern = re.compile(r"message\s+(\w+)\s*\{([^}]+)\}", re.DOTALL)
        for msg_match in msg_pattern.finditer(content):
            msg_name = msg_match.group(1)
            msg_body = msg_match.group(2)
            fields: List[FieldDefinition] = []

            for line in msg_body.splitlines():
                line = line.strip()
                if not line or line.startswith("//"):
                    continue
                # e.g.: repeated string tag_ids = 1; // Comments
                field_re = re.match(r"(repeated\s+)?(\w+)\s+(\w+)\s*=\s*\d+;(?:\s*//\s*(.*))?", line)
                if field_re:
                    is_rep = bool(field_re.group(1))
                    ftype = field_re.group(2)
                    fname = field_re.group(3)
                    comment = field_re.group(4) or ""
                    fields.append(FieldDefinition(
                        name=fname,
                        field_type=ftype,
                        is_repeated=is_rep,
                        doc_comment=comment.strip()
                    ))
            messages[msg_name] = fields

        # Extract services
        svc_pattern = re.compile(r"(?://\s*(.*)\n)?\s*service\s+(\w+)\s*\{([^}]+)\}", re.DOTALL)
        for svc_match in svc_pattern.finditer(content):
            svc_doc = (svc_match.group(1) or "").strip()
            svc_name = svc_match.group(2)
            svc_body = svc_match.group(3)

            methods: List[MethodDefinition] = []
            # rpc GetDriverInfo (GetDriverRequest) returns (GetDriverResponse);
            rpc_pattern = re.compile(
                r"(?://\s*(.*)\n)?\s*rpc\s+(\w+)\s*\(\s*(\w+)\s*\)\s*returns\s*\(\s*(\w+)\s*\);",
                re.DOTALL
            )
            for rpc_match in rpc_pattern.finditer(svc_body):
                rpc_doc = (rpc_match.group(1) or "").strip()
                method_name = rpc_match.group(2)
                req_type = rpc_match.group(3)
                resp_type = rpc_match.group(4)

                method_fields = messages.get(req_type, [])
                methods.append(MethodDefinition(
                    name=method_name,
                    request_type=req_type,
                    response_type=resp_type,
                    fields=method_fields,
                    doc_comment=rpc_doc,
                    protocol=ProtocolType.GRPC,
                ))

            services.append(ServiceDefinition(
                service_name=svc_name,
                package=package,
                methods=methods,
                doc_comment=svc_doc,
                protocol=ProtocolType.GRPC,
            ))

        return services

    @classmethod
    def parse_thrift(cls, content: str) -> List[ServiceDefinition]:
        """Parses a Thrift string definition (TChannel protocol)."""
        services: List[ServiceDefinition] = []

        # Extract namespace
        ns_match = re.search(r"namespace\s+\w+\s+([\w\.]+)", content)
        package = ns_match.group(1) if ns_match else "uber.tchannel"

        # service RiderGateway { RiderStatus getRiderStatus(1: string riderId, 2: bool includeHistory) }
        svc_pattern = re.compile(r"(?://\s*(.*)\n)?\s*service\s+(\w+)\s*\{([^}]+)\}", re.DOTALL)
        for svc_match in svc_pattern.finditer(content):
            svc_doc = (svc_match.group(1) or "").strip()
            svc_name = svc_match.group(2)
            svc_body = svc_match.group(3)

            methods: List[MethodDefinition] = []
            # ReturnType methodName(params)
            method_pattern = re.compile(
                r"(?://\s*(.*)\n)?\s*(\w+)\s+(\w+)\s*\(([^)]*)\)",
                re.DOTALL
            )
            for m_match in method_pattern.finditer(svc_body):
                m_doc = (m_match.group(1) or "").strip()
                resp_type = m_match.group(2)
                m_name = m_match.group(3)
                params_str = m_match.group(4)

                fields: List[FieldDefinition] = []
                for param in params_str.split(","):
                    param = param.strip()
                    if not param:
                        continue
                    # 1: string riderId
                    p_re = re.search(r"\d+:\s*(?:(list<\w+>)|(\w+))\s+(\w+)", param)
                    if p_re:
                        is_rep = bool(p_re.group(1))
                        ftype = p_re.group(2) or "string"
                        fname = p_re.group(3)
                        fields.append(FieldDefinition(name=fname, field_type=ftype, is_repeated=is_rep))

                methods.append(MethodDefinition(
                    name=m_name,
                    request_type=f"{m_name}Request",
                    response_type=resp_type,
                    fields=fields,
                    doc_comment=m_doc,
                    protocol=ProtocolType.TCHANNEL,
                ))

            services.append(ServiceDefinition(
                service_name=svc_name,
                package=package,
                methods=methods,
                doc_comment=svc_doc,
                protocol=ProtocolType.TCHANNEL,
            ))

        return services

    @classmethod
    def translate_to_json_schema(cls, method: MethodDefinition) -> Dict[str, Any]:
        """Translates method parameter fields into an MCP JSON Schema."""
        properties: Dict[str, Any] = {}
        required: List[str] = []

        for field_def in method.fields:
            json_type = cls.TYPE_MAP.get(field_def.field_type.lower(), "string")
            prop: Dict[str, Any] = {
                "type": "array" if field_def.is_repeated else json_type,
                "description": field_def.doc_comment or f"Field {field_def.name} ({field_def.field_type})"
            }
            if field_def.is_repeated:
                prop["items"] = {"type": json_type}

            properties[field_def.name] = prop
            if field_def.is_required:
                required.append(field_def.name)

        schema: Dict[str, Any] = {
            "type": "object",
            "properties": properties,
        }
        if required:
            schema["required"] = required
        return schema

