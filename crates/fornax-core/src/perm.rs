// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Repository permissions (GitHub/GitLab-like levels).
//!
//! A user's effective permission on a repository is the **highest** of: site admin, owner,
//! organisation role, team grants, direct collaborator grant, and public visibility.

use std::fmt;
use std::str::FromStr;

use crate::ValidationError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    /// Cannot see the repository.
    #[default]
    None,
    /// Clone, browse.
    Read,
    /// Push.
    Write,
    /// Settings, collaborators, rename, delete.
    Admin,
}

impl Permission {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Read => "read",
            Self::Write => "write",
            Self::Admin => "admin",
        }
    }

    /// Permissions that can be granted to collaborators and teams.
    pub fn parse_grant(s: &str) -> Result<Self, ValidationError> {
        match s.parse()? {
            Self::None => Err(ValidationError::new(
                "permission",
                "must be read, write or admin",
            )),
            p => Ok(p),
        }
    }
}

impl FromStr for Permission {
    type Err = ValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(Self::None),
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            "admin" => Ok(Self::Admin),
            _ => Err(ValidationError::new(
                "permission",
                "must be read, write or admin",
            )),
        }
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Role of a user in an organisation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OrgRole {
    /// Full control of the organisation and all its repositories.
    Owner,
    /// Read access to every organisation repository; more via teams.
    Member,
}

impl OrgRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Member => "member",
        }
    }

    /// Permission an organisation role grants on every repository of the organisation.
    pub fn base_permission(self) -> Permission {
        match self {
            Self::Owner => Permission::Admin,
            Self::Member => Permission::Read,
        }
    }
}

impl FromStr for OrgRole {
    type Err = ValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "owner" => Ok(Self::Owner),
            "member" => Ok(Self::Member),
            _ => Err(ValidationError::new("role", "must be owner or member")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering_and_parsing() {
        assert!(Permission::Admin > Permission::Write);
        assert!(Permission::Write > Permission::Read);
        assert!(Permission::Read > Permission::None);
        assert_eq!("write".parse::<Permission>(), Ok(Permission::Write));
        assert!(Permission::parse_grant("none").is_err());
        assert!(Permission::parse_grant("owner").is_err());
        assert_eq!(OrgRole::Owner.base_permission(), Permission::Admin);
        assert_eq!("member".parse::<OrgRole>(), Ok(OrgRole::Member));
    }
}
