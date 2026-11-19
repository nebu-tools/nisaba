//! Checks of the fields people type. Each refusal names its field.

use super::Fail;
use crate::limits::{EMAIL_MAX, NAME_MAX, PASSWORD_MAX, PASSWORD_MIN, PROJECT_NAME_MAX};

/// An email, trimmed and in lower case.
pub fn email(field: &str, email: &str) -> Result<String, Fail> {
    let email = email.trim().to_lowercase();
    let well_formed = email.len() <= EMAIL_MAX
        && email
            .split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && !domain.is_empty())
        && !email.chars().any(|c| c.is_whitespace() || c.is_control());
    if !well_formed {
        return Err(Fail::field(
            field,
            format!("enter an email address of at most {EMAIL_MAX} characters"),
        ));
    }
    Ok(email)
}

pub fn password(field: &str, password: &str) -> Result<(), Fail> {
    if password.chars().count() < PASSWORD_MIN {
        return Err(Fail::field(
            field,
            format!("use at least {PASSWORD_MIN} characters"),
        ));
    }
    if password.len() > PASSWORD_MAX {
        return Err(Fail::field(
            field,
            format!("use at most {PASSWORD_MAX} bytes"),
        ));
    }
    Ok(())
}

/// A display name, trimmed; `fallback` when empty and one is given.
pub fn name(field: &str, name: &str, fallback: Option<&str>) -> Result<String, Fail> {
    text(field, name, NAME_MAX, fallback)
}

pub fn project_name(name: &str) -> Result<String, Fail> {
    text("name", name, PROJECT_NAME_MAX, None)
}

fn text(field: &str, value: &str, max: usize, fallback: Option<&str>) -> Result<String, Fail> {
    let value = value.trim();
    let value = match (value.is_empty(), fallback) {
        (true, Some(f)) => f,
        _ => value,
    };
    if value.is_empty() || value.chars().count() > max || value.chars().any(char::is_control) {
        return Err(Fail::field(
            field,
            format!("use 1 to {max} characters and no control characters"),
        ));
    }
    Ok(value.to_owned())
}
