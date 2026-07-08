use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// PROJECT.md 4.6/4.7: a new domain under `tofu` mode hangs here until a
/// human approves it via the gateway, instead of the guest ever getting to
/// make that call itself. `http_fetch` writes (POST/PUT/etc) used to queue
/// here too (`kind: "http_write"`) — removed once `ssh_exec` existed as an
/// ungated way to do the same thing, since the gate had stopped being real
/// containment once an equivalent ungated path existed (see `http_fetch.rs`
/// module docs). Old `"http_write"` entries may still exist in
/// `logs/grants.json` from before that; nothing creates new ones.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PendingGrant {
    pub id: String,
    /// `"tofu_domain"` — approving unlocks the whole domain permanently
    pub kind: String,
    pub method: String,
    pub url: String,
    pub domain: String,
    pub created_at: i64,
    pub status: String, // "pending" | "approved" | "denied"
}

fn grants_path(agent_home: &Path) -> PathBuf {
    agent_home.join("logs/grants.json")
}

fn approved_domains_path(agent_home: &Path) -> PathBuf {
    agent_home.join("logs/approved_domains.json")
}

pub fn load_grants(agent_home: &Path) -> Vec<PendingGrant> {
    std::fs::read_to_string(grants_path(agent_home))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_grants(agent_home: &Path, grants: &[PendingGrant]) -> anyhow::Result<()> {
    let path = grants_path(agent_home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(grants)?)?;
    Ok(())
}

/// Called from `http_fetch` itself — creates a pending entry and returns its
/// id for the guest's `pending_approval` error, without blocking (kernel
/// syscalls are synchronous; a human isn't going to click "approve" within
/// an epoch-interruption window, so the guest just gets told to try again
/// on a later wake).
pub fn request_grant(agent_home: &Path, kind: &str, method: &str, url: &str, domain: &str) -> anyhow::Result<String> {
    let mut grants = load_grants(agent_home);
    let id = format!("{}-{}", crate::logs::now_unix_secs(), grants.len());
    grants.push(PendingGrant {
        id: id.clone(),
        kind: kind.to_string(),
        method: method.to_string(),
        url: url.to_string(),
        domain: domain.to_string(),
        created_at: crate::logs::now_unix_secs(),
        status: "pending".to_string(),
    });
    save_grants(agent_home, &grants)?;
    Ok(id)
}

pub fn approve(agent_home: &Path, id: &str) -> anyhow::Result<Option<PendingGrant>> {
    let mut grants = load_grants(agent_home);
    let Some(grant) = grants.iter_mut().find(|g| g.id == id) else {
        return Ok(None);
    };
    grant.status = "approved".to_string();
    let approved = grant.clone();
    if approved.kind == "tofu_domain" {
        add_approved_domain(agent_home, &approved.domain)?;
    }
    save_grants(agent_home, &grants)?;
    Ok(Some(approved))
}

pub fn deny(agent_home: &Path, id: &str) -> anyhow::Result<Option<PendingGrant>> {
    let mut grants = load_grants(agent_home);
    let Some(grant) = grants.iter_mut().find(|g| g.id == id) else {
        return Ok(None);
    };
    grant.status = "denied".to_string();
    let denied = grant.clone();
    save_grants(agent_home, &grants)?;
    Ok(Some(denied))
}

fn add_approved_domain(agent_home: &Path, domain: &str) -> anyhow::Result<()> {
    let mut domains = load_approved_domains(agent_home);
    if !domains.iter().any(|d| d == domain) {
        domains.push(domain.to_string());
        let path = approved_domains_path(agent_home);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(&domains)?)?;
    }
    Ok(())
}

pub fn load_approved_domains(agent_home: &Path) -> Vec<String> {
    std::fs::read_to_string(approved_domains_path(agent_home))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn is_domain_approved(agent_home: &Path, domain: &str) -> bool {
    load_approved_domains(agent_home).iter().any(|d| d == domain)
}
