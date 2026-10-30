# Security Policy

## Reporting a vulnerability

Please do **not** open a public issue for security problems. Use GitHub's
private vulnerability reporting on
<https://github.com/Rangow4562/omni-mcp-gateway/security/advisories/new>.
Include the affected version, reproduction steps and impact. We aim to
acknowledge reports within 7 days.

## Supported versions

Only the latest minor release line receives security fixes.

## Scope notes

- The gateway does not terminate TLS in-process. Run it behind a TLS-terminating
  reverse proxy or sidecar when exposed beyond localhost.
- The Code Mode sandbox is an in-process AST interpreter, not an OS-level
  isolation boundary. Do not treat it as a hardened multi-tenant jail.
