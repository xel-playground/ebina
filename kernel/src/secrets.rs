use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Flat name → value table loaded from a `secrets.toml` that lives *outside*
/// `agent_home` — the guest's WASI preopen root is exactly `agent_home`, so a
/// sibling file is structurally invisible to it, same guarantee as the empty
/// env (PROJECT.md 4.8: keys never enter the sandbox).
///
/// This is the minimal slice of the eventual Credential Vault (4.8) needed
/// now: naming a secret for `llm_call`/`embed` to use instead of a host env
/// var. Domain-bound secrets for `http_get` are Phase 5.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Secrets(HashMap<String, String>);

impl Secrets {
    /// Missing file is not an error — env var fallback still works, so a
    /// brand new agent-home with no vault configured just works as before.
    pub fn load(path: &Path) -> Secrets {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// Upserts one secret and returns the updated set of names — never the
    /// values (the gateway's `/api/secrets` is deliberately write-only, see
    /// [`Secrets::names`]).
    pub fn set(&mut self, name: &str, value: &str) {
        self.0.insert(name.to_string(), value.to_string());
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Names only, never values — the gateway can report *which* secrets
    /// exist (e.g. so a UI can show "ollama: configured") without ever
    /// exposing a value over HTTP.
    pub fn names(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }
}

const PREFIX: &str = "{secrets.";
const SUFFIX: &str = "}";

/// Resolves a config field that must be a vault placeholder — `{secrets.NAME}`
/// — into the actual secret value. Rejects anything else (including a bare
/// literal key) so a key can never end up sitting in `config.toml` in the
/// clear, since that file lives inside agent_home and the guest reads it.
pub fn resolve_placeholder(secrets: &Secrets, spec: &str) -> Result<String, String> {
    let name = spec
        .strip_prefix(PREFIX)
        .and_then(|s| s.strip_suffix(SUFFIX))
        .ok_or_else(|| format!("expected a vault placeholder like `{{secrets.name}}`, got: {spec}"))?;

    secrets
        .get(name)
        .map(str::to_string)
        .ok_or_else(|| format!("no secret named `{name}` in the vault"))
}

/// Resolves every `{secrets.NAME}` occurrence *within* a larger string —
/// unlike `resolve_placeholder`, which requires the whole field to be
/// nothing but one placeholder — for a syscall like `ssh_exec` where a
/// secret needs to be substituted into one piece of an otherwise
/// guest-authored command (e.g. `curl -H "Authorization: Bot
/// {secrets.discord_bot_token}" ...`). Errors, rather than leaving the
/// literal placeholder text sitting in the command, the moment any named
/// secret is missing, not in `allowed`, or a `{secrets.` is left
/// unterminated.
///
/// `allowed` is a deliberate allowlist, not "any secret in the vault" —
/// even though `ssh_exec`'s destination is fixed (the same property that
/// makes `llm_call`'s api_key resolution safe, unlike `http_get`'s
/// arbitrary guest-chosen URL), the agent still authors the *whole*
/// command, and the target machine has its own outbound network access.
/// Without this list, a prompt-injected agent could put any other vault
/// secret's placeholder — the LLM provider's api_key, `ssh_key_passphrase`
/// — into an `ssh_exec` command just as easily as the one secret actually
/// meant to be usable this way (see `SshConfig::allowed_secrets`).
pub fn resolve_placeholders_in(secrets: &Secrets, text: &str, allowed: &[String]) -> Result<String, String> {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(PREFIX) {
        result.push_str(&rest[..start]);
        let after_prefix = &rest[start + PREFIX.len()..];
        let Some(end) = after_prefix.find(SUFFIX) else {
            return Err(format!("unterminated `{{secrets.` placeholder in: {text}"));
        };
        let name = &after_prefix[..end];
        if !allowed.iter().any(|a| a == name) {
            return Err(format!("secret `{name}` is not in `[ssh] allowed_secrets` — not permitted for ssh_exec placeholder substitution"));
        }
        let value = secrets.get(name).ok_or_else(|| format!("no secret named `{name}` in the vault"))?;
        result.push_str(value);
        rest = &after_prefix[end + SUFFIX.len()..];
    }
    result.push_str(rest);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secrets_with(name: &str, value: &str) -> Secrets {
        let mut map = HashMap::new();
        map.insert(name.to_string(), value.to_string());
        Secrets(map)
    }

    #[test]
    fn resolves_a_placeholder_to_its_vault_value() {
        let secrets = secrets_with("ollama", "the-real-key");
        assert_eq!(resolve_placeholder(&secrets, "{secrets.ollama}").unwrap(), "the-real-key");
    }

    #[test]
    fn errors_when_secret_missing_from_vault() {
        let secrets = Secrets::default();
        assert!(resolve_placeholder(&secrets, "{secrets.nope}").is_err());
    }

    #[test]
    fn rejects_anything_that_isnt_a_placeholder() {
        let secrets = secrets_with("ollama", "the-real-key");
        // a literal value typed directly into config.toml must be rejected,
        // not silently accepted — config.toml is guest-readable
        assert!(resolve_placeholder(&secrets, "the-real-key").is_err());
        assert!(resolve_placeholder(&secrets, "").is_err());
    }

    #[test]
    fn resolves_allowed_placeholder_inside_a_larger_command() {
        let secrets = secrets_with("discord", "tok-123");
        let allowed = vec!["discord".to_string()];
        let out = resolve_placeholders_in(&secrets, "curl -H 'Authorization: Bot {secrets.discord}' https://x", &allowed).unwrap();
        assert_eq!(out, "curl -H 'Authorization: Bot tok-123' https://x");
    }

    #[test]
    fn rejects_placeholder_not_in_allowlist() {
        let secrets = secrets_with("llm_api_key", "sk-real");
        // present in the vault, but the caller only allowed "discord" —
        // must not resolve, even though the secret genuinely exists
        let allowed = vec!["discord".to_string()];
        let err = resolve_placeholders_in(&secrets, "curl {secrets.llm_api_key}", &allowed).unwrap_err();
        assert!(err.contains("llm_api_key"));
    }

    #[test]
    fn rejects_unterminated_placeholder() {
        let secrets = secrets_with("discord", "tok-123");
        let allowed = vec!["discord".to_string()];
        assert!(resolve_placeholders_in(&secrets, "curl {secrets.discord", &allowed).is_err());
    }

    #[test]
    fn passes_through_text_with_no_placeholders() {
        let secrets = Secrets::default();
        assert_eq!(resolve_placeholders_in(&secrets, "curl https://example.com", &[]).unwrap(), "curl https://example.com");
    }
}
