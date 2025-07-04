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

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct Config {
    pub listen_addr: String,
    pub secret_key: Option<String>,
    pub database: String,
    pub anonymous_read_perms_kind: String,
    pub anonymous_read_perms: String,
    pub anonymous_write_perms_kind: String,
    pub anonymous_write_perms: String,
    pub anonymous_delete_perms_kind: String,
    pub anonymous_delete_perms: String,
}

impl Config {
    pub fn validate(&self) -> Option<String> {
        None // TODO: check (return None if no error)
    }
}

impl Default for Config {
    fn default() -> Self {
        Config{
            listen_addr: "127.0.0.1:8080".to_string(),
            secret_key: None,
            database: String::new(),
            anonymous_read_perms_kind: "any".to_string(),
            anonymous_read_perms: String::new(),
            anonymous_write_perms_kind: "any".to_string(),
            anonymous_write_perms: String::new(),
            anonymous_delete_perms_kind: "any".to_string(),
            anonymous_delete_perms: String::new(),
        }
    }
}