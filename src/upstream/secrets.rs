//! Refs-Only Credential Resolver for External MCP Gateways.
//! Strictly validates and resolves credentials from `{env:NAME}` or `{file:PATH}` references.
//! Raw secret bytes are never permitted at registration and are resolved exclusively at spawn/connect time.

use std::fs;
use std::path::{Path, PathBuf};

/// Validates that a secret reference string follows strict `{env:NAME}` or `{file:PATH}` syntax.
/// Returns an error if the value is a raw credential or an invalid reference.
pub fn validate_secret_ref_syntax(val: &str) -> Result<(), String> {
    let trimmed = val.trim();
    if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
        return Err("Raw credentials are not permitted; use {env:NAME} or {file:PATH}".to_string());
    }

    if let Some(inner) = trimmed
        .strip_prefix("{env:")
        .and_then(|s| s.strip_suffix('}'))
    {
        let var_name = inner.trim();
        if var_name.is_empty() {
            return Err("Environment variable reference cannot be empty".to_string());
        }
        if var_name == "OMNI_AUTH_SECRET" {
            return Err("Access to OMNI_AUTH_SECRET via secret ref is forbidden".to_string());
        }
        let first = var_name.chars().next().unwrap();
        if !first.is_ascii_uppercase() && first != '_' {
            return Err(format!(
                "Environment variable name '{}' must start with an uppercase letter or underscore",
                var_name
            ));
        }
        if !var_name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(format!(
                "Environment variable name '{}' must match ^[A-Z_][A-Z0-9_]*$",
                var_name
            ));
        }
        return Ok(());
    }

    if let Some(inner) = trimmed
        .strip_prefix("{file:")
        .and_then(|s| s.strip_suffix('}'))
    {
        let path_str = inner.trim();
        if path_str.is_empty() || path_str.contains('\0') {
            return Err("Invalid file path in secret reference".to_string());
        }
        return Ok(());
    }

    Err("Raw credentials are not permitted; use {env:NAME} or {file:PATH}".to_string())
}

/// Resolves a secret reference string `{env:NAME}` or `{file:PATH}` into its string value.
/// File references are canonicalized and strictly verified to reside within `secrets_dir`.
pub fn resolve_secret_ref(secret_ref: &str, secrets_dir: &Path) -> Result<String, String> {
    validate_secret_ref_syntax(secret_ref)?;
    let trimmed = secret_ref.trim();

    if let Some(inner) = trimmed
        .strip_prefix("{env:")
        .and_then(|s| s.strip_suffix('}'))
    {
        let var_name = inner.trim();
        return std::env::var(var_name)
            .map_err(|_| format!("Secret environment variable '{}' not set", var_name));
    }

    if let Some(inner) = trimmed
        .strip_prefix("{file:")
        .and_then(|s| s.strip_suffix('}'))
    {
        let path_str = inner.trim();
        let candidate: PathBuf = if Path::new(path_str).is_absolute() {
            PathBuf::from(path_str)
        } else {
            secrets_dir.join(path_str)
        };

        let canonical_file = fs::canonicalize(&candidate)
            .map_err(|e| format!("Cannot resolve secret file '{}': {}", path_str, e))?;

        let canonical_dir = fs::canonicalize(secrets_dir).map_err(|e| {
            format!(
                "Cannot canonicalize secrets_dir '{}': {}",
                secrets_dir.display(),
                e
            )
        })?;

        if !canonical_file.starts_with(&canonical_dir) {
            return Err(format!(
                "Path traversal rejected: secret file '{}' resolves outside secrets_dir '{}'",
                path_str,
                secrets_dir.display()
            ));
        }

        let content = fs::read_to_string(&canonical_file).map_err(|e| {
            format!(
                "Failed to read secret file '{}': {}",
                canonical_file.display(),
                e
            )
        })?;

        return Ok(content.trim().to_string());
    }

    Err("Invalid secret reference format".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_secret_ref_validation() {
        assert!(validate_secret_ref_syntax("{env:GITHUB_TOKEN}").is_ok());
        assert!(validate_secret_ref_syntax("{env:_SECRET_123}").is_ok());
        assert!(validate_secret_ref_syntax("{file:keys/cert.pem}").is_ok());
        assert!(validate_secret_ref_syntax("{file:./my_token.txt}").is_ok());

        // Raw tokens rejected
        assert!(validate_secret_ref_syntax("Bearer sk-12345").is_err());
        assert!(validate_secret_ref_syntax("my-secret-key").is_err());
        assert!(validate_secret_ref_syntax("ghp_1234567890abcdef").is_err());

        // OMNI_AUTH_SECRET rejected
        assert!(validate_secret_ref_syntax("{env:OMNI_AUTH_SECRET}").is_err());

        // Lowercase or invalid env vars rejected
        assert!(validate_secret_ref_syntax("{env:github_token}").is_err());
        assert!(validate_secret_ref_syntax("{env:123_VAR}").is_err());
        assert!(validate_secret_ref_syntax("{env:}").is_err());
        assert!(validate_secret_ref_syntax("{other:something}").is_err());
    }

    #[test]
    fn test_secret_ref_env_resolution() {
        std::env::set_var("OMNI_TEST_RESOLVE_TOKEN", "resolved_value_99");
        let dir = std::env::temp_dir();
        let res = resolve_secret_ref("{env:OMNI_TEST_RESOLVE_TOKEN}", &dir);
        assert_eq!(res.unwrap(), "resolved_value_99");

        let missing = resolve_secret_ref("{env:OMNI_TEST_NON_EXISTENT_VAR}", &dir);
        assert!(missing.is_err());
    }

    #[test]
    fn test_secret_ref_file_resolution_and_traversal_guard() {
        let tmp = std::env::temp_dir().join(format!(
            "omni_secrets_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let secrets_dir = tmp.join("secrets");
        let outside_dir = tmp.join("outside");
        let _ = fs::create_dir_all(&secrets_dir);
        let _ = fs::create_dir_all(&outside_dir);

        let valid_file = secrets_dir.join("token.txt");
        fs::write(&valid_file, "super_secure_file_token\n").unwrap();

        let outside_file = outside_dir.join("forbidden.txt");
        fs::write(&outside_file, "leak_data\n").unwrap();

        // Valid file inside secrets_dir
        let res = resolve_secret_ref("{file:token.txt}", &secrets_dir);
        assert_eq!(res.unwrap(), "super_secure_file_token");

        // Traversal attempt to outside_dir
        let traversal = resolve_secret_ref("{file:../outside/forbidden.txt}", &secrets_dir);
        assert!(traversal.is_err());
        assert!(traversal.unwrap_err().contains("Path traversal rejected"));

        let _ = fs::remove_dir_all(&tmp);
    }
}
