//! Secrets live in Windows Credential Manager (service "senapati").
//! `SENAPATI_SECRET_<NAME>` overrides, for CI and tests.

use anyhow::{anyhow, bail, Result};

const SERVICE: &str = "senapati";

pub const KNOWN: &[(&str, &str)] = &[
    ("telegram_bot_token", "Telegram bot token from @BotFather"),
    ("discord_webhook_url", "Discord channel webhook URL"),
];

fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        bail!("secret names use letters, digits and _ only");
    }
    Ok(())
}

pub fn get(name: &str) -> Result<String> {
    check_name(name)?;
    if let Ok(v) = std::env::var(format!("SENAPATI_SECRET_{}", name.to_ascii_uppercase())) {
        return Ok(v);
    }
    keyring::Entry::new(SERVICE, name)?
        .get_password()
        .map_err(|_| anyhow!("secret '{name}' not set; run: senapati secret set {name}"))
}

pub fn set(name: &str, value: &str) -> Result<()> {
    check_name(name)?;
    keyring::Entry::new(SERVICE, name)?.set_password(value)?;
    Ok(())
}

pub fn delete(name: &str) -> Result<()> {
    check_name(name)?;
    keyring::Entry::new(SERVICE, name)?.delete_credential()?;
    Ok(())
}

pub fn is_set(name: &str) -> bool {
    get(name).is_ok()
}
