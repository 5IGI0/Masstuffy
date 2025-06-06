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

use chrono::{NaiveDateTime};

#[derive(sqlx::FromRow)]
pub struct DBWarcRecord {
    pub id: i64,
    pub flags: i32,
    pub date: NaiveDateTime,
    pub identifier: String,
    pub collection: String,
    pub filename: String,
    pub offset: i64,
    pub r#type: String,
    pub uri: Option<String>,
    pub dict_type: Option<String>,
    pub dict_id: Option<i64>,
    pub massaged_url: String,
    pub raw_size: i64
}

pub const RECORD_FLAG_ACTIVE: i32 = 1<<0;

#[derive(sqlx::FromRow)]
pub struct DBToken {
    pub token: String,
    pub comment: String,

    pub read_perms_kind: i16,
    pub read_perms: String,
    pub write_perms_kind: i16,
    pub write_perms: String,
    pub delete_perms_kind: i16,
    pub delete_perms: String
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

    pub fn from_db_perms(kind: i16, perms: String) -> Self {
        match kind {
            0 => Self::None,
            1 => Self::Any,
            2 => Self::List(perms.split(",").map(|x|x.to_string()).collect()),
            3 => Self::Prefix(perms),
            _ => Self::None
        }
    }
}