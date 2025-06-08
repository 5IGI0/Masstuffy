/**
 *  This file is part of Masstuffy. Masstuffy is free software:
 *  you can redistribute it and/or modify it under the terms of 
 *  the GNU Affero General Public License as published by
 *  the Free Software Foundation, either version 3 of the License,
 *  or (at your option) any later version.
 * 
 *  Masstuffy is distributed in the hope that it will be useful,
 *  but WITHOUT ANY WARRANTY; without even the implied warranty of
 *  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
 * 
 *  See the GNU Affero General Public License for more details.
 *  You should have received a copy of the GNU Affero General Public License
 *  along with Masstuffy. If not, see <https://www.gnu.org/licenses/>. 
 * 
 *  Copyright (C) 2025 5IGI0 / Ethan L. C. Lorenzetti
**/

use std::fmt::Display;
use anyhow::anyhow;
use crate::{database::{structs::DBToken, DBManager}, filesystem::FileSystem};

pub enum PermissionType {
    READ,
    WRITE,
    DELETE
}

pub struct TokenInfo {
    pub token: String,
    pub comment: String,

    pub read_perms: TokenPermission,
    pub write_perms: TokenPermission,
    pub delete_perms: TokenPermission,
}

impl TokenInfo {
    pub fn from_db_row(token: DBToken) -> Self {
        Self{
            token: token.token,
            comment: token.comment,
            read_perms: TokenPermission::from_db_perms(token.read_perms_kind, token.read_perms),
            write_perms: TokenPermission::from_db_perms(token.write_perms_kind, token.write_perms),
            delete_perms: TokenPermission::from_db_perms(token.delete_perms_kind, token.delete_perms)
        }
    }
}

pub enum TokenPermission {
    None,
    Any,
    List(Vec<String>),
    Prefix(String)
}

impl TokenPermission {
    pub fn get_perms_kind(&self) -> i16 {
        match self {
            TokenPermission::None => 0,
            TokenPermission::Any => 1,
            TokenPermission::List(_) => 2,
            TokenPermission::Prefix(_) => 3
        }
    }

    pub fn get_perms(&self) -> String {
        match self {
            TokenPermission::None => String::new(),
            TokenPermission::Any => String::new(),
            TokenPermission::List(x) => x.join(","),
            TokenPermission::Prefix(x) => x.clone()
        }
    }

    pub fn get_perms_kind_str(&self) -> &str {
        match self {
            TokenPermission::None => "none",
            TokenPermission::Any => "any",
            TokenPermission::List(_) => "list",
            TokenPermission::Prefix(_) => "prefix"
        }
    }

    pub fn from_fs_perms(kind: &str, perms: &String) -> Self {
        match kind {
            "none" => Self::None,
            "any" => Self::Any,
            "list" => Self::List(perms.split(",").map(|x|x.to_string()).collect()),
            "prefix" => Self::Prefix(perms.clone()),
            _ => Self::None
        }
    }

    pub fn from_db_perms(kind: i16, perms: String) -> Self {
        match kind {
            0 => Self::None,
            1 => Self::Any,
            2 => Self::List(perms.split(",").map(|x|x.to_string()).collect()),
            3 => Self::Prefix(perms),
            _ => Self::None
        }
    }

    pub fn check(&self, coll_slug: &str) -> bool {
        match self {
            TokenPermission::None => false,
            TokenPermission::Any => true,
            TokenPermission::List(l) => l.contains(&coll_slug.to_string()),
            TokenPermission::Prefix(p) => coll_slug.starts_with(p)
        }
    }
}

impl Display for TokenPermission {
    fn fmt(&self, format: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        format.write_fmt(format_args!("{}({})", self.get_perms_kind_str(), self.get_perms()))
    }
}

pub async fn check_access_token(db: &DBManager, fs: &FileSystem, permtype: PermissionType, token: &str, coll_slug: &str) -> anyhow::Result<bool> {
    let token_info = 
    if let Some(ti) = db.get_permissions(token).await? {
        ti
    } else {
        fs.get_default_permissions()
    };

    Ok(match permtype {
        PermissionType::READ => token_info.read_perms.check(coll_slug),
        PermissionType::WRITE => token_info.write_perms.check(coll_slug),
        PermissionType::DELETE => token_info.delete_perms.check(coll_slug)
    })
}

pub async fn assert_access(db: &DBManager, fs: &FileSystem, permtype: PermissionType, token: &str, coll_slug: &str) -> anyhow::Result<()> {
    if !check_access_token(db, fs, permtype, token, coll_slug).await? {
        Err(anyhow!("forbidden"))
    } else {
        Ok(())
    }
}